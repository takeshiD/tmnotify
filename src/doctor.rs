//! Read-only diagnostics with a stable machine-readable schema.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

use serde::Serialize;

use crate::config::{ConfigError, ConfigOverrides, load};
use crate::platform::{Environment, PlatformPaths};
use crate::tmux::Server;

pub const DOCTOR_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warning,
    Fail,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiagnosticCheck {
    /// Stable programmatic identifier.
    pub code: &'static str,
    pub status: CheckStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

impl DiagnosticCheck {
    fn pass(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            status: CheckStatus::Pass,
            message: message.into(),
            remediation: None,
        }
    }

    fn warning(
        code: &'static str,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            code,
            status: CheckStatus::Warning,
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }

    fn fail(
        code: &'static str,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            code,
            status: CheckStatus::Fail,
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DoctorReport {
    pub schema_version: u16,
    pub checks: Vec<DiagnosticCheck>,
}

impl DoctorReport {
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.checks
            .iter()
            .all(|check| check.status != CheckStatus::Fail)
    }

    pub fn write_json(&self, mut output: impl Write) -> Result<(), DoctorOutputError> {
        serde_json::to_writer(&mut output, self)?;
        output.write_all(b"\n")?;
        Ok(())
    }

    pub fn write_human(&self, mut output: impl Write, unicode: bool) -> Result<(), io::Error> {
        for check in &self.checks {
            let (symbol, word) = match (check.status, unicode) {
                (CheckStatus::Pass, true) => ("✓", "PASS"),
                (CheckStatus::Warning, true) => ("!", "WARN"),
                (CheckStatus::Fail, true) => ("×", "FAIL"),
                (CheckStatus::Pass, false) => ("[ok]", "PASS"),
                (CheckStatus::Warning, false) => ("[!]", "WARN"),
                (CheckStatus::Fail, false) => ("[x]", "FAIL"),
            };
            writeln!(output, "{symbol} {word} {}: {}", check.code, check.message)?;
            if let Some(remediation) = &check.remediation {
                writeln!(output, "    {remediation}")?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookObservation {
    pub provider: &'static str,
    pub installed: bool,
    pub synchronized: bool,
}

/// Inputs to the production read-only probe. Supplying no tmux target still
/// runs all independent checks.
pub struct SystemProbe<'a> {
    pub paths: &'a PlatformPaths,
    pub environment: &'a Environment,
    pub tmux: Option<&'a Server>,
    pub daemon_socket: Option<&'a Path>,
    pub hooks: &'a [HookObservation],
}

impl SystemProbe<'_> {
    #[must_use]
    pub fn run(&self) -> DoctorReport {
        let mut checks = vec![
            self.check_config(),
            check_private_directory(&self.paths.runtime_directory),
            check_history_path(&self.paths.history_file),
            self.check_tmux(),
            self.check_daemon(),
        ];
        checks.extend(self.check_hooks());
        DoctorReport {
            schema_version: DOCTOR_SCHEMA_VERSION,
            checks,
        }
    }

    fn check_config(&self) -> DiagnosticCheck {
        match load(
            &self.paths.config_file,
            self.environment,
            ConfigOverrides::default(),
        ) {
            Ok(_) => DiagnosticCheck::pass("config", "configuration is valid"),
            Err(error) => DiagnosticCheck::fail(
                "config",
                safe_config_error(&error),
                "fix the user-level config.toml; repository config is not loaded",
            ),
        }
    }

    fn check_tmux(&self) -> DiagnosticCheck {
        let Some(tmux) = self.tmux else {
            return DiagnosticCheck::warning(
                "tmux_capabilities",
                "no tmux server target was selected",
                "run inside tmux or pass -L/-S to check display capabilities",
            );
        };
        match tmux.probe() {
            Ok(report) if report.supports_display_service() => DiagnosticCheck::pass(
                "tmux_capabilities",
                format!(
                    "required capability surface is available ({})",
                    report.version
                ),
            ),
            Ok(report) => DiagnosticCheck::fail(
                "tmux_capabilities",
                format!(
                    "{} required capability groups are missing ({})",
                    report.missing.len(),
                    report.version
                ),
                "install a tmux build exposing the documented 3.8-compatible command, format, and event surface",
            ),
            Err(_) => DiagnosticCheck::fail(
                "tmux_capabilities",
                "the selected tmux server could not be probed",
                "verify the -L/-S target and that the tmux server is running",
            ),
        }
    }

    fn check_daemon(&self) -> DiagnosticCheck {
        let Some(socket) = self.daemon_socket else {
            return daemon_check(DaemonObservation::NotApplicable);
        };
        #[cfg(unix)]
        {
            let metadata = match fs::symlink_metadata(socket) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return daemon_check(DaemonObservation::Missing);
                }
                Err(_) => return daemon_check(DaemonObservation::UnsafeType),
            };
            if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
                return daemon_check(DaemonObservation::UnsafeType);
            }
            daemon_check(if UnixStream::connect(socket).is_ok() {
                DaemonObservation::Reachable
            } else {
                DaemonObservation::Unreachable
            })
        }
        #[cfg(not(unix))]
        daemon_check(DaemonObservation::UnsupportedPlatform)
    }

    fn check_hooks(&self) -> Vec<DiagnosticCheck> {
        let mut checks = Vec::new();
        for hook in self.hooks {
            let code = match hook.provider {
                "claude" => "hook_claude",
                "codex" => "hook_codex",
                _ => "hook_unknown",
            };
            checks.push(if !hook.installed {
                DiagnosticCheck::warning(
                    code,
                    format!("{} hook is not installed", hook.provider),
                    format!(
                        "run tmnotify hook install {} at the intended scope",
                        hook.provider
                    ),
                )
            } else if !hook.synchronized {
                DiagnosticCheck::warning(
                    code,
                    format!("{} hook is installed but out of sync", hook.provider),
                    format!("run tmnotify hook sync {}", hook.provider),
                )
            } else {
                DiagnosticCheck::pass(
                    code,
                    format!("{} hook is installed and synchronized", hook.provider),
                )
            });
            checks.push(DiagnosticCheck::warning(
                match hook.provider {
                    "claude" => "trust_claude",
                    "codex" => "trust_codex",
                    _ => "trust_unknown",
                },
                format!("{} hook trust is unknown", hook.provider),
                "verify trust using the providers own /hooks interface; tmnotify does not inspect or change trust stores",
            ));
        }
        checks
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DaemonObservation {
    NotApplicable,
    Missing,
    UnsafeType,
    Reachable,
    Unreachable,
    #[cfg(not(unix))]
    UnsupportedPlatform,
}

fn daemon_check(observation: DaemonObservation) -> DiagnosticCheck {
    match observation {
        DaemonObservation::NotApplicable => DiagnosticCheck::warning(
            "daemon_reachability",
            "daemon socket is not applicable without a selected server",
            "select a tmux server to check its lazy daemon",
        ),
        DaemonObservation::Missing => DiagnosticCheck::warning(
            "daemon_reachability",
            "the lazy daemon has not created its socket",
            "run a display command to start the daemon, then rerun doctor",
        ),
        DaemonObservation::UnsafeType => DiagnosticCheck::fail(
            "daemon_reachability",
            "daemon path is missing, unreadable, a symlink, or not a Unix socket",
            "check runtime permissions and stale-socket diagnostics; no repair was attempted",
        ),
        DaemonObservation::Reachable => {
            DiagnosticCheck::pass("daemon_reachability", "daemon socket is reachable")
        }
        DaemonObservation::Unreachable => DiagnosticCheck::fail(
            "daemon_reachability",
            "daemon socket exists but cannot be reached",
            "check runtime permissions and stale-socket diagnostics; no repair was attempted",
        ),
        #[cfg(not(unix))]
        DaemonObservation::UnsupportedPlatform => DiagnosticCheck::warning(
            "daemon_reachability",
            "Unix daemon sockets are unsupported on this platform",
            "use Linux or macOS for supported display delivery",
        ),
    }
}

fn safe_config_error(error: &ConfigError) -> &'static str {
    match error {
        ConfigError::TooLarge => "configuration exceeds its size limit",
        ConfigError::Parse(_) => "configuration contains an unknown field or invalid value",
        ConfigError::Serialize(_) => "configuration could not be serialized",
        ConfigError::PrivatePath(_) => {
            "configuration path ownership, type, or permissions are unsafe"
        }
        ConfigError::OutOfRange { .. }
        | ConfigError::DurationOutOfRange { .. }
        | ConfigError::ConflictingHookEvent { .. } => {
            "configuration contains an invalid bounded value"
        }
        ConfigError::Io { .. } => "configuration could not be read",
    }
}

fn check_private_directory(path: &Path) -> DiagnosticCheck {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return DiagnosticCheck::warning(
                "runtime_permissions",
                "runtime directory does not exist yet",
                "it will be created privately on first daemon start",
            );
        }
        Err(_) => {
            return DiagnosticCheck::fail(
                "runtime_permissions",
                "runtime directory metadata cannot be read",
                "check ownership and parent-directory permissions",
            );
        }
    };
    #[cfg(unix)]
    {
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return DiagnosticCheck::fail(
                "runtime_permissions",
                "runtime directory is not a current-user-owned mode-0700 directory",
                "replace it with a real directory owned by the current user and mode 0700",
            );
        }
    }
    DiagnosticCheck::pass("runtime_permissions", "runtime directory is private")
}

