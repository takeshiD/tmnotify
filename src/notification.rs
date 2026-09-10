//! The normalized Notification model.
//!
//! All externally supplied text crosses this module's constructors before it
//! can reach scheduling, persistence, rendering, or output code.

use std::{fmt, path::PathBuf, str::FromStr, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

/// Maximum size of one newline-delimited IPC request.
pub const MAX_IPC_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_TITLE_BYTES: usize = 1024;
pub const MAX_BODY_BYTES: usize = 48 * 1024;
pub const MAX_KEY_BYTES: usize = 1024;
pub const MAX_SOURCE_ID_BYTES: usize = 256;
pub const MAX_SOURCE_VALUE_BYTES: usize = 1024;
pub const MAX_CWD_BYTES: usize = 4096;
pub const MAX_METADATA_VALUE_BYTES: usize = 1024;

const TAB_WIDTH: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IngressError {
    RequestTooLarge {
        actual: usize,
        maximum: usize,
    },
    ValueTooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    EmptyValue {
        field: &'static str,
    },
    InvalidTmuxId {
        field: &'static str,
    },
    AttentionRequiresSource,
    InvalidNotificationId,
}

impl fmt::Display for IngressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequestTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "IPC request is {actual} bytes; maximum is {maximum}"
                )
            }
            Self::ValueTooLarge {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "{field} is {actual} bytes after normalization; maximum is {maximum}"
            ),
            Self::EmptyValue { field } => write!(formatter, "{field} must not be empty"),
            Self::InvalidTmuxId { field } => write!(formatter, "{field} is not a stable tmux ID"),
            Self::AttentionRequiresSource => {
                formatter.write_str("Attention requires a valid Source Pane")
            }
            Self::InvalidNotificationId => formatter.write_str("invalid Notification ID"),
        }
    }
}

impl std::error::Error for IngressError {}

