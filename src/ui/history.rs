use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::history::HistoryEntry;
use crate::notification::{Level, NotificationId, SourceContext, TmuxServerId};

pub const MIN_WIDTH: u16 = 48;
pub const MIN_HEIGHT: u16 = 10;
pub const WIDE_WIDTH: u16 = 100;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadState {
    Loading,
    Ready,
    Error(String),
    Disconnected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryAction {
    Jump {
        id: NotificationId,
        source: SourceContext,
    },
    Hide(NotificationId),
    Unhide(NotificationId),
    Quit,
}

#[derive(Clone, Debug)]
pub struct HistoryView {
    entries: Vec<HistoryEntry>,
    current_server: TmuxServerId,
    selected: usize,
    detail_visible: bool,
    filter: String,
    editing_filter: bool,
    help_visible: bool,
    hidden_this_session: Vec<HistoryEntry>,
    confirmation: Option<NotificationId>,
    status: Option<String>,
    load_state: LoadState,
    color: bool,
    unicode: bool,
}

impl HistoryView {
    #[must_use]
    pub fn new(entries: Vec<HistoryEntry>, current_server: TmuxServerId) -> Self {
        Self {
            entries,
            current_server,
            selected: 0,
            detail_visible: false,
            filter: String::new(),
            editing_filter: false,
            help_visible: false,
            hidden_this_session: Vec::new(),
            confirmation: None,
            status: None,
            load_state: LoadState::Ready,
            color: true,
            unicode: true,
        }
    }

    #[must_use]
    pub fn loading(current_server: TmuxServerId) -> Self {
        let mut view = Self::new(Vec::new(), current_server);
        view.load_state = LoadState::Loading;
        view
    }

    pub fn set_load_state(&mut self, state: LoadState) {
        self.load_state = state;
    }

    pub fn set_display_modes(&mut self, color: bool, unicode: bool) {
        self.color = color;
        self.unicode = unicode;
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<HistoryAction> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if self.confirmation.is_some() {
            return self.handle_confirmation(key);
        }
        if self.editing_filter {
            return self.handle_filter_key(key);
        }
        if self.help_visible {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
            ) {
                self.help_visible = false;
            }
            return None;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.filtered_indices().len().saturating_sub(1)
            }
            KeyCode::Enter => return self.begin_jump(),
            KeyCode::Char(' ') => self.detail_visible = !self.detail_visible,
            KeyCode::Char('/') => self.editing_filter = true,
            KeyCode::Char('?') => self.help_visible = true,
            KeyCode::Char('d') => return self.hide_selected(),
            KeyCode::Char('u') => return self.undo_hide(),
            KeyCode::Esc | KeyCode::Char('q') => return Some(HistoryAction::Quit),
            _ => {}
        }
        None
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Option<HistoryAction> {
        match key.code {
            KeyCode::Enter => self.editing_filter = false,
            KeyCode::Esc => {
                self.editing_filter = false;
                self.filter.clear();
                self.selected = 0;
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.filter.push(character);
                self.selected = 0;
            }
            _ => {}
        }
        None
    }

    fn handle_confirmation(&mut self, key: KeyEvent) -> Option<HistoryAction> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let id = self.confirmation.take()?;
                self.jump_action(id)
            }
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                self.confirmation = None;
                None
            }
            _ => None,
        }
    }

    fn move_selection(&mut self, amount: isize) {
        let count = self.filtered_indices().len();
        self.selected = self
            .selected
            .saturating_add_signed(amount)
            .min(count.saturating_sub(1));
    }

    fn selected_id(&self) -> Option<NotificationId> {
        self.filtered_indices()
            .get(self.selected)
            .map(|index| self.entries[*index].id)
    }

    fn begin_jump(&mut self) -> Option<HistoryAction> {
        let id = self.selected_id()?;
        let entry = self.entries.iter().find(|entry| entry.id == id)?;
        if entry.source.is_none() {
            self.status = Some("Notification has no Source Pane".to_owned());
            return None;
        }
        if entry.tmux_server_id != self.current_server {
            self.confirmation = Some(id);
            return None;
        }
        self.jump_action(id)
    }

    fn jump_action(&mut self, id: NotificationId) -> Option<HistoryAction> {
        let entry = self.entries.iter().find(|entry| entry.id == id)?;
        let source = entry.source.clone()?;
        Some(HistoryAction::Jump { id, source })
    }

    fn hide_selected(&mut self) -> Option<HistoryAction> {
        let id = self.selected_id()?;
        let position = self.entries.iter().position(|entry| entry.id == id)?;
        self.hidden_this_session.push(self.entries.remove(position));
        self.selected = self
            .selected
            .min(self.filtered_indices().len().saturating_sub(1));
        self.status = Some("Hidden · u to undo".to_owned());
        Some(HistoryAction::Hide(id))
    }

    fn undo_hide(&mut self) -> Option<HistoryAction> {
        let entry = self.hidden_this_session.pop()?;
        let id = entry.id;
        self.entries.push(entry);
        self.entries.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then(right.id.to_string().cmp(&left.id.to_string()))
        });
        self.selected = self
            .filtered_indices()
            .iter()
            .position(|index| self.entries[*index].id == id)
            .unwrap_or(0);
        self.status = Some("Hide undone".to_owned());
        Some(HistoryAction::Unhide(id))
    }

    fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let smart_case = self.filter.chars().any(char::is_uppercase);
        let needle = if smart_case {
            self.filter.clone()
        } else {
            self.filter.to_lowercase()
        };
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let haystack = format!("{}\n{}", entry.title, entry.body);
                let haystack = if smart_case {
                    haystack
                } else {
                    haystack.to_lowercase()
                };
                haystack.contains(&needle).then_some(index)
            })
            .collect()
    }
}

