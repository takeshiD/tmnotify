use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
};
use std::thread;
use std::time::{Duration, Instant};

use crate::protocol::RendererTermination;
use crate::render::{
    RendererLaunch, RendererSessions, SessionError, SessionLimits, WindowDisplayId,
};

use super::control::{CommandResult, CommandTicket, ControlItem, ControlParser};
use super::{
    Backend, CapabilityReport, DisplayKind, DisplayPlan, Error, Event, Geometry, JumpTarget,
    PaneId, PlannedDisplay, ReconcileReport, Server, Topology, WindowId,
};

pub const DEFAULT_TOPOLOGY_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONTROL_LINE_BYTES: usize = 256 * 1024;
const CONTROL_LINE_CHANNEL_CAPACITY: usize = 256;
const CONTROL_CHILD_SHUTDOWN_WAIT: Duration = Duration::from_millis(100);
const CONTROL_CHILD_STATUS_POLL: Duration = Duration::from_millis(5);

/// The production tmux boundary for one daemon.
///
/// It owns exactly one persistent control-mode child at a time. A disconnected
/// child is replaced lazily by `topology`, which is how the daemon's bounded
/// reconnect loop performs a full reconciliation without creating observers in
/// renderer processes.
pub struct ProductionBackend {
    server: Server,
    executable: PathBuf,
    connection: Option<ControlConnection>,
    displays: BTreeMap<String, ActualDisplay>,
    renderers: Arc<Mutex<RendererSessions>>,
    refresh_interval: Duration,
    next_refresh: Instant,
}

#[derive(Clone, Debug)]
struct ActualDisplay {
    pane_id: PaneId,
    window_id: WindowId,
    kind: DisplayKind,
    geometry: Geometry,
    content: crate::protocol::RendererContent,
}

impl ProductionBackend {
    pub fn connect(server: Server, executable: impl Into<PathBuf>) -> Result<Self, Error> {
        Self::with_refresh_interval(server, executable, DEFAULT_TOPOLOGY_REFRESH_INTERVAL)
    }

    pub fn with_refresh_interval(
        server: Server,
        executable: impl Into<PathBuf>,
        refresh_interval: Duration,
    ) -> Result<Self, Error> {
        let executable = executable.into();
        if !executable.is_absolute() {
            return Err(Error::Protocol(
                "renderer executable path must be absolute".into(),
            ));
        }
        if executable.to_str().is_none() {
            return Err(Error::Protocol(
                "renderer executable path must be valid UTF-8".into(),
            ));
        }
        if refresh_interval.is_zero() {
            return Err(Error::Protocol(
                "topology refresh interval must be positive".into(),
            ));
        }
        let renderers = RendererSessions::new(SessionLimits::default())
            .map_err(|error| Error::Protocol(error.to_string()))?;
        let connection = ControlConnection::connect(server.socket_path())?;
        Ok(Self {
            server,
            executable,
            connection: Some(connection),
            displays: BTreeMap::new(),
            renderers: Arc::new(Mutex::new(renderers)),
            refresh_interval,
            next_refresh: Instant::now() + refresh_interval,
        })
    }

    /// Gives the daemon renderer broker access to the credentials owned by
    /// this backend. The broker still streams content over its private socket;
    /// tmux only receives the opaque launch ID and token.
    pub fn renderer_sessions(&self) -> Arc<Mutex<RendererSessions>> {
        Arc::clone(&self.renderers)
    }

    fn ensure_connection(&mut self) -> Result<&mut ControlConnection, Error> {
        if self.connection.is_none() {
            self.connection = Some(ControlConnection::connect(self.server.socket_path())?);
            self.next_refresh = Instant::now() + self.refresh_interval;
        }
        Ok(self
            .connection
            .as_mut()
            .expect("connection was initialized"))
    }

    fn command(&mut self, commands: &[Vec<String>]) -> Result<CommandResult, Error> {
        let result = self.ensure_connection()?.command(commands);
        if result.is_err() {
            self.connection = None;
        }
        result
    }

    fn snapshot(&mut self) -> Result<Topology, Error> {
        let clients = self.command(&[strings(&[
            "list-clients",
            "-F",
            "#{client_name}\t#{client_control_mode}\t#{session_id}\t#{window_id}\t#{client_activity}",
        ])])?;
        require_success(&clients)?;
        let panes = self.command(&[strings(&[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{session_id}\t#{window_id}\t#{pane_floating_flag}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}",
        ])])?;
        require_success(&panes)?;
        let topology =
            Topology::from_format_output(&clients.output.join("\n"), &panes.output.join("\n"))
                .map_err(Error::from)?;
        self.prune_missing_displays(&topology);
        Ok(topology)
    }

