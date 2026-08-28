use codex_bridge::sandbox::{PathOperation, SecurePathResolver};

#[test]
fn capability_filesystem_round_trip_create_read_copy_move_and_remove() {
    let temp = tempfile::tempdir().unwrap();
    let resolver = SecurePathResolver;
    resolver
        .write_file_atomic(temp.path(), "src/nested/input.txt", b"hello")
        .unwrap();
    assert_eq!(
        resolver
            .read_file_bounded(temp.path(), "src/nested/input.txt", 16)
            .unwrap(),
        b"hello"
    );
    assert_eq!(
        resolver
            .copy_file_secure(temp.path(), "src/nested/input.txt", "copy.txt", 16)
            .unwrap(),
        5
    );
    resolver
        .move_path_secure(temp.path(), "copy.txt", "moved/output.txt")
        .unwrap();
    assert_eq!(
        resolver
            .read_file_bounded(temp.path(), "moved/output.txt", 16)
            .unwrap(),
        b"hello"
    );
    resolver.remove_path_secure(temp.path(), "src").unwrap();
    assert!(!temp.path().join("src").exists());
}

#[test]
fn create_and_existing_resolution_have_distinct_missing_path_semantics() {
    let temp = tempfile::tempdir().unwrap();
    let resolver = SecurePathResolver;
    let future = resolver
        .resolve_project_path(temp.path(), "future/deep/file.txt", PathOperation::Create)
        .unwrap();
    assert!(future.ends_with("future/deep/file.txt"));
    assert_eq!(
        resolver
            .resolve_project_path(temp.path(), "future/deep/file.txt", PathOperation::Existing,)
            .unwrap_err()
            .code(),
        "FILE_NOT_FOUND"
    );
}

#[test]
fn ranged_reads_return_total_length_and_allow_exact_eof() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("large.txt"), b"0123456789abcdef").unwrap();
    let resolver = SecurePathResolver;
    let (middle, total) = resolver
        .read_file_range(temp.path(), "large.txt", 4, 6)
        .unwrap();
    assert_eq!(middle, b"456789");
    assert_eq!(total, 16);
    let (eof, eof_total) = resolver
        .read_file_range(temp.path(), "large.txt", 16, 8)
        .unwrap();
    assert!(eof.is_empty());
    assert_eq!(eof_total, total);
}

#[test]
fn bounded_reads_fail_closed_before_returning_partial_whole_file_content() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("data.txt"), b"123456").unwrap();
    let resolver = SecurePathResolver;
    assert_eq!(
        resolver
            .read_file_bounded(temp.path(), "data.txt", 5)
            .unwrap_err()
            .code(),
        "RESOURCE_LIMIT_EXCEEDED"
    );
    assert_eq!(
        resolver
            .read_file_bounded(temp.path(), "data.txt", 6)
            .unwrap(),
        b"123456"
    );
}

#[test]
fn ranged_reads_work_across_large_files_without_whole_file_limit() {
    let temp = tempfile::tempdir().unwrap();
    let resolver = SecurePathResolver;
    let mut bytes = vec![b'a'; 9 * 1024 * 1024];
    bytes[8 * 1024 * 1024 + 123] = b'Z';
    std::fs::write(temp.path().join("large.bin"), &bytes).unwrap();

    let (window, total) = resolver
        .read_file_range(temp.path(), "large.bin", (8 * 1024 * 1024 + 120) as u64, 8)
        .unwrap();
    assert_eq!(total as usize, bytes.len());
    assert_eq!(window, b"aaaZaaaa");
}

#[test]
fn write_creates_parent_directories_and_replaces_content() {
    let temp = tempfile::tempdir().unwrap();
    let resolver = SecurePathResolver;
    resolver
        .write_file_atomic(temp.path(), "deep/nested/file.txt", b"first")
        .unwrap();
    resolver
        .write_file_atomic(temp.path(), "deep/nested/file.txt", b"second")
        .unwrap();
    assert_eq!(
        resolver
            .read_file_bounded(temp.path(), "deep/nested/file.txt", 16)
            .unwrap(),
        b"second"
    );
}

#[test]
fn secure_copy_respects_source_size_limit_without_creating_destination() {
    let temp = tempfile::tempdir().unwrap();
    let resolver = SecurePathResolver;
    std::fs::write(temp.path().join("source.txt"), b"123456").unwrap();
    let error = resolver
        .copy_file_secure(temp.path(), "source.txt", "copy.txt", 5)
        .unwrap_err();
    assert_eq!(error.code(), "RESOURCE_LIMIT_EXCEEDED");
    assert!(!temp.path().join("copy.txt").exists());
}

#[test]
fn dot_components_are_normalized_but_backslashes_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let resolver = SecurePathResolver;
    std::fs::create_dir_all(temp.path().join("a")).unwrap();
    std::fs::write(temp.path().join("a/b.txt"), b"ok").unwrap();
    assert_eq!(
        resolver
            .read_file_bounded(temp.path(), "a/./b.txt", 8)
            .unwrap(),
        b"ok"
    );
    assert_eq!(
        resolver
            .resolve_project_path(temp.path(), r"a\b.txt", PathOperation::Existing)
            .unwrap_err()
            .code(),
        "PATH_OUTSIDE_WORKSPACE"
    );
}

#[cfg(unix)]
#[test]
fn removing_a_symlink_removes_only_the_link_not_its_target() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("target.txt"), b"keep").unwrap();
    symlink(
        outside.path().join("target.txt"),
        temp.path().join("link.txt"),
    )
    .unwrap();
    let resolver = SecurePathResolver;
    resolver
        .remove_path_secure(temp.path(), "link.txt")
        .unwrap();
    assert!(!temp.path().join("link.txt").exists());
    assert_eq!(
        std::fs::read(outside.path().join("target.txt")).unwrap(),
        b"keep"
    );
}

#[cfg(unix)]
#[test]
fn copying_a_symlink_source_is_rejected() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        temp.path().join("link.txt"),
    )
    .unwrap();
    let resolver = SecurePathResolver;
    let error = resolver
        .copy_file_secure(temp.path(), "link.txt", "copy.txt", 1024)
        .unwrap_err();
    assert_eq!(error.code(), "SYMLINK_ESCAPE");
    assert!(!temp.path().join("copy.txt").exists());
}