pub fn render(frame: &mut Frame<'_>, view: &HistoryView) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new("terminal too small — need 48×10")
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    render_header(frame, view, rows[0]);
    render_body(frame, view, rows[1]);
    render_footer(frame, view, rows[2]);
    if view.help_visible {
        render_help(frame, area);
    }
    if let Some(id) = view.confirmation {
        render_confirmation(frame, area, id);
    }
}

fn render_header(frame: &mut Frame<'_>, view: &HistoryView, area: Rect) {
    let marker = if view.unicode { "●" } else { "*" };
    let filter = if view.editing_filter {
        format!("  /{}▏", view.filter)
    } else if view.filter.is_empty() {
        String::new()
    } else {
        format!("  /{}", view.filter)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{marker} History"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(filter),
        ])),
        area,
    );
}

fn render_body(frame: &mut Frame<'_>, view: &HistoryView, area: Rect) {
    match &view.load_state {
        LoadState::Loading => {
            frame.render_widget(Paragraph::new("Loading History…"), area);
            return;
        }
        LoadState::Error(error) => {
            frame.render_widget(Paragraph::new(format!("error: {error}")), area);
            return;
        }
        LoadState::Disconnected => {
            frame.render_widget(Paragraph::new("History disconnected"), area);
            return;
        }
        LoadState::Ready => {}
    }
    let filtered = view.filtered_indices();
    if filtered.is_empty() {
        frame.render_widget(Paragraph::new("No Notifications"), area);
        return;
    }
    if area.width >= WIDE_WIDTH {
        let columns = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(area);
        render_list(frame, view, &filtered, columns[0]);
        render_detail(frame, view, &filtered, columns[1]);
    } else if view.detail_visible {
        render_detail(frame, view, &filtered, area);
    } else {
        render_list(frame, view, &filtered, area);
    }
}

fn render_list(frame: &mut Frame<'_>, view: &HistoryView, filtered: &[usize], area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Notifications ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let visible = usize::from(inner.height);
    let start = view
        .selected
        .saturating_sub(visible / 2)
        .min(filtered.len().saturating_sub(visible));
    let lines = filtered
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(position, index)| {
            let entry = &view.entries[*index];
            let symbol = level_symbol(entry.level, view.unicode);
            let mut style = level_style(entry.level, view.color);
            if position == view.selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Line::styled(format!("{symbol} {}  {}", entry.title, entry.body), style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_detail(frame: &mut Frame<'_>, view: &HistoryView, filtered: &[usize], area: Rect) {
    let Some(index) = filtered.get(view.selected) else {
        return;
    };
    let entry = &view.entries[*index];
    let source = entry.source.as_ref().map_or_else(
        || "no Source Pane".to_owned(),
        |source| {
            format!(
                "{} · {}",
                source.pane_id(),
                source
                    .cwd()
                    .map_or_else(|| "".into(), |p| p.display().to_string())
            )
        },
    );
    let text = vec![
        Line::styled(&entry.title, Style::default().add_modifier(Modifier::BOLD)),
        Line::raw(format!(
            "{} · {:?}",
            entry.updated_at.to_rfc3339(),
            entry.level
        )),
        Line::raw(source),
        Line::raw(""),
        Line::raw(&entry.body),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(" Detail "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, view: &HistoryView, area: Rect) {
    let text = view.status.as_deref().unwrap_or(if view.detail_visible {
        "[Space] list  [Enter] jump  [d] hide  [q] back"
    } else {
        "[j/k] move  [Enter] jump  [Space] detail  [/] filter  [?] help  [q] quit"
    });
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), area);
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered(area, 54, 14);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(
            "j/k, ↑/↓  move\ng/G        first/last\nEnter      jump\nSpace      detail/list\n/          smart-case filter\nd          Hide\nu          undo latest Hide\nq/Esc      back or close\n?          close help",
        )
        .block(Block::default().borders(Borders::ALL).title(" Keys ")),
        popup,
    );
}

fn render_confirmation(frame: &mut Frame<'_>, area: Rect, id: NotificationId) {
    let popup = centered(area, 52, 7);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "Notification {id} belongs to another tmux server.\nConnect and attempt jump?\n\n[y/Enter] confirm  [n/Esc] cancel"
        ))
        .block(Block::default().borders(Borders::ALL).title(" Cross-server jump "))
        .wrap(Wrap { trim: true }),
        popup,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [horizontal] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [vertical] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(horizontal);
    vertical
}

