use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{AgentHandler, ReadFileArgs, structured_result_with_text};
use crate::{error::AppError, request_context::ProjectRequestContext, sandbox::PathOperation};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct ListDirectoryArgs {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct DirectoryEntryOutput {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct ListDirectoryOutput {
    pub path: String,
    pub entries: Vec<DirectoryEntryOutput>,
    pub count: usize,
    pub traversed: usize,
    pub truncated: bool,
    pub traversal_limit_hit: bool,
    pub next_offset: Option<usize>,
    pub continuation: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct ReadFileOutput {
    pub path: String,
    pub bytes: usize,
    pub total_lines: usize,
    pub content: String,
    pub offset: usize,
    pub line_byte_offset: usize,
    pub shown_lines: usize,
    pub truncated: bool,
    pub next_offset: Option<usize>,
    pub next_line_byte_offset: Option<usize>,
    pub continuation: Option<String>,
}

#[derive(Debug)]
struct RenderedFileWindow {
    content: String,
    shown_lines: usize,
    truncated: bool,
    next_offset: Option<usize>,
    next_line_byte_offset: Option<usize>,
    continuation: Option<String>,
}

#[cfg(test)]
fn render_file_window(
    lines: &[&str],
    offset: usize,
    line_byte_offset: usize,
    requested_lines: usize,
    byte_budget: usize,
) -> Result<RenderedFileWindow, AppError> {
    if requested_lines == 0 {
        return Err(AppError::new("INVALID_INPUT", "limit must be positive"));
    }
    if offset > lines.len() {
        return Err(AppError::new("INVALID_INPUT", "offset is outside the file"));
    }
    if offset == lines.len() {
        if line_byte_offset != 0 {
            return Err(AppError::new(
                "INVALID_INPUT",
                "line_byte_offset requires an existing line",
            ));
        }
        return Ok(RenderedFileWindow {
            content: String::new(),
            shown_lines: 0,
            truncated: false,
            next_offset: None,
            next_line_byte_offset: None,
            continuation: None,
        });
    }
    render_file_window_at(
        &lines[offset..],
        offset,
        line_byte_offset,
        requested_lines,
        byte_budget,
    )
}

fn render_file_window_at(
    lines: &[&str],
    line_index_base: usize,
    line_byte_offset: usize,
    requested_lines: usize,
    byte_budget: usize,
) -> Result<RenderedFileWindow, AppError> {
    if requested_lines == 0 {
        return Err(AppError::new("INVALID_INPUT", "limit must be positive"));
    }
    if lines.is_empty() {
        return Ok(RenderedFileWindow {
            content: String::new(),
            shown_lines: 0,
            truncated: false,
            next_offset: None,
            next_line_byte_offset: None,
            continuation: None,
        });
    }
    let first_line = lines[0];
    if line_byte_offset > first_line.len() || !first_line.is_char_boundary(line_byte_offset) {
        return Err(AppError::new(
            "INVALID_INPUT",
            "line_byte_offset is outside the line or not a UTF-8 boundary",
        ));
    }

    let mut rendered = Vec::new();
    let mut rendered_bytes = 0usize;
    let mut fully_consumed = 0usize;
    let mut shown_lines = 0usize;

    for (relative, line) in lines.iter().enumerate().take(requested_lines) {
        let line_start = if relative == 0 { line_byte_offset } else { 0 };
        let remaining = &line[line_start..];
        let absolute_line = line_index_base.saturating_add(relative);
        let prefix = format!("{}\t", absolute_line + 1);
        let separator_bytes = usize::from(!rendered.is_empty());
        let full_bytes = separator_bytes
            .saturating_add(prefix.len())
            .saturating_add(remaining.len());

        if rendered_bytes.saturating_add(full_bytes) <= byte_budget {
            rendered_bytes = rendered_bytes.saturating_add(full_bytes);
            rendered.push(format!("{prefix}{remaining}"));
            shown_lines = shown_lines.saturating_add(1);
            fully_consumed = fully_consumed.saturating_add(1);
            continue;
        }

        // If previous full lines already consumed the presentation budget, do
        // not start another line. Continue from that line on the next call.
        if !rendered.is_empty() {
            let next_offset = absolute_line;
            return Ok(RenderedFileWindow {
                content: rendered.join("\n"),
                shown_lines,
                truncated: true,
                next_offset: Some(next_offset),
                next_line_byte_offset: None,
                continuation: Some(format!("Call read_file again with offset={next_offset}.")),
            });
        }

        // A single logical line exceeds the whole presentation budget. Return
        // a UTF-8-safe byte prefix and keep a byte cursor inside the same line
        // so no portion of minified/generated source is lost.
        if prefix.len() > byte_budget {
            return Err(AppError::new(
                "RESOURCE_LIMIT_EXCEEDED",
                "file output budget is too small to include the line-number prefix",
            ));
        }
        let available = byte_budget.saturating_sub(prefix.len());
        if available == 0 && !remaining.is_empty() {
            return Err(AppError::new(
                "RESOURCE_LIMIT_EXCEEDED",
                "file output budget is too small to make progress on this line",
            ));
        }
        let mut end = line_start.saturating_add(available).min(line.len());
        while end > line_start && !line.is_char_boundary(end) {
            end -= 1;
        }
        if end == line_start && !remaining.is_empty() {
            return Err(AppError::new(
                "RESOURCE_LIMIT_EXCEEDED",
                "file output budget is too small to include the next UTF-8 character",
            ));
        }

        rendered.push(format!("{prefix}{}", &line[line_start..end]));
        shown_lines = 1;
        if end < line.len() {
            return Ok(RenderedFileWindow {
                content: rendered.join("\n"),
                shown_lines,
                truncated: true,
                next_offset: Some(absolute_line),
                next_line_byte_offset: Some(end),
                continuation: Some(format!(
                    "Call read_file again with offset={absolute_line} and line_byte_offset={end} to continue the same line."
                )),
            });
        }

        // The remaining suffix of a previously-partial line fit exactly, but
        // no room remains for another line. Continue at the next line.
        fully_consumed = 1;
        break;
    }

    let next_offset = line_index_base.saturating_add(fully_consumed);
    let truncated = fully_consumed < lines.len();
    Ok(RenderedFileWindow {
        content: rendered.join("\n"),
        shown_lines,
        truncated,
        next_offset: truncated.then_some(next_offset),
        next_line_byte_offset: None,
        continuation: truncated
            .then_some(format!("Call read_file again with offset={next_offset}.")),
    })
}

type FileLayout = (u64, usize, Option<(u64, u64)>);

const FILE_LAYOUT_SCAN_CHUNK: usize = 256 * 1024;

#[cfg(test)]
fn scan_file_layout(
    paths: &crate::sandbox::SecurePathResolver,
    root: &std::path::Path,
    input: &str,
    target_line: usize,
) -> Result<FileLayout, AppError> {
    let mut file = paths.open_regular_file(root, input)?;
    let total_bytes = file.metadata()?.len();
    scan_open_file_layout(&mut file, total_bytes, target_line)
}

fn scan_open_file_layout<R>(
    file: &mut R,
    total_bytes: u64,
    target_line: usize,
) -> Result<FileLayout, AppError>
where
    R: std::io::Read + std::io::Seek,
{
    use std::io::SeekFrom;

    file.seek(SeekFrom::Start(0))?;
    let mut absolute = 0u64;
    let mut current_line = 0usize;
    let mut target_start = (target_line == 0).then_some(0u64);
    let mut target_end = None;
    let mut previous_byte = None;
    let mut buffer = vec![0_u8; FILE_LAYOUT_SCAN_CHUNK];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        for (index, byte) in buffer[..read].iter().enumerate() {
            if *byte != b'\n' {
                previous_byte = Some(*byte);
                continue;
            }
            let position = absolute + index as u64;
            if current_line == target_line {
                target_end = Some(if previous_byte == Some(b'\r') {
                    position.saturating_sub(1)
                } else {
                    position
                });
            }
            current_line = current_line.saturating_add(1);
            if current_line == target_line {
                target_start = Some(position + 1);
            }
            previous_byte = Some(*byte);
        }
        absolute = absolute.saturating_add(read as u64);
        if absolute >= total_bytes {
            break;
        }
    }
    let total_lines = current_line.saturating_add(1);
    if target_line > total_lines {
        return Err(AppError::new("INVALID_INPUT", "offset is outside the file"));
    }
    if target_line == total_lines {
        return Ok((total_bytes, total_lines, None));
    }
    let start = target_start.unwrap_or(total_bytes);
    let end = target_end.unwrap_or(total_bytes);
    Ok((total_bytes, total_lines, Some((start, end))))
}

#[allow(clippy::too_many_arguments)]
fn read_file_window_with_after_scan_hook<F>(
    paths: &crate::sandbox::SecurePathResolver,
    root: &std::path::Path,
    input: &str,
    line_offset: usize,
    line_byte_offset: usize,
    requested_lines: usize,
    byte_budget: usize,
    after_scan: F,
) -> Result<(u64, usize, RenderedFileWindow), AppError>
where
    F: FnOnce(),
{
    use std::io::{Read as _, Seek as _, SeekFrom};

    // Keep one descriptor for the entire scan/render operation. An atomic
    // pathname replacement after the scan must not splice bytes from a new
    // inode into metadata derived from the old one.
    let mut file = paths.open_regular_file(root, input)?;
    let total_bytes = file.metadata()?.len();
    let (total_bytes, total_lines, target) =
        scan_open_file_layout(&mut file, total_bytes, line_offset)?;
    after_scan();
    let window = if let Some((line_start, line_end)) = target {
        let line_length = line_end.saturating_sub(line_start);
        if line_byte_offset as u64 > line_length {
            return Err(AppError::new(
                "INVALID_INPUT",
                "line_byte_offset is outside the line or not a UTF-8 boundary",
            ));
        }
        let range_start = line_start.saturating_add(line_byte_offset as u64);
        let wanted = byte_budget
            .saturating_add(requested_lines)
            .saturating_add(4);
        file.seek(SeekFrom::Start(range_start))?;
        let available = total_bytes.saturating_sub(range_start) as usize;
        let mut bytes = Vec::with_capacity(wanted.min(available));
        file.take(wanted as u64).read_to_end(&mut bytes)?;
        if (line_byte_offset as u64) < line_length
            && bytes
                .first()
                .is_some_and(|byte| byte & 0b1100_0000 == 0b1000_0000)
        {
            return Err(AppError::new(
                "INVALID_INPUT",
                "line_byte_offset is outside the line or not a UTF-8 boundary",
            ));
        }
        let content = match std::str::from_utf8(&bytes) {
            Ok(content) => content,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()]).map_err(|error| {
                    AppError::new("PROCESS_FAILED", format!("UTF-8 read failed: {error}"))
                })?
            }
            Err(error) => {
                return Err(AppError::new(
                    "PROCESS_FAILED",
                    format!("UTF-8 read failed: {error}"),
                ));
            }
        };
        let mut lines = content.split('\n').collect::<Vec<_>>();
        let terminated_lines = lines.len().saturating_sub(1);
        for line in &mut lines[..terminated_lines] {
            *line = line.strip_suffix('\r').unwrap_or(line);
        }
        let mut window =
            render_file_window_at(&lines, line_offset, 0, requested_lines, byte_budget)?;
        if window.next_offset == Some(line_offset)
            && let Some(next) = window.next_line_byte_offset.as_mut()
        {
            *next = line_byte_offset.saturating_add(*next);
            window.continuation = Some(format!(
                "Call read_file again with offset={line_offset} and line_byte_offset={} to continue the same line.",
                *next
            ));
        }
        window
    } else {
        if line_byte_offset != 0 {
            return Err(AppError::new(
                "INVALID_INPUT",
                "line_byte_offset requires an existing line",
            ));
        }
        RenderedFileWindow {
            content: String::new(),
            shown_lines: 0,
            truncated: false,
            next_offset: None,
            next_line_byte_offset: None,
            continuation: None,
        }
    };
    Ok((total_bytes, total_lines, window))
}