    fn reconcile_window(
        &mut self,
        window: &WindowId,
        desired: &[PlannedDisplay],
    ) -> Result<(), Error> {
        validate_stable_id(&window.0, '@')?;
        let desired_ids: BTreeSet<_> = desired
            .iter()
            .map(|item| item.display_id.as_str())
            .collect();
        let obsolete = self
            .displays
            .iter()
            .filter(|(id, actual)| {
                actual.window_id == *window && !desired_ids.contains(id.as_str())
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        self.close_displays(&obsolete, RendererTermination::Dismissed)?;

        for display in desired {
            validate_display_id(&display.display_id)?;
            let recreate = self
                .displays
                .get(&display.display_id)
                .is_some_and(|actual| actual.window_id != *window || actual.kind != display.kind);
            if recreate {
                self.close_displays(
                    std::slice::from_ref(&display.display_id),
                    RendererTermination::Recreated,
                )?;
            }
            if let Some(actual) = self.displays.get(&display.display_id).cloned() {
                self.update_display(&display.display_id, &actual, display)?;
            } else {
                self.create_display(window, display)?;
            }
        }
        Ok(())
    }

    fn create_display(&mut self, window: &WindowId, desired: &PlannedDisplay) -> Result<(), Error> {
        let display_id = WindowDisplayId::new(desired.display_id.clone())
            .map_err(|error| Error::Protocol(error.to_string()))?;
        let launch = self
            .renderers
            .lock()
            .map_err(|_| Error::Protocol("renderer session lock is poisoned".into()))?
            .create(display_id.clone(), desired.content.clone(), Instant::now())
            .map_err(session_error)?;
        let renderer_mode = match desired.kind {
            DisplayKind::Toast => "__render-toast",
            DisplayKind::Attention => "__render-attention",
        };
        let mut create = vec![
            "split-window".into(),
            "-d".into(),
            "-P".into(),
            "-F".into(),
            "#{pane_id}".into(),
            "-t".into(),
            window.0.clone(),
        ];
        let renderer_argv = match self.renderer_argv(renderer_mode, &launch) {
            Ok(arguments) => arguments,
            Err(error) => {
                self.terminate_renderer(&display_id, RendererTermination::RenderFailed);
                return Err(error);
            }
        };
        create.extend(renderer_argv);
        let result = match self.command(&[create]) {
            Ok(result) => result,
            Err(error) => {
                self.terminate_renderer(&display_id, RendererTermination::RenderFailed);
                return Err(error);
            }
        };
        if let Err(error) = require_success(&result) {
            self.terminate_renderer(&display_id, RendererTermination::RenderFailed);
            return Err(error);
        }
        let pane = result
            .output
            .iter()
            .rev()
            .find(|line| validate_stable_id(line, '%').is_ok())
            .cloned();
        let Some(pane) = pane else {
            self.terminate_renderer(&display_id, RendererTermination::RenderFailed);
            return Err(Error::Protocol(
                "split-window did not return a pane ID".into(),
            ));
        };
        let pane_id = PaneId(pane);
        let mut commands = vec![break_floating_command(&pane_id, window, desired.geometry)];
        if desired.kind == DisplayKind::Attention {
            commands.push(vec!["select-pane".into(), "-t".into(), pane_id.0.clone()]);
        }
        let result = match self.command(&commands) {
            Ok(result) => result,
            Err(error) => {
                let _ = self.command(&[vec!["kill-pane".into(), "-t".into(), pane_id.0.clone()]]);
                self.terminate_renderer(&display_id, RendererTermination::RenderFailed);
                return Err(error);
            }
        };
        if let Err(error) = require_success(&result) {
            let _ = self.command(&[vec!["kill-pane".into(), "-t".into(), pane_id.0.clone()]]);
            self.terminate_renderer(&display_id, RendererTermination::RenderFailed);
            return Err(error);
        }
        self.displays.insert(
            desired.display_id.clone(),
            ActualDisplay {
                pane_id,
                window_id: window.clone(),
                kind: desired.kind,
                geometry: desired.geometry,
                content: desired.content.clone(),
            },
        );
        Ok(())
    }

    fn renderer_argv(&self, mode: &str, launch: &RendererLaunch) -> Result<Vec<String>, Error> {
        // tmux 3.8's split-window accepts `shell-command [argument ...]` and
        // calls execvp when more than one argument is supplied. Preserve that
        // argv boundary: notification/provider text never enters the command.
        let executable = self
            .executable
            .to_str()
            .ok_or_else(|| Error::Protocol("renderer executable path is not UTF-8".into()))?;
        Ok(vec![
            executable.to_owned(),
            mode.to_owned(),
            "--window-display".to_owned(),
            launch.window_display_id().as_str().to_owned(),
            "--token".to_owned(),
            launch.token().expose_secret().to_owned(),
        ])
    }

    fn update_display(
        &mut self,
        id: &str,
        actual: &ActualDisplay,
        desired: &PlannedDisplay,
    ) -> Result<(), Error> {
        validate_stable_id(&actual.pane_id.0, '%')?;
        let mut commands = Vec::new();
        if actual.geometry.width != desired.geometry.width
            || actual.geometry.height != desired.geometry.height
        {
            let (width, height) = floating_resize_size(desired.geometry);
            commands.push(vec![
                "resize-pane".into(),
                "-t".into(),
                actual.pane_id.0.clone(),
                "-x".into(),
                width.to_string(),
                "-y".into(),
                height.to_string(),
            ]);
        }
        append_relative_moves(
            &mut commands,
            &actual.pane_id,
            actual.geometry,
            desired.geometry,
        );
        if actual.geometry.z_index != desired.geometry.z_index {
            commands.push(vec![
                "move-pane".into(),
                "-t".into(),
                actual.pane_id.0.clone(),
                "-z".into(),
                desired.geometry.z_index.to_string(),
            ]);
        }
        if !commands.is_empty() {
            let result = self.command(&commands)?;
            require_success(&result)?;
            self.displays
                .get_mut(id)
                .expect("updated display remains registered")
                .geometry = desired.geometry;
        }
        if actual.content != desired.content {
            self.renderers
                .lock()
                .map_err(|_| Error::Protocol("renderer session lock is poisoned".into()))?
                .update(
                    &WindowDisplayId::new(id.to_owned()).map_err(session_error)?,
                    desired.content.clone(),
                )
                .map_err(session_error)?;
            self.displays
                .get_mut(id)
                .expect("updated display remains registered")
                .content = desired.content.clone();
        }
        Ok(())
    }

    fn close_displays(&mut self, ids: &[String], reason: RendererTermination) -> Result<(), Error> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut commands = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(actual) = self.displays.get(id) {
                commands.push(vec![
                    "kill-pane".into(),
                    "-t".into(),
                    actual.pane_id.0.clone(),
                ]);
            }
        }
        if !commands.is_empty() {
            let result = self.command(&commands)?;
            require_success(&result)?;
        }
        for id in ids {
            if self.displays.remove(id).is_some() {
                let display_id = WindowDisplayId::new(id.clone())
                    .map_err(|error| Error::Protocol(error.to_string()))?;
                self.terminate_renderer(&display_id, reason);
            }
        }
        Ok(())
    }

