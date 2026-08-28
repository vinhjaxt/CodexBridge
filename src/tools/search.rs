use std::path::Path;

use globset::{Glob, GlobSetBuilder};
use regex::{Regex, RegexBuilder};
use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use super::{AgentHandler, structured_result_with_text};
use crate::{
    error::{AppError, Result as AppResult},
    ignore_rules::IgnoreMatcher,
    request_context::ProjectRequestContext,
    sandbox::PathOperation,
};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GlobArgs {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub max_results: Option<usize>,
    #[serde(default)]
    pub offset: usize,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SearchArgs {
    #[serde(alias = "pattern")]
    pub query: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub include: Option<String>,
    #[serde(default)]
    pub context: usize,
    #[serde(default, alias = "ignoreCase")]
    pub ignore_case: bool,
    #[serde(default, alias = "filesOnly")]
    pub files_only: bool,
    #[serde(default, alias = "maxResults")]
    pub max_results: Option<usize>,
    #[serde(default)]
    pub offset: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct GlobOutput {
    pub paths: Vec<String>,
    pub count: usize,
    pub traversed: usize,
    pub truncated: bool,
    pub traversal_limit_hit: bool,
    pub next_offset: Option<usize>,
    pub continuation: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct SearchMatchOutput {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_before: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_after: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct SearchOutput {
    pub matches: Vec<SearchMatchOutput>,
    pub count: usize,
    pub traversed: usize,
    pub output_bytes: usize,
    pub output_limit_bytes: usize,
    pub truncated: bool,
    pub incomplete: bool,
    pub traversal_limit_hit: bool,
    pub skipped_files: usize,
    pub next_offset: Option<usize>,
    pub continuation: Option<String>,
}

fn include_matcher(pattern: Option<&str>) -> AppResult<Option<globset::GlobMatcher>> {
    pattern
        .map(|value| {
            Glob::new(value)
                .map(|glob| glob.compile_matcher())
                .map_err(|error| AppError::new("INVALID_INPUT", error.to_string()))
        })
        .transpose()
}

fn walker<'a>(
    base: &'a Path,
    ignore: &'a IgnoreMatcher,
) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> + 'a {
    WalkDir::new(base)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(move |entry| {
            entry.depth() == 0 || !ignore.is_ignored(entry.path(), entry.file_type().is_dir())
        })
}

fn scan_glob(
    root: &Path,
    base: &Path,
    pattern: &str,
    offset: usize,
    limit: usize,
    traversed_limit: usize,
) -> AppResult<GlobOutput> {
    if limit == 0 {
        return Err(AppError::new(
            "INVALID_INPUT",
            "result limit must be positive",
        ));
    }
    let mut builder = GlobSetBuilder::new();
    builder.add(
        Glob::new(pattern).map_err(|error| AppError::new("INVALID_INPUT", error.to_string()))?,
    );
    let set = builder
        .build()
        .map_err(|error| AppError::new("INVALID_INPUT", error.to_string()))?;
    let ignore = IgnoreMatcher::for_project(root);
    let mut matches = Vec::new();
    let mut matched = 0usize;
    let mut traversed = 0usize;
    let mut page_truncated = false;
    let mut traversal_limit_hit = false;
    for entry in walker(base, &ignore) {
        let entry = entry.map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
        traversed += 1;
        if traversed > traversed_limit {
            traversal_limit_hit = true;
            break;
        }
        if entry.depth() == 0 || entry.file_type().is_dir() || entry.file_type().is_symlink() {
            continue;
        }
        let relative_to_base = entry
            .path()
            .strip_prefix(base)
            .map_err(|_| AppError::new("PATH_OUTSIDE_WORKSPACE", "glob escaped base"))?;
        if !set.is_match(relative_to_base) {
            continue;
        }
        if matched < offset {
            matched += 1;
            continue;
        }
        if matches.len() == limit {
            page_truncated = true;
            break;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| AppError::new("PATH_OUTSIDE_WORKSPACE", "glob escaped project"))?;
        matches.push(relative.to_string_lossy().replace('\\', "/"));
        matched += 1;
    }
    matches.sort();
    let next_offset =
        (page_truncated && !traversal_limit_hit).then_some(offset.saturating_add(matches.len()));
    let count = matches.len();
    Ok(GlobOutput {
        paths: matches,
        count,
        traversed,
        truncated: page_truncated || traversal_limit_hit,
        traversal_limit_hit,
        next_offset,
        continuation: if let Some(next) = next_offset {
            Some(format!("Call glob again with offset={next}."))
        } else if traversal_limit_hit {
            Some("Traversal limit reached; narrow path before continuing.".to_owned())
        } else {
            None
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn scan_content(
    root: &Path,
    base: &Path,
    regex: &Regex,
    include: Option<&str>,
    context: usize,
    files_only: bool,
    offset: usize,
    limit: usize,
    traversed_limit: usize,
    output_limit: usize,
) -> AppResult<SearchOutput> {
    scan_content_with_before_open_hook(
        root,
        base,
        regex,
        include,
        context,
        files_only,
        offset,
        limit,
        traversed_limit,
        output_limit,
        |_| {},
    )
}

#[allow(clippy::too_many_arguments)]
fn scan_content_with_before_open_hook<F>(
    root: &Path,
    base: &Path,
    regex: &Regex,
    include: Option<&str>,
    context: usize,
    files_only: bool,
    offset: usize,
    limit: usize,
    traversed_limit: usize,
    output_limit: usize,
    mut before_open: F,
) -> AppResult<SearchOutput>
where
    F: FnMut(&Path),
{
    if limit == 0 || output_limit == 0 {
        return Err(AppError::new(
            "INVALID_INPUT",
            "search result and output limits must be positive",
        ));
    }
    let include = include_matcher(include)?;
    let ignore = IgnoreMatcher::for_project(root);
    let mut results = Vec::new();
    let mut skipped = 0usize;
    let mut traversed = 0usize;
    const SEARCH_FILE_MAX_BYTES: u64 = 64 * 1024 * 1024;
    let mut page_truncated = false;
    let mut traversal_limit_hit = false;
    let mut skipped_files = 0usize;
    let mut output_bytes = 0usize;
    for entry in walker(base, &ignore) {
        let entry = entry.map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
        traversed += 1;
        if traversed > traversed_limit {
            traversal_limit_hit = true;
            break;
        }
        if !entry.file_type().is_file() || entry.file_type().is_symlink() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| AppError::new("PATH_OUTSIDE_WORKSPACE", "search escaped project"))?;
        if include
            .as_ref()
            .is_some_and(|matcher| !matcher.is_match(relative))
        {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
        if metadata.len() > SEARCH_FILE_MAX_BYTES {
            skipped_files = skipped_files.saturating_add(1);
            continue;
        }
        before_open(entry.path());
        let relative_input = relative.to_string_lossy().replace('\\', "/");
        let content = match crate::sandbox::SecurePathResolver.read_file_bounded(
            root,
            &relative_input,
            SEARCH_FILE_MAX_BYTES as usize,
        ) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(content) => content,
                Err(_) => {
                    skipped_files = skipped_files.saturating_add(1);
                    continue;
                }
            },
            Err(_) => {
                skipped_files = skipped_files.saturating_add(1);
                continue;
            }
        };
        let lines: Vec<&str> = content.lines().collect();
        let indexes: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| regex.is_match(line).then_some(index))
            .collect();
        if indexes.is_empty() {
            continue;
        }
        if files_only {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if results.len() == limit {
                page_truncated = true;
                break;
            }
            let path = relative.to_string_lossy().replace('\\', "/");
            if output_bytes.saturating_add(path.len()) > output_limit {
                if results.is_empty() {
                    return Err(AppError::new(
                        "RESOURCE_LIMIT_EXCEEDED",
                        "one search result exceeds the output budget; narrow the path or raise OUTPUT_SEARCH_BYTES",
                    ));
                }
                page_truncated = true;
                break;
            }
            output_bytes = output_bytes.saturating_add(path.len());
            results.push(SearchMatchOutput {
                path,
                line: None,
                text: None,
                context_before: None,
                context_after: None,
            });
            continue;
        }
        for index in indexes {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if results.len() == limit {
                page_truncated = true;
                break;
            }
            let start = index.saturating_sub(context);
            let end = index.saturating_add(context + 1).min(lines.len());
            let estimated = relative.as_os_str().len()
                + lines[start..end]
                    .iter()
                    .map(|line| line.len())
                    .sum::<usize>();
            if output_bytes.saturating_add(estimated) > output_limit {
                if results.is_empty() {
                    return Err(AppError::new(
                        "RESOURCE_LIMIT_EXCEEDED",
                        "one search match exceeds the output budget; narrow the query/context or use files_only",
                    ));
                }
                page_truncated = true;
                break;
            }
            output_bytes = output_bytes.saturating_add(estimated);
            results.push(SearchMatchOutput {
                path: relative.to_string_lossy().replace('\\', "/"),
                line: Some(index + 1),
                text: Some(lines[index].to_owned()),
                context_before: Some(
                    lines[start..index]
                        .iter()
                        .map(|line| (*line).to_owned())
                        .collect(),
                ),
                context_after: Some(
                    lines[index + 1..end]
                        .iter()
                        .map(|line| (*line).to_owned())
                        .collect(),
                ),
            });
        }
        if page_truncated {
            break;
        }
    }
    let next_offset =
        (page_truncated && !traversal_limit_hit).then_some(offset.saturating_add(results.len()));
    let count = results.len();
    let incomplete = traversal_limit_hit || skipped_files != 0;
    Ok(SearchOutput {
        matches: results,
        count,
        traversed,
        output_bytes,
        output_limit_bytes: output_limit,
        truncated: page_truncated || traversal_limit_hit,
        incomplete,
        traversal_limit_hit,
        skipped_files,
        next_offset,
        continuation: if let Some(next) = next_offset {
            Some(format!("Call the same search again with offset={next}."))
        } else if traversal_limit_hit {
            Some("Traversal limit reached; narrow path before continuing.".to_owned())
        } else if skipped_files != 0 {
            Some(format!(
                "Search skipped {skipped_files} unreadable, non-UTF-8, or larger-than-64-MiB file(s); narrow path or inspect them explicitly before treating results as complete."
            ))
        } else {
            None
        },
    })
}

#[tool_router(router = search_router, vis = "pub(crate)")]
impl AgentHandler {
    #[tool(
        description = "Find files matching a glob relative to an optional project subdirectory. Shared ignore rules honor defaults, .gitignore, and .git/info/exclude. Results are sorted and support offset continuation. Follow next_offset while truncated=true; traversal_limit_hit means even a zero-result page is not exhaustive."
    )]
    async fn glob(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<GlobArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let shared = self.shared.clone();
        let params = serde_json::to_value(&args).unwrap_or_default();
        self.run_content(context.0, "glob", params, move |project| async move {
            let _search = shared.permit(shared.searches.clone()).await?;
            let _cpu = shared.permit(shared.cpu.clone()).await?;
            let scoped_path = args
                .path
                .as_deref()
                .filter(|path| !path.is_empty())
                .unwrap_or(".");
            let instruction_notice = shared.scoped_instruction_notice(&project, scoped_path)?;
            let base = shared.paths.resolve_project_path(
                &project.project_root,
                scoped_path,
                PathOperation::Existing,
            )?;
            let root = project.project_root.clone();
            let limit = args
                .max_results
                .unwrap_or(shared.config.output.results)
                .min(shared.config.limits.results);
            let output = tokio::task::spawn_blocking(move || {
                scan_glob(
                    &root,
                    &base,
                    &args.pattern,
                    args.offset,
                    limit,
                    shared.config.limits.traversed_entries,
                )
            })
            .await
            .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))??;
            let mut text = output.paths.join("\n");
            if text.is_empty() {
                text = "No matching files.".to_owned();
            }
            if let Some(continuation) = output.continuation.as_deref() {
                text.push_str("\n\n");
                text.push_str(continuation);
            }
            if let Some(notice) = instruction_notice {
                text = format!("{notice}\n{text}");
            }
            let value = serde_json::to_value(&output).unwrap_or_default();
            Ok((structured_result_with_text(value.clone(), text), value))
        })
        .await
    }

    #[tool(
        description = "Search UTF-8 files in the active project with a Rust regular expression. Supports include glob, ignoreCase, context, filesOnly, shared nested .gitignore rules, and offset continuation. If incomplete=true, one or more files could not be searched or the traversal cap was reached; do not treat zero/partial matches as exhaustive."
    )]
    async fn grep(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.search_impl(context, "grep", args).await
    }

    async fn search_impl(
        &self,
        context: ProjectRequestContext,
        tool: &'static str,
        args: SearchArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let regex = RegexBuilder::new(&args.query)
            .case_insensitive(args.ignore_case)
            .build()
            .map_err(|error| AppError::new("INVALID_INPUT", error.to_string()));
        let regex = match regex {
            Ok(regex) => regex,
            Err(error) => return Ok(super::error_result(&error)),
        };
        let shared = self.shared.clone();
        let params = serde_json::to_value(&args).unwrap_or_default();
        self.run_content(context.0, tool, params, move |project| async move {
            let _search = shared.permit(shared.searches.clone()).await?;
            let _cpu = shared.permit(shared.cpu.clone()).await?;
            let scoped_path = args
                .path
                .as_deref()
                .filter(|path| !path.is_empty())
                .unwrap_or(".");
            let instruction_notice = shared.scoped_instruction_notice(&project, scoped_path)?;
            let base = shared.paths.resolve_project_path(
                &project.project_root,
                scoped_path,
                PathOperation::Existing,
            )?;
            let root = project.project_root.clone();
            let limit = args
                .max_results
                .unwrap_or(shared.config.output.results)
                .min(shared.config.limits.results);
            let output = tokio::task::spawn_blocking(move || {
                scan_content(
                    &root,
                    &base,
                    &regex,
                    args.include.as_deref(),
                    args.context.min(100),
                    args.files_only,
                    args.offset,
                    limit,
                    shared.config.limits.traversed_entries,
                    shared.config.output.search_bytes,
                )
            })
            .await
            .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))??;
            let mut lines = Vec::new();
            for item in &output.matches {
                if let Some(line) = item.line {
                    lines.push(format!(
                        "{}:{line}:{}",
                        item.path,
                        item.text.as_deref().unwrap_or_default()
                    ));
                } else {
                    lines.push(item.path.clone());
                }
            }
            let mut text = if lines.is_empty() {
                "No matches.".to_owned()
            } else {
                lines.join("\n")
            };
            if let Some(continuation) = output.continuation.as_deref() {
                text.push_str("\n\n");
                text.push_str(continuation);
            }
            if let Some(notice) = instruction_notice {
                text = format!("{notice}\n{text}");
            }
            let value = serde_json::to_value(&output).unwrap_or_default();
            Ok((structured_result_with_text(value.clone(), text), value))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_honors_ignore_context_and_continuation() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::create_dir_all(temp.path().join("target")).unwrap();
        std::fs::write(
            temp.path().join("src/a.rs"),
            "before one\nNeedle one\nafter one\nbefore two\nNeedle two\nafter two\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("target/hidden.rs"), "Needle\n").unwrap();
        let regex = RegexBuilder::new("needle")
            .case_insensitive(true)
            .build()
            .unwrap();
        let first = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            Some("**/*.rs"),
            1,
            false,
            0,
            1,
            100,
            4096,
        )
        .unwrap();
        assert_eq!(first.count, 1);
        assert_eq!(first.matches[0].path, "src/a.rs");
        assert_eq!(first.matches[0].line, Some(2));
        assert_eq!(
            first.matches[0].context_before.as_ref().unwrap()[0],
            "before one"
        );
        assert!(first.truncated);
        assert_eq!(first.next_offset, Some(1));
        assert_eq!(
            first.continuation.as_deref(),
            Some("Call the same search again with offset=1.")
        );
        assert!(
            first
                .matches
                .iter()
                .all(|item| item.path != "target/hidden.rs")
        );

        let second = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            Some("**/*.rs"),
            1,
            false,
            first.next_offset.unwrap(),
            1,
            100,
            4096,
        )
        .unwrap();
        assert_eq!(second.count, 1);
        assert_eq!(second.matches[0].path, "src/a.rs");
        assert_eq!(second.matches[0].line, Some(5));
        assert_eq!(second.matches[0].text.as_deref(), Some("Needle two"));
        assert!(!second.truncated);
        assert_eq!(second.next_offset, None);
        assert_eq!(second.continuation, None);
        assert!(
            second
                .matches
                .iter()
                .all(|item| item.path != "target/hidden.rs")
        );
    }

    #[test]
    fn glob_is_relative_to_base_and_sorted() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src/a")).unwrap();
        for path in ["src/z.rs", "src/a/z.rs", "src/a.rs", "src/m.rs"] {
            std::fs::write(temp.path().join(path), "").unwrap();
        }
        std::fs::write(temp.path().join("outside.rs"), "").unwrap();

        let value =
            scan_glob(temp.path(), &temp.path().join("src"), "**/*.rs", 0, 10, 100).unwrap();

        assert_eq!(
            value.paths,
            vec!["src/a.rs", "src/a/z.rs", "src/m.rs", "src/z.rs"]
        );
        assert!(value.paths.iter().all(|path| path.starts_with("src/")));
    }

    #[test]
    fn zero_result_limits_are_rejected_instead_of_returning_stuck_continuations() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "needle\n").unwrap();
        assert_eq!(
            scan_glob(temp.path(), temp.path(), "**/*.rs", 0, 0, 100)
                .unwrap_err()
                .code(),
            "INVALID_INPUT"
        );
        let regex = Regex::new("needle").unwrap();
        assert_eq!(
            scan_content(
                temp.path(),
                temp.path(),
                &regex,
                None,
                0,
                false,
                0,
                0,
                100,
                1024,
            )
            .unwrap_err()
            .code(),
            "INVALID_INPUT"
        );
    }

    #[test]
    fn files_only_returns_each_matching_file_once() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "needle\nneedle\n").unwrap();
        let regex = Regex::new("needle").unwrap();
        let value = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            None,
            0,
            true,
            0,
            10,
            100,
            1024,
        )
        .unwrap();
        assert_eq!(value.count, 1);
        assert_eq!(value.matches[0].path, "a.rs");
        assert!(value.matches[0].line.is_none());
    }

    #[test]
    fn search_offset_continues_without_repeating_matches() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "needle one\nneedle two\n").unwrap();
        let regex = Regex::new("needle").unwrap();
        let first = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            None,
            0,
            false,
            0,
            1,
            100,
            4096,
        )
        .unwrap();
        assert_eq!(first.matches[0].line, Some(1));
        assert_eq!(first.next_offset, Some(1));
        let second = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            None,
            0,
            false,
            1,
            1,
            100,
            4096,
        )
        .unwrap();
        assert_eq!(second.matches[0].line, Some(2));
    }

    #[test]
    fn oversized_single_match_fails_instead_of_looping_at_same_offset() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("a.rs"),
            format!("needle {}\n", "x".repeat(1024)),
        )
        .unwrap();
        let regex = Regex::new("needle").unwrap();
        let error = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            None,
            0,
            false,
            0,
            10,
            100,
            32,
        )
        .unwrap_err();
        assert_eq!(error.code(), "RESOURCE_LIMIT_EXCEEDED");
    }

    #[test]
    fn glob_respects_default_ignores_and_gitignore() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::create_dir_all(temp.path().join("node_modules/pkg")).unwrap();
        std::fs::write(temp.path().join("src/a.ts"), "").unwrap();
        std::fs::write(temp.path().join("node_modules/pkg/hidden.ts"), "").unwrap();
        std::fs::write(temp.path().join("secret.ts"), "").unwrap();
        std::fs::write(temp.path().join(".gitignore"), "secret.ts\n").unwrap();

        let value = scan_glob(temp.path(), temp.path(), "**/*.ts", 0, 20, 100).unwrap();
        assert_eq!(value.paths, vec!["src/a.ts"]);
    }

    #[test]
    fn glob_offset_and_limit_form_a_stable_sorted_window() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["c.rs", "a.rs", "b.rs"] {
            std::fs::write(temp.path().join(name), "").unwrap();
        }
        let first = scan_glob(temp.path(), temp.path(), "*.rs", 0, 2, 100).unwrap();
        assert_eq!(first.paths, vec!["a.rs", "b.rs"]);
        assert!(first.truncated);
        assert_eq!(first.next_offset, Some(2));

        let second = scan_glob(temp.path(), temp.path(), "*.rs", 2, 2, 100).unwrap();
        assert_eq!(second.paths, vec!["c.rs"]);
        assert!(!second.truncated);
        assert_eq!(second.next_offset, None);
    }

    #[test]
    fn traversal_limit_requires_narrowing_instead_of_repeating_same_offset() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            std::fs::write(temp.path().join(name), "needle\n").unwrap();
        }
        let glob = scan_glob(temp.path(), temp.path(), "*.txt", 0, 10, 3).unwrap();
        assert!(glob.traversal_limit_hit);
        assert!(glob.truncated);
        assert_eq!(glob.next_offset, None);
        assert!(glob.continuation.as_deref().unwrap().contains("narrow"));

        let regex = Regex::new("needle").unwrap();
        let search = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            None,
            0,
            false,
            0,
            10,
            3,
            4096,
        )
        .unwrap();
        assert!(search.traversal_limit_hit);
        assert!(search.incomplete);
        assert_eq!(search.next_offset, None);
    }

    #[test]
    fn nested_gitignore_is_honored_by_glob_and_search() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("sub")).unwrap();
        std::fs::write(temp.path().join("sub/.gitignore"), "hidden.txt\n").unwrap();
        std::fs::write(temp.path().join("sub/hidden.txt"), "needle\n").unwrap();
        std::fs::write(temp.path().join("sub/visible.txt"), "needle\n").unwrap();

        let glob = scan_glob(temp.path(), temp.path(), "**/*.txt", 0, 10, 100).unwrap();
        assert_eq!(glob.paths, vec!["sub/visible.txt"]);

        let regex = Regex::new("needle").unwrap();
        let search = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            None,
            0,
            true,
            0,
            10,
            100,
            4096,
        )
        .unwrap();
        assert_eq!(search.matches.len(), 1);
        assert_eq!(search.matches[0].path, "sub/visible.txt");
    }

    #[test]
    fn unreadable_text_marks_search_incomplete_instead_of_false_complete() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("binary.dat"), [0xff, 0xfe, 0xfd]).unwrap();
        let regex = Regex::new("needle").unwrap();
        let value = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            None,
            0,
            false,
            0,
            10,
            100,
            4096,
        )
        .unwrap();
        assert!(value.incomplete);
        assert_eq!(value.skipped_files, 1);
        assert!(value.continuation.as_deref().unwrap().contains("skipped 1"));
    }

    #[test]
    fn invalid_glob_and_include_patterns_fail_explicitly() {
        let temp = tempfile::tempdir().unwrap();
        let glob_error = scan_glob(temp.path(), temp.path(), "src/[abc", 0, 10, 100).unwrap_err();
        assert_eq!(glob_error.code(), "INVALID_INPUT");

        let regex = Regex::new("needle").unwrap();
        let include_error = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            Some("[broken"),
            0,
            false,
            0,
            10,
            100,
            4096,
        )
        .unwrap_err();
        assert_eq!(include_error.code(), "INVALID_INPUT");
    }

    #[test]
    fn search_include_filter_and_context_are_bounded_to_matching_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::create_dir_all(temp.path().join("docs")).unwrap();
        std::fs::write(temp.path().join("src/a.rs"), "before\nneedle\nafter\n").unwrap();
        std::fs::write(temp.path().join("docs/a.md"), "needle\n").unwrap();
        let regex = Regex::new("needle").unwrap();
        let value = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            Some("**/*.rs"),
            1,
            false,
            0,
            10,
            100,
            4096,
        )
        .unwrap();
        assert_eq!(value.count, 1);
        assert_eq!(value.matches[0].path, "src/a.rs");
        assert_eq!(
            value.matches[0].context_before.as_deref(),
            Some(&["before".to_owned()][..])
        );
        assert_eq!(
            value.matches[0].context_after.as_deref(),
            Some(&["after".to_owned()][..])
        );
    }

    #[test]
    fn search_case_sensitivity_is_driven_by_the_compiled_regex() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.txt"), "Needle\n").unwrap();
        let sensitive = RegexBuilder::new("needle")
            .case_insensitive(false)
            .build()
            .unwrap();
        let insensitive = RegexBuilder::new("needle")
            .case_insensitive(true)
            .build()
            .unwrap();
        let strict = scan_content(
            temp.path(),
            temp.path(),
            &sensitive,
            None,
            0,
            false,
            0,
            10,
            100,
            4096,
        )
        .unwrap();
        let folded = scan_content(
            temp.path(),
            temp.path(),
            &insensitive,
            None,
            0,
            false,
            0,
            10,
            100,
            4096,
        )
        .unwrap();
        assert_eq!(strict.count, 0);
        assert_eq!(folded.count, 1);
    }

    #[test]
    fn files_only_pagination_advances_by_matching_file_not_matching_line() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "needle\nneedle\n").unwrap();
        std::fs::write(temp.path().join("b.rs"), "needle\n").unwrap();
        let regex = Regex::new("needle").unwrap();
        let first = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            None,
            0,
            true,
            0,
            1,
            100,
            4096,
        )
        .unwrap();
        assert_eq!(first.matches[0].path, "a.rs");
        assert_eq!(first.next_offset, Some(1));
        let second = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            None,
            0,
            true,
            1,
            1,
            100,
            4096,
        )
        .unwrap();
        assert_eq!(second.matches[0].path, "b.rs");
    }

    #[test]
    fn traversal_limit_marks_search_as_truncated() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..10 {
            std::fs::write(temp.path().join(format!("f{index}.txt")), "needle\n").unwrap();
        }
        let regex = Regex::new("needle").unwrap();
        let value = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            None,
            0,
            false,
            0,
            20,
            2,
            4096,
        )
        .unwrap();
        assert!(value.truncated);
        assert!(value.incomplete);
        assert!(value.traversal_limit_hit);
        assert!(value.traversed > 2);
        assert_eq!(value.next_offset, None);
    }

    #[test]
    fn invalid_utf8_files_are_skipped_without_poisoning_other_matches() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("bad.txt"), b"needle\xff\n").unwrap();
        std::fs::write(temp.path().join("good.txt"), "needle\n").unwrap();
        let regex = Regex::new("needle").unwrap();
        let value = scan_content(
            temp.path(),
            temp.path(),
            &regex,
            None,
            0,
            false,
            0,
            10,
            100,
            4096,
        )
        .unwrap();
        assert_eq!(value.count, 1);
        assert_eq!(value.matches[0].path, "good.txt");
    }

    #[cfg(unix)]
    #[test]
    fn regression_search_check_then_path_open_can_follow_swapped_symlink() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let candidate = project.path().join("candidate.txt");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&candidate, "safe\n").unwrap();
        std::fs::write(&secret, "outside-secret\n").unwrap();

        let regex = Regex::new("outside-secret").unwrap();
        let mut swapped = false;
        let output = scan_content_with_before_open_hook(
            project.path(),
            project.path(),
            &regex,
            None,
            0,
            false,
            0,
            10,
            100,
            4096,
            |path| {
                if !swapped && path == candidate {
                    swapped = true;
                    std::fs::remove_file(&candidate).unwrap();
                    symlink(&secret, &candidate).unwrap();
                }
            },
        )
        .unwrap();

        assert!(swapped, "test hook never reached the validated candidate");
        assert!(
            output.matches.is_empty(),
            "grep exposed content through a symlink swapped in after traversal validation: {:?}",
            output.matches
        );
    }
}
