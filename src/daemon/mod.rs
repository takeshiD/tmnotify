//! Daemon-owned live Notification scheduling.
//!
//! This module owns lifecycle, ordering, timeout, and direct-jump state. It is
//! intentionally independent of terminal and tmux protocol I/O: callers apply
//! [`DisplayPlan`] through the tmux boundary and commit a [`JumpIntent`] only
//! after that boundary reports a successful pane switch.

pub mod application;
mod reconcile;
pub mod runtime;
mod service;

pub use reconcile::{
    ReconcileError, ReconcileOutcome, ReconcileStatus, ToastAnimationPolicy, WindowDisplayPolicy,
    WindowReconciler,
};
pub use service::{DaemonService, JumpExecutor, ServiceError, ServiceResponse};

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::notification::{
    CloseReason, DeliveryState, Notification, NotificationDraft, NotificationId, NotificationKey,
    NotificationUpdate, Presentation, SourceContext, Timeout,
};

/// Monotonic time elapsed since a daemon-selected epoch.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct MonotonicTime(Duration);

impl MonotonicTime {
    #[must_use]
    pub const fn from_duration(value: Duration) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    fn elapsed_since(self, earlier: Self) -> Result<Duration, SchedulerError> {
        self.0
            .checked_sub(earlier.0)
            .ok_or(SchedulerError::TimeWentBackwards)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerLimits {
    pub max_pending: usize,
    pub max_visible_toasts: usize,
}

impl SchedulerLimits {
    pub fn new(max_pending: usize, max_visible_toasts: usize) -> Result<Self, SchedulerError> {
        if max_pending == 0 || max_visible_toasts == 0 {
            return Err(SchedulerError::InvalidLimits);
        }
        Ok(Self {
            max_pending,
            max_visible_toasts,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitDisposition {
    Queued,
    Visible,
    Updated,
    Duplicate,
    Suppressed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubmitOutcome {
    pub id: NotificationId,
    pub disposition: SubmitDisposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateDisposition {
    Updated,
    Duplicate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownReason {
    Interrupted,
    Stopped,
    ServerEnded,
}

impl ShutdownReason {
    fn close_reason(self) -> CloseReason {
        match self {
            Self::Interrupted => CloseReason::DaemonInterrupted,
            Self::Stopped => CloseReason::DaemonStopped,
            Self::ServerEnded => CloseReason::ServerEnded,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayPlan {
    /// Changes whenever desired visible content or ordering changes.
    pub revision: u64,
    pub attention: Option<NotificationId>,
    pub toasts: Vec<NotificationId>,
}

/// Opaque proof that a particular live Notification was approved for a jump.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JumpIntent {
    notification_id: NotificationId,
    nonce: u64,
    source: SourceContext,
}

impl JumpIntent {
    #[must_use]
    pub fn notification_id(&self) -> NotificationId {
        self.notification_id
    }

    #[must_use]
    pub fn source(&self) -> &SourceContext {
        &self.source
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerError {
    InvalidLimits,
    TimeWentBackwards,
    NotificationNotLive,
    KeyNotLive,
    MissingSource,
    JumpAlreadyPending,
    StaleJumpIntent,
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLimits => "scheduler limits must be greater than zero",
            Self::TimeWentBackwards => "monotonic scheduler time went backwards",
            Self::NotificationNotLive => "Notification is not live",
            Self::KeyNotLive => "Notification Key does not identify a live Notification",
            Self::MissingSource => "Notification has no Source Pane",
            Self::JumpAlreadyPending => "a jump is already pending for this Notification",
            Self::StaleJumpIntent => "jump intent is stale or does not belong to this scheduler",
        })
    }
}

impl std::error::Error for SchedulerError {}

#[derive(Clone, Debug)]
struct LiveEntry {
    notification: Notification,
    sequence: u64,
    timeout: TimeoutState,
    jump_nonce: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
enum TimeoutState {
    Never,
    Remaining {
        duration: Duration,
        running_since: Option<MonotonicTime>,
    },
}

impl TimeoutState {
    fn new(timeout: Timeout) -> Self {
        match timeout {
            Timeout::After(duration) => Self::Remaining {
                duration,
                running_since: None,
            },
            Timeout::Never => Self::Never,
        }
    }

    fn restart(&mut self, timeout: Timeout, now: MonotonicTime, running: bool) {
        *self = match timeout {
            Timeout::After(duration) => Self::Remaining {
                duration,
                running_since: running.then_some(now),
            },
            Timeout::Never => Self::Never,
        };
    }

    fn set_running(&mut self, now: MonotonicTime, running: bool) -> Result<(), SchedulerError> {
        let Self::Remaining {
            duration,
            running_since,
        } = self
        else {
            return Ok(());
        };
        match (*running_since, running) {
            (None, true) => *running_since = Some(now),
            (Some(start), false) => {
                *duration = duration.saturating_sub(now.elapsed_since(start)?);
                *running_since = None;
            }
            (Some(start), true) => {
                now.elapsed_since(start)?;
            }
            (None, false) => {}
        }
        Ok(())
    }

    fn expired(self, now: MonotonicTime) -> Result<bool, SchedulerError> {
        match self {
            Self::Never => Ok(false),
            Self::Remaining {
                duration,
                running_since: Some(start),
            } => Ok(now.elapsed_since(start)? >= duration),
            Self::Remaining {
                running_since: None,
                ..
            } => Ok(false),
        }
    }
}

/// Complete live scheduler for one tmux server daemon.
pub struct LiveScheduler {
    limits: SchedulerLimits,
    entries: HashMap<NotificationId, LiveEntry>,
    closed: VecDeque<Notification>,
    closed_capacity: usize,
    live_keys: HashMap<NotificationKey, NotificationId>,
    pending: Vec<NotificationId>,
    active_toasts: Vec<NotificationId>,
    active_attention: Option<NotificationId>,
    display_available: bool,
    next_sequence: u64,
    next_jump_nonce: u64,
    revision: u64,
    content_revision: u64,
    last_now: MonotonicTime,
}

impl LiveScheduler {
    pub fn new(limits: SchedulerLimits, now: MonotonicTime) -> Self {
        Self {
            limits,
            entries: HashMap::new(),
            closed: VecDeque::new(),
            // At most one overload close is produced per submit and at most
            // the visible set plus one Attention can close in one scheduler turn.
            closed_capacity: limits
                .max_pending
                .saturating_add(limits.max_visible_toasts)
                .saturating_add(1),
            live_keys: HashMap::new(),
            pending: Vec::new(),
            active_toasts: Vec::new(),
            active_attention: None,
            display_available: false,
            next_sequence: 0,
            next_jump_nonce: 0,
            revision: 0,
            content_revision: 0,
            last_now: now,
        }
    }

    pub fn submit(
        &mut self,
        draft: NotificationDraft,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<SubmitOutcome, SchedulerError> {
        self.prepare_time(monotonic_now, wall_now)?;
        if let Some(key) = draft.key()
            && let Some(id) = self.live_keys.get(key).copied()
        {
            let duplicate = self
                .entries
                .get(&id)
                .is_some_and(|entry| entry.notification.has_same_content(&draft));
            if duplicate {
                return Ok(SubmitOutcome {
                    id,
                    disposition: SubmitDisposition::Duplicate,
                });
            }
            let running = self.toast_timer_should_run(id);
            let prior_presentation = self.entries[&id].notification.presentation();
            let entry = self.entries.get_mut(&id).expect("live key must have entry");
            entry.notification.replace_content(draft, wall_now);
            entry
                .timeout
                .restart(entry.notification.timeout(), monotonic_now, running);
            if entry.notification.presentation() != prior_presentation {
                self.pending.retain(|candidate| *candidate != id);
                self.active_toasts.retain(|candidate| *candidate != id);
                if self.active_attention == Some(id) {
                    self.active_attention = None;
                }
                self.pending.push(id);
            }
            self.sort_pending();
            self.rebalance(monotonic_now, wall_now)?;
            self.bump_content_revision();
            return Ok(SubmitOutcome {
                id,
                disposition: SubmitDisposition::Updated,
            });
        }

        let notification = Notification::from_draft(draft, wall_now);
        let id = notification.id();
        let timeout = TimeoutState::new(notification.timeout());
        if let Some(key) = notification.key().cloned() {
            self.live_keys.insert(key, id);
        }
        self.entries.insert(
            id,
            LiveEntry {
                notification,
                sequence: self.next_sequence,
                timeout,
                jump_nonce: None,
            },
        );
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.pending.push(id);
        self.sort_pending();
        self.rebalance(monotonic_now, wall_now)?;
        let suppressed = self.enforce_pending_limit(wall_now);
        self.bump_content_revision();

        let disposition = if suppressed.contains(&id) {
            SubmitDisposition::Suppressed
        } else if self.is_desired_visible(id) {
            SubmitDisposition::Visible
        } else {
            SubmitDisposition::Queued
        };
        Ok(SubmitOutcome { id, disposition })
    }

    pub fn update_by_key(
        &mut self,
        key: &NotificationKey,
        update: NotificationUpdate,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<UpdateDisposition, SchedulerError> {
        self.prepare_time(monotonic_now, wall_now)?;
        let id = self
            .live_keys
            .get(key)
            .copied()
            .ok_or(SchedulerError::KeyNotLive)?;
        self.apply_update(id, update, monotonic_now, wall_now)
    }

    pub fn update(
        &mut self,
        id: NotificationId,
        update: NotificationUpdate,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<UpdateDisposition, SchedulerError> {
        self.prepare_time(monotonic_now, wall_now)?;
        self.apply_update(id, update, monotonic_now, wall_now)
    }

    fn apply_update(
        &mut self,
        id: NotificationId,
        update: NotificationUpdate,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<UpdateDisposition, SchedulerError> {
        let running = self.toast_timer_should_run(id);
        let entry = self
            .entries
            .get_mut(&id)
            .ok_or(SchedulerError::NotificationNotLive)?;
        if !entry.notification.apply_update(update, wall_now) {
            return Ok(UpdateDisposition::Duplicate);
        }
        entry
            .timeout
            .restart(entry.notification.timeout(), monotonic_now, running);
        self.sort_pending();
        self.bump_content_revision();
        Ok(UpdateDisposition::Updated)
    }

    pub fn dismiss_by_key(
        &mut self,
        key: &NotificationKey,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<NotificationId, SchedulerError> {
        let id = self
            .live_keys
            .get(key)
            .copied()
            .ok_or(SchedulerError::KeyNotLive)?;
        self.close(id, CloseReason::Dismissed, monotonic_now, wall_now)?;
        Ok(id)
    }

    pub fn dismiss(
        &mut self,
        id: NotificationId,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        self.close(id, CloseReason::Dismissed, monotonic_now, wall_now)
    }

    /// Close a live Notification after the tmux boundary exhausts its bounded
    /// retry budget for every eligible window.
    pub fn render_failed(
        &mut self,
        id: NotificationId,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        self.close(id, CloseReason::RenderFailed, monotonic_now, wall_now)
    }

    /// Close every live Notification for a daemon lifecycle boundary.
    pub fn shutdown(
        &mut self,
        reason: ShutdownReason,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        self.prepare_time(monotonic_now, wall_now)?;
        let ids: Vec<_> = self.entries.keys().copied().collect();
        for id in ids {
            self.close_inner(id, reason.close_reason(), wall_now);
        }
        Ok(())
    }

    pub fn set_display_available(
        &mut self,
        available: bool,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        self.prepare_time(monotonic_now, wall_now)?;
        if self.display_available != available {
            self.display_available = available;
            self.bump_revision();
        }
        self.rebalance(monotonic_now, wall_now)
    }

    /// Applies bounded live queue/capacity settings without rebuilding live
    /// Notification state. Toasts beyond a reduced visible capacity return to
    /// the pending queue with their timeout paused.
    pub fn update_limits(
        &mut self,
        limits: SchedulerLimits,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        self.prepare_time(monotonic_now, wall_now)?;
        if self.limits == limits {
            return Ok(());
        }
        self.limits = limits;
        self.closed_capacity = limits
            .max_pending
            .saturating_add(limits.max_visible_toasts)
            .saturating_add(1);
        if self.active_toasts.len() > limits.max_visible_toasts {
            let deferred = self.active_toasts.split_off(limits.max_visible_toasts);
            for id in &deferred {
                self.entries
                    .get_mut(id)
                    .expect("active Toast must exist")
                    .timeout
                    .set_running(monotonic_now, false)?;
            }
            self.pending.extend(deferred);
        }
        self.sort_pending();
        self.rebalance(monotonic_now, wall_now)?;
        self.enforce_pending_limit(wall_now);
        while self.closed.len() > self.closed_capacity {
            self.closed.pop_front();
        }
        self.bump_content_revision();
        Ok(())
    }

    pub fn advance(
        &mut self,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        self.prepare_time(monotonic_now, wall_now)
    }

    pub fn begin_jump_by_key(
        &mut self,
        key: &NotificationKey,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<JumpIntent, SchedulerError> {
        self.prepare_time(monotonic_now, wall_now)?;
        let id = self
            .live_keys
            .get(key)
            .copied()
            .ok_or(SchedulerError::KeyNotLive)?;
        self.begin_jump_inner(id)
    }

    pub fn begin_jump(
        &mut self,
        id: NotificationId,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<JumpIntent, SchedulerError> {
        self.prepare_time(monotonic_now, wall_now)?;
        self.begin_jump_inner(id)
    }

    fn begin_jump_inner(&mut self, id: NotificationId) -> Result<JumpIntent, SchedulerError> {
        let timer_was_running = self.toast_timer_should_run(id);
        let entry = self
            .entries
            .get_mut(&id)
            .ok_or(SchedulerError::NotificationNotLive)?;
        if entry.notification.delivery() == DeliveryState::Closed {
            return Err(SchedulerError::NotificationNotLive);
        }
        if entry.jump_nonce.is_some() {
            return Err(SchedulerError::JumpAlreadyPending);
        }
        let source = entry
            .notification
            .source()
            .cloned()
            .ok_or(SchedulerError::MissingSource)?;
        let nonce = self.next_jump_nonce;
        self.next_jump_nonce = self.next_jump_nonce.wrapping_add(1);
        entry.jump_nonce = Some(nonce);
        if timer_was_running {
            entry.timeout.set_running(self.last_now, false)?;
        }
        Ok(JumpIntent {
            notification_id: id,
            nonce,
            source,
        })
    }

    pub fn cancel_jump(
        &mut self,
        intent: JumpIntent,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        self.prepare_time(monotonic_now, wall_now)?;
        let entry = self
            .entries
            .get_mut(&intent.notification_id)
            .ok_or(SchedulerError::StaleJumpIntent)?;
        if entry.jump_nonce != Some(intent.nonce) {
            return Err(SchedulerError::StaleJumpIntent);
        }
        entry.jump_nonce = None;
        if self.toast_timer_should_run(intent.notification_id) {
            self.entries
                .get_mut(&intent.notification_id)
                .expect("validated jump entry must exist")
                .timeout
                .set_running(self.last_now, true)?;
        }
        Ok(())
    }

    pub fn commit_jump(
        &mut self,
        intent: JumpIntent,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        let valid = self
            .entries
            .get(&intent.notification_id)
            .is_some_and(|entry| entry.jump_nonce == Some(intent.nonce));
        if !valid {
            return Err(SchedulerError::StaleJumpIntent);
        }
        self.close(
            intent.notification_id,
            CloseReason::Jumped,
            monotonic_now,
            wall_now,
        )
    }

    #[must_use]
    pub fn display_plan(&self) -> DisplayPlan {
        DisplayPlan {
            revision: self.revision,
            attention: self
                .display_available
                .then_some(self.active_attention)
                .flatten(),
            toasts: if self.display_available && self.active_attention.is_none() {
                self.active_toasts.clone()
            } else {
                Vec::new()
            },
        }
    }

    #[must_use]
    pub fn notification(&self, id: NotificationId) -> Option<&Notification> {
        self.entries.get(&id).map(|entry| &entry.notification)
    }

    pub(crate) fn id_for_key(&self, key: &NotificationKey) -> Option<NotificationId> {
        self.live_keys.get(key).copied()
    }

    /// Closed lifecycle records waiting for the daemon to forward them to History.
    ///
    /// The queue is bounded. The daemon should drain it after every scheduler
    /// operation, before accepting more work.
    #[must_use]
    pub fn closed_notification(&self, id: NotificationId) -> Option<&Notification> {
        self.closed
            .iter()
            .find(|notification| notification.id() == id)
    }

    pub fn drain_closed(&mut self) -> Vec<Notification> {
        self.closed.drain(..).collect()
    }

    #[must_use]
    pub fn live_len(&self) -> usize {
        self.entries.len()
    }

    /// Changes only when Notification content or lifecycle changes, not when
    /// display availability pauses and resumes the same live work.
    #[must_use]
    pub(crate) fn content_revision(&self) -> u64 {
        self.content_revision
    }

    fn prepare_time(
        &mut self,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        monotonic_now.elapsed_since(self.last_now)?;
        self.last_now = monotonic_now;
        let expired: Vec<_> = self
            .active_toasts
            .iter()
            .copied()
            .filter_map(|id| {
                self.entries
                    .get(&id)
                    .and_then(|entry| entry.timeout.expired(monotonic_now).ok())
                    .filter(|expired| *expired)
                    .map(|_| id)
            })
            .collect();
        for id in expired {
            self.close_inner(id, CloseReason::TimedOut, wall_now);
        }
        if !self.active_toasts.is_empty()
            || self.active_attention.is_some()
            || !self.pending.is_empty()
        {
            self.rebalance(monotonic_now, wall_now)?;
        }
        Ok(())
    }

    fn close(
        &mut self,
        id: NotificationId,
        reason: CloseReason,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        self.prepare_time(monotonic_now, wall_now)?;
        if self
            .entries
            .get(&id)
            .is_none_or(|entry| entry.notification.delivery() == DeliveryState::Closed)
        {
            return Err(SchedulerError::NotificationNotLive);
        }
        self.close_inner(id, reason, wall_now);
        self.rebalance(monotonic_now, wall_now)?;
        Ok(())
    }

    fn close_inner(&mut self, id: NotificationId, reason: CloseReason, wall_now: DateTime<Utc>) {
        let Some(mut entry) = self.entries.remove(&id) else {
            return;
        };
        entry.notification.close(reason, wall_now);
        if let Some(key) = entry.notification.key() {
            self.live_keys.remove(key);
        }
        self.pending.retain(|candidate| *candidate != id);
        self.active_toasts.retain(|candidate| *candidate != id);
        if self.active_attention == Some(id) {
            self.active_attention = None;
        }
        if self.closed.len() == self.closed_capacity {
            self.closed.pop_front();
        }
        self.closed.push_back(entry.notification);
        self.bump_content_revision();
    }

    fn rebalance(
        &mut self,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        if self.display_available
            && self.active_attention.is_none()
            && let Some(index) = self.pending.iter().position(|id| {
                self.entries[id].notification.presentation() == Presentation::Attention
            })
        {
            self.active_attention = Some(self.pending.remove(index));
        }

        if self.display_available && self.active_attention.is_none() {
            while self.active_toasts.len() < self.limits.max_visible_toasts {
                let Some(index) = self.pending.iter().position(|id| {
                    self.entries[id].notification.presentation() == Presentation::Toast
                }) else {
                    break;
                };
                self.active_toasts.push(self.pending.remove(index));
            }
        }

        if self.display_available {
            if let Some(id) = self.active_attention {
                self.entries
                    .get_mut(&id)
                    .expect("active Attention must exist")
                    .notification
                    .mark_visible(wall_now);
            } else {
                for id in &self.active_toasts {
                    self.entries
                        .get_mut(id)
                        .expect("active Toast must exist")
                        .notification
                        .mark_visible(wall_now);
                }
            }
        }

        let running_ids: HashSet<_> = if self.display_available && self.active_attention.is_none() {
            self.active_toasts
                .iter()
                .copied()
                .filter(|id| self.entries[id].jump_nonce.is_none())
                .collect()
        } else {
            HashSet::new()
        };
        for id in &self.active_toasts {
            self.entries
                .get_mut(id)
                .expect("active Toast must exist")
                .timeout
                .set_running(monotonic_now, running_ids.contains(id))?;
        }
        Ok(())
    }

    fn enforce_pending_limit(&mut self, wall_now: DateTime<Utc>) -> HashSet<NotificationId> {
        let mut suppressed = HashSet::new();
        while self.pending.len() > self.limits.max_pending {
            let index = self
                .pending
                .iter()
                .rposition(|id| self.entries[id].jump_nonce.is_none())
                .expect("a newly submitted pending Notification is not jump-pinned");
            let id = self.pending.remove(index);
            suppressed.insert(id);
            self.close_inner(id, CloseReason::RenderSuppressed, wall_now);
        }
        suppressed
    }

    fn sort_pending(&mut self) {
        let entries = &self.entries;
        self.pending.sort_by(|left, right| {
            let left = &entries[left];
            let right = &entries[right];
            right
                .notification
                .priority()
                .cmp(&left.notification.priority())
                .then_with(|| left.sequence.cmp(&right.sequence))
        });
    }

    fn toast_timer_should_run(&self, id: NotificationId) -> bool {
        self.display_available
            && self.active_attention.is_none()
            && self.active_toasts.contains(&id)
    }

    fn is_desired_visible(&self, id: NotificationId) -> bool {
        self.display_available
            && (self.active_attention == Some(id)
                || (self.active_attention.is_none() && self.active_toasts.contains(&id)))
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    fn bump_content_revision(&mut self) {
        self.content_revision = self.content_revision.wrapping_add(1);
        self.bump_revision();
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::notification::{Level, Priority, TmuxServerId};

    fn mono(seconds: u64) -> MonotonicTime {
        MonotonicTime::from_duration(Duration::from_secs(seconds))
    }

    fn wall(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).single().unwrap()
    }

    fn source() -> SourceContext {
        SourceContext::new(TmuxServerId::new("server").unwrap(), "$1", "@2", "%3").unwrap()
    }

    fn toast(title: &str, priority: Priority) -> NotificationDraft {
        NotificationDraft::new(Presentation::Toast, title, "body", Some(source()))
            .unwrap()
            .with_priority(priority)
    }

    fn attention(title: &str, priority: Priority) -> NotificationDraft {
        NotificationDraft::new(Presentation::Attention, title, "body", Some(source()))
            .unwrap()
            .with_priority(priority)
    }

    fn scheduler(max_pending: usize, max_visible_toasts: usize) -> LiveScheduler {
        LiveScheduler::new(
            SchedulerLimits::new(max_pending, max_visible_toasts).unwrap(),
            mono(0),
        )
    }

    #[test]
    fn live_limits_defer_excess_toasts_and_pause_their_timeout() {
        let mut scheduler = scheduler(10, 2);
        scheduler
            .set_display_available(true, mono(0), wall(0))
            .unwrap();
        let first = scheduler
            .submit(toast("first", Priority::Normal), mono(0), wall(0))
            .unwrap();
        let second = scheduler
            .submit(toast("second", Priority::Normal), mono(0), wall(0))
            .unwrap();
        scheduler
            .update_limits(SchedulerLimits::new(10, 1).unwrap(), mono(1), wall(1))
            .unwrap();
        assert_eq!(scheduler.display_plan().toasts, vec![first.id]);

        scheduler.advance(mono(2), wall(2)).unwrap();
        assert!(scheduler.notification(second.id).is_some());
        scheduler.dismiss(first.id, mono(2), wall(2)).unwrap();
        assert_eq!(scheduler.display_plan().toasts, vec![second.id]);
        scheduler.advance(mono(3), wall(3)).unwrap();
        assert!(scheduler.notification(second.id).is_some());
    }

    #[test]
    fn priority_then_fifo_orders_pending_without_preempting_visible_toast() {
        let mut scheduler = scheduler(10, 1);
        scheduler
            .set_display_available(true, mono(0), wall(0))
            .unwrap();
        let visible = scheduler
            .submit(toast("visible", Priority::Low), mono(0), wall(0))
            .unwrap();
        let normal = scheduler
            .submit(toast("normal", Priority::Normal), mono(0), wall(0))
            .unwrap();
        let high_first = scheduler
            .submit(toast("high first", Priority::High), mono(0), wall(0))
            .unwrap();
        let high_second = scheduler
            .submit(toast("high second", Priority::High), mono(0), wall(0))
            .unwrap();

        assert_eq!(scheduler.display_plan().toasts, vec![visible.id]);
        scheduler.dismiss(visible.id, mono(0), wall(0)).unwrap();
        assert_eq!(scheduler.display_plan().toasts, vec![high_first.id]);
        scheduler.dismiss(high_first.id, mono(0), wall(0)).unwrap();
        assert_eq!(scheduler.display_plan().toasts, vec![high_second.id]);
        scheduler.dismiss(high_second.id, mono(0), wall(0)).unwrap();
        assert_eq!(scheduler.display_plan().toasts, vec![normal.id]);
    }

    #[test]
    fn overload_retains_best_pending_and_closes_loser() {
        let mut scheduler = scheduler(2, 1);
        let low = scheduler
            .submit(toast("low", Priority::Low), mono(0), wall(0))
            .unwrap();
        let normal = scheduler
            .submit(toast("normal", Priority::Normal), mono(0), wall(0))
            .unwrap();
        let critical = scheduler
            .submit(toast("critical", Priority::Critical), mono(0), wall(0))
            .unwrap();

        assert_eq!(critical.disposition, SubmitDisposition::Queued);
        assert_eq!(
            scheduler
                .closed_notification(low.id)
                .unwrap()
                .close_reason(),
            Some(CloseReason::RenderSuppressed)
        );
        scheduler
            .set_display_available(true, mono(0), wall(0))
            .unwrap();
        assert_eq!(scheduler.display_plan().toasts, vec![critical.id]);
        scheduler.dismiss(critical.id, mono(0), wall(0)).unwrap();
        assert_eq!(scheduler.display_plan().toasts, vec![normal.id]);
    }

    #[test]
    fn keyed_upsert_reuses_id_does_not_restart_duplicate_and_restarts_change() {
        let mut scheduler = scheduler(10, 1);
        scheduler
            .set_display_available(true, mono(0), wall(0))
            .unwrap();
        let key = NotificationKey::new("build").unwrap();
        let draft = toast("building", Priority::Normal)
            .with_key(key.clone())
            .with_timeout(Timeout::After(Duration::from_secs(3)));
        let initial = scheduler.submit(draft.clone(), mono(0), wall(0)).unwrap();
        scheduler.advance(mono(2), wall(2)).unwrap();
        let duplicate = scheduler.submit(draft, mono(2), wall(2)).unwrap();
        assert_eq!(duplicate.id, initial.id);
        assert_eq!(duplicate.disposition, SubmitDisposition::Duplicate);
        scheduler.advance(mono(3), wall(3)).unwrap();
        assert_eq!(
            scheduler
                .closed_notification(initial.id)
                .unwrap()
                .close_reason(),
            Some(CloseReason::TimedOut)
        );

        let recreated = scheduler
            .submit(
                toast("building", Priority::Normal)
                    .with_key(key.clone())
                    .with_timeout(Timeout::After(Duration::from_secs(3))),
                mono(3),
                wall(3),
            )
            .unwrap();
        scheduler.advance(mono(5), wall(5)).unwrap();
        let changed = scheduler
            .submit(
                toast("done", Priority::Normal)
                    .with_key(key)
                    .with_level(Level::Success)
                    .with_timeout(Timeout::After(Duration::from_secs(3))),
                mono(5),
                wall(5),
            )
            .unwrap();
        assert_eq!(changed.id, recreated.id);
        assert_eq!(changed.disposition, SubmitDisposition::Updated);
        scheduler.advance(mono(7), wall(7)).unwrap();
        assert_eq!(
            scheduler.notification(recreated.id).unwrap().delivery(),
            DeliveryState::Visible
        );
        scheduler.advance(mono(8), wall(8)).unwrap();
        assert_eq!(
            scheduler
                .closed_notification(recreated.id)
                .unwrap()
                .close_reason(),
            Some(CloseReason::TimedOut)
        );
    }

    #[test]
    fn partial_update_requires_live_target_and_reorders_only_pending_items() {
        let mut scheduler = scheduler(10, 1);
        scheduler
            .set_display_available(true, mono(0), wall(0))
            .unwrap();
        let visible = scheduler
            .submit(toast("visible", Priority::Low), mono(0), wall(0))
            .unwrap();
        let first_key = NotificationKey::new("first").unwrap();
        let first = scheduler
            .submit(
                toast("first", Priority::Low).with_key(first_key.clone()),
                mono(0),
                wall(0),
            )
            .unwrap();
        let second = scheduler
            .submit(toast("second", Priority::Normal), mono(0), wall(0))
            .unwrap();

        let disposition = scheduler
            .update_by_key(
                &first_key,
                NotificationUpdate::new().with_priority(Priority::Critical),
                mono(1),
                wall(1),
            )
            .unwrap();
        assert_eq!(disposition, UpdateDisposition::Updated);
        assert_eq!(scheduler.display_plan().toasts, vec![visible.id]);
        scheduler.dismiss(visible.id, mono(1), wall(1)).unwrap();
        assert_eq!(scheduler.display_plan().toasts, vec![first.id]);
        scheduler.dismiss(first.id, mono(1), wall(1)).unwrap();
        assert_eq!(scheduler.display_plan().toasts, vec![second.id]);
        assert_eq!(
            scheduler.update_by_key(
                &first_key,
                NotificationUpdate::new().with_level(Level::Warning),
                mono(1),
                wall(1),
            ),
            Err(SchedulerError::KeyNotLive)
        );
    }

    #[test]
    fn no_display_and_attention_pause_toast_timeout() {
        let mut scheduler = scheduler(10, 1);
        let toast = scheduler
            .submit(
                toast("toast", Priority::Normal)
                    .with_timeout(Timeout::After(Duration::from_secs(3))),
                mono(0),
                wall(0),
            )
            .unwrap();
        scheduler.advance(mono(10), wall(10)).unwrap();
        assert_eq!(
            scheduler.notification(toast.id).unwrap().delivery(),
            DeliveryState::Pending
        );

        scheduler
            .set_display_available(true, mono(10), wall(10))
            .unwrap();
        scheduler.advance(mono(11), wall(11)).unwrap();
        let attention = scheduler
            .submit(attention("gate", Priority::Normal), mono(11), wall(11))
            .unwrap();
        assert_eq!(scheduler.display_plan().attention, Some(attention.id));
        assert!(scheduler.display_plan().toasts.is_empty());
        scheduler.advance(mono(30), wall(30)).unwrap();
        scheduler.dismiss(attention.id, mono(30), wall(30)).unwrap();
        scheduler.advance(mono(31), wall(31)).unwrap();
        assert_eq!(
            scheduler.notification(toast.id).unwrap().delivery(),
            DeliveryState::Visible
        );
        scheduler.advance(mono(32), wall(32)).unwrap();
        assert_eq!(
            scheduler
                .closed_notification(toast.id)
                .unwrap()
                .close_reason(),
            Some(CloseReason::TimedOut)
        );
    }

    #[test]
    fn attention_is_global_and_queued_by_priority_fifo() {
        let mut scheduler = scheduler(10, 2);
        scheduler
            .set_display_available(true, mono(0), wall(0))
            .unwrap();
        let first = scheduler
            .submit(attention("first", Priority::Low), mono(0), wall(0))
            .unwrap();
        let normal = scheduler
            .submit(attention("normal", Priority::Normal), mono(0), wall(0))
            .unwrap();
        let high = scheduler
            .submit(attention("high", Priority::High), mono(0), wall(0))
            .unwrap();
        assert_eq!(scheduler.display_plan().attention, Some(first.id));
        scheduler.dismiss(first.id, mono(0), wall(0)).unwrap();
        assert_eq!(scheduler.display_plan().attention, Some(high.id));
        scheduler.dismiss(high.id, mono(0), wall(0)).unwrap();
        assert_eq!(scheduler.display_plan().attention, Some(normal.id));
    }

    #[test]
    fn direct_jump_closes_only_after_successful_commit() {
        let mut scheduler = scheduler(10, 1);
        scheduler
            .set_display_available(true, mono(0), wall(0))
            .unwrap();
        let key = NotificationKey::new("build").unwrap();
        let sent = scheduler
            .submit(
                toast("done", Priority::Normal).with_key(key.clone()),
                mono(0),
                wall(0),
            )
            .unwrap();

        let failed_switch = scheduler.begin_jump_by_key(&key, mono(0), wall(0)).unwrap();
        assert_eq!(failed_switch.source().pane_id(), "%3");
        scheduler
            .cancel_jump(failed_switch, mono(0), wall(0))
            .unwrap();
        assert_eq!(
            scheduler.notification(sent.id).unwrap().delivery(),
            DeliveryState::Visible
        );
        assert_eq!(scheduler.display_plan().toasts, vec![sent.id]);

        let successful_switch = scheduler.begin_jump_by_key(&key, mono(0), wall(0)).unwrap();
        scheduler
            .commit_jump(successful_switch, mono(1), wall(1))
            .unwrap();
        assert_eq!(
            scheduler
                .closed_notification(sent.id)
                .unwrap()
                .close_reason(),
            Some(CloseReason::Jumped)
        );
        assert!(scheduler.display_plan().toasts.is_empty());
        assert_eq!(
            scheduler.begin_jump_by_key(&key, mono(1), wall(1)),
            Err(SchedulerError::KeyNotLive)
        );
    }

    #[test]
    fn jump_intent_pins_timeout_until_switch_result() {
        let mut scheduler = scheduler(10, 1);
        scheduler
            .set_display_available(true, mono(0), wall(0))
            .unwrap();
        let key = NotificationKey::new("build").unwrap();
        let sent = scheduler
            .submit(
                toast("done", Priority::Normal)
                    .with_key(key.clone())
                    .with_timeout(Timeout::After(Duration::from_secs(1))),
                mono(0),
                wall(0),
            )
            .unwrap();
        let intent = scheduler.begin_jump_by_key(&key, mono(0), wall(0)).unwrap();

        scheduler.advance(mono(10), wall(10)).unwrap();
        scheduler.commit_jump(intent, mono(10), wall(10)).unwrap();
        assert_eq!(
            scheduler
                .closed_notification(sent.id)
                .unwrap()
                .close_reason(),
            Some(CloseReason::Jumped)
        );
    }

    #[test]
    fn direct_jump_rejects_missing_source_and_stale_commit() {
        let mut scheduler = scheduler(10, 1);
        scheduler
            .set_display_available(true, mono(0), wall(0))
            .unwrap();
        let key = NotificationKey::new("no-source").unwrap();
        scheduler
            .submit(
                NotificationDraft::new(Presentation::Toast, "done", "body", None)
                    .unwrap()
                    .with_key(key.clone()),
                mono(0),
                wall(0),
            )
            .unwrap();
        assert_eq!(
            scheduler.begin_jump_by_key(&key, mono(0), wall(0)),
            Err(SchedulerError::MissingSource)
        );

        let jumpable = NotificationKey::new("jumpable").unwrap();
        let sent = scheduler
            .submit(
                toast("done", Priority::Normal).with_key(jumpable.clone()),
                mono(0),
                wall(0),
            )
            .unwrap();
        let intent = scheduler
            .begin_jump_by_key(&jumpable, mono(0), wall(0))
            .unwrap();
        scheduler.dismiss(sent.id, mono(0), wall(0)).unwrap();
        assert_eq!(
            scheduler.commit_jump(intent, mono(0), wall(0)),
            Err(SchedulerError::StaleJumpIntent)
        );
    }

    #[test]
    fn monotonic_time_must_not_move_backwards() {
        let mut scheduler = LiveScheduler::new(
            SchedulerLimits::new(10, 1).unwrap(),
            MonotonicTime::from_duration(Duration::from_secs(5)),
        );
        assert_eq!(
            scheduler.advance(mono(4), wall(4)),
            Err(SchedulerError::TimeWentBackwards)
        );
    }

    #[test]
    fn render_and_daemon_boundaries_record_explicit_close_reasons() {
        let mut scheduler = scheduler(10, 2);
        let failed = scheduler
            .submit(toast("failed", Priority::Normal), mono(0), wall(0))
            .unwrap();
        scheduler
            .render_failed(failed.id, mono(0), wall(0))
            .unwrap();
        assert_eq!(
            scheduler
                .closed_notification(failed.id)
                .unwrap()
                .close_reason(),
            Some(CloseReason::RenderFailed)
        );

        let first = scheduler
            .submit(toast("first", Priority::Normal), mono(0), wall(0))
            .unwrap();
        let second = scheduler
            .submit(attention("second", Priority::Normal), mono(0), wall(0))
            .unwrap();
        scheduler
            .shutdown(ShutdownReason::ServerEnded, mono(1), wall(1))
            .unwrap();
        assert_eq!(scheduler.live_len(), 0);
        for id in [first.id, second.id] {
            assert_eq!(
                scheduler.closed_notification(id).unwrap().close_reason(),
                Some(CloseReason::ServerEnded)
            );
        }
        assert_eq!(scheduler.drain_closed().len(), 3);
    }
}
