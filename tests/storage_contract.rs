use std::sync::{Arc, Barrier};

use codex_bridge::storage::{PlanItemRecord, Storage};

#[test]
fn memory_pagination_is_sorted_lossless_and_stable() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    for (key, value) in [
        ("delta", "4"),
        ("alpha", "1"),
        ("charlie", "3"),
        ("bravo", "2"),
        ("echo", "5"),
    ] {
        storage.memory_archive_set("project", key, value).unwrap();
    }

    let (first, snapshot_hash) = storage
        .memory_archive_recall_page_from_snapshot("project", 0, 2, None)
        .unwrap();
    assert_eq!(
        first
            .notes
            .iter()
            .map(|note| note.key.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "bravo"]
    );
    assert_eq!(first.total, 5);
    assert_eq!(first.offset, 0);
    assert!(first.truncated);
    assert_eq!(first.next_offset, Some(2));

    let (second, second_snapshot_hash) = storage
        .memory_archive_recall_page_from_snapshot(
            "project",
            first.next_offset.unwrap(),
            2,
            Some(&snapshot_hash),
        )
        .unwrap();
    assert_eq!(
        second
            .notes
            .iter()
            .map(|note| note.key.as_str())
            .collect::<Vec<_>>(),
        vec!["charlie", "delta"]
    );
    assert_eq!(second.next_offset, Some(4));
    assert_eq!(second_snapshot_hash, snapshot_hash);

    let (third, third_snapshot_hash) = storage
        .memory_archive_recall_page_from_snapshot(
            "project",
            second.next_offset.unwrap(),
            2,
            Some(&snapshot_hash),
        )
        .unwrap();
    assert_eq!(third.notes.len(), 1);
    assert_eq!(third.notes[0].key, "echo");
    assert!(!third.truncated);
    assert_eq!(third.next_offset, None);
    assert_eq!(third_snapshot_hash, snapshot_hash);

    let records = first
        .notes
        .iter()
        .chain(&second.notes)
        .chain(&third.notes)
        .map(|note| (note.key.as_str(), note.value.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        records,
        vec![
            ("alpha", "1"),
            ("bravo", "2"),
            ("charlie", "3"),
            ("delta", "4"),
            ("echo", "5"),
        ]
    );

    storage
        .memory_archive_set("project", "alpha", "updated")
        .unwrap();
    let error = storage
        .memory_archive_recall_page_from_snapshot("project", 2, 2, Some(&snapshot_hash))
        .unwrap_err();
    assert_eq!(error.code(), "PAGINATION_STALE");
}

#[test]
fn memory_pagination_accepts_exact_end_and_rejects_past_end() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    storage.memory_archive_set("project", "a", "1").unwrap();
    storage.memory_archive_set("project", "b", "2").unwrap();

    let (end, snapshot_hash) = storage
        .memory_archive_recall_page_from_snapshot("project", 2, 10, None)
        .unwrap();
    assert!(end.notes.is_empty());
    assert_eq!(end.total, 2);
    assert!(!end.truncated);
    assert_eq!(end.next_offset, None);

    let error = storage
        .memory_archive_recall_page_from_snapshot("project", 3, 10, Some(&snapshot_hash))
        .unwrap_err();
    assert_eq!(error.code(), "INVALID_INPUT");
}

#[test]
fn memory_pagination_rejects_zero_page_size() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let error = storage
        .memory_archive_recall_page_from_snapshot("project", 0, 0, None)
        .unwrap_err();
    assert_eq!(error.code(), "INVALID_INPUT");
}

#[test]
fn semantic_memory_hash_is_independent_of_insertion_order() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    for (key, value) in [("z", "last"), ("a", "first"), ("m", "middle")] {
        storage.memory_set("left", key, value).unwrap();
    }
    for (key, value) in [("a", "first"), ("m", "middle"), ("z", "last")] {
        storage.memory_set("right", key, value).unwrap();
    }
    assert_eq!(
        storage.memory_semantic_hash("left").unwrap(),
        storage.memory_semantic_hash("right").unwrap()
    );
}

#[test]
fn semantic_memory_hash_changes_for_update_and_delete() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    storage.memory_set("project", "decision", "one").unwrap();
    let first = storage.memory_semantic_hash("project").unwrap();

    storage.memory_set("project", "decision", "two").unwrap();
    let second = storage.memory_semantic_hash("project").unwrap();
    assert_ne!(first, second);

    assert!(storage.memory_delete("project", "decision").unwrap());
    let third = storage.memory_semantic_hash("project").unwrap();
    assert_ne!(second, third);
}

#[test]
fn semantic_memory_hash_excludes_archive_history() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    storage.memory_set("project", "active", "before").unwrap();
    let before = storage.memory_semantic_hash("project").unwrap();
    storage
        .memory_archive_set("project", "historical", "after")
        .unwrap();
    let after = storage.memory_semantic_hash("project").unwrap();
    assert_eq!(before, after);
}

#[test]
fn concurrent_memory_writers_preserve_all_distinct_keys() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    let barrier = Arc::new(Barrier::new(9));
    let mut workers = Vec::new();
    for index in 0..8 {
        let storage = storage.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            storage
                .memory_set(
                    "project",
                    &format!("worker-{index}"),
                    &format!("value-{index}"),
                )
                .unwrap();
        }));
    }
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(storage.memory_count("project").unwrap(), 8);
    for index in 0..8 {
        assert_eq!(
            storage
                .memory_get("project", &format!("worker-{index}"))
                .unwrap()
                .as_deref(),
            Some(format!("value-{index}").as_str())
        );
    }
}

#[test]
fn plan_and_memory_updates_do_not_overwrite_each_other() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&temp.path().join("state.sqlite3")).unwrap();
    storage.memory_set("project", "note", "keep").unwrap();
    storage
        .plan_set(
            "project",
            Some("why".to_owned()),
            vec![PlanItemRecord {
                step: "work".to_owned(),
                status: "in_progress".to_owned(),
            }],
        )
        .unwrap();

    storage.memory_set("project", "note", "updated").unwrap();
    assert_eq!(
        storage.memory_get("project", "note").unwrap().as_deref(),
        Some("updated")
    );
    let plan = storage.plan_get("project").unwrap().unwrap();
    assert_eq!(plan.explanation.as_deref(), Some("why"));
    assert_eq!(plan.items[0].step, "work");
    assert_eq!(plan.items[0].status, "in_progress");
}