#[tool_router(router = filesystem_router, vis = "pub(crate)")]
impl AgentHandler {
    #[tool(
        description = "List sorted, non-ignored entries in a project-relative directory. Defaults, .gitignore, and .git/info/exclude are honored. Use offset/max_results for bounded pages and follow next_offset while truncated=true; traversal_limit_hit means the visible result is not exhaustive."
    )]
    async fn list_directory(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<ListDirectoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let shared = self.shared.clone();
        let params = serde_json::to_value(&args).unwrap();
        self.run_content(
            context.0,
            "list_directory",
            params,
            move |project| async move {
                let input = args
                    .path
                    .as_deref()
                    .filter(|path| !path.is_empty())
                    .unwrap_or(".");
                let instruction_notice = shared.scoped_instruction_notice(&project, input)?;
                let path = shared.paths.resolve_project_path(
                    &project.project_root,
                    input,
                    PathOperation::Existing,
                )?;
                let matcher =
                    crate::ignore_rules::IgnoreMatcher::for_project(&project.project_root);
                let target_ignored = matcher.is_ignored(&path, true);
                let mut reader = tokio::fs::read_dir(path).await?;
                let mut all = Vec::new();
                let mut traversed = 0usize;
                let mut traversal_limit_hit = false;
                while let Some(entry) = reader.next_entry().await? {
                    traversed = traversed.saturating_add(1);
                    if traversed > shared.config.limits.traversed_entries {
                        traversal_limit_hit = true;
                        break;
                    }
                    let file_type = entry.file_type().await?;
                    if !target_ignored && matcher.is_ignored(&entry.path(), file_type.is_dir()) {
                        continue;
                    }
                    all.push((
                        entry.file_name().to_string_lossy().into_owned(),
                        if file_type.is_dir() {
                            "directory"
                        } else if file_type.is_symlink() {
                            "symlink"
                        } else {
                            "file"
                        },
                        entry.path(),
                    ));
                }
                all.sort_by(|left, right| left.0.cmp(&right.0));
                let total = all.len();
                let maximum = args
                    .max_results
                    .unwrap_or(shared.config.output.results)
                    .min(shared.config.limits.results);
                if maximum == 0 {
                    return Err(AppError::new(
                        "INVALID_INPUT",
                        "max_results must be positive",
                    ));
                }
                let selected: Vec<_> = all.into_iter().skip(args.offset).take(maximum).collect();
                let mut entries = Vec::with_capacity(selected.len());
                let mut lines = Vec::with_capacity(selected.len());
                for (name, kind, path) in selected {
                    let bytes = if kind == "file" {
                        tokio::fs::metadata(path)
                            .await
                            .ok()
                            .map(|value| value.len())
                    } else {
                        None
                    };
                    lines.push(match bytes {
                        Some(bytes) => format!("{kind}\t{bytes}\t{name}"),
                        None => format!("{kind}\t-\t{name}"),
                    });
                    entries.push(DirectoryEntryOutput {
                        name,
                        kind: kind.to_owned(),
                        bytes,
                    });
                }
                let count = entries.len();
                let page_truncated = args.offset.saturating_add(count) < total;
                let truncated = traversal_limit_hit || page_truncated;
                let next_offset = (!traversal_limit_hit && page_truncated)
                    .then_some(args.offset.saturating_add(count));
                let output = ListDirectoryOutput {
                    path: input.to_owned(),
                    entries,
                    count,
                    traversed,
                    truncated,
                    traversal_limit_hit,
                    next_offset,
                    continuation: if let Some(next) = next_offset {
                        Some(format!("Call list_directory again with offset={next}."))
                    } else if traversal_limit_hit {
                        Some("Traversal limit reached; inspect a narrower directory.".to_owned())
                    } else {
                        None
                    },
                };
                let value = serde_json::to_value(&output).unwrap_or_default();
                let mut text = if lines.is_empty() {
                    "Directory is empty.".to_owned()
                } else {
                    lines.join("\n")
                };
                if let Some(continuation) = value["continuation"].as_str() {
                    text.push_str("\n\n");
                    text.push_str(continuation);
                }
                if let Some(notice) = instruction_notice {
                    text = format!("{notice}\n{text}");
                }
                Ok((structured_result_with_text(value.clone(), text), value))
            },
        )
        .await
    }

    #[tool(
        description = "Read a project-relative UTF-8 file using Codex-style line windows. offset is the 0-based starting line; limit is the maximum line count. Very long single lines continue losslessly with line_byte_offset. When truncated=true, follow the returned next_offset and next_line_byte_offset exactly rather than deriving cursors. max_bytes optionally raises the per-call presentation window from OUTPUT_FILE_BYTES up to OUTPUT_MULTI_FILE_BYTES. Returned text is prefixed with 1-based line numbers."
    )]
    async fn read_file(
        &self,
        context: ProjectRequestContext,
        Parameters(args): Parameters<ReadFileArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let shared = self.shared.clone();
        let params = serde_json::to_value(&args).unwrap();
        self.run_content(context.0, "read_file", params, move |project| async move {
            let instruction_notice = shared.scoped_instruction_notice(&project, &args.path)?;
            let paths = shared.paths.clone();
            let root = project.project_root.clone();
            let input = args.path.clone();
            let requested_lines = args.limit.unwrap_or(1000).min(10_000);
            let byte_budget = match args.max_bytes {
                Some(0) => {
                    return Err(AppError::new(
                        "INVALID_INPUT",
                        "max_bytes must be greater than zero",
                    ));
                }
                Some(requested) if requested > shared.config.output.multi_file_bytes => {
                    return Err(AppError::new(
                        "INVALID_INPUT",
                        format!(
                            "max_bytes exceeds the per-call presentation ceiling of {} bytes",
                            shared.config.output.multi_file_bytes
                        ),
                    ));
                }
                Some(requested) => requested,
                None => shared.config.output.file_bytes,
            };
            let line_offset = args.offset;
            let line_byte_offset = args.line_byte_offset;
            let (total_bytes, total_lines, window) = tokio::task::spawn_blocking(move || {
                read_file_window_with_after_scan_hook(
                    &paths,
                    &root,
                    &input,
                    line_offset,
                    line_byte_offset,
                    requested_lines,
                    byte_budget,
                    || {},
                )
            })
            .await
            .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))??;
            let total_bytes = usize::try_from(total_bytes).unwrap_or(usize::MAX);
            let output = ReadFileOutput {
                path: args.path.clone(),
                bytes: total_bytes,
                total_lines,
                content: window.content,
                offset: args.offset,
                line_byte_offset: args.line_byte_offset,
                shown_lines: window.shown_lines,
                truncated: window.truncated,
                next_offset: window.next_offset,
                next_line_byte_offset: window.next_line_byte_offset,
                continuation: window.continuation,
            };
            let value = serde_json::to_value(&output).unwrap_or_default();
            let mut text = format!(
                "{} ({} bytes, {} lines; window starts at line {}, byte {}; {} logical line(s) shown)\n\n{}",
                value["path"].as_str().unwrap_or("file"),
                total_bytes,
                total_lines,
                args.offset.saturating_add(1),
                args.line_byte_offset,
                output.shown_lines,
                output.content
            );
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_single_line_continues_by_utf8_byte_cursor_without_loss() {
        let line = format!("{}END", "🙂".repeat(32));
        let lines = vec![line.as_str()];
        let mut offset = 0usize;
        let mut collected = String::new();
        let mut calls = 0usize;
        loop {
            calls += 1;
            let window = render_file_window(&lines, 0, offset, 1000, 31).unwrap();
            let chunk = window.content.split_once('\t').unwrap().1;
            collected.push_str(chunk);
            if !window.truncated {
                break;
            }
            assert_eq!(window.next_offset, Some(0));
            offset = window
                .next_line_byte_offset
                .expect("same-line continuation byte cursor");
            assert!(line.is_char_boundary(offset));
            assert!(calls < 32, "continuation must make progress");
        }
        assert_eq!(collected, line);
    }

    #[test]
    fn normal_line_windows_keep_line_offset_continuation() {
        let lines = vec!["one", "two", "three"];
        let window = render_file_window(&lines, 0, 0, 2, 1024).unwrap();
        assert_eq!(window.content, "1\tone\n2\ttwo");
        assert!(window.truncated);
        assert_eq!(window.next_offset, Some(2));
        assert_eq!(window.next_line_byte_offset, None);
    }

    #[test]
    fn line_byte_offset_must_be_utf8_boundary() {
        let lines = vec!["🙂abc"];
        let error = render_file_window(&lines, 0, 1, 1, 1024).unwrap_err();
        assert_eq!(error.code(), "INVALID_INPUT");
    }

    #[test]
    fn small_file_returns_whole_window_without_continuation() {
        let lines = vec!["alpha", "beta"];
        let window = render_file_window(&lines, 0, 0, 100, 1024).unwrap();
        assert_eq!(window.content, "1\talpha\n2\tbeta");
        assert_eq!(window.shown_lines, 2);
        assert!(!window.truncated);
        assert_eq!(window.next_offset, None);
        assert_eq!(window.continuation, None);
    }

    #[test]
    fn offset_and_requested_line_limit_select_expected_window() {
        let lines = vec!["one", "two", "three", "four"];
        let window = render_file_window(&lines, 1, 0, 2, 1024).unwrap();
        assert_eq!(window.content, "2\ttwo\n3\tthree");
        assert_eq!(window.shown_lines, 2);
        assert_eq!(window.next_offset, Some(3));
    }

    #[test]
    fn file_layout_scans_files_larger_than_write_limit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.txt");
        let mut file = std::fs::File::create(&path).unwrap();
        use std::io::Write as _;
        writeln!(file, "first").unwrap();
        file.write_all(&vec![b'x'; 9 * 1024 * 1024]).unwrap();
        writeln!(file).unwrap();
        writeln!(file, "last").unwrap();
        drop(file);

        let resolver = crate::sandbox::SecurePathResolver;
        let (bytes, total_lines, target) =
            scan_file_layout(&resolver, temp.path(), "large.txt", 2).unwrap();
        assert!(bytes > 8 * 1024 * 1024);
        assert_eq!(total_lines, 4);
        let (start, end) = target.expect("third line");
        let (line, _) = resolver
            .read_file_range(temp.path(), "large.txt", start, (end - start) as usize)
            .unwrap();
        assert_eq!(line, b"last");
    }

    #[test]
    fn file_layout_scan_requests_bounded_chunks() {
        struct TrackingReader {
            inner: std::io::Cursor<Vec<u8>>,
            largest_request: usize,
            read_requests: usize,
        }

        impl std::io::Read for TrackingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.largest_request = self.largest_request.max(buffer.len());
                self.read_requests += 1;
                self.inner.read(buffer)
            }
        }

        impl std::io::Seek for TrackingReader {
            fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(position)
            }
        }

        let bytes = vec![b'x'; FILE_LAYOUT_SCAN_CHUNK * 3 + 17];
        let total_bytes = bytes.len() as u64;
        let mut reader = TrackingReader {
            inner: std::io::Cursor::new(bytes),
            largest_request: 0,
            read_requests: 0,
        };

        let (scanned_bytes, _, _) = scan_open_file_layout(&mut reader, total_bytes, 0).unwrap();

        assert_eq!(scanned_bytes, total_bytes);
        assert_eq!(reader.largest_request, FILE_LAYOUT_SCAN_CHUNK);
        assert!(reader.read_requests > 1);
    }

    #[test]
    fn regression_scan_and_window_read_must_not_mix_different_file_versions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("changing.txt");
        let version_a = b"first\nsecond\n";
        let version_b = b"xxxxxxCHANGED-CONTENT\n";
        std::fs::write(&path, version_a).unwrap();
        let resolver = crate::sandbox::SecurePathResolver;

        let (bytes, total_lines, window) = read_file_window_with_after_scan_hook(
            &resolver,
            temp.path(),
            "changing.txt",
            1,
            0,
            10,
            4096,
            || {
                // Simulate an atomic writer/build step replacing the file after
                // layout discovery but before the requested bytes are rendered.
                let replacement = path.with_extension("replacement");
                std::fs::write(&replacement, version_b).unwrap();
                std::fs::rename(&replacement, &path).unwrap();
            },
        )
        .unwrap();

        let version_a_result = (version_a.len() as u64, 3usize, "2\tsecond\n3\t".to_owned());
        let version_b_result = (version_b.len() as u64, 2usize, "2\t".to_owned());
        assert!(
            (bytes, total_lines, window.content.clone()) == version_a_result
                || (bytes, total_lines, window.content.clone()) == version_b_result,
            "a read_file result mixed layout metadata from one file version with bytes from another: bytes={bytes}, total_lines={total_lines}, content={:?}",
            window.content
        );
    }

    #[test]
    fn offset_at_eof_is_empty_but_offset_past_eof_is_invalid() {
        let lines = vec!["one", "two"];
        let eof = render_file_window(&lines, 2, 0, 10, 1024).unwrap();
        assert!(eof.content.is_empty());
        assert!(!eof.truncated);
        let error = render_file_window(&lines, 3, 0, 10, 1024).unwrap_err();
        assert_eq!(error.code(), "INVALID_INPUT");
    }

    #[test]
    fn eof_cursor_rejects_nonzero_intra_line_offset() {
        let lines = vec!["one"];
        let error = render_file_window(&lines, 1, 1, 10, 1024).unwrap_err();
        assert_eq!(error.code(), "INVALID_INPUT");
        assert!(error.message().contains("existing line"));
    }

    #[test]
    fn byte_budget_stops_before_starting_another_line() {
        let lines = vec!["aaaa", "bbbb", "cccc"];
        let first_line_bytes = "1\taaaa".len();
        let window = render_file_window(&lines, 0, 0, 3, first_line_bytes).unwrap();
        assert_eq!(window.content, "1\taaaa");
        assert!(window.truncated);
        assert_eq!(window.next_offset, Some(1));
        assert_eq!(window.next_line_byte_offset, None);
    }

    #[test]
    fn zero_line_limit_is_rejected() {
        let error = render_file_window(&["one"], 0, 0, 0, 1024).unwrap_err();
        assert_eq!(error.code(), "INVALID_INPUT");
    }

    #[test]
    fn tiny_output_budget_fails_instead_of_returning_stuck_cursor() {
        let prefix_error = render_file_window(&["abc"], 0, 0, 1, 1).unwrap_err();
        assert_eq!(prefix_error.code(), "RESOURCE_LIMIT_EXCEEDED");

        let utf8_error = render_file_window(&["🙂abc"], 0, 0, 1, 3).unwrap_err();
        assert_eq!(utf8_error.code(), "RESOURCE_LIMIT_EXCEEDED");
        assert!(utf8_error.message().contains("UTF-8 character"));
    }

    #[test]
    fn resumed_suffix_can_finish_line_and_advance_to_next_line() {
        let lines = vec!["abcdefgh", "next"];
        let window = render_file_window(&lines, 0, 4, 2, "1\tefgh".len()).unwrap();
        assert_eq!(window.content, "1\tefgh");
        assert!(window.truncated);
        assert_eq!(window.next_offset, Some(1));
        assert_eq!(window.next_line_byte_offset, None);
    }

    #[test]
    fn exact_budget_fit_does_not_mark_truncated_for_single_line() {
        let lines = vec!["abcdef"];
        let budget = "1\t".len() + "abcdef".len();
        let window = render_file_window(&lines, 0, 0, 1, budget).unwrap();
        assert_eq!(window.content, "1\tabcdef");
        assert!(!window.truncated);
        assert_eq!(window.next_offset, None);
    }

    #[test]
    fn one_byte_over_budget_splits_same_line_with_cursor() {
        let lines = vec!["abcdef"];
        let budget = "1\t".len() + "abcdef".len() - 1;
        let window = render_file_window(&lines, 0, 0, 10, budget).unwrap();
        assert!(window.truncated);
        assert_eq!(window.next_offset, Some(0));
        assert_eq!(window.next_line_byte_offset, Some("abcdef".len() - 1));
    }

    #[test]
    fn line_byte_offset_zero_on_missing_line_is_rejected_but_eof_line_ok() {
        let lines = vec!["only"];
        assert!(render_file_window(&lines, 1, 0, 1, 64).is_ok());
        assert!(render_file_window(&lines, 2, 0, 1, 64).is_err());
    }

    #[test]
    fn continuation_across_many_lines_never_repeats_or_skips_lines() {
        let lines: Vec<String> = (0..50).map(|index| format!("line-{index:02}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut offset = 0usize;
        let mut seen = Vec::new();
        let mut calls = 0usize;
        while offset < refs.len() {
            calls += 1;
            assert!(calls <= 60, "pagination must terminate");
            let window = render_file_window(&refs, offset, 0, 3, 24).unwrap();
            for rendered in window.content.lines() {
                seen.push(rendered.split_once('\t').unwrap().1.to_owned());
            }
            match window.next_offset {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(
            seen,
            (0..50)
                .map(|index| format!("line-{index:02}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn crlf_file_content_windows_preserve_logical_lines() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("windows.txt"), b"alpha\r\nbeta\r\ngamma").unwrap();
        let resolver = crate::sandbox::SecurePathResolver;

        let (bytes, total_lines, first) = read_file_window_with_after_scan_hook(
            &resolver,
            temp.path(),
            "windows.txt",
            0,
            0,
            2,
            128,
            || {},
        )
        .unwrap();

        assert_eq!(bytes, b"alpha\r\nbeta\r\ngamma".len() as u64);
        assert_eq!(total_lines, 3);
        assert_eq!(first.content, "1\talpha\n2\tbeta");
        assert!(!first.content.contains('\r'));
        assert!(first.truncated);
        assert_eq!(first.next_offset, Some(2));
        assert_eq!(first.next_line_byte_offset, None);
        assert_eq!(
            first.continuation.as_deref(),
            Some("Call read_file again with offset=2.")
        );

        let (_, _, second) = read_file_window_with_after_scan_hook(
            &resolver,
            temp.path(),
            "windows.txt",
            first.next_offset.unwrap(),
            first.next_line_byte_offset.unwrap_or(0),
            2,
            128,
            || {},
        )
        .unwrap();
        assert_eq!(second.content, "3\tgamma");
        assert!(!second.content.contains('\r'));
        assert!(!second.truncated);
        assert_eq!(second.next_offset, None);
        assert_eq!(second.continuation, None);
    }
}
