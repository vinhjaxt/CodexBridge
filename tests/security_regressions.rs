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
    std::fs::write(temp.path().join("source.txt"), "source").unwrap();
    let resolver = SecurePathResolver;

    assert!(
        resolver
            .copy_file_secure(temp.path(), "source.txt", "../copy.txt", 1024)
            .is_err()
    );
    assert!(
        resolver
            .move_path_secure(temp.path(), "source.txt", "../move.txt")
            .is_err()
    );
    assert!(temp.path().join("source.txt").is_file());
}

#[cfg(windows)]
#[test]
fn windows_junction_components_cannot_escape_capability_paths() {
    use std::os::windows::process::CommandExt as _;

    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();

    let system_root = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    let command = format!("mklink /J \"linked\" \"{}\"", outside.path().display());
    let mut cmd = std::process::Command::new(system_root.join("System32/cmd.exe"));
    cmd.args(["/d", "/s", "/c"])
        // cmd.exe does not use the standard Windows argv parser. In particular,
        // `/s /c` expects the complete command string to be surrounded by one
        // extra pair of quotes. Rust's normal `.arg()` escaping can backslash-
        // escape the embedded path quotes and make `mklink` report a syntax
        // error. Match the production cmd path and pass the command tail verbatim.
        .raw_arg(format!("\"{command}\""))
        .current_dir(temp.path());
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "failed to create Windows junction: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let resolver = SecurePathResolver;
    for operation in [PathOperation::Existing, PathOperation::Create] {
        let error = resolver
            .resolve_project_path(temp.path(), "linked/secret.txt", operation)
            .unwrap_err();
        assert!(
            matches!(error.code(), "PATH_OUTSIDE_WORKSPACE" | "SYMLINK_ESCAPE"),
            "junction escape returned unexpected error: {error}"
        );
    }
    assert_eq!(
        resolver
            .read_file_bounded(temp.path(), "linked/secret.txt", 1024)
            .unwrap_err()
            .code(),
        "PATH_OUTSIDE_WORKSPACE"
    );
    assert_eq!(
        resolver
            .read_file_range(temp.path(), "linked/secret.txt", 0, 16)
            .unwrap_err()
            .code(),
        "PATH_OUTSIDE_WORKSPACE"
    );
    assert_eq!(
        resolver
            .write_file_atomic(temp.path(), "linked/escaped.txt", b"nope")
            .unwrap_err()
            .code(),
        "PATH_OUTSIDE_WORKSPACE"
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
        "PATH_OUTSIDE_WORKSPACE"
    );
    assert_eq!(
        resolver
            .copy_file_secure(temp.path(), "source.txt", "linked/copied.txt", 1024)
            .unwrap_err()
            .code(),
        "PATH_OUTSIDE_WORKSPACE"
    );
    assert_eq!(
        resolver
            .move_path_secure(temp.path(), "linked/secret.txt", "moved-from-linked.txt")
            .unwrap_err()
            .code(),
        "PATH_OUTSIDE_WORKSPACE"
    );
    assert_eq!(
        resolver
            .move_path_secure(temp.path(), "source.txt", "linked/moved.txt")
            .unwrap_err()
            .code(),
        "PATH_OUTSIDE_WORKSPACE"
    );
    assert!(temp.path().join("source.txt").is_file());
    assert_eq!(
        resolver
            .create_directory_all(temp.path(), "linked/new/directory")
            .unwrap_err()
            .code(),
        "PATH_OUTSIDE_WORKSPACE"
    );
    assert_eq!(
        resolver
            .remove_path_secure(temp.path(), "linked/secret.txt")
            .unwrap_err()
            .code(),
        "PATH_OUTSIDE_WORKSPACE"
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
