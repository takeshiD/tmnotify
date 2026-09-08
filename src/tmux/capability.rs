use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MAX_TMUX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const TMUX_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const CHILD_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(10);

const REQUIRED_FORMATS: &[&str] = &[
    "client_activity",
    "client_control_mode",
    "client_name",
    "pane_floating_flag",
    "pane_height",
    "pane_id",
    "pane_left",
    "pane_top",
    "pane_width",
    "session_id",
    "socket_path",
    "window_id",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Capability {
    ControlMode,
    FloatingPaneCreation,
    FloatingPaneMovement,
    FloatingPaneResize,
    StableIdsAndTopologyFormats,
    ControlClientIdentification,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityReport {
    pub version: String,
    pub available: BTreeSet<Capability>,
    pub missing: BTreeMap<Capability, String>,
}

impl CapabilityReport {
    pub fn supports_display_service(&self) -> bool {
        self.missing.is_empty()
    }

    pub fn require_display_service(&self) -> Result<(), UnsupportedTmux> {
        if self.supports_display_service() {
            Ok(())
        } else {
            Err(UnsupportedTmux {
                version: self.version.clone(),
                missing: self.missing.clone(),
            })
        }
    }

    #[cfg(test)]
    pub(crate) fn supported_for_tests() -> Self {
        let available = [
            Capability::ControlMode,
            Capability::FloatingPaneCreation,
            Capability::FloatingPaneMovement,
            Capability::FloatingPaneResize,
            Capability::StableIdsAndTopologyFormats,
            Capability::ControlClientIdentification,
        ]
        .into_iter()
        .collect();
        Self {
            version: "test".into(),
            available,
            missing: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsupportedTmux {
    pub version: String,
    pub missing: BTreeMap<Capability, String>,
}

impl fmt::Display for UnsupportedTmux {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "tmux display service is unsupported ({})",
            self.version
        )?;
        for (capability, detail) in &self.missing {
            write!(formatter, "; {capability:?}: {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for UnsupportedTmux {}

#[derive(Clone, Debug)]
pub struct ProductionProbe {
    socket_path: PathBuf,
}

impl ProductionProbe {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn run(&self) -> std::io::Result<CapabilityReport> {
        let version = run_tmux(&self.socket_path, &["-V"])?;
        let commands = run_tmux(&self.socket_path, &["list-commands"])?;
        let formats = run_tmux(&self.socket_path, &["display-message", "-a", "-p"])?;
        let control = run_tmux(
            &self.socket_path,
            &["-C", "display-message", "-p", "tmnotify-control-probe"],
        )?;

        Ok(evaluate_observed(
            version.trim(),
            &commands,
            &formats,
            control.contains("%begin")
                && control.contains("tmnotify-control-probe")
                && control.contains("%end"),
        ))
    }
}

pub(super) fn run_tmux(socket_path: &Path, arguments: &[&str]) -> std::io::Result<String> {
    let mut command = Command::new("tmux");
    command.arg("-S").arg(socket_path).args(arguments);
    run_tmux_command(command)
}

pub(super) fn run_tmux_named(socket_name: &str, arguments: &[&str]) -> std::io::Result<String> {
    if socket_name.is_empty() || socket_name.contains(['\0', '\n', '\r']) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid tmux socket name",
        ));
    }
    let mut command = Command::new("tmux");
    command.arg("-L").arg(socket_name).args(arguments);
    run_tmux_command(command)
}

fn run_tmux_command(mut command: Command) -> std::io::Result<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other("tmux stdout was unavailable"));
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other("tmux stderr was unavailable"));
    };
    let stdout = thread::spawn(move || read_bounded(stdout, MAX_TMUX_OUTPUT_BYTES));
    let stderr = thread::spawn(move || read_bounded(stderr, MAX_TMUX_OUTPUT_BYTES));
    let deadline = Instant::now() + TMUX_COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "tmux command exceeded timeout",
            ));
        }
        thread::sleep(CHILD_STATUS_POLL_INTERVAL);
    };
    let stdout = join_reader(stdout)?;
    let stderr = join_reader(stderr)?;
    checked_utf8(status, stdout, stderr)
}

fn read_bounded(mut reader: impl Read, maximum: usize) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(maximum.min(8 * 1024));
    reader
        .by_ref()
        .take(
            u64::try_from(maximum)
                .unwrap_or(u64::MAX - 1)
                .saturating_add(1),
        )
        .read_to_end(&mut output)?;
    if output.len() > maximum {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "tmux command output exceeded limit",
        ));
    }
    Ok(output)
}

fn join_reader(reader: thread::JoinHandle<std::io::Result<Vec<u8>>>) -> std::io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| std::io::Error::other("tmux output reader panicked"))?
}