    fn prune_missing_displays(&mut self, topology: &Topology) {
        let missing = self
            .displays
            .iter()
            .filter(|(_, actual)| !topology.panes.contains_key(&actual.pane_id))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in missing {
            self.displays.remove(&id);
            if let Ok(display_id) = WindowDisplayId::new(id) {
                self.terminate_renderer(&display_id, RendererTermination::RenderFailed);
            }
        }
    }

    fn close_attention_before_jump(&mut self) -> Result<(), Error> {
        let ids = self
            .displays
            .iter()
            .filter(|(_, display)| display.kind == DisplayKind::Attention)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        self.close_displays(&ids, RendererTermination::Jumped)
    }

    fn terminate_renderer(&self, id: &WindowDisplayId, reason: RendererTermination) {
        if let Ok(mut sessions) = self.renderers.lock() {
            let _ = sessions.terminate(id, reason);
        }
    }
}

impl Backend for ProductionBackend {
    fn capabilities(&mut self) -> Result<CapabilityReport, Error> {
        // Probe command/format support through the already-owned persistent
        // connection. This avoids creating a second control observer merely to
        // prove the connection that is currently carrying correlated replies.
        let version = super::capability::run_tmux(self.server.socket_path(), &["-V"])?;
        let commands = self.command(&[strings(&["list-commands"])])?;
        require_success(&commands)?;
        let formats = self.command(&[strings(&["display-message", "-a", "-p"])])?;
        require_success(&formats)?;
        let report = super::capability::evaluate_observed(
            version.trim(),
            &commands.output.join("\n"),
            &formats.output.join("\n"),
            true,
        );
        report
            .require_display_service()
            .map_err(Error::Unsupported)?;
        Ok(report)
    }

    fn topology(&mut self) -> Result<Topology, Error> {
        let topology = self.snapshot()?;
        self.next_refresh = Instant::now() + self.refresh_interval;
        Ok(topology)
    }

    fn next_event(&mut self) -> Result<Option<Event>, Error> {
        if Instant::now() >= self.next_refresh {
            self.next_refresh = Instant::now() + self.refresh_interval;
            return Ok(Some(Event::TopologyChanged));
        }
        let Some(connection) = &mut self.connection else {
            return Ok(Some(Event::Disconnected));
        };
        match connection.next_event() {
            Ok(Some(Event::Disconnected)) => {
                self.connection = None;
                Ok(Some(Event::Disconnected))
            }
            Ok(event) => Ok(event),
            Err(error) => {
                self.connection = None;
                Ok(Some(match error {
                    Error::Io(_) | Error::Protocol(_) | Error::Unsupported(_) => {
                        Event::Disconnected
                    }
                }))
            }
        }
    }

