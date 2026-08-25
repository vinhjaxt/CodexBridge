use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{content::read_text_bounded, plugin_skills::discover_plugin_skills};
use crate::{
    error::{AppError, Result as AppResult},
    project::ProjectContext,
    sandbox::{PathOperation, SecurePathResolver},
};

pub(super) const SKILL_DOC_LIMIT: usize = 512 * 1024;
pub(super) const SKILL_PAGE_MAX: usize = 64 * 1024;
pub(super) const SKILL_PACKAGE_MAX_FILES: usize = 50;
const SKILL_PACKAGE_MAX_ENTRIES: usize = 4096;
const SKILL_CATALOG_MAX: usize = 256;
pub(super) const SKILL_WARNING_MAX: usize = 128;
pub(super) const SKILL_INSTRUCTION_CATALOG_BYTES: usize = 64 * 1024;
const REPO_SKILL_DIRS: &[&str] = &[".agents/skills", ".codex/skills"];
const SKILL_NAME_MAX_CHARS: usize = 64;
const SKILL_DESCRIPTION_MAX_CHARS: usize = 1024;
const SKILL_SCAN_MAX_DEPTH: usize = 6;
const SKILL_SCAN_MAX_DIRS_PER_ROOT: usize = 2000;
/// Directory names never descended into while scanning a skills root.
///
/// Mirrors codex's `HiddenDirectoryPolicy::Skip`: hidden directories and
/// dependency/vendor trees cannot contain meaningful skill packages, so
/// descending into them only burns the per-root scan budget and can crowd out
/// real skills behind them.
const SKILL_SCAN_PRUNE_DIRS: &[&str] = &[
    // Hidden directories (leading dot).
    ".git",
    ".hg",
    ".svn",
    ".github",
    ".codex-plugin",
    ".claude",
    // Dependency and vendor trees.
    "node_modules",
    "vendor",
    "venv",
    ".venv",
    "__pycache__",
    "target",
    "dist",
    "build",
    ".next",
    ".cache",
    ".terraform",
];

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct SkillsListArgs {
    /// Optional project path whose directory acts like Codex's current working
    /// directory for repo skill-root discovery.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SkillReadArgs {
    pub name: String,
    /// Repeat the path used for skills_list when selecting a nested repo skill.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct SkillSummary {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) scope: &'static str,
    pub(super) source: String,
    #[serde(skip)]
    pub(super) root: PathBuf,
    pub(super) path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct SkillWarning {
    pub(super) code: &'static str,
    pub(super) source: String,
    pub(super) package: String,
    pub(super) message: String,
}

#[derive(Debug, Default)]
pub(super) struct SkillCatalog {
    pub(super) skills: Vec<SkillSummary>,
    pub(super) warnings: Vec<SkillWarning>,
}

pub(super) fn skill_key(name: &str) -> String {
    name.to_lowercase()
}

pub(super) fn push_skill_warning(
    warnings: &mut Vec<SkillWarning>,
    code: &'static str,
    source: &str,
    package: &str,
    message: impl AsRef<str>,
) {
    if warnings.len() >= SKILL_WARNING_MAX {
        return;
    }
    let message = message.as_ref();
    let mut end = message.len().min(512);
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    warnings.push(SkillWarning {
        code,
        source: source.to_owned(),
        package: package.chars().take(256).collect(),
        message: message[..end].to_owned(),
    });
}

pub(super) fn read_bounded(root: &Path, relative: &Path, maximum: usize) -> AppResult<String> {
    read_text_bounded(&root.join(relative), maximum)
}

/// Quote plain YAML scalars that contain `: ` or flow-like openers so prose
/// frontmatter from third-party skills parses. Line-oriented on purpose: any
/// other malformed YAML must still surface as an error. Returns `None` when
/// nothing was repaired.
fn repair_frontmatter_scalar_fields(frontmatter: &str) -> Option<String> {
    let mut changed = false;
    let mut block_scalar_indent: Option<usize> = None;
    let mut repaired_lines = Vec::new();
    for line in frontmatter.lines() {
        let indent = line
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        if let Some(block_indent) = block_scalar_indent {
            if line.trim().is_empty() || indent > block_indent {
                repaired_lines.push(line.to_string());
                continue;
            }
            block_scalar_indent = None;
        }

        let Some((key, value)) = line.split_once(':') else {
            repaired_lines.push(line.to_string());
            continue;
        };
        if key.trim().is_empty() || value.starts_with(char::is_whitespace) {
            let trimmed_start = value.trim_start();
            let leading_whitespace = &value[..value.len() - trimmed_start.len()];
            let mut scalar = trimmed_start;
            let mut comment = "";
            for (index, character) in trimmed_start.char_indices() {
                if character == '#'
                    && (index == 0
                        || trimmed_start[..index]
                            .chars()
                            .next_back()
                            .is_some_and(char::is_whitespace))
                {
                    let comment_start = trimmed_start[..index].trim_end().len();
                    scalar = &trimmed_start[..comment_start];
                    comment = &trimmed_start[comment_start..];
                    break;
                }
            }

            let scalar = scalar.trim_end();
            match scalar.chars().next() {
                None | Some('\'' | '"' | '|') => {
                    repaired_lines.push(line.to_string());
                }
                _ => {
                    if matches!(scalar.chars().next(), Some('|' | '>')) {
                        block_scalar_indent = Some(indent);
                        repaired_lines.push(line.to_string());
                    } else {
                        let has_colon_separator =
                            scalar.char_indices().any(|(index, character)| {
                                character == ':'
                                    && scalar[index + 1..]
                                        .chars()
                                        .next()
                                        .is_some_and(char::is_whitespace)
                            });
                        let invalid_flow_like =
                            matches!(scalar.chars().next(), Some('[' | '{' | '@' | '`'))
                                && serde_yaml::from_str::<serde_yaml::Value>(scalar).is_err();
                        if !has_colon_separator && !invalid_flow_like {
                            repaired_lines.push(line.to_string());
                        } else {
                            let quoted_scalar = format!("'{}'", scalar.replace('\'', "''"));
                            repaired_lines.push(format!(
                                "{key}:{leading_whitespace}{quoted_scalar}{comment}"
                            ));
                            changed = true;
                        }
                    }
                }
            }
            continue;
        }
        repaired_lines.push(line.to_string());
    }
    changed.then(|| repaired_lines.join("\n"))
}

pub(super) fn parse_frontmatter(contents: &str, fallback: &str) -> AppResult<(String, String)> {
    let normalized = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let mut lines = normalized.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err(AppError::new(
            "INVALID_INPUT",
            "SKILL.md is missing YAML frontmatter",
        ));
    }
    let mut yaml = String::new();
    for line in lines {
        if line.trim() == "---" {
            let value: serde_yaml::Value = match serde_yaml::from_str(&yaml) {
                Ok(value) => value,
                // Third-party skills often contain prose scalars that are not
                // valid YAML, e.g. `description: Build for AWS: ECS`. Retry
                // once with plain scalars quoted (codex's skills parser does
                // the same) so unrelated invalid YAML still fails.
                Err(error) => {
                    let Some(repaired) = repair_frontmatter_scalar_fields(&yaml) else {
                        return Err(AppError::new("INVALID_INPUT", error.to_string()));
                    };
                    serde_yaml::from_str(&repaired)
                        .map_err(|_| AppError::new("INVALID_INPUT", error.to_string()))?
                }
            };
            let name = value
                .get("name")
                .and_then(serde_yaml::Value::as_str)
                .unwrap_or(fallback)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let description = value
                .get("description")
                .and_then(serde_yaml::Value::as_str)
                .filter(|description| !description.trim().is_empty())
                .or_else(|| {
                    value
                        .get("metadata")
                        .and_then(|metadata| metadata.get("short-description"))
                        .and_then(serde_yaml::Value::as_str)
                })
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if name.is_empty()
                || name.chars().count() > SKILL_NAME_MAX_CHARS
                || name.chars().any(|character| {
                    character.is_control() || matches!(character, '/' | '\\' | '\0')
                })
            {
                return Err(AppError::new("INVALID_INPUT", "invalid skill name"));
            }
            if description.is_empty() || description.chars().count() > SKILL_DESCRIPTION_MAX_CHARS {
                return Err(AppError::new(
                    "INVALID_INPUT",
                    "skill description is missing or too long",
                ));
            }
            return Ok((name, description));
        }
        yaml.push_str(line);
        yaml.push('\n');
        if yaml.len() > 64 * 1024 {
            break;
        }
    }
    Err(AppError::new(
        "INVALID_INPUT",
        "SKILL.md frontmatter is not terminated",
    ))
}

