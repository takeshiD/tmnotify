use serde::Deserialize;

use super::{DraftParts, HookPolicy, ProviderError, make_draft};
use crate::notification::{
    AgentEventKind, Level, NotificationDraft, Presentation, Priority, Provider, SourceContext,
};

#[derive(Deserialize)]
struct CommonInput {
    session_id: String,
    hook_event_name: String,
}

#[derive(Deserialize)]
struct ToolInput {
    session_id: String,
    tool_name: String,
}

#[derive(Deserialize)]
struct StopInput {
    session_id: String,
    last_assistant_message: Option<String>,
}

#[derive(Deserialize)]
struct SubagentInput {
    session_id: String,
    agent_type: String,
    last_assistant_message: Option<String>,
}

pub(super) fn normalize(
    input: &[u8],
    policy: &HookPolicy,
    source: Option<SourceContext>,
) -> Result<Option<NotificationDraft>, ProviderError> {
    let common: CommonInput = serde_json::from_slice(input).map_err(ProviderError::InvalidJson)?;
    match common.hook_event_name.as_str() {
        "PermissionRequest" => permission(input, policy, source),
        "Stop" => stop(input, policy, source),
        "SubagentStop" => subagent(input, policy, source),
        "Interrupt" => simple(
            &common.session_id,
            AgentEventKind::Interrupted,
            "interrupted",
            "Codex was interrupted",
            policy,
            source,
        ),
        "SessionStart" => simple(
            &common.session_id,
            AgentEventKind::Started,
            "started",
            "Codex started",
            policy,
            source,
        ),
        "PreToolUse" => tool(input, policy, source, AgentEventKind::ToolStarted),
        "PostToolUse" => tool(input, policy, source, AgentEventKind::ToolCompleted),
        _ => Ok(None),
    }
}

fn permission(
    input: &[u8],
    policy: &HookPolicy,
    source: Option<SourceContext>,
) -> Result<Option<NotificationDraft>, ProviderError> {
    let event: ToolInput = serde_json::from_slice(input).map_err(ProviderError::InvalidJson)?;
    if !policy.is_enabled(AgentEventKind::NeedsAttention) {
        return Ok(None);
    }
    let body = format!("Permission requested for {}", event.tool_name);
    make_draft(
        DraftParts {
            provider: Provider::Codex,
            provider_name: "codex",
            session_id: &event.session_id,
            kind: AgentEventKind::NeedsAttention,
            purpose: "attention",
            presentation: Presentation::Attention,
            level: Level::Warning,
            priority: Priority::High,
            title: "Codex needs attention",
            body: &body,
            tool_name: Some(&event.tool_name),
        },
        source,
    )
    .map(Some)
}

fn stop(
    input: &[u8],
    policy: &HookPolicy,
    source: Option<SourceContext>,
) -> Result<Option<NotificationDraft>, ProviderError> {
    let event: StopInput = serde_json::from_slice(input).map_err(ProviderError::InvalidJson)?;
    if !policy.is_enabled(AgentEventKind::Completed) {
        return Ok(None);
    }
    make_draft(
        DraftParts {
            provider: Provider::Codex,
            provider_name: "codex",
            session_id: &event.session_id,
            kind: AgentEventKind::Completed,
            purpose: "completed",
            presentation: Presentation::Toast,
            level: Level::Success,
            priority: Priority::Normal,
            title: "Codex",
            body: event
                .last_assistant_message
                .as_deref()
                .unwrap_or("Completed"),
            tool_name: None,
        },
        source,
    )
    .map(Some)
}

fn subagent(
    input: &[u8],
    policy: &HookPolicy,
    source: Option<SourceContext>,
) -> Result<Option<NotificationDraft>, ProviderError> {
    let event: SubagentInput = serde_json::from_slice(input).map_err(ProviderError::InvalidJson)?;
    if !policy.is_enabled(AgentEventKind::SubagentCompleted) {
        return Ok(None);
    }
    make_draft(
        DraftParts {
            provider: Provider::Codex,
            provider_name: "codex",
            session_id: &event.session_id,
            kind: AgentEventKind::SubagentCompleted,
            purpose: "subagent-completed",
            presentation: Presentation::Toast,
            level: Level::Success,
            priority: Priority::Normal,
            title: &format!("Codex {}", event.agent_type),
            body: event
                .last_assistant_message
                .as_deref()
                .unwrap_or("Subagent completed"),
            tool_name: None,
        },
        source,
    )
    .map(Some)
}

