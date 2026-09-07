//! Provider ingress adapters.
//!
//! Provider JSON is deserialized only in this module and converted immediately
//! into the normalized domain model. Raw payloads and transcript paths have no
//! representation in the returned value.

mod claude;
mod codex;

pub use crate::config::HookPreset;

use crate::config::{HookEvent, HookProviderConfig};
use crate::notification::{
    AgentEventKind, IngressError, Level, NormalizedMetadata, NotificationDraft, NotificationKey,
    Presentation, Priority, Provider, SourceContext,
};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookPolicy {
    preset: HookPreset,
    enable: Vec<AgentEventKind>,
    disable: Vec<AgentEventKind>,
}

impl HookPolicy {
    #[must_use]
    pub fn new(preset: HookPreset) -> Self {
        Self {
            preset,
            enable: Vec::new(),
            disable: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_enabled(mut self, events: impl IntoIterator<Item = AgentEventKind>) -> Self {
        self.enable.extend(events);
        self
    }

    #[must_use]
    pub fn with_disabled(mut self, events: impl IntoIterator<Item = AgentEventKind>) -> Self {
        self.disable.extend(events);
        self
    }

    #[must_use]
    pub fn is_enabled(&self, event: AgentEventKind) -> bool {
        if self.disable.contains(&event) {
            return false;
        }
        self.enable.contains(&event) || self.preset_events().contains(&event)
    }

    fn preset_events(&self) -> &'static [AgentEventKind] {
        use AgentEventKind::{
            Completed, Failed, Interrupted, NeedsAttention, Started, SubagentCompleted,
            ToolCompleted, ToolStarted,
        };
        match self.preset {
            HookPreset::Minimal => &[NeedsAttention, Completed, Failed],
            HookPreset::Normal => &[
                NeedsAttention,
                Completed,
                Failed,
                SubagentCompleted,
                Interrupted,
            ],
            HookPreset::Verbose => &[
                NeedsAttention,
                Completed,
                Failed,
                SubagentCompleted,
                Interrupted,
                Started,
                ToolStarted,
                ToolCompleted,
            ],
        }
    }
}

impl From<&HookProviderConfig> for HookPolicy {
    fn from(config: &HookProviderConfig) -> Self {
        Self::new(config.preset)
            .with_enabled(config.enable.iter().copied().map(event_kind))
            .with_disabled(config.disable.iter().copied().map(event_kind))
    }
}

fn event_kind(event: HookEvent) -> AgentEventKind {
    match event {
        HookEvent::NeedsAttention => AgentEventKind::NeedsAttention,
        HookEvent::Completed => AgentEventKind::Completed,
        HookEvent::Failed => AgentEventKind::Failed,
        HookEvent::Started => AgentEventKind::Started,
        HookEvent::Interrupted => AgentEventKind::Interrupted,
        HookEvent::SubagentCompleted => AgentEventKind::SubagentCompleted,
        HookEvent::ToolStarted => AgentEventKind::ToolStarted,
        HookEvent::ToolCompleted => AgentEventKind::ToolCompleted,
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider hook input exceeds the {limit}-byte limit")]
    InputTooLarge { limit: usize },
    #[error("invalid provider hook JSON: {0}")]
    InvalidJson(serde_json::Error),
    #[error("invalid provider hook input: {0}")]
    InvalidInput(&'static str),
    #[error("provider event failed ingress normalization: {0}")]
    Ingress(#[from] IngressError),
}

pub const MAX_PROVIDER_INPUT_BYTES: usize = 64 * 1024;

/// The single provider-facing operation used by the hook receiver.
pub fn normalize(
    provider: Provider,
    bounded_json: &[u8],
    policy: &HookPolicy,
    source: Option<SourceContext>,
) -> Result<Option<NotificationDraft>, ProviderError> {
    if bounded_json.len() > MAX_PROVIDER_INPUT_BYTES {
        return Err(ProviderError::InputTooLarge {
            limit: MAX_PROVIDER_INPUT_BYTES,
        });
    }
    match provider {
        Provider::Claude => claude::normalize(bounded_json, policy, source),
        Provider::Codex => codex::normalize(bounded_json, policy, source),
    }
}

struct DraftParts<'a> {
    provider: Provider,
    provider_name: &'static str,
    session_id: &'a str,
    kind: AgentEventKind,
    purpose: &'static str,
    presentation: Presentation,
    level: Level,
    priority: Priority,
    title: &'a str,
    body: &'a str,
    tool_name: Option<&'a str>,
}

fn make_draft(
    parts: DraftParts<'_>,
    source: Option<SourceContext>,
) -> Result<NotificationDraft, ProviderError> {
    let source = source
        .map(|source| source.with_provider_session(parts.provider, parts.session_id))
        .transpose()?;
    let key = NotificationKey::new(&format!(
        "{}:{}:{}",
        parts.provider_name, parts.session_id, parts.purpose
    ))?;
    let mut metadata = NormalizedMetadata::new(Some(parts.kind));
    if let Some(tool_name) = parts.tool_name {
        metadata = metadata.with_tool_name(tool_name)?;
    }
    Ok(
        NotificationDraft::new(parts.presentation, parts.title, parts.body, source)?
            .with_key(key)
            .with_level(parts.level)
            .with_priority(parts.priority)
            .with_metadata(metadata),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_wins_after_preset_and_enable_expansion() {
        let policy = HookPolicy::new(HookPreset::Minimal)
            .with_enabled([AgentEventKind::ToolStarted])
            .with_disabled([AgentEventKind::Completed, AgentEventKind::ToolStarted]);
        assert!(policy.is_enabled(AgentEventKind::NeedsAttention));
        assert!(!policy.is_enabled(AgentEventKind::Completed));
        assert!(!policy.is_enabled(AgentEventKind::ToolStarted));
    }

    #[test]
    fn oversized_input_is_rejected_before_deserialization() {
        let error = normalize(
            Provider::Claude,
            &vec![b'x'; MAX_PROVIDER_INPUT_BYTES + 1],
            &HookPolicy::new(HookPreset::Minimal),
            None,
        )
        .unwrap_err();
        assert!(matches!(error, ProviderError::InputTooLarge { .. }));
    }
}