fn home_plugin_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = crate::platform::user_home_dir()
        && home.join(".claude").exists()
    {
        roots.push(home.join(".claude/plugins/cache"));
    }
    roots
}

pub(super) fn skill_catalog(project: &ProjectContext) -> AppResult<SkillCatalog> {
    skill_catalog_for_target(project, None)
}

pub(super) fn skill_catalog_for_target(
    project: &ProjectContext,
    target: Option<&str>,
) -> AppResult<SkillCatalog> {
    let user_roots = user_skill_roots();
    let plugin_roots = home_plugin_roots();
    skill_catalog_from_sources(project, target, &user_roots, &plugin_roots)
}

pub(super) fn skill_catalog_from_sources(
    project: &ProjectContext,
    target: Option<&str>,
    user_roots: &[PathBuf],
    plugin_roots: &[PathBuf],
) -> AppResult<SkillCatalog> {
    let mut catalog = SkillCatalog::default();
    let mut roots = repo_skill_roots(project, target)?;
    roots.extend(
        user_roots
            .iter()
            .map(|root| (root.clone(), "user", format!("user:{}", root.display()))),
    );
    let mut seen_roots = HashSet::new();
    roots.retain(|(root, _, _)| seen_roots.insert(root.clone()));
    for (root, scope, source) in roots {
        for skill_path in discover_skill_documents(&root, &source, &mut catalog.warnings) {
            let directory_name = skill_path
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "skill".to_owned());
            let relative_skill = match skill_path.strip_prefix(&root) {
                Ok(relative) => relative,
                Err(_) => {
                    push_skill_warning(
                        &mut catalog.warnings,
                        "SKILL_OUTSIDE_WORKSPACE",
                        &source,
                        &directory_name,
                        "skill path escaped the active project",
                    );
                    continue;
                }
            };
            let content = match read_bounded(&root, relative_skill, SKILL_DOC_LIMIT) {
                Ok(content) => content,
                Err(error) => {
                    push_skill_warning(
                        &mut catalog.warnings,
                        "SKILL_READ_FAILED",
                        &source,
                        &directory_name,
                        error.to_string(),
                    );
                    continue;
                }
            };
            let (name, description) = match parse_frontmatter(&content, &directory_name) {
                Ok(parsed) => parsed,
                Err(error) => {
                    push_skill_warning(
                        &mut catalog.warnings,
                        "INVALID_SKILL",
                        &source,
                        &directory_name,
                        error.to_string(),
                    );
                    continue;
                }
            };
            let key = skill_key(&name);
            if let Some(existing) = catalog
                .skills
                .iter()
                .find(|skill| skill_key(&skill.name) == key)
            {
                push_skill_warning(
                    &mut catalog.warnings,
                    "DUPLICATE_SKILL",
                    &source,
                    &directory_name,
                    format!(
                        "skill `{name}` was ignored; the higher-precedence definition from {} wins",
                        existing.source
                    ),
                );
                continue;
            }
            if catalog.skills.len() == SKILL_CATALOG_MAX {
                push_skill_warning(
                    &mut catalog.warnings,
                    "SKILL_CATALOG_TRUNCATED",
                    &source,
                    &directory_name,
                    format!("catalogue is limited to {SKILL_CATALOG_MAX} skills"),
                );
                continue;
            }
            catalog.skills.push(SkillSummary {
                name,
                description,
                scope,
                source: source.clone(),
                root: root.clone(),
                path: skill_path,
            });
        }
    }
    for candidate in discover_plugin_skills(plugin_roots) {
        let package = candidate
            .path
            .parent()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "skill".to_owned());
        let relative = match candidate.path.strip_prefix(&candidate.root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        let content = match read_bounded(&candidate.root, relative, SKILL_DOC_LIMIT) {
            Ok(content) => content,
            Err(error) => {
                push_skill_warning(
                    &mut catalog.warnings,
                    "SKILL_READ_FAILED",
                    &candidate.source,
                    &package,
                    error.to_string(),
                );
                continue;
            }
        };
        let (skill_name, description) = match parse_frontmatter(&content, &package) {
            Ok(parsed) => parsed,
            Err(error) => {
                push_skill_warning(
                    &mut catalog.warnings,
                    "INVALID_SKILL",
                    &candidate.source,
                    &package,
                    error.to_string(),
                );
                continue;
            }
        };
        let name = format!("{}:{skill_name}", candidate.plugin);
        let key = skill_key(&name);
        if let Some(existing) = catalog
            .skills
            .iter()
            .find(|skill| skill_key(&skill.name) == key)
        {
            push_skill_warning(
                &mut catalog.warnings,
                "DUPLICATE_SKILL",
                &candidate.source,
                &package,
                format!(
                    "skill `{name}` was ignored; the higher-precedence definition from {} wins",
                    existing.source
                ),
            );
            continue;
        }
        if catalog.skills.len() == SKILL_CATALOG_MAX {
            push_skill_warning(
                &mut catalog.warnings,
                "SKILL_CATALOG_TRUNCATED",
                &candidate.source,
                &package,
                format!("catalogue is limited to {SKILL_CATALOG_MAX} skills"),
            );
            break;
        }
        catalog.skills.push(SkillSummary {
            name,
            description,
            scope: "plugin",
            source: candidate.source,
            root: candidate.root,
            path: candidate.path,
        });
    }
    catalog
        .skills
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(catalog)
}