    fn reconcile(&mut self, desired: &DisplayPlan) -> Result<ReconcileReport, Error> {
        let desired_windows: BTreeSet<_> = desired.windows.keys().cloned().collect();
        let mut report = ReconcileReport::default();
        let mut stale_by_window = BTreeMap::<WindowId, Vec<String>>::new();
        for (id, actual) in &self.displays {
            if !desired_windows.contains(&actual.window_id) {
                stale_by_window
                    .entry(actual.window_id.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        for (window, ids) in stale_by_window {
            if let Err(error) = self.close_displays(&ids, RendererTermination::Dismissed) {
                if self.connection.is_none() {
                    return Err(error);
                }
                report.failed.insert(window, error.to_string());
            }
        }

        for (window, displays) in &desired.windows {
            match self.reconcile_window(window, displays) {
                Ok(()) => {
                    report.applied.insert(window.clone());
                }
                Err(error) => {
                    if self.connection.is_none() {
                        return Err(error);
                    }
                    report.failed.insert(window.clone(), error.to_string());
                }
            }
        }
        Ok(report)
    }

    fn jump(&mut self, target: &JumpTarget) -> Result<(), Error> {
        validate_stable_id(&target.pane_id.0, '%')?;
        let topology = self.snapshot()?;
        let pane = topology
            .panes
            .get(&target.pane_id)
            .ok_or_else(|| Error::Protocol("Source Pane no longer exists".into()))?;
        let client = target
            .likely_client
            .as_deref()
            .and_then(|name| {
                topology
                    .clients
                    .iter()
                    .find(|client| !client.is_control && client.name == name)
            })
            .or_else(|| {
                topology
                    .clients
                    .iter()
                    .filter(|client| !client.is_control)
                    .max_by_key(|client| client.last_activity)
            })
            .ok_or_else(|| Error::Protocol("no attached tmux client can perform jump".into()))?;
        let client_name = validate_client_name(&client.name)?;
        let session_id = pane.session_id.clone();
        let window_id = pane.window_id.0.clone();
        let pane_id = pane.id.0.clone();

        // A modal Attention pane must be gone before focus is moved to the
        // underlying Source Pane. Keep these as two correlated acknowledgements
        // so selection is never attempted while the modal still owns input.
        self.close_attention_before_jump()?;
        let result = self.command(&jump_commands(client_name, session_id, window_id, pane_id))?;
        require_success(&result)
    }
}

struct ControlConnection {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Result<String, std::io::Error>>,
    parser: ControlParser,
    next_ticket: u64,
    results: HashMap<CommandTicket, CommandResult>,
    events: VecDeque<Event>,
}

impl ControlConnection {
    fn connect(socket_path: &Path) -> Result<Self, Error> {
        let mut child = Command::new("tmux")
            .arg("-S")
            .arg(socket_path)
            .arg("-C")
            .arg("attach-session")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Protocol("tmux control stdin was unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("tmux control stdout was unavailable".into()))?;
        let (sender, lines) = mpsc::sync_channel(CONTROL_LINE_CHANNEL_CAPACITY);
        thread::Builder::new()
            .name("tmnotify-tmux-control".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_bounded_control_line(&mut reader, MAX_CONTROL_LINE_BYTES) {
                        Ok(None) => break,
                        Ok(Some(line)) => {
                            if sender.send(Ok(line)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            break;
                        }
                    }
                }
            })?;
        let mut connection = Self {
            child,
            stdin,
            lines,
            parser: ControlParser::default(),
            next_ticket: 1,
            results: HashMap::new(),
            events: VecDeque::new(),
        };
        // `attach-session` itself is command zero on the long-lived control
        // client. Consume its correlated response before accepting daemon work.
        connection.parser.submitted(CommandTicket(0));
        let startup = connection.wait_for(CommandTicket(0))?;
        require_success(&startup)?;
        Ok(connection)
    }

    fn command(&mut self, commands: &[Vec<String>]) -> Result<CommandResult, Error> {
        if commands.is_empty() {
            return Err(Error::Protocol("empty tmux command batch".into()));
        }
        let encoded = encode_batch(commands)?;
        // tmux emits one %begin/%end pair per command even when commands are
        // written in one semicolon-separated control-mode batch. Register and
        // drain every pair before returning, otherwise a later snapshot can
        // consume the tail of the previous batch as its own response.
        let tickets = commands
            .iter()
            .map(|_| {
                let ticket = CommandTicket(self.next_ticket);
                self.next_ticket = self.next_ticket.wrapping_add(1).max(1);
                self.parser.submitted(ticket);
                ticket
            })
            .collect::<Vec<_>>();
        self.stdin.write_all(encoded.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let mut combined = CommandResult {
            ticket: tickets[0],
            output: Vec::new(),
            success: true,
        };
        for ticket in tickets {
            let result = self.wait_for(ticket)?;
            combined.success &= result.success;
            combined.output.extend(result.output);
        }
        Ok(combined)
    }

    fn next_event(&mut self) -> Result<Option<Event>, Error> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        loop {
            match self.lines.try_recv() {
                Ok(line) => self.consume(line?)?,
                Err(TryRecvError::Empty) => return Ok(self.events.pop_front()),
                Err(TryRecvError::Disconnected) => return Ok(Some(Event::Disconnected)),
            }
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
        }
    }

    fn wait_for(&mut self, ticket: CommandTicket) -> Result<CommandResult, Error> {
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            if let Some(result) = self.results.remove(&ticket) {
                return Ok(result);
            }
            let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
                return Err(Error::Protocol("tmux control command timed out".into()));
            };
            match self.lines.recv_timeout(timeout) {
                Ok(line) => self.consume(line?)?,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::Protocol("tmux control command timed out".into()));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Protocol("tmux control connection closed".into()));
                }
            }
        }
    }

    fn consume(&mut self, line: String) -> Result<(), Error> {
        for item in self
            .parser
            .receive(&line)
            .map_err(|error| Error::Protocol(error.to_string()))?
        {
            match item {
                ControlItem::Command(result) => {
                    self.results.insert(result.ticket, result);
                }
                ControlItem::Event(event) => enqueue_event(&mut self.events, event),
            }
        }
        Ok(())
    }
}

