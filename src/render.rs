//! Authenticated, bounded renderer sessions owned by one daemon.
//!
//! A renderer is launched with only a Window Display ID and a one-time random
//! token. Notification content enters this module only as private-socket wire
//! messages after redemption; it is never converted into argv, environment, a
//! temporary file, or a diagnostic value.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::protocol::{RendererContent, RendererMessage, RendererTermination};

pub const DEFAULT_MAX_RENDERER_SESSIONS: usize = 256;
pub const DEFAULT_RENDERER_CHANNEL_CAPACITY: usize = 16;
pub const DEFAULT_RENDERER_TOKEN_TTL: Duration = Duration::from_secs(15);
const TOKEN_BYTES: usize = 32;
const TOKEN_GENERATION_ATTEMPTS: usize = 4;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WindowDisplayId(String);

impl WindowDisplayId {
    pub fn new(value: impl Into<String>) -> Result<Self, SessionError> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 {
            return Err(SessionError::InvalidWindowDisplayId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An argv-safe launch capability. Debug output intentionally redacts it.
#[derive(Clone, Eq, PartialEq)]
pub struct RendererToken(String);

impl RendererToken {
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RendererToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RendererToken([REDACTED])")
    }
}

/// Values required by the hidden renderer command. The daemon-only generation
/// is retained separately so old child exits cannot poison a recreated display.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RendererLaunch {
    window_display_id: WindowDisplayId,
    token: RendererToken,
    generation: u64,
}

impl RendererLaunch {
    #[must_use]
    pub fn window_display_id(&self) -> &WindowDisplayId {
        &self.window_display_id
    }

    #[must_use]
    pub fn token(&self) -> &RendererToken {
        &self.token
    }

    /// Used by the daemon's child watcher, never passed to the renderer.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLimits {
    pub max_sessions: usize,
    pub channel_capacity: usize,
    pub token_ttl: Duration,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_sessions: DEFAULT_MAX_RENDERER_SESSIONS,
            channel_capacity: DEFAULT_RENDERER_CHANNEL_CAPACITY,
            token_ttl: DEFAULT_RENDERER_TOKEN_TTL,
        }
    }
}

pub trait TokenGenerator {
    fn generate(&mut self) -> Result<[u8; TOKEN_BYTES], SessionError>;
}

#[derive(Debug, Default)]
pub struct OsTokenGenerator;

