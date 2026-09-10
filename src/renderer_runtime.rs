//! Hidden pane-renderer entry points.
//!
//! Renderers derive the private daemon socket from the tmux socket in `TMUX`,
//! redeem an argv-only capability, then keep that same connection for bounded
//! content updates, termination, and (Attention only) actions.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event};
use thiserror::Error;

use crate::config::BodyPresentation;
use crate::daemon::runtime::ServerIdentity;
use crate::platform::{Environment, PathError, PlatformPaths, validate_runtime_socket};
use crate::protocol::{
    FrameDecoder, ProtocolError, RendererAction, RendererBodyMode, RendererContent,
    RendererMessage, RendererRedemption,
};
use crate::toast::{
    ColorMode, DisplayAge, GlyphMode, Rect, RenderOptions, Surface, Toast, render_toast,
};
use crate::ui::attention::{AttentionCommand, AttentionState};
use crate::ui::{InterruptWatcher, TerminalSession};

const IO_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_PENDING_FRAMES: usize = 16;
const HIDE_AND_CLEAR: &[u8] = b"\x1b[?25l\x1b[H\x1b[2J";
const RESTORE_TERMINAL: &[u8] = b"\x1b[0m\x1b[?25h";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RendererKind {
    Toast,
    Attention,
}

pub fn run_hidden_renderer(
    kind: RendererKind,
    window_display_id: &str,
    token: &str,
) -> Result<(), RendererRuntimeError> {
    let environment = Environment::current();
    let socket = daemon_socket_from_environment(&environment)?;
    let connection = RendererConnection::connect(&socket, window_display_id, token)?;
    match kind {
        RendererKind::Toast => run_toast(connection, io::stdout()),
        RendererKind::Attention => run_attention(connection),
    }
}

/// Resolve exactly the daemon selected by the current tmux pane. No socket
/// name guessing is permitted in a hidden renderer.
pub fn daemon_socket_from_environment(
    environment: &Environment,
) -> Result<PathBuf, RendererRuntimeError> {
    let tmux = environment
        .get("TMUX")
        .and_then(|value| value.to_str())
        .ok_or(RendererRuntimeError::TmuxUnavailable)?;
    let socket = tmux
        .split_once(',')
        .map(|(socket, _)| socket)
        .filter(|socket| !socket.is_empty())
        .ok_or(RendererRuntimeError::InvalidTmuxEnvironment)?;
    let identity = ServerIdentity::resolve(Path::new(socket))?;
    let paths = PlatformPaths::resolve(environment)?;
    paths.socket_path(identity.server_id()).map_err(Into::into)
}

struct RendererConnection {
    stream: UnixStream,
    decoder: FrameDecoder,
    pending: VecDeque<RendererMessage>,
}

impl RendererConnection {
    fn connect(
        socket: &Path,
        window_display_id: &str,
        token: &str,
    ) -> Result<Self, RendererRuntimeError> {
        validate_runtime_socket(socket)?;
        let stream = UnixStream::connect(socket).map_err(RendererRuntimeError::Io)?;
        Self::redeem(stream, window_display_id, token)
    }

    fn redeem(
        mut stream: UnixStream,
        window_display_id: &str,
        token: &str,
    ) -> Result<Self, RendererRuntimeError> {
        let redemption = RendererRedemption::new(window_display_id, token)?;
        stream.write_all(&redemption.encode_line()?)?;
        stream.set_read_timeout(Some(IO_POLL_INTERVAL))?;
        Ok(Self {
            stream,
            decoder: FrameDecoder::default(),
            pending: VecDeque::new(),
        })
    }

    fn poll(&mut self) -> Result<Option<RendererMessage>, RendererRuntimeError> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(Some(message));
        }
        let mut buffer = [0_u8; 4096];
        match self.stream.read(&mut buffer) {
            Ok(0) => Err(RendererRuntimeError::DaemonDisconnected),
            Ok(read) => {
                let frames = self.decoder.push(&buffer[..read])?;
                if frames.len() > MAX_PENDING_FRAMES {
                    return Err(RendererRuntimeError::TooManyFrames);
                }
                for frame in frames {
                    self.pending.push_back(RendererMessage::decode(&frame)?);
                }
                Ok(self.pending.pop_front())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(RendererRuntimeError::Io(error)),
        }
    }

    fn send_action(&mut self, action: RendererAction) -> Result<(), RendererRuntimeError> {
        self.stream.write_all(&action.encode_line()?)?;
        Ok(())
    }
}

struct ToastTerminal<W: Write> {
    output: W,
}