fn check_history_path(path: &Path) -> DiagnosticCheck {
    let target = if path.exists() {
        path.to_path_buf()
    } else if let Some(parent) = path.parent() {
        parent.to_path_buf()
    } else {
        PathBuf::from(path)
    };
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return DiagnosticCheck::warning(
                "history_writable",
                "History path does not exist yet",
                "it will be created privately when History first opens",
            );
        }
        Err(_) => {
            return DiagnosticCheck::fail(
                "history_writable",
                "History path metadata cannot be read",
                "check the XDG state path ownership and permissions",
            );
        }
    };
    #[cfg(unix)]
    {
        let is_expected_type = if target == path {
            metadata.is_file() && !metadata.file_type().is_symlink()
        } else {
            metadata.is_dir() && !metadata.file_type().is_symlink()
        };
        if !is_expected_type
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o200 == 0
        {
            return DiagnosticCheck::fail(
                "history_writable",
                "History path is not safely writable by the current user",
                "fix the XDG state directory ownership, type, and private write permissions",
            );
        }
    }
    DiagnosticCheck::pass("history_writable", "History path is safely writable")
}

#[derive(Debug, thiserror::Error)]
pub enum DoctorOutputError {
    #[error("failed to encode doctor JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to write doctor output: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use tempfile::TempDir;