impl TokenGenerator for OsTokenGenerator {
    fn generate(&mut self) -> Result<[u8; TOKEN_BYTES], SessionError> {
        let mut bytes = [0_u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| SessionError::RandomnessUnavailable)?;
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RendererExit {
    Successful,
    Failed,
    Signalled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RendererOutcome {
    StaleExit,
    Stopped {
        window_display_id: WindowDisplayId,
    },
    Crashed {
        window_display_id: WindowDisplayId,
        exit: RendererExit,
    },
}

/// Authenticated stream identity retained by the daemon socket task. The
/// generation is not sent to the renderer; it prevents an old connection from
/// acting after the same Window Display has been recreated.
pub struct RendererStream {
    generation: u64,
    receiver: Receiver<RendererMessage>,
}

impl RendererStream {
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn recv(&self) -> Result<RendererMessage, mpsc::RecvError> {
        self.receiver.recv()
    }

    #[must_use]
    pub fn into_receiver(self) -> Receiver<RendererMessage> {
        self.receiver
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SessionError {
    #[error("invalid Window Display ID")]
    InvalidWindowDisplayId,
    #[error("renderer session limits must be positive")]
    InvalidLimits,
    #[error("renderer session capacity is exhausted")]
    CapacityExhausted,
    #[error("cryptographic randomness is unavailable")]
    RandomnessUnavailable,
    #[error("could not generate a unique renderer credential")]
    CredentialCollision,
    #[error("renderer credential is unknown")]
    UnknownCredential,
    #[error("renderer credential belongs to another Window Display")]
    MismatchedCredential,
    #[error("renderer credential has expired or was invalidated")]
    StaleCredential,
    #[error("renderer credential has already been consumed")]
    DuplicateRedemption,
    #[error("Window Display is unknown")]
    UnknownWindowDisplay,
    #[error("renderer update channel is full")]
    ChannelFull,
    #[error("renderer connection is closed")]
    RendererDisconnected,
}

#[derive(Clone)]
struct PendingCredential {
    window_display_id: WindowDisplayId,
    generation: u64,
    expires_at: Instant,
}

#[derive(Clone, Copy)]
enum RetiredReason {
    Consumed,
    Stale,
}

struct DisplaySession {
    generation: u64,
    pending_token: Option<String>,
    sender: Option<SyncSender<RendererMessage>>,
    content: RendererContent,
}

pub struct RendererSessions<G = OsTokenGenerator> {
    generator: G,
    limits: SessionLimits,
    displays: HashMap<WindowDisplayId, DisplaySession>,
    credentials: HashMap<String, PendingCredential>,
    retired: HashMap<String, (WindowDisplayId, RetiredReason)>,
    retired_order: VecDeque<String>,
    next_generation: u64,
}

impl RendererSessions<OsTokenGenerator> {
    pub fn new(limits: SessionLimits) -> Result<Self, SessionError> {
        Self::with_generator(limits, OsTokenGenerator)
    }
}

impl<G: TokenGenerator> RendererSessions<G> {
    pub fn with_generator(limits: SessionLimits, generator: G) -> Result<Self, SessionError> {
        if limits.max_sessions == 0 || limits.channel_capacity == 0 || limits.token_ttl.is_zero() {
            return Err(SessionError::InvalidLimits);
        }
        Ok(Self {
            generator,
            limits,
            displays: HashMap::with_capacity(limits.max_sessions),
            credentials: HashMap::with_capacity(limits.max_sessions),
            retired: HashMap::with_capacity(limits.max_sessions.saturating_mul(2)),
            retired_order: VecDeque::with_capacity(limits.max_sessions.saturating_mul(2)),
            next_generation: 1,
        })
    }

    /// Creates or recreates a Window Display. Recreation terminates the old
    /// stream and makes its unredeemed credential stale before returning.
    pub fn create(
        &mut self,
        window_display_id: WindowDisplayId,
        content: RendererContent,
        now: Instant,
    ) -> Result<RendererLaunch, SessionError> {
        self.expire_credentials(now);
        if !self.displays.contains_key(&window_display_id)
            && self.displays.len() >= self.limits.max_sessions
        {
            return Err(SessionError::CapacityExhausted);
        }

        if let Some(previous) = self.displays.remove(&window_display_id) {
            self.retire_pending(&window_display_id, &previous, RetiredReason::Stale);
            if let Some(sender) = previous.sender {
                let _ = sender.try_send(RendererMessage::Terminate {
                    reason: RendererTermination::Recreated,
                });
            }
        }

        let token = self.unique_token()?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.credentials.insert(
            token.clone(),
            PendingCredential {
                window_display_id: window_display_id.clone(),
                generation,
                expires_at: now + self.limits.token_ttl,
            },
        );
        self.displays.insert(
            window_display_id.clone(),
            DisplaySession {
                generation,
                pending_token: Some(token.clone()),
                sender: None,
                content,
            },
        );
        Ok(RendererLaunch {
            window_display_id,
            token: RendererToken(token),
            generation,
        })
    }

    /// Consumes a launch token and returns the bounded stream backing the
    /// already-authenticated private daemon connection.
    pub fn redeem(
        &mut self,
        window_display_id: &WindowDisplayId,
        token: &str,
        now: Instant,
    ) -> Result<RendererStream, SessionError> {
        self.expire_credentials(now);
        let Some(credential) = self.credentials.get(token).cloned() else {
            return Err(self.retired_error(window_display_id, token));
        };
        if credential.window_display_id != *window_display_id {
            return Err(SessionError::MismatchedCredential);
        }
        let Some(display) = self.displays.get_mut(window_display_id) else {
            return Err(SessionError::StaleCredential);
        };
        if display.generation != credential.generation
            || display.pending_token.as_deref() != Some(token)
        {
            return Err(SessionError::StaleCredential);
        }

        let (sender, receiver) = mpsc::sync_channel(self.limits.channel_capacity);
        sender
            .try_send(RendererMessage::Initial {
                content: display.content.clone(),
            })
            .map_err(|_| SessionError::ChannelFull)?;
        display.pending_token = None;
        display.sender = Some(sender);
        self.credentials.remove(token);
        self.retire(
            token.to_owned(),
            window_display_id.clone(),
            RetiredReason::Consumed,
        );
        Ok(RendererStream {
            generation: credential.generation,
            receiver,
        })
    }

    pub fn update(
        &mut self,
        window_display_id: &WindowDisplayId,
        content: RendererContent,
    ) -> Result<(), SessionError> {
        let display = self
            .displays
            .get_mut(window_display_id)
            .ok_or(SessionError::UnknownWindowDisplay)?;
        display.content = content.clone();
        let Some(sender) = display.sender.as_ref() else {
            // A keyed update may race renderer startup. Redemption must return
            // the newest content rather than forcing a display recreation.
            return Ok(());
        };
        match sender.try_send(RendererMessage::Update { content }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(SessionError::ChannelFull),
            Err(TrySendError::Disconnected(_)) => {
                display.sender = None;
                Err(SessionError::RendererDisconnected)
            }
        }
    }

    pub fn terminate(
        &mut self,
        window_display_id: &WindowDisplayId,
        reason: RendererTermination,
    ) -> Result<(), SessionError> {
        let Some(display) = self.displays.remove(window_display_id) else {
            return Err(SessionError::UnknownWindowDisplay);
        };
        self.retire_pending(window_display_id, &display, RetiredReason::Stale);
        if let Some(sender) = display.sender {
            match sender.try_send(RendererMessage::Terminate { reason }) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => Ok(()),
                Err(TrySendError::Full(_)) => Err(SessionError::ChannelFull),
            }
        } else {
            Ok(())
        }
    }

    /// Hook for the daemon's child watcher. A stale child's exit cannot remove
    /// a newer recreation of the same Window Display.
    pub fn renderer_exited(
        &mut self,
        window_display_id: &WindowDisplayId,
        generation: u64,
        exit: RendererExit,
    ) -> RendererOutcome {
        let is_current = self
            .displays
            .get(window_display_id)
            .is_some_and(|display| display.generation == generation);
        if !is_current {
            return RendererOutcome::StaleExit;
        }
        if let Some(display) = self.displays.remove(window_display_id) {
            self.retire_pending(window_display_id, &display, RetiredReason::Stale);
        }
        match exit {
            RendererExit::Successful => RendererOutcome::Stopped {
                window_display_id: window_display_id.clone(),
            },
            RendererExit::Failed | RendererExit::Signalled => RendererOutcome::Crashed {
                window_display_id: window_display_id.clone(),
                exit,
            },
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.displays.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.displays.is_empty()
    }

    #[must_use]
    pub fn is_current_generation(
        &self,
        window_display_id: &WindowDisplayId,
        generation: u64,
    ) -> bool {
        self.displays
            .get(window_display_id)
            .is_some_and(|display| display.generation == generation)
    }

    fn unique_token(&mut self) -> Result<String, SessionError> {
        for _ in 0..TOKEN_GENERATION_ATTEMPTS {
            let bytes = self.generator.generate()?;
            let token = encode_hex(bytes);
            if !self.credentials.contains_key(&token) && !self.retired.contains_key(&token) {
                return Ok(token);
            }
        }
        Err(SessionError::CredentialCollision)
    }

    fn expire_credentials(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .credentials
            .iter()
            .filter(|(_, credential)| credential.expires_at <= now)
            .map(|(token, credential)| (token.clone(), credential.clone()))
            .collect();
        for (token, credential) in expired {
            self.credentials.remove(&token);
            if self
                .displays
                .get(&credential.window_display_id)
                .is_some_and(|display| display.generation == credential.generation)
            {
                self.displays.remove(&credential.window_display_id);
            }
            self.retire(token, credential.window_display_id, RetiredReason::Stale);
        }
    }

    fn retire_pending(
        &mut self,
        window_display_id: &WindowDisplayId,
        display: &DisplaySession,
        reason: RetiredReason,
    ) {
        if let Some(token) = &display.pending_token {
            self.credentials.remove(token);
            self.retire(token.clone(), window_display_id.clone(), reason);
        }
    }

    fn retire(&mut self, token: String, id: WindowDisplayId, reason: RetiredReason) {
        let capacity = self.limits.max_sessions.saturating_mul(2);
        self.retired.insert(token.clone(), (id, reason));
        self.retired_order.push_back(token);
        while self.retired_order.len() > capacity {
            if let Some(oldest) = self.retired_order.pop_front() {
                self.retired.remove(&oldest);
            }
        }
    }

    fn retired_error(&self, id: &WindowDisplayId, token: &str) -> SessionError {
        match self.retired.get(token) {
            Some((expected, _)) if expected != id => SessionError::MismatchedCredential,
            Some((_, RetiredReason::Consumed)) => SessionError::DuplicateRedemption,
            Some((_, RetiredReason::Stale)) => SessionError::StaleCredential,
            None => SessionError::UnknownCredential,
        }
    }
}

fn encode_hex(bytes: [u8; TOKEN_BYTES]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(TOKEN_BYTES * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::{NotificationDraft, Presentation};
    use chrono::Utc;

    struct SequenceGenerator(u8);

    impl TokenGenerator for SequenceGenerator {
        fn generate(&mut self) -> Result<[u8; TOKEN_BYTES], SessionError> {
            self.0 = self.0.wrapping_add(1);
            Ok([self.0; TOKEN_BYTES])
        }
    }

    fn manager(
        max_sessions: usize,
        channel_capacity: usize,
    ) -> RendererSessions<SequenceGenerator> {
        RendererSessions::with_generator(
            SessionLimits {
                max_sessions,
                channel_capacity,
                token_ttl: Duration::from_secs(5),
            },
            SequenceGenerator(0),
        )
        .unwrap()
    }

    fn id(value: &str) -> WindowDisplayId {
        WindowDisplayId::new(value).unwrap()
    }

    fn content(body: &str) -> RendererContent {
        let notification = NotificationDraft::new(Presentation::Toast, "build", body, None)
            .map(|draft| crate::notification::Notification::from_draft(draft, Utc::now()))
            .unwrap();
        RendererContent::from(&notification)
    }

    #[test]
    fn token_is_cryptographic_length_and_debug_is_redacted() {
        let now = Instant::now();
        let launch = manager(1, 2)
            .create(id("display-1"), content("initial"), now)
            .unwrap();
        assert_eq!(launch.token().expose_secret().len(), TOKEN_BYTES * 2);
        assert_eq!(format!("{:?}", launch.token()), "RendererToken([REDACTED])");
        assert!(!format!("{launch:?}").contains(launch.token().expose_secret()));
    }

    #[test]
    fn redemption_streams_initial_updates_and_termination() {
        let now = Instant::now();
        let display_id = id("display-1");
        let mut sessions = manager(1, 4);
        let launch = sessions
            .create(display_id.clone(), content("initial"), now)
            .unwrap();
        let stream = sessions
            .redeem(&display_id, launch.token().expose_secret(), now)
            .unwrap();
        sessions.update(&display_id, content("updated")).unwrap();
        sessions
            .terminate(&display_id, RendererTermination::Dismissed)
            .unwrap();

        assert!(matches!(
            stream.recv().unwrap(),
            RendererMessage::Initial { content } if content.body() == "initial"
        ));
        assert!(matches!(
            stream.recv().unwrap(),
            RendererMessage::Update { content } if content.body() == "updated"
        ));
        assert_eq!(
            stream.recv().unwrap(),
            RendererMessage::Terminate {
                reason: RendererTermination::Dismissed
            }
        );
    }

    #[test]
    fn update_before_redemption_becomes_the_initial_frame() {
        let now = Instant::now();
        let display_id = id("display-1");
        let mut sessions = manager(1, 2);
        let launch = sessions
            .create(display_id.clone(), content("old"), now)
            .unwrap();
        sessions.update(&display_id, content("newest")).unwrap();
        let stream = sessions
            .redeem(&display_id, launch.token().expose_secret(), now)
            .unwrap();
        assert!(matches!(
            stream.recv().unwrap(),
            RendererMessage::Initial { content } if content.body() == "newest"
        ));
    }

    #[test]
    fn rejects_mismatch_duplicate_expiry_and_recreation_staleness() {
        let now = Instant::now();
        let first_id = id("display-1");
        let other_id = id("display-2");
        let mut sessions = manager(2, 2);
        let first = sessions
            .create(first_id.clone(), content("secret"), now)
            .unwrap();
        assert!(matches!(
            sessions.redeem(&other_id, first.token().expose_secret(), now),
            Err(SessionError::MismatchedCredential)
        ));
        let _stream = sessions
            .redeem(&first_id, first.token().expose_secret(), now)
            .unwrap();
        assert!(matches!(
            sessions.redeem(&first_id, first.token().expose_secret(), now),
            Err(SessionError::DuplicateRedemption)
        ));

        let expiring = sessions
            .create(other_id.clone(), content("secret"), now)
            .unwrap();
        assert!(matches!(
            sessions.redeem(
                &other_id,
                expiring.token().expose_secret(),
                now + Duration::from_secs(5)
            ),
            Err(SessionError::StaleCredential)
        ));

        let old = sessions
            .create(first_id.clone(), content("old"), now)
            .unwrap();
        let replacement = sessions
            .create(first_id.clone(), content("replacement"), now)
            .unwrap();
        assert_ne!(old.token(), replacement.token());
        assert!(matches!(
            sessions.redeem(&first_id, old.token().expose_secret(), now),
            Err(SessionError::StaleCredential)
        ));
    }

    #[test]
    fn sessions_and_channels_are_bounded() {
        let now = Instant::now();
        let mut sessions = manager(1, 1);
        let first_id = id("display-1");
        let launch = sessions
            .create(first_id.clone(), content("initial"), now)
            .unwrap();
        assert_eq!(
            sessions.create(id("display-2"), content("other"), now),
            Err(SessionError::CapacityExhausted)
        );
        let _stream = sessions
            .redeem(&first_id, launch.token().expose_secret(), now)
            .unwrap();
        assert_eq!(
            sessions.update(&first_id, content("blocked")),
            Err(SessionError::ChannelFull)
        );
    }

    #[test]
    fn stale_child_exit_does_not_remove_recreated_display() {
        let now = Instant::now();
        let display_id = id("display-1");
        let mut sessions = manager(1, 2);
        let old = sessions
            .create(display_id.clone(), content("old"), now)
            .unwrap();
        let old_stream = sessions
            .redeem(&display_id, old.token().expose_secret(), now)
            .unwrap();
        assert!(sessions.is_current_generation(&display_id, old_stream.generation()));
        let current = sessions
            .create(display_id.clone(), content("current"), now)
            .unwrap();
        assert!(!sessions.is_current_generation(&display_id, old_stream.generation()));
        assert!(sessions.is_current_generation(&display_id, current.generation()));

        assert_eq!(
            sessions.renderer_exited(&display_id, old.generation(), RendererExit::Failed),
            RendererOutcome::StaleExit
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions.renderer_exited(&display_id, current.generation(), RendererExit::Signalled),
            RendererOutcome::Crashed {
                window_display_id: display_id,
                exit: RendererExit::Signalled
            }
        );
        assert!(sessions.is_empty());
    }
}
