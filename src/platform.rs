//! User-private platform paths, filesystem boundaries, and diagnostics.
//!
//! This module is the only place that interprets XDG path environment variables
//! or creates tmnotify-owned files. Callers receive resolved paths and
//! deliberately content-free log events rather than filesystem mechanics.

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};

const APPLICATION_DIRECTORY: &str = "tmnotify";
const CONFIG_FILE: &str = "config.toml";
const HISTORY_FILE: &str = "history.sqlite3";
const LOG_FILE: &str = "tmnotify.log";
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const LOG_ROTATE_BYTES: u64 = 1024 * 1024;

/// An injectable environment used so path and configuration decisions can be
/// tested without mutating process-global environment variables.
#[derive(Clone, Debug, Default)]
pub struct Environment {
    values: BTreeMap<OsString, OsString>,
}

impl Environment {
    pub fn current() -> Self {
        Self {
            values: env::vars_os().collect(),
        }
    }

    pub fn from_pairs(pairs: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Self {
            values: pairs.into_iter().collect(),
        }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&OsStr> {
        self.values.get(OsStr::new(name)).map(OsString::as_os_str)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.values.contains_key(OsStr::new(name))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformPaths {
    pub config_file: PathBuf,
    pub history_file: PathBuf,
    pub log_file: PathBuf,
    pub runtime_directory: PathBuf,
}

impl PlatformPaths {
    pub fn resolve(environment: &Environment) -> Result<Self, PathError> {
        let home = environment
            .get("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        if home.is_none()
            && (xdg_home(environment, "XDG_CONFIG_HOME").is_none()
                || xdg_home(environment, "XDG_STATE_HOME").is_none())
        {
            return Err(PathError::HomeUnavailable);
        }
        // `home` is consulted only for an unset XDG base. When both bases are
        // explicit, no home-directory discovery is necessary.
        let home = home.unwrap_or_default();
        let temporary_directory = env::temp_dir();
        Self::resolve_with(
            environment,
            &home,
            &temporary_directory,
            effective_user_id(),
        )
    }

    fn resolve_with(
        environment: &Environment,
        home: &Path,
        temporary_directory: &Path,
        user_id: u32,
    ) -> Result<Self, PathError> {
        let config_home =
            xdg_home(environment, "XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let state_home =
            xdg_home(environment, "XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"));
        let runtime_directory = match xdg_home(environment, "XDG_RUNTIME_DIR") {
            Some(directory) => directory.join(APPLICATION_DIRECTORY),
            None => temporary_directory.join(format!("tmnotify-{user_id}")),
        };

        let config_directory = config_home.join(APPLICATION_DIRECTORY);
        let state_directory = state_home.join(APPLICATION_DIRECTORY);
        Ok(Self {
            config_file: config_directory.join(CONFIG_FILE),
            history_file: state_directory.join(HISTORY_FILE),
            log_file: state_directory.join(LOG_FILE),
            runtime_directory,
        })
    }

    /// Creates and verifies tmnotify-owned directories. Existing paths must be
    /// real directories owned by the current user and inaccessible to others.
    pub fn ensure_private_directories(&self) -> Result<(), PathError> {
        if let Some(directory) = self.config_file.parent() {
            ensure_private_directory(directory)?;
        }
        if let Some(directory) = self.history_file.parent() {
            ensure_private_directory(directory)?;
        }
        ensure_private_directory(&self.runtime_directory)
    }

    pub fn socket_path(&self, server_id: &str) -> Result<PathBuf, PathError> {
        if server_id.is_empty()
            || server_id.len() > 64
            || !server_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(PathError::InvalidServerId);
        }
        Ok(self.runtime_directory.join(format!("{server_id}.sock")))
    }
}

fn xdg_home(environment: &Environment, name: &'static str) -> Option<PathBuf> {
    environment
        .get(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        // The XDG base directory specification requires absolute paths. Ignore
        // relative values rather than letting them escape into the cwd.
        .filter(|path| path.is_absolute())
}

#[derive(Debug, Error)]
pub enum PathError {
    #[error("HOME is unavailable")]
    HomeUnavailable,
    #[error("invalid tmux server identifier")]
    InvalidServerId,
    #[error("private path is a symbolic link: {0}")]
    SymbolicLink(PathBuf),
    #[error("private path has an unexpected file type: {0}")]
    UnexpectedFileType(PathBuf),
    #[error("private path is not owned by the current user: {0}")]
    WrongOwner(PathBuf),
    #[error("private path permissions are too broad: {0}")]
    InsecurePermissions(PathBuf),
    #[error("filesystem operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub fn ensure_private_directory(path: &Path) -> Result<(), PathError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_directory(path)?;
            let metadata = fs::symlink_metadata(path).map_err(|source| PathError::Io {
                path: path.to_owned(),
                source,
            })?;
            validate_private_directory(path, &metadata)
        }
        Err(source) => Err(PathError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Prepares a private file's parent and returns a path with every existing
/// ancestor resolved. This lets consumers retain no-follow file opens on
/// platforms whose temporary-directory path contains a system symlink (for
/// example, macOS `/var` -> `/private/var`). The final parent is still checked
/// at both the supplied and canonical paths before the file is opened.
pub(crate) fn prepare_private_file_path(path: &Path) -> Result<PathBuf, PathError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| PathError::UnexpectedFileType(path.to_owned()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| PathError::UnexpectedFileType(path.to_owned()))?;
    ensure_private_directory(parent)?;

    let canonical_parent = fs::canonicalize(parent).map_err(|source| PathError::Io {
        path: parent.to_owned(),
        source,
    })?;
    let metadata = fs::symlink_metadata(&canonical_parent).map_err(|source| PathError::Io {
        path: canonical_parent.clone(),
        source,
    })?;
    validate_private_directory(&canonical_parent, &metadata)?;
    Ok(canonical_parent.join(file_name))
}

fn create_private_directory(path: &Path) -> Result<(), PathError> {
    let parent = path
        .parent()
        .ok_or_else(|| PathError::UnexpectedFileType(path.to_owned()))?;
    fs::create_dir_all(parent).map_err(|source| PathError::Io {
        path: parent.to_owned(),
        source,
    })?;
    let result = fs::create_dir(path);
    if let Err(error) = result
        && error.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(PathError::Io {
            path: path.to_owned(),
            source: error,
        });
    }
    set_mode(path, PRIVATE_DIRECTORY_MODE)
}

fn validate_private_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), PathError> {
    if metadata.file_type().is_symlink() {
        return Err(PathError::SymbolicLink(path.to_owned()));
    }
    if !metadata.is_dir() {
        return Err(PathError::UnexpectedFileType(path.to_owned()));
    }
    validate_owner_and_mode(path, metadata, PRIVATE_DIRECTORY_MODE)
}

pub(crate) fn open_private_read(path: &Path) -> Result<File, PathError> {
    let mut options = private_open_options();
    let file = options
        .read(true)
        .open(path)
        .map_err(|source| map_open_error(path, source))?;
    validate_private_file(
        path,
        &file.metadata().map_err(|source| PathError::Io {
            path: path.to_owned(),
            source,
        })?,
    )?;
    Ok(file)
}

fn open_private_append(path: &Path) -> Result<File, PathError> {
    let mut existing = private_open_options();
    let file = match existing.append(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut create = private_open_options();
            match create.append(true).create_new(true).open(path) {
                Ok(file) => {
                    set_file_mode(path, &file, PRIVATE_FILE_MODE)?;
                    file
                }
                // Another local writer may have won creation. Open and verify
                // its file rather than weakening create-new semantics.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let mut retry = private_open_options();
                    retry
                        .append(true)
                        .open(path)
                        .map_err(|source| map_open_error(path, source))?
                }
                Err(source) => return Err(map_open_error(path, source)),
            }
        }
        Err(source) => return Err(map_open_error(path, source)),
    };
    validate_private_file(
        path,
        &file.metadata().map_err(|source| PathError::Io {
            path: path.to_owned(),
            source,
        })?,
    )?;
    Ok(file)
}

