use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::OnceLock,
    time::Duration,
};

use serde::Serialize;
use tokio::{io::AsyncReadExt, process::Command};

#[cfg(windows)]
use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::{
    config::Config,
    error::{AppError, Result},
    project::ProjectContext,
};

#[derive(Debug, Clone, Copy)]
pub enum PathOperation {
    Existing,
    Create,
}

#[derive(Clone, Default)]
pub struct SecurePathResolver;

impl SecurePathResolver {
    pub fn resolve_project_path(
        &self,
        project_root: &Path,
        user_path: &str,
        operation: PathOperation,
    ) -> Result<PathBuf> {
        if user_path.is_empty() || user_path.len() > 4096 || user_path.contains('\0') {
            return Err(AppError::new(
                "PATH_OUTSIDE_WORKSPACE",
                "path is empty or too long",
            ));
        }
        if user_path.contains('\\') {
            return Err(AppError::new(
                "PATH_OUTSIDE_WORKSPACE",
                "backslash and Windows/UNC paths are not accepted",
            ));
        }
        let relative = Path::new(user_path);
        if relative.is_absolute() {
            return Err(AppError::new(
                "PATH_OUTSIDE_WORKSPACE",
                "absolute paths are not accepted",
            ));
        }
        for component in relative.components() {
            if !matches!(component, Component::Normal(_) | Component::CurDir) {
                return Err(AppError::new(
                    "PATH_OUTSIDE_WORKSPACE",
                    "path traversal is not accepted",
                ));
            }
        }

        let canonical_root = project_root.canonicalize()?;
        let mut current = canonical_root.clone();
        let components: Vec<&OsStr> = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(value) => Some(value),
                _ => None,
            })
            .collect();
        for (index, component) in components.iter().enumerate() {
            current.push(component);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        return Err(AppError::new(
                            "SYMLINK_ESCAPE",
                            format!("symlink component rejected: {}", relative.display()),
                        ));
                    }
                    let canonical = current.canonicalize()?;
                    if !canonical.starts_with(&canonical_root) {
                        return Err(AppError::new(
                            "PATH_OUTSIDE_WORKSPACE",
                            "canonical path escaped the project",
                        ));
                    }
                    current = canonical;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if matches!(operation, PathOperation::Existing) {
                        return Err(AppError::new(
                            "FILE_NOT_FOUND",
                            format!("{} does not exist", relative.display()),
                        ));
                    }
                    for rest in &components[index + 1..] {
                        current.push(rest);
                    }
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        if !current.starts_with(&canonical_root) {
            return Err(AppError::new(
                "PATH_OUTSIDE_WORKSPACE",
                "resolved path escaped the project",
            ));
        }
        Ok(current)
    }

    pub fn read_file_bounded(
        &self,
        project_root: &Path,
        user_path: &str,
        maximum: usize,
    ) -> Result<Vec<u8>> {
        #[cfg(unix)]
        {
            unix_capability::read_file_bounded(project_root, user_path, maximum)
        }
        #[cfg(not(unix))]
        {
            let path =
                self.resolve_project_path(project_root, user_path, PathOperation::Existing)?;
            let file = std::fs::File::open(&path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(AppError::new("INVALID_INPUT", "path is not a regular file"));
            }
            if metadata.len() as usize > maximum {
                return Err(AppError::new(
                    "RESOURCE_LIMIT_EXCEEDED",
                    "file exceeds read limit",
                ));
            }
            use std::io::Read as _;
            let mut bytes = Vec::with_capacity((metadata.len() as usize).min(maximum));
            file.take((maximum + 1) as u64).read_to_end(&mut bytes)?;
            if bytes.len() > maximum {
                return Err(AppError::new(
                    "RESOURCE_LIMIT_EXCEEDED",
                    "file exceeds read limit",
                ));
            }
            Ok(bytes)
        }
    }

    /// Open one regular file without following symlinks. Callers that need a
    /// multi-phase read can keep this descriptor open so metadata and content
    /// come from the same inode even if the pathname is atomically replaced.
    pub(crate) fn open_regular_file(
        &self,
        project_root: &Path,
        user_path: &str,
    ) -> Result<std::fs::File> {
        #[cfg(unix)]
        {
            unix_capability::open_regular_file(project_root, user_path)
        }
        #[cfg(not(unix))]
        {
            let path =
                self.resolve_project_path(project_root, user_path, PathOperation::Existing)?;
            let file = std::fs::File::open(path)?;
            if !file.metadata()?.is_file() {
                return Err(AppError::new("INVALID_INPUT", "path is not a regular file"));
            }
            Ok(file)
        }
    }

    /// Read a bounded byte range without requiring the whole file to fit in
    /// memory. Returns the bytes and the file's total length. The same
    /// descriptor-relative/no-follow path walk used by direct reads protects
    /// the open operation from symlink replacement races.
    pub fn read_file_range(
        &self,
        project_root: &Path,
        user_path: &str,
        offset: u64,
        maximum: usize,
    ) -> Result<(Vec<u8>, u64)> {
        #[cfg(unix)]
        {
            unix_capability::read_file_range(project_root, user_path, offset, maximum)
        }
        #[cfg(not(unix))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let path =
                self.resolve_project_path(project_root, user_path, PathOperation::Existing)?;
            let mut file = std::fs::File::open(path)?;
            let length = file.metadata()?.len();
            if offset > length {
                return Err(AppError::new("INVALID_INPUT", "offset is outside the file"));
            }
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = Vec::with_capacity(maximum.min(length.saturating_sub(offset) as usize));
            file.take(maximum as u64).read_to_end(&mut bytes)?;
            Ok((bytes, length))
        }
    }

    pub fn write_file_atomic(
        &self,
        project_root: &Path,
        user_path: &str,
        data: &[u8],
    ) -> Result<()> {
        #[cfg(unix)]
        {
            unix_capability::write_file_atomic(project_root, user_path, data)
        }
        #[cfg(not(unix))]
        {
            let path = self.resolve_project_path(project_root, user_path, PathOperation::Create)?;
            let parent = path
                .parent()
                .ok_or_else(|| AppError::new("PATH_OUTSIDE_WORKSPACE", "target has no parent"))?;
            std::fs::create_dir_all(parent)?;
            let temporary = parent.join(format!(".rust-agent-{}.tmp", uuid::Uuid::now_v7()));
            std::fs::write(&temporary, data)?;
            std::fs::rename(temporary, path)?;
            Ok(())
        }
    }

    pub fn create_directory_all(&self, project_root: &Path, user_path: &str) -> Result<()> {
        #[cfg(unix)]
        {
            unix_capability::create_directory_all(project_root, user_path)
        }
        #[cfg(not(unix))]
        {
            let path = self.resolve_project_path(project_root, user_path, PathOperation::Create)?;
            std::fs::create_dir_all(path)?;
            Ok(())
        }
    }

    pub fn copy_file_secure(
        &self,
        project_root: &Path,
        source: &str,
        destination: &str,
        maximum: usize,
    ) -> Result<u64> {
        let bytes = self.read_file_bounded(project_root, source, maximum)?;
        self.write_file_atomic(project_root, destination, &bytes)?;
        Ok(bytes.len() as u64)
    }

    pub fn move_path_secure(
        &self,
        project_root: &Path,
        source: &str,
        destination: &str,
    ) -> Result<()> {
        #[cfg(unix)]
        {
            unix_capability::move_path(project_root, source, destination)
        }
        #[cfg(not(unix))]
        {
            let source =
                self.resolve_project_path(project_root, source, PathOperation::Existing)?;
            let destination =
                self.resolve_project_path(project_root, destination, PathOperation::Create)?;
            let destination_parent = destination.parent().ok_or_else(|| {
                AppError::new("PATH_OUTSIDE_WORKSPACE", "destination has no parent")
            })?;
            std::fs::create_dir_all(destination_parent)?;
            std::fs::rename(source, destination)?;
            Ok(())
        }
    }

    pub fn remove_path_secure(&self, project_root: &Path, user_path: &str) -> Result<()> {
        #[cfg(unix)]
        {
            unix_capability::remove_path(project_root, user_path)
        }
        #[cfg(not(unix))]
        {
            let path =
                self.resolve_project_path(project_root, user_path, PathOperation::Existing)?;
            if std::fs::metadata(&path)?.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
            Ok(())
        }
    }
}

