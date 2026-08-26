use std::{path::PathBuf, sync::Arc};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use dashmap::DashMap;
use regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    error::{AppError, Result},
    request_context::{RequestIdentity, TransportMode},
    storage::{Storage, TurnRefCommit, TurnRefCommitOutcome},
};

const INITIALIZED_CACHE_MAX_ENTRIES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ProjectKey(String);

impl ProjectKey {
    pub fn new(value: String) -> Result<Self> {
        if value.is_empty()
            || matches!(value.as_str(), "." | "..")
            || !value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
            })
        {
            return Err(AppError::new(
                "PROJECT_ALIAS_INVALID_STATE",
                "invalid persisted project key",
            ));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub native_project_key: ProjectKey,
    pub effective_project_key: ProjectKey,
    pub project_alias: Option<String>,
    pub project_root: PathBuf,
    pub metadata_root: PathBuf,
    pub transport_mode: TransportMode,
    pub mcp_session_present: bool,
}

#[derive(Debug, Clone)]
pub struct PreparedProjectInitialization {
    pub project: ProjectContext,
    pub joined: bool,
    pub reused_existing_binding: bool,
    subject_key: String,
    alias: Option<String>,
    expected_binding: Option<String>,
    expected_alias_binding: Option<String>,
}

#[derive(Clone)]
pub struct ProjectResolver {
    workspace_root: Arc<PathBuf>,
    metadata_root: Arc<PathBuf>,
    storage: Storage,
    alias_pattern: Arc<Regex>,
    initialized_cache: Arc<DashMap<String, ProjectContext>>,
}

pub fn encode_project_key(digest: [u8; 32]) -> String {
    URL_SAFE_NO_PAD.encode(digest).replace('-', "")
}

pub fn derive_native_project_key(subject: &str, conversation: &str) -> ProjectKey {
    let mut hasher = Sha256::new();
    hasher.update(b"chatgpt\0");
    hasher.update(subject.as_bytes());
    hasher.update(b"\0");
    hasher.update(conversation.as_bytes());
    ProjectKey(encode_project_key(hasher.finalize().into()))
}

fn derive_subject_key(subject: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"chatgpt-subject\0");
    hasher.update(subject.as_bytes());
    encode_project_key(hasher.finalize().into())
}

impl ProjectResolver {
    fn cache_initialized(&self, project: &ProjectContext) {
        if self.initialized_cache.len() >= INITIALIZED_CACHE_MAX_ENTRIES {
            let victim = self
                .initialized_cache
                .iter()
                .find(|entry| entry.key().as_str() != project.native_project_key.as_str())
                .map(|entry| entry.key().clone());
            if let Some(victim) = victim {
                self.initialized_cache.remove(&victim);
            }
        }
        self.initialized_cache.insert(
            project.native_project_key.as_str().to_owned(),
            project.clone(),
        );
    }