/// Reject an IPC frame before deserialization allocates values from it.
pub fn validate_ipc_request_size(bytes: &[u8]) -> Result<(), IngressError> {
    if bytes.len() > MAX_IPC_REQUEST_BYTES {
        return Err(IngressError::RequestTooLarge {
            actual: bytes.len(),
            maximum: MAX_IPC_REQUEST_BYTES,
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NotificationId(Uuid);

impl NotificationId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    #[must_use]
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for NotificationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for NotificationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for NotificationId {
    type Err = IngressError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = Uuid::parse_str(value).map_err(|_| IngressError::InvalidNotificationId)?;
        if id.get_version_num() != 7 {
            return Err(IngressError::InvalidNotificationId);
        }
        Ok(Self(id))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NotificationKey(String);

impl NotificationKey {
    pub fn new(value: &str) -> Result<Self, IngressError> {
        Ok(Self(normalize_required(
            "Notification Key",
            value,
            MAX_KEY_BYTES,
        )?))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presentation {
    Toast,
    Attention,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
    Critical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Timeout {
    After(Duration),
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Pending,
    Visible,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    TimedOut,
    Dismissed,
    Jumped,
    RenderSuppressed,
    RenderFailed,
    DaemonInterrupted,
    DaemonStopped,
    ServerEnded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Codex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventKind {
    NeedsAttention,
    Completed,
    Failed,
    Started,
    Interrupted,
    SubagentCompleted,
    ToolStarted,
    ToolCompleted,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TmuxServerId(String);

impl TmuxServerId {
    pub fn new(value: &str) -> Result<Self, IngressError> {
        Ok(Self(normalize_required(
            "tmux server ID",
            value,
            MAX_SOURCE_ID_BYTES,
        )?))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceContext {
    provider: Option<Provider>,
    provider_session_id: Option<String>,
    tmux_server_id: TmuxServerId,
    session_id: String,
    window_id: String,
    pane_id: String,
    cwd: Option<PathBuf>,
    command: Option<String>,
    pane_title: Option<String>,
}

impl SourceContext {
    pub fn new(
        tmux_server_id: TmuxServerId,
        session_id: &str,
        window_id: &str,
        pane_id: &str,
    ) -> Result<Self, IngressError> {
        Ok(Self {
            provider: None,
            provider_session_id: None,
            tmux_server_id,
            session_id: stable_tmux_id("session ID", session_id, '$')?,
            window_id: stable_tmux_id("window ID", window_id, '@')?,
            pane_id: stable_tmux_id("pane ID", pane_id, '%')?,
            cwd: None,
            command: None,
            pane_title: None,
        })
    }

    pub fn with_provider_session(
        mut self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Self, IngressError> {
        self.provider = Some(provider);
        self.provider_session_id = Some(normalize_required(
            "provider session ID",
            session_id,
            MAX_SOURCE_VALUE_BYTES,
        )?);
        Ok(self)
    }

    pub fn with_cwd(mut self, cwd: &str) -> Result<Self, IngressError> {
        self.cwd = Some(PathBuf::from(normalize_required(
            "source cwd",
            cwd,
            MAX_CWD_BYTES,
        )?));
        Ok(self)
    }

    pub fn with_command(mut self, command: &str) -> Result<Self, IngressError> {
        self.command = Some(normalize_required(
            "source command",
            command,
            MAX_SOURCE_VALUE_BYTES,
        )?);
        Ok(self)
    }

    pub fn with_pane_title(mut self, title: &str) -> Result<Self, IngressError> {
        self.pane_title = Some(normalize_bounded(
            "source pane title",
            title,
            MAX_SOURCE_VALUE_BYTES,
        )?);
        Ok(self)
    }

    #[must_use]
    pub fn provider(&self) -> Option<Provider> {
        self.provider
    }

    #[must_use]
    pub fn provider_session_id(&self) -> Option<&str> {
        self.provider_session_id.as_deref()
    }

    #[must_use]
    pub fn tmux_server_id(&self) -> &TmuxServerId {
        &self.tmux_server_id
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn window_id(&self) -> &str {
        &self.window_id
    }

    #[must_use]
    pub fn pane_id(&self) -> &str {
        &self.pane_id
    }

    #[must_use]
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    #[must_use]
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    #[must_use]
    pub fn pane_title(&self) -> Option<&str> {
        self.pane_title.as_deref()
    }
}

/// Explicitly allowlisted metadata. Raw provider JSON, transcript paths,
/// complete argv, and arbitrary key/value fields have no representation here.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct NormalizedMetadata {
    agent_event_kind: Option<AgentEventKind>,
    agent_name: Option<String>,
    tool_name: Option<String>,
}

impl NormalizedMetadata {
    #[must_use]
    pub fn new(agent_event_kind: Option<AgentEventKind>) -> Self {
        Self {
            agent_event_kind,
            agent_name: None,
            tool_name: None,
        }
    }

    pub fn with_agent_name(mut self, value: &str) -> Result<Self, IngressError> {
        self.agent_name = Some(normalize_required(
            "agent name",
            value,
            MAX_METADATA_VALUE_BYTES,
        )?);
        Ok(self)
    }

    pub fn with_tool_name(mut self, value: &str) -> Result<Self, IngressError> {
        self.tool_name = Some(normalize_required(
            "tool name",
            value,
            MAX_METADATA_VALUE_BYTES,
        )?);
        Ok(self)
    }

    #[must_use]
    pub fn agent_event_kind(&self) -> Option<AgentEventKind> {
        self.agent_event_kind
    }

    #[must_use]
    pub fn agent_name(&self) -> Option<&str> {
        self.agent_name.as_deref()
    }

    #[must_use]
    pub fn tool_name(&self) -> Option<&str> {
        self.tool_name.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PresentationOverrides {
    position: Option<Placement>,
}

impl PresentationOverrides {
    #[must_use]
    pub fn with_position(mut self, position: Placement) -> Self {
        self.position = Some(position);
        self
    }

    #[must_use]
    pub fn position(&self) -> Option<Placement> {
        self.position
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationDraft {
    key: Option<NotificationKey>,
    level: Level,
    priority: Priority,
    presentation: Presentation,
    title: String,
    body: String,
    timeout: Timeout,
    source: Option<SourceContext>,
    overrides: PresentationOverrides,
    metadata: NormalizedMetadata,
}

/// Validated, partial changes for a live Notification.
///
/// Identity, presentation, and Source Context are deliberately immutable here.
/// A producer that wants to replace all send-time content can use keyed upsert.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NotificationUpdate {
    title: Option<String>,
    body: Option<String>,
    level: Option<Level>,
    priority: Option<Priority>,
    timeout: Option<Timeout>,
    overrides: Option<PresentationOverrides>,
}

impl NotificationUpdate {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_title(mut self, value: &str) -> Result<Self, IngressError> {
        self.title = Some(normalize_bounded(
            "Notification title",
            value,
            MAX_TITLE_BYTES,
        )?);
        Ok(self)
    }

    pub fn with_body(mut self, value: &str) -> Result<Self, IngressError> {
        self.body = Some(normalize_bounded(
            "Notification body",
            value,
            MAX_BODY_BYTES,
        )?);
        Ok(self)
    }

    #[must_use]
    pub fn with_level(mut self, value: Level) -> Self {
        self.level = Some(value);
        self
    }

    #[must_use]
    pub fn with_priority(mut self, value: Priority) -> Self {
        self.priority = Some(value);
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, value: Timeout) -> Self {
        self.timeout = Some(value);
        self
    }

    #[must_use]
    pub fn with_overrides(mut self, value: PresentationOverrides) -> Self {
        self.overrides = Some(value);
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.body.is_none()
            && self.level.is_none()
            && self.priority.is_none()
            && self.timeout.is_none()
            && self.overrides.is_none()
    }

    pub(crate) fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub(crate) fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    pub(crate) fn level(&self) -> Option<Level> {
        self.level
    }

    pub(crate) fn priority(&self) -> Option<Priority> {
        self.priority
    }

    pub(crate) fn timeout(&self) -> Option<Timeout> {
        self.timeout
    }

    pub(crate) fn overrides(&self) -> Option<&PresentationOverrides> {
        self.overrides.as_ref()
    }
}

impl NotificationDraft {
    pub fn new(
        presentation: Presentation,
        title: &str,
        body: &str,
        source: Option<SourceContext>,
    ) -> Result<Self, IngressError> {
        if presentation == Presentation::Attention && source.is_none() {
            return Err(IngressError::AttentionRequiresSource);
        }
        Ok(Self {
            key: None,
            level: Level::Info,
            priority: Priority::Normal,
            presentation,
            title: normalize_bounded("Notification title", title, MAX_TITLE_BYTES)?,
            body: normalize_bounded("Notification body", body, MAX_BODY_BYTES)?,
            timeout: if presentation == Presentation::Attention {
                Timeout::Never
            } else {
                Timeout::After(Duration::from_secs(3))
            },
            source,
            overrides: PresentationOverrides::default(),
            metadata: NormalizedMetadata::default(),
        })
    }

    pub fn with_key(mut self, key: NotificationKey) -> Self {
        self.key = Some(key);
        self
    }

    pub fn with_level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_timeout(mut self, timeout: Timeout) -> Self {
        self.timeout = if self.presentation == Presentation::Attention {
            Timeout::Never
        } else {
            timeout
        };
        self
    }

    pub fn with_overrides(mut self, overrides: PresentationOverrides) -> Self {
        self.overrides = overrides;
        self
    }

    pub fn with_metadata(mut self, metadata: NormalizedMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    #[must_use]
    pub(crate) fn key(&self) -> Option<&NotificationKey> {
        self.key.as_ref()
    }

    pub(crate) fn level(&self) -> Level {
        self.level
    }

    pub(crate) fn priority(&self) -> Priority {
        self.priority
    }

    pub(crate) fn presentation(&self) -> Presentation {
        self.presentation
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn body(&self) -> &str {
        &self.body
    }

    pub(crate) fn timeout(&self) -> Timeout {
        self.timeout
    }

    pub(crate) fn source(&self) -> Option<&SourceContext> {
        self.source.as_ref()
    }

    pub(crate) fn overrides(&self) -> &PresentationOverrides {
        &self.overrides
    }

    pub(crate) fn metadata(&self) -> &NormalizedMetadata {
        &self.metadata
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Notification {
    id: NotificationId,
    key: Option<NotificationKey>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    level: Level,
    priority: Priority,
    presentation: Presentation,
    title: String,
    body: String,
    timeout: Timeout,
    source: Option<SourceContext>,
    overrides: PresentationOverrides,
    delivery: DeliveryState,
    close_reason: Option<CloseReason>,
    hidden_at: Option<DateTime<Utc>>,
    last_jumped_at: Option<DateTime<Utc>>,
    metadata: NormalizedMetadata,
}

impl Notification {
    #[must_use]
    pub fn from_draft(draft: NotificationDraft, now: DateTime<Utc>) -> Self {
        Self {
            id: NotificationId::new(),
            key: draft.key,
            created_at: now,
            updated_at: now,
            level: draft.level,
            priority: draft.priority,
            presentation: draft.presentation,
            title: draft.title,
            body: draft.body,
            timeout: draft.timeout,
            source: draft.source,
            overrides: draft.overrides,
            delivery: DeliveryState::Pending,
            close_reason: None,
            hidden_at: None,
            last_jumped_at: None,
            metadata: draft.metadata,
        }
    }

    #[must_use]
    pub fn id(&self) -> NotificationId {
        self.id
    }
    #[must_use]
    pub fn key(&self) -> Option<&NotificationKey> {
        self.key.as_ref()
    }
    #[must_use]
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    #[must_use]
    pub fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }
    #[must_use]
    pub fn level(&self) -> Level {
        self.level
    }
    #[must_use]
    pub fn priority(&self) -> Priority {
        self.priority
    }
    #[must_use]
    pub fn presentation(&self) -> Presentation {
        self.presentation
    }
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }
    #[must_use]
    pub fn timeout(&self) -> Timeout {
        self.timeout
    }
    #[must_use]
    pub fn source(&self) -> Option<&SourceContext> {
        self.source.as_ref()
    }
    #[must_use]
    pub fn overrides(&self) -> &PresentationOverrides {
        &self.overrides
    }
    #[must_use]
    pub fn delivery(&self) -> DeliveryState {
        self.delivery
    }
    #[must_use]
    pub fn close_reason(&self) -> Option<CloseReason> {
        self.close_reason
    }
    #[must_use]
    pub fn hidden_at(&self) -> Option<DateTime<Utc>> {
        self.hidden_at
    }
    #[must_use]
    pub fn last_jumped_at(&self) -> Option<DateTime<Utc>> {
        self.last_jumped_at
    }
    #[must_use]
    pub fn metadata(&self) -> &NormalizedMetadata {
        &self.metadata
    }

    pub(crate) fn has_same_content(&self, draft: &NotificationDraft) -> bool {
        self.key == draft.key
            && self.level == draft.level
            && self.priority == draft.priority
            && self.presentation == draft.presentation
            && self.title == draft.title
            && self.body == draft.body
            && self.timeout == draft.timeout
            && self.source == draft.source
            && self.overrides == draft.overrides
            && self.metadata == draft.metadata
    }

    pub(crate) fn replace_content(&mut self, draft: NotificationDraft, now: DateTime<Utc>) {
        self.key = draft.key;
        self.level = draft.level;
        self.priority = draft.priority;
        self.presentation = draft.presentation;
        self.title = draft.title;
        self.body = draft.body;
        self.timeout = draft.timeout;
        self.source = draft.source;
        self.overrides = draft.overrides;
        self.metadata = draft.metadata;
        self.updated_at = now;
    }

    pub(crate) fn apply_update(&mut self, update: NotificationUpdate, now: DateTime<Utc>) -> bool {
        let mut changed = false;
        macro_rules! replace_if_changed {
            ($field:ident) => {
                if let Some(value) = update.$field {
                    if self.$field != value {
                        self.$field = value;
                        changed = true;
                    }
                }
            };
        }
        replace_if_changed!(title);
        replace_if_changed!(body);
        replace_if_changed!(level);
        replace_if_changed!(priority);
        replace_if_changed!(overrides);
        if self.presentation == Presentation::Toast {
            replace_if_changed!(timeout);
        }
        if changed {
            self.updated_at = now;
        }
        changed
    }

    pub(crate) fn mark_visible(&mut self, now: DateTime<Utc>) {
        if self.delivery == DeliveryState::Pending {
            self.delivery = DeliveryState::Visible;
            self.updated_at = now;
        }
    }

    pub(crate) fn close(&mut self, reason: CloseReason, now: DateTime<Utc>) {
        self.delivery = DeliveryState::Closed;
        self.close_reason = Some(reason);
        self.updated_at = now;
        if reason == CloseReason::Jumped {
            self.last_jumped_at = Some(now);
        }
    }
}

/// Measure normalized content in terminal cells rather than scalar values or bytes.
#[must_use]
pub fn display_width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

fn normalize_required(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<String, IngressError> {
    let normalized = normalize_bounded(field, value, maximum)?;
    if normalized.is_empty() {
        return Err(IngressError::EmptyValue { field });
    }
    Ok(normalized)
}

fn normalize_bounded(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<String, IngressError> {
    let normalized = normalize_plain_text(value);
    if normalized.len() > maximum {
        return Err(IngressError::ValueTooLarge {
            field,
            actual: normalized.len(),
            maximum,
        });
    }
    Ok(normalized)
}

fn stable_tmux_id(field: &'static str, value: &str, prefix: char) -> Result<String, IngressError> {
    if value.len() > MAX_SOURCE_ID_BYTES {
        return Err(IngressError::ValueTooLarge {
            field,
            actual: value.len(),
            maximum: MAX_SOURCE_ID_BYTES,
        });
    }
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(IngressError::InvalidTmuxId { field });
    };
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(IngressError::InvalidTmuxId { field });
    }
    Ok(value.to_owned())
}

/// Convert untrusted input into terminal-safe plain text.
#[must_use]
fn normalize_plain_text(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                result.push('\n');
            }
            '\n' => result.push('\n'),
            '\t' => result.push_str(&" ".repeat(TAB_WIDTH)),
            '\u{1b}' => {
                if !consume_ansi_after_escape(&mut chars) {
                    result.push('\u{fffd}');
                }
            }
            '\u{009b}' => consume_csi(&mut chars),
            '\u{009d}' => consume_osc(&mut chars),
            value if is_display_control(value) => result.push('\u{fffd}'),
            value => result.push(value),
        }
    }
    result
}

fn consume_ansi_after_escape<I>(chars: &mut std::iter::Peekable<I>) -> bool
where
    I: Iterator<Item = char>,
{
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            consume_csi(chars);
            true
        }
        Some(']') => {
            chars.next();
            consume_osc(chars);
            true
        }
        Some(value) if (' '..='/').contains(&value) => {
            chars.next();
            while matches!(chars.peek(), Some(value) if (' '..='/').contains(value)) {
                chars.next();
            }
            if matches!(chars.peek(), Some(value) if ('0'..='~').contains(value)) {
                chars.next();
            }
            true
        }
        _ => false,
    }
}

fn consume_csi<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    for value in chars.by_ref() {
        if ('@'..='~').contains(&value) {
            break;
        }
    }
}

fn consume_osc<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    while let Some(value) = chars.next() {
        if value == '\u{0007}' {
            break;
        }
        if value == '\u{001b}' && chars.peek() == Some(&'\\') {
            chars.next();
            break;
        }
    }
}

fn is_display_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{00ad}'
                | '\u{200b}'
                | '\u{200e}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_line_endings_and_tabs() {
        assert_eq!(normalize_plain_text("a\r\nb\rc\td"), "a\nb\nc    d");
    }

    #[test]
    fn removes_ansi_and_replaces_controls() {
        assert_eq!(
            normalize_plain_text("\u{1b}[31mred\u{1b}[0m\0\u{007f}\u{0085}"),
            "red\u{fffd}\u{fffd}\u{fffd}"
        );
        assert_eq!(
            normalize_plain_text("lone\u{1b}escape"),
            "lone\u{fffd}escape"
        );
        assert_eq!(normalize_plain_text("\u{009b}32mgreen\u{009b}0m"), "green");
    }

    #[test]
    fn removes_osc_sequences_without_leaking_payload() {
        assert_eq!(
            normalize_plain_text("a\u{1b}]0;hostile title\u{0007}b"),
            "ab"
        );
        assert_eq!(
            normalize_plain_text("a\u{009d}8;;https://example.invalid\u{1b}\\b"),
            "ab"
        );
    }

    #[test]
    fn replaces_bidi_and_invisible_display_controls() {
        assert_eq!(
            normalize_plain_text("a\u{202e}b\u{2066}c\u{200b}d"),
            "a\u{fffd}b\u{fffd}c\u{fffd}d"
        );
    }

    #[test]
    fn measures_cjk_combining_marks_and_emoji_in_cells() {
        assert_eq!(display_width("通知"), 4);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(display_width("🙂"), 2);
        assert_eq!(display_width("👩\u{200d}💻"), 2);
        assert_eq!(
            normalize_plain_text("e\u{301} 👩\u{200d}💻"),
            "e\u{301} 👩\u{200d}💻"
        );
    }

    #[test]
    fn rejects_oversized_ipc_and_normalized_values() {
        let request = vec![b'x'; MAX_IPC_REQUEST_BYTES + 1];
        assert!(matches!(
            validate_ipc_request_size(&request),
            Err(IngressError::RequestTooLarge { .. })
        ));
        assert!(validate_ipc_request_size(&request[..MAX_IPC_REQUEST_BYTES]).is_ok());

        let body = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(matches!(
            NotificationDraft::new(Presentation::Toast, "", &body, None),
            Err(IngressError::ValueTooLarge {
                field: "Notification body",
                ..
            })
        ));
    }

    #[test]
    fn notification_only_exposes_normalized_text() {
        let draft =
            NotificationDraft::new(Presentation::Toast, "build\u{1b}[31m", "done\r\nnow", None)
                .unwrap();
        let notification = Notification::from_draft(draft, Utc::now());
        assert_eq!(notification.title(), "build");
        assert_eq!(notification.body(), "done\nnow");
        assert_eq!(notification.delivery(), DeliveryState::Pending);
        assert_eq!(notification.created_at(), notification.updated_at());
    }

    #[test]
    fn attention_requires_a_valid_source_pane() {
        assert_eq!(
            NotificationDraft::new(Presentation::Attention, "", "input", None),
            Err(IngressError::AttentionRequiresSource)
        );
        assert!(SourceContext::new(TmuxServerId::new("server").unwrap(), "$1", "@2", "%3").is_ok());
        assert!(
            SourceContext::new(TmuxServerId::new("server").unwrap(), "name", "@2", "%3").is_err()
        );
    }

    #[test]
    fn source_and_metadata_are_normalized_and_allowlisted() {
        let source = SourceContext::new(TmuxServerId::new("server").unwrap(), "$1", "@2", "%3")
            .unwrap()
            .with_provider_session(Provider::Codex, "session\u{202e}id")
            .unwrap()
            .with_command("cargo\u{1b}[31m")
            .unwrap();
        assert_eq!(source.provider_session_id(), Some("session\u{fffd}id"));
        assert_eq!(source.command(), Some("cargo"));

        let metadata = NormalizedMetadata::new(Some(AgentEventKind::ToolCompleted))
            .with_tool_name("shell\0tool")
            .unwrap();
        assert_eq!(metadata.tool_name(), Some("shell\u{fffd}tool"));
    }

    #[test]
    fn notification_ids_are_uuid_v7() {
        let id = NotificationId::new();
        assert_eq!(id.as_uuid().get_version_num(), 7);
        assert_eq!(id.to_string().parse::<NotificationId>().unwrap(), id);
        assert!(Uuid::nil().to_string().parse::<NotificationId>().is_err());
    }
}