fn private_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    options
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options
}

fn map_open_error(path: &Path, source: io::Error) -> PathError {
    #[cfg(unix)]
    if source.raw_os_error() == Some(libc::ELOOP) {
        return PathError::SymbolicLink(path.to_owned());
    }
    PathError::Io {
        path: path.to_owned(),
        source,
    }
}

fn validate_private_file(path: &Path, metadata: &fs::Metadata) -> Result<(), PathError> {
    if !metadata.is_file() {
        return Err(PathError::UnexpectedFileType(path.to_owned()));
    }
    validate_owner_and_mode(path, metadata, PRIVATE_FILE_MODE)
}

#[cfg(unix)]
fn validate_owner_and_mode(
    path: &Path,
    metadata: &fs::Metadata,
    expected_mode: u32,
) -> Result<(), PathError> {
    if metadata.uid() != effective_user_id() {
        return Err(PathError::WrongOwner(path.to_owned()));
    }
    if metadata.mode() & 0o777 != expected_mode {
        return Err(PathError::InsecurePermissions(path.to_owned()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_owner_and_mode(
    _path: &Path,
    _metadata: &fs::Metadata,
    _expected_mode: u32,
) -> Result<(), PathError> {
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), PathError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| PathError::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(unix)]
fn set_file_mode(path: &Path, file: &File, mode: u32) -> Result<(), PathError> {
    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|source| PathError::Io {
            path: path.to_owned(),
            source,
        })
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path, _file: &File, _mode: u32) -> Result<(), PathError> {
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), PathError> {
    Ok(())
}

#[cfg(unix)]
fn effective_user_id() -> u32 {
    // SAFETY: geteuid takes no arguments and has no safety preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn effective_user_id() -> u32 {
    0
}

/// Validates a prospective runtime socket without following symbolic links.
/// A missing socket is valid; creation/binding remains the daemon's job.
pub fn validate_runtime_socket(path: &Path) -> Result<(), PathError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PathError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(PathError::SymbolicLink(path.to_owned()));
    }
    #[cfg(unix)]
    if !metadata.file_type().is_socket() {
        return Err(PathError::UnexpectedFileType(path.to_owned()));
    }
    validate_owner_and_mode(path, &metadata, PRIVATE_FILE_MODE)
}