    pub fn new(workspace_root: PathBuf, storage: Storage) -> Result<Self> {
        std::fs::create_dir_all(&workspace_root)?;
        // Keep every ProjectContext root in the same canonical form returned by
        // SecurePathResolver. Without this, a valid workspace supplied as
        // `./workspace`, through `..`, or through a symlink can resolve to a
        // canonical child that no longer has the original lexical root as a
        // prefix. Skill/AGENTS discovery would then falsely report that the
        // target escaped the project during the very first turn initialization.
        let workspace_root = workspace_root.canonicalize()?;
        let metadata_root = workspace_root.join(".metadata");
        std::fs::create_dir_all(metadata_root.join("projects"))?;
        Ok(Self {
            workspace_root: Arc::new(workspace_root),
            metadata_root: Arc::new(metadata_root),
            storage,
            alias_pattern: Arc::new(
                Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").expect("static alias regex"),
            ),
            initialized_cache: Arc::new(DashMap::new()),
        })
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn initialized_cache_entries(&self) -> usize {
        self.initialized_cache.len()
    }

    pub fn validate_alias(&self, alias: &str) -> Result<()> {
        if !self.alias_pattern.is_match(alias)
            || matches!(alias, "." | "..")
            || alias.contains('/')
            || alias.contains('\\')
            || is_windows_reserved_alias(alias)
        {
            return Err(AppError::new(
                "INVALID_PROJECT_ALIAS",
                "alias must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$, cannot be . or .., and must be portable to Windows filesystems",
            ));
        }
        Ok(())
    }

    pub fn resolve(&self, identity: &RequestIdentity) -> Result<ProjectContext> {
        let native =
            derive_native_project_key(&identity.openai_subject, &identity.openai_conversation_id);
        let effective = self
            .storage
            .effective_binding(native.as_str())?
            .unwrap_or_else(|| native.as_str().to_owned());
        let project = self.context(native, ProjectKey::new(effective)?, identity)?;
        self.ensure_layout(&project)?;
        Ok(project)
    }

    pub fn resolve_initialized(&self, identity: &RequestIdentity) -> Result<ProjectContext> {
        let native =
            derive_native_project_key(&identity.openai_subject, &identity.openai_conversation_id);
        if let Some(cached) = self.initialized_cache.get(native.as_str()) {
            let mut project = cached.clone();
            project.transport_mode = identity.transport_mode;
            project.mcp_session_present = identity.mcp_session_id.is_some();
            return Ok(project);
        }
        let effective = self
            .storage
            .effective_binding(native.as_str())?
            .ok_or_else(|| {
                AppError::new(
                    "TURN_NOT_INITIALIZED",
                    "this ChatGPT conversation has no initialized project turn yet; call chatgpt_turn_init first, for example with project_key=\"demo-workspace\"",
                )
            })?;
        let project = self.context(native, ProjectKey::new(effective)?, identity)?;
        self.ensure_layout(&project)?;
        self.cache_initialized(&project);
        Ok(project)
    }

    pub fn initialize(
        &self,
        identity: &RequestIdentity,
        alias: Option<&str>,
    ) -> Result<(ProjectContext, bool)> {
        let prepared = self.prepare_initialize(identity, alias)?;
        self.commit_initialize(&prepared)?;
        Ok((prepared.project, prepared.joined))
    }

    pub fn prepare_initialize(
        &self,
        identity: &RequestIdentity,
        alias: Option<&str>,
    ) -> Result<PreparedProjectInitialization> {
        self.prepare_initialize_inner(identity, alias, None)
    }

    pub fn prepare_turn_initialize(
        &self,
        identity: &RequestIdentity,
        alias: Option<&str>,
        previous_turn_ref: Option<&str>,
    ) -> Result<PreparedProjectInitialization> {
        let subject_key = derive_subject_key(&identity.openai_subject);
        let inherited_effective = if let Some(previous_turn_ref) = previous_turn_ref {
            Some(
                self.storage
                    .turn_ref_effective_for_subject(previous_turn_ref, &subject_key)?
                    .ok_or_else(|| {
                        AppError::new(
                            "TURN_REF_NOT_FOUND",
                            "previous_turn_ref does not exist or is not available to this ChatGPT subject",
                        )
                    })?,
            )
        } else {
            None
        };
        self.prepare_initialize_inner(identity, alias, inherited_effective.as_deref())
    }

    fn prepare_initialize_inner(
        &self,
        identity: &RequestIdentity,
        alias: Option<&str>,
        inherited_effective: Option<&str>,
    ) -> Result<PreparedProjectInitialization> {
        let native =
            derive_native_project_key(&identity.openai_subject, &identity.openai_conversation_id);
        let subject_key = derive_subject_key(&identity.openai_subject);
        let requested_alias = alias.filter(|value| !value.is_empty()).map(str::to_owned);
        if let Some(alias) = requested_alias.as_deref() {
            self.validate_alias(alias)?;
        }
        let expected_binding = self.storage.effective_binding(native.as_str())?;
        let expected_alias_binding = if let Some(alias) = requested_alias.as_deref() {
            self.storage.effective_for_alias(alias)?
        } else {
            None
        };
        let joined = expected_alias_binding.is_some()
            || (expected_binding.is_none() && inherited_effective.is_some());
        let effective = expected_alias_binding
            .clone()
            .or_else(|| expected_binding.clone())
            .or_else(|| inherited_effective.map(str::to_owned))
            // For a genuinely new named project, use the validated human
            // alias as the effective key so the checkout lives at
            // <workspace>/<project-name>/ instead of an opaque native hash.
            .or_else(|| requested_alias.clone())
            .unwrap_or_else(|| native.as_str().to_owned());
        let reused_existing_binding = expected_binding.as_deref() == Some(effective.as_str());
        let mut project = self.context(native, ProjectKey::new(effective)?, identity)?;
        if expected_alias_binding.is_none()
            && let Some(alias) = requested_alias.as_ref()
        {
            project.project_alias = Some(alias.clone());
        }
        Ok(PreparedProjectInitialization {
            project,
            joined,
            reused_existing_binding,
            subject_key,
            alias: requested_alias,
            expected_binding,
            expected_alias_binding,
        })
    }

    pub fn commit_initialize(&self, prepared: &PreparedProjectInitialization) -> Result<()> {
        let layout = self.ensure_layout(&prepared.project)?;
        let result = self.storage.commit_initialization(
            prepared.project.native_project_key.as_str(),
            prepared.project.effective_project_key.as_str(),
            prepared.alias.as_deref(),
            prepared.expected_binding.as_deref(),
            prepared.expected_alias_binding.as_deref(),
        );
        if let Err(error) = result {
            self.cleanup_uncommitted_layout(prepared, layout);
            return Err(error);
        }
        self.cache_initialized(&prepared.project);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_initialize_with_turn_ref(
        &self,
        prepared: &PreparedProjectInitialization,
        turn_ref: &str,
        parent_turn_ref: Option<&str>,
        instruction_hash: &str,
        state_hash: &str,
        brief_snapshot: &str,
        state_snapshot: Option<&str>,
    ) -> Result<TurnRefCommitOutcome> {
        let layout = self.ensure_layout(&prepared.project)?;
        let outcome = self.storage.commit_initialization_with_turn_ref(
            prepared.project.native_project_key.as_str(),
            prepared.project.effective_project_key.as_str(),
            prepared.alias.as_deref(),
            prepared.expected_binding.as_deref(),
            prepared.expected_alias_binding.as_deref(),
            TurnRefCommit {
                turn_ref,
                parent_turn_ref,
                instruction_hash,
                state_hash,
                subject_key: &prepared.subject_key,
                brief_snapshot,
                state_snapshot,
            },
        );
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                self.cleanup_uncommitted_layout(prepared, layout);
                return Err(error);
            }
        };
        self.cache_initialized(&prepared.project);
        Ok(outcome)
    }

