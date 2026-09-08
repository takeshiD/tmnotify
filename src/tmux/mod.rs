//! Plan-oriented boundary to tmux.
//!
//! tmux command syntax and control-mode records intentionally stay in this
//! module. Callers describe desired displays and consume topology changes; they
//! do not construct tmux commands or depend on control-mode notification names.

mod capability;
mod control;
mod production;
mod topology;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::notification::{IngressError, SourceContext, TmuxServerId};
use crate::protocol::RendererContent;

pub use capability::{Capability, CapabilityReport, ProductionProbe, UnsupportedTmux};
pub use production::{DEFAULT_TOPOLOGY_REFRESH_INTERVAL, ProductionBackend};
pub use topology::{ClientView, Pane, Topology, TopologyParseError, WindowSize};

/// A stable tmux window ID (`@N`).
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WindowId(pub String);

/// A stable tmux pane ID (`%N`).
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PaneId(pub String);

/// Desired displays keyed by Attention Window.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DisplayPlan {
    pub windows: BTreeMap<WindowId, Vec<PlannedDisplay>>,
}

/// The renderer identity and geometry desired in one Attention Window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedDisplay {
    pub display_id: String,
    pub kind: DisplayKind,
    pub geometry: Geometry,
    /// Normalized content delivered only through the authenticated daemon
    /// socket. The tmux implementation must never place it in argv or env.
    pub content: RendererContent,
    /// Follow/retry recreations must not replay a Notification's entrance.
    pub play_enter_animation: bool,
}

