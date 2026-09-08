use std::collections::VecDeque;
use std::fmt;

use super::Event;

const MAX_COMMAND_OUTPUT_LINES: usize = 4_096;
const MAX_COMMAND_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct CommandTicket(pub(super) u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CommandResult {
    pub(super) ticket: CommandTicket,
    pub(super) output: Vec<String>,
    pub(super) success: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ControlItem {
    Command(CommandResult),
    Event(Event),
}

/// Correlates caller tickets with tmux's `%begin`/`%end` command numbers.
///
/// Commands on one control connection complete in submission order. tmux may
/// interleave asynchronous notifications with a response block, so they are
/// translated separately and never become command output.
#[derive(Debug, Default)]
pub(super) struct ControlParser {
    pending: VecDeque<CommandTicket>,
    active: Option<Active>,
}

#[derive(Debug)]
struct Active {
    ticket: CommandTicket,
    tmux_command: u64,
    output: Vec<String>,
    output_bytes: usize,
}

impl ControlParser {
    pub(super) fn submitted(&mut self, ticket: CommandTicket) {
        self.pending.push_back(ticket);
    }

    pub(super) fn receive(&mut self, line: &str) -> Result<Vec<ControlItem>, ProtocolError> {
        if line.starts_with("%begin ") {
            return self.begin(line).map(|()| Vec::new());
        }
        if line.starts_with("%end ") || line.starts_with("%error ") {
            return self
                .finish(line)
                .map(|result| vec![ControlItem::Command(result)]);
        }
        if let Some(event) = translate_notification(line) {
            return Ok(vec![ControlItem::Event(event)]);
        }
        if let Some(active) = &mut self.active {
            if active.output.len() >= MAX_COMMAND_OUTPUT_LINES
                || active.output_bytes.saturating_add(line.len()) > MAX_COMMAND_OUTPUT_BYTES
            {
                return Err(ProtocolError::new(
                    "command response exceeded the bounded output limit",
                ));
            }
            active.output_bytes += line.len();
            active.output.push(line.to_owned());
        }
        Ok(Vec::new())
    }

    fn begin(&mut self, line: &str) -> Result<(), ProtocolError> {
        if self.active.is_some() {
            return Err(ProtocolError::new("nested %begin record"));
        }
        let command = frame_command(line)?;
        let ticket = self
            .pending
            .pop_front()
            .ok_or_else(|| ProtocolError::new("%begin without a submitted command"))?;
        self.active = Some(Active {
            ticket,
            tmux_command: command,
            output: Vec::new(),
            output_bytes: 0,
        });
        Ok(())
    }

    fn finish(&mut self, line: &str) -> Result<CommandResult, ProtocolError> {
        let command = frame_command(line)?;
        let active = self
            .active
            .take()
            .ok_or_else(|| ProtocolError::new("completion without %begin"))?;
        if active.tmux_command != command {
            return Err(ProtocolError::new(
                "completion command number did not match %begin",
            ));
        }
        Ok(CommandResult {
            ticket: active.ticket,
            output: active.output,
            success: line.starts_with("%end "),
        })
    }
}

fn frame_command(line: &str) -> Result<u64, ProtocolError> {
    let fields: Vec<_> = line.split_whitespace().collect();
    if fields.len() != 4 {
        return Err(ProtocolError::new(
            "frame must contain time, command, and flags",
        ));
    }
    fields[2]
        .parse()
        .map_err(|_| ProtocolError::new("frame command number was not numeric"))
}

fn translate_notification(line: &str) -> Option<Event> {
    if line == "%exit" || line.starts_with("%exit ") {
        return Some(Event::Disconnected);
    }
    if !line.starts_with('%') {
        return None;
    }
    let name = line.split_once(' ').map_or(line, |(name, _)| name);
    if matches!(
        name,
        "%client-attached"
            | "%client-detached"
            | "%client-session-changed"
            | "%layout-change"
            | "%session-changed"
            | "%session-window-changed"
            | "%sessions-changed"
            | "%unlinked-window-add"
            | "%unlinked-window-close"
            | "%window-add"
            | "%window-close"
            | "%window-pane-changed"
            | "%window-renamed"
    ) {
        Some(Event::TopologyChanged)
    } else {
        None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ProtocolError {
    detail: String,
}

impl ProtocolError {
    fn new(detail: &str) -> Self {
        Self {
            detail: detail.to_owned(),
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;

    const OBSERVED_FIXTURE: &str = include_str!("../tests/fixtures/tmux/control-mode-3.8.txt");

    #[test]
    fn correlates_response_while_translating_interleaved_events() {
        let mut codec = ControlParser::default();
        codec.submitted(CommandTicket(41));
        let items = OBSERVED_FIXTURE
            .lines()
            .flat_map(|line| codec.receive(line).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            items,
            vec![
                ControlItem::Event(Event::TopologyChanged),
                ControlItem::Command(CommandResult {
                    ticket: CommandTicket(41),
                    output: vec!["$0|@0|%0".into()],
                    success: true,
                }),
                ControlItem::Event(Event::TopologyChanged),
                ControlItem::Event(Event::TopologyChanged),
                ControlItem::Event(Event::TopologyChanged),
                ControlItem::Event(Event::TopologyChanged),
                ControlItem::Event(Event::Disconnected),
            ]
        );
    }

    #[test]
    fn correlates_failures_and_keeps_error_text() {
        let mut codec = ControlParser::default();
        codec.submitted(CommandTicket(7));
        codec.receive("%begin 10 22 1").unwrap();
        codec.receive("can't find pane: %99").unwrap();
        let items = codec.receive("%error 10 22 1").unwrap();

        assert_eq!(
            items,
            vec![ControlItem::Command(CommandResult {
                ticket: CommandTicket(7),
                output: vec!["can't find pane: %99".into()],
                success: false,
            })]
        );
    }

    #[test]
    fn malformed_or_uncorrelated_frames_are_errors() {
        let mut codec = ControlParser::default();
        assert!(codec.receive("%begin bad").is_err());
        assert!(codec.receive("%end 1 2 0").is_err());
    }

    #[test]
    fn pane_output_is_not_mistaken_for_a_topology_event() {
        let mut codec = ControlParser::default();
        assert!(codec.receive("%output %1 hello").unwrap().is_empty());
    }

    #[test]
    fn command_output_line_and_byte_counts_are_bounded() {
        let mut by_lines = ControlParser::default();
        by_lines.submitted(CommandTicket(1));
        by_lines.receive("%begin 1 1 0").unwrap();
        for _ in 0..MAX_COMMAND_OUTPUT_LINES {
            by_lines.receive("").unwrap();
        }
        assert!(by_lines.receive("").is_err());

        let mut by_bytes = ControlParser::default();
        by_bytes.submitted(CommandTicket(2));
        by_bytes.receive("%begin 1 2 0").unwrap();
        by_bytes
            .receive(&"x".repeat(MAX_COMMAND_OUTPUT_BYTES))
            .unwrap();
        assert!(by_bytes.receive("x").is_err());
    }
}