    fn context(
        &self,
        native: ProjectKey,
        effective: ProjectKey,
        identity: &RequestIdentity,
    ) -> Result<ProjectContext> {
        let project_root = self.workspace_root.join(effective.as_str());
        let metadata_root = self.metadata_root.join("projects").join(effective.as_str());
        let project_alias = self.storage.alias_for_effective(effective.as_str())?;
        Ok(ProjectContext {
            native_project_key: native,
            effective_project_key: effective,
            project_alias,
            project_root,
            metadata_root,
            transport_mode: identity.transport_mode,
            mcp_session_present: identity.mcp_session_id.is_some(),
        })
    }

    fn ensure_layout(&self, project: &ProjectContext) -> Result<CreatedProjectLayout> {
        let mut layout = CreatedProjectLayout {
            project_root_created: false,
            metadata_root_created: false,
            metadata_dirs_created: Vec::new(),
        };
        let result = (|| {
            if project.project_root.exists() {
                if !project.project_root.is_dir() {
                    return Err(AppError::new(
                        "INVALID_PROJECT_ALIAS",
                        "project checkout path exists but is not a directory",
                    ));
                }
            } else {
                std::fs::create_dir(&project.project_root)?;
                layout.project_root_created = true;
            }
            if project.metadata_root.exists() {
                if !project.metadata_root.is_dir() {
                    return Err(AppError::new(
                        "PROCESS_FAILED",
                        "project metadata path exists but is not a directory",
                    ));
                }
            } else {
                std::fs::create_dir(&project.metadata_root)?;
                layout.metadata_root_created = true;
            }
            for directory in ["memory", "plans", "tmp", "home", "state"] {
                let path = project.metadata_root.join(directory);
                if path.exists() {
                    if !path.is_dir() {
                        return Err(AppError::new(
                            "PROCESS_FAILED",
                            format!(
                                "project metadata entry {} is not a directory",
                                path.display()
                            ),
                        ));
                    }
                } else {
                    std::fs::create_dir(&path)?;
                    layout.metadata_dirs_created.push(path);
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            layout.remove_empty_created_paths(project);
            return Err(error);
        }
        Ok(layout)
    }

    fn cleanup_uncommitted_layout(
        &self,
        prepared: &PreparedProjectInitialization,
        mut layout: CreatedProjectLayout,
    ) {
        let effective = prepared.project.effective_project_key.as_str();
        let native = prepared.project.native_project_key.as_str();
        let committed_elsewhere = self
            .storage
            .effective_binding(native)
            .ok()
            .flatten()
            .as_deref()
            == Some(effective)
            || self
                .storage
                .alias_for_effective(effective)
                .ok()
                .flatten()
                .is_some();
        if committed_elsewhere {
            return;
        }
        layout.remove_empty_created_paths(&prepared.project);
    }
}

fn is_windows_reserved_alias(alias: &str) -> bool {
    // Windows strips trailing dots/spaces during Win32 path normalization, so
    // allowing those names would make two distinct logical aliases share one
    // filesystem entry. Spaces are already excluded by the alias regex, but
    // keep the check here with the device-name rules for portability.
    if alias.ends_with(['.', ' ']) {
        return true;
    }
    let stem = alias.split('.').next().unwrap_or(alias);
    let stem = stem.to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || stem.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
}

struct CreatedProjectLayout {
    project_root_created: bool,
    metadata_root_created: bool,
    metadata_dirs_created: Vec<PathBuf>,
}

impl CreatedProjectLayout {
    fn remove_empty_created_paths(&mut self, project: &ProjectContext) {
        self.metadata_dirs_created.reverse();
        for path in self.metadata_dirs_created.drain(..) {
            let _ = std::fs::remove_dir(path);
        }
        if self.metadata_root_created {
            let _ = std::fs::remove_dir(&project.metadata_root);
        }
        if self.project_root_created {
            // remove_dir is intentionally non-recursive. If anything wrote to
            // the checkout concurrently, preserve it rather than deleting user
            // or another initializer's data.
            let _ = std::fs::remove_dir(&project.project_root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_context::{RequestIdentity, TransportMode};
    use sha2::Sha256;

    fn identity(
        subject: &str,
        conversation: &str,
        mcp_session_id: Option<&str>,
    ) -> RequestIdentity {
        RequestIdentity {
            openai_subject: subject.to_owned(),
            openai_conversation_id: conversation.to_owned(),
            mcp_session_id: mcp_session_id.map(str::to_owned),
            transport_mode: if mcp_session_id.is_some() {
                TransportMode::LegacySession
            } else {
                TransportMode::Stateless
            },
        }
    }

    #[test]
    fn chatgpt_key_derivation_is_exact_and_separated() {
        let key = derive_native_project_key("subject", "conversation");
        let expected = encode_project_key(Sha256::digest(b"chatgpt\0subject\0conversation").into());
        assert_eq!(key.as_str(), expected);
        assert_eq!(key, derive_native_project_key("subject", "conversation"));
        assert_ne!(key, derive_native_project_key("subject", "other"));
        assert_ne!(key, derive_native_project_key("other", "conversation"));
        assert!(
            key.as_str()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        );
    }

    #[test]
    fn transport_session_never_changes_chatgpt_project_identity() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();
        let stateless = resolver.resolve(&identity("usr", "conv", None)).unwrap();
        let legacy_a = resolver
            .resolve(&identity("usr", "conv", Some("SESSION_A")))
            .unwrap();
        let legacy_b = resolver
            .resolve(&identity("usr", "conv", Some("SESSION_B")))
            .unwrap();
        assert_eq!(stateless.native_project_key, legacy_a.native_project_key);
        assert_eq!(legacy_a.native_project_key, legacy_b.native_project_key);
        assert_eq!(stateless.project_root, legacy_b.project_root);
    }

    #[test]
    fn project_roots_are_canonical_even_when_workspace_argument_is_not() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let workspace = directory
            .path()
            .join("lexical")
            .join("..")
            .join("workspace");
        let resolver = ProjectResolver::new(workspace, storage).unwrap();
        let project = resolver.resolve(&identity("usr", "conv", None)).unwrap();
        assert_eq!(
            project.project_root,
            project.project_root.canonicalize().unwrap()
        );
        assert_eq!(
            project.metadata_root,
            project.metadata_root.canonicalize().unwrap()
        );
    }

    #[test]
    fn conversations_and_users_are_isolated() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();
        let a = resolver
            .resolve(&identity("usr_a", "conv_1", None))
            .unwrap();
        let b = resolver
            .resolve(&identity("usr_a", "conv_2", None))
            .unwrap();
        let c = resolver
            .resolve(&identity("usr_b", "conv_1", None))
            .unwrap();
        assert_ne!(a.project_root, b.project_root);
        assert_ne!(a.project_root, c.project_root);
        std::fs::write(a.project_root.join("secret.txt"), "a").unwrap();
        assert!(!b.project_root.join("secret.txt").exists());
        assert!(!c.project_root.join("secret.txt").exists());
    }

    #[test]
    fn alias_rejoin_and_binding_persist_across_storage_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("state.sqlite3");
        let workspace = directory.path().join("workspace");
        let identity_a = identity("usr_a", "conv_a", None);
        let identity_b = identity("usr_b", "conv_b", None);
        let expected = {
            let storage = Storage::open(&database).unwrap();
            let resolver = ProjectResolver::new(workspace.clone(), storage).unwrap();
            let (a, joined) = resolver.initialize(&identity_a, Some("rust-demo")).unwrap();
            assert!(!joined);
            assert_eq!(
                a.project_root,
                workspace.canonicalize().unwrap().join("rust-demo")
            );
            let (b, joined) = resolver.initialize(&identity_b, Some("rust-demo")).unwrap();
            assert!(joined);
            assert_eq!(a.effective_project_key, b.effective_project_key);
            std::fs::write(a.project_root.join("shared.txt"), "shared").unwrap();
            a.effective_project_key
        };
        let storage = Storage::open(&database).unwrap();
        let resolver = ProjectResolver::new(workspace, storage).unwrap();
        let restored = resolver.resolve(&identity_b).unwrap();
        assert_eq!(restored.effective_project_key, expected);
        assert_eq!(restored.project_alias.as_deref(), Some("rust-demo"));
        assert_eq!(
            std::fs::read_to_string(restored.project_root.join("shared.txt")).unwrap(),
            "shared"
        );
    }

    #[test]
    fn aliases_are_case_insensitive_so_windows_paths_cannot_collide() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();

        let (first, joined) = resolver
            .initialize(&identity("usr-a", "conv-a", None), Some("ReleaseProject"))
            .unwrap();
        assert!(!joined);
        let (second, joined) = resolver
            .initialize(&identity("usr-b", "conv-b", None), Some("releaseproject"))
            .unwrap();
        assert!(joined);
        assert_eq!(second.project_alias.as_deref(), Some("ReleaseProject"));
        assert_eq!(first.effective_project_key, second.effective_project_key);
        assert_eq!(first.project_root, second.project_root);
        assert_eq!(first.metadata_root, second.metadata_root);
        assert_eq!(first.effective_project_key.as_str(), "ReleaseProject");

        let (isolated, joined) = resolver
            .initialize(&identity("usr-c", "conv-c", None), None)
            .unwrap();
        assert!(!joined);
        assert_ne!(isolated.effective_project_key, first.effective_project_key);
        assert_ne!(isolated.project_root, first.project_root);
    }

    #[test]
    fn aliasless_init_persists_the_initialization_binding() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("state.sqlite3");
        let workspace = directory.path().join("workspace");
        let request = identity("usr", "conv", None);
        {
            let storage = Storage::open(&database).unwrap();
            let resolver = ProjectResolver::new(workspace.clone(), storage).unwrap();
            assert_eq!(
                resolver.resolve_initialized(&request).unwrap_err().code(),
                "TURN_NOT_INITIALIZED"
            );
            let (initialized, joined) = resolver.initialize(&request, None).unwrap();
            assert!(!joined);
            assert_eq!(
                initialized.native_project_key,
                initialized.effective_project_key
            );
        }
        let storage = Storage::open(&database).unwrap();
        let resolver = ProjectResolver::new(workspace, storage).unwrap();
        let restored = resolver.resolve_initialized(&request).unwrap();
        assert_eq!(restored.native_project_key, restored.effective_project_key);
    }