#[cfg(unix)]
mod unix_capability {
    use std::{
        ffi::CString,
        fs::File,
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
            unix::ffi::OsStrExt,
        },
        path::{Component, Path},
    };

    use uuid::Uuid;

    use crate::error::{AppError, Result};

    fn components(user_path: &str) -> Result<Vec<CString>> {
        if user_path.is_empty()
            || user_path.len() > 4096
            || user_path.contains('\0')
            || user_path.contains('\\')
        {
            return Err(AppError::new(
                "PATH_OUTSIDE_WORKSPACE",
                "invalid relative path",
            ));
        }
        let path = Path::new(user_path);
        if path.is_absolute() {
            return Err(AppError::new(
                "PATH_OUTSIDE_WORKSPACE",
                "absolute paths are not accepted",
            ));
        }
        let mut values = Vec::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(value) => {
                    values.push(CString::new(value.as_bytes()).map_err(|_| {
                        AppError::new("PATH_OUTSIDE_WORKSPACE", "path contains NUL")
                    })?)
                }
                _ => {
                    return Err(AppError::new(
                        "PATH_OUTSIDE_WORKSPACE",
                        "path traversal is not accepted",
                    ));
                }
            }
        }
        if values.is_empty() {
            return Err(AppError::new("PATH_OUTSIDE_WORKSPACE", "path is empty"));
        }
        Ok(values)
    }

    fn open_root(root: &Path) -> Result<File> {
        let bytes = root.as_os_str().as_bytes();
        let path = CString::new(bytes)
            .map_err(|_| AppError::new("PATH_OUTSIDE_WORKSPACE", "invalid project root"))?;
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        file_from_fd(fd)
    }

    fn file_from_fd(fd: RawFd) -> Result<File> {
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            let code = if error
                .raw_os_error()
                .is_some_and(|code| code == libc::ELOOP || code == libc::ENOTDIR)
            {
                "SYMLINK_ESCAPE"
            } else if error.kind() == std::io::ErrorKind::NotFound {
                "FILE_NOT_FOUND"
            } else {
                "PROCESS_FAILED"
            };
            return Err(AppError::new(code, error.to_string()));
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn open_parent(root: &Path, path: &[CString], create: bool) -> Result<File> {
        let mut directory = open_root(root)?;
        for component in &path[..path.len().saturating_sub(1)] {
            if create {
                let status =
                    unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o755) };
                if status != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(error.into());
                    }
                }
            }
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            directory = file_from_fd(fd)?;
        }
        Ok(directory)
    }

    pub(super) fn open_regular_file(root: &Path, user_path: &str) -> Result<File> {
        let path = components(user_path)?;
        let parent = open_parent(root, &path, false)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                path.last().expect("nonempty path").as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        let file = file_from_fd(fd)?;
        if !file.metadata()?.is_file() {
            return Err(AppError::new("INVALID_INPUT", "path is not a regular file"));
        }
        Ok(file)
    }

    pub fn read_file_bounded(root: &Path, user_path: &str, maximum: usize) -> Result<Vec<u8>> {
        let file = open_regular_file(root, user_path)?;
        let length = file.metadata()?.len() as usize;
        if length > maximum {
            return Err(AppError::new(
                "RESOURCE_LIMIT_EXCEEDED",
                "file exceeds read limit",
            ));
        }
        let mut bytes = Vec::with_capacity(length.min(maximum));
        file.take((maximum + 1) as u64).read_to_end(&mut bytes)?;
        if bytes.len() > maximum {
            return Err(AppError::new(
                "RESOURCE_LIMIT_EXCEEDED",
                "file exceeds read limit",
            ));
        }
        Ok(bytes)
    }

    pub fn read_file_range(
        root: &Path,
        user_path: &str,
        offset: u64,
        maximum: usize,
    ) -> Result<(Vec<u8>, u64)> {
        use std::os::unix::fs::FileExt;

        let file = open_regular_file(root, user_path)?;
        let length = file.metadata()?.len();
        if offset > length {
            return Err(AppError::new("INVALID_INPUT", "offset is outside the file"));
        }
        let wanted = maximum.min(length.saturating_sub(offset) as usize);
        let mut bytes = vec![0_u8; wanted];
        let mut read = 0usize;
        while read < wanted {
            let count = file.read_at(&mut bytes[read..], offset + read as u64)?;
            if count == 0 {
                break;
            }
            read += count;
        }
        bytes.truncate(read);
        Ok((bytes, length))
    }

    pub fn write_file_atomic(root: &Path, user_path: &str, data: &[u8]) -> Result<()> {
        let path = components(user_path)?;
        let parent = open_parent(root, &path, true)?;
        let target = path.last().expect("nonempty path");
        let mut existing: libc::stat = unsafe { std::mem::zeroed() };
        let existing_status = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                target.as_ptr(),
                &mut existing,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        let target_mode = if existing_status == 0 {
            if existing.st_mode & libc::S_IFMT == libc::S_IFLNK {
                return Err(AppError::new("SYMLINK_ESCAPE", "symlink target rejected"));
            }
            existing.st_mode & 0o777
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.into());
            }
            0o600
        };
        let temporary_name = CString::new(format!(".rust-agent-{}.tmp", Uuid::now_v7()))
            .expect("UUID temp name has no NUL");
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                temporary_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        let mut temporary = file_from_fd(fd)?;
        if unsafe { libc::fchmod(temporary.as_raw_fd(), target_mode as libc::mode_t) } != 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), temporary_name.as_ptr(), 0);
            }
            return Err(error.into());
        }
        if let Err(error) = temporary
            .write_all(data)
            .and_then(|()| temporary.sync_all())
        {
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), temporary_name.as_ptr(), 0);
            }
            return Err(error.into());
        }
        let status = unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                temporary_name.as_ptr(),
                parent.as_raw_fd(),
                target.as_ptr(),
            )
        };
        if status != 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), temporary_name.as_ptr(), 0);
            }
            return Err(error.into());
        }
        parent.sync_all()?;
        Ok(())
    }

    pub fn create_directory_all(root: &Path, user_path: &str) -> Result<()> {
        let mut path = components(user_path)?;
        path.push(CString::new("sentinel").expect("static string"));
        let _ = open_parent(root, &path, true)?;
        Ok(())
    }

    pub fn move_path(root: &Path, source: &str, destination: &str) -> Result<()> {
        let source = components(source)?;
        let destination = components(destination)?;
        let source_parent = open_parent(root, &source, false)?;
        let destination_parent = open_parent(root, &destination, true)?;
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        let status = unsafe {
            libc::fstatat(
                source_parent.as_raw_fd(),
                source.last().expect("nonempty path").as_ptr(),
                &mut metadata,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if metadata.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err(AppError::new("SYMLINK_ESCAPE", "symlink source rejected"));
        }
        let status = unsafe {
            libc::renameat(
                source_parent.as_raw_fd(),
                source.last().expect("nonempty path").as_ptr(),
                destination_parent.as_raw_fd(),
                destination.last().expect("nonempty path").as_ptr(),
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn remove_entry(parent: RawFd, name: &CString) -> Result<()> {
        struct DirectoryStream(*mut libc::DIR);

        impl Drop for DirectoryStream {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }

        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                parent,
                name.as_ptr(),
                &mut metadata,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if metadata.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let directory = file_from_fd(unsafe {
                libc::openat(
                    parent,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            })?;
            // fdopendir works on Linux, macOS, and the other supported Unix
            // targets; unlike /proc/self/fd it does not require procfs to be
            // mounted (common in containers and absent on macOS).
            let directory_fd = directory.into_raw_fd();
            let stream = unsafe { libc::fdopendir(directory_fd) };
            if stream.is_null() {
                let error = std::io::Error::last_os_error();
                unsafe { libc::close(directory_fd) };
                return Err(error.into());
            }
            let stream = DirectoryStream(stream);
            let mut children = Vec::new();
            loop {
                let entry = unsafe { libc::readdir(stream.0) };
                if entry.is_null() {
                    break;
                }
                let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
                if name.to_bytes() == b"." || name.to_bytes() == b".." {
                    continue;
                }
                children.push(CString::new(name.to_bytes()).map_err(|_| {
                    AppError::new("PROCESS_FAILED", "directory entry contains NUL")
                })?);
            }
            let child_parent = unsafe { libc::dirfd(stream.0) };
            for child in children {
                remove_entry(child_parent, &child)?;
            }
            drop(stream);
            if unsafe { libc::unlinkat(parent, name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        } else if unsafe { libc::unlinkat(parent, name.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    pub fn remove_path(root: &Path, user_path: &str) -> Result<()> {
        let path = components(user_path)?;
        let parent = open_parent(root, &path, false)?;
        remove_entry(parent.as_raw_fd(), path.last().expect("nonempty path"))
    }
}

#[derive(Debug, Serialize)]
pub struct ProcessResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub truncated: bool,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

async fn bounded_read<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> std::io::Result<(Vec<u8>, usize, bool)> {
    let mut retained = Vec::with_capacity(limit.min(64 * 1024));
    let mut total = 0usize;
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        if retained.len() < limit {
            let remaining = limit - retained.len();
            retained.extend_from_slice(&buffer[..read.min(remaining)]);
        }
    }
    Ok((retained, total, total > limit))
}

#[cfg(unix)]
pub(crate) fn process_limits(command: &mut Command, timeout: Duration) {
    let cpu_seconds = timeout.as_secs().saturating_add(2).max(2);
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let cpu = libc::rlimit {
                rlim_cur: cpu_seconds,
                rlim_max: cpu_seconds.saturating_add(1),
            };
            let nofile = libc::rlimit {
                rlim_cur: 256,
                rlim_max: 256,
            };
            // RLIMIT_NPROC is counted per real UID on Linux, not per child
            // process tree. Applying a small value here can make an otherwise
            // idle project unable to fork when the daemon UID is shared with
            // platform/background processes. Process fan-out must instead be
            // bounded by the configured process concurrency and the outer
            // container/cgroup PID limit.
            for (resource, limit) in [(libc::RLIMIT_CPU, cpu), (libc::RLIMIT_NOFILE, nofile)] {
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn process_limits(_command: &mut Command, _timeout: Duration) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Posix,
    PowerShell,
    Cmd,
}

impl ShellKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Posix => "posix",
            Self::PowerShell => "powershell",
            Self::Cmd => "cmd",
        }
    }
}

fn shell_kind(shell: &str) -> ShellKind {
    let base = shell.rsplit(['/', '\\']).next().unwrap_or(shell);
    let base = if base.len() >= 4 && base[base.len() - 4..].eq_ignore_ascii_case(".exe") {
        &base[..base.len() - 4]
    } else {
        base
    };
    match base.to_ascii_lowercase().as_str() {
        "powershell" | "pwsh" => ShellKind::PowerShell,
        "cmd" => ShellKind::Cmd,
        _ => ShellKind::Posix,
    }
}

fn shell_command(
    explicit: Option<&str>,
    command_text: &str,
) -> Result<(String, Vec<String>, String)> {
    let shell = explicit
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(default_shell);
    if !valid_shell_executable(&shell) {
        return Err(AppError::new("INVALID_INPUT", "invalid shell executable"));
    }
    let kind = shell_kind(&shell);
    let shell = resolve_shell_executable(shell, kind);
    let (args, command_text) = match kind {
        ShellKind::PowerShell => {
            #[cfg(windows)]
            {
                (
                    vec![
                        "-NoLogo".to_owned(),
                        "-NoProfile".to_owned(),
                        "-InputFormat".to_owned(),
                        "Text".to_owned(),
                        "-OutputFormat".to_owned(),
                        "Text".to_owned(),
                        "-NonInteractive".to_owned(),
                        "-EncodedCommand".to_owned(),
                    ],
                    powershell_encoded_command(command_text),
                )
            }
            #[cfg(not(windows))]
            {
                (
                    vec![
                        "-NoLogo".to_owned(),
                        "-NoProfile".to_owned(),
                        "-Command".to_owned(),
                    ],
                    powershell_script(command_text),
                )
            }
        }
        ShellKind::Cmd => (
            vec!["/d".to_owned(), "/s".to_owned(), "/c".to_owned()],
            command_text.to_owned(),
        ),
        ShellKind::Posix => (vec!["-c".to_owned()], command_text.to_owned()),
    };
    Ok((shell, args, command_text))
}

fn append_shell_command_text(command: &mut Command, kind: ShellKind, command_text: &str) {
    #[cfg(windows)]
    if kind == ShellKind::Cmd {
        use std::os::windows::process::CommandExt as _;

        // `cmd.exe /s /c` parses the raw process command line rather than an argv
        // reconstructed with CommandLineToArgvW rules. Rust's ordinary `.arg()`
        // encoding quotes a script containing spaces and backslash-escapes every
        // embedded `"`, which changes commands such as `echo ok & "tool.exe"`.
        // Pass the command tail verbatim and provide the outer quotes that `/s`
        // removes, leaving the user's command text (and its quotes) intact.
        command.as_std_mut().raw_arg(format!("\"{command_text}\""));
        return;
    }

    #[cfg(not(windows))]
    let _ = kind;
    command.arg(command_text);
}

fn valid_shell_executable(shell: &str) -> bool {
    !shell.is_empty() && shell.len() <= 4096 && !shell.contains(['\0', '\n', '\r'])
}

#[cfg(any(windows, test))]
fn windows_shell_executable(shell: &str, kind: ShellKind, comspec: Option<&str>) -> String {
    if shell.contains(['/', '\\', ':']) {
        return shell.to_owned();
    }

    match kind {
        ShellKind::Cmd => comspec
            .filter(|value| is_cmd_executable(value))
            .map(str::to_owned)
            .unwrap_or_else(|| "cmd.exe".to_owned()),
        ShellKind::PowerShell
            if shell.eq_ignore_ascii_case("powershell")
                || shell.eq_ignore_ascii_case("powershell.exe") =>
        {
            "powershell.exe".to_owned()
        }
        ShellKind::PowerShell
            if shell.eq_ignore_ascii_case("pwsh") || shell.eq_ignore_ascii_case("pwsh.exe") =>
        {
            "pwsh.exe".to_owned()
        }
        _ => shell.to_owned(),
    }
}

#[cfg(any(windows, test))]
fn is_cmd_executable(shell: &str) -> bool {
    if !valid_shell_executable(shell) {
        return false;
    }
    shell
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|base| base.eq_ignore_ascii_case("cmd.exe"))
}

fn resolve_shell_executable(shell: String, kind: ShellKind) -> String {
    #[cfg(windows)]
    {
        let comspec = std::env::var("ComSpec").ok();
        windows_shell_executable(&shell, kind, comspec.as_deref())
    }
    #[cfg(not(windows))]
    {
        let _ = kind;
        shell
    }
}

fn default_shell() -> String {
    if cfg!(windows) {
        "powershell.exe".to_owned()
    } else {
        std::env::var("SHELL")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_owned())
    }
}

fn powershell_script(command_text: &str) -> String {
    // PowerShell process status is based on the final command, but a non-zero
    // native exit code is otherwise collapsed to 1. Keep the user command in the
    // top-level command scope, inspect `$?` immediately after it, and only use
    // `$LASTEXITCODE` when that final command failed. Initializing `$LASTEXITCODE`
    // avoids reusing a value inherited from an earlier PowerShell session state.
    format!(
        "$global:LASTEXITCODE = $null\n{command_text}\n$codexbridge_success = $?\n$codexbridge_exit_code = $LASTEXITCODE\nif ($codexbridge_success) {{ exit 0 }}\nif ($null -ne $codexbridge_exit_code) {{ exit $codexbridge_exit_code }}\nexit 1"
    )
}

#[cfg(windows)]
fn powershell_encoded_command(command_text: &str) -> String {
    let script = powershell_script(command_text);
    let mut bytes = Vec::with_capacity(script.len().saturating_mul(2));
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    STANDARD.encode(bytes)
}

fn sanitized_base_environment(command: &mut Command, use_bwrap: bool) {
    if cfg!(windows) {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        let path = std::env::var("PATH").unwrap_or_else(|_| {
            format!(r"{system_root}\System32;{system_root};{system_root}\System32\WindowsPowerShell\v1.0")
        });
        let temporary = std::env::temp_dir();
        command.env("PATH", path);
        command.env("SystemRoot", &system_root);
        command.env("WINDIR", &system_root);
        let comspec = std::env::var("ComSpec")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!(r"{system_root}\System32\cmd.exe"));
        command.env("ComSpec", comspec);
        // `env_clear()` must not erase the Windows process contract that
        // PowerShell itself relies on. In particular, PATHEXT determines whether
        // `.exe`/`.cmd` files are launched as synchronous native commands. Without
        // it, PowerShell can route a batch file through a file association/new
        // console instead, disconnecting the child from our pipes and exit status.
        // The user/profile paths are also used to initialize `$HOME`, PSModulePath,
        // and other Windows PowerShell 5.1 startup state. Keep this allowlist narrow
        // rather than inheriting the complete (potentially secret-bearing) parent
        // environment.
        let pathext = std::env::var_os("PATHEXT")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
        command.env("PATHEXT", pathext);
        for name in [
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "LOCALAPPDATA",
            "APPDATA",
            "ProgramData",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "SystemDrive",
        ] {
            if let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) {
                command.env(name, value);
            }
        }
        command.env("TEMP", &temporary);
        command.env("TMP", &temporary);
    } else {
        let path = if use_bwrap {
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned()
        } else {
            std::env::var("PATH")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned()
                })
        };
        command.env("PATH", path);
        command.env("HOME", "/tmp");
        command.env("TMPDIR", "/tmp");
        command.env("LANG", "C.UTF-8");
    }
}