/// Per-window result of applying a desired plan. The backend owns the actual
/// pane identities and command diff; the daemon only reasons about windows.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReconcileReport {
    pub applied: BTreeSet<WindowId>,
    pub failed: BTreeMap<WindowId, String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayKind {
    Toast,
    Attention,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub z_index: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JumpTarget {
    pub pane_id: PaneId,
    pub likely_client: Option<String>,
}

/// Product-level events emitted by the tmux boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    /// Client, session, window, pane, or layout state may have changed.
    TopologyChanged,
    /// The control connection ended. Server loss must be confirmed separately.
    Disconnected,
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Protocol(String),
    Unsupported(UnsupportedTmux),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "tmux I/O failed: {error}"),
            Self::Protocol(message) => write!(formatter, "invalid tmux response: {message}"),
            Self::Unsupported(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Unsupported(error) => Some(error),
            Self::Protocol(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<TopologyParseError> for Error {
    fn from(error: TopologyParseError) -> Self {
        Self::Protocol(error.to_string())
    }
}

/// Deep seam used by the daemon. Implementations own tmux protocol mechanics.
pub trait Backend {
    fn capabilities(&mut self) -> Result<CapabilityReport, Error>;
    fn topology(&mut self) -> Result<Topology, Error>;
    fn next_event(&mut self) -> Result<Option<Event>, Error>;
    fn reconcile(&mut self, desired: &DisplayPlan) -> Result<ReconcileReport, Error>;
    fn jump(&mut self, target: &JumpTarget) -> Result<(), Error>;
}

/// Location of a tmux server selected by `-S` or resolved from `$TMUX`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Server {
    socket_path: PathBuf,
}

impl Server {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// Resolves a tmux `-L` name to the server's own socket path. The name is
    /// passed as one argv item and is never interpreted by a shell.
    pub fn from_socket_name(socket_name: &str) -> Result<Self, Error> {
        let output =
            capability::run_tmux_named(socket_name, &["display-message", "-p", "#{socket_path}"])?;
        let path = output.trim_end_matches(['\r', '\n']);
        if path.is_empty() || path.contains(['\0', '\n', '\r']) {
            return Err(Error::Protocol(
                "tmux returned an invalid canonical socket path".into(),
            ));
        }
        Ok(Self::new(path))
    }

    pub fn probe(&self) -> Result<CapabilityReport, Error> {
        ProductionProbe::new(&self.socket_path)
            .run()
            .map_err(Error::from)
    }

    #[must_use]
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    /// Takes a full topology snapshot using formats owned by this module.
    pub fn topology(&self) -> Result<Topology, Error> {
        let clients = capability::run_tmux(
            &self.socket_path,
            &[
                "list-clients",
                "-F",
                "#{client_name}\t#{client_control_mode}\t#{session_id}\t#{window_id}\t#{client_activity}",
            ],
        )?;
        let panes = capability::run_tmux(
            &self.socket_path,
            &[
                "list-panes",
                "-a",
                "-F",
                "#{pane_id}\t#{session_id}\t#{window_id}\t#{pane_floating_flag}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}",
            ],
        )?;
        Topology::from_format_output(&clients, &panes).map_err(Error::from)
    }

    /// Captures only the bounded Source Pane fields in the product contract.
    /// The caller supplies the stable pane ID from `TMUX_PANE`; no current pane
    /// is guessed for an outside-tmux invocation.
    pub fn source_context(
        &self,
        server_id: TmuxServerId,
        pane_id: &str,
    ) -> Result<SourceContext, SourceCaptureError> {
        validate_source_pane_id(pane_id)?;
        let stable = capability::run_tmux(
            &self.socket_path,
            &[
                "display-message",
                "-t",
                pane_id,
                "-p",
                "#{session_id}\t#{window_id}\t#{pane_id}",
            ],
        )?;
        let fields = stable
            .trim_end_matches(['\r', '\n'])
            .split('\t')
            .collect::<Vec<_>>();
        if fields.len() != 3 || fields[2] != pane_id {
            return Err(SourceCaptureError::InvalidResponse);
        }
        let mut source = SourceContext::new(server_id, fields[0], fields[1], fields[2])?;
        for (format, kind) in [
            ("#{pane_current_path}", SourceField::Cwd),
            ("#{pane_current_command}", SourceField::Command),
            ("#{pane_title}", SourceField::Title),
        ] {
            let value = capability::run_tmux(
                &self.socket_path,
                &["display-message", "-t", pane_id, "-p", format],
            )?;
            let value = value.trim_end_matches(['\r', '\n']);
            if value.is_empty() {
                continue;
            }
            source = match kind {
                SourceField::Cwd => source.with_cwd(value)?,
                SourceField::Command => source.with_command(value)?,
                SourceField::Title => source.with_pane_title(value)?,
            };
        }
        Ok(source)
    }

    /// Starts the interactive History viewer in a focused floating pane.
    /// tmux receives the executable and fixed arguments as distinct argv
    /// values; no user or History text crosses a shell-command boundary.
    pub fn launch_history_ui(
        &self,
        window_id: &str,
        executable: &Path,
        all_servers: bool,
        include_hidden: bool,
    ) -> Result<(), Error> {
        validate_stable_window_id(window_id)?;
        let executable = executable
            .to_str()
            .filter(|value| !value.contains(['\0', '\n', '\r']))
            .ok_or_else(|| Error::Protocol("History executable path is not valid UTF-8".into()))?;
        if !executable.starts_with('/') {
            return Err(Error::Protocol(
                "History executable path must be absolute".into(),
            ));
        }

        let window = WindowId(window_id.to_owned());
        let size = self
            .topology()?
            .window_size(&window)
            .ok_or_else(|| Error::Protocol("History target window no longer exists".into()))?;
        let width = u16::try_from(u32::from(size.width).saturating_mul(4) / 5)
            .unwrap_or(size.width)
            .max(1);
        let height = u16::try_from(u32::from(size.height).saturating_mul(4) / 5)
            .unwrap_or(size.height)
            .max(1);
        let create =
            history_ui_create_arguments(window_id, executable, all_servers, include_hidden);
        let output = run_owned_tmux(self.socket_path(), &create)?;
        let pane_id = output.trim_end_matches(['\r', '\n']);
        validate_source_pane_id(pane_id)
            .map_err(|_| Error::Protocol("tmux did not return a stable History pane ID".into()))?;

        let floating = vec![
            "break-pane".to_owned(),
            "-W".to_owned(),
            "-d".to_owned(),
            "-s".to_owned(),
            pane_id.to_owned(),
            "-t".to_owned(),
            window_id.to_owned(),
            "-X".to_owned(),
            size.width
                .saturating_sub(width)
                .checked_div(2)
                .unwrap_or(0)
                .to_string(),
            "-Y".to_owned(),
            size.height
                .saturating_sub(height)
                .checked_div(2)
                .unwrap_or(0)
                .to_string(),
            "-x".to_owned(),
            width.to_string(),
            "-y".to_owned(),
            height.to_string(),
        ];
        if let Err(error) = run_owned_tmux(self.socket_path(), &floating) {
            let _ = capability::run_tmux(self.socket_path(), &["kill-pane", "-t", pane_id]);
            return Err(error);
        }
        capability::run_tmux(self.socket_path(), &["select-pane", "-t", pane_id])?;
        Ok(())
    }
}

fn history_ui_create_arguments(
    window_id: &str,
    executable: &str,
    all_servers: bool,
    include_hidden: bool,
) -> Vec<String> {
    let mut arguments = vec![
        "split-window".to_owned(),
        "-d".to_owned(),
        "-P".to_owned(),
        "-F".to_owned(),
        "#{pane_id}".to_owned(),
        "-t".to_owned(),
        window_id.to_owned(),
        executable.to_owned(),
        "__history-ui".to_owned(),
    ];
    if all_servers {
        arguments.push("--all-servers".to_owned());
    }
    if include_hidden {
        arguments.push("--include-hidden".to_owned());
    }
    arguments
}

fn run_owned_tmux(socket_path: &Path, arguments: &[String]) -> Result<String, Error> {
    let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    capability::run_tmux(socket_path, &arguments).map_err(Error::from)
}

fn validate_stable_window_id(value: &str) -> Result<(), Error> {
    if value
        .strip_prefix('@')
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
    {
        Ok(())
    } else {
        Err(Error::Protocol("invalid stable tmux window ID".into()))
    }
}

#[derive(Clone, Copy)]
enum SourceField {
    Cwd,
    Command,
    Title,
}

fn validate_source_pane_id(value: &str) -> Result<(), SourceCaptureError> {
    if value
        .strip_prefix('%')
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
    {
        Ok(())
    } else {
        Err(SourceCaptureError::InvalidPaneId)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SourceCaptureError {
    #[error("TMUX_PANE is not a stable tmux pane ID")]
    InvalidPaneId,
    #[error("tmux returned invalid Source Pane metadata")]
    InvalidResponse,
    #[error(transparent)]
    Tmux(#[from] std::io::Error),
    #[error(transparent)]
    Ingress(#[from] IngressError),
}

#[cfg(test)]
pub(crate) mod fake {
    use crate::notification::{Notification, NotificationDraft, Presentation};
    use chrono::Utc;
    use std::collections::VecDeque;

    use super::*;

    fn content() -> RendererContent {
        let draft = NotificationDraft::new(
            Presentation::Attention,
            "test",
            "body",
            Some(
                crate::notification::SourceContext::new(
                    crate::notification::TmuxServerId::new("server").unwrap(),
                    "$1",
                    "@2",
                    "%3",
                )
                .unwrap(),
            ),
        )
        .unwrap();
        RendererContent::from(&Notification::from_draft(draft, Utc::now()))
    }

    #[derive(Debug)]
    pub(crate) struct FakeTmux {
        pub capabilities: CapabilityReport,
        pub topology: Topology,
        pub events: VecDeque<Event>,
        pub plans: Vec<DisplayPlan>,
        pub reconcile_results: VecDeque<Result<ReconcileReport, String>>,
        pub jumps: Vec<JumpTarget>,
    }

    impl FakeTmux {
        pub(crate) fn supported(topology: Topology) -> Self {
            Self {
                capabilities: CapabilityReport::supported_for_tests(),
                topology,
                events: VecDeque::new(),
                plans: Vec::new(),
                reconcile_results: VecDeque::new(),
                jumps: Vec::new(),
            }
        }
    }

    impl Backend for FakeTmux {
        fn capabilities(&mut self) -> Result<CapabilityReport, Error> {
            Ok(self.capabilities.clone())
        }

        fn topology(&mut self) -> Result<Topology, Error> {
            Ok(self.topology.clone())
        }

        fn next_event(&mut self) -> Result<Option<Event>, Error> {
            Ok(self.events.pop_front())
        }

        fn reconcile(&mut self, desired: &DisplayPlan) -> Result<ReconcileReport, Error> {
            self.plans.push(desired.clone());
            if let Some(result) = self.reconcile_results.pop_front() {
                return result.map_err(Error::Protocol);
            }
            Ok(ReconcileReport {
                applied: desired.windows.keys().cloned().collect(),
                failed: BTreeMap::new(),
            })
        }

        fn jump(&mut self, target: &JumpTarget) -> Result<(), Error> {
            self.jumps.push(target.clone());
            Ok(())
        }
    }

    #[test]
    fn fake_records_plans_and_jumps_without_protocol_details() {
        let mut tmux = FakeTmux::supported(Topology::default());
        let mut plan = DisplayPlan::default();
        plan.windows.insert(
            WindowId("@4".into()),
            vec![PlannedDisplay {
                display_id: "display-1".into(),
                kind: DisplayKind::Attention,
                geometry: Geometry {
                    x: 2,
                    y: 3,
                    width: 40,
                    height: 9,
                    z_index: 5,
                },
                content: content(),
                play_enter_animation: true,
            }],
        );
        let jump = JumpTarget {
            pane_id: PaneId("%9".into()),
            likely_client: Some("/dev/pts/7".into()),
        };

        tmux.reconcile(&plan).unwrap();
        tmux.jump(&jump).unwrap();

        assert_eq!(tmux.plans, vec![plan]);
        assert_eq!(tmux.jumps, vec![jump]);
    }

    #[test]
    fn history_ui_launch_keeps_executable_and_flags_as_distinct_arguments() {
        assert_eq!(
            history_ui_create_arguments("@4", "/tmp/bin with space/tmnotify", true, true),
            vec![
                "split-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                "@4",
                "/tmp/bin with space/tmnotify",
                "__history-ui",
                "--all-servers",
                "--include-hidden",
            ]
        );
    }
}