    #[test]
    fn prepare_distinguishes_new_existing_and_joined_projects() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();
        let first = identity("usr_a", "conv_a", None);
        let second = identity("usr_b", "conv_b", None);

        let fresh = resolver.prepare_initialize(&first, None).unwrap();
        assert!(!fresh.reused_existing_binding);
        assert!(!fresh.joined);
        resolver.commit_initialize(&fresh).unwrap();

        let existing = resolver.prepare_initialize(&first, None).unwrap();
        assert!(existing.reused_existing_binding);
        assert!(!existing.joined);

        let alias_owner = identity("usr_c", "conv_c", None);
        let alias = resolver
            .prepare_initialize(&alias_owner, Some("shared-project"))
            .unwrap();
        resolver.commit_initialize(&alias).unwrap();
        let joined = resolver
            .prepare_initialize(&second, Some("shared-project"))
            .unwrap();
        assert!(!joined.reused_existing_binding);
        assert!(joined.joined);
    }

    #[test]
    fn turn_ref_can_branch_same_subject_into_same_effective_project() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();
        let original = identity("usr", "conv-a", None);
        let branch = identity("usr", "conv-b", None);

        let first = resolver
            .prepare_turn_initialize(&original, Some("demo-project"), None)
            .unwrap();
        resolver
            .commit_initialize_with_turn_ref(&first, "r_A", None, "I1", "S1", "brief-A", None)
            .unwrap();

        let prepared_branch = resolver
            .prepare_turn_initialize(&branch, None, Some("r_A"))
            .unwrap();
        assert!(prepared_branch.joined);
        assert!(!prepared_branch.reused_existing_binding);
        assert_eq!(
            prepared_branch.project.effective_project_key,
            first.project.effective_project_key
        );
        let outcome = resolver
            .commit_initialize_with_turn_ref(
                &prepared_branch,
                "r_A1",
                Some("r_A"),
                "I1",
                "S1",
                "brief-A1",
                None,
            )
            .unwrap();
        assert_eq!(outcome.parent_turn_ref.as_deref(), Some("r_A"));
        assert_eq!(
            outcome.parent_native_key.as_deref(),
            Some(first.project.native_project_key.as_str())
        );
    }

    #[test]
    fn turn_ref_branch_is_scoped_to_openai_subject() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();
        let original = identity("usr-a", "conv-a", None);
        let other_user = identity("usr-b", "conv-b", None);

        let first = resolver
            .prepare_turn_initialize(&original, Some("demo-project"), None)
            .unwrap();
        resolver
            .commit_initialize_with_turn_ref(&first, "r_A", None, "I1", "S1", "brief-A", None)
            .unwrap();

        let error = resolver
            .prepare_turn_initialize(&other_user, None, Some("r_A"))
            .unwrap_err();
        assert_eq!(error.code(), "TURN_REF_NOT_FOUND");
    }

    #[test]
    fn bound_conversation_requires_explicit_previous_turn_ref_for_next_turn() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();
        let request = identity("usr", "conv", None);

        let first = resolver
            .prepare_turn_initialize(&request, Some("demo-project"), None)
            .unwrap();
        resolver
            .commit_initialize_with_turn_ref(&first, "r_A", None, "I1", "S1", "brief-A", None)
            .unwrap();

        let missing_parent = resolver
            .prepare_turn_initialize(&request, None, None)
            .unwrap();
        let error = resolver
            .commit_initialize_with_turn_ref(
                &missing_parent,
                "r_B",
                None,
                "I1",
                "S1",
                "brief-B",
                None,
            )
            .unwrap_err();
        assert_eq!(error.code(), "PREVIOUS_TURN_REF_REQUIRED");
    }

    #[test]
    fn bound_conversation_rejects_a_parent_from_another_native_branch_as_stale() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();
        let original = identity("usr", "conv-a", None);
        let current = identity("usr", "conv-b", None);

        let original_root = resolver
            .prepare_turn_initialize(&original, Some("shared-project"), None)
            .unwrap();
        resolver
            .commit_initialize_with_turn_ref(
                &original_root,
                "r_original",
                None,
                "I1",
                "S1",
                "brief-original",
                None,
            )
            .unwrap();

        let current_root = resolver
            .prepare_turn_initialize(&current, Some("shared-project"), None)
            .unwrap();
        resolver
            .commit_initialize_with_turn_ref(
                &current_root,
                "r_current",
                None,
                "I1",
                "S1",
                "brief-current",
                None,
            )
            .unwrap();

        let stale = resolver
            .prepare_turn_initialize(&current, None, Some("r_original"))
            .unwrap();
        let error = resolver
            .commit_initialize_with_turn_ref(
                &stale,
                "r_should_not_commit",
                Some("r_original"),
                "I1",
                "S1",
                "brief-stale",
                None,
            )
            .unwrap_err();
        assert_eq!(error.code(), "STALE_TURN_REF");
    }

    #[test]
    fn explicit_alias_cannot_rebind_a_turn_reference_from_another_project() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();
        let first = identity("usr", "conv-a", None);
        let second = identity("usr", "conv-b", None);

        let first_root = resolver
            .prepare_turn_initialize(&first, Some("project-one"), None)
            .unwrap();
        resolver
            .commit_initialize_with_turn_ref(
                &first_root,
                "r_one",
                None,
                "I1",
                "S1",
                "brief-one",
                None,
            )
            .unwrap();

        let second_root = resolver
            .prepare_turn_initialize(&second, Some("project-two"), None)
            .unwrap();
        resolver
            .commit_initialize_with_turn_ref(
                &second_root,
                "r_two",
                None,
                "I2",
                "S2",
                "brief-two",
                None,
            )
            .unwrap();

        let prepared = resolver
            .prepare_turn_initialize(&second, Some("project-two"), Some("r_one"))
            .unwrap();
        let error = resolver
            .commit_initialize_with_turn_ref(
                &prepared,
                "r_mismatch",
                Some("r_one"),
                "I2",
                "S2",
                "brief-mismatch",
                None,
            )
            .unwrap_err();
        assert_eq!(error.code(), "TURN_PROJECT_MISMATCH");
    }

    #[test]
    fn prepared_initialization_is_not_visible_until_commit() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();
        let request = identity("usr", "conv", None);

        let prepared = resolver
            .prepare_initialize(&request, Some("shared-project"))
            .unwrap();
        assert!(prepared.project.project_root.ends_with("shared-project"));
        assert!(!prepared.project.project_root.exists());
        assert!(!prepared.project.metadata_root.exists());
        assert_eq!(
            resolver.resolve_initialized(&request).unwrap_err().code(),
            "TURN_NOT_INITIALIZED"
        );
        assert_eq!(
            resolver
                .storage()
                .effective_for_alias("shared-project")
                .unwrap(),
            None
        );

        resolver.commit_initialize(&prepared).unwrap();
        assert!(prepared.project.project_root.is_dir());
        assert!(prepared.project.metadata_root.is_dir());
        let initialized = resolver.resolve_initialized(&request).unwrap();
        assert_eq!(initialized.project_alias.as_deref(), Some("shared-project"));
    }

    #[test]
    fn named_project_reuses_a_precreated_checkout_directory() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(workspace.join("production-stress-test")).unwrap();
        std::fs::write(
            workspace.join("production-stress-test/seed.txt"),
            "operator-created",
        )
        .unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(workspace.clone(), storage).unwrap();
        let prepared = resolver
            .prepare_initialize(
                &identity("usr", "conv", None),
                Some("production-stress-test"),
            )
            .unwrap();
        assert_eq!(
            prepared.project.project_root,
            workspace
                .canonicalize()
                .unwrap()
                .join("production-stress-test")
        );
        resolver.commit_initialize(&prepared).unwrap();
        assert_eq!(
            std::fs::read_to_string(prepared.project.project_root.join("seed.txt")).unwrap(),
            "operator-created"
        );
    }

    #[test]
    fn named_project_rejects_a_checkout_path_that_is_a_file_without_binding() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("blocked-project"), "not-a-directory").unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(workspace, storage).unwrap();
        let request = identity("usr", "conv", None);
        let prepared = resolver
            .prepare_initialize(&request, Some("blocked-project"))
            .unwrap();
        let error = resolver.commit_initialize(&prepared).unwrap_err();
        assert_eq!(error.code(), "INVALID_PROJECT_ALIAS");
        assert_eq!(
            resolver.resolve_initialized(&request).unwrap_err().code(),
            "TURN_NOT_INITIALIZED"
        );
    }

    #[test]
    fn failed_new_project_commit_does_not_leave_orphan_checkout() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();

        let first = resolver
            .prepare_turn_initialize(&identity("usr-a", "conv-a", None), Some("first"), None)
            .unwrap();
        resolver
            .commit_initialize_with_turn_ref(
                &first,
                "r_duplicate",
                None,
                "I1",
                "S1",
                "brief-first",
                None,
            )
            .unwrap();

        let failed = resolver
            .prepare_turn_initialize(
                &identity("usr-b", "conv-b", None),
                Some("production-stress-test"),
                None,
            )
            .unwrap();
        assert!(!failed.project.project_root.exists());
        assert!(!failed.project.metadata_root.exists());
        assert!(
            resolver
                .commit_initialize_with_turn_ref(
                    &failed,
                    "r_duplicate",
                    None,
                    "I2",
                    "S2",
                    "brief-failed",
                    None,
                )
                .is_err()
        );
        assert!(!failed.project.project_root.exists());
        assert!(!failed.project.metadata_root.exists());
    }

    #[test]
    fn concurrent_new_alias_commit_fails_closed_without_binding_loser() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();
        let first = identity("usr_a", "conv_a", None);
        let second = identity("usr_b", "conv_b", None);
        let prepared_first = resolver
            .prepare_initialize(&first, Some("shared-project"))
            .unwrap();
        let prepared_second = resolver
            .prepare_initialize(&second, Some("shared-project"))
            .unwrap();

        resolver.commit_initialize(&prepared_first).unwrap();
        let error = resolver.commit_initialize(&prepared_second).unwrap_err();
        assert_eq!(error.code(), "SERVER_BUSY");
        assert_eq!(
            resolver.resolve_initialized(&second).unwrap_err().code(),
            "TURN_NOT_INITIALIZED"
        );
    }

    #[test]
    fn unsafe_aliases_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
        let resolver = ProjectResolver::new(directory.path().join("workspace"), storage).unwrap();
        for alias in [
            "../foo", "/foo", "foo/bar", r"foo\bar", ".", "..", "demo.", "CON", "con.txt", "NUL",
            "COM1", "com9.log", "LPT1", "lpt9.txt",
        ] {
            assert!(resolver.validate_alias(alias).is_err(), "{alias}");
        }
        for alias in ["COM0", "COM10", "LPT0", "LPT10", "console", "demo.txt"] {
            assert!(resolver.validate_alias(alias).is_ok(), "{alias}");
        }
    }
}