static BWRAP_USABLE: OnceLock<bool> = OnceLock::new();
static PODMAN_IN_BWRAP_USABLE: OnceLock<bool> = OnceLock::new();
static PODMAN_INVOCATION: OnceLock<PodmanInvocation> = OnceLock::new();

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub(crate) enum PodmanInvocation {
    Direct,
    DirectWithSudoFallback,
    Sudo,
    Unavailable,
}

impl PodmanInvocation {
    pub(crate) fn agent_advice(self) -> &'static str {
        match self {
            Self::Direct => {
                "Linux Podman is available directly; invoke it explicitly as `podman ...`. No passwordless sudo fallback was detected, so do not invent `sudo podman` if a direct command fails. For interactive `podman run -it`, use exec_command with tty=true."
            }
            Self::DirectWithSudoFallback => {
                "Linux Podman is available directly and a passwordless rootful fallback is verified. Start with `podman ...`; if that direct Podman operation fails with a rootless runtime error such as `crun`, mount/`/proc`, `Operation not permitted`, permission, or user-namespace failure, retry the same Podman operation once as `sudo -n podman ...` before changing flags, images, or sandbox settings. Explicit `sudo -n podman ...` runs outside Bubblewrap. For interactive `podman run -it`, use exec_command with tty=true."
            }
            Self::Sudo => {
                "Linux Podman requires rootful execution in this runtime; invoke it explicitly as `sudo -n podman ...` and do not rely on shell aliases. Explicit `sudo -n podman ...` runs outside Bubblewrap. For interactive `podman run -it`, use exec_command with tty=true."
            }
            Self::Unavailable => {
                "Podman is only probed and enabled by CodexBridge on Linux, and no usable Linux Podman invocation was detected for this runtime; do not assume `podman` or `sudo podman` is available."
            }
        }
    }
}

fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and does not mutate process state.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn command_probe(program: &str, args: &[&str]) -> bool {
    let child = match StdCommand::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    wait_probe(child, Duration::from_secs(3))
}

fn select_podman_invocation(
    is_linux: bool,
    running_as_root: bool,
    direct_usable: bool,
    sudo_usable: bool,
) -> PodmanInvocation {
    if !is_linux {
        PodmanInvocation::Unavailable
    } else if direct_usable && !running_as_root && sudo_usable {
        PodmanInvocation::DirectWithSudoFallback
    } else if direct_usable {
        PodmanInvocation::Direct
    } else if !running_as_root && sudo_usable {
        PodmanInvocation::Sudo
    } else {
        PodmanInvocation::Unavailable
    }
}

fn probe_direct_podman_runtime(running_as_root: bool) -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    if running_as_root {
        return command_probe("podman", &["info", "--format", "json"]);
    }

    // `podman info` can succeed in a restricted nested rootless runtime even
    // when crun cannot mount /proc for an actual container. Keep this probe
    // image-free while exercising the user/mount/PID namespace shape needed
    // by rootless container startup.
    command_probe(
        "/bin/sh",
        &[
            "-c",
            "rootless=$(podman info --format '{{.Host.Security.Rootless}}' 2>/dev/null) || exit 1; if [ \"$rootless\" = false ]; then exit 0; fi; [ \"$rootless\" = true ] || exit 1; command -v unshare >/dev/null 2>&1 || exit 0; exec podman unshare unshare --mount --pid --fork --mount-proc true",
        ],
    )
}

fn probe_podman_invocation() -> PodmanInvocation {
    if !cfg!(target_os = "linux") {
        return PodmanInvocation::Unavailable;
    }
    let root = running_as_root();
    let direct_usable = probe_direct_podman_runtime(root);
    let sudo_usable = !root && command_probe("sudo", &["-n", "podman", "info", "--format", "json"]);
    select_podman_invocation(true, root, direct_usable, sudo_usable)
}

pub(crate) fn podman_invocation() -> PodmanInvocation {
    *PODMAN_INVOCATION.get_or_init(probe_podman_invocation)
}

