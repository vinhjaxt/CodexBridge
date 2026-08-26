use crate::{
    error::{AppError, Result as AppResult},
    project::ProjectContext,
    sandbox::PathOperation,
};
use std::collections::HashSet;

const HOME_AGENT_DOC_LIMIT: usize = 128 * 1024;
const PROJECT_AGENT_DOC_LIMIT: usize = 256 * 1024;
const AGENT_DOC_MAX_FILES: usize = 128;
use super::content::read_text_prefix_bounded;
use super::home::AgentHome;

struct DocBudget {
    remaining_bytes: usize,
    remaining_files: usize,
}

impl DocBudget {
    fn new(max_bytes: usize) -> Self {
        Self {
            remaining_bytes: max_bytes,
            remaining_files: AGENT_DOC_MAX_FILES,
        }
    }

    fn exhausted(&self) -> bool {
        self.remaining_bytes == 0 || self.remaining_files == 0
    }

    fn read(&mut self, path: &std::path::Path) -> AppResult<Option<(String, bool)>> {
        if self.exhausted() {
            return Ok(None);
        }
        let (content, truncated) = read_text_prefix_bounded(path, self.remaining_bytes)?;
        // Blank instruction files should not consume either the byte or file
        // budget. This also keeps generated briefs free of empty source markers.
        if content.trim().is_empty() {
            return Ok(None);
        }
        self.remaining_bytes = self.remaining_bytes.saturating_sub(content.len());
        self.remaining_files = self.remaining_files.saturating_sub(1);
        Ok(Some((content, truncated)))
    }
}

fn sorted_files(directory: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    files
}

pub(super) fn project_docs(
    project: &ProjectContext,
    target: Option<&str>,
    fallback_filenames: &[String],
) -> AppResult<Vec<(String, String)>> {
    project_docs_with_home(project, target, fallback_filenames, AgentHome::discover())
}

pub(super) fn project_docs_with_home(
    project: &ProjectContext,
    target: Option<&str>,
    fallback_filenames: &[String],
    home: Option<AgentHome>,
) -> AppResult<Vec<(String, String)>> {
    let target = target.unwrap_or(".");
    let directory = if !project.project_root.exists() {
        if target != "." {
            return Err(AppError::new(
                "FILE_NOT_FOUND",
                "project path does not exist yet",
            ));
        }
        project.project_root.clone()
    } else {
        let resolved = match crate::sandbox::SecurePathResolver.resolve_project_path(
            &project.project_root,
            target,
            PathOperation::Existing,
        ) {
            Ok(path) => path,
            Err(error) if error.code() == "FILE_NOT_FOUND" => crate::sandbox::SecurePathResolver
                .resolve_project_path(&project.project_root, target, PathOperation::Create)?,
            Err(error) => return Err(error),
        };
        if resolved.is_dir() {
            resolved
        } else {
            resolved
                .parent()
                .unwrap_or(&project.project_root)
                .to_path_buf()
        }
    };
    let relative = directory.strip_prefix(&project.project_root).map_err(|_| {
        AppError::new(
            "PATH_OUTSIDE_WORKSPACE",
            "instruction target escaped project",
        )
    })?;
    let mut directories = vec![project.project_root.clone()];
    let mut cursor = project.project_root.clone();
    for component in relative.components() {
        cursor.push(component);
        directories.push(cursor.clone());
    }
    let mut docs = Vec::new();

    // The daemon account's selected home ecosystem is the shared baseline.
    // Project-local instructions are appended afterward and therefore have
    // higher precedence in the assembled brief.
    if let Some(home) = home {
        let mut budget = DocBudget::new(HOME_AGENT_DOC_LIMIT);
        let root = home.root.clone();
        for name in ["AGENTS.override.md", "AGENTS.md"] {
            let path = root.join(name);
            if !std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                continue;
            }
            if let Some((content, truncated)) = budget.read(&path)? {
                let suffix = if truncated { " [truncated]" } else { "" };
                docs.push((
                    format!("home:{}/{}{}", home.source_name(), name, suffix),
                    content,
                ));
            }
            break;
        }
        for path in sorted_files(&root.join("rules")) {
            if budget.exhausted() {
                break;
            }
            if let Some((content, truncated)) = budget.read(&path)? {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "rule".to_owned());
                let suffix = if truncated { " [truncated]" } else { "" };
                docs.push((
                    format!("home:{}/rules/{name}{suffix}", home.source_name()),
                    content,
                ));
            }
        }
    }

    // Keep project instructions independent from the shared home baseline so
    // a large global AGENTS.md cannot consume the project's entire budget.
    let mut budget = DocBudget::new(PROJECT_AGENT_DOC_LIMIT);
    for directory in directories {
        if budget.exhausted() {
            break;
        }
        let names = ["AGENTS.override.md", "AGENTS.md"]
            .into_iter()
            .map(str::to_owned)
            .chain(fallback_filenames.iter().cloned());
        for name in names {
            let path = directory.join(name);
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let relative = path.strip_prefix(&project.project_root).map_err(|_| {
                AppError::new("PATH_OUTSIDE_WORKSPACE", "instruction file escaped project")
            })?;
            if let Some((content, truncated)) = budget.read(&path)? {
                let mut source = relative.to_string_lossy().replace('\\', "/");
                if truncated {
                    source.push_str(" [truncated]");
                }
                docs.push((source, content));
            }
            break;
        }
    }

    // Project rules are explicit companion documents. Agent content follows
    // symlinks by policy, unlike ordinary project filesystem tools.
    for directory in [
        project.project_root.join(".agents/rules"),
        project.project_root.join(".codex/rules"),
        project.project_root.join(".claude/rules"),
    ] {
        for path in sorted_files(&directory) {
            if budget.exhausted() {
                break;
            }
            if let Ok(relative) = path.strip_prefix(&project.project_root)
                && let Some((content, truncated)) = budget.read(&path)?
            {
                let mut source = relative.to_string_lossy().replace('\\', "/");
                if truncated {
                    source.push_str(" [truncated]");
                }
                docs.push((source, content));
            }
        }
        if budget.exhausted() {
            break;
        }
    }
    Ok(docs)
}

