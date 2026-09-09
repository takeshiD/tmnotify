use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::{DisplayPlan as SchedulerDisplayPlan, LiveScheduler, MonotonicTime, SchedulerError};
use crate::config::{BodyPresentation, Easing, Placement, StackOrder};
use crate::notification::NotificationId;
use crate::protocol::{RendererBodyMode, RendererContent, RendererDisplayOptions};
use crate::tmux::{
    Backend, DisplayKind, DisplayPlan, Event, Geometry, PlannedDisplay, ReconcileReport, Topology,
    WindowId, WindowSize,
};
use crate::toast::{MotionPhase, MotionTrack, Rect, Viewport, offscreen_rect, plan_frames};

const MAX_EVENTS_PER_TICK: usize = 128;
const MAX_RENDER_ATTEMPTS: u8 = 3;
const INITIAL_RENDER_BACKOFF: Duration = Duration::from_millis(50);
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(100);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowDisplayPolicy {
    pub placement: Placement,
    pub toast_width: u16,
    pub toast_height: u16,
    pub toast_gap: u16,
    pub max_visible_toasts: usize,
    pub stack_order: StackOrder,
    pub body: BodyPresentation,
    pub unicode: bool,
    pub color: bool,
    pub animation: ToastAnimationPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToastAnimationPolicy {
    pub enabled: bool,
    pub fps: u32,
    pub enter_duration: Duration,
    pub exit_duration: Duration,
    pub enter_easing: Easing,
    pub exit_easing: Easing,
}

impl ToastAnimationPolicy {
    #[must_use]
    pub fn frame_interval(self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.fps.clamp(1, 120)))
    }
}

impl Default for ToastAnimationPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            fps: 20,
            enter_duration: Duration::from_millis(180),
            exit_duration: Duration::from_millis(150),
            enter_easing: Easing::EaseOut,
            exit_easing: Easing::EaseIn,
        }
    }
}

impl Default for WindowDisplayPolicy {
    fn default() -> Self {
        Self {
            placement: Placement::TopRight,
            toast_width: 42,
            toast_height: 3,
            toast_gap: 1,
            max_visible_toasts: 4,
            stack_order: StackOrder::OldestFirst,
            body: BodyPresentation::FirstLine,
            unicode: true,
            color: true,
            animation: ToastAnimationPolicy::default(),
        }
    }
}

#[derive(Clone, Debug)]
struct DisplayMetadata {
    notification_id: NotificationId,
    placement: Placement,
    viewport: Viewport,
}

#[derive(Clone, Debug)]
struct Motion {
    started_at: MonotonicTime,
    phase: MotionPhase,
    frames: VecDeque<(Duration, Rect)>,
    current: Rect,
    finished_on_previous_tick: bool,
}

#[derive(Clone, Debug)]
struct MotionSpec {
    display_id: String,
    from: Rect,
    to: Rect,
    phase: MotionPhase,
    duration: Duration,
    easing: Easing,
}

#[derive(Default)]
struct AnimationRuntime {
    motions: HashMap<String, Motion>,
    exiting: HashMap<String, (WindowId, PlannedDisplay)>,
    paused_at: Option<MonotonicTime>,
}

impl AnimationRuntime {
    fn clear(&mut self) {
        self.motions.clear();
        self.exiting.clear();
    }

    fn pause(&mut self, now: MonotonicTime) {
        self.paused_at.get_or_insert(now);
    }

    fn resume(&mut self, now: MonotonicTime) {
        let Some(paused_at) = self.paused_at.take() else {
            return;
        };
        let paused = now.as_duration().saturating_sub(paused_at.as_duration());
        for motion in self.motions.values_mut() {
            motion.started_at = MonotonicTime::from_duration(
                motion.started_at.as_duration().saturating_add(paused),
            );
        }
    }

    fn start(&mut self, specs: Vec<MotionSpec>, fps: u32, now: MonotonicTime) {
        if specs.is_empty() {
            return;
        }
        let tracks = specs
            .iter()
            .map(|spec| MotionTrack {
                window_id: spec.display_id.as_str(),
                from: spec.from,
                to: spec.to,
                phase: spec.phase,
                duration: spec.duration,
                easing: spec.easing,
            })
            .collect::<Vec<_>>();
        let batches = plan_frames(&tracks, fps, true);
        let mut planned = HashMap::<String, VecDeque<(Duration, Rect)>>::new();
        for batch in &batches {
            for update in &batch.updates {
                planned
                    .entry(update.window_id.to_owned())
                    .or_default()
                    .push_back((batch.at, update.rect));
            }
        }
        drop(batches);
        drop(tracks);
        for spec in specs {
            let frames = planned.remove(&spec.display_id).unwrap_or_default();
            self.motions.insert(
                spec.display_id,
                Motion {
                    started_at: now,
                    phase: spec.phase,
                    frames,
                    current: spec.from,
                    finished_on_previous_tick: false,
                },
            );
        }
    }

    fn advance(&mut self, now: MonotonicTime) -> bool {
        let mut changed = false;
        let mut remove = Vec::new();
        for (id, motion) in &mut self.motions {
            if motion.finished_on_previous_tick {
                remove.push(id.clone());
                changed = true;
                continue;
            }
            let elapsed = now
                .as_duration()
                .saturating_sub(motion.started_at.as_duration());
            while motion.frames.front().is_some_and(|(at, _)| *at <= elapsed) {
                let (_, rect) = motion.frames.pop_front().expect("front was present");
                if motion.current != rect {
                    motion.current = rect;
                    changed = true;
                }
            }
            if motion.frames.is_empty() {
                match motion.phase {
                    MotionPhase::Enter => remove.push(id.clone()),
                    MotionPhase::Exit => motion.finished_on_previous_tick = true,
                    MotionPhase::Stay => remove.push(id.clone()),
                }
            }
        }
        for id in remove {
            self.motions.remove(&id);
            self.exiting.remove(&id);
        }
        changed
    }

