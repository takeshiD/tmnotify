use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Margin, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use thiserror::Error;

use super::{InterruptWatcher, TerminalSession};
use crate::notification::{Notification, Presentation};

pub const COMPACT_MIN_WIDTH: u16 = 32;
pub const COMPACT_MIN_HEIGHT: u16 = 7;
pub const WIDE_MIN_WIDTH: u16 = 60;
pub const WIDE_MIN_HEIGHT: u16 = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionCommand {
    None,
    Jump,
    Dismiss,
    Interrupt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionState {
    pub title: String,
    pub body: String,
    pub source: Option<String>,
    pub error: Option<String>,
    pub unicode: bool,
    pub color: bool,
}

impl AttentionState {
    pub fn from_notification(
        notification: &Notification,
        unicode: bool,
        color: bool,
    ) -> Result<Self, AttentionError> {
        if notification.presentation() != Presentation::Attention {
            return Err(AttentionError::NotAttention);
        }
        let source = notification.source().map(|source| {
            source.cwd().map_or_else(
                || source.pane_id().to_owned(),
                |cwd| format!("{} · {}", source.pane_id(), cwd.display()),
            )
        });
        Ok(Self {
            title: notification.title().to_owned(),
            body: notification.body().to_owned(),
            source,
            error: None,
            unicode,
            color,
        })
    }

    pub fn set_jump_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
    }

    pub fn handle_key(&self, key: KeyEvent) -> AttentionCommand {
        if key.kind != KeyEventKind::Press {
            return AttentionCommand::None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Enter, KeyModifiers::NONE) => AttentionCommand::Jump,
            (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => {
                AttentionCommand::Dismiss
            }
            (KeyCode::Char('c'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                AttentionCommand::Interrupt
            }
            _ => AttentionCommand::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionOutcome {
    JumpRequested,
    Dismissed,
    Interrupted,
}

/// Runs only the renderer event loop. Jump execution remains a daemon/tmux
/// responsibility so pane selection and global lifecycle stay coordinated.
pub fn run(state: &AttentionState) -> Result<AttentionOutcome, AttentionError> {
    const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
    let mut terminal = TerminalSession::enter()?;
    let interrupt = InterruptWatcher::new()?;
    loop {
        terminal.terminal_mut().draw(|frame| render(frame, state))?;
        if interrupt.is_interrupted() {
            return Ok(AttentionOutcome::Interrupted);
        }
        if !event::poll(EVENT_POLL_INTERVAL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => match state.handle_key(key) {
                AttentionCommand::Jump => return Ok(AttentionOutcome::JumpRequested),
                AttentionCommand::Dismiss => return Ok(AttentionOutcome::Dismissed),
                AttentionCommand::Interrupt => return Ok(AttentionOutcome::Interrupted),
                AttentionCommand::None => {}
            },
            Event::Resize(_, _) => {}
            Event::Mouse(_) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
        }
    }
}

pub fn render(frame: &mut Frame<'_>, state: &AttentionState) {
    let area = frame.area();
    if area.width < COMPACT_MIN_WIDTH || area.height < COMPACT_MIN_HEIGHT {
        render_too_small(frame, state, area);
    } else if area.width < WIDE_MIN_WIDTH || area.height < WIDE_MIN_HEIGHT {
        render_compact(frame, state, area);
    } else {
        render_wide(frame, state, area);
    }
}

fn render_too_small(frame: &mut Frame<'_>, state: &AttentionState, area: Rect) {
    let separator = if state.unicode { " · " } else { " | " };
    let line = format!("terminal too small{separator}Enter jump{separator}Esc dismiss");
    frame.render_widget(
        Paragraph::new(line)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_compact(frame: &mut Frame<'_>, state: &AttentionState, area: Rect) {
    let popup = area.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    frame.render_widget(Clear, popup);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(popup);
    frame.render_widget(title(state), rows[0]);
    frame.render_widget(
        Paragraph::new(state.body.as_str()).wrap(Wrap { trim: true }),
        rows[1],
    );
    render_error(frame, state, rows[2]);
    frame.render_widget(footer(state), rows[3]);
}

fn render_wide(frame: &mut Frame<'_>, state: &AttentionState, area: Rect) {
    let [popup] = Layout::horizontal([Constraint::Percentage(60)])
        .flex(Flex::Center)
        .areas(area);
    let popup_height = area.height.saturating_mul(60).div_ceil(100).max(10);
    let [popup] = Layout::vertical([Constraint::Length(popup_height.min(area.height))])
        .flex(Flex::Center)
        .areas(popup);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(" Attention ").bold());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(title(state), rows[0]);
    frame.render_widget(
        Paragraph::new(state.body.as_str()).wrap(Wrap { trim: true }),
        rows[1],
    );
    if let Some(source) = &state.source {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("from ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw(source),
            ])),
            rows[2],
        );
    }
    render_error(frame, state, rows[3]);
    frame.render_widget(footer(state), rows[4]);
}

fn title<'a>(state: &'a AttentionState) -> Paragraph<'a> {
    let marker = if state.unicode { "! " } else { "[!] " };
    let style = if state.color {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    Paragraph::new(Line::from(vec![
        Span::styled(marker, style),
        Span::styled(&state.title, style),
    ]))
}

fn footer(state: &AttentionState) -> Paragraph<'static> {
    let separator = if state.unicode { "  ·  " } else { "  |  " };
    Paragraph::new(format!("[Enter] jump{separator}[Esc/q] dismiss")).alignment(Alignment::Center)
}