    use super::*;

    fn environment(root: &Path) -> Environment {
        Environment::from_pairs([
            (OsString::from("HOME"), root.as_os_str().to_owned()),
            (
                OsString::from("XDG_CONFIG_HOME"),
                root.join("config").into_os_string(),
            ),
            (
                OsString::from("XDG_STATE_HOME"),
                root.join("state").into_os_string(),
            ),
            (
                OsString::from("XDG_RUNTIME_DIR"),
                root.join("runtime-base").into_os_string(),
            ),
        ])
    }

    #[test]
    fn report_schema_and_human_output_are_stable_and_accessible() {
        let report = DoctorReport {
            schema_version: DOCTOR_SCHEMA_VERSION,
            checks: vec![
                DiagnosticCheck::pass("config", "valid"),
                DiagnosticCheck::warning("trust_codex", "unknown", "verify in /hooks"),
                DiagnosticCheck::fail("history_writable", "unsafe", "fix permissions"),
            ],
        };
        assert!(!report.is_healthy());
        let mut json = Vec::new();
        report.write_json(&mut json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["checks"][1]["status"], "warning");

        let mut human = Vec::new();
        report.write_human(&mut human, false).unwrap();
        let human = String::from_utf8(human).unwrap();
        assert!(human.contains("[ok] PASS config"));
        assert!(human.contains("[!] WARN trust_codex"));
        assert!(human.contains("[x] FAIL history_writable"));
    }

    #[test]
    fn system_probe_is_read_only_and_runs_without_tmux_or_daemon() {
        let temporary = TempDir::new().unwrap();
        let environment = environment(temporary.path());
        let paths = PlatformPaths::resolve(&environment).unwrap();
        let report = SystemProbe {
            paths: &paths,
            environment: &environment,
            tmux: None,
            daemon_socket: None,
            hooks: &[],
        }
        .run();
        assert!(report.is_healthy());
        assert_eq!(report.checks.len(), 5);
        assert_eq!(report.checks[0].status, CheckStatus::Pass);
        assert_eq!(report.checks[1].status, CheckStatus::Warning);
        assert!(!paths.runtime_directory.exists());
        assert!(!paths.history_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn reports_private_paths_reachable_daemon_and_unknown_trust() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = TempDir::new().unwrap();
        let environment = environment(temporary.path());
        let paths = PlatformPaths::resolve(&environment).unwrap();
        fs::create_dir_all(&paths.runtime_directory).unwrap();
        fs::set_permissions(&paths.runtime_directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(paths.history_file.parent().unwrap()).unwrap();
        fs::set_permissions(
            paths.history_file.parent().unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let hooks = [HookObservation {
            provider: "codex",
            installed: true,
            synchronized: true,
        }];
        let report = SystemProbe {
            paths: &paths,
            environment: &environment,
            tmux: None,
            daemon_socket: None,
            hooks: &hooks,
        }
        .run();
        assert_eq!(
            daemon_check(DaemonObservation::Reachable).status,
            CheckStatus::Pass
        );
        assert_eq!(
            report
                .checks
                .iter()
                .find(|check| check.code == "trust_codex")
                .unwrap()
                .status,
            CheckStatus::Warning
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_runtime_permissions_fail_without_being_repaired() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("runtime");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let check = check_private_directory(&path);
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