fn probe_bwrap() -> bool {
    if !cfg!(target_os = "linux") || !Path::new("/usr/bin/bwrap").is_file() {
        return false;
    }
    StdCommand::new("/usr/bin/bwrap")
        .args([
            "--unshare-all",
            "--share-net",
            "--die-with-parent",
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "/bin/true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub(crate) fn bwrap_usable() -> bool {
    *BWRAP_USABLE.get_or_init(probe_bwrap)
}

pub(crate) fn invokes_podman(command: &str) -> bool {
    command
        .split(|character: char| "|&;()<>".contains(character))
        .any(|segment| segment_podman_invocation(segment).is_some())
}

pub(crate) fn invokes_sudo_podman(command: &str) -> bool {
    command
        .split(|character: char| "|&;()<>".contains(character))
        .any(|segment| segment_podman_invocation(segment) == Some(true))
}

pub(crate) fn invokes_direct_podman(command: &str) -> bool {
    command
        .split(|character: char| "|&;()<>".contains(character))
        .any(|segment| segment_podman_invocation(segment) == Some(false))
}

fn segment_podman_invocation(segment: &str) -> Option<bool> {
    let tokens = segment
        .split_whitespace()
        .map(|token| token.trim_matches(['\'', '"']))
        .filter(|token| !token.is_empty());
    let mut saw_sudo = false;
    for token in tokens {
        if token.contains('=') && !token.starts_with('=') {
            continue;
        }
        let base = token.rsplit(['/', '\\']).next().unwrap_or(token);
        if base == "sudo" {
            saw_sudo = true;
            continue;
        }
        if saw_sudo && matches!(base, "-n" | "--non-interactive") {
            continue;
        }
        if matches!(base, "env" | "command" | "exec" | "nohup" | "rtk") {
            continue;
        }
        return (base == "podman").then_some(saw_sudo);
    }
    None
}

fn wait_probe(mut child: std::process::Child, timeout: Duration) -> bool {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn probe_podman_in_bwrap(config: &Config, project: &ProjectContext) -> bool {
    if !bwrap_usable() {
        return false;
    }
    let mut command = match bubblewrap_base_std_command(config, project, &project.project_root) {
        Ok(command) => command,
        Err(_) => return false,
    };
    command
        .arg("podman")
        .args(["info", "--format", "json"])
        .env_clear();
    sanitized_base_std_environment(&mut command, true);
    if let Some(socket) = &config.container_socket {
        command
            .env("CONTAINER_HOST", "unix:///run/podman.sock")
            .env("DOCKER_HOST", "unix:///run/podman.sock");
        if !socket.exists() {
            return false;
        }
    }
    let child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    wait_probe(child, Duration::from_secs(3))
}

fn podman_usable_inside_bwrap(config: &Config, project: &ProjectContext) -> bool {
    *PODMAN_IN_BWRAP_USABLE.get_or_init(|| probe_podman_in_bwrap(config, project))
}

fn should_use_bwrap(
    sandbox_backend: &str,
    bwrap_ok: bool,
    podman_command: bool,
    sudo_podman_command: bool,
    podman_in_bwrap_ok: bool,
) -> bool {
    matches!(sandbox_backend, "auto" | "bwrap")
        && bwrap_ok
        && !sudo_podman_command
        && (!podman_command || podman_in_bwrap_ok)
}

fn podman_can_use_bwrap(invocation: PodmanInvocation, probe_ok: bool) -> bool {
    matches!(
        invocation,
        PodmanInvocation::Direct | PodmanInvocation::DirectWithSudoFallback
    ) && probe_ok
}

fn use_bwrap(config: &Config, project: &ProjectContext, command: Option<&str>) -> bool {
    if !matches!(config.sandbox_backend.as_str(), "auto" | "bwrap") {
        return false;
    }
    let bwrap_ok = bwrap_usable();
    if !bwrap_ok {
        return false;
    }
    let podman_command = command.is_some_and(invokes_podman);
    let sudo_podman_command = command.is_some_and(invokes_sudo_podman);
    let podman_in_bwrap_ok = !podman_command
        || podman_can_use_bwrap(
            podman_invocation(),
            podman_usable_inside_bwrap(config, project),
        );
    should_use_bwrap(
        &config.sandbox_backend,
        bwrap_ok,
        podman_command,
        sudo_podman_command,
        podman_in_bwrap_ok,
    )
}

pub(crate) fn effective_default_sandbox_backend(config: &Config) -> &'static str {
    if matches!(config.sandbox_backend.as_str(), "auto" | "bwrap") && bwrap_usable() {
        "bubblewrap"
    } else {
        "native"
    }
}

pub(crate) fn default_exec_shell(config: &Config) -> (String, &'static str, Vec<String>) {
    let shell = if matches!(config.sandbox_backend.as_str(), "auto" | "bwrap") && bwrap_usable() {
        "/bin/sh".to_owned()
    } else {
        default_shell()
    };
    let kind = shell_kind(&shell);
    let args = match kind {
        ShellKind::PowerShell => {
            #[cfg(windows)]
            {
                vec![
                    "-NoLogo".to_owned(),
                    "-NoProfile".to_owned(),
                    "-NonInteractive".to_owned(),
                    "-EncodedCommand".to_owned(),
                ]
            }
            #[cfg(not(windows))]
            {
                vec![
                    "-NoLogo".to_owned(),
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                ]
            }
        }
        ShellKind::Cmd => vec!["/d".to_owned(), "/s".to_owned(), "/c".to_owned()],
        ShellKind::Posix => vec!["-c".to_owned()],
    };
    (shell, kind.as_str(), args)
}

fn bubblewrap_base_command(
    config: &Config,
    project: &ProjectContext,
    workdir: &Path,
    runtime_bind: Option<(&Path, &Path)>,
) -> Result<Command> {
    let mut command = Command::new("/usr/bin/bwrap");
    command.args([
        "--unshare-all",
        "--share-net",
        "--die-with-parent",
        "--new-session",
    ]);
    for path in [
        "/usr",
        "/bin",
        "/lib",
        "/lib64",
        "/usr/local",
        "/etc/ssl",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/passwd",
        "/etc/group",
    ] {
        if Path::new(path).exists() {
            command.args(["--ro-bind", path, path]);
        }
    }
    command.args([
        "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--bind",
    ]);
    command.arg(&project.project_root);
    let relative_workdir = workdir
        .strip_prefix(&project.project_root)
        .map_err(|_| AppError::new("PATH_OUTSIDE_WORKSPACE", "process workdir escaped project"))?;
    let sandbox_workdir = if relative_workdir.as_os_str().is_empty() {
        "/workspace".to_owned()
    } else {
        format!(
            "/workspace/{}",
            relative_workdir.to_string_lossy().replace('\\', "/")
        )
    };
    command.args(["/workspace", "--chdir", &sandbox_workdir, "--dir", "/run"]);
    if let Some(socket) = &config.container_socket {
        let socket = socket.canonicalize().map_err(|error| {
            AppError::new(
                "SANDBOX_UNAVAILABLE",
                format!("configured container socket is unavailable: {error}"),
            )
        })?;
        command.args([
            "--bind",
            socket.to_string_lossy().as_ref(),
            "/run/podman.sock",
        ]);
    }
    if let Some(root) = &config.container_config_root {
        let root = root.canonicalize().map_err(|error| {
            AppError::new(
                "SANDBOX_UNAVAILABLE",
                format!("configured container config root is unavailable: {error}"),
            )
        })?;
        command.args([
            "--ro-bind",
            root.to_string_lossy().as_ref(),
            "/etc/containers",
        ]);
    }
    if let Some((host, sandbox)) = runtime_bind {
        let host = host.canonicalize().map_err(|error| {
            AppError::new(
                "SANDBOX_UNAVAILABLE",
                format!("runtime bind source is unavailable: {error}"),
            )
        })?;
        command.arg("--bind").arg(host).arg(sandbox);
    }
    Ok(command)
}

fn bubblewrap_base_std_command(
    config: &Config,
    project: &ProjectContext,
    workdir: &Path,
) -> Result<StdCommand> {
    let mut command = StdCommand::new("/usr/bin/bwrap");
    command.args([
        "--unshare-all",
        "--share-net",
        "--die-with-parent",
        "--new-session",
    ]);
    for path in [
        "/usr",
        "/bin",
        "/lib",
        "/lib64",
        "/usr/local",
        "/etc/ssl",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/passwd",
        "/etc/group",
    ] {
        if Path::new(path).exists() {
            command.args(["--ro-bind", path, path]);
        }
    }
    command.args([
        "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--bind",
    ]);
    command.arg(&project.project_root);
    let relative_workdir = workdir
        .strip_prefix(&project.project_root)
        .map_err(|_| AppError::new("PATH_OUTSIDE_WORKSPACE", "process workdir escaped project"))?;
    let sandbox_workdir = if relative_workdir.as_os_str().is_empty() {
        "/workspace".to_owned()
    } else {
        format!(
            "/workspace/{}",
            relative_workdir.to_string_lossy().replace('\\', "/")
        )
    };
    command.args(["/workspace", "--chdir", &sandbox_workdir, "--dir", "/run"]);
    if let Some(socket) = &config.container_socket {
        let socket = socket.canonicalize().map_err(|error| {
            AppError::new(
                "SANDBOX_UNAVAILABLE",
                format!("configured container socket is unavailable: {error}"),
            )
        })?;
        command.args([
            "--bind",
            socket.to_string_lossy().as_ref(),
            "/run/podman.sock",
        ]);
    }
    if let Some(root) = &config.container_config_root {
        let root = root.canonicalize().map_err(|error| {
            AppError::new(
                "SANDBOX_UNAVAILABLE",
                format!("configured container config root is unavailable: {error}"),
            )
        })?;
        command.args([
            "--ro-bind",
            root.to_string_lossy().as_ref(),
            "/etc/containers",
        ]);
    }
    Ok(command)
}

fn sanitized_base_std_environment(command: &mut StdCommand, use_bwrap: bool) {
    if cfg!(windows) {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        let path = std::env::var("PATH").unwrap_or_else(|_| {
            format!(r"{system_root}\System32;{system_root};{system_root}\System32\WindowsPowerShell\v1.0")
        });
        command.env("PATH", path);
        command.env("SystemRoot", &system_root);
        command.env("WINDIR", &system_root);
    } else {
        let path = if use_bwrap {
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned()
        } else {
            std::env::var("PATH")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned()
                })
        };
        command.env("PATH", path);
        command.env("HOME", "/tmp");
        command.env("TMPDIR", "/tmp");
        command.env("LANG", "C.UTF-8");
    }
}

