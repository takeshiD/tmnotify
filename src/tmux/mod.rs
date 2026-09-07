//! Plan-oriented boundary to tmux.
//!
//! tmux command syntax and control-mode records intentionally stay in this
//! module. Callers describe desired displays and consume topology changes; they
//! do not construct tmux commands or depend on control-mode notification names.

mod capability;
// The persistent connection will consume this codec in the daemon integration
// change. Keep raw control-mode framing private to the tmux boundary meanwhile.
#[allow(dead_code)]
mod control;
mod topology;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

pub use capability::{Capability, CapabilityReport, ProductionProbe, UnsupportedTmux};
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

    pub fn probe(&self) -> Result<CapabilityReport, Error> {
        ProductionProbe::new(&self.socket_path)
            .run()
            .map_err(Error::from)
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
}

#[cfg(test)]
pub(crate) mod fake {
    use std::collections::VecDeque;

    use super::*;

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
}
