//! Bounded, versioned messages exchanged over tmnotify's private Unix sockets.
//!
//! This module deliberately stops at the wire boundary. It validates framing
//! and envelope semantics, but leaves domain conversion to the daemon so wire
//! representations cannot leak into scheduler state.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::notification::{
    AgentEventKind, IngressError, Level, NormalizedMetadata, Notification, NotificationDraft,
    NotificationId, NotificationKey, NotificationUpdate, Placement, Presentation,
    PresentationOverrides, Priority, Provider, SourceContext, Timeout, TmuxServerId,
};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const REQUEST_CACHE_CAPACITY: usize = 10_000;
pub const REQUEST_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Send,
    Update,
    Dismiss,
    Jump,
    History,
    HistoryClear,
    RendererRedeem,
}

impl RequestKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "send" => Some(Self::Send),
            "update" => Some(Self::Update),
            "dismiss" => Some(Self::Dismiss),
            "jump" => Some(Self::Jump),
            "history" => Some(Self::History),
            "history-clear" => Some(Self::HistoryClear),
            "renderer-redeem" => Some(Self::RendererRedeem),
            _ => None,
        }
    }
}

/// A validated request plus the complete JSON value used for replay detection.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestEnvelope {
    pub request_id: Uuid,
    pub kind: RequestKind,
    pub value: Value,
}

impl RequestEnvelope {
    pub fn decode(frame: &[u8]) -> Result<Self, ProtocolError> {
        if frame.len() > MAX_REQUEST_BYTES {
            return Err(ProtocolError::FrameTooLarge {
                limit: MAX_REQUEST_BYTES,
            });
        }

        let value: Value = serde_json::from_slice(frame).map_err(ProtocolError::MalformedJson)?;
        let object = value
            .as_object()
            .ok_or(ProtocolError::RequestMustBeObject)?;

        let version = object
            .get("version")
            .and_then(Value::as_u64)
            .ok_or(ProtocolError::MissingOrInvalidField("version"))?;
        if version != u64::from(PROTOCOL_VERSION) {
            return Err(ProtocolError::UnsupportedVersion(version));
        }

        let request_id = object
            .get("request_id")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::MissingOrInvalidField("request_id"))?
            .parse()
            .map_err(|_| ProtocolError::MissingOrInvalidField("request_id"))?;