impl<W: Write> ToastTerminal<W> {
    fn enter(mut output: W) -> Result<Self, RendererRuntimeError> {
        output.write_all(HIDE_AND_CLEAR)?;
        output.flush()?;
        Ok(Self { output })
    }

    fn draw(&mut self, content: &RendererContent) -> Result<(u16, u16), RendererRuntimeError> {
        if content.presentation() != crate::notification::Presentation::Toast {
            return Err(RendererRuntimeError::WrongPresentation);
        }
        let (width, height) = crossterm::terminal::size().unwrap_or((40, 1));
        let surface = if height >= 3 && width >= 42 {
            Surface::Bordered
        } else {
            Surface::Borderless
        };
        let age = DisplayAge::from_timeout(content.timeout(), None);
        let toast = Toast {
            sequence: 0,
            level: content.level(),
            title: content.title(),
            body: content.body(),
            notification_key: content.notification_key(),
            age,
            updated: false,
        };
        let rendered = render_toast(
            &toast,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            surface,
            render_options(content),
        );
        self.output.write_all(HIDE_AND_CLEAR)?;
        for (index, line) in rendered.styled_lines.iter().enumerate() {
            if index > 0 {
                self.output.write_all(b"\r\n")?;
            }
            self.output.write_all(line.as_bytes())?;
        }
        self.output.flush()?;
        Ok((width, height))
    }
}

fn render_options(content: &RendererContent) -> RenderOptions {
    let display = content.display_options();
    RenderOptions {
        body: match display.body() {
            RendererBodyMode::FirstLine => BodyPresentation::FirstLine,
            RendererBodyMode::JoinLines => BodyPresentation::JoinLines,
            RendererBodyMode::Wrap => BodyPresentation::Wrap,
        },
        glyphs: if display.unicode() {
            GlyphMode::Unicode
        } else {
            GlyphMode::Ascii
        },
        color: if display.color() {
            ColorMode::Ansi16
        } else {
            ColorMode::Monochrome
        },
        show_jump_hint: true,
    }
}

impl<W: Write> Drop for ToastTerminal<W> {
    fn drop(&mut self) {
        let _ = self.output.write_all(RESTORE_TERMINAL);
        let _ = self.output.flush();
    }
}

fn run_toast<W: Write>(
    mut connection: RendererConnection,
    output: W,
) -> Result<(), RendererRuntimeError> {
    let mut terminal = ToastTerminal::enter(output)?;
    let interrupt = InterruptWatcher::new()?;
    let mut current_content = None;
    let mut rendered_size = None;
    let mut settle_redraw = false;
    while !interrupt.is_interrupted() {
        match connection.poll()? {
            Some(RendererMessage::Initial { content } | RendererMessage::Update { content }) => {
                rendered_size = Some(terminal.draw(&content)?);
                current_content = Some(content);
                // tmux can launch the renderer before the new floating pane's
                // final content dimensions are observable. Redraw once after
                // the next IO poll even if no keyed update arrives.
                settle_redraw = true;
            }
            Some(RendererMessage::Terminate { .. }) => return Ok(()),
            Some(RendererMessage::Error { .. }) => {}
            None => {
                let size = crossterm::terminal::size().unwrap_or((40, 1));
                if let Some(content) = current_content.as_ref()
                    && (settle_redraw || rendered_size != Some(size))
                {
                    rendered_size = Some(terminal.draw(content)?);
                    settle_redraw = false;
                }
            }
        }
    }
    Ok(())
}

fn run_attention(mut connection: RendererConnection) -> Result<(), RendererRuntimeError> {
    let mut terminal = TerminalSession::enter()?;
    let interrupt = InterruptWatcher::new()?;
    let mut state: Option<AttentionState> = None;
    loop {
        if interrupt.is_interrupted() {
            return Ok(());
        }
        while let Some(message) = connection.poll()? {
            match message {
                RendererMessage::Initial { content } | RendererMessage::Update { content } => {
                    let display = content.display_options();
                    state = Some(AttentionState::from_renderer_content(
                        &content,
                        display.unicode(),
                        display.color(),
                    )?);
                }
                RendererMessage::Error { message } => {
                    if let Some(state) = &mut state {
                        state.set_jump_error(message);
                    }
                }
                RendererMessage::Terminate { .. } => return Ok(()),
            }
        }
        if let Some(state) = &state {
            terminal
                .terminal_mut()
                .draw(|frame| crate::ui::attention::render(frame, state))?;
        }
        if !event::poll(IO_POLL_INTERVAL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => {
                let action = state.as_ref().map(|state| state.handle_key(key));
                match action {
                    Some(AttentionCommand::Jump) => connection.send_action(RendererAction::Jump)?,
                    Some(AttentionCommand::Dismiss) => {
                        connection.send_action(RendererAction::Dismiss)?
                    }
                    Some(AttentionCommand::Interrupt) => return Ok(()),
                    Some(AttentionCommand::None) | None => {}
                }
            }
            Event::Resize(_, _) => {}
            Event::Mouse(_) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
        }
    }
}