fn level_symbol(level: Level, unicode: bool) -> &'static str {
    match (level, unicode) {
        (Level::Info, true) => "i",
        (Level::Success, true) => "✓",
        (Level::Warning, true) => "!",
        (Level::Error, true) => "×",
        (Level::Info, false) => "[i]",
        (Level::Success, false) => "[ok]",
        (Level::Warning, false) => "[!]",
        (Level::Error, false) => "[x]",
    }
}

fn level_style(level: Level, color: bool) -> Style {
    if !color {
        return Style::default();
    }
    Style::default().fg(match level {
        Level::Info => Color::Cyan,
        Level::Success => Color::Green,
        Level::Warning => Color::Yellow,
        Level::Error => Color::Red,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::notification::{DeliveryState, NormalizedMetadata, Presentation, Priority, Timeout};

    fn server(name: &str) -> TmuxServerId {
        TmuxServerId::new(name).unwrap()
    }

    fn entry(server_name: &str, title: &str, body: &str, pane: bool) -> HistoryEntry {
        let now = DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).unwrap();
        HistoryEntry {
            id: NotificationId::new(),
            key: None,
            tmux_server_id: server(server_name),
            created_at: now,
            updated_at: now,
            level: Level::Success,
            priority: Priority::Normal,
            presentation: Presentation::Toast,
            delivery: DeliveryState::Closed,
            close_reason: None,
            timeout: Timeout::After(Duration::from_secs(3)),
            title: title.to_owned(),
            body: body.to_owned(),
            source: pane
                .then(|| SourceContext::new(server(server_name), "$1", "@2", "%3").unwrap()),
            hidden_at: None,
            last_jumped_at: None,
            metadata: NormalizedMetadata::default(),
        }
    }

    fn draw(width: u16, height: u16, view: &HistoryView) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, view)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wide_and_compact_breakpoints_preserve_content_and_actions() {
        let mut view = HistoryView::new(
            vec![entry("alpha", "Build", "finished", true)],
            server("alpha"),
        );
        let wide = draw(100, 24, &view);
        assert!(wide.contains("Notifications"));
        assert!(wide.contains("Detail"));
        assert!(wide.contains("%3"));
        let compact = draw(60, 20, &view);
        assert!(compact.contains("Notifications"));
        assert!(!compact.contains(" Detail "));
        view.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(draw(60, 20, &view).contains("Detail"));
        assert!(draw(48, 10, &view).contains("Build"));
        assert!(draw(47, 9, &view).contains("terminal too small — need 48×10"));
    }

    #[test]
    fn smart_case_filter_and_virtual_selection_are_deterministic() {
        let mut view = HistoryView::new(
            vec![
                entry("alpha", "Build", "SUCCESS", true),
                entry("alpha", "deploy", "success", true),
            ],
            server("alpha"),
        );
        view.filter = "success".into();
        assert_eq!(view.filtered_indices().len(), 2);
        view.filter = "SUCCESS".into();
        assert_eq!(view.filtered_indices().len(), 1);
    }

    #[test]
    fn hide_and_session_undo_emit_storage_actions() {
        let item = entry("alpha", "Build", "done", true);
        let id = item.id;
        let mut view = HistoryView::new(vec![item], server("alpha"));
        assert_eq!(
            view.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            Some(HistoryAction::Hide(id))
        );
        assert_eq!(
            view.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)),
            Some(HistoryAction::Unhide(id))
        );
    }

    #[test]
    fn jump_requires_source_and_confirms_other_server() {
        let local = entry("alpha", "local", "done", false);
        let mut view = HistoryView::new(vec![local], server("alpha"));
        assert!(
            view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(
            view.status.as_deref(),
            Some("Notification has no Source Pane")
        );

        let remote = entry("beta", "remote", "done", true);
        let id = remote.id;
        let mut view = HistoryView::new(vec![remote], server("alpha"));
        assert!(
            view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(view.confirmation, Some(id));
        assert!(matches!(
            view.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Some(HistoryAction::Jump { id: selected, .. }) if selected == id
        ));
    }

    #[test]
    fn empty_loading_error_disconnected_ascii_and_help_states_render() {
        let mut view = HistoryView::new(Vec::new(), server("alpha"));
        assert!(draw(80, 24, &view).contains("No Notifications"));
        view.set_load_state(LoadState::Loading);
        assert!(draw(80, 24, &view).contains("Loading History"));
        view.set_load_state(LoadState::Error("database busy".into()));
        assert!(draw(80, 24, &view).contains("error: database busy"));
        view.set_load_state(LoadState::Disconnected);
        assert!(draw(80, 24, &view).contains("History disconnected"));
        view.set_load_state(LoadState::Ready);
        view.set_display_modes(false, false);
        view.help_visible = true;
        let rendered = draw(80, 24, &view);
        assert!(rendered.contains("* History"));
        assert!(rendered.contains("smart-case filter"));
    }
}
