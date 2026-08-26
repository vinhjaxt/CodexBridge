use codex_bridge::{
    project::ProjectResolver,
    request_context::{RequestIdentity, TransportMode},
    storage::{PlanItemRecord, Storage},
};

fn identity(subject: &str, conversation: &str) -> RequestIdentity {
    RequestIdentity {
        openai_subject: subject.to_owned(),
        openai_conversation_id: conversation.to_owned(),
        mcp_session_id: None,
        transport_mode: TransportMode::Stateless,
    }
}

#[test]
fn memory_and_plan_survive_storage_reopen_together() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("state.sqlite3");
    {
        let storage = Storage::open(&database).unwrap();
        storage
            .memory_set("project", "decision", "keep sqlite")
            .unwrap();
        storage
            .plan_set(
                "project",
                Some("contract".to_owned()),
                vec![PlanItemRecord {
                    step: "verify".to_owned(),
                    status: "completed".to_owned(),
                }],
            )
            .unwrap();
    }

    let reopened = Storage::open(&database).unwrap();
    assert_eq!(
        reopened
            .memory_get("project", "decision")
            .unwrap()
            .as_deref(),
        Some("keep sqlite")
    );
    let plan = reopened.plan_get("project").unwrap().unwrap();
    assert_eq!(plan.explanation.as_deref(), Some("contract"));
    assert_eq!(plan.items[0].step, "verify");
    assert_eq!(plan.items[0].status, "completed");
}

#[test]
fn invalid_plan_update_preserves_last_committed_state() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let saved = storage
        .plan_set(
            "project",
            None,
            vec![PlanItemRecord {
                step: "keep".to_owned(),
                status: "in_progress".to_owned(),
            }],
        )
        .unwrap();

    let error = storage
        .plan_set(
            "project",
            None,
            vec![
                PlanItemRecord {
                    step: "one".to_owned(),
                    status: "in_progress".to_owned(),
                },
                PlanItemRecord {
                    step: "two".to_owned(),
                    status: "in_progress".to_owned(),
                },
            ],
        )
        .unwrap_err();
    assert_eq!(error.code(), "INVALID_INPUT");
    assert_eq!(storage.plan_get("project").unwrap().unwrap(), saved);
}

#[test]
fn conversations_joining_one_alias_share_effective_state_but_not_native_identity() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let resolver = ProjectResolver::new(temp.path().join("workspace"), storage.clone()).unwrap();
    let owner = resolver
        .initialize(&identity("owner", "conversation-a"), Some("team"))
        .unwrap()
        .0;
    let joiner = resolver
        .initialize(&identity("joiner", "conversation-b"), Some("team"))
        .unwrap()
        .0;

    assert_ne!(owner.native_project_key, joiner.native_project_key);
    assert_eq!(owner.effective_project_key, joiner.effective_project_key);
    storage
        .memory_set(owner.effective_project_key.as_str(), "shared", "visible")
        .unwrap();
    assert_eq!(
        storage
            .memory_get(joiner.effective_project_key.as_str(), "shared")
            .unwrap()
            .as_deref(),
        Some("visible")
    );
}

#[test]
fn fresh_conversation_joining_existing_project_key_reuses_existing_folder_and_memory() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let workspace = temp.path().join("workspace");
    let resolver = ProjectResolver::new(workspace.clone(), storage.clone()).unwrap();

    let owner = resolver
        .initialize(&identity("owner", "conversation-a"), Some("shared-folder"))
        .unwrap()
        .0;
    let expected_project_root = workspace.canonicalize().unwrap().join("shared-folder");
    assert_eq!(owner.project_root, expected_project_root);
    assert!(owner.project_root.is_dir());
    std::fs::write(
        owner.project_root.join("checkout-marker.txt"),
        "existing checkout",
    )
    .unwrap();
    storage
        .memory_set(
            owner.effective_project_key.as_str(),
            "architecture/decision",
            "reuse existing project memory",
        )
        .unwrap();

    let prepared = resolver
        .prepare_initialize(&identity("joiner", "conversation-b"), Some("shared-folder"))
        .unwrap();
    assert!(prepared.joined);
    assert!(!prepared.reused_existing_binding);
    assert_eq!(
        prepared.project.effective_project_key,
        owner.effective_project_key
    );
    assert_eq!(prepared.project.project_root, expected_project_root);
    resolver.commit_initialize(&prepared).unwrap();

    assert_eq!(
        std::fs::read_to_string(prepared.project.project_root.join("checkout-marker.txt")).unwrap(),
        "existing checkout"
    );
    assert_eq!(
        storage
            .memory_get(
                prepared.project.effective_project_key.as_str(),
                "architecture/decision",
            )
            .unwrap()
            .as_deref(),
        Some("reuse existing project memory")
    );
}

#[test]
fn preexisting_directory_without_alias_is_not_treated_as_existing_project_binding() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let workspace = temp.path().join("workspace");
    let resolver = ProjectResolver::new(workspace.clone(), storage.clone()).unwrap();
    let existing_folder = workspace.canonicalize().unwrap().join("directory-only");
    std::fs::create_dir(&existing_folder).unwrap();
    std::fs::write(
        existing_folder.join("checkout-marker.txt"),
        "preexisting folder",
    )
    .unwrap();

    assert_eq!(storage.effective_for_alias("directory-only").unwrap(), None);

    let prepared = resolver
        .prepare_initialize(
            &identity("user", "fresh-conversation"),
            Some("directory-only"),
        )
        .unwrap();
    assert!(!prepared.joined);
    assert!(!prepared.reused_existing_binding);
    assert_eq!(
        prepared.project.effective_project_key.as_str(),
        "directory-only"
    );
    assert_eq!(prepared.project.project_root, existing_folder);
    resolver.commit_initialize(&prepared).unwrap();

    assert_eq!(
        std::fs::read_to_string(prepared.project.project_root.join("checkout-marker.txt")).unwrap(),
        "preexisting folder"
    );
    assert_eq!(
        storage
            .effective_for_alias("directory-only")
            .unwrap()
            .as_deref(),
        Some("directory-only")
    );
    assert_eq!(
        storage
            .memory_get(
                prepared.project.effective_project_key.as_str(),
                "architecture/decision",
            )
            .unwrap(),
        None
    );
}

#[test]
fn unrelated_effective_projects_keep_memory_isolated() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let resolver = ProjectResolver::new(temp.path().join("workspace"), storage.clone()).unwrap();
    let first = resolver
        .initialize(&identity("user", "one"), None)
        .unwrap()
        .0;
    let second = resolver
        .initialize(&identity("user", "two"), None)
        .unwrap()
        .0;
    assert_ne!(first.effective_project_key, second.effective_project_key);

    storage
        .memory_set(first.effective_project_key.as_str(), "private", "one")
        .unwrap();
    assert_eq!(
        storage
            .memory_get(second.effective_project_key.as_str(), "private")
            .unwrap(),
        None
    );
}
