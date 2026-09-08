//! Request orchestration for one daemon.
//!
//! Scheduler mutation is short and synchronous. History and tmux work happens
//! only after its lock is released, so SQLite contention or a slow jump cannot
//! block timer/display state access.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;

use super::{LiveScheduler, MonotonicTime, SchedulerError, SubmitDisposition, UpdateDisposition};
use crate::history::{ClearFilter, History, HistoryError, HistoryQuery, PersistenceStatus};
use crate::notification::{
    Notification, NotificationId, NotificationKey, Presentation, SourceContext,
};
use crate::protocol::{
    ClientCommand, ClientRequest, Disposition, ProtocolError, RendererAction, RequestEnvelope,
    TargetSelector, WireHistoryClear,
};

pub trait JumpExecutor: Send + Sync + 'static {
    fn jump(
        &self,
        source: SourceContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;

    /// Attention actions include the authenticated Attention Window so a tmux
    /// executor can select the most recently active client viewing it.
    fn jump_from_attention(
        &self,
        source: SourceContext,
        _attention_window: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.jump(source)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ServiceResponse {
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_id: Option<NotificationId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_persisted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<Disposition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ServiceResponse {
    fn notification(id: NotificationId, disposition: Disposition, history_persisted: bool) -> Self {
        Self {
            accepted: true,
            notification_id: Some(id),
            history_persisted: Some(history_persisted),
            disposition: Some(disposition),
            data: None,
        }
    }

    fn data(data: Value) -> Self {
        Self {
            accepted: true,
            notification_id: None,
            history_persisted: None,
            disposition: None,
            data: Some(data),
        }
    }
}

pub struct DaemonService<J> {
    scheduler: Arc<Mutex<LiveScheduler>>,
    history: Arc<History>,
    jump: Arc<J>,
}

impl<J> Clone for DaemonService<J> {
    fn clone(&self) -> Self {
        Self {
            scheduler: Arc::clone(&self.scheduler),
            history: Arc::clone(&self.history),
            jump: Arc::clone(&self.jump),
        }
    }
}

impl<J: JumpExecutor> DaemonService<J> {
    #[must_use]
    pub fn new(scheduler: LiveScheduler, history: Arc<History>, jump: J) -> Self {
        Self {
            scheduler: Arc::new(Mutex::new(scheduler)),
            history,
            jump: Arc::new(jump),
        }
    }

    #[must_use]
    pub fn scheduler(&self) -> Arc<Mutex<LiveScheduler>> {
        Arc::clone(&self.scheduler)
    }

    pub async fn handle_envelope(
        &self,
        envelope: &RequestEnvelope,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<ServiceResponse, ServiceError> {
        let request = ClientRequest::from_envelope(envelope)?;
        self.handle(request.command, monotonic_now, wall_now).await
    }

    pub async fn handle(
        &self,
        command: ClientCommand,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<ServiceResponse, ServiceError> {
        match command {
            ClientCommand::Send { notification } => {
                let draft = (*notification).into_domain()?;
                let (outcome, snapshots) = {
                    let mut scheduler = self.scheduler.lock().await;
                    let outcome = scheduler.submit(draft, monotonic_now, wall_now)?;
                    let mut snapshots = scheduler
                        .notification(outcome.id)
                        .cloned()
                        .into_iter()
                        .collect::<Vec<_>>();
                    snapshots.extend(scheduler.drain_closed());
                    (outcome, snapshots)
                };
                let persisted = persist_snapshots(&self.history, snapshots).await;
                Ok(ServiceResponse::notification(
                    outcome.id,
                    submit_disposition(outcome.disposition),
                    persisted,
                ))
            }
            ClientCommand::Update { selector, update } => {
                let selector = selector.into_domain()?;
                let update = update.into_domain()?;
                if update.is_empty() {
                    return Err(ServiceError::Protocol(ProtocolError::EmptyUpdate));
                }
                let (id, disposition, snapshots) = {
                    let mut scheduler = self.scheduler.lock().await;
                    let (id, disposition) = match selector {
                        TargetSelector::Id(id) => {
                            let disposition =
                                scheduler.update(id, update, monotonic_now, wall_now)?;
                            (id, disposition)
                        }
                        TargetSelector::Key(key) => {
                            let id = scheduler
                                .id_for_key(&key)
                                .ok_or(SchedulerError::KeyNotLive)?;
                            let disposition =
                                scheduler.update_by_key(&key, update, monotonic_now, wall_now)?;
                            (id, disposition)
                        }
                    };
                    let mut snapshots = scheduler
                        .notification(id)
                        .cloned()
                        .into_iter()
                        .collect::<Vec<_>>();
                    snapshots.extend(scheduler.drain_closed());
                    (id, disposition, snapshots)
                };
                let persisted = persist_snapshots(&self.history, snapshots).await;
                Ok(ServiceResponse::notification(
                    id,
                    match disposition {
                        UpdateDisposition::Updated => Disposition::Updated,
                        UpdateDisposition::Duplicate => Disposition::Duplicate,
                    },
                    persisted,
                ))
            }
            ClientCommand::Dismiss { selector } => {
                let selector = selector.into_domain()?;
                let (id, snapshots) = {
                    let mut scheduler = self.scheduler.lock().await;
                    let id = match selector {
                        TargetSelector::Id(id) => {
                            scheduler.dismiss(id, monotonic_now, wall_now)?;
                            id
                        }
                        TargetSelector::Key(key) => {
                            scheduler.dismiss_by_key(&key, monotonic_now, wall_now)?
                        }
                    };
                    (id, scheduler.drain_closed())
                };
                let persisted = persist_snapshots(&self.history, snapshots).await;
                Ok(ServiceResponse::notification(
                    id,
                    Disposition::Updated,
                    persisted,
                ))
            }
            ClientCommand::Jump { key } => {
                let key = NotificationKey::new(&key).map_err(ProtocolError::Ingress)?;
                let intent = {
                    self.scheduler
                        .lock()
                        .await
                        .begin_jump_by_key(&key, monotonic_now, wall_now)?
                };
                let id = intent.notification_id();
                match self.jump.jump(intent.source().clone()).await {
                    Ok(()) => {
                        let snapshots = {
                            let mut scheduler = self.scheduler.lock().await;
                            scheduler.commit_jump(intent, monotonic_now, wall_now)?;
                            scheduler.drain_closed()
                        };
                        let persisted = persist_snapshots(&self.history, snapshots).await;
                        Ok(ServiceResponse::notification(
                            id,
                            Disposition::Updated,
                            persisted,
                        ))
                    }
                    Err(error) => {
                        self.scheduler
                            .lock()
                            .await
                            .cancel_jump(intent, monotonic_now, wall_now)?;
                        Err(ServiceError::Jump(error))
                    }
                }
            }
            ClientCommand::History {
                include_hidden,
                all_servers,
                limit,
            } => {
                if limit == 0 || limit > 100_000 {
                    return Err(ServiceError::Protocol(ProtocolError::InvalidHistoryLimit));
                }
                let entries = self
                    .history
                    .list(HistoryQuery {
                        include_hidden,
                        all_servers,
                        limit,
                    })?
                    .wait()
                    .await?;
                Ok(ServiceResponse::data(serde_json::to_value(entries)?))
            }
            ClientCommand::HistoryClear {
                selector,
                all_servers,
            } => {
                let filter = match selector {
                    WireHistoryClear::Hidden => ClearFilter::Hidden,
                    WireHistoryClear::BeforeMillis(value) => ClearFilter::Before(
                        DateTime::from_timestamp_millis(value)
                            .ok_or(ServiceError::InvalidClearTimestamp)?,
                    ),
                    WireHistoryClear::All => ClearFilter::All,
                };
                let count = self.history.clear(filter, all_servers)?.wait().await?;
                Ok(ServiceResponse::data(
                    serde_json::json!({ "deleted": count }),
                ))
            }
        }
    }

    /// Resolves an action from an already-authenticated renderer connection.
    /// The display ID is `<notification UUID>:<stable window ID>`; there is no
    /// caller-supplied selector that could target a different Notification.
    pub async fn handle_renderer_action(
        &self,
        window_display_id: &str,
        action: RendererAction,
        monotonic_now: MonotonicTime,
        wall_now: DateTime<Utc>,
    ) -> Result<(), ServiceError> {
        let (notification, attention_window) = parse_window_display_id(window_display_id)?;
        {
            let scheduler = self.scheduler.lock().await;
            let live = scheduler
                .notification(notification)
                .ok_or(SchedulerError::NotificationNotLive)?;
            if live.presentation() != Presentation::Attention {
                return Err(ServiceError::RendererActionNotAttention);
            }
        }

        match action {
            RendererAction::Dismiss => {
                let snapshots = {
                    let mut scheduler = self.scheduler.lock().await;
                    scheduler.dismiss(notification, monotonic_now, wall_now)?;
                    scheduler.drain_closed()
                };
                let _ = persist_snapshots(&self.history, snapshots).await;
                Ok(())
            }
            RendererAction::Jump => {
                let intent = self.scheduler.lock().await.begin_jump(
                    notification,
                    monotonic_now,
                    wall_now,
                )?;
                match self
                    .jump
                    .jump_from_attention(intent.source().clone(), attention_window)
                    .await
                {
                    Ok(()) => {
                        let snapshots = {
                            let mut scheduler = self.scheduler.lock().await;
                            scheduler.commit_jump(intent, monotonic_now, wall_now)?;
                            scheduler.drain_closed()
                        };
                        let _ = persist_snapshots(&self.history, snapshots).await;
                        Ok(())
                    }
                    Err(error) => {
                        self.scheduler
                            .lock()
                            .await
                            .cancel_jump(intent, monotonic_now, wall_now)?;
                        Err(ServiceError::Jump(error))
                    }
                }
            }
        }
    }
}

fn parse_window_display_id(value: &str) -> Result<(NotificationId, String), ServiceError> {
    let (notification, window) = value
        .split_once(':')
        .ok_or(ServiceError::InvalidWindowDisplay)?;
    let notification = notification
        .parse()
        .map_err(|_| ServiceError::InvalidWindowDisplay)?;
    if !window.strip_prefix('@').is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return Err(ServiceError::InvalidWindowDisplay);
    }
    Ok((notification, window.to_owned()))
}

async fn persist_snapshots(history: &History, snapshots: Vec<Notification>) -> bool {
    let mut persisted = true;
    for notification in snapshots {
        match history.persist(&notification) {
            Ok(task) => match task.wait().await {
                Ok(PersistenceStatus::Persisted | PersistenceStatus::Disabled) => {}
                Err(_) => persisted = false,
            },
            Err(_) => persisted = false,
        }
    }
    persisted
}

fn submit_disposition(disposition: SubmitDisposition) -> Disposition {
    match disposition {
        SubmitDisposition::Queued => Disposition::Queued,
        SubmitDisposition::Visible => Disposition::Visible,
        SubmitDisposition::Updated => Disposition::Updated,
        SubmitDisposition::Duplicate => Disposition::Duplicate,
        SubmitDisposition::Suppressed => Disposition::Suppressed,
    }
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("failed to encode response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Source Pane jump failed: {0}")]
    Jump(String),
    #[error("History clear timestamp is invalid")]
    InvalidClearTimestamp,
    #[error("invalid authenticated Window Display ID")]
    InvalidWindowDisplay,
    #[error("renderer actions are valid only for an Attention Gate")]
    RendererActionNotAttention,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::config::HistoryConfig;
    use crate::notification::{NotificationDraft, Presentation, TmuxServerId};
    use crate::protocol::{WireNotificationDraft, WireSelector};

    #[derive(Default)]
    struct FakeJump {
        fail: bool,
        sources: StdMutex<Vec<SourceContext>>,
        attention_windows: StdMutex<Vec<String>>,
    }

    impl JumpExecutor for FakeJump {
        fn jump(
            &self,
            source: SourceContext,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
            self.sources.lock().unwrap().push(source);
            let result = if self.fail {
                Err("pane missing".to_owned())
            } else {
                Ok(())
            };
            Box::pin(async move { result })
        }

        fn jump_from_attention(
            &self,
            source: SourceContext,
            attention_window: String,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
            self.attention_windows
                .lock()
                .unwrap()
                .push(attention_window);
            self.jump(source)
        }
    }

    fn times() -> (MonotonicTime, DateTime<Utc>) {
        (
            MonotonicTime::from_duration(Duration::from_secs(1)),
            DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
        )
    }

    fn source() -> SourceContext {
        SourceContext::new(TmuxServerId::new("server").unwrap(), "$1", "@2", "%3").unwrap()
    }

    fn service(temp: &TempDir, jump: FakeJump) -> DaemonService<FakeJump> {
        let history = History::open(
            temp.path().join("state/history.sqlite3"),
            TmuxServerId::new("server").unwrap(),
            &HistoryConfig {
                enabled: true,
                max_entries: 100,
            },
            Utc::now(),
        )
        .unwrap();
        DaemonService::new(
            LiveScheduler::new(
                super::super::SchedulerLimits::new(100, 4).unwrap(),
                MonotonicTime::default(),
            ),
            Arc::new(history),
            jump,
        )
    }

    fn keyed_send(key: &str) -> ClientCommand {
        let draft = NotificationDraft::new(Presentation::Toast, "build", "done", Some(source()))
            .unwrap()
            .with_key(NotificationKey::new(key).unwrap());
        ClientCommand::Send {
            notification: Box::new(WireNotificationDraft::from_domain(&draft)),
        }
    }

    fn attention_send() -> ClientCommand {
        let draft = NotificationDraft::new(
            Presentation::Attention,
            "input needed",
            "review source",
            Some(source()),
        )
        .unwrap();
        ClientCommand::Send {
            notification: Box::new(WireNotificationDraft::from_domain(&draft)),
        }
    }

    #[tokio::test]
    async fn authenticated_attention_action_uses_display_window_and_closes_globally() {
        let temporary = TempDir::new().unwrap();
        let service = service(&temporary, FakeJump::default());
        let (mono, wall) = times();
        let sent = service.handle(attention_send(), mono, wall).await.unwrap();
        let display = format!("{}:@7", sent.notification_id.unwrap());
        service
            .handle_renderer_action(&display, RendererAction::Jump, mono, wall)
            .await
            .unwrap();
        assert_eq!(
            service.jump.attention_windows.lock().unwrap().as_slice(),
            &["@7"]
        );
        assert!(
            service
                .scheduler
                .lock()
                .await
                .notification(sent.notification_id.unwrap())
                .is_none()
        );
    }

    #[tokio::test]
    async fn renderer_action_rejects_toast_and_bad_display_identity() {
        let temporary = TempDir::new().unwrap();
        let service = service(&temporary, FakeJump::default());
        let (mono, wall) = times();
        let sent = service
            .handle(keyed_send("build"), mono, wall)
            .await
            .unwrap();
        assert!(matches!(
            service
                .handle_renderer_action(
                    &format!("{}:@1", sent.notification_id.unwrap()),
                    RendererAction::Dismiss,
                    mono,
                    wall,
                )
                .await,
            Err(ServiceError::RendererActionNotAttention)
        ));
        assert!(matches!(
            service
                .handle_renderer_action("not-a-display", RendererAction::Jump, mono, wall)
                .await,
            Err(ServiceError::InvalidWindowDisplay)
        ));
    }

    #[tokio::test]
    async fn send_persists_and_confirmed_key_jump_closes_as_jumped() {
        let temporary = TempDir::new().unwrap();
        let service = service(&temporary, FakeJump::default());
        let (mono, wall) = times();
        let sent = service
            .handle(keyed_send("build"), mono, wall)
            .await
            .unwrap();
        assert!(sent.accepted);
        assert_eq!(sent.history_persisted, Some(true));

        let jumped = service
            .handle(
                ClientCommand::Jump {
                    key: "build".into(),
                },
                mono,
                wall,
            )
            .await
            .unwrap();
        assert_eq!(jumped.notification_id, sent.notification_id);
        let entries = service
            .history
            .list(HistoryQuery::default())
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(
            entries[0].close_reason,
            Some(crate::notification::CloseReason::Jumped)
        );
    }

    #[tokio::test]
    async fn failed_jump_cancels_intent_and_leaves_notification_live() {
        let temporary = TempDir::new().unwrap();
        let service = service(
            &temporary,
            FakeJump {
                fail: true,
                ..FakeJump::default()
            },
        );
        let (mono, wall) = times();
        let sent = service
            .handle(keyed_send("build"), mono, wall)
            .await
            .unwrap();
        assert!(matches!(
            service
                .handle(
                    ClientCommand::Jump {
                        key: "build".into()
                    },
                    mono,
                    wall
                )
                .await,
            Err(ServiceError::Jump(_))
        ));
        assert!(
            service
                .scheduler
                .lock()
                .await
                .notification(sent.notification_id.unwrap())
                .is_some()
        );
    }

    #[tokio::test]
    async fn update_dismiss_and_history_query_share_one_lifecycle() {
        let temporary = TempDir::new().unwrap();
        let service = service(&temporary, FakeJump::default());
        let (mono, wall) = times();
        let sent = service
            .handle(keyed_send("build"), mono, wall)
            .await
            .unwrap();
        let update = crate::notification::NotificationUpdate::new()
            .with_body("really done")
            .unwrap();
        service
            .handle(
                ClientCommand::Update {
                    selector: WireSelector {
                        id: None,
                        key: Some("build".into()),
                    },
                    update: crate::protocol::WireNotificationUpdate::from_domain(&update),
                },
                mono,
                wall,
            )
            .await
            .unwrap();
        service
            .handle(
                ClientCommand::Dismiss {
                    selector: WireSelector {
                        id: Some(sent.notification_id.unwrap().to_string()),
                        key: None,
                    },
                },
                mono,
                wall,
            )
            .await
            .unwrap();
        let response = service
            .handle(
                ClientCommand::History {
                    include_hidden: false,
                    all_servers: false,
                    limit: 10,
                },
                mono,
                wall,
            )
            .await
            .unwrap();
        let rows = response.data.unwrap().as_array().unwrap().clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["body"], "really done");
        assert_eq!(rows[0]["close_reason"], "dismissed");
    }
}