fn tool(
    input: &[u8],
    policy: &HookPolicy,
    source: Option<SourceContext>,
    kind: AgentEventKind,
) -> Result<Option<NotificationDraft>, ProviderError> {
    let event: ToolInput = serde_json::from_slice(input).map_err(ProviderError::InvalidJson)?;
    if !policy.is_enabled(kind) {
        return Ok(None);
    }
    let (purpose, body) = match kind {
        AgentEventKind::ToolStarted => ("tool-started", "Tool started"),
        AgentEventKind::ToolCompleted => ("tool-completed", "Tool completed"),
        _ => return Err(ProviderError::InvalidInput("invalid tool event kind")),
    };
    make_draft(
        DraftParts {
            provider: Provider::Codex,
            provider_name: "codex",
            session_id: &event.session_id,
            kind,
            purpose,
            presentation: Presentation::Toast,
            level: Level::Info,
            priority: Priority::Low,
            title: "Codex",
            body,
            tool_name: Some(&event.tool_name),
        },
        source,
    )
    .map(Some)
}

fn simple(
    session_id: &str,
    kind: AgentEventKind,
    purpose: &'static str,
    body: &'static str,
    policy: &HookPolicy,
    source: Option<SourceContext>,
) -> Result<Option<NotificationDraft>, ProviderError> {
    if !policy.is_enabled(kind) {
        return Ok(None);
    }
    make_draft(
        DraftParts {
            provider: Provider::Codex,
            provider_name: "codex",
            session_id,
            kind,
            purpose,
            presentation: Presentation::Toast,
            level: Level::Info,
            priority: Priority::Low,
            title: "Codex",
            body,
            tool_name: None,
        },
        source,
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::notification::{Notification, TmuxServerId};
    use crate::providers::HookPreset;

    const FIXTURE_SOURCE: &str = "https://learn.chatgpt.com/docs/hooks";
    const VERIFIED: &str = "2026-09-08";

    fn source() -> SourceContext {
        SourceContext::new(TmuxServerId::new("server").unwrap(), "$1", "@2", "%3").unwrap()
    }

    fn event(json: &str, preset: HookPreset) -> Option<Notification> {
        normalize(json.as_bytes(), &HookPolicy::new(preset), Some(source()))
            .unwrap()
            .map(|draft| Notification::from_draft(draft, Utc::now()))
    }

    #[test]
    fn fixture_provenance_is_recorded() {
        assert!(FIXTURE_SOURCE.starts_with("https://learn.chatgpt.com/"));
        assert_eq!(VERIFIED, "2026-09-08");
    }

    #[test]
    fn minimal_maps_permission_and_stop_without_raw_tool_input() {
        let permission = event(
            r#"{"session_id":"thr_1","hook_event_name":"PermissionRequest","turn_id":"turn_1","tool_name":"Bash","tool_input":{"command":"secret"},"transcript_path":"/secret"}"#,
            HookPreset::Minimal,
        )
        .unwrap();
        assert_eq!(permission.presentation(), Presentation::Attention);
        assert_eq!(permission.metadata().tool_name(), Some("Bash"));
        assert!(
            !serde_json::to_string(&permission)
                .unwrap()
                .contains("secret")
        );

        let completed = event(
            r#"{"session_id":"thr_1","hook_event_name":"Stop","turn_id":"turn_1","stop_hook_active":false,"last_assistant_message":"done"}"#,
            HookPreset::Minimal,
        )
        .unwrap();
        assert_eq!(completed.level(), Level::Success);
        assert_eq!(completed.body(), "done");
    }

    #[test]
    fn normal_adds_interruption_and_subagent_completion() {
        assert!(
            event(
                r#"{"session_id":"thr_1","hook_event_name":"Interrupt","turn_id":"turn_1"}"#,
                HookPreset::Minimal
            )
            .is_none()
        );
        assert!(
            event(
                r#"{"session_id":"thr_1","hook_event_name":"Interrupt","turn_id":"turn_1"}"#,
                HookPreset::Normal
            )
            .is_some()
        );
        assert!(event(r#"{"session_id":"thr_1","hook_event_name":"SubagentStop","agent_type":"worker","last_assistant_message":"done"}"#, HookPreset::Normal).is_some());
    }

    #[test]
    fn unknown_events_are_ignored_and_required_types_are_checked() {
        assert!(
            event(
                r#"{"session_id":"s1","hook_event_name":"Future"}"#,
                HookPreset::Verbose
            )
            .is_none()
        );
        assert!(
            normalize(
                br#"{"session_id":"s1","hook_event_name":"PermissionRequest","tool_name":1}"#,
                &HookPolicy::new(HookPreset::Minimal),
                Some(source())
            )
            .is_err()
        );
    }
}