    fn apply(&self, plan: &mut DisplayPlan) {
        for (id, (window, display)) in &self.exiting {
            plan.windows
                .entry(window.clone())
                .or_default()
                .push(display.clone());
            debug_assert_eq!(id, &display.display_id);
        }
        for displays in plan.windows.values_mut() {
            for display in displays {
                if let Some(motion) = self.motions.get(&display.display_id) {
                    display.geometry = geometry_from_rect(motion.current, display.geometry.z_index);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileStatus {
    Applied,
    WaitingForRetry,
    Disconnected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconcileOutcome {
    pub status: ReconcileStatus,
    pub visible_windows: BTreeSet<WindowId>,
    pub render_failed: Vec<NotificationId>,
    pub next_wakeup: Option<MonotonicTime>,
}

#[derive(Debug)]
pub enum ReconcileError {
    Scheduler(SchedulerError),
}

impl fmt::Display for ReconcileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scheduler(error) => write!(formatter, "scheduler reconciliation failed: {error}"),
        }
    }
}

impl std::error::Error for ReconcileError {}

impl From<SchedulerError> for ReconcileError {
    fn from(error: SchedulerError) -> Self {
        Self::Scheduler(error)
    }
}

#[derive(Clone, Copy, Debug)]
struct RetryState {
    attempts: u8,
    retry_at: MonotonicTime,
}

/// Daemon-side owner of topology and desired Window Displays.
///
/// tmux remains responsible for actual pane identities, command sequencing and
/// desired/actual command diffs. This type only projects scheduler state into
/// distinct eligible windows and applies lifecycle policy to backend results.
pub struct WindowReconciler {
    policy: WindowDisplayPolicy,
    topology: Topology,
    connected: bool,
    dirty: bool,
    successful_windows: BTreeSet<WindowId>,
    retries: BTreeMap<WindowId, RetryState>,
    projected_notifications: HashSet<NotificationId>,
    content_revision: u64,
    next_reconnect_at: MonotonicTime,
    reconnect_backoff: Duration,
    target_plan: DisplayPlan,
    target_metadata: HashMap<String, DisplayMetadata>,
    animation: AnimationRuntime,
}

impl WindowReconciler {
    #[must_use]
    pub fn new(policy: WindowDisplayPolicy, now: MonotonicTime) -> Self {
        Self {
            policy,
            topology: Topology::default(),
            connected: false,
            dirty: true,
            successful_windows: BTreeSet::new(),
            retries: BTreeMap::new(),
            projected_notifications: HashSet::new(),
            content_revision: 0,
            next_reconnect_at: now,
            reconnect_backoff: INITIAL_RECONNECT_BACKOFF,
            target_plan: DisplayPlan::default(),
            target_metadata: HashMap::new(),
            animation: AnimationRuntime::default(),
        }
    }

    #[must_use]
    pub fn topology(&self) -> &Topology {
        &self.topology
    }

    /// Forces the permitted low-frequency topology snapshot on the next tick.
    pub fn request_full_reconciliation(&mut self) {
        self.dirty = true;
    }

    /// Atomically replaces display policy. Reflow and restyling are immediate;
    /// an in-flight motion is cancelled rather than replayed under new values.
    pub fn update_policy(&mut self, policy: WindowDisplayPolicy) {
        if self.policy != policy {
            self.policy = policy;
            self.animation.clear();
            self.dirty = true;
        }
    }

    #[must_use]
    pub fn frame_interval(&self) -> Duration {
        self.policy.animation.frame_interval()
    }

    pub fn tick<B: Backend>(
        &mut self,
        backend: &mut B,
        scheduler: &mut LiveScheduler,
        now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<ReconcileOutcome, ReconcileError> {
        if self.connected {
            for _ in 0..MAX_EVENTS_PER_TICK {
                match backend.next_event() {
                    Ok(Some(Event::TopologyChanged)) => {
                        self.dirty = true;
                        self.retries.clear();
                    }
                    Ok(Some(Event::Disconnected)) | Err(_) => {
                        self.disconnect(now);
                        break;
                    }
                    Ok(None) => break,
                }
            }
        }

        if !self.connected {
            if now < self.next_reconnect_at {
                scheduler.set_display_available(false, now, wall_now)?;
                return Ok(self.outcome(ReconcileStatus::Disconnected, Vec::new()));
            }
            match backend.topology() {
                Ok(topology) => {
                    self.topology = topology;
                    self.connected = true;
                    self.animation.resume(now);
                    self.dirty = true;
                    self.retries.clear();
                    self.reconnect_backoff = INITIAL_RECONNECT_BACKOFF;
                }
                Err(_) => {
                    self.schedule_reconnect(now);
                    scheduler.set_display_available(false, now, wall_now)?;
                    return Ok(self.outcome(ReconcileStatus::Disconnected, Vec::new()));
                }
            }
        } else if self.dirty {
            match backend.topology() {
                Ok(topology) => {
                    if topology != self.topology {
                        self.topology = topology;
                        self.retries.clear();
                    }
                }
                Err(_) => {
                    self.disconnect(now);
                    scheduler.set_display_available(false, now, wall_now)?;
                    return Ok(self.outcome(ReconcileStatus::Disconnected, Vec::new()));
                }
            }
        }

        self.projected_notifications
            .retain(|id| scheduler.notification(*id).is_some());
        let eligible = self.topology.eligible_windows();
        if eligible.is_empty() {
            self.animation.clear();
            scheduler.set_display_available(false, now, wall_now)?;
            let retry_due = self
                .retries
                .values()
                .any(|retry| retry.attempts < MAX_RENDER_ATTEMPTS && retry.retry_at <= now);
            if self.dirty || !self.successful_windows.is_empty() || retry_due {
                match backend.reconcile(&DisplayPlan::default()) {
                    Ok(report) if report.failed.is_empty() => self.retries.clear(),
                    Ok(report) => {
                        for window in report.failed.keys() {
                            self.record_failure(window.clone(), now);
                        }
                    }
                    Err(_) => {
                        self.disconnect(now);
                        return Ok(self.outcome(ReconcileStatus::Disconnected, Vec::new()));
                    }
                }
            }
            self.successful_windows.clear();
            self.dirty = false;
            let status = if self.retries.is_empty() {
                ReconcileStatus::Applied
            } else {
                ReconcileStatus::WaitingForRetry
            };
            return Ok(self.outcome(status, Vec::new()));
        }

        // Make Pending work selectable without letting elapsed time advance:
        // availability is corrected from confirmed backend results below.
        scheduler.set_display_available(true, now, wall_now)?;
        let scheduler_plan = scheduler.display_plan();
        let content_revision = scheduler.content_revision();
        if content_revision != self.content_revision {
            self.content_revision = content_revision;
            self.retries.clear();
            self.dirty = true;
        }

        let (target, metadata) = self.build_plan(scheduler, &scheduler_plan, &eligible);
        self.update_animations(scheduler, &target, &metadata, now);
        if target != self.target_plan {
            self.dirty = true;
        }
        self.target_plan = target.clone();
        clear_enter_hints(&mut self.target_plan);
        self.target_metadata = metadata;
        if self.animation.advance(now) {
            self.dirty = true;
        }
        let mut desired = target;
        self.animation.apply(&mut desired);
        let retry_due = self
            .retries
            .values()
            .any(|retry| retry.attempts < MAX_RENDER_ATTEMPTS && retry.retry_at <= now);
        if !self.dirty && !retry_due {
            scheduler.set_display_available(!self.successful_windows.is_empty(), now, wall_now)?;
            let status = if self.retries.is_empty() {
                ReconcileStatus::Applied
            } else {
                ReconcileStatus::WaitingForRetry
            };
            return Ok(self.outcome(status, Vec::new()));
        }

        let report = match backend.reconcile(&desired) {
            Ok(report) => report,
            Err(_) => {
                self.disconnect(now);
                scheduler.set_display_available(false, now, wall_now)?;
                return Ok(self.outcome(ReconcileStatus::Disconnected, Vec::new()));
            }
        };
        self.apply_report(&eligible, &desired, report, now);
        self.dirty = false;

        let mut render_failed = Vec::new();
        let content_windows: Vec<_> = desired
            .windows
            .iter()
            .filter_map(|(window, displays)| (!displays.is_empty()).then_some(window))
            .collect();
        let all_exhausted = !content_windows.is_empty()
            && self.successful_windows.is_empty()
            && content_windows.iter().all(|window| {
                self.retries
                    .get(*window)
                    .is_some_and(|retry| retry.attempts >= MAX_RENDER_ATTEMPTS)
            });
        if all_exhausted {
            let ids = desired_notification_ids(&scheduler_plan);
            for id in ids {
                if scheduler.render_failed(id, now, wall_now).is_ok() {
                    render_failed.push(id);
                }
            }
            let _ = backend.reconcile(&DisplayPlan::default());
            self.retries.clear();
            self.projected_notifications
                .retain(|id| scheduler.notification(*id).is_some());
        }

        scheduler.set_display_available(!self.successful_windows.is_empty(), now, wall_now)?;
        let status = if self.retries.is_empty() {
            ReconcileStatus::Applied
        } else {
            ReconcileStatus::WaitingForRetry
        };
        Ok(self.outcome(status, render_failed))
    }

    fn build_plan(
        &mut self,
        scheduler: &LiveScheduler,
        scheduler_plan: &SchedulerDisplayPlan,
        eligible: &BTreeSet<WindowId>,
    ) -> (DisplayPlan, HashMap<String, DisplayMetadata>) {
        let initial_ids = desired_notification_ids(scheduler_plan)
            .into_iter()
            .filter(|id| !self.projected_notifications.contains(id))
            .collect::<HashSet<_>>();
        let mut plan = DisplayPlan::default();
        let mut metadata = HashMap::new();
        for window in eligible {
            let size = self.topology.window_size(window).unwrap_or(WindowSize {
                width: 80,
                height: 24,
            });
            let displays = if let Some(id) = scheduler_plan.attention {
                vec![self.planned_attention(
                    window,
                    id,
                    size,
                    initial_ids.contains(&id),
                    renderer_content(scheduler, id, self.policy),
                )]
            } else {
                self.planned_toasts(
                    scheduler,
                    window,
                    &scheduler_plan.toasts,
                    size,
                    &initial_ids,
                    &mut metadata,
                )
            };
            plan.windows.insert(window.clone(), displays);
        }
        self.projected_notifications
            .extend(desired_notification_ids(scheduler_plan));
        (plan, metadata)
    }

    fn planned_attention(
        &self,
        window: &WindowId,
        id: NotificationId,
        size: WindowSize,
        play_enter_animation: bool,
        content: RendererContent,
    ) -> PlannedDisplay {
        let width = if size.width >= 60 {
            ((u32::from(size.width) * 60) / 100).max(32) as u16
        } else {
            size.width
        };
        let height = if size.height >= 10 { 7 } else { size.height };
        PlannedDisplay {
            display_id: display_id(id, window),
            kind: DisplayKind::Attention,
            geometry: Geometry {
                x: size.width.saturating_sub(width) / 2,
                y: size.height.saturating_sub(height) / 2,
                width,
                height,
                z_index: 100,
            },
            content,
            play_enter_animation,
        }
    }

    fn planned_toasts(
        &self,
        scheduler: &LiveScheduler,
        window: &WindowId,
        ids: &[NotificationId],
        size: WindowSize,
        initial_ids: &HashSet<NotificationId>,
        metadata: &mut HashMap<String, DisplayMetadata>,
    ) -> Vec<PlannedDisplay> {
        if size.width < 24 || size.height == 0 {
            return Vec::new();
        }
        let (width, height) = if size.width < self.policy.toast_width {
            (size.width, 1)
        } else {
            (self.policy.toast_width, self.policy.toast_height)
        };
        let stride = height.saturating_add(self.policy.toast_gap).max(1);
        let capacity = usize::from(size.height.saturating_add(self.policy.toast_gap) / stride)
            .min(self.policy.max_visible_toasts);
        let ids = match self.policy.stack_order {
            StackOrder::OldestFirst => ids.iter().take(capacity).copied().collect::<Vec<_>>(),
            StackOrder::NewestFirst => ids.iter().rev().take(capacity).copied().collect(),
        };
        let mut placement_slots = [0_usize; 6];
        ids.into_iter()
            .enumerate()
            .map(|(z_index, id)| {
                let placement = scheduler
                    .notification(id)
                    .and_then(|notification| notification.overrides().position())
                    .map(config_placement)
                    .unwrap_or(self.policy.placement);
                let slot = &mut placement_slots[placement_index(placement)];
                let geometry =
                    toast_geometry(size, width, height, stride, *slot, placement, z_index);
                *slot = slot.saturating_add(1);
                let display_id = display_id(id, window);
                metadata.insert(
                    display_id.clone(),
                    DisplayMetadata {
                        notification_id: id,
                        placement,
                        viewport: Viewport {
                            width: size.width,
                            height: size.height,
                        },
                    },
                );
                PlannedDisplay {
                    display_id,
                    kind: DisplayKind::Toast,
                    geometry,
                    content: renderer_content(scheduler, id, self.policy),
                    play_enter_animation: initial_ids.contains(&id),
                }
            })
            .collect()
    }

    fn update_animations(
        &mut self,
        scheduler: &LiveScheduler,
        target: &DisplayPlan,
        metadata: &HashMap<String, DisplayMetadata>,
        now: MonotonicTime,
    ) {
        let policy = self.policy.animation;
        if !policy.enabled {
            self.animation.clear();
            return;
        }
        let target_displays = displays_by_id(target);
        let previous_displays = displays_by_id(&self.target_plan);
        for (id, (_, previous)) in &previous_displays {
            let still_live_but_removed = !target_displays.contains_key(id)
                && self
                    .target_metadata
                    .get(id)
                    .is_some_and(|meta| scheduler.notification(meta.notification_id).is_some());
            let reflowed = target_displays
                .get(id)
                .is_some_and(|(_, current)| current.geometry != previous.geometry);
            if still_live_but_removed || reflowed {
                self.animation.motions.remove(id);
                self.animation.exiting.remove(id);
            }
        }
        let mut specs = Vec::new();
        if !policy.enter_duration.is_zero() {
            for (id, (_, display)) in &target_displays {
                if !previous_displays.contains_key(id)
                    && display.kind == DisplayKind::Toast
                    && display.play_enter_animation
                    && let Some(meta) = metadata.get(id)
                {
                    let target_rect = rect_from_geometry(display.geometry);
                    specs.push(MotionSpec {
                        display_id: id.clone(),
                        from: offscreen_rect(target_rect, meta.viewport, meta.placement),
                        to: target_rect,
                        phase: MotionPhase::Enter,
                        duration: policy.enter_duration,
                        easing: policy.enter_easing,
                    });
                }
            }
        }
        if !policy.exit_duration.is_zero() {
            for (id, (window, display)) in previous_displays {
                let Some(meta) = self.target_metadata.get(&id) else {
                    continue;
                };
                if target_displays.contains_key(&id)
                    || display.kind != DisplayKind::Toast
                    || scheduler.notification(meta.notification_id).is_some()
                {
                    continue;
                }
                let from = self.animation.motions.get(&id).map_or_else(
                    || rect_from_geometry(display.geometry),
                    |motion| motion.current,
                );
                let mut exit_display = display.clone();
                exit_display.geometry = geometry_from_rect(from, display.geometry.z_index);
                exit_display.play_enter_animation = false;
                self.animation
                    .exiting
                    .insert(id.clone(), (window, exit_display));
                specs.push(MotionSpec {
                    display_id: id,
                    from,
                    to: offscreen_rect(from, meta.viewport, meta.placement),
                    phase: MotionPhase::Exit,
                    duration: policy.exit_duration,
                    easing: policy.exit_easing,
                });
            }
        }
        self.animation.start(specs, policy.fps, now);
    }

    fn apply_report(
        &mut self,
        eligible: &BTreeSet<WindowId>,
        desired: &DisplayPlan,
        report: ReconcileReport,
        now: MonotonicTime,
    ) {
        self.successful_windows
            .retain(|window| eligible.contains(window));
        for window in eligible {
            if report.applied.contains(window) {
                self.retries.remove(window);
                if desired
                    .windows
                    .get(window)
                    .is_some_and(|displays| !displays.is_empty())
                {
                    self.successful_windows.insert(window.clone());
                } else {
                    self.successful_windows.remove(window);
                }
            } else {
                self.record_failure(window.clone(), now);
                // A failed update does not destroy an already confirmed display.
                let _detail = report.failed.get(window);
            }
        }
        for window in report
            .failed
            .keys()
            .filter(|window| !eligible.contains(*window))
        {
            self.record_failure(window.clone(), now);
        }
    }

    fn record_failure(&mut self, window: WindowId, now: MonotonicTime) {
        let retry = self.retries.entry(window).or_insert(RetryState {
            attempts: 0,
            retry_at: now,
        });
        retry.attempts = retry.attempts.saturating_add(1);
        retry.retry_at = add_time(now, render_backoff(retry.attempts));
    }

    fn disconnect(&mut self, now: MonotonicTime) {
        self.connected = false;
        self.animation.pause(now);
        self.successful_windows.clear();
        self.next_reconnect_at = add_time(now, self.reconnect_backoff);
        self.reconnect_backoff = self
            .reconnect_backoff
            .saturating_mul(2)
            .min(MAX_RECONNECT_BACKOFF);
    }

    fn schedule_reconnect(&mut self, now: MonotonicTime) {
        self.connected = false;
        self.animation.pause(now);
        self.next_reconnect_at = add_time(now, self.reconnect_backoff);
        self.reconnect_backoff = self
            .reconnect_backoff
            .saturating_mul(2)
            .min(MAX_RECONNECT_BACKOFF);
    }

    fn outcome(
        &self,
        status: ReconcileStatus,
        render_failed: Vec<NotificationId>,
    ) -> ReconcileOutcome {
        let next_wakeup = if !self.connected {
            Some(self.next_reconnect_at)
        } else {
            self.retries
                .values()
                .filter(|retry| retry.attempts < MAX_RENDER_ATTEMPTS)
                .map(|retry| retry.retry_at)
                .min()
        };
        ReconcileOutcome {
            status,
            visible_windows: self.successful_windows.clone(),
            render_failed,
            next_wakeup,
        }
    }
}

fn config_placement(placement: crate::notification::Placement) -> Placement {
    match placement {
        crate::notification::Placement::TopLeft => Placement::TopLeft,
        crate::notification::Placement::TopCenter => Placement::TopCenter,
        crate::notification::Placement::TopRight => Placement::TopRight,
        crate::notification::Placement::BottomLeft => Placement::BottomLeft,
        crate::notification::Placement::BottomCenter => Placement::BottomCenter,
        crate::notification::Placement::BottomRight => Placement::BottomRight,
    }
}

fn placement_index(placement: Placement) -> usize {
    match placement {
        Placement::TopLeft => 0,
        Placement::TopCenter => 1,
        Placement::TopRight => 2,
        Placement::BottomLeft => 3,
        Placement::BottomCenter => 4,
        Placement::BottomRight => 5,
    }
}

fn toast_geometry(
    size: WindowSize,
    width: u16,
    height: u16,
    stride: u16,
    index: usize,
    placement: Placement,
    z_index: usize,
) -> Geometry {
    let offset = u16::try_from(index)
        .unwrap_or(u16::MAX)
        .saturating_mul(stride);
    let x = match placement {
        Placement::TopLeft | Placement::BottomLeft => 0,
        Placement::TopCenter | Placement::BottomCenter => size.width.saturating_sub(width) / 2,
        Placement::TopRight | Placement::BottomRight => size.width.saturating_sub(width),
    };
    let y = match placement {
        Placement::TopLeft | Placement::TopCenter | Placement::TopRight => offset,
        Placement::BottomLeft | Placement::BottomCenter | Placement::BottomRight => {
            size.height.saturating_sub(height).saturating_sub(offset)
        }
    };
    Geometry {
        x,
        y,
        width,
        height,
        z_index: u16::try_from(z_index).unwrap_or(u16::MAX),
    }
}

fn displays_by_id(plan: &DisplayPlan) -> HashMap<String, (WindowId, PlannedDisplay)> {
    plan.windows
        .iter()
        .flat_map(|(window, displays)| {
            displays.iter().map(|display| {
                (
                    display.display_id.clone(),
                    (window.clone(), display.clone()),
                )
            })
        })
        .collect()
}

fn clear_enter_hints(plan: &mut DisplayPlan) {
    for displays in plan.windows.values_mut() {
        for display in displays {
            display.play_enter_animation = false;
        }
    }
}

fn rect_from_geometry(geometry: Geometry) -> Rect {
    Rect {
        x: geometry.x,
        y: geometry.y,
        width: geometry.width,
        height: geometry.height,
    }
}

fn geometry_from_rect(rect: Rect, z_index: u16) -> Geometry {
    Geometry {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
        z_index,
    }
}

fn renderer_content(
    scheduler: &LiveScheduler,
    id: NotificationId,
    policy: WindowDisplayPolicy,
) -> RendererContent {
    RendererContent::from(
        scheduler
            .notification(id)
            .expect("display plan IDs must refer to live Notifications"),
    )
    .with_display_options(RendererDisplayOptions::new(
        match policy.body {
            BodyPresentation::FirstLine => RendererBodyMode::FirstLine,
            BodyPresentation::JoinLines => RendererBodyMode::JoinLines,
            BodyPresentation::Wrap => RendererBodyMode::Wrap,
        },
        policy.unicode,
        policy.color,
    ))
}

fn display_id(id: NotificationId, window: &WindowId) -> String {
    format!("{id}:{}", window.0)
}

fn desired_notification_ids(plan: &SchedulerDisplayPlan) -> Vec<NotificationId> {
    plan.attention
        .into_iter()
        .chain(plan.toasts.iter().copied())
        .collect()
}

fn render_backoff(attempt: u8) -> Duration {
    INITIAL_RENDER_BACKOFF.saturating_mul(u32::from(attempt.max(1)))
}

fn add_time(time: MonotonicTime, duration: Duration) -> MonotonicTime {
    MonotonicTime::from_duration(time.as_duration().saturating_add(duration))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};

    use chrono::TimeZone;

    use super::*;
    use crate::daemon::SchedulerLimits;
    use crate::notification::{
        CloseReason, NotificationDraft, Placement as NotificationPlacement, Presentation,
        PresentationOverrides, Priority, SourceContext, Timeout, TmuxServerId,
    };
    use crate::tmux::{ClientView, Error as TmuxError, Pane, PaneId};

    #[derive(Default)]
    struct FakeBackend {
        topology: Topology,
        events: VecDeque<Event>,
        reports: VecDeque<ReconcileReport>,
        plans: Vec<DisplayPlan>,
        topology_failures: usize,
    }

    impl Backend for FakeBackend {
        fn capabilities(&mut self) -> Result<crate::tmux::CapabilityReport, TmuxError> {
            unreachable!()
        }

        fn topology(&mut self) -> Result<Topology, TmuxError> {
            if self.topology_failures > 0 {
                self.topology_failures -= 1;
                return Err(TmuxError::Protocol("disconnected".into()));
            }
            Ok(self.topology.clone())
        }

        fn next_event(&mut self) -> Result<Option<Event>, TmuxError> {
            Ok(self.events.pop_front())
        }

        fn reconcile(&mut self, desired: &DisplayPlan) -> Result<ReconcileReport, TmuxError> {
            self.plans.push(desired.clone());
            Ok(self.reports.pop_front().unwrap_or_else(|| ReconcileReport {
                applied: desired.windows.keys().cloned().collect(),
                failed: BTreeMap::new(),
            }))
        }

        fn jump(&mut self, _: &crate::tmux::JumpTarget) -> Result<(), TmuxError> {
            unreachable!()
        }
    }

    fn mono(milliseconds: u64) -> MonotonicTime {
        MonotonicTime::from_duration(Duration::from_millis(milliseconds))
    }

    fn wall(milliseconds: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(milliseconds).single().unwrap()
    }

    fn scheduler() -> LiveScheduler {
        LiveScheduler::new(SchedulerLimits::new(20, 4).unwrap(), mono(0))
    }

    fn source() -> SourceContext {
        SourceContext::new(TmuxServerId::new("server").unwrap(), "$1", "@1", "%1").unwrap()
    }

    fn toast(title: &str) -> NotificationDraft {
        NotificationDraft::new(Presentation::Toast, title, "body", Some(source()))
            .unwrap()
            .with_priority(Priority::Normal)
            .with_timeout(Timeout::Never)
    }

    fn finite_toast(title: &str, timeout: Duration) -> NotificationDraft {
        NotificationDraft::new(Presentation::Toast, title, "body", Some(source()))
            .unwrap()
            .with_priority(Priority::Normal)
            .with_timeout(Timeout::After(timeout))
    }

    fn topology(clients: &[(&str, bool, &str)], windows: &[(&str, u16, u16)]) -> Topology {
        let clients = clients
            .iter()
            .map(|(name, control, window)| ClientView {
                name: (*name).into(),
                is_control: *control,
                session_id: "$1".into(),
                window_id: WindowId((*window).into()),
                last_activity: 1,
            })
            .collect();
        let panes = windows
            .iter()
            .enumerate()
            .map(|(index, (window, width, height))| {
                let id = PaneId(format!("%{}", index + 1));
                (
                    id.clone(),
                    Pane {
                        id,
                        session_id: "$1".into(),
                        window_id: WindowId((*window).into()),
                        is_floating: false,
                        left: 0,
                        top: 0,
                        width: *width,
                        height: *height,
                    },
                )
            })
            .collect();
        Topology { clients, panes }
    }

    fn report(applied: &[&str], failed: &[&str]) -> ReconcileReport {
        ReconcileReport {
            applied: applied
                .iter()
                .map(|window| WindowId((*window).into()))
                .collect(),
            failed: failed
                .iter()
                .map(|window| (WindowId((*window).into()), "failed".into()))
                .collect(),
        }
    }

    #[test]
    fn deduplicates_shared_windows_and_excludes_control_clients() {
        let mut backend = FakeBackend {
            topology: topology(
                &[("a", false, "@1"), ("b", false, "@1"), ("ctl", true, "@9")],
                &[("@1", 80, 24), ("@9", 80, 24)],
            ),
            ..FakeBackend::default()
        };
        let mut scheduler = scheduler();
        scheduler.submit(toast("done"), mono(0), wall(0)).unwrap();
        let mut reconciler = WindowReconciler::new(WindowDisplayPolicy::default(), mono(0));

        let outcome = reconciler
            .tick(&mut backend, &mut scheduler, mono(0), wall(0))
            .unwrap();

        assert_eq!(outcome.visible_windows, [WindowId("@1".into())].into());
        assert_eq!(backend.plans[0].windows.len(), 1);
        assert_eq!(backend.plans[0].windows[&WindowId("@1".into())].len(), 1);
    }

    #[test]
    fn configured_stack_and_per_notification_placement_reach_display_geometry() {
        let mut backend = FakeBackend {
            topology: topology(&[("a", false, "@1")], &[("@1", 80, 24)]),
            ..FakeBackend::default()
        };
        let mut scheduler = scheduler();
        let first = scheduler.submit(toast("first"), mono(0), wall(0)).unwrap();
        let second = scheduler
            .submit(
                toast("second").with_overrides(
                    PresentationOverrides::default()
                        .with_position(NotificationPlacement::BottomLeft),
                ),
                mono(1),
                wall(1),
            )
            .unwrap();
        let mut reconciler = WindowReconciler::new(
            WindowDisplayPolicy {
                placement: Placement::BottomCenter,
                stack_order: StackOrder::NewestFirst,
                animation: ToastAnimationPolicy {
                    enabled: false,
                    ..ToastAnimationPolicy::default()
                },
                ..WindowDisplayPolicy::default()
            },
            mono(0),
        );

        reconciler
            .tick(&mut backend, &mut scheduler, mono(1), wall(1))
            .unwrap();
        let displays = &backend.plans[0].windows[&WindowId("@1".into())];
        assert!(displays[0].display_id.starts_with(&second.id.to_string()));
        assert_eq!(displays[0].geometry.x, 0);
        assert_eq!(displays[0].geometry.y, 21);
        assert!(displays[1].display_id.starts_with(&first.id.to_string()));
        assert_eq!(displays[1].geometry.x, 19);
        assert_eq!(displays[1].geometry.y, 21);
    }

    #[test]
    fn attach_detach_and_move_recreate_without_replaying_enter() {
        let mut backend = FakeBackend {
            topology: topology(&[("a", false, "@1")], &[("@1", 80, 24), ("@2", 80, 24)]),
            ..FakeBackend::default()
        };
        let mut scheduler = scheduler();
        let sent = scheduler.submit(toast("done"), mono(0), wall(0)).unwrap();
        let mut reconciler = WindowReconciler::new(WindowDisplayPolicy::default(), mono(0));
        reconciler
            .tick(&mut backend, &mut scheduler, mono(0), wall(0))
            .unwrap();
        assert!(backend.plans[0].windows[&WindowId("@1".into())][0].play_enter_animation);

        backend.topology = topology(&[("a", false, "@2")], &[("@1", 80, 24), ("@2", 80, 24)]);
        backend.events.push_back(Event::TopologyChanged);
        reconciler
            .tick(&mut backend, &mut scheduler, mono(1), wall(1))
            .unwrap();
        let moved = backend.plans.last().unwrap();
        assert!(!moved.windows.contains_key(&WindowId("@1".into())));
        assert!(!moved.windows[&WindowId("@2".into())][0].play_enter_animation);
        assert_eq!(
            scheduler.notification(sent.id).unwrap().timeout(),
            Timeout::Never
        );
        reconciler
            .tick(&mut backend, &mut scheduler, mono(50), wall(50))
            .unwrap();
        assert_eq!(
            backend.plans.len(),
            2,
            "abandoned enter frames stay cancelled"
        );

        backend.topology = topology(&[], &[("@2", 80, 24)]);
        backend.events.push_back(Event::TopologyChanged);
        reconciler
            .tick(&mut backend, &mut scheduler, mono(51), wall(51))
            .unwrap();
        assert!(
            backend
                .plans
                .last()
                .unwrap()
                .windows
                .values()
                .all(Vec::is_empty)
        );
        assert_eq!(
            scheduler.notification(sent.id).unwrap().delivery(),
            crate::notification::DeliveryState::Visible
        );
    }

    #[test]
    fn production_plan_runs_batched_enter_stay_exit_frames_and_timeout() {
        let mut backend = FakeBackend {
            topology: topology(
                &[("a", false, "@1"), ("b", false, "@2")],
                &[("@1", 100, 24), ("@2", 80, 24)],
            ),
            ..FakeBackend::default()
        };
        let mut scheduler = scheduler();
        let sent = scheduler
            .submit(
                finite_toast("animated", Duration::from_millis(200)),
                mono(0),
                wall(0),
            )
            .unwrap();
        let mut reconciler = WindowReconciler::new(
            WindowDisplayPolicy {
                animation: ToastAnimationPolicy {
                    fps: 20,
                    enter_duration: Duration::from_millis(100),
                    exit_duration: Duration::from_millis(100),
                    ..ToastAnimationPolicy::default()
                },
                ..WindowDisplayPolicy::default()
            },
            mono(0),
        );

        reconciler
            .tick(&mut backend, &mut scheduler, mono(0), wall(0))
            .unwrap();
        let entered = backend.plans.last().unwrap();
        assert_eq!(
            entered.windows.len(),
            2,
            "all windows share one frame batch"
        );
        assert_eq!(entered.windows[&WindowId("@1".into())][0].geometry.x, 16);
        assert_eq!(entered.windows[&WindowId("@2".into())][0].geometry.x, 0);

        reconciler
            .tick(&mut backend, &mut scheduler, mono(10), wall(10))
            .unwrap();
        assert_eq!(backend.plans.len(), 1, "quantized stay emits no duplicate");
        reconciler
            .tick(&mut backend, &mut scheduler, mono(50), wall(50))
            .unwrap();
        assert_eq!(backend.plans.len(), 2);
        let midway = backend.plans.last().unwrap();
        assert_eq!(midway.windows[&WindowId("@1".into())][0].geometry.x, 48);
        assert_eq!(midway.windows[&WindowId("@2".into())][0].geometry.x, 29);

        reconciler
            .tick(&mut backend, &mut scheduler, mono(100), wall(100))
            .unwrap();
        assert_eq!(
            backend.plans.last().unwrap().windows[&WindowId("@1".into())][0]
                .geometry
                .x,
            58
        );
        assert!(scheduler.notification(sent.id).is_some());

        reconciler
            .tick(&mut backend, &mut scheduler, mono(199), wall(199))
            .unwrap();
        assert!(scheduler.notification(sent.id).is_some());
        let stay_plan_count = backend.plans.len();
        reconciler
            .tick(&mut backend, &mut scheduler, mono(200), wall(200))
            .unwrap();
        assert!(scheduler.notification(sent.id).is_none());
        assert_eq!(backend.plans.len(), stay_plan_count + 1);
        assert!(!backend.plans.last().unwrap().windows.is_empty());

        reconciler
            .tick(&mut backend, &mut scheduler, mono(300), wall(300))
            .unwrap();
        assert_eq!(
            backend.plans.last().unwrap().windows[&WindowId("@1".into())][0]
                .geometry
                .x,
            16
        );
        reconciler
            .tick(&mut backend, &mut scheduler, mono(301), wall(301))
            .unwrap();
        assert!(
            backend
                .plans
                .last()
                .unwrap()
                .windows
                .values()
                .all(Vec::is_empty)
        );
    }

    #[test]
    fn display_policy_reload_reconciles_without_replaying_enter() {
        let mut backend = FakeBackend {
            topology: topology(&[("a", false, "@1")], &[("@1", 100, 24)]),
            ..FakeBackend::default()
        };
        let mut scheduler = scheduler();
        scheduler
            .submit(toast("configured"), mono(0), wall(0))
            .unwrap();
        let disabled = ToastAnimationPolicy {
            enabled: false,
            ..ToastAnimationPolicy::default()
        };
        let mut reconciler = WindowReconciler::new(
            WindowDisplayPolicy {
                animation: disabled,
                ..WindowDisplayPolicy::default()
            },
            mono(0),
        );
        reconciler
            .tick(&mut backend, &mut scheduler, mono(0), wall(0))
            .unwrap();
        reconciler.update_policy(WindowDisplayPolicy {
            placement: Placement::BottomLeft,
            toast_width: 50,
            animation: disabled,
            ..WindowDisplayPolicy::default()
        });
        reconciler
            .tick(&mut backend, &mut scheduler, mono(1), wall(1))
            .unwrap();

        let display = &backend.plans.last().unwrap().windows[&WindowId("@1".into())][0];
        assert_eq!(
            (
                display.geometry.x,
                display.geometry.y,
                display.geometry.width
            ),
            (0, 21, 50)
        );
        assert!(!display.play_enter_animation);
    }

    #[test]
    fn partial_success_retries_only_after_backoff_and_all_exhaustion_closes() {
        let mut backend = FakeBackend {
            topology: topology(
                &[("a", false, "@1"), ("b", false, "@2")],
                &[("@1", 80, 24), ("@2", 80, 24)],
            ),
            reports: [report(&["@1"], &["@2"]), report(&["@1"], &["@2"])].into(),
            ..FakeBackend::default()
        };
        let mut partial_scheduler = scheduler();
        partial_scheduler
            .submit(toast("partial"), mono(0), wall(0))
            .unwrap();
        let mut reconciler = WindowReconciler::new(WindowDisplayPolicy::default(), mono(0));

        let first = reconciler
            .tick(&mut backend, &mut partial_scheduler, mono(0), wall(0))
            .unwrap();
        assert_eq!(first.visible_windows, [WindowId("@1".into())].into());
        reconciler
            .tick(&mut backend, &mut partial_scheduler, mono(49), wall(49))
            .unwrap();
        assert_eq!(backend.plans.len(), 1);
        reconciler
            .tick(&mut backend, &mut partial_scheduler, mono(50), wall(50))
            .unwrap();
        assert_eq!(backend.plans.len(), 2);

        let mut failed_backend = FakeBackend {
            topology: topology(&[("a", false, "@1")], &[("@1", 80, 24)]),
            reports: [
                report(&[], &["@1"]),
                report(&[], &["@1"]),
                report(&[], &["@1"]),
            ]
            .into(),
            ..FakeBackend::default()
        };
        let mut failed_scheduler = scheduler();
        let sent = failed_scheduler
            .submit(toast("failed"), mono(0), wall(0))
            .unwrap();
        let mut failed_reconciler = WindowReconciler::new(WindowDisplayPolicy::default(), mono(0));
        for time in [0, 50, 150] {
            let outcome = failed_reconciler
                .tick(
                    &mut failed_backend,
                    &mut failed_scheduler,
                    mono(time),
                    wall(time as i64),
                )
                .unwrap();
            if time == 150 {
                assert_eq!(outcome.render_failed, vec![sent.id]);
            }
        }
        assert_eq!(
            failed_scheduler
                .closed_notification(sent.id)
                .unwrap()
                .close_reason(),
            Some(CloseReason::RenderFailed)
        );
    }

    #[test]
    fn content_and_topology_changes_reset_failed_window_budget() {
        let mut backend = FakeBackend {
            topology: topology(&[("a", false, "@1")], &[("@1", 80, 24)]),
            reports: [
                report(&[], &["@1"]),
                report(&[], &["@1"]),
                report(&["@1"], &[]),
            ]
            .into(),
            ..FakeBackend::default()
        };
        let mut scheduler = scheduler();
        let sent = scheduler.submit(toast("first"), mono(0), wall(0)).unwrap();
        let mut reconciler = WindowReconciler::new(WindowDisplayPolicy::default(), mono(0));
        reconciler
            .tick(&mut backend, &mut scheduler, mono(0), wall(0))
            .unwrap();
        scheduler
            .update(
                sent.id,
                crate::notification::NotificationUpdate::new()
                    .with_title("changed")
                    .unwrap(),
                mono(1),
                wall(1),
            )
            .unwrap();
        reconciler
            .tick(&mut backend, &mut scheduler, mono(1), wall(1))
            .unwrap();
        backend.events.push_back(Event::TopologyChanged);
        reconciler
            .tick(&mut backend, &mut scheduler, mono(2), wall(2))
            .unwrap();
        assert_eq!(backend.plans.len(), 3);
        assert!(scheduler.notification(sent.id).is_some());
    }

    #[test]
    fn each_window_has_independent_capacity() {
        let mut backend = FakeBackend {
            topology: topology(
                &[("large", false, "@1"), ("small", false, "@2")],
                &[("@1", 100, 24), ("@2", 30, 2)],
            ),
            ..FakeBackend::default()
        };
        let mut scheduler = scheduler();
        for title in ["one", "two", "three", "four"] {
            scheduler.submit(toast(title), mono(0), wall(0)).unwrap();
        }
        let mut reconciler = WindowReconciler::new(WindowDisplayPolicy::default(), mono(0));
        reconciler
            .tick(&mut backend, &mut scheduler, mono(0), wall(0))
            .unwrap();

        let plan = &backend.plans[0];
        assert_eq!(plan.windows[&WindowId("@1".into())].len(), 4);
        assert_eq!(plan.windows[&WindowId("@2".into())].len(), 1);
    }

    #[test]
    fn disconnect_pauses_and_reconnects_with_exponential_backoff_and_full_plan() {
        let mut backend = FakeBackend {
            topology: topology(&[("a", false, "@1")], &[("@1", 80, 24)]),
            ..FakeBackend::default()
        };
        let mut scheduler = scheduler();
        scheduler.submit(toast("done"), mono(0), wall(0)).unwrap();
        let mut reconciler = WindowReconciler::new(WindowDisplayPolicy::default(), mono(0));
        reconciler
            .tick(&mut backend, &mut scheduler, mono(0), wall(0))
            .unwrap();
        backend.events.push_back(Event::Disconnected);
        let disconnected = reconciler
            .tick(&mut backend, &mut scheduler, mono(1), wall(1))
            .unwrap();
        assert_eq!(disconnected.status, ReconcileStatus::Disconnected);
        assert_eq!(disconnected.next_wakeup, Some(mono(101)));

        backend.topology_failures = 1;
        let failed_reconnect = reconciler
            .tick(&mut backend, &mut scheduler, mono(101), wall(101))
            .unwrap();
        assert_eq!(failed_reconnect.next_wakeup, Some(mono(301)));
        reconciler
            .tick(&mut backend, &mut scheduler, mono(301), wall(301))
            .unwrap();
        assert_eq!(backend.plans.len(), 2);
        assert_eq!(
            backend.plans.last().unwrap().windows[&WindowId("@1".into())][0]
                .geometry
                .x,
            0,
            "control loss pauses rather than completes enter animation"
        );
    }
}