pub(crate) fn project_instruction_delta(
    project: &ProjectContext,
    target: &str,
    fallback_filenames: &[String],
) -> AppResult<Vec<(String, String)>> {
    let base = project_docs(project, None, fallback_filenames)?;
    let base_sources = base
        .into_iter()
        .map(|(source, _)| source)
        .collect::<HashSet<_>>();
    Ok(project_docs(project, Some(target), fallback_filenames)?
        .into_iter()
        .filter(|(source, _)| !base_sources.contains(source))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{project::ProjectKey, request_context::TransportMode};

    fn project(root: &std::path::Path) -> ProjectContext {
        let root = root.canonicalize().expect("test project root must exist");
        ProjectContext {
            native_project_key: ProjectKey::new("native_key".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective_key".to_owned()).unwrap(),
            project_alias: None,
            project_root: root.clone(),
            metadata_root: root.join(".metadata"),
            transport_mode: TransportMode::Stateless,
            mcp_session_present: false,
        }
    }

    #[test]
    fn docs_follow_root_to_target_and_prefer_override_per_directory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src/nested")).unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "root").unwrap();
        std::fs::write(temp.path().join("src/AGENTS.md"), "shadowed").unwrap();
        std::fs::write(temp.path().join("src/AGENTS.override.md"), "override").unwrap();
        std::fs::write(temp.path().join("src/nested/AGENTS.md"), "nested").unwrap();

        let docs =
            project_docs_with_home(&project(temp.path()), Some("src/nested"), &[], None).unwrap();
        assert_eq!(
            docs,
            vec![
                ("AGENTS.md".to_owned(), "root".to_owned()),
                ("src/AGENTS.override.md".to_owned(), "override".to_owned()),
                ("src/nested/AGENTS.md".to_owned(), "nested".to_owned()),
            ]
        );
    }

    #[test]
    fn target_file_uses_its_parent_instruction_chain() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src/nested")).unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "root").unwrap();
        std::fs::write(temp.path().join("src/AGENTS.md"), "src").unwrap();
        std::fs::write(temp.path().join("src/nested/lib.rs"), "fn main() {}\n").unwrap();
        let docs =
            project_docs_with_home(&project(temp.path()), Some("src/nested/lib.rs"), &[], None)
                .unwrap();
        assert_eq!(
            docs.iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["AGENTS.md", "src/AGENTS.md"]
        );
    }

    #[test]
    fn instruction_delta_returns_only_nested_scope_for_existing_or_new_target() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("services/api")).unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "root").unwrap();
        std::fs::write(temp.path().join("services/AGENTS.md"), "services").unwrap();
        std::fs::write(temp.path().join("services/api/AGENTS.md"), "api").unwrap();
        std::fs::write(temp.path().join("services/api/lib.rs"), "").unwrap();

        for target in ["services/api/lib.rs", "services/api/new.rs"] {
            let delta = project_instruction_delta(&project(temp.path()), target, &[]).unwrap();
            assert_eq!(
                delta,
                vec![
                    ("services/AGENTS.md".to_owned(), "services".to_owned()),
                    ("services/api/AGENTS.md".to_owned(), "api".to_owned()),
                ]
            );
        }
    }

    #[test]
    fn fallback_is_used_only_when_standard_agent_files_are_absent() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("CONTRIBUTING.md"), "fallback").unwrap();
        let fallbacks = vec!["CONTRIBUTING.md".to_owned()];
        let docs = project_docs_with_home(&project(temp.path()), None, &fallbacks, None).unwrap();
        assert_eq!(
            docs,
            vec![("CONTRIBUTING.md".to_owned(), "fallback".to_owned())]
        );

        std::fs::write(temp.path().join("AGENTS.md"), "primary").unwrap();
        let docs = project_docs_with_home(&project(temp.path()), None, &fallbacks, None).unwrap();
        assert_eq!(docs, vec![("AGENTS.md".to_owned(), "primary".to_owned())]);
    }

    #[test]
    fn whitespace_only_doc_is_skipped_without_spending_budget() {
        let temp = tempfile::tempdir().unwrap();
        let blank = temp.path().join("blank.md");
        let real = temp.path().join("real.md");
        std::fs::write(&blank, "   \n\t\n").unwrap();
        std::fs::write(&real, "rules").unwrap();
        let mut budget = DocBudget::new(5);
        assert!(budget.read(&blank).unwrap().is_none());
        assert_eq!(budget.remaining_bytes, 5);
        assert_eq!(budget.remaining_files, AGENT_DOC_MAX_FILES);
        assert_eq!(
            budget.read(&real).unwrap(),
            Some(("rules".to_owned(), false))
        );
    }

    #[test]
    fn doc_budget_is_shared_and_marks_the_first_truncated_file() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.md");
        let second = temp.path().join("second.md");
        std::fs::write(&first, "12345").unwrap();
        std::fs::write(&second, "67890").unwrap();
        let mut budget = DocBudget::new(8);
        assert_eq!(
            budget.read(&first).unwrap(),
            Some(("12345".to_owned(), false))
        );
        assert_eq!(
            budget.read(&second).unwrap(),
            Some(("678".to_owned(), true))
        );
        assert!(budget.exhausted());
    }

    #[test]
    fn doc_budget_counts_utf8_bytes_not_characters() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("unicode.md");
        std::fs::write(&path, "ééé").unwrap();
        let mut budget = DocBudget::new(4);
        assert_eq!(budget.read(&path).unwrap(), Some(("éé".to_owned(), true)));
        assert_eq!(budget.remaining_bytes, 0);
    }

    #[test]
    fn zero_doc_budget_disables_reads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("AGENTS.md");
        std::fs::write(&path, "rules").unwrap();
        let mut budget = DocBudget::new(0);
        assert!(budget.read(&path).unwrap().is_none());
    }

    #[test]
    fn project_rule_directories_are_sorted_and_ordered_by_ecosystem() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".agents/rules")).unwrap();
        std::fs::create_dir_all(temp.path().join(".codex/rules")).unwrap();
        std::fs::write(temp.path().join(".agents/rules/z.md"), "z").unwrap();
        std::fs::write(temp.path().join(".agents/rules/a.md"), "a").unwrap();
        std::fs::write(temp.path().join(".codex/rules/b.md"), "b").unwrap();
        let docs = project_docs_with_home(&project(temp.path()), None, &[], None).unwrap();
        assert_eq!(
            docs.iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec![
                ".agents/rules/a.md",
                ".agents/rules/z.md",
                ".codex/rules/b.md"
            ]
        );
    }

    #[test]
    fn project_doc_target_traversal_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let error = project_docs_with_home(&project(temp.path()), Some("../outside"), &[], None)
            .unwrap_err();
        assert_eq!(error.code(), "PATH_OUTSIDE_WORKSPACE");
    }
}