fn checked_utf8(status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> std::io::Result<String> {
    if !status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&stderr).trim().to_owned(),
        ));
    }
    String::from_utf8(stdout)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub(super) fn evaluate_observed(
    version: &str,
    commands: &str,
    formats: &str,
    control_confirmed: bool,
) -> CapabilityReport {
    let command_flags = parse_command_flags(commands);
    let format_names = parse_format_names(formats);
    let mut report = CapabilityReport {
        version: version.to_owned(),
        available: BTreeSet::new(),
        missing: BTreeMap::new(),
    };

    check(
        &mut report,
        Capability::ControlMode,
        control_confirmed,
        "a -C command did not produce correlated %begin/%end framing",
    );
    check_flags(
        &mut report,
        &command_flags,
        Capability::FloatingPaneCreation,
        "break-pane",
        "WdstxyXY",
    );
    check_flags(
        &mut report,
        &command_flags,
        Capability::FloatingPaneMovement,
        "move-pane",
        "tDLRUz",
    );
    check_flags(
        &mut report,
        &command_flags,
        Capability::FloatingPaneResize,
        "resize-pane",
        "txy",
    );
    check(
        &mut report,
        Capability::StableIdsAndTopologyFormats,
        REQUIRED_FORMATS
            .iter()
            .filter(|name| **name != "client_control_mode")
            .all(|name| format_names.contains(*name)),
        "one or more required stable ID, geometry, socket, or client formats are absent",
    );
    check(
        &mut report,
        Capability::ControlClientIdentification,
        format_names.contains("client_control_mode"),
        "client_control_mode format is absent",
    );
    report
}

fn check_flags(
    report: &mut CapabilityReport,
    commands: &BTreeMap<String, BTreeSet<char>>,
    capability: Capability,
    command: &str,
    required: &str,
) {
    let actual = commands.get(command);
    let missing: String = required
        .chars()
        .filter(|flag| actual.is_none_or(|flags| !flags.contains(flag)))
        .collect();
    check(
        report,
        capability,
        missing.is_empty(),
        &format!("{command} is absent or lacks flags -{missing}"),
    );
}

fn check(report: &mut CapabilityReport, capability: Capability, ok: bool, detail: &str) {
    if ok {
        report.available.insert(capability);
    } else {
        report.missing.insert(capability, detail.to_owned());
    }
}

fn parse_command_flags(commands: &str) -> BTreeMap<String, BTreeSet<char>> {
    commands
        .lines()
        .filter_map(|line| {
            let (name, _) = line.split_once(' ')?;
            let mut flags = BTreeSet::new();
            for group in line
                .split_whitespace()
                .filter(|part| part.starts_with("[-"))
            {
                flags.extend(
                    group
                        .trim_start_matches('[')
                        .trim_start_matches('-')
                        .chars()
                        .take_while(|character| character.is_ascii_alphabetic()),
                );
            }
            Some((name.to_owned(), flags))
        })
        .collect()
}

fn parse_format_names(formats: &str) -> BTreeSet<&str> {
    formats
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMANDS: &str = "break-pane (breakp) [-abdPW] [-F format] [-s src-pane] [-t dst-window] [-x width] [-y height] [-X x-position] [-Y y-position]\nmove-pane (movep) [-bdfhMv] [-D lines] [-L columns] [-R columns] [-t dst-pane] [-U lines] [-X x-position] [-Y y-position] [-z z-index]\nresize-pane (resizep) [-MTZ] [-x width] [-y height] [-t target-pane]\n";

    fn formats() -> String {
        REQUIRED_FORMATS
            .iter()
            .map(|name| format!("{name}=value\n"))
            .collect()
    }

    #[test]
    fn accepts_the_observed_tmux_3_8_surface() {
        let formats = formats();
        let report = evaluate_observed("tmux next-3.8", COMMANDS, &formats, true);

        assert!(report.supports_display_service(), "{:?}", report.missing);
        assert!(report.require_display_service().is_ok());
    }

    #[test]
    fn reports_each_missing_capability_without_using_the_version_string() {
        let report = evaluate_observed(
            "tmux 99.0",
            "resize-pane [-t target-pane]\n",
            "pane_id=%1\n",
            false,
        );
        let error = report.require_display_service().unwrap_err();

        assert_eq!(error.missing.len(), 6);
        assert!(error.missing.contains_key(&Capability::ControlMode));
        assert!(
            error
                .missing
                .contains_key(&Capability::ControlClientIdentification)
        );
    }

    #[test]
    fn command_flag_parser_handles_multiple_option_groups() {
        let flags = parse_command_flags(COMMANDS);
        assert!(flags["break-pane"].is_superset(&['W', 'x', 'y', 'X', 'Y'].into()));
        assert!(flags["move-pane"].is_superset(&['D', 'L', 'R', 'U', 'z'].into()));
    }

    #[test]
    fn probe_path_is_passed_as_an_argument_not_shell_text() {
        let probe = ProductionProbe::new(std::path::Path::new("/tmp/socket;not-a-command"));
        assert_eq!(
            probe.socket_path,
            std::path::Path::new("/tmp/socket;not-a-command")
        );
    }

    #[test]
    fn child_output_reader_accepts_limit_and_rejects_one_byte_beyond() {
        assert_eq!(read_bounded(&b"abcd"[..], 4).unwrap(), b"abcd");
        let error = read_bounded(&b"abcde"[..], 4).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