fn finalize_process_command(
    mut command: Command,
    config: &Config,
    use_bwrap: bool,
    interactive: bool,
    timeout: Duration,
    environment: &BTreeMap<String, String>,
) -> Result<Command> {
    command.env_clear();
    sanitized_base_environment(&mut command, use_bwrap);
    if let Some(socket) = &config.container_socket {
        let uri = if use_bwrap {
            "unix:///run/podman.sock".to_owned()
        } else {
            format!("unix://{}", socket.to_string_lossy())
        };
        command.env("CONTAINER_HOST", &uri);
        command.env("DOCKER_HOST", &uri);
    }
    for (key, value) in environment {
        if key.len() > 128
            || value.len() > 8192
            || key.contains('=')
            || key.contains('\0')
            || value.contains('\0')
        {
            return Err(AppError::new(
                "INVALID_INPUT",
                "invalid environment addition",
            ));
        }
        command.env(key, value);
    }
    process_limits(&mut command, timeout);
    command
        .kill_on_drop(true)
        .stdin(if interactive {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

pub(crate) fn build_command(
    config: &Config,
    project: &ProjectContext,
    command_text: &str,
    interactive: bool,
    timeout: Duration,
    environment: &BTreeMap<String, String>,
) -> Result<Command> {
    build_command_with_options(
        config,
        project,
        command_text,
        interactive,
        timeout,
        environment,
        &project.project_root,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_command_with_options(
    config: &Config,
    project: &ProjectContext,
    command_text: &str,
    interactive: bool,
    timeout: Duration,
    environment: &BTreeMap<String, String>,
    workdir: &Path,
    shell: Option<&str>,
) -> Result<Command> {
    build_command_with_options_and_runtime_bind(
        config,
        project,
        command_text,
        interactive,
        timeout,
        environment,
        workdir,
        shell,
        None,
    )
    .map(|(command, _)| command)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_command_with_options_and_runtime_bind(
    config: &Config,
    project: &ProjectContext,
    command_text: &str,
    interactive: bool,
    timeout: Duration,
    environment: &BTreeMap<String, String>,
    workdir: &Path,
    shell: Option<&str>,
    runtime_bind: Option<(&Path, &Path)>,
) -> Result<(Command, bool)> {
    let use_bwrap = use_bwrap(config, project, Some(command_text));
    let sandbox_default_shell = (use_bwrap && shell.is_none()).then_some("/bin/sh");
    let (shell_bin, shell_args, shell_text) =
        shell_command(shell.or(sandbox_default_shell), command_text)?;
    let kind = shell_kind(&shell_bin);
    let command = if use_bwrap {
        let mut command = bubblewrap_base_command(config, project, workdir, runtime_bind)?;
        command.arg(&shell_bin).args(&shell_args).arg(&shell_text);
        command
    } else if config.allow_unsandboxed_exec {
        let mut command = Command::new(&shell_bin);
        command.args(&shell_args);
        append_shell_command_text(&mut command, kind, &shell_text);
        command.current_dir(workdir);
        command
    } else {
        return Err(AppError::new(
            "SANDBOX_UNAVAILABLE",
            "no usable exec backend; Bubblewrap is unavailable and native execution is disabled",
        ));
    };
    let command = finalize_process_command(
        command,
        config,
        use_bwrap,
        interactive,
        timeout,
        environment,
    )?;
    Ok((command, use_bwrap))
}

pub(crate) fn build_argv_command(
    config: &Config,
    project: &ProjectContext,
    executable: &str,
    arguments: &[String],
    timeout: Duration,
    environment: &BTreeMap<String, String>,
) -> Result<Command> {
    if executable.is_empty()
        || executable.len() > 4096
        || executable.contains(['\0', '\n', '\r'])
        || arguments
            .iter()
            .any(|argument| argument.contains('\0') || argument.contains(['\n', '\r']))
    {
        return Err(AppError::new(
            "INVALID_INPUT",
            "invalid executable or argv value",
        ));
    }
    let encoded_len = executable
        .len()
        .saturating_add(arguments.iter().map(String::len).sum::<usize>());
    if encoded_len > config.limits.input_string_bytes {
        return Err(AppError::new(
            "INPUT_TOO_LARGE",
            "executable and arguments exceed MAX_INPUT_STRING_BYTES",
        ));
    }
    let use_bwrap = use_bwrap(config, project, Some(executable));
    let command = if use_bwrap {
        let mut command = bubblewrap_base_command(config, project, &project.project_root, None)?;
        command.arg(executable).args(arguments);
        command
    } else if config.allow_unsandboxed_exec {
        let mut command = Command::new(executable);
        command.args(arguments).current_dir(&project.project_root);
        command
    } else {
        return Err(AppError::new(
            "SANDBOX_UNAVAILABLE",
            "no usable exec backend; Bubblewrap is unavailable and native execution is disabled",
        ));
    };
    finalize_process_command(command, config, use_bwrap, false, timeout, environment)
}

async fn execute_prepared(
    mut command: Command,
    timeout: Duration,
    output_limit: usize,
) -> Result<ProcessResult> {
    #[cfg(all(test, windows))]
    if std::env::var_os("CODEXBRIDGE_WINDOWS_PROCESS_DIAGNOSTICS").is_some() {
        eprintln!("codexbridge-windows-sandbox spawn-command={command:?}");
    }
    let mut child = command
        .spawn()
        .map_err(|error| AppError::new("SANDBOX_UNAVAILABLE", error.to_string()))?;
    let process_id = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::new("PROCESS_FAILED", "stdout pipe unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::new("PROCESS_FAILED", "stderr pipe unavailable"))?;
    let stdout_task = tokio::spawn(bounded_read(stdout, output_limit));
    let stderr_task = tokio::spawn(bounded_read(stderr, output_limit));
    let wait = tokio::time::timeout(timeout, child.wait()).await;
    let (status, timed_out) = match wait {
        Ok(status) => (Some(status?), false),
        Err(_) => {
            #[cfg(unix)]
            if let Some(process_id) = process_id {
                unsafe {
                    libc::kill(-(process_id as i32), libc::SIGKILL);
                }
            }
            #[cfg(windows)]
            if let Some(process_id) = process_id {
                let _ = crate::platform::windows_taskkill(process_id, true);
            }
            let _ = child.kill().await;
            let status = child.wait().await.ok();
            (status, true)
        }
    };
    let (stdout, stdout_bytes, stdout_truncated) = stdout_task
        .await
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))??;
    let (stderr, stderr_bytes, stderr_truncated) = stderr_task
        .await
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))??;
    Ok(ProcessResult {
        exit_code: status.and_then(|value| value.code()),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
        stdout_bytes,
        stderr_bytes,
    })
}

pub async fn execute(
    config: &Config,
    project: &ProjectContext,
    command_text: &str,
    timeout: Duration,
    environment: &BTreeMap<String, String>,
) -> Result<ProcessResult> {
    if command_text.is_empty() || command_text.len() > config.limits.input_string_bytes {
        return Err(AppError::new(
            "INPUT_TOO_LARGE",
            "command is empty or exceeds MAX_INPUT_STRING_BYTES",
        ));
    }
    let command = build_command(config, project, command_text, false, timeout, environment)?;
    execute_prepared(command, timeout, config.limits.process_output_bytes).await
}

pub async fn execute_argv(
    config: &Config,
    project: &ProjectContext,
    executable: &str,
    arguments: &[String],
    timeout: Duration,
    environment: &BTreeMap<String, String>,
) -> Result<ProcessResult> {
    let command = build_argv_command(config, project, executable, arguments, timeout, environment)?;
    execute_prepared(command, timeout, config.limits.process_output_bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigBuilder;
    use crate::project::ProjectKey;

    #[cfg(target_os = "linux")]
    #[test]
    fn regression_bwrap_probe_and_runtime_must_use_the_same_executable() {
        use std::ffi::OsStr;

        let project_dir = tempfile::tempdir().unwrap();
        let config = ConfigBuilder::from_map(std::collections::BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_EXEC_SANDBOX".to_owned(), "bwrap".to_owned()),
        ]))
        .build()
        .unwrap();
        let project = ProjectContext {
            native_project_key: ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root: project_dir.path().to_path_buf(),
            metadata_root: project_dir.path().join(".metadata"),
            transport_mode: crate::request_context::TransportMode::Stateless,
            mcp_session_present: false,
        };

        // probe_bwrap() validates /usr/bin/bwrap. The runtime command must use
        // that exact executable rather than re-resolving "bwrap" through PATH.
        let command = bubblewrap_base_std_command(&config, &project, project_dir.path()).unwrap();
        assert_eq!(command.get_program(), OsStr::new("/usr/bin/bwrap"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn yolo_bwrap_keeps_host_network_available() {
        let project_dir = tempfile::tempdir().unwrap();
        let config = ConfigBuilder::from_map(std::collections::BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_EXEC_SANDBOX".to_owned(), "bwrap".to_owned()),
        ]))
        .build()
        .unwrap();
        let project = ProjectContext {
            native_project_key: ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root: project_dir.path().to_path_buf(),
            metadata_root: project_dir.path().join(".metadata"),
            transport_mode: crate::request_context::TransportMode::Stateless,
            mcp_session_present: false,
        };
        let command = bubblewrap_base_std_command(&config, &project, project_dir.path()).unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.iter().any(|arg| arg == "--share-net"),
            "YOLO mode must keep network access available: {args:?}"
        );
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        let directory = tempfile::tempdir().unwrap();
        let resolver = SecurePathResolver;
        for path in [
            "../secret",
            "../../etc/passwd",
            "/foo",
            r"C:\Windows",
            r"\\server\share",
        ] {
            assert!(
                resolver
                    .resolve_project_path(directory.path(), path, PathOperation::Create)
                    .is_err(),
                "{path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape_for_existing_and_create() {
        use std::os::unix::fs::symlink;
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "secret").unwrap();
        symlink(outside.path(), project.path().join("link")).unwrap();
        let resolver = SecurePathResolver;
        assert!(matches!(
            resolver.resolve_project_path(project.path(), "link/secret", PathOperation::Existing),
            Err(AppError::Structured {
                code: "SYMLINK_ESCAPE",
                ..
            })
        ));
        assert!(matches!(
            resolver.resolve_project_path(project.path(), "link/new", PathOperation::Create),
            Err(AppError::Structured {
                code: "SYMLINK_ESCAPE",
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn capability_io_round_trip_move_copy_and_recursive_remove() {
        let project = tempfile::tempdir().unwrap();
        let resolver = SecurePathResolver;
        resolver
            .write_file_atomic(project.path(), "src/nested/file.txt", b"hello")
            .unwrap();
        assert_eq!(
            resolver
                .read_file_bounded(project.path(), "src/nested/file.txt", 100)
                .unwrap(),
            b"hello"
        );
        resolver
            .copy_file_secure(project.path(), "src/nested/file.txt", "copy.txt", 100)
            .unwrap();
        resolver
            .move_path_secure(project.path(), "copy.txt", "moved/output.txt")
            .unwrap();
        assert_eq!(
            resolver
                .read_file_bounded(project.path(), "moved/output.txt", 100)
                .unwrap(),
            b"hello"
        );
        resolver.remove_path_secure(project.path(), "src").unwrap();
        assert!(!project.path().join("src").exists());
    }

    #[test]
    fn ranged_read_does_not_require_the_whole_file_to_fit() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("large.txt"), b"0123456789abcdef").unwrap();
        let resolver = SecurePathResolver;
        let (first, total) = resolver
            .read_file_range(project.path(), "large.txt", 0, 4)
            .unwrap();
        let (middle, middle_total) = resolver
            .read_file_range(project.path(), "large.txt", 8, 4)
            .unwrap();
        assert_eq!(first, b"0123");
        assert_eq!(middle, b"89ab");
        assert_eq!(total, 16);
        assert_eq!(middle_total, total);
        assert!(
            resolver
                .read_file_range(project.path(), "large.txt", 17, 4)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn capability_io_never_follows_symlinked_parent() {
        use std::os::unix::fs::symlink;
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        symlink(outside.path(), project.path().join("link")).unwrap();
        let resolver = SecurePathResolver;

        assert_eq!(
            resolver
                .write_file_atomic(project.path(), "link/escaped.txt", b"no")
                .unwrap_err()
                .code(),
            "SYMLINK_ESCAPE"
        );
        assert_eq!(
            resolver
                .read_file_bounded(project.path(), "link/secret.txt", 100)
                .unwrap_err()
                .code(),
            "SYMLINK_ESCAPE"
        );
        assert_eq!(
            resolver
                .read_file_range(project.path(), "link/secret.txt", 0, 100)
                .unwrap_err()
                .code(),
            "SYMLINK_ESCAPE"
        );

        std::fs::write(project.path().join("source.txt"), b"source").unwrap();
        assert_eq!(
            resolver
                .copy_file_secure(project.path(), "link/secret.txt", "copy-from-link.txt", 100)
                .unwrap_err()
                .code(),
            "SYMLINK_ESCAPE"
        );
        assert_eq!(
            resolver
                .copy_file_secure(project.path(), "source.txt", "link/copied.txt", 100)
                .unwrap_err()
                .code(),
            "SYMLINK_ESCAPE"
        );
        assert_eq!(
            resolver
                .move_path_secure(project.path(), "link/secret.txt", "moved-from-link.txt")
                .unwrap_err()
                .code(),
            "SYMLINK_ESCAPE"
        );
        assert_eq!(
            resolver
                .move_path_secure(project.path(), "source.txt", "link/moved.txt")
                .unwrap_err()
                .code(),
            "SYMLINK_ESCAPE"
        );
        assert!(project.path().join("source.txt").is_file());
        assert_eq!(
            resolver
                .create_directory_all(project.path(), "link/new/directory")
                .unwrap_err()
                .code(),
            "SYMLINK_ESCAPE"
        );
        assert_eq!(
            resolver
                .remove_path_secure(project.path(), "link/secret.txt")
                .unwrap_err()
                .code(),
            "SYMLINK_ESCAPE"
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

    #[test]
    fn capability_apis_reject_cross_platform_absolute_and_traversal_forms() {
        let project = tempfile::tempdir().unwrap();
        let resolver = SecurePathResolver;
        std::fs::write(project.path().join("source.txt"), b"source").unwrap();

        for path in ["../x", "/etc/passwd", r"C:\Windows\x", r"\\host\share"] {
            let assert_rejected = |operation: &str, error: AppError| {
                assert_eq!(
                    error.code(),
                    "PATH_OUTSIDE_WORKSPACE",
                    "{operation} accepted {path}"
                );
            };

            assert_rejected(
                "bounded read",
                resolver
                    .read_file_bounded(project.path(), path, 100)
                    .unwrap_err(),
            );
            assert_rejected(
                "ranged read",
                resolver
                    .read_file_range(project.path(), path, 0, 100)
                    .unwrap_err(),
            );
            assert_rejected(
                "write",
                resolver
                    .write_file_atomic(project.path(), path, b"no")
                    .unwrap_err(),
            );
            assert_rejected(
                "copy source",
                resolver
                    .copy_file_secure(project.path(), path, "copy.txt", 100)
                    .unwrap_err(),
            );
            assert_rejected(
                "copy destination",
                resolver
                    .copy_file_secure(project.path(), "source.txt", path, 100)
                    .unwrap_err(),
            );
            assert_rejected(
                "move source",
                resolver
                    .move_path_secure(project.path(), path, "moved.txt")
                    .unwrap_err(),
            );
            assert_rejected(
                "move destination",
                resolver
                    .move_path_secure(project.path(), "source.txt", path)
                    .unwrap_err(),
            );
            assert_rejected(
                "remove",
                resolver
                    .remove_path_secure(project.path(), path)
                    .unwrap_err(),
            );
            assert_rejected(
                "directory creation",
                resolver
                    .create_directory_all(project.path(), path)
                    .unwrap_err(),
            );

            assert!(project.path().join("source.txt").is_file(), "{path}");
            assert!(!project.path().join("copy.txt").exists(), "{path}");
            assert!(!project.path().join("moved.txt").exists(), "{path}");
        }
    }

    #[test]
    fn lexical_resolver_accepts_dot_and_normal_nested_paths() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("src/nested")).unwrap();
        std::fs::write(project.path().join("src/nested/file.txt"), "ok").unwrap();
        let resolver = SecurePathResolver;
        assert_eq!(
            resolver
                .resolve_project_path(project.path(), ".", PathOperation::Existing)
                .unwrap(),
            project.path().canonicalize().unwrap()
        );
        assert!(
            resolver
                .resolve_project_path(
                    project.path(),
                    "./src/nested/file.txt",
                    PathOperation::Existing
                )
                .unwrap()
                .ends_with("src/nested/file.txt")
        );
    }

    #[test]
    fn lexical_resolver_rejects_empty_null_backslash_and_oversized_paths() {
        let project = tempfile::tempdir().unwrap();
        let resolver = SecurePathResolver;
        for path in ["", "a\0b", r"a\b"] {
            assert_eq!(
                resolver
                    .resolve_project_path(project.path(), path, PathOperation::Create)
                    .unwrap_err()
                    .code(),
                "PATH_OUTSIDE_WORKSPACE",
                "{path:?}"
            );
        }
        let oversized = "a".repeat(4097);
        assert_eq!(
            resolver
                .resolve_project_path(project.path(), &oversized, PathOperation::Create)
                .unwrap_err()
                .code(),
            "PATH_OUTSIDE_WORKSPACE"
        );
    }

    #[test]
    fn create_resolution_allows_missing_tail_but_existing_resolution_does_not() {
        let project = tempfile::tempdir().unwrap();
        let resolver = SecurePathResolver;
        let create = resolver
            .resolve_project_path(project.path(), "new/deep/file.txt", PathOperation::Create)
            .unwrap();
        assert!(create.ends_with("new/deep/file.txt"));
        assert_eq!(
            resolver
                .resolve_project_path(project.path(), "new/deep/file.txt", PathOperation::Existing)
                .unwrap_err()
                .code(),
            "FILE_NOT_FOUND"
        );
    }

    #[test]
    fn bounded_read_accepts_exact_limit_and_rejects_one_byte_over() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("data.bin"), b"12345").unwrap();
        let resolver = SecurePathResolver;
        assert_eq!(
            resolver
                .read_file_bounded(project.path(), "data.bin", 5)
                .unwrap(),
            b"12345"
        );
        assert_eq!(
            resolver
                .read_file_bounded(project.path(), "data.bin", 4)
                .unwrap_err()
                .code(),
            "RESOURCE_LIMIT_EXCEEDED"
        );
    }

    #[cfg(unix)]
    #[test]
    fn regression_bounded_read_rejects_fifo_without_waiting_for_peer() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::sync::mpsc;
        use std::time::Duration;

        let project = tempfile::tempdir().unwrap();
        let fifo = project.path().join("pipe");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);

        // There is intentionally no peer writer. Production opens with O_NONBLOCK
        // and rejects non-regular metadata before reading, so completion must not
        // depend on a FIFO peer. The timeout is only an outer deadlock guard.
        let project_path = project.path().to_path_buf();
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let result = SecurePathResolver.read_file_bounded(&project_path, "pipe", 16);
            let _ = sender.send(result);
        });

        let result = match receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("FIFO read blocked waiting for a peer")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                reader.join().expect("FIFO reader thread panicked");
                panic!("FIFO reader exited without reporting a result")
            }
        };
        reader.join().expect("FIFO reader thread panicked");

        let error = result.expect_err("FIFO must be rejected as non-regular");
        assert_eq!(error.code(), "INVALID_INPUT");
        assert_eq!(error.message(), "path is not a regular file");
    }

    #[test]
    fn ranged_read_supports_eof_and_zero_length_windows() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("data.bin"), b"12345").unwrap();
        let resolver = SecurePathResolver;
        let (empty, total) = resolver
            .read_file_range(project.path(), "data.bin", 5, 10)
            .unwrap();
        assert!(empty.is_empty());
        assert_eq!(total, 5);
        let (zero, total) = resolver
            .read_file_range(project.path(), "data.bin", 2, 0)
            .unwrap();
        assert!(zero.is_empty());
        assert_eq!(total, 5);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_overwrite_preserves_executable_mode() {
        use std::os::unix::fs::PermissionsExt;

        let project = tempfile::tempdir().unwrap();
        let path = project.path().join("script.sh");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        SecurePathResolver
            .write_file_atomic(project.path(), "script.sh", b"new")
            .unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }

    #[test]
    fn powershell_script_emits_native_exit_code_propagation_logic() {
        let (shell, args, command_payload) = shell_command(Some("pwsh"), "native-command").unwrap();
        if cfg!(windows) {
            assert_eq!(shell, "pwsh.exe");
        } else {
            assert_eq!(shell, "pwsh");
        }
        #[cfg(windows)]
        let script = {
            assert_eq!(args.last().map(String::as_str), Some("-EncodedCommand"));
            let bytes = STANDARD.decode(command_payload).unwrap();
            assert_eq!(bytes.len() % 2, 0);
            let (chunks, remainder) = bytes.as_chunks::<2>();
            assert!(remainder.is_empty());
            let utf16 = chunks
                .iter()
                .map(|chunk| u16::from_le_bytes(*chunk))
                .collect::<Vec<_>>();
            String::from_utf16(&utf16).unwrap()
        };
        #[cfg(not(windows))]
        let script = {
            assert_eq!(args.last().map(String::as_str), Some("-Command"));
            command_payload
        };
        assert!(script.starts_with("$global:LASTEXITCODE = $null\n"));
        assert!(script.contains("$codexbridge_success = $?"));
        assert!(script.contains("$codexbridge_exit_code = $LASTEXITCODE"));
        assert!(script.contains("if ($codexbridge_success) { exit 0 }"));
        assert!(
            script
                .contains("if ($null -ne $codexbridge_exit_code) { exit $codexbridge_exit_code }")
        );
        assert!(script.ends_with("\nexit 1"));
        assert!(!script.starts_with("& {"));
    }

    #[test]
    fn windows_shell_resolution_canonicalizes_bare_names_and_preserves_explicit_paths() {
        let comspec = r"D:\Custom Windows\System32\cmd.exe";
        for shell in ["cmd", "cmd.exe", "CMD.EXE"] {
            assert_eq!(
                windows_shell_executable(shell, ShellKind::Cmd, Some(comspec)),
                comspec
            );
        }
        assert_eq!(
            windows_shell_executable("cmd", ShellKind::Cmd, None),
            "cmd.exe"
        );
        assert_eq!(
            windows_shell_executable("cmd.exe", ShellKind::Cmd, Some("powershell.exe")),
            "cmd.exe"
        );
        assert_eq!(
            windows_shell_executable("cmd", ShellKind::Cmd, Some("cmd")),
            "cmd.exe"
        );
        assert_eq!(
            windows_shell_executable("powershell", ShellKind::PowerShell, None),
            "powershell.exe"
        );
        assert_eq!(
            windows_shell_executable("pwsh", ShellKind::PowerShell, None),
            "pwsh.exe"
        );

        let explicit_cmd = r"C:\Windows\System32\cmd.exe";
        assert_eq!(
            windows_shell_executable(explicit_cmd, ShellKind::Cmd, Some(comspec)),
            explicit_cmd
        );
        let relative_cmd = r"tools\cmd.exe";
        // Explicit paths, including relative ones, are intentionally operator-selected
        // executables. Resolution only canonicalizes bare shell names.
        assert_eq!(
            windows_shell_executable(relative_cmd, ShellKind::Cmd, Some(comspec)),
            relative_cmd
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_shell_command_never_returns_bare_cmd() {
        let (shell, args, script) = shell_command(Some("cmd"), "echo ok").unwrap();
        assert!(!shell.eq_ignore_ascii_case("cmd"));
        assert_eq!(shell_kind(&shell), ShellKind::Cmd);
        if let Ok(comspec) = std::env::var("ComSpec")
            && is_cmd_executable(&comspec)
        {
            assert_eq!(shell, comspec);
        }
        assert_eq!(args, ["/d", "/s", "/c"]);
        assert_eq!(script, "echo ok");
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_shell_remains_powershell() {
        assert_eq!(default_shell(), "powershell.exe");
        let (shell, args, _) = shell_command(None, "echo ok").unwrap();
        assert_eq!(shell, "powershell.exe");
        assert_eq!(
            args,
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand"
            ]
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_bare_cmd_spawns_through_native_exec() {
        let project_dir = tempfile::tempdir().unwrap();
        let config = ConfigBuilder::from_map(std::collections::BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_EXEC_SANDBOX".to_owned(), "none".to_owned()),
        ]))
        .build()
        .unwrap();
        let project = ProjectContext {
            native_project_key: ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root: project_dir.path().to_path_buf(),
            metadata_root: project_dir.path().join(".metadata"),
            transport_mode: crate::request_context::TransportMode::Stateless,
            mcp_session_present: false,
        };
        let mut command = build_command_with_options(
            &config,
            &project,
            "echo codexbridge-native-cmd",
            false,
            Duration::from_secs(5),
            &BTreeMap::new(),
            project_dir.path(),
            Some("cmd"),
        )
        .unwrap();

        let output = command.output().await.unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("codexbridge-native-cmd"),
            "{output:?}"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_explicit_cmd_preserves_quoted_executable_after_separator() {
        let project_dir = tempfile::tempdir().unwrap();
        let config = ConfigBuilder::from_map(std::collections::BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_EXEC_SANDBOX".to_owned(), "none".to_owned()),
        ]))
        .build()
        .unwrap();
        let project = ProjectContext {
            native_project_key: ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root: project_dir.path().to_path_buf(),
            metadata_root: project_dir.path().join(".metadata"),
            transport_mode: crate::request_context::TransportMode::Stateless,
            mcp_session_present: false,
        };
        let quoted_executable = crate::platform::windows_system32_executable("where.exe");
        let command_text = format!(
            "echo codexbridge-before-quoted-child & \"{}\" cmd.exe",
            quoted_executable.display()
        );
        let mut command = build_command_with_options(
            &config,
            &project,
            &command_text,
            false,
            Duration::from_secs(5),
            &BTreeMap::new(),
            project_dir.path(),
            Some("cmd"),
        )
        .unwrap();

        let output = command.output().await.unwrap();
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("codexbridge-before-quoted-child"),
            "{output:?}"
        );
        assert!(
            stdout.to_ascii_lowercase().contains("cmd.exe"),
            "{output:?}"
        );
    }

    #[test]
    fn shell_classification_strips_exe_case_insensitively() {
        for shell in ["cmd.Exe", "PowerShell.Exe", "pwsh.EXE"] {
            let (_, args, _) = shell_command(Some(shell), "echo ok").unwrap();
            if shell.to_ascii_lowercase().starts_with("cmd") {
                assert_eq!(args.last().map(String::as_str), Some("/c"));
            } else {
                #[cfg(windows)]
                assert_eq!(args.last().map(String::as_str), Some("-EncodedCommand"));
                #[cfg(not(windows))]
                assert_eq!(args.last().map(String::as_str), Some("-Command"));
            }
        }
    }

    #[test]
    fn shell_classification_handles_names_paths_and_unknown_shells() {
        for shell in [
            "/bin/sh",
            "/bin/bash",
            "zsh",
            "fish",
            r"C:\Program Files\Git\bin\bash.exe",
        ] {
            assert_eq!(shell_kind(shell), ShellKind::Posix, "{shell}");
        }
        for shell in [
            "powershell",
            "pwsh",
            "PowerShell.EXE",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        ] {
            assert_eq!(shell_kind(shell), ShellKind::PowerShell, "{shell}");
        }
        for shell in ["cmd", "cmd.exe", r"C:\Windows\System32\cmd.EXE"] {
            assert_eq!(shell_kind(shell), ShellKind::Cmd, "{shell}");
        }
    }

    #[test]
    fn shell_command_arguments_follow_explicit_shell_not_host_platform() {
        let (_, posix_args, posix_script) = shell_command(Some("bash"), "echo ok").unwrap();
        assert_eq!(posix_args, ["-c"]);
        assert_eq!(posix_script, "echo ok");

        let (_, powershell_args, powershell_payload) =
            shell_command(Some("powershell.exe"), "native-command").unwrap();
        #[cfg(windows)]
        let powershell_script = {
            assert_eq!(
                powershell_args,
                [
                    "-NoLogo",
                    "-NoProfile",
                    "-InputFormat",
                    "Text",
                    "-OutputFormat",
                    "Text",
                    "-NonInteractive",
                    "-EncodedCommand"
                ]
            );
            let bytes = STANDARD.decode(powershell_payload).unwrap();
            let (chunks, remainder) = bytes.as_chunks::<2>();
            assert!(remainder.is_empty());
            let utf16 = chunks
                .iter()
                .map(|chunk| u16::from_le_bytes(*chunk))
                .collect::<Vec<_>>();
            String::from_utf16(&utf16).unwrap()
        };
        #[cfg(not(windows))]
        let powershell_script = {
            assert_eq!(powershell_args, ["-NoLogo", "-NoProfile", "-Command"]);
            powershell_payload
        };
        assert!(powershell_script.contains("$LASTEXITCODE"));

        let (_, cmd_args, cmd_script) = shell_command(Some("cmd.exe"), "echo ok").unwrap();
        assert_eq!(cmd_args, ["/d", "/s", "/c"]);
        assert_eq!(cmd_script, "echo ok");
    }

    #[test]
    fn shell_command_rejects_control_characters_and_oversized_executable() {
        for shell in ["bad\nshell", "bad\rshell", "bad\0shell"] {
            assert_eq!(
                shell_command(Some(shell), "echo ok").unwrap_err().code(),
                "INVALID_INPUT"
            );
        }
        let long = "s".repeat(4097);
        assert_eq!(
            shell_command(Some(&long), "echo ok").unwrap_err().code(),
            "INVALID_INPUT"
        );
    }

    #[test]
    fn default_exec_shell_reports_args_consistent_with_detected_kind() {
        use crate::config::ConfigBuilder;
        use std::collections::BTreeMap;
        let config = ConfigBuilder::from_map(BTreeMap::from([(
            "MCP_AUTH_TOKEN".to_owned(),
            "1234567890abcdef".to_owned(),
        )]))
        .build()
        .unwrap();
        let (shell, kind, args) = default_exec_shell(&config);
        assert_eq!(shell_kind(&shell).as_str(), kind);
        match kind {
            "powershell" => {
                #[cfg(windows)]
                assert_eq!(args.last().map(String::as_str), Some("-EncodedCommand"));
                #[cfg(not(windows))]
                assert_eq!(args.last().map(String::as_str), Some("-Command"));
            }
            "cmd" => assert_eq!(args.last().map(String::as_str), Some("/c")),
            _ => assert_eq!(args, ["-c"]),
        }
    }

    #[test]
    fn podman_commands_are_detected_without_special_casing_other_engines() {
        for command in [
            "podman run --rm alpine true",
            "/usr/bin/podman ps",
            "FOO=1 podman build .",
            "printf x | podman load",
            "env FOO=1 podman ps",
            "sudo -n podman run --rm alpine true",
            "sudo --non-interactive /usr/bin/podman ps",
            "env FOO=1 sudo -n podman build .",
            "rtk sudo -n podman run --rm alpine true",
            "rtk podman ps",
        ] {
            assert!(invokes_podman(command), "{command}");
        }
        for command in [
            "sudo -n podman run --rm alpine true",
            "sudo --non-interactive /usr/bin/podman ps",
            "env FOO=1 sudo -n podman build .",
            "rtk sudo -n podman run --rm alpine true",
        ] {
            assert!(invokes_sudo_podman(command), "{command}");
        }
        for command in [
            "podman run --rm alpine true",
            "/usr/bin/podman ps",
            "FOO=1 podman build .",
        ] {
            assert!(!invokes_sudo_podman(command), "{command}");
        }
        for command in [
            "printf podman-image",
            "echo podman",
            "FOO=1 docker build .",
            "buildah bud .",
        ] {
            assert!(!invokes_podman(command), "{command}");
        }
    }

    #[test]
    fn podman_falls_back_only_when_its_bwrap_probe_fails() {
        assert!(should_use_bwrap("auto", true, false, false, false));
        assert!(should_use_bwrap("auto", true, true, false, true));
        assert!(!should_use_bwrap("auto", true, true, false, false));
        assert!(!should_use_bwrap("auto", false, false, false, false));
        assert!(!should_use_bwrap("none", true, false, false, true));
    }

    #[test]
    fn podman_invocation_is_linux_only_and_tracks_verified_sudo_fallback() {
        assert_eq!(
            select_podman_invocation(false, false, true, true),
            PodmanInvocation::Unavailable
        );
        assert_eq!(
            select_podman_invocation(true, false, true, true),
            PodmanInvocation::DirectWithSudoFallback
        );
        assert_eq!(
            select_podman_invocation(true, false, false, true),
            PodmanInvocation::Sudo
        );
        assert_eq!(
            select_podman_invocation(true, false, true, false),
            PodmanInvocation::Direct
        );
        assert_eq!(
            select_podman_invocation(true, true, true, true),
            PodmanInvocation::Direct
        );
        assert_eq!(
            select_podman_invocation(true, false, false, false),
            PodmanInvocation::Unavailable
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn podman_runtime_probe_is_disabled_outside_linux() {
        assert_eq!(probe_podman_invocation(), PodmanInvocation::Unavailable);
    }

    #[test]
    fn direct_podman_with_sudo_fallback_tells_agent_to_retry_rootless_crun_failures() {
        let advice = PodmanInvocation::DirectWithSudoFallback.agent_advice();
        assert!(advice.contains("crun"));
        assert!(advice.contains("Operation not permitted"));
        assert!(advice.contains("retry the same Podman operation once"));
        assert!(advice.contains("sudo -n podman"));
    }

    #[test]
    fn sudo_podman_never_stays_inside_bwrap() {
        assert!(podman_can_use_bwrap(PodmanInvocation::Direct, true));
        assert!(podman_can_use_bwrap(
            PodmanInvocation::DirectWithSudoFallback,
            true
        ));
        assert!(!podman_can_use_bwrap(PodmanInvocation::Direct, false));
        assert!(!podman_can_use_bwrap(PodmanInvocation::Sudo, true));
        assert!(!podman_can_use_bwrap(PodmanInvocation::Unavailable, true));
        assert!(!should_use_bwrap("auto", true, true, true, true));
    }

    #[test]
    fn posix_invocation_is_not_wrapped_as_powershell() {
        let (_, args, script) = shell_command(Some("/bin/bash"), "printf ok").unwrap();
        assert_eq!(args, ["-c"]);
        assert_eq!(script, "printf ok");
    }
    #[tokio::test]
    async fn exec_env_additions_reject_malformed_keys_and_oversized_values() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigBuilder::from_map(BTreeMap::from([
            (
                "WORKSPACE_ROOT".to_owned(),
                directory.path().display().to_string(),
            ),
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_EXEC_SANDBOX".to_owned(), "none".to_owned()),
        ]))
        .build()
        .unwrap();
        let project = ProjectContext {
            native_project_key: ProjectKey::new("native".to_owned()).unwrap(),
            effective_project_key: ProjectKey::new("effective".to_owned()).unwrap(),
            project_alias: None,
            project_root: directory.path().to_path_buf(),
            metadata_root: directory.path().join(".metadata"),
            transport_mode: crate::request_context::TransportMode::Stateless,
            mcp_session_present: false,
        };
        let build = |environment: BTreeMap<String, String>| {
            let workdir = project.project_root.clone();
            build_command_with_options(
                &config,
                &project,
                "echo ok",
                true,
                Duration::from_secs(10),
                &environment,
                &workdir,
                None,
            )
        };

        let mut oversized_value = BTreeMap::new();
        oversized_value.insert("BIG".to_owned(), "x".repeat(8193));
        assert_eq!(build(oversized_value).unwrap_err().code(), "INVALID_INPUT");

        let mut key_with_equals = BTreeMap::new();
        key_with_equals.insert("BAD=KEY".to_owned(), "v".to_owned());
        assert_eq!(build(key_with_equals).unwrap_err().code(), "INVALID_INPUT");

        // A well-formed addition must pass and reach the command environment.
        let mut valid = BTreeMap::new();
        valid.insert(
            "CODEXBRIDGE_ENV_FORWARDING_PROBE".to_owned(),
            "forwarded-exactly-42".to_owned(),
        );
        assert!(build(valid.clone()).is_ok());

        let command_text = if cfg!(windows) {
            "Write-Output $env:CODEXBRIDGE_ENV_FORWARDING_PROBE"
        } else {
            "printf '%s' \"$CODEXBRIDGE_ENV_FORWARDING_PROBE\""
        };
        let result = execute(
            &config,
            &project,
            command_text,
            Duration::from_secs(10),
            &valid,
        )
        .await
        .unwrap();
        if cfg!(windows) && std::env::var_os("CODEXBRIDGE_WINDOWS_PROCESS_DIAGNOSTICS").is_some() {
            let (shell, args, payload) = shell_command(None, command_text).unwrap();
            #[cfg(windows)]
            let decoded = {
                let bytes = STANDARD.decode(&payload).unwrap();
                let (chunks, remainder) = bytes.as_chunks::<2>();
                assert!(remainder.is_empty());
                let utf16 = chunks
                    .iter()
                    .map(|chunk| u16::from_le_bytes(*chunk))
                    .collect::<Vec<_>>();
                String::from_utf16(&utf16).unwrap()
            };
            #[cfg(not(windows))]
            let decoded = payload;
            eprintln!(
                "codexbridge-windows-sandbox shell={shell:?}; args={args:?}; script={decoded:?}; exit_code={:?}; timed_out={}; stdout={:?}; stderr={:?}; stdout_bytes={}; stderr_bytes={}; truncated={}",
                result.exit_code,
                result.timed_out,
                result.stdout,
                result.stderr,
                result.stdout_bytes,
                result.stderr_bytes,
                result.truncated
            );
            for name in [
                "PATH",
                "PATHEXT",
                "SystemRoot",
                "WINDIR",
                "ComSpec",
                "USERPROFILE",
                "HOMEDRIVE",
                "HOMEPATH",
                "LOCALAPPDATA",
                "APPDATA",
                "ProgramData",
                "ProgramFiles",
                "ProgramFiles(x86)",
                "ProgramW6432",
                "SystemDrive",
                "TEMP",
                "TMP",
                "PSModulePath",
            ] {
                eprintln!(
                    "codexbridge-windows-parent-env {name}={:?}",
                    std::env::var_os(name)
                );
            }
        }
        assert_eq!(
            result.exit_code,
            Some(0),
            "timed_out={}; stdout={:?}; stderr={:?}; stdout_bytes={}; stderr_bytes={}; truncated={}",
            result.timed_out,
            result.stdout,
            result.stderr,
            result.stdout_bytes,
            result.stderr_bytes,
            result.truncated
        );
        assert_eq!(
            result.stdout.trim(),
            "forwarded-exactly-42",
            "timed_out={}; exit_code={:?}; stderr={:?}",
            result.timed_out,
            result.exit_code,
            result.stderr
        );
    }
}
