use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::{PaneId, WindowId};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Topology {
    pub clients: Vec<ClientView>,
    pub panes: BTreeMap<PaneId, Pane>,
}

impl Topology {
    /// Parses output from the fixed, tab-separated formats owned by this module.
    pub(super) fn from_format_output(
        clients: &str,
        panes: &str,
    ) -> Result<Self, TopologyParseError> {
        let clients = clients
            .lines()
            .filter(|line| !line.is_empty())
            .map(parse_client)
            .collect::<Result<Vec<_>, _>>()?;
        let panes = panes
            .lines()
            .filter(|line| !line.is_empty())
            .map(parse_pane)
            .map(|result| result.map(|pane| (pane.id.clone(), pane)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        Ok(Self { clients, panes })
    }

    /// Distinct Attention Windows, excluding infrastructure control clients.
    pub fn eligible_windows(&self) -> BTreeSet<WindowId> {
        self.clients
            .iter()
            .filter(|client| !client.is_control)
            .map(|client| client.window_id.clone())
            .collect()
    }

    /// Best available size for a window from its tiled panes. Floating panes
    /// are excluded because their extents do not describe the underlying
    /// Attention Window.
    #[must_use]
    pub fn window_size(&self, window_id: &WindowId) -> Option<WindowSize> {
        let mut width = 0_u16;
        let mut height = 0_u16;
        let mut found = false;
        for pane in self
            .panes
            .values()
            .filter(|pane| pane.window_id == *window_id && !pane.is_floating)
        {
            found = true;
            width = width.max(pane.left.saturating_add(pane.width));
            height = height.max(pane.top.saturating_add(pane.height));
        }
        found.then_some(WindowSize { width, height })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowSize {
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientView {
    pub name: String,
    pub is_control: bool,
    pub session_id: String,
    pub window_id: WindowId,
    pub last_activity: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pane {
    pub id: PaneId,
    pub session_id: String,
    pub window_id: WindowId,
    pub is_floating: bool,
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopologyParseError {
    record: &'static str,
    detail: String,
}

impl fmt::Display for TopologyParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid tmux {} record: {}",
            self.record, self.detail
        )
    }
}

impl std::error::Error for TopologyParseError {}

fn parse_client(line: &str) -> Result<ClientView, TopologyParseError> {
    let fields: Vec<_> = line.split('\t').collect();
    if fields.len() != 5 {
        return Err(invalid("client", "expected 5 tab-separated fields"));
    }
    Ok(ClientView {
        name: fields[0].to_owned(),
        is_control: parse_bool("client", fields[1])?,
        session_id: stable_id("client", fields[2], '$')?,
        window_id: WindowId(stable_id("client", fields[3], '@')?),
        last_activity: parse_number("client", fields[4])?,
    })
}

fn parse_pane(line: &str) -> Result<Pane, TopologyParseError> {
    let fields: Vec<_> = line.split('\t').collect();
    if fields.len() != 8 {
        return Err(invalid("pane", "expected 8 tab-separated fields"));
    }
    Ok(Pane {
        id: PaneId(stable_id("pane", fields[0], '%')?),
        session_id: stable_id("pane", fields[1], '$')?,
        window_id: WindowId(stable_id("pane", fields[2], '@')?),
        is_floating: parse_bool("pane", fields[3])?,
        left: parse_number("pane", fields[4])?,
        top: parse_number("pane", fields[5])?,
        width: parse_number("pane", fields[6])?,
        height: parse_number("pane", fields[7])?,
    })
}

fn stable_id(
    record: &'static str,
    value: &str,
    prefix: char,
) -> Result<String, TopologyParseError> {
    if value
        .strip_prefix(prefix)
        .is_some_and(|id| !id.is_empty() && id.chars().all(|character| character.is_ascii_digit()))
    {
        Ok(value.to_owned())
    } else {
        Err(invalid(record, &format!("invalid stable ID {value:?}")))
    }
}

fn parse_bool(record: &'static str, value: &str) -> Result<bool, TopologyParseError> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(invalid(record, &format!("invalid boolean {value:?}"))),
    }
}

fn parse_number<T>(record: &'static str, value: &str) -> Result<T, TopologyParseError>
where
    T: std::str::FromStr,
{
    value
        .parse()
        .map_err(|_| invalid(record, &format!("invalid number {value:?}")))
}

fn invalid(record: &'static str, detail: &str) -> TopologyParseError {
    TopologyParseError {
        record,
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_topology_and_deduplicates_eligible_windows() {
        let topology = Topology::from_format_output(
            "client-a\t0\t$0\t@1\t100\nclient-b\t0\t$2\t@1\t110\ntmnotify-control\t1\t$0\t@9\t120\n",
            "%3\t$0\t@1\t0\t0\t0\t80\t24\n%8\t$0\t@9\t1\t4\t3\t40\t8\n",
        )
        .unwrap();

        assert_eq!(topology.eligible_windows(), [WindowId("@1".into())].into());
        assert!(topology.panes[&PaneId("%8".into())].is_floating);
        assert_eq!(
            topology.window_size(&WindowId("@1".into())),
            Some(WindowSize {
                width: 80,
                height: 24
            })
        );
    }

    #[test]
    fn rejects_names_and_indexes_where_stable_ids_are_required() {
        let error = Topology::from_format_output("client-a\t0\twork\t1\t100\n", "").unwrap_err();
        assert!(error.to_string().contains("stable ID"));
    }

    #[test]
    fn rejects_truncated_records() {
        let error = Topology::from_format_output("client-a\t0\t$0\n", "").unwrap_err();
        assert!(error.to_string().contains("expected 5"));
    }
}