#[derive(Debug, Error)]
pub enum RendererRuntimeError {
    #[error("renderer must run inside tmux")]
    TmuxUnavailable,
    #[error("TMUX does not contain a valid socket path")]
    InvalidTmuxEnvironment,
    #[error("daemon disconnected from renderer")]
    DaemonDisconnected,
    #[error("renderer received too many frames at once")]
    TooManyFrames,
    #[error("renderer received content for the wrong presentation")]
    WrongPresentation,
    #[error("renderer IO failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Runtime(#[from] crate::daemon::runtime::RuntimeError),
    #[error(transparent)]
    Attention(#[from] crate::ui::attention::AttentionError),
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::thread;

    use chrono::Utc;

    use super::*;
    use crate::notification::{Notification, NotificationDraft, Presentation};

    fn content() -> RendererContent {
        let draft = NotificationDraft::new(Presentation::Toast, "build", "done", None).unwrap();
        RendererContent::from(&Notification::from_draft(draft, Utc::now()))
    }

    #[test]
    fn daemon_socket_is_derived_from_tmux_and_private_runtime_path() {
        let directory = tempfile::tempdir().unwrap();
        let tmux_socket = directory.path().join("tmux.sock");
        fs::write(&tmux_socket, b"identity-only fixture").unwrap();
        let runtime = directory.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let environment = Environment::from_pairs([
            (
                OsString::from("HOME"),
                directory.path().as_os_str().to_owned(),
            ),
            (
                OsString::from("XDG_RUNTIME_DIR"),
                runtime.as_os_str().to_owned(),
            ),
            (
                OsString::from("TMUX"),
                OsString::from(format!("{},123,0", tmux_socket.display())),
            ),
        ]);
        let identity = ServerIdentity::resolve(&tmux_socket).unwrap();
        assert_eq!(
            daemon_socket_from_environment(&environment).unwrap(),
            runtime
                .join("tmnotify")
                .join(format!("{}.sock", identity.server_id()))
        );
    }

    #[test]
    fn fake_socket_redeems_then_streams_without_content_in_handshake() {
        let (client, mut server_stream) = UnixStream::pair().unwrap();
        let expected = content();
        let server = thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut byte = [0_u8; 1];
            while server_stream.read_exact(&mut byte).is_ok() && byte[0] != b'\n' {
                bytes.push(byte[0]);
            }
            let envelope = crate::protocol::RequestEnvelope::decode(&bytes).unwrap();
            let redemption = envelope.renderer_redemption().unwrap();
            assert_eq!(redemption.window_display_id(), "display-1");
            assert_eq!(redemption.token(), "ab".repeat(32));
            assert!(!String::from_utf8(bytes).unwrap().contains("build"));
            server_stream
                .write_all(
                    &RendererMessage::Initial { content: expected }
                        .encode_line()
                        .unwrap(),
                )
                .unwrap();
        });
        let mut connection =
            RendererConnection::redeem(client, "display-1", &"ab".repeat(32)).unwrap();
        loop {
            if let Some(RendererMessage::Initial { content }) = connection.poll().unwrap() {
                assert_eq!(content.title(), "build");
                break;
            }
        }
        server.join().unwrap();
    }

    #[test]
    fn toast_guard_uses_only_fixed_control_frames_and_restores_cursor() {
        let mut bytes = Vec::new();
        {
            let mut terminal = ToastTerminal::enter(&mut bytes).unwrap();
            terminal.draw(&content()).unwrap();
        }
        assert!(bytes.starts_with(HIDE_AND_CLEAR));
        assert!(bytes.windows("build".len()).any(|part| part == b"build"));
        assert!(bytes.ends_with(RESTORE_TERMINAL));
    }

    #[test]
    fn daemon_display_options_control_renderer_body_glyphs_and_color() {
        let content = content().with_display_options(crate::protocol::RendererDisplayOptions::new(
            RendererBodyMode::JoinLines,
            false,
            false,
        ));
        assert_eq!(
            render_options(&content),
            RenderOptions {
                body: BodyPresentation::JoinLines,
                glyphs: GlyphMode::Ascii,
                color: ColorMode::Monochrome,
                show_jump_hint: true,
            }
        );
    }
}