#[derive(Clone, Copy, Debug)]
pub enum LogLevel {
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug)]
pub enum LogEvent {
    InvalidConfiguration,
    HistoryUnavailable,
    HookInputRejected,
    HookSubmissionFailed,
    RendererAuthenticationRejected,
    RuntimePermissionRejected,
}

impl LogEvent {
    fn code(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "config_invalid",
            Self::HistoryUnavailable => "history_unavailable",
            Self::HookInputRejected => "hook_input_rejected",
            Self::HookSubmissionFailed => "hook_submission_failed",
            Self::RendererAuthenticationRejected => "renderer_auth_rejected",
            Self::RuntimePermissionRejected => "runtime_permission_rejected",
        }
    }
}

/// A one-generation private logger. Its API intentionally accepts no provider
/// payload or Notification content, making sensitive-data exclusion structural.
pub struct PrivateLogger {
    path: PathBuf,
    write_guard: Mutex<()>,
}

impl PrivateLogger {
    pub fn new(path: PathBuf) -> Result<Self, PathError> {
        let directory = path
            .parent()
            .ok_or_else(|| PathError::UnexpectedFileType(path.clone()))?;
        ensure_private_directory(directory)?;
        Ok(Self {
            path,
            write_guard: Mutex::new(()),
        })
    }

    pub fn write(&self, level: LogLevel, event: LogEvent) -> Result<(), PathError> {
        let _guard = self
            .write_guard
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.rotate_if_needed()?;
        let mut file = open_private_append(&self.path)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        writeln!(
            file,
            "time_unix_ms={timestamp} level={} event={}",
            match level {
                LogLevel::Warning => "warning",
                LogLevel::Error => "error",
            },
            event.code()
        )
        .map_err(|source| PathError::Io {
            path: self.path.clone(),
            source,
        })
    }

    fn rotate_if_needed(&self) -> Result<(), PathError> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(PathError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(PathError::SymbolicLink(self.path.clone()));
        }
        validate_private_file(&self.path, &metadata)?;
        if metadata.len() < LOG_ROTATE_BYTES {
            return Ok(());
        }