fn render_error(frame: &mut Frame<'_>, state: &AttentionState, area: Rect) {
    if let Some(error) = &state.error {
        let style = if state.color {
            Style::default().fg(Color::Red)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("error: ", style),
                Span::raw(error),
            ])),
            area,
        );
    }
}

#[derive(Debug, Error)]
pub enum AttentionError {
    #[error("Notification is not Attention")]
    NotAttention,
    #[error("terminal IO failed: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::notification::{NotificationDraft, SourceContext, TmuxServerId};

    fn state() -> AttentionState {
        let source = SourceContext::new(TmuxServerId::new("server").unwrap(), "$1", "@2", "%3")
            .unwrap()
            .with_cwd("/work/project")
            .unwrap();
        let draft = NotificationDraft::new(
            Presentation::Attention,
            "Codex needs input",
            "Approve the requested operation? This content wraps safely.",
            Some(source),
        )
        .unwrap();
        AttentionState::from_notification(
            &crate::notification::Notification::from_draft(draft, Utc::now()),
            true,
            true,
        )
        .unwrap()
    }

    fn draw(width: u16, height: u16, state: &AttentionState) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn renders_wide_content_source_error_and_footer() {
        let mut state = state();
        state.set_jump_error("Source Pane no longer exists");
        let text = draw(100, 24, &state).join("\n");
        assert!(text.contains("Codex needs input"));
        assert!(text.contains("%3 · /work/project"));
        assert!(text.contains("error: Source Pane no longer exists"));
        assert!(text.contains("[Enter] jump"));
    }

    #[test]
    fn compact_omits_metadata_but_keeps_actions() {
        let text = draw(48, 10, &state()).join("\n");
        assert!(text.contains("Codex needs input"));
        assert!(!text.contains("/work/project"));
        assert!(text.contains("[Esc/q] dismiss"));
    }

    #[test]
    fn tiny_layout_states_size_and_both_actions_truthfully() {
        let text = draw(31, 6, &state()).join(" ");
        assert!(text.contains("terminal too small"));
        assert!(text.contains("Enter jump"));
        assert!(text.contains("Esc dismiss"));
    }

    #[test]
    fn keys_are_global_and_other_input_is_ignored() {
        let state = state();
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AttentionCommand::Jump
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            AttentionCommand::Dismiss
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            AttentionCommand::Dismiss
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            AttentionCommand::None
        );
    }

    #[test]
    fn monochrome_ascii_uses_text_and_reverse_error_not_color_alone() {
        let mut state = state();
        state.unicode = false;
        state.color = false;
        state.set_jump_error("missing");
        let text = draw(80, 20, &state).join("\n");
        assert!(text.contains("[!] Codex needs input"));
        assert!(text.contains("error: missing"));
        assert!(text.contains(" | "));
    }

    #[test]
    fn rejects_toast_state() {
        let toast = crate::notification::Notification::from_draft(
            NotificationDraft::new(Presentation::Toast, "title", "body", None).unwrap(),
            Utc::now(),
        );
        assert!(matches!(
            AttentionState::from_notification(&toast, true, true),
            Err(AttentionError::NotAttention)
        ));
    }
}
