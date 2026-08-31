use codex_bridge::sandbox::{PathOperation, SecurePathResolver};

#[test]
fn normal_filesystem_paths_reject_parent_traversal() {
    let temp = tempfile::tempdir().unwrap();
    let resolver = SecurePathResolver;
    let error = resolver
        .resolve_project_path(temp.path(), "../escape.txt", PathOperation::Create)
        .unwrap_err();
    assert_eq!(error.code(), "PATH_OUTSIDE_WORKSPACE");
}

#[cfg(unix)]
#[test]
fn normal_filesystem_paths_reject_symlink_components() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    symlink(outside.path(), temp.path().join("linked")).unwrap();

    let resolver = SecurePathResolver;
    let error = resolver
        .resolve_project_path(temp.path(), "linked/secret.txt", PathOperation::Existing)
        .unwrap_err();
    assert_eq!(error.code(), "SYMLINK_ESCAPE");
}

#[cfg(unix)]
#[test]
fn capability_reads_reject_a_symlink_as_the_final_component() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        temp.path().join("linked.txt"),
    )
    .unwrap();

    let resolver = SecurePathResolver;
    let direct = resolver
        .read_file_bounded(temp.path(), "linked.txt", 1024)
        .unwrap_err();
    assert_eq!(direct.code(), "SYMLINK_ESCAPE");
    let ranged = resolver
        .read_file_range(temp.path(), "linked.txt", 0, 16)
        .unwrap_err();
    assert_eq!(ranged.code(), "SYMLINK_ESCAPE");
}

#[cfg(unix)]
#[test]
fn capability_writes_reject_symlinked_parent_directories() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), temp.path().join("linked")).unwrap();

    let resolver = SecurePathResolver;
    let error = resolver
        .write_file_atomic(temp.path(), "linked/escape.txt", b"nope")
        .unwrap_err();
    assert_eq!(error.code(), "SYMLINK_ESCAPE");
    assert!(!outside.path().join("escape.txt").exists());
}

#[test]
fn capability_copy_and_move_reject_parent_traversal() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("source.txt"), "source").unwrap();
    let resolver = SecurePathResolver;

    let copy_error = resolver
        .copy_file_secure(&project, "source.txt", "../copy.txt", 1024)
        .unwrap_err();
    assert_eq!(
        copy_error.code(),
        "PATH_OUTSIDE_WORKSPACE",
        "copy traversal returned unexpected error: {copy_error}"
    );
    assert!(!temp.path().join("copy.txt").exists());

    let move_error = resolver
        .move_path_secure(&project, "source.txt", "../move.txt")
        .unwrap_err();
    assert_eq!(
        move_error.code(),
        "PATH_OUTSIDE_WORKSPACE",
        "move traversal returned unexpected error: {move_error}"
    );
    assert!(!temp.path().join("move.txt").exists());
    assert_eq!(
        std::fs::read(project.join("source.txt")).unwrap(),
        b"source"
    );
}

#[cfg(windows)]
#[test]
fn windows_junction_components_cannot_escape_capability_paths() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();

    // Build the fixture through the reparse-point API rather than cmd.exe/mklink.
    // Rust paths can use Win32 verbatim syntax (`\\?\`), which is not a stable
    // command-line input contract for external applications. The junction crate
    // normalizes the target, strips a verbatim prefix when present, and writes an
    // IO_REPARSE_TAG_MOUNT_POINT directly, so this test exercises the resolver
    // rather than shell quoting or mklink path parsing.
    let linked = temp.path().join("linked");
    junction::create(outside.path(), &linked).unwrap();
    assert!(
        junction::exists(&linked).unwrap(),
        "fixture is not a junction"
    );

    let resolver = SecurePathResolver;
    let mut junction_rejection = None;
    for operation in [PathOperation::Existing, PathOperation::Create] {
        let error = resolver
            .resolve_project_path(temp.path(), "linked/secret.txt", operation)
            .unwrap_err();
        assert!(
            matches!(error.code(), "PATH_OUTSIDE_WORKSPACE" | "SYMLINK_ESCAPE"),
            "junction escape returned unexpected error: {error}"
        );
        match junction_rejection {
            Some(expected) => assert_eq!(
                error.code(),
                expected,
                "junction rejection changed across path operations"
            ),
            None => junction_rejection = Some(error.code()),
        }
    }
    let junction_rejection = junction_rejection.expect("junction operations were checked");

    // A Windows junction is an IO_REPARSE_TAG_MOUNT_POINT. Depending on how the
    // Rust standard library reports that reparse point through symlink_metadata,
    // the resolver can classify the same safe rejection as either a canonical
    // workspace escape or a symlink/reparse escape. Capability operations must
    // preserve that resolver classification rather than silently following it.
    assert_eq!(
        resolver
            .read_file_bounded(temp.path(), "linked/secret.txt", 1024)
            .unwrap_err()
            .code(),
        junction_rejection
    );
    assert_eq!(
        resolver
            .read_file_range(temp.path(), "linked/secret.txt", 0, 16)
            .unwrap_err()
            .code(),
        junction_rejection
    );
    assert_eq!(
        resolver
            .write_file_atomic(temp.path(), "linked/escaped.txt", b"nope")
            .unwrap_err()
            .code(),
        junction_rejection
    );

    std::fs::write(temp.path().join("source.txt"), b"source").unwrap();
    assert_eq!(
        resolver
            .copy_file_secure(
                temp.path(),
                "linked/secret.txt",
                "copy-from-linked.txt",
                1024,
            )
            .unwrap_err()
            .code(),
        junction_rejection
    );
    assert_eq!(
        resolver
            .copy_file_secure(temp.path(), "source.txt", "linked/copied.txt", 1024)
            .unwrap_err()
            .code(),
        junction_rejection
    );
    assert_eq!(
        resolver
            .move_path_secure(temp.path(), "linked/secret.txt", "moved-from-linked.txt")
            .unwrap_err()
            .code(),
        junction_rejection
    );
    assert_eq!(
        resolver
            .move_path_secure(temp.path(), "source.txt", "linked/moved.txt")
            .unwrap_err()
            .code(),
        junction_rejection
    );
    assert!(temp.path().join("source.txt").is_file());
    assert_eq!(
        resolver
            .create_directory_all(temp.path(), "linked/new/directory")
            .unwrap_err()
            .code(),
        junction_rejection
    );
    assert_eq!(
        resolver
            .remove_path_secure(temp.path(), "linked/secret.txt")
            .unwrap_err()
            .code(),
        junction_rejection
    );

    assert!(!outside.path().join("escaped.txt").exists());
    assert!(!outside.path().join("copied.txt").exists());
    assert!(!outside.path().join("moved.txt").exists());
    assert!(!outside.path().join("new").exists());
    assert_eq!(
        std::fs::read(outside.path().join("secret.txt")).unwrap(),
        b"secret"
    );
}