        let rotated = self.path.with_file_name(format!("{LOG_FILE}.1"));
        if let Ok(metadata) = fs::symlink_metadata(&rotated)
            && metadata.is_dir()
        {
            return Err(PathError::UnexpectedFileType(rotated));
        }
        fs::rename(&self.path, &rotated).map_err(|source| PathError::Io {
            path: self.path.clone(),
            source,
        })?;
        set_mode(&rotated, PRIVATE_FILE_MODE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn environment(entries: &[(&str, &Path)]) -> Environment {
        Environment::from_pairs(
            entries
                .iter()
                .map(|(name, path)| (OsString::from(name), path.as_os_str().to_owned())),
        )
    }

    #[test]
    fn xdg_paths_are_identical_for_supported_platform_policy() {
        let env = environment(&[
            ("XDG_CONFIG_HOME", Path::new("/xdg/config")),
            ("XDG_STATE_HOME", Path::new("/xdg/state")),
            ("XDG_RUNTIME_DIR", Path::new("/xdg/runtime")),
        ]);
        let paths = PlatformPaths::resolve_with(&env, Path::new("/home/me"), Path::new("/tmp"), 42)
            .expect("paths");
        assert_eq!(
            paths.config_file,
            Path::new("/xdg/config/tmnotify/config.toml")
        );
        assert_eq!(
            paths.history_file,
            Path::new("/xdg/state/tmnotify/history.sqlite3")
        );
        assert_eq!(
            paths.log_file,
            Path::new("/xdg/state/tmnotify/tmnotify.log")
        );
        assert_eq!(paths.runtime_directory, Path::new("/xdg/runtime/tmnotify"));
    }

    #[test]
    fn explicit_xdg_bases_do_not_require_home_discovery() {
        let env = environment(&[
            ("XDG_CONFIG_HOME", Path::new("/xdg/config")),
            ("XDG_STATE_HOME", Path::new("/xdg/state")),
        ]);
        let paths = PlatformPaths::resolve(&env).expect("explicit XDG paths");
        assert_eq!(
            paths.config_file,
            Path::new("/xdg/config/tmnotify/config.toml")
        );
        assert_eq!(
            paths.history_file,
            Path::new("/xdg/state/tmnotify/history.sqlite3")
        );
    }

    #[test]
    fn relative_xdg_values_use_linux_and_macos_fallbacks() {
        let env = environment(&[("XDG_CONFIG_HOME", Path::new("relative"))]);
        let paths = PlatformPaths::resolve_with(
            &env,
            Path::new("/Users/me"),
            Path::new("/private/tmp"),
            501,
        )
        .expect("paths");
        assert_eq!(
            paths.config_file,
            Path::new("/Users/me/.config/tmnotify/config.toml")
        );
        assert_eq!(
            paths.history_file,
            Path::new("/Users/me/.local/state/tmnotify/history.sqlite3")
        );
        assert_eq!(
            paths.runtime_directory,
            Path::new("/private/tmp/tmnotify-501")
        );
    }

    #[cfg(unix)]
    #[test]
    fn creates_runtime_directory_with_private_permissions() {
        let temp = tempfile::tempdir().expect("temp directory");
        let path = temp.path().join("tmnotify-1");
        ensure_private_directory(&path).expect("private directory");
        let metadata = fs::metadata(path).expect("metadata");
        assert_eq!(metadata.mode() & 0o777, PRIVATE_DIRECTORY_MODE);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_insecure_directory_and_symlink() {
        let temp = tempfile::tempdir().expect("temp directory");
        let insecure = temp.path().join("insecure");
        fs::create_dir(&insecure).expect("directory");
        fs::set_permissions(&insecure, fs::Permissions::from_mode(0o755)).expect("permissions");
        assert!(matches!(
            ensure_private_directory(&insecure),
            Err(PathError::InsecurePermissions(_))
        ));

        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&insecure, &link).expect("symlink");
        assert!(matches!(
            ensure_private_directory(&link),
            Err(PathError::SymbolicLink(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_when_opening_private_file() {
        let temp = tempfile::tempdir().expect("temp directory");
        let target = temp.path().join("target");
        fs::write(&target, "secret").expect("target");
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(target, &link).expect("symlink");
        assert!(matches!(
            open_private_read(&link),
            Err(PathError::SymbolicLink(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_socket_rejects_non_socket_and_symlink_entries() {
        let temp = tempfile::tempdir().expect("temp directory");
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).expect("permissions");
        let socket = temp.path().join("server.sock");
        fs::write(&socket, "not a socket").expect("regular file");
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("permissions");
        assert!(matches!(
            validate_runtime_socket(&socket),
            Err(PathError::UnexpectedFileType(_))
        ));

        let link = temp.path().join("link.sock");
        std::os::unix::fs::symlink(&socket, &link).expect("symlink");
        assert!(matches!(
            validate_runtime_socket(&link),
            Err(PathError::SymbolicLink(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn logger_never_accepts_sensitive_content_and_rotates_once() {
        let temp = tempfile::tempdir().expect("temp directory");
        let directory = temp.path().join("state");
        fs::create_dir(&directory).expect("state directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).expect("permissions");
        let path = directory.join(LOG_FILE);
        let mut oversized = OpenOptions::new()
            .create(true)
            .write(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&path)
            .expect("log");
        oversized
            .write_all(&vec![b'x'; LOG_ROTATE_BYTES as usize])
            .expect("oversized log");
        drop(oversized);

        let logger = PrivateLogger::new(path.clone()).expect("logger");
        logger
            .write(LogLevel::Warning, LogEvent::HookInputRejected)
            .expect("write");

        assert_eq!(fs::metadata(&path).expect("new log").mode() & 0o777, 0o600);
        let rotated = directory.join("tmnotify.log.1");
        assert_eq!(
            fs::metadata(rotated).expect("rotated").len(),
            LOG_ROTATE_BYTES
        );
        let mut contents = String::new();
        open_private_read(&path)
            .expect("private log")
            .read_to_string(&mut contents)
            .expect("read");
        assert_eq!(contents.matches('\n').count(), 1);
        assert!(contents.contains("event=hook_input_rejected"));
    }

    #[test]
    fn socket_name_is_bounded_and_path_safe() {
        let paths = PlatformPaths {
            config_file: PathBuf::from("/config"),
            history_file: PathBuf::from("/history"),
            log_file: PathBuf::from("/log"),
            runtime_directory: PathBuf::from("/runtime"),
        };
        assert_eq!(
            paths.socket_path("server_01").expect("socket"),
            Path::new("/runtime/server_01.sock")
        );
        assert!(matches!(
            paths.socket_path("../escape"),
            Err(PathError::InvalidServerId)
        ));
    }
}