fn user_skill_roots() -> Vec<PathBuf> {
    let Some(home) = crate::platform::user_home_dir() else {
        return Vec::new();
    };
    let mut roots = vec![home.join(".agents/skills")];
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    roots.push(codex_home.join("skills"));
    let mut seen = HashSet::new();
    roots.retain(|root| seen.insert(root.clone()));
    roots
}

fn repo_skill_roots(
    project: &ProjectContext,
    target: Option<&str>,
) -> AppResult<Vec<(PathBuf, &'static str, String)>> {
    let target = target.filter(|value| !value.is_empty()).unwrap_or(".");
    if !project.project_root.exists() {
        if target != "." {
            return Err(AppError::new(
                "FILE_NOT_FOUND",
                "project path does not exist yet",
            ));
        }
        return Ok(REPO_SKILL_DIRS
            .iter()
            .map(|relative_root| {
                (
                    project.project_root.join(relative_root),
                    "project",
                    format!("project:./{relative_root}"),
                )
            })
            .collect());
    }
    let resolved = SecurePathResolver.resolve_project_path(
        &project.project_root,
        target,
        PathOperation::Existing,
    )?;
    let directory = if resolved.is_dir() {
        resolved
    } else {
        resolved
            .parent()
            .unwrap_or(&project.project_root)
            .to_path_buf()
    };
    let relative = directory.strip_prefix(&project.project_root).map_err(|_| {
        AppError::new(
            "PATH_OUTSIDE_WORKSPACE",
            "skill discovery target escaped project",
        )
    })?;
    let mut directories = vec![project.project_root.clone()];
    let mut cursor = project.project_root.clone();
    for component in relative.components() {
        cursor.push(component);
        directories.push(cursor.clone());
    }

    let mut roots = Vec::new();
    // Closer repo roots win duplicate names, matching scoped coding behavior.
    // `.agents/skills` is canonical; `.codex/skills` is a compatibility alias.
    for directory in directories.into_iter().rev() {
        for relative_root in REPO_SKILL_DIRS {
            let root = directory.join(relative_root);
            let directory_label = directory
                .strip_prefix(&project.project_root)
                .ok()
                .filter(|path| !path.as_os_str().is_empty())
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| ".".to_owned());
            roots.push((
                root,
                "project",
                format!("project:{directory_label}/{relative_root}"),
            ));
        }
    }
    Ok(roots)
}

