use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use walkdir::WalkDir;

use super::{AgentHandler, structured_result_with_text};
use crate::{error::AppError, request_context::ProjectRequestContext, sandbox::PathOperation};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct TreeArgs {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default, alias = "depth")]
    pub max_depth: Option<usize>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub max_entries: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct TreeEntryOutput {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub depth: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct TreeOutput {
    pub path: String,
    pub entries: Vec<TreeEntryOutput>,
    pub traversed: usize,
    pub truncated: bool,
    pub traversal_limit_hit: bool,
    pub next_offset: Option<usize>,
    pub continuation: Option<String>,
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[tool_router(router = misc_router, vis = "pub(crate)")]
impl AgentHandler {
    #[tool(
        description = "Render a bounded tree under an optional project-relative path. max_depth and max_entries bound traversal; offset/next_offset provide continuation. If traversal_limit_hit=true, the visible tree is incomplete and must not be treated as exhaustive."
    )]
    async fn tree(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<TreeArgs>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let shared = self.shared.clone();
        let params = serde_json::to_value(&args).unwrap_or_default();
        self.run_content(context.0, "tree", params, move |project| async move {
            let base_input = args
                .path
                .as_deref()
                .filter(|path| !path.is_empty())
                .unwrap_or(".");
            let instruction_notice = shared.scoped_instruction_notice(&project, base_input)?;
            let base = shared.paths.resolve_project_path(
                &project.project_root,
                base_input,
                PathOperation::Existing,
            )?;
            let root = project.project_root.clone();
            let depth = args.max_depth.unwrap_or(4).min(32);
            let maximum = args
                .max_entries
                .unwrap_or(shared.config.output.results)
                .min(shared.config.limits.results);
            if maximum == 0 {
                return Err(AppError::new(
                    "INVALID_INPUT",
                    "max_entries must be positive",
                ));
            }
            let traversal_limit = shared.config.limits.traversed_entries;
            let offset = args.offset;
            let entries = tokio::task::spawn_blocking(move || {
                let mut values = Vec::new();
                let mut traversed = 0usize;
                let mut matched = 0usize;
                let mut page_truncated = false;
                let mut traversal_truncated = false;
                let ignore = crate::ignore_rules::IgnoreMatcher::for_project(&root);
                let walker = WalkDir::new(&base)
                    .max_depth(depth)
                    .follow_links(false)
                    .sort_by_file_name()
                    .into_iter()
                    .filter_entry(|entry| {
                        entry.depth() == 0
                            || !ignore.is_ignored(entry.path(), entry.file_type().is_dir())
                    });
                for entry in walker {
                    let entry = entry
                        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
                    traversed += 1;
                    if traversed > traversal_limit {
                        traversal_truncated = true;
                        break;
                    }
                    if entry.path() == base {
                        continue;
                    }
                    if matched < offset {
                        matched += 1;
                        continue;
                    }
                    if values.len() >= maximum {
                        page_truncated = true;
                        break;
                    }
                    let relative = entry.path().strip_prefix(&root).map_err(|_| {
                        AppError::new("PATH_OUTSIDE_WORKSPACE", "tree escaped project")
                    })?;
                    let kind = if entry.file_type().is_dir() {
                        "directory"
                    } else if entry.file_type().is_symlink() {
                        "symlink"
                    } else {
                        "file"
                    };
                    values.push(TreeEntryOutput {
                        path: relative.to_string_lossy().replace('\\', "/"),
                        kind: kind.to_owned(),
                        depth: entry.depth(),
                    });
                    matched += 1;
                }
                Ok::<_, AppError>((values, traversed, page_truncated, traversal_truncated))
            })
            .await
            .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))??;
            let truncated = entries.2 || entries.3;
            let next_offset = entries.2.then_some(offset.saturating_add(entries.0.len()));
            let output = TreeOutput {
                path: base_input.to_owned(),
                entries: entries.0,
                traversed: entries.1,
                truncated,
                traversal_limit_hit: entries.3,
                next_offset,
                continuation: if let Some(value) = next_offset {
                    Some(format!("Call tree again with offset={value}."))
                } else if entries.3 {
                    Some(
                        "Traversal limit reached; narrow path or max_depth before continuing."
                            .to_owned(),
                    )
                } else {
                    None
                },
            };
            let value = serde_json::to_value(&output).unwrap_or_default();
            let mut lines = Vec::new();
            if let Some(items) = value["entries"].as_array() {
                for item in items {
                    let path = item["path"].as_str().unwrap_or("?");
                    let depth = item["depth"].as_u64().unwrap_or(0);
                    let kind = item["type"].as_str().unwrap_or("file");
                    let name = path.rsplit('/').next().unwrap_or(path);
                    let suffix = if kind == "directory" {
                        "/"
                    } else if kind == "symlink" {
                        "@"
                    } else {
                        ""
                    };
                    lines.push(format!(
                        "{}{}{}",
                        "  ".repeat(depth.saturating_sub(1) as usize),
                        name,
                        suffix
                    ));
                }
            }
            let mut text = if lines.is_empty() {
                ".".to_owned()
            } else {
                format!(".\n{}", lines.join("\n"))
            };
            if let Some(continuation) = value["continuation"].as_str() {
                text.push_str("\n\n");
                text.push_str(continuation);
            }
            if let Some(notice) = instruction_notice {
                text = format!("{notice}\n{text}");
            }
            Ok((structured_result_with_text(value.clone(), text), value))
        })
        .await
    }

    #[tool(
        description = "Read a project-relative PNG, JPEG, GIF, BMP, or WebP image, validate it by actual image decoding, and return a native MCP image content block. Signature-only or corrupt image payloads are rejected."
    )]
    async fn view_image(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<super::PathArgs>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let shared = self.shared.clone();
        let params = serde_json::to_value(&args).unwrap_or_default();
        self.run_content(context.0, "view_image", params, move |project| async move {
            let instruction_notice = shared.scoped_instruction_notice(&project, &args.path)?;
            let paths = shared.paths.clone();
            let root = project.project_root.clone();
            let input = args.path.clone();
            let maximum = shared.config.limits.write_bytes;
            let bytes = tokio::task::spawn_blocking(move || {
                paths.read_file_bounded(&root, &input, maximum)
            }).await.map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))??;
            let mime = image_mime(&bytes).ok_or_else(|| {
                AppError::new("INVALID_INPUT", "unsupported or invalid image format")
            })?;
            image::load_from_memory(&bytes).map_err(|error| {
                AppError::new(
                    "INVALID_INPUT",
                    format!("image bytes could not be decoded: {error}"),
                )
            })?;
            let encoded = STANDARD.encode(&bytes);
            let structured = json!({"path":args.path.clone(),"bytes":bytes.len(),"mime_type":mime});
            let mut result = CallToolResult::success(vec![
                ContentBlock::text(if let Some(notice) = instruction_notice {
                    format!("{notice}\n{}", json!({"path":args.path.clone(),"bytes":bytes.len(),"mime_type":mime}))
                } else {
                    json!({"path":args.path.clone(),"bytes":bytes.len(),"mime_type":mime}).to_string()
                }),
                ContentBlock::image(encoded, mime),
            ]);
            result.structured_content = Some(structured);
            Ok((result, json!({"path":args.path,"bytes":bytes.len(),"mime_type":mime,"image_data":"[BINARY REDACTED]"})))
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_images_by_magic_not_extension() {
        assert_eq!(image_mime(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(image_mime(b"GIF89arest"), Some("image/gif"));
        assert_eq!(image_mime(b"not image"), None);
    }

    #[test]
    fn detects_every_supported_image_family() {
        let cases: &[(&[u8], &str)] = &[
            (b"\x89PNG\r\n\x1a\nrest", "image/png"),
            (b"\xff\xd8\xffrest", "image/jpeg"),
            (b"GIF87arest", "image/gif"),
            (b"GIF89arest", "image/gif"),
            (b"BMrest", "image/bmp"),
            (b"RIFFxxxxWEBPrest", "image/webp"),
        ];

        for (bytes, expected) in cases {
            assert_eq!(image_mime(bytes), Some(*expected));
        }
    }

    #[test]
    fn rejects_extension_like_and_incomplete_image_bytes() {
        assert_eq!(image_mime(b"pixel.png"), None);
        assert_eq!(image_mime(b"RIFFxxxxWEB"), None);
        assert_eq!(image_mime(b"\x89PNG"), None);
    }

    #[tokio::test]
    async fn regression_view_image_rejects_signature_only_corrupt_png() {
        use std::{collections::BTreeMap, sync::Arc};

        use crate::{
            audit::AuditLogger,
            config::ConfigBuilder,
            project::{ProjectContext, ProjectKey, ProjectResolver},
            request_context::{ProjectRequestContext, TransportMode},
            storage::Storage,
            tools::{PathArgs, SharedState},
            upstream::Aggregator,
        };

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let project_root = workspace.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(
            project_root.join("broken.png"),
            b"\x89PNG\r\n\x1a\nnot-a-real-png",
        )
        .unwrap();

        let config = Arc::new(
            ConfigBuilder::from_map(BTreeMap::from([
                ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
                ("WORKSPACE_ROOT".to_owned(), workspace.display().to_string()),
            ]))
            .build()
            .unwrap(),
        );
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(workspace, storage.clone()).unwrap();
        let audit = AuditLogger::new(config.logs.clone(), config.auth_token.clone())
            .await
            .unwrap();
        let handler = AgentHandler::new(SharedState::new(
            config,
            resolver,
            storage,
            audit,
            Aggregator::default(),
        ));
        let project = ProjectContext {
            native_project_key: ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root,
            metadata_root: directory.path().join("metadata"),
            transport_mode: TransportMode::Stateless,
            mcp_session_present: false,
        };

        let response = handler
            .view_image(
                ProjectRequestContext(Ok(project)),
                Parameters(PathArgs {
                    path: "broken.png".to_owned(),
                }),
            )
            .await
            .unwrap();

        assert_eq!(
            response.is_error,
            Some(true),
            "view_image must decode bytes before reporting MCP success"
        );
    }

    #[test]
    fn codex_compatible_argument_aliases_deserialize() {
        let tree: TreeArgs = serde_json::from_value(json!({"depth": 3})).unwrap();
        assert_eq!(tree.max_depth, Some(3));
    }
}