        let request_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::MissingOrInvalidField("type"))?;
        let kind = RequestKind::parse(request_type)
            .ok_or_else(|| ProtocolError::UnknownRequestType(request_type.to_owned()))?;

        Ok(Self {
            request_id,
            kind,
            value,
        })
    }

    pub fn payload_matches(&self, other: &Self) -> bool {
        self.request_id == other.request_id && self.value == other.value
    }

    /// Extracts the credential from a renderer's first private-socket frame.
    /// Its custom Debug implementation prevents accidental token disclosure.
    pub fn renderer_redemption(&self) -> Result<RendererRedemption, ProtocolError> {
        if self.kind != RequestKind::RendererRedeem {
            return Err(ProtocolError::UnexpectedRequestType);
        }
        let window_display_id = self
            .value
            .get("window_display")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 128)
            .ok_or(ProtocolError::MissingOrInvalidField("window_display"))?;
        let token = self
            .value
            .get("token")
            .and_then(Value::as_str)
            .filter(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .ok_or(ProtocolError::MissingOrInvalidField("token"))?;
        Ok(RendererRedemption {
            window_display_id: window_display_id.to_owned(),
            token: token.to_owned(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientRequest {
    pub version: u16,
    pub request_id: Uuid,
    #[serde(flatten)]
    pub command: ClientCommand,
}

impl ClientRequest {
    #[must_use]
    pub fn new(command: ClientCommand) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: Uuid::now_v7(),
            command,
        }
    }

    pub fn from_envelope(envelope: &RequestEnvelope) -> Result<Self, ProtocolError> {
        let request: Self = serde_json::from_value(envelope.value.clone())
            .map_err(ProtocolError::InvalidPayload)?;
        if request.version != PROTOCOL_VERSION
            || request.request_id != envelope.request_id
            || request.command.kind() != envelope.kind
        {
            return Err(ProtocolError::EnvelopeMismatch);
        }
        request.command.validate()?;
        Ok(request)
    }

    pub fn encode_line(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut bytes = serde_json::to_vec(self).map_err(ProtocolError::Encode)?;
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(ProtocolError::FrameTooLarge {
                limit: MAX_REQUEST_BYTES,
            });
        }
        bytes.push(b'\n');
        Ok(bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientCommand {
    Send {
        notification: Box<WireNotificationDraft>,
    },
    Update {
        selector: WireSelector,
        update: WireNotificationUpdate,
    },
    Dismiss {
        selector: WireSelector,
    },
    Jump {
        key: String,
    },
    History {
        include_hidden: bool,
        all_servers: bool,
        limit: u32,
    },
    HistoryClear {
        selector: WireHistoryClear,
        all_servers: bool,
    },
}

impl ClientCommand {
    fn kind(&self) -> RequestKind {
        match self {
            Self::Send { .. } => RequestKind::Send,
            Self::Update { .. } => RequestKind::Update,
            Self::Dismiss { .. } => RequestKind::Dismiss,
            Self::Jump { .. } => RequestKind::Jump,
            Self::History { .. } => RequestKind::History,
            Self::HistoryClear { .. } => RequestKind::HistoryClear,
        }
    }

    fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Send { notification } => notification.as_ref().clone().into_domain().map(|_| ()),
            Self::Update { selector, update } => {
                selector.clone().into_domain()?;
                let update = update.clone().into_domain()?;
                if update.is_empty() {
                    return Err(ProtocolError::EmptyUpdate);
                }
                Ok(())
            }
            Self::Dismiss { selector } => selector.clone().into_domain().map(|_| ()),
            Self::Jump { key } => NotificationKey::new(key)
                .map(|_| ())
                .map_err(ProtocolError::Ingress),
            Self::History { limit, .. } if *limit == 0 || *limit > 100_000 => {
                Err(ProtocolError::InvalidHistoryLimit)
            }
            Self::History { .. } | Self::HistoryClear { .. } => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WireNotificationDraft {
    pub key: Option<String>,
    pub level: Level,
    pub priority: Priority,
    pub presentation: Presentation,
    pub title: String,
    pub body: String,
    pub timeout_ms: Option<u64>,
    pub source: Option<WireSourceContext>,
    pub position: Option<Placement>,
    pub metadata: WireMetadata,
}

impl WireNotificationDraft {
    #[must_use]
    pub fn from_domain(draft: &NotificationDraft) -> Self {
        Self {
            key: draft.key().map(|key| key.as_str().to_owned()),
            level: draft.level(),
            priority: draft.priority(),
            presentation: draft.presentation(),
            title: draft.title().to_owned(),
            body: draft.body().to_owned(),
            timeout_ms: timeout_millis(draft.timeout()),
            source: draft.source().map(WireSourceContext::from_domain),
            position: draft.overrides().position(),
            metadata: WireMetadata::from_domain(draft.metadata()),
        }
    }

    pub fn into_domain(self) -> Result<NotificationDraft, ProtocolError> {
        let source = self
            .source
            .map(WireSourceContext::into_domain)
            .transpose()?;
        let mut draft = NotificationDraft::new(self.presentation, &self.title, &self.body, source)?
            .with_level(self.level)
            .with_priority(self.priority)
            .with_timeout(wire_timeout(self.timeout_ms));
        if let Some(key) = self.key {
            draft = draft.with_key(NotificationKey::new(&key)?);
        }
        if let Some(position) = self.position {
            draft = draft.with_overrides(PresentationOverrides::default().with_position(position));
        }
        draft = draft.with_metadata(self.metadata.into_domain()?);
        Ok(draft)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WireNotificationUpdate {
    pub title: Option<String>,
    pub body: Option<String>,
    pub level: Option<Level>,
    pub priority: Option<Priority>,
    /// `Some(None)` selects Never; `Some(Some(ms))` selects a finite timeout.
    pub timeout_ms: Option<Option<u64>>,
    pub position: Option<Placement>,
}

impl WireNotificationUpdate {
    #[must_use]
    pub fn from_domain(update: &NotificationUpdate) -> Self {
        Self {
            title: update.title().map(str::to_owned),
            body: update.body().map(str::to_owned),
            level: update.level(),
            priority: update.priority(),
            timeout_ms: update.timeout().map(timeout_millis),
            position: update.overrides().and_then(PresentationOverrides::position),
        }
    }

    pub fn into_domain(self) -> Result<NotificationUpdate, ProtocolError> {
        let mut update = NotificationUpdate::new();
        if let Some(title) = self.title {
            update = update.with_title(&title)?;
        }
        if let Some(body) = self.body {
            update = update.with_body(&body)?;
        }
        if let Some(level) = self.level {
            update = update.with_level(level);
        }
        if let Some(priority) = self.priority {
            update = update.with_priority(priority);
        }
        if let Some(timeout) = self.timeout_ms {
            update = update.with_timeout(wire_timeout(timeout));
        }
        if let Some(position) = self.position {
            update =
                update.with_overrides(PresentationOverrides::default().with_position(position));
        }
        Ok(update)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WireSourceContext {
    pub provider: Option<Provider>,
    pub provider_session_id: Option<String>,
    pub tmux_server_id: String,
    pub session_id: String,
    pub window_id: String,
    pub pane_id: String,
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub pane_title: Option<String>,
}

impl WireSourceContext {
    fn from_domain(source: &SourceContext) -> Self {
        Self {
            provider: source.provider(),
            provider_session_id: source.provider_session_id().map(str::to_owned),
            tmux_server_id: source.tmux_server_id().as_str().to_owned(),
            session_id: source.session_id().to_owned(),
            window_id: source.window_id().to_owned(),
            pane_id: source.pane_id().to_owned(),
            cwd: source.cwd().map(|path| path.to_string_lossy().into_owned()),
            command: source.command().map(str::to_owned),
            pane_title: source.pane_title().map(str::to_owned),
        }
    }

    fn into_domain(self) -> Result<SourceContext, ProtocolError> {
        let mut source = SourceContext::new(
            TmuxServerId::new(&self.tmux_server_id)?,
            &self.session_id,
            &self.window_id,
            &self.pane_id,
        )?;
        match (self.provider, self.provider_session_id) {
            (Some(provider), Some(session)) => {
                source = source.with_provider_session(provider, &session)?;
            }
            (None, None) => {}
            _ => return Err(ProtocolError::IncompleteProviderSource),
        }
        if let Some(cwd) = self.cwd {
            source = source.with_cwd(&cwd)?;
        }
        if let Some(command) = self.command {
            source = source.with_command(&command)?;
        }
        if let Some(title) = self.pane_title {
            source = source.with_pane_title(&title)?;
        }
        Ok(source)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WireMetadata {
    pub agent_event_kind: Option<AgentEventKind>,
    pub agent_name: Option<String>,
    pub tool_name: Option<String>,
}

impl WireMetadata {
    fn from_domain(metadata: &NormalizedMetadata) -> Self {
        Self {
            agent_event_kind: metadata.agent_event_kind(),
            agent_name: metadata.agent_name().map(str::to_owned),
            tool_name: metadata.tool_name().map(str::to_owned),
        }
    }

    fn into_domain(self) -> Result<NormalizedMetadata, ProtocolError> {
        let mut metadata = NormalizedMetadata::new(self.agent_event_kind);
        if let Some(name) = self.agent_name {
            metadata = metadata.with_agent_name(&name)?;
        }
        if let Some(name) = self.tool_name {
            metadata = metadata.with_tool_name(&name)?;
        }
        Ok(metadata)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WireSelector {
    pub id: Option<String>,
    pub key: Option<String>,
}

impl WireSelector {
    pub fn into_domain(self) -> Result<TargetSelector, ProtocolError> {
        match (self.id, self.key) {
            (Some(id), None) => Ok(TargetSelector::Id(id.parse()?)),
            (None, Some(key)) => Ok(TargetSelector::Key(NotificationKey::new(&key)?)),
            _ => Err(ProtocolError::InvalidSelector),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetSelector {
    Id(NotificationId),
    Key(NotificationKey),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireHistoryClear {
    Hidden,
    BeforeMillis(i64),
    All,
}

fn timeout_millis(timeout: Timeout) -> Option<u64> {
    match timeout {
        Timeout::After(duration) => Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
        Timeout::Never => None,
    }
}

fn wire_timeout(milliseconds: Option<u64>) -> Timeout {
    milliseconds.map_or(Timeout::Never, |value| {
        Timeout::After(Duration::from_millis(value))
    })
}

#[derive(Clone, Eq, PartialEq)]
pub struct RendererRedemption {
    window_display_id: String,
    token: String,
}

impl RendererRedemption {
    #[must_use]
    pub fn window_display_id(&self) -> &str {
        &self.window_display_id
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl std::fmt::Debug for RendererRedemption {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RendererRedemption")
            .field("window_display_id", &self.window_display_id)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Incrementally extracts newline-delimited frames while enforcing the limit
/// before allocation can grow beyond one complete request.
#[derive(Debug)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    max_frame_bytes: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new(MAX_REQUEST_BYTES)
    }
}

impl FrameDecoder {
    pub fn new(max_frame_bytes: usize) -> Self {
        assert!(max_frame_bytes > 0, "frame limit must be positive");
        Self {
            buffer: Vec::new(),
            max_frame_bytes,
        }
    }

    /// Adds bytes and returns every complete frame in arrival order.
    ///
    /// A frame excludes its newline. Callers should close the connection after
    /// `FrameTooLarge`; the decoder clears its partial buffer before returning.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, ProtocolError> {
        let mut frames = Vec::new();
        let mut start = 0;

        for (index, byte) in bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }

            if let Err(error) = self.append_bounded(&bytes[start..index]) {
                self.buffer.clear();
                return Err(error);
            }
            frames.push(std::mem::take(&mut self.buffer));
            start = index + 1;
        }

        if let Err(error) = self.append_bounded(&bytes[start..]) {
            self.buffer.clear();
            return Err(error);
        }

        Ok(frames)
    }

    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }

    fn append_bounded(&mut self, bytes: &[u8]) -> Result<(), ProtocolError> {
        if self.buffer.len().saturating_add(bytes.len()) > self.max_frame_bytes {
            return Err(ProtocolError::FrameTooLarge {
                limit: self.max_frame_bytes,
            });
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope<T> {
    pub version: u16,
    pub request_id: Uuid,
    #[serde(flatten)]
    pub result: T,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClientResponse {
    Success(Value),
    Error(String),
}

pub fn decode_client_response(
    frame: &[u8],
    expected_request_id: Uuid,
) -> Result<ClientResponse, ProtocolError> {
    if frame.len() > MAX_REQUEST_BYTES {
        return Err(ProtocolError::ResponseTooLarge {
            limit: MAX_REQUEST_BYTES,
        });
    }
    let value: Value = serde_json::from_slice(frame).map_err(ProtocolError::MalformedJson)?;
    let object = value
        .as_object()
        .ok_or(ProtocolError::ResponseMustBeObject)?;
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .ok_or(ProtocolError::MissingOrInvalidField("version"))?;
    if version != u64::from(PROTOCOL_VERSION) {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    let request_id = object
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::MissingOrInvalidField("request_id"))?
        .parse::<Uuid>()
        .map_err(|_| ProtocolError::MissingOrInvalidField("request_id"))?;
    if request_id != expected_request_id {
        return Err(ProtocolError::ResponseRequestMismatch {
            expected: expected_request_id,
            actual: request_id,
        });
    }
    match (object.get("result"), object.get("error")) {
        (Some(result), None) => Ok(ClientResponse::Success(result.clone())),
        (None, Some(error)) => error
            .as_str()
            .map(|error| ClientResponse::Error(error.to_owned()))
            .ok_or(ProtocolError::MissingOrInvalidField("error")),
        _ => Err(ProtocolError::InvalidResponseShape),
    }
}

impl<T> ResponseEnvelope<T> {
    pub fn new(request_id: Uuid, result: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            result,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    Queued,
    Visible,
    Updated,
    Duplicate,
    Suppressed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acknowledgement {
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_id: Option<Uuid>,
    pub history_persisted: bool,
    pub disposition: Disposition,
}

/// The content sent only after a renderer has authenticated on the private
/// daemon connection. This type deliberately contains no renderer credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendererContent {
    notification_id: Uuid,
    presentation: Presentation,
    level: Level,
    #[serde(skip_serializing_if = "Option::is_none")]
    notification_key: Option<String>,
    title: String,
    body: String,
    timeout: Timeout,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<RendererSource>,
    metadata: RendererMetadata,
}

impl RendererContent {
    #[must_use]
    pub fn notification_id(&self) -> Uuid {
        self.notification_id
    }

    #[must_use]
    pub fn presentation(&self) -> Presentation {
        self.presentation
    }

    #[must_use]
    pub fn level(&self) -> Level {
        self.level
    }

    /// A live direct-jump handle. Renderers may advertise the command, but
    /// must never turn their display surface into an input target.
    #[must_use]
    pub fn notification_key(&self) -> Option<&str> {
        self.notification_key.as_deref()
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
    pub fn source(&self) -> Option<&RendererSource> {
        self.source.as_ref()
    }

    #[must_use]
    pub fn metadata(&self) -> &RendererMetadata {
        &self.metadata
    }
}

impl From<&Notification> for RendererContent {
    fn from(notification: &Notification) -> Self {
        let source = notification.source().map(|source| RendererSource {
            provider: source.provider(),
            provider_session_id: source.provider_session_id().map(str::to_owned),
            session_id: source.session_id().to_owned(),
            window_id: source.window_id().to_owned(),
            pane_id: source.pane_id().to_owned(),
            cwd: source.cwd().map(|path| path.to_string_lossy().into_owned()),
            command: source.command().map(str::to_owned),
            pane_title: source.pane_title().map(str::to_owned),
        });
        let metadata = notification.metadata();
        Self {
            notification_id: notification.id().as_uuid(),
            presentation: notification.presentation(),
            level: notification.level(),
            notification_key: notification.key().map(|key| key.as_str().to_owned()),
            title: notification.title().to_owned(),
            body: notification.body().to_owned(),
            timeout: notification.timeout(),
            source,
            metadata: RendererMetadata {
                agent_event_kind: metadata.agent_event_kind(),
                agent_name: metadata.agent_name().map(str::to_owned),
                tool_name: metadata.tool_name().map(str::to_owned),
            },
        }
    }
}

/// Source fields useful to an Attention renderer. Complete process arguments
/// and provider transcript paths have no representation on this wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendererSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<Provider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_session_id: Option<String>,
    session_id: String,
    window_id: String,
    pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_title: Option<String>,
}

impl RendererSource {
    #[must_use]
    pub fn provider(&self) -> Option<Provider> {
        self.provider
    }

    #[must_use]
    pub fn provider_session_id(&self) -> Option<&str> {
        self.provider_session_id.as_deref()
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
    pub fn cwd(&self) -> Option<&str> {
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

/// Only normalized, explicitly allowlisted metadata crosses this boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendererMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_event_kind: Option<crate::notification::AgentEventKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}

impl RendererMetadata {
    #[must_use]
    pub fn agent_event_kind(&self) -> Option<crate::notification::AgentEventKind> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RendererTermination {
    Recreated,
    Dismissed,
    Jumped,
    TimedOut,
    RenderFailed,
    DaemonStopped,
    ServerEnded,
}

/// Frames streamed after a successful one-time credential redemption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RendererMessage {
    Initial { content: RendererContent },
    Update { content: RendererContent },
    Terminate { reason: RendererTermination },
}

#[derive(Serialize, Deserialize)]
struct RendererWireFrame {
    version: u16,
    #[serde(flatten)]
    message: RendererMessage,
}

impl RendererMessage {
    pub fn encode_line(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut encoded = serde_json::to_vec(&RendererWireFrame {
            version: PROTOCOL_VERSION,
            message: self.clone(),
        })
        .map_err(ProtocolError::MalformedJson)?;
        if encoded.len() > MAX_REQUEST_BYTES {
            return Err(ProtocolError::FrameTooLarge {
                limit: MAX_REQUEST_BYTES,
            });
        }
        encoded.push(b'\n');
        Ok(encoded)
    }

    pub fn decode(frame: &[u8]) -> Result<Self, ProtocolError> {
        let frame = frame.strip_suffix(b"\n").unwrap_or(frame);
        if frame.len() > MAX_REQUEST_BYTES {
            return Err(ProtocolError::FrameTooLarge {
                limit: MAX_REQUEST_BYTES,
            });
        }
        let decoded: RendererWireFrame =
            serde_json::from_slice(frame).map_err(ProtocolError::MalformedJson)?;
        if decoded.version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(u64::from(
                decoded.version,
            )));
        }
        Ok(decoded.message)
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("request exceeds the {limit}-byte limit")]
    FrameTooLarge { limit: usize },
    #[error("response exceeds the {limit}-byte limit")]
    ResponseTooLarge { limit: usize },
    #[error("malformed JSON: {0}")]
    MalformedJson(serde_json::Error),
    #[error("request must be a JSON object")]
    RequestMustBeObject,
    #[error("response must be a JSON object")]
    ResponseMustBeObject,
    #[error("missing or invalid field: {0}")]
    MissingOrInvalidField(&'static str),
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(u64),
    #[error("unknown request type: {0}")]
    UnknownRequestType(String),
    #[error("request payload is invalid: {0}")]
    InvalidPayload(serde_json::Error),
    #[error("failed to encode request: {0}")]
    Encode(serde_json::Error),
    #[error("validated envelope and typed request do not match")]
    EnvelopeMismatch,
    #[error("response request ID mismatch: expected {expected}, received {actual}")]
    ResponseRequestMismatch { expected: Uuid, actual: Uuid },
    #[error("response must contain exactly one of result or error")]
    InvalidResponseShape,
    #[error("selector must contain exactly one of id or key")]
    InvalidSelector,
    #[error("update contains no fields")]
    EmptyUpdate,
    #[error("History limit must be between 1 and 100000")]
    InvalidHistoryLimit,
    #[error("provider and provider_session_id must either both be present or both be absent")]
    IncompleteProviderSource,
    #[error("request failed safe ingress validation: {0}")]
    Ingress(#[from] IngressError),
    #[error("request type is not renderer-redeem")]
    UnexpectedRequestType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLookup<T> {
    Miss,
    Replay(T),
    PayloadMismatch,
}

#[derive(Debug)]
struct CacheEntry<T> {
    payload: Value,
    response: T,
    touched_at: Instant,
}

/// A small explicit LRU used to make client retries idempotent.
#[derive(Debug)]
pub struct RequestResultCache<T> {
    entries: HashMap<Uuid, CacheEntry<T>>,
    order: VecDeque<Uuid>,
    capacity: usize,
    ttl: Duration,
}

impl<T: Clone> Default for RequestResultCache<T> {
    fn default() -> Self {
        Self::new(REQUEST_CACHE_CAPACITY, REQUEST_CACHE_TTL)
    }
}

impl<T: Clone> RequestResultCache<T> {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        assert!(capacity > 0, "cache capacity must be positive");
        Self {
            entries: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
            ttl,
        }
    }

    pub fn lookup(&mut self, request: &RequestEnvelope, now: Instant) -> CacheLookup<T> {
        self.expire(now);
        let Some(entry) = self.entries.get_mut(&request.request_id) else {
            return CacheLookup::Miss;
        };

        if entry.payload != request.value {
            return CacheLookup::PayloadMismatch;
        }

        entry.touched_at = now;
        let response = entry.response.clone();
        self.touch(request.request_id);
        CacheLookup::Replay(response)
    }

    pub fn insert(&mut self, request: &RequestEnvelope, response: T, now: Instant) {
        self.expire(now);
        self.entries.insert(
            request.request_id,
            CacheEntry {
                payload: request.value.clone(),
                response,
                touched_at: now,
            },
        );
        self.touch(request.request_id);

        while self.entries.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn touch(&mut self, request_id: Uuid) {
        if let Some(index) = self.order.iter().position(|id| *id == request_id) {
            self.order.remove(index);
        }
        self.order.push_back(request_id);
    }

    fn expire(&mut self, now: Instant) {
        while let Some(request_id) = self.order.front().copied() {
            let Some(entry) = self.entries.get(&request_id) else {
                self.order.pop_front();
                continue;
            };
            if now.saturating_duration_since(entry.touched_at) < self.ttl {
                break;
            }
            self.order.pop_front();
            self.entries.remove(&request_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: Uuid, body: &str) -> RequestEnvelope {
        RequestEnvelope::decode(
            format!(r#"{{"version":1,"request_id":"{id}","type":"send","body":"{body}"}}"#)
                .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn decoder_handles_partial_and_multiplexed_frames() {
        let mut decoder = FrameDecoder::new(16);
        assert!(decoder.push(b"one").unwrap().is_empty());
        assert_eq!(decoder.pending_bytes(), 3);
        assert_eq!(
            decoder.push(b"\ntwo\nthree").unwrap(),
            vec![b"one".to_vec(), b"two".to_vec()]
        );
        assert_eq!(decoder.pending_bytes(), 5);
        assert_eq!(decoder.push(b"\n").unwrap(), vec![b"three".to_vec()]);
    }

    #[test]
    fn decoder_rejects_a_partial_frame_as_soon_as_it_is_too_large() {
        let mut decoder = FrameDecoder::new(4);
        assert!(decoder.push(b"1234").unwrap().is_empty());
        assert!(matches!(
            decoder.push(b"5"),
            Err(ProtocolError::FrameTooLarge { limit: 4 })
        ));
        assert_eq!(decoder.pending_bytes(), 0);
    }

    #[test]
    fn envelope_ignores_unknown_fields_but_keeps_them_for_replay_matching() {
        let id = Uuid::now_v7();
        let decoded = RequestEnvelope::decode(
            format!(r#"{{"version":1,"request_id":"{id}","type":"jump","future":true}}"#)
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(decoded.kind, RequestKind::Jump);
        assert_eq!(decoded.value["future"], true);
    }

    #[test]
    fn envelope_rejects_bad_envelope_semantics() {
        assert!(matches!(
            RequestEnvelope::decode(br#"{"version":2,"request_id":"bad","type":"send"}"#),
            Err(ProtocolError::UnsupportedVersion(2))
        ));
        assert!(matches!(
            RequestEnvelope::decode(br#"{"version":1,"request_id":"bad","type":"send"}"#),
            Err(ProtocolError::MissingOrInvalidField("request_id"))
        ));
        let id = Uuid::now_v7();
        assert!(matches!(
            RequestEnvelope::decode(
                format!(r#"{{"version":1,"request_id":"{id}","type":"future"}}"#).as_bytes()
            ),
            Err(ProtocolError::UnknownRequestType(kind)) if kind == "future"
        ));
    }

    #[test]
    fn cache_replays_only_an_identical_payload() {
        let id = Uuid::now_v7();
        let now = Instant::now();
        let mut cache = RequestResultCache::new(2, Duration::from_secs(60));
        let original = request(id, "one");
        cache.insert(&original, "accepted", now);

        assert_eq!(
            cache.lookup(&original, now),
            CacheLookup::Replay("accepted")
        );
        assert_eq!(
            cache.lookup(&request(id, "two"), now),
            CacheLookup::PayloadMismatch
        );
    }

    #[test]
    fn cache_is_lru_and_expires_entries() {
        let now = Instant::now();
        let first = request(Uuid::now_v7(), "first");
        let second = request(Uuid::now_v7(), "second");
        let third = request(Uuid::now_v7(), "third");
        let mut cache = RequestResultCache::new(2, Duration::from_secs(5));

        cache.insert(&first, 1, now);
        cache.insert(&second, 2, now);
        assert_eq!(cache.lookup(&first, now), CacheLookup::Replay(1));
        cache.insert(&third, 3, now);
        assert_eq!(cache.lookup(&second, now), CacheLookup::Miss);
        assert_eq!(cache.lookup(&first, now), CacheLookup::Replay(1));
        assert_eq!(
            cache.lookup(&first, now + Duration::from_secs(5)),
            CacheLookup::Miss
        );
    }

    #[test]
    fn renderer_messages_round_trip_as_bounded_newline_frames() {
        let message = RendererMessage::Initial {
            content: RendererContent {
                notification_id: Uuid::now_v7(),
                presentation: Presentation::Toast,
                level: Level::Success,
                notification_key: Some("build".into()),
                title: "Codex".into(),
                body: "finished".into(),
                timeout: Timeout::Never,
                source: None,
                metadata: RendererMetadata {
                    agent_event_kind: None,
                    agent_name: None,
                    tool_name: None,
                },
            },
        };
        let encoded = message.encode_line().unwrap();
        assert_eq!(encoded.last(), Some(&b'\n'));
        assert_eq!(
            serde_json::from_slice::<Value>(&encoded[..encoded.len() - 1]).unwrap()["version"],
            PROTOCOL_VERSION
        );
        assert_eq!(RendererMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn renderer_redemption_validates_fields_and_redacts_the_token() {
        let request_id = Uuid::now_v7();
        let token = "ab".repeat(32);
        let envelope = RequestEnvelope::decode(
            format!(
                r#"{{"version":1,"request_id":"{request_id}","type":"renderer-redeem","window_display":"display-1","token":"{token}"}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        let redemption = envelope.renderer_redemption().unwrap();
        assert_eq!(redemption.window_display_id(), "display-1");
        assert_eq!(redemption.token(), token);
        assert!(!format!("{redemption:?}").contains(&token));

        let malformed = RequestEnvelope::decode(
            format!(
                r#"{{"version":1,"request_id":"{request_id}","type":"renderer-redeem","window_display":"display-1","token":"not-secret"}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            malformed.renderer_redemption(),
            Err(ProtocolError::MissingOrInvalidField("token"))
        ));
    }

    #[test]
    fn typed_send_round_trips_through_the_validated_domain_boundary() {
        let source = SourceContext::new(TmuxServerId::new("server").unwrap(), "$1", "@2", "%3")
            .unwrap()
            .with_provider_session(Provider::Codex, "thread-1")
            .unwrap()
            .with_cwd("/work")
            .unwrap();
        let draft =
            NotificationDraft::new(Presentation::Toast, "Codex", "done\u{1b}[31m", Some(source))
                .unwrap()
                .with_key(NotificationKey::new("codex:thread-1:completed").unwrap())
                .with_level(Level::Success)
                .with_priority(Priority::High)
                .with_timeout(Timeout::Never)
                .with_overrides(
                    PresentationOverrides::default().with_position(Placement::BottomRight),
                )
                .with_metadata(
                    NormalizedMetadata::new(Some(AgentEventKind::Completed))
                        .with_agent_name("worker")
                        .unwrap(),
                );
        let request = ClientRequest::new(ClientCommand::Send {
            notification: Box::new(WireNotificationDraft::from_domain(&draft)),
        });
        let encoded = request.encode_line().unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("\u{1b}"));
        let envelope = RequestEnvelope::decode(&encoded[..encoded.len() - 1]).unwrap();
        let decoded = ClientRequest::from_envelope(&envelope).unwrap();
        let ClientCommand::Send { notification } = decoded.command else {
            panic!("expected send");
        };
        assert_eq!((*notification).into_domain().unwrap(), draft);
    }

    #[test]
    fn typed_update_and_selectors_reject_empty_or_ambiguous_requests() {
        let id = NotificationId::new();
        let update = NotificationUpdate::new()
            .with_body("done")
            .unwrap()
            .with_timeout(Timeout::After(Duration::from_secs(2)));
        let request = ClientRequest::new(ClientCommand::Update {
            selector: WireSelector {
                id: Some(id.to_string()),
                key: None,
            },
            update: WireNotificationUpdate::from_domain(&update),
        });
        let line = request.encode_line().unwrap();
        let envelope = RequestEnvelope::decode(&line[..line.len() - 1]).unwrap();
        assert!(ClientRequest::from_envelope(&envelope).is_ok());

        let empty = ClientRequest::new(ClientCommand::Update {
            selector: WireSelector {
                id: None,
                key: Some("build".into()),
            },
            update: WireNotificationUpdate::default(),
        });
        let line = empty.encode_line().unwrap();
        let envelope = RequestEnvelope::decode(&line[..line.len() - 1]).unwrap();
        assert!(matches!(
            ClientRequest::from_envelope(&envelope),
            Err(ProtocolError::EmptyUpdate)
        ));

        assert!(matches!(
            WireSelector {
                id: Some(id.to_string()),
                key: Some("build".into()),
            }
            .into_domain(),
            Err(ProtocolError::InvalidSelector)
        ));
    }

    #[test]
    fn typed_attention_revalidates_source_and_history_limits() {
        let attention = WireNotificationDraft {
            key: Some("attention".into()),
            level: Level::Warning,
            priority: Priority::High,
            presentation: Presentation::Attention,
            title: "input".into(),
            body: "needed".into(),
            timeout_ms: None,
            source: None,
            position: None,
            metadata: WireMetadata::default(),
        };
        assert!(matches!(
            attention.into_domain(),
            Err(ProtocolError::Ingress(
                IngressError::AttentionRequiresSource
            ))
        ));

        let history = ClientRequest::new(ClientCommand::History {
            include_hidden: false,
            all_servers: false,
            limit: 0,
        });
        let line = history.encode_line().unwrap();
        let envelope = RequestEnvelope::decode(&line[..line.len() - 1]).unwrap();
        assert!(matches!(
            ClientRequest::from_envelope(&envelope),
            Err(ProtocolError::InvalidHistoryLimit)
        ));
    }

    #[test]
    fn client_response_requires_matching_version_id_and_exclusive_shape() {
        let id = Uuid::now_v7();
        let success = serde_json::to_vec(&serde_json::json!({
            "version": PROTOCOL_VERSION,
            "request_id": id,
            "result": { "accepted": true }
        }))
        .unwrap();
        assert_eq!(
            decode_client_response(&success, id).unwrap(),
            ClientResponse::Success(serde_json::json!({ "accepted": true }))
        );
        let failure = serde_json::to_vec(&serde_json::json!({
            "version": PROTOCOL_VERSION,
            "request_id": id,
            "accepted": false,
            "error": "missing Source Pane"
        }))
        .unwrap();
        assert_eq!(
            decode_client_response(&failure, id).unwrap(),
            ClientResponse::Error("missing Source Pane".into())
        );

        assert!(matches!(
            decode_client_response(&success, Uuid::now_v7()),
            Err(ProtocolError::ResponseRequestMismatch { .. })
        ));
        let ambiguous = serde_json::to_vec(&serde_json::json!({
            "version": PROTOCOL_VERSION,
            "request_id": id,
            "result": {},
            "error": "bad"
        }))
        .unwrap();
        assert!(matches!(
            decode_client_response(&ambiguous, id),
            Err(ProtocolError::InvalidResponseShape)
        ));
    }
}