fn discover_skill_documents(
    root: &Path,
    source: &str,
    warnings: &mut Vec<SkillWarning>,
) -> Vec<PathBuf> {
    match std::fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Vec::new(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            push_skill_warning(
                warnings,
                "SKILL_DISCOVERY_FAILED",
                source,
                "",
                error.to_string(),
            );
            return Vec::new();
        }
    }

    let mut documents = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = HashSet::new();
    let mut inspected = 0usize;
    while let Some((directory, depth)) = stack.pop() {
        if inspected >= SKILL_SCAN_MAX_DIRS_PER_ROOT {
            push_skill_warning(
                warnings,
                "SKILL_SCOPE_TRUNCATED",
                source,
                "",
                format!(
                    "recursive skill discovery is limited to {SKILL_SCAN_MAX_DIRS_PER_ROOT} directories per root"
                ),
            );
            break;
        }
        let identity = std::fs::canonicalize(&directory).unwrap_or_else(|_| directory.clone());
        if !visited.insert(identity) {
            continue;
        }
        inspected += 1;

        let skill_path = directory.join("SKILL.md");
        if std::fs::metadata(&skill_path).is_ok_and(|metadata| metadata.is_file()) {
            documents.push(skill_path);
        }
        if depth >= SKILL_SCAN_MAX_DEPTH {
            continue;
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                push_skill_warning(
                    warnings,
                    "SKILL_DISCOVERY_FAILED",
                    source,
                    &directory.to_string_lossy(),
                    error.to_string(),
                );
                continue;
            }
        };
        let mut child_directories = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
                    && !SKILL_SCAN_PRUNE_DIRS.contains(
                        &path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default(),
                    )
            })
            .collect::<Vec<_>>();
        child_directories.sort();
        for child in child_directories.into_iter().rev() {
            stack.push((child, depth + 1));
        }
    }
    documents.sort();
    documents
}