fn enqueue_event(events: &mut VecDeque<Event>, event: Event) {
    // All translated events are invalidation signals rather than a lossless
    // journal. Coalesce repeats so an event storm cannot grow memory while a
    // command is in flight.
    if !events.contains(&event) {
        events.push_back(event);
    }
}

/// Read one control-mode record without ever growing the destination beyond
/// the named limit. `BufRead::read_line` cannot provide this guarantee because
/// it allocates through the delimiter before the caller can inspect length.
fn read_bounded_control_line(
    reader: &mut impl BufRead,
    maximum: usize,
) -> std::io::Result<Option<String>> {
    debug_assert!(maximum > 0);
    let mut bytes = Vec::with_capacity(maximum.min(4096));
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return decode_control_line(bytes).map(Some);
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if bytes.len().saturating_add(consumed) > maximum {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "tmux control line exceeded limit",
            ));
        }
        let complete = available[consumed - 1] == b'\n';
        bytes.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if complete {
            return decode_control_line(bytes).map(Some);
        }
    }
}

fn decode_control_line(mut bytes: Vec<u8>) -> std::io::Result<String> {
    while bytes.ends_with(b"\n") || bytes.ends_with(b"\r") {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

impl Drop for ControlConnection {
    fn drop(&mut self) {
        // Never risk blocking on a full control stdin during shutdown. SIGKILL
        // closes the observer without mutating user hooks or key bindings.
        let _ = self.child.kill();
        let deadline = Instant::now() + CONTROL_CHILD_SHUTDOWN_WAIT;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => thread::sleep(CONTROL_CHILD_STATUS_POLL),
            }
        }
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn require_success(result: &CommandResult) -> Result<(), Error> {
    if result.success {
        Ok(())
    } else {
        Err(Error::Protocol(if result.output.is_empty() {
            "tmux command failed".into()
        } else {
            result.output.join("; ")
        }))
    }
}

fn session_error(error: SessionError) -> Error {
    Error::Protocol(error.to_string())
}

fn validate_stable_id(value: &str, prefix: char) -> Result<(), Error> {
    if value.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        Ok(())
    } else {
        Err(Error::Protocol(format!("invalid stable tmux ID {value:?}")))
    }
}

fn validate_display_id(value: &str) -> Result<(), Error> {
    if !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'@' | b'%')
        })
    {
        Ok(())
    } else {
        Err(Error::Protocol("invalid Window Display ID".into()))
    }
}

fn validate_client_name(value: &str) -> Result<String, Error> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        Err(Error::Protocol("invalid tmux client name".into()))
    } else {
        Ok(value.to_owned())
    }
}

