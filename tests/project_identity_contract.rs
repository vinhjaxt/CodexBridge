use codex_bridge::{
    project::ProjectResolver,
    request_context::{RequestIdentity, TransportMode},
    storage::Storage,
};

fn identity(subject: &str, conversation: &str, mcp_session_id: Option<&str>) -> RequestIdentity {
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
fn same_subject_and_conversation_have_stable_native_identity_after_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("state.sqlite3");
    let workspace = temp.path().join("workspace");
    let request = identity("user", "conversation", None);
    let first_key = {
        let storage = Storage::open(&database).unwrap();
        let resolver = ProjectResolver::new(workspace.clone(), storage)
            .unwrap()
            .with_native_project_fallback(true);
        resolver.resolve(&request).unwrap().native_project_key
    };
    let storage = Storage::open(&database).unwrap();
    let resolver = ProjectResolver::new(workspace, storage)
        .unwrap()
        .with_native_project_fallback(true);
    assert_eq!(
        resolver.resolve(&request).unwrap().native_project_key,
        first_key
    );
}

#[test]
fn changing_subject_or_conversation_changes_native_identity() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let resolver = ProjectResolver::new(temp.path().join("workspace"), storage)
        .unwrap()
        .with_native_project_fallback(true);
    let baseline = resolver
        .resolve(&identity("user-a", "conversation-a", None))
        .unwrap()
        .native_project_key;
    assert_ne!(
        baseline,
        resolver
            .resolve(&identity("user-b", "conversation-a", None))
            .unwrap()
            .native_project_key
    );
    assert_ne!(
        baseline,
        resolver
            .resolve(&identity("user-a", "conversation-b", None))
            .unwrap()
            .native_project_key
    );
}

#[test]
fn transport_session_id_never_changes_project_identity() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let resolver = ProjectResolver::new(temp.path().join("workspace"), storage)
        .unwrap()
        .with_native_project_fallback(true);
    let stateless = resolver
        .resolve(&identity("user", "conversation", None))
        .unwrap();
    let legacy = resolver
        .resolve(&identity("user", "conversation", Some("mcp-session-123")))
        .unwrap();
    assert_eq!(stateless.native_project_key, legacy.native_project_key);
    assert_eq!(
        stateless.effective_project_key,
        legacy.effective_project_key
    );
    assert_eq!(stateless.transport_mode, TransportMode::Stateless);
    assert_eq!(legacy.transport_mode, TransportMode::LegacySession);
}

#[test]
fn alias_length_boundary_is_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let resolver = ProjectResolver::new(temp.path().join("workspace"), storage).unwrap();
    let valid = format!("a{}", "x".repeat(127));
    let too_long = format!("a{}", "x".repeat(128));
    resolver.validate_alias(&valid).unwrap();
    assert_eq!(
        resolver.validate_alias(&too_long).unwrap_err().code(),
        "INVALID_PROJECT_ALIAS"
    );
}

#[test]
fn joining_alias_changes_only_effective_identity_not_native_identity() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let resolver = ProjectResolver::new(temp.path().join("workspace"), storage)
        .unwrap()
        .with_native_project_fallback(true);
    let owner_request = identity("owner", "one", None);
    let joiner_request = identity("joiner", "two", None);
    let owner_native = resolver.resolve(&owner_request).unwrap().native_project_key;
    let joiner_native = resolver
        .resolve(&joiner_request)
        .unwrap()
        .native_project_key;
    assert_ne!(owner_native, joiner_native);

    let (owner, owner_joined) = resolver.initialize(&owner_request, Some("team")).unwrap();
    let (joiner, joiner_joined) = resolver.initialize(&joiner_request, Some("team")).unwrap();
    assert!(!owner_joined);
    assert!(joiner_joined);
    assert_eq!(owner.native_project_key, owner_native);
    assert_eq!(joiner.native_project_key, joiner_native);
    assert_eq!(owner.effective_project_key, joiner.effective_project_key);
}

#[test]
fn resolve_initialized_fails_before_commit_and_succeeds_after_commit() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let resolver = ProjectResolver::new(temp.path().join("workspace"), storage)
        .unwrap()
        .with_native_project_fallback(true);
    let request = identity("user", "conversation", None);
    let prepared = resolver.prepare_initialize(&request, None).unwrap();
    assert_eq!(
        resolver.resolve_initialized(&request).unwrap_err().code(),
        "TURN_NOT_INITIALIZED"
    );
    resolver.commit_initialize(&prepared).unwrap();
    assert_eq!(
        resolver
            .resolve_initialized(&request)
            .unwrap()
            .effective_project_key,
        prepared.project.effective_project_key
    );
}