pub(super) fn byte_page(
    content: &str,
    offset: usize,
    requested: usize,
) -> AppResult<(String, usize, bool)> {
    if requested == 0 {
        return Err(AppError::new("INVALID_INPUT", "limit must be positive"));
    }
    if offset > content.len() || !content.is_char_boundary(offset) {
        return Err(AppError::new(
            "INVALID_INPUT",
            "offset is outside the document or not a UTF-8 boundary",
        ));
    }
    let mut end = offset.saturating_add(requested).min(content.len());
    while end > offset && !content.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset && offset < content.len() {
        return Err(AppError::new(
            "RESOURCE_LIMIT_EXCEEDED",
            "page limit is too small to include the next UTF-8 character",
        ));
    }
    Ok((content[offset..end].to_owned(), end, end < content.len()))
}

pub(super) fn validate_skill_resource(resource: &str) -> AppResult<()> {
    if resource.is_empty()
        || resource.len() > 4096
        || resource.contains('\\')
        || Path::new(resource).is_absolute()
    {
        return Err(AppError::new(
            "PATH_OUTSIDE_WORKSPACE",
            "skill resource must be a project-relative package path",
        ));
    }
    if Path::new(resource).components().any(|component| {
        !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err(AppError::new(
            "PATH_OUTSIDE_WORKSPACE",
            "skill resource traversal is not accepted",
        ));
    }
    Ok(())
}

pub(super) fn package_files(skill: &SkillSummary) -> AppResult<(Vec<String>, bool)> {
    let directory = skill
        .path
        .parent()
        .ok_or_else(|| AppError::new("INVALID_INPUT", "skill has no package directory"))?;
    let mut files = Vec::new();
    let mut traversed = 0usize;
    let mut truncated = false;
    for entry in walkdir::WalkDir::new(directory)
        .follow_links(true)
        .sort_by_file_name()
        .into_iter()
        .skip(1)
    {
        traversed += 1;
        if traversed > SKILL_PACKAGE_MAX_ENTRIES {
            truncated = true;
            break;
        }
        let entry = entry.map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
        if entry.file_type().is_file() && entry.file_name() != "SKILL.md" {
            let relative = entry
                .path()
                .strip_prefix(directory)
                .map_err(|_| AppError::new("PATH_OUTSIDE_WORKSPACE", "skill package escaped"))?;
            entry.path().strip_prefix(&skill.root).map_err(|_| {
                AppError::new("PATH_OUTSIDE_WORKSPACE", "skill package escaped root")
            })?;
            files.push(relative.to_string_lossy().replace('\\', "/"));
            if files.len() == SKILL_PACKAGE_MAX_FILES {
                truncated = true;
                break;
            }
        }
    }
    files.sort();
    Ok((files, truncated))
}

pub(super) fn available_skill_names(
    catalog: &SkillCatalog,
    gateway_summaries: &[serde_json::Value],
    maximum: usize,
) -> Vec<String> {
    let mut names = catalog
        .skills
        .iter()
        .map(|skill| skill.name.clone())
        .collect::<Vec<_>>();
    for gateway in gateway_summaries {
        if let Some(name) = gateway.get("name").and_then(serde_json::Value::as_str)
            && !names
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(name))
        {
            names.push(name.to_owned());
        }
    }
    names.sort_by_key(|name| name.to_lowercase());
    names.truncate(maximum);
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{project::ProjectKey, request_context::TransportMode};

    fn project(root: &Path) -> ProjectContext {
        ProjectContext {
            native_project_key: ProjectKey::new("native_key".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective_key".to_owned()).unwrap(),
            project_alias: None,
            project_root: root.to_path_buf(),
            metadata_root: root.join(".metadata"),
            transport_mode: TransportMode::Stateless,
            mcp_session_present: false,
        }
    }

    fn write_skill(root: &Path, package: &str, body: &str) -> PathBuf {
        let directory = root.join(package);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("SKILL.md"), body).unwrap();
        directory
    }

    #[test]
    fn skill_scan_prunes_hidden_and_vendor_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".agents/skills");
        write_skill(&root, "real", "---\nname: real\ndescription: Real\n---\n");
        // A hidden directory and a dependency tree that each shadow the same
        // skill name must not be scanned, so the top-level skill survives.
        write_skill(
            &root.join(".git"),
            "fake",
            "---\nname: real\ndescription: Shadowed\n---\n",
        );
        write_skill(
            &root.join("node_modules").join("leftpad"),
            "shadow",
            "---\nname: real\ndescription: Shadowed\n---\n",
        );
        // A visible, non-vendored nested package still counts.
        write_skill(
            &root.join("nested"),
            "deep",
            "---\nname: deep\ndescription: D\n---\n",
        );

        let catalog = skill_catalog_from_sources(&project(temp.path()), None, &[], &[]).unwrap();
        let names: Vec<&str> = catalog
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        assert_eq!(names, vec!["deep", "real"]);
        assert!(
            !catalog
                .warnings
                .iter()
                .any(|warning| warning.code == "DUPLICATE_SKILL")
        );
    }

    #[test]
    fn frontmatter_accepts_bom_fallback_name_and_wrapped_description() {
        let parsed = parse_frontmatter(
            "\u{feff}---\ndescription: >\n  one\n  two\n---\nbody\n",
            "on-disk",
        )
        .unwrap();
        assert_eq!(parsed, ("on-disk".to_owned(), "one two".to_owned()));
    }

    #[test]
    fn frontmatter_uses_short_description_when_description_is_blank() {
        let parsed = parse_frontmatter(
            "---\nname: demo\ndescription: '   '\nmetadata:\n  short-description: Short reason\n---\n",
            "fallback",
        )
        .unwrap();
        assert_eq!(parsed, ("demo".to_owned(), "Short reason".to_owned()));
    }

    #[test]
    fn frontmatter_repairs_prose_scalars_with_colons() {
        // `description: Build for AWS: ECS` is not valid YAML, but the
        // line-oriented repair pass quotes the scalar so the skill loads.
        let parsed = parse_frontmatter(
            "---\nname: aws\ndescription: Build for AWS: ECS\n---\nbody\n",
            "fallback",
        )
        .unwrap();
        assert_eq!(parsed.0, "aws");
        assert_eq!(parsed.1, "Build for AWS: ECS");
    }

    #[test]
    fn frontmatter_still_rejects_structurally_broken_yaml() {
        assert_eq!(
            parse_frontmatter(
                "---\nname: x\ndescription:\n  bad_indent:\n    - [unclosed\n---\n",
                "x"
            )
            .unwrap_err()
            .code(),
            "INVALID_INPUT"
        );
    }

    #[test]
    fn frontmatter_rejects_missing_invalid_unterminated_and_oversized_fields() {
        assert_eq!(
            parse_frontmatter("# body\n", "x").unwrap_err().code(),
            "INVALID_INPUT"
        );
        assert_eq!(
            parse_frontmatter("---\nname: [broken\n---\n", "x")
                .unwrap_err()
                .code(),
            "INVALID_INPUT"
        );
        assert!(
            parse_frontmatter("---\nname: x\ndescription: y\n", "x")
                .unwrap_err()
                .message()
                .contains("not terminated")
        );
        let long_name = "x".repeat(65);
        assert_eq!(
            parse_frontmatter(
                &format!("---\nname: {long_name}\ndescription: d\n---\n"),
                "x"
            )
            .unwrap_err()
            .code(),
            "INVALID_INPUT"
        );
        let long_description = "d".repeat(SKILL_DESCRIPTION_MAX_CHARS + 1);
        assert_eq!(
            parse_frontmatter(
                &format!("---\nname: demo\ndescription: {long_description}\n---\n"),
                "x"
            )
            .unwrap_err()
            .code(),
            "INVALID_INPUT"
        );
    }

    #[test]
    fn frontmatter_matches_codex_single_line_unicode_name_behavior() {
        let parsed = parse_frontmatter(
            "---\nname: công-cụ\ndescription: Unicode display name\n---\n",
            "fallback",
        )
        .unwrap();
        assert_eq!(parsed.0, "công-cụ");
    }

    #[test]
    fn byte_page_is_utf8_safe_and_validates_cursor_and_limit() {
        let content = "a🙂bc";
        let (first, next, truncated) = byte_page(content, 0, 3).unwrap();
        assert_eq!(first, "a");
        assert_eq!(next, 1);
        assert!(truncated);
        let (second, next, truncated) = byte_page(content, 1, 5).unwrap();
        assert_eq!(second, "🙂b");
        assert_eq!(next, 6);
        assert!(truncated);
        assert_eq!(
            byte_page(content, 2, 4).unwrap_err().code(),
            "INVALID_INPUT"
        );
        assert_eq!(
            byte_page(content, content.len() + 1, 4).unwrap_err().code(),
            "INVALID_INPUT"
        );
        assert_eq!(
            byte_page(content, 0, 0).unwrap_err().code(),
            "INVALID_INPUT"
        );
    }

    #[test]
    fn regression_byte_page_must_progress_or_error_when_limit_splits_first_utf8_character() {
        match byte_page("🙂abc", 0, 1) {
            Ok((page, next, truncated)) => {
                assert!(
                    !truncated || next > 0,
                    "truncated pagination must advance; page={page:?}, next={next}"
                );
            }
            Err(error) => {
                assert_eq!(error.code(), "RESOURCE_LIMIT_EXCEEDED");
            }
        }
    }

    #[test]
    fn resource_validation_accepts_curdir_but_rejects_escape_and_cross_platform_absolute_forms() {
        for valid in ["SKILL.md", "references/api.md", "./scripts/run.py"] {
            assert!(validate_skill_resource(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "../other/SKILL.md",
            "a/../../b",
            "/etc/passwd",
            "C:\\temp\\x",
            "a\\b",
        ] {
            assert_eq!(
                validate_skill_resource(invalid).unwrap_err().code(),
                "PATH_OUTSIDE_WORKSPACE",
                "{invalid}"
            );
        }
    }

    #[test]
    fn package_files_excludes_skill_doc_and_is_sorted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".agents/skills");
        let directory = write_skill(
            &root,
            "alpha",
            "---\nname: alpha\ndescription: Alpha\n---\n",
        );
        std::fs::create_dir_all(directory.join("references")).unwrap();
        std::fs::write(directory.join("z.txt"), "z").unwrap();
        std::fs::write(directory.join("references/a.md"), "a").unwrap();
        let catalog = skill_catalog_from_sources(&project(temp.path()), None, &[], &[]).unwrap();
        let (files, truncated) = package_files(&catalog.skills[0]).unwrap();
        assert_eq!(files, vec!["references/a.md", "z.txt"]);
        assert!(!truncated);
    }

    #[test]
    fn discovery_is_sorted_and_ignores_non_skill_packages_quietly() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".agents/skills");
        write_skill(&root, "beta", "---\nname: beta\ndescription: B\n---\n");
        write_skill(&root, "alpha", "---\nname: alpha\ndescription: A\n---\n");
        std::fs::create_dir_all(root.join("not-a-skill")).unwrap();
        std::fs::write(root.join("loose.md"), "stray").unwrap();
        let catalog = skill_catalog_from_sources(&project(temp.path()), None, &[], &[]).unwrap();
        assert_eq!(
            catalog
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        assert!(catalog.warnings.is_empty());
    }

    #[test]
    fn project_skill_shadows_home_skill_and_emits_warning() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        let home_root = temp.path().join("home-agent");
        write_skill(
            &project_root.join(".agents/skills"),
            "deploy",
            "---\nname: deploy\ndescription: Project version\n---\n",
        );
        write_skill(
            &home_root.join("skills"),
            "deploy",
            "---\nname: deploy\ndescription: Home version\n---\n",
        );
        let user_roots = vec![home_root.join("skills")];
        let catalog =
            skill_catalog_from_sources(&project(&project_root), None, &user_roots, &[]).unwrap();
        assert_eq!(catalog.skills.len(), 1);
        assert_eq!(catalog.skills[0].description, "Project version");
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.code == "DUPLICATE_SKILL")
        );
    }

    #[test]
    fn repo_skill_roots_follow_target_ancestry_and_closest_definition_wins() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(project_root.join("services/api/src")).unwrap();
        std::fs::write(
            project_root.join("services/api/src/lib.rs"),
            "pub fn api() {}\n",
        )
        .unwrap();
        write_skill(
            &project_root.join(".agents/skills"),
            "shared",
            "---\nname: shared\ndescription: Root definition\n---\n",
        );
        write_skill(
            &project_root.join(".agents/skills"),
            "root-only",
            "---\nname: root-only\ndescription: Root only\n---\n",
        );
        write_skill(
            &project_root.join("services/api/.agents/skills"),
            "shared",
            "---\nname: shared\ndescription: API definition\n---\n",
        );
        write_skill(
            &project_root.join("services/web/.agents/skills"),
            "web-only",
            "---\nname: web-only\ndescription: Web only\n---\n",
        );

        let catalog = skill_catalog_from_sources(
            &project(&project_root),
            Some("services/api/src/lib.rs"),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(
            catalog
                .skills
                .iter()
                .find(|skill| skill.name == "shared")
                .unwrap()
                .description,
            "API definition"
        );
        assert!(catalog.skills.iter().any(|skill| skill.name == "root-only"));
        assert!(!catalog.skills.iter().any(|skill| skill.name == "web-only"));

        let root_catalog =
            skill_catalog_from_sources(&project(&project_root), None, &[], &[]).unwrap();
        assert_eq!(
            root_catalog
                .skills
                .iter()
                .find(|skill| skill.name == "shared")
                .unwrap()
                .description,
            "Root definition"
        );
    }

    #[test]
    fn repo_codex_skills_are_a_lower_precedence_agents_alias() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(
            &temp.path().join(".agents/skills"),
            "shared",
            "---\nname: shared\ndescription: Canonical agents definition\n---\n",
        );
        write_skill(
            &temp.path().join(".codex/skills"),
            "shared",
            "---\nname: shared\ndescription: Compatibility alias definition\n---\n",
        );
        write_skill(
            &temp.path().join(".codex/skills"),
            "legacy-only",
            "---\nname: legacy-only\ndescription: Legacy only\n---\n",
        );
        let catalog = skill_catalog_from_sources(&project(temp.path()), None, &[], &[]).unwrap();
        assert_eq!(
            catalog
                .skills
                .iter()
                .find(|skill| skill.name == "shared")
                .unwrap()
                .description,
            "Canonical agents definition"
        );
        assert!(
            catalog
                .skills
                .iter()
                .any(|skill| skill.name == "legacy-only")
        );
    }

    #[test]
    fn skill_roots_are_scanned_recursively_like_codex() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(
            &temp.path().join(".agents/skills/team"),
            "deploy",
            "---\nname: deploy\ndescription: Nested package\n---\n",
        );
        let catalog = skill_catalog_from_sources(&project(temp.path()), None, &[], &[]).unwrap();
        let deploy = catalog
            .skills
            .iter()
            .find(|skill| skill.name == "deploy")
            .unwrap();
        assert!(deploy.path.ends_with(".agents/skills/team/deploy/SKILL.md"));
    }

    #[test]
    fn repo_claude_skills_are_not_codex_skill_roots() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(
            &temp.path().join(".claude/skills"),
            "claude-only",
            "---\nname: claude-only\ndescription: Claude only\n---\n",
        );
        let catalog = skill_catalog_from_sources(&project(temp.path()), None, &[], &[]).unwrap();
        assert!(catalog.skills.is_empty());
    }

    #[test]
    fn malformed_skill_does_not_hide_valid_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".agents/skills");
        write_skill(&root, "broken", "---\nname: broken\n---\n");
        write_skill(&root, "valid", "---\nname: valid\ndescription: Good\n---\n");
        let catalog = skill_catalog_from_sources(&project(temp.path()), None, &[], &[]).unwrap();
        assert_eq!(catalog.skills.len(), 1);
        assert_eq!(catalog.skills[0].name, "valid");
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.code == "INVALID_SKILL")
        );
    }

    #[test]
    fn skill_scope_scan_and_catalogue_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".agents/skills");
        for index in 0..260 {
            write_skill(
                &root,
                &format!("pkg-{index:03}"),
                &format!("---\nname: skill-{index:03}\ndescription: D\n---\n"),
            );
        }
        let catalog = skill_catalog_from_sources(&project(temp.path()), None, &[], &[]).unwrap();
        assert!(catalog.skills.len() <= SKILL_CATALOG_MAX);
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.code == "SKILL_CATALOG_TRUNCATED")
        );
    }

    #[test]
    fn available_names_merge_gateway_names_deduplicate_sort_and_truncate() {
        let mut catalog = SkillCatalog::default();
        catalog.skills.push(SkillSummary {
            name: "Zulu".to_owned(),
            description: "z".to_owned(),
            scope: "project",
            source: "project".to_owned(),
            root: PathBuf::new(),
            path: PathBuf::new(),
        });
        catalog.skills.push(SkillSummary {
            name: "alpha".to_owned(),
            description: "a".to_owned(),
            scope: "project",
            source: "project".to_owned(),
            root: PathBuf::new(),
            path: PathBuf::new(),
        });
        let gateways = vec![
            serde_json::json!({"name":"ALPHA"}),
            serde_json::json!({"name":"gateway_docs"}),
        ];
        assert_eq!(
            available_skill_names(&catalog, &gateways, 2),
            vec!["alpha", "gateway_docs"]
        );
    }

    #[test]
    fn skill_key_uses_unicode_lowercasing() {
        assert_eq!(skill_key("ÜBER"), "über");
    }
}