fn encode_batch(commands: &[Vec<String>]) -> Result<String, Error> {
    commands
        .iter()
        .map(|command| {
            if command.is_empty() {
                return Err(Error::Protocol("empty tmux command".into()));
            }
            command
                .iter()
                .map(|argument| tmux_literal(argument))
                .collect::<Result<Vec<_>, _>>()
                .map(|arguments| arguments.join(" "))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|commands| commands.join(" ; "))
}

fn tmux_literal(value: &str) -> Result<String, Error> {
    if value.as_bytes().contains(&0)
        || value
            .chars()
            .any(|character| matches!(character, '\n' | '\r'))
    {
        return Err(Error::Protocol(
            "tmux argument contains an invalid byte".into(),
        ));
    }
    let mut encoded = String::from("\"");
    for character in value.chars() {
        if matches!(character, '\\' | '"' | '$' | ';') {
            encoded.push('\\');
        }
        encoded.push(character);
    }
    encoded.push('"');
    Ok(encoded)
}

fn break_floating_command(pane: &PaneId, window: &WindowId, geometry: Geometry) -> Vec<String> {
    vec![
        "break-pane".into(),
        "-W".into(),
        "-d".into(),
        "-s".into(),
        pane.0.clone(),
        "-t".into(),
        window.0.clone(),
        "-X".into(),
        geometry.x.to_string(),
        "-Y".into(),
        geometry.y.to_string(),
        "-x".into(),
        geometry.width.to_string(),
        "-y".into(),
        geometry.height.to_string(),
    ]
}

fn floating_resize_size(geometry: Geometry) -> (u16, u16) {
    // Like break-pane -x/-y, resize-pane accepts the complete floating pane
    // dimensions. pane_width/pane_height later report the bordered content
    // area, which is two cells smaller in each dimension.
    (geometry.width.max(1), geometry.height.max(1))
}

fn append_relative_moves(
    commands: &mut Vec<Vec<String>>,
    pane: &PaneId,
    actual: Geometry,
    desired: Geometry,
) {
    let directions = [
        (desired.x.saturating_sub(actual.x), "-R"),
        (actual.x.saturating_sub(desired.x), "-L"),
        (desired.y.saturating_sub(actual.y), "-D"),
        (actual.y.saturating_sub(desired.y), "-U"),
    ];
    for (cells, direction) in directions {
        if cells > 0 {
            commands.push(vec![
                "move-pane".into(),
                "-t".into(),
                pane.0.clone(),
                direction.into(),
                cells.to_string(),
            ]);
        }
    }
}

fn jump_commands(
    client_name: String,
    session_id: String,
    window_id: String,
    pane_id: String,
) -> Vec<Vec<String>> {
    vec![
        vec![
            "switch-client".into(),
            "-c".into(),
            client_name,
            "-t".into(),
            session_id,
        ],
        vec!["select-window".into(), "-t".into(), window_id],
        vec!["select-pane".into(), "-t".into(), pane_id],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use crate::notification::{Notification, NotificationDraft, Presentation};
    use crate::protocol::RendererMessage;
    use chrono::Utc;

    fn content() -> crate::protocol::RendererContent {
        let draft = NotificationDraft::new(Presentation::Toast, "test", "body", None).unwrap();
        crate::protocol::RendererContent::from(&Notification::from_draft(draft, Utc::now()))
    }

    fn geometry(x: u16, y: u16, width: u16, height: u16, z_index: u16) -> Geometry {
        Geometry {
            x,
            y,
            width,
            height,
            z_index,
        }
    }

    #[test]
    fn control_batches_quote_each_argument_and_reject_record_injection() {
        assert_eq!(
            encode_batch(&[
                vec!["select-pane".into(), "-t".into(), "%7".into()],
                vec!["display-message".into(), "a;b $c".into()],
            ])
            .unwrap(),
            "\"select-pane\" \"-t\" \"%7\" ; \"display-message\" \"a\\;b \\$c\""
        );
        assert!(
            encode_batch(&[vec!["display-message".into(), "bad\nkill-server".into()]]).is_err()
        );
    }

    #[test]
    fn renderer_command_is_direct_argv_with_only_executable_id_and_token() {
        let display = WindowDisplayId::new("display-1").unwrap();
        let mut sessions = RendererSessions::new(SessionLimits::default()).unwrap();
        let launch = sessions.create(display, content(), Instant::now()).unwrap();
        let backend = ProductionBackend {
            server: Server::new("/tmp/not-connected"),
            executable: PathBuf::from("/tmp/tm notify's binary"),
            connection: None,
            displays: BTreeMap::new(),
            renderers: Arc::new(Mutex::new(
                RendererSessions::new(SessionLimits::default()).unwrap(),
            )),
            refresh_interval: DEFAULT_TOPOLOGY_REFRESH_INTERVAL,
            next_refresh: Instant::now() + DEFAULT_TOPOLOGY_REFRESH_INTERVAL,
        };
        assert_eq!(
            backend.renderer_argv("__render-toast", &launch).unwrap(),
            vec![
                "/tmp/tm notify's binary",
                "__render-toast",
                "--window-display",
                "display-1",
                "--token",
                launch.token().expose_secret(),
            ]
        );
    }

    #[test]
    fn content_changes_stream_without_recreating_or_using_tmux_commands() {
        let old = content();
        let new_draft =
            NotificationDraft::new(Presentation::Toast, "updated", "new", None).unwrap();
        let new = crate::protocol::RendererContent::from(&Notification::from_draft(
            new_draft,
            Utc::now(),
        ));
        let sessions = Arc::new(Mutex::new(
            RendererSessions::new(SessionLimits::default()).unwrap(),
        ));
        let display_id = WindowDisplayId::new("display-1").unwrap();
        let launch = sessions
            .lock()
            .unwrap()
            .create(display_id.clone(), old.clone(), Instant::now())
            .unwrap();
        let stream = sessions
            .lock()
            .unwrap()
            .redeem(&display_id, launch.token().expose_secret(), Instant::now())
            .unwrap();
        let actual = ActualDisplay {
            pane_id: PaneId("%1".into()),
            window_id: WindowId("@1".into()),
            kind: DisplayKind::Toast,
            geometry: geometry(0, 0, 42, 3, 0),
            content: old,
        };
        let mut backend = ProductionBackend {
            server: Server::new("/tmp/not-connected"),
            executable: PathBuf::from("/tmp/tmnotify"),
            connection: None,
            displays: [("display-1".to_owned(), actual.clone())].into(),
            renderers: sessions,
            refresh_interval: DEFAULT_TOPOLOGY_REFRESH_INTERVAL,
            next_refresh: Instant::now() + DEFAULT_TOPOLOGY_REFRESH_INTERVAL,
        };
        backend
            .update_display(
                "display-1",
                &actual,
                &PlannedDisplay {
                    display_id: "display-1".into(),
                    kind: DisplayKind::Toast,
                    geometry: actual.geometry,
                    content: new,
                    play_enter_animation: false,
                },
            )
            .unwrap();
        assert!(matches!(
            stream.recv().unwrap(),
            RendererMessage::Initial { .. }
        ));
        assert!(matches!(
            stream.recv().unwrap(),
            RendererMessage::Update { content } if content.title() == "updated"
        ));
    }

    #[test]
    fn floating_create_uses_stable_ids_and_outer_geometry() {
        assert_eq!(
            break_floating_command(
                &PaneId("%4".into()),
                &WindowId("@2".into()),
                geometry(7, 3, 42, 5, 0),
            ),
            strings(&[
                "break-pane",
                "-W",
                "-d",
                "-s",
                "%4",
                "-t",
                "@2",
                "-X",
                "7",
                "-Y",
                "3",
                "-x",
                "42",
                "-y",
                "5"
            ])
        );
    }

    #[test]
    fn geometry_updates_are_relative_and_batchable() {
        let mut commands = Vec::new();
        append_relative_moves(
            &mut commands,
            &PaneId("%9".into()),
            geometry(8, 9, 40, 3, 0),
            geometry(5, 12, 40, 3, 0),
        );
        assert_eq!(
            commands,
            vec![
                strings(&["move-pane", "-t", "%9", "-L", "3"]),
                strings(&["move-pane", "-t", "%9", "-D", "3"]),
            ]
        );
    }

    #[test]
    fn resize_uses_complete_floating_geometry() {
        assert_eq!(floating_resize_size(geometry(0, 0, 42, 5, 0)), (42, 5));
        assert_eq!(floating_resize_size(geometry(0, 0, 1, 1, 0)), (1, 1));
    }

    #[test]
    fn identifiers_are_strictly_bounded_before_command_construction() {
        assert!(validate_stable_id("%12", '%').is_ok());
        assert!(validate_stable_id("%1; kill-server", '%').is_err());
        assert!(validate_display_id("019abc:@2").is_ok());
        assert!(validate_display_id("bad\ncommand").is_err());
    }

    #[test]
    fn control_reader_enforces_limit_before_reading_through_delimiter() {
        let mut exact = vec![b'x'; MAX_CONTROL_LINE_BYTES - 1];
        exact.push(b'\n');
        let line = read_bounded_control_line(&mut Cursor::new(exact), MAX_CONTROL_LINE_BYTES)
            .unwrap()
            .unwrap();
        assert_eq!(line.len(), MAX_CONTROL_LINE_BYTES - 1);

        let mut oversized = vec![b'x'; MAX_CONTROL_LINE_BYTES];
        oversized.push(b'\n');
        let error = read_bounded_control_line(&mut Cursor::new(oversized), MAX_CONTROL_LINE_BYTES)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn control_reader_handles_eof_crlf_and_invalid_utf8() {
        let mut records = Cursor::new(b"one\r\ntwo".to_vec());
        assert_eq!(
            read_bounded_control_line(&mut records, 16).unwrap(),
            Some("one".into())
        );
        assert_eq!(
            read_bounded_control_line(&mut records, 16).unwrap(),
            Some("two".into())
        );
        assert_eq!(read_bounded_control_line(&mut records, 16).unwrap(), None);

        let error = read_bounded_control_line(&mut Cursor::new(vec![0xff, b'\n']), 16).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn repeated_control_events_are_coalesced() {
        let mut events = VecDeque::new();
        for _ in 0..10_000 {
            enqueue_event(&mut events, Event::TopologyChanged);
        }
        enqueue_event(&mut events, Event::Disconnected);
        enqueue_event(&mut events, Event::Disconnected);
        assert_eq!(
            events,
            VecDeque::from([Event::TopologyChanged, Event::Disconnected])
        );
    }

    #[test]
    fn jump_switches_client_then_current_window_then_source_pane() {
        assert_eq!(
            jump_commands("/dev/pts/7".into(), "$3".into(), "@12".into(), "%99".into(),),
            vec![
                strings(&["switch-client", "-c", "/dev/pts/7", "-t", "$3"]),
                strings(&["select-window", "-t", "@12"]),
                strings(&["select-pane", "-t", "%99"]),
            ]
        );
    }

    #[test]
    #[ignore = "requires the pinned tmux next-3.8 capability surface and util-linux script"]
    fn isolated_jump_resolves_source_pane_after_it_moves() {
        struct ServerGuard(PathBuf);
        impl Drop for ServerGuard {
            fn drop(&mut self) {
                let _ = Command::new("tmux")
                    .arg("-S")
                    .arg(&self.0)
                    .arg("kill-server")
                    .status();
            }
        }

        fn tmux(socket: &Path, arguments: &[&str]) -> String {
            let output = Command::new("tmux")
                .arg("-S")
                .arg(socket)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "tmux failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("jump.sock");
        let status = Command::new("tmux")
            .arg("-S")
            .arg(&socket)
            .arg("-f")
            .arg("/dev/null")
            .args(["new-session", "-d", "-s", "tmnotify-jump", "sleep 30"])
            .status()
            .unwrap();
        assert!(status.success());
        let _guard = ServerGuard(socket.clone());
        tmux(
            &socket,
            &["new-window", "-d", "-t", "tmnotify-jump", "sleep 30"],
        );
        let source_pane = tmux(
            &socket,
            &[
                "display-message",
                "-p",
                "-t",
                "tmnotify-jump:1",
                "#{pane_id}",
            ],
        );
        let original_pane = tmux(
            &socket,
            &[
                "display-message",
                "-p",
                "-t",
                "tmnotify-jump:0",
                "#{pane_id}",
            ],
        );
        let target_window = tmux(
            &socket,
            &[
                "display-message",
                "-p",
                "-t",
                "tmnotify-jump:0",
                "#{window_id}",
            ],
        );
        tmux(&socket, &["select-window", "-t", &target_window]);

        let attach_command = format!(
            "exec tmux -S {} attach-session -t tmnotify-jump",
            socket.display()
        );
        let mut attached = Command::new("script")
            .args(["-q", "-c", &attach_command, "/dev/null"])
            .env("TERM", "xterm-256color")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let client = (0..50)
            .find_map(|_| {
                let clients = tmux(
                    &socket,
                    &[
                        "list-clients",
                        "-F",
                        "#{client_name}\t#{client_control_mode}",
                    ],
                );
                let client = clients.lines().find_map(|line| {
                    let (name, control) = line.split_once('\t')?;
                    (control == "0").then(|| name.to_owned())
                });
                if client.is_none() {
                    thread::sleep(Duration::from_millis(10));
                }
                client
            })
            .expect("a real display client attached through a PTY");

        tmux(
            &socket,
            &["move-pane", "-d", "-s", &source_pane, "-t", &original_pane],
        );
        tmux(&socket, &["select-pane", "-t", &original_pane]);
        let renderer = std::env::current_exe().unwrap();
        let mut backend = ProductionBackend::connect(Server::new(&socket), renderer).unwrap();
        backend
            .jump(&JumpTarget {
                pane_id: PaneId(source_pane.clone()),
                likely_client: Some(client.clone()),
            })
            .unwrap();
        let selected = tmux(
            &socket,
            &[
                "display-message",
                "-p",
                "-c",
                &client,
                "#{window_id}\t#{pane_id}",
            ],
        );
        assert_eq!(selected, format!("{target_window}\t{source_pane}"));

        let _ = attached.kill();
        let _ = attached.wait();
    }

    #[test]
    #[ignore = "requires the pinned tmux next-3.8 capability surface"]
    fn isolated_control_connection_observes_only_the_explicit_server() {
        use std::os::unix::fs::PermissionsExt;

        struct ServerGuard(PathBuf);
        impl Drop for ServerGuard {
            fn drop(&mut self) {
                let _ = Command::new("tmux")
                    .arg("-S")
                    .arg(&self.0)
                    .arg("kill-server")
                    .status();
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("isolated.sock");
        let status = Command::new("tmux")
            .arg("-S")
            .arg(&socket)
            .arg("-f")
            .arg("/dev/null")
            .args(["new-session", "-d", "-s", "tmnotify-test", "sleep 30"])
            .status()
            .unwrap();
        assert!(status.success());
        let _guard = ServerGuard(socket.clone());
        let global_hooks_before =
            crate::tmux::capability::run_tmux(&socket, &["show-hooks", "-g"]).unwrap();
        let global_keys_before =
            crate::tmux::capability::run_tmux(&socket, &["list-keys"]).unwrap();

        let renderer = directory.path().join("renderer helper");
        std::fs::write(&renderer, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&renderer, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut backend = ProductionBackend::connect(Server::new(&socket), renderer).unwrap();
        let capabilities = backend.capabilities().unwrap();
        assert!(capabilities.supports_display_service());
        let topology = backend.topology().unwrap();
        assert!(topology.clients.iter().all(|client| client.is_control));
        assert!(
            topology
                .panes
                .values()
                .any(|pane| pane.session_id == "$0" && pane.window_id == WindowId("@0".into()))
        );

        let mut desired = DisplayPlan::default();
        desired.windows.insert(
            WindowId("@0".into()),
            vec![PlannedDisplay {
                display_id: "display-1".into(),
                kind: DisplayKind::Toast,
                geometry: geometry(3, 2, 32, 7, 0),
                content: content(),
                play_enter_animation: true,
            }],
        );
        let report = backend.reconcile(&desired).unwrap();
        assert_eq!(
            report.applied,
            [WindowId("@0".into())].into(),
            "{:?}",
            report.failed
        );
        assert!(
            backend
                .topology()
                .unwrap()
                .panes
                .values()
                .any(|pane| { pane.window_id == WindowId("@0".into()) && pane.is_floating })
        );

        desired.windows.get_mut(&WindowId("@0".into())).unwrap()[0].geometry =
            geometry(8, 5, 36, 9, 1);
        let report = backend.reconcile(&desired).unwrap();
        assert_eq!(
            report.applied,
            [WindowId("@0".into())].into(),
            "{:?}",
            report.failed
        );
        let topology = backend.topology().unwrap();
        let pane = topology
            .panes
            .values()
            .find(|pane| pane.is_floating)
            .unwrap_or_else(|| panic!("no floating pane after update: {topology:?}"));
        // pane_left/pane_top describe the inner content origin; the desired
        // geometry includes the floating border, so both are offset by one.
        assert_eq!((pane.left, pane.top), (9, 6));
        assert_eq!((pane.width, pane.height), (34, 7));

        backend.reconcile(&DisplayPlan::default()).unwrap();
        assert!(
            !backend
                .topology()
                .unwrap()
                .panes
                .values()
                .any(|pane| pane.is_floating)
        );
        assert_eq!(
            crate::tmux::capability::run_tmux(&socket, &["show-hooks", "-g"]).unwrap(),
            global_hooks_before
        );
        assert_eq!(
            crate::tmux::capability::run_tmux(&socket, &["list-keys"]).unwrap(),
            global_keys_before
        );
    }
}
