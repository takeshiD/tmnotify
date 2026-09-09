//! Complete application boundary for one per-tmux-server daemon.
//!
//! The caller selects the server and supplies resolved product configuration.
//! This module owns everything needed to serve it: ownership election, live
//! state, the concrete History writer, renderer sessions, reconciliation, and
//! bounded shutdown cleanup.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use thiserror::Error;
use tokio::sync::{Mutex, oneshot};

use super::runtime::{
    OwnedSocket, RuntimeError, RuntimeLimits, RuntimeShutdown, ServerIdentity,
    SessionRendererBroker, serve_with_renderers_and_force_shutdown,
};
use super::{
    DaemonService, JumpExecutor, LiveScheduler, MonotonicTime, SchedulerError, SchedulerLimits,
    ShutdownReason, WindowDisplayPolicy, WindowReconciler,
};
use crate::config::{Config, FeatureMode};
use crate::history::{History, HistoryError};
use crate::notification::{Notification, SourceContext, TmuxServerId};
use crate::platform::{Environment, PathError, PlatformPaths};
use crate::protocol::RendererAction;
use crate::tmux::{
    Backend as _, DisplayPlan, Error as TmuxError, JumpTarget, PaneId, ProductionBackend, Server,
    WindowId,
};

/// Runs the complete daemon application for one selected tmux server.
///
/// Construction and cleanup order are part of this interface. A successful
/// return means graceful cleanup completed; a second SIGINT aborts it through
/// the runtime's immediate force-shutdown path.
pub async fn run(server: Server, paths: PlatformPaths, config: Config) -> Result<(), Error> {
    run_inner(server, paths, config).await.map_err(Error)
}

async fn run_inner(
    server: Server,
    paths: PlatformPaths,
    config: Config,
) -> Result<(), ApplicationFailure> {
    paths.ensure_private_directories()?;
    let environment = Environment::current();
    let settings = DaemonSettings::from_config(&config, &environment)?;
    let identity = ServerIdentity::resolve(server.socket_path())?;
    let daemon_socket = paths.socket_path(identity.server_id())?;
    let domain_id = TmuxServerId::new(identity.server_id())?;
    let executable = std::env::current_exe()?;
    let mut production =
        ProductionBackend::connect(Server::new(identity.tmux_socket()), executable)?;
    production.capabilities()?;

    // Binding is the ownership election. Only the elected process may recover
    // stale History rows or construct the remaining daemon-owned state.
    let owned = OwnedSocket::bind(&daemon_socket)?;
    let history = Arc::new(History::open(
        &paths.history_file,
        domain_id,
        &config.history,
        Utc::now(),
    )?);
    let origin = Instant::now();
    let scheduler = LiveScheduler::new(settings.scheduler_limits, MonotonicTime::default());
    let renderer_sessions = production.renderer_sessions();
    let backend = Arc::new(Mutex::new(production));
    let service = DaemonService::new(
        scheduler,
        Arc::clone(&history),
        ProductionJump {
            backend: Arc::clone(&backend),
        },
    );
    let scheduler = service.scheduler();
    let action_service = service.clone();
    let renderer_broker = Arc::new(SessionRendererBroker::new(
        renderer_sessions,
        move |display_id: String, action: RendererAction| {
            let service = action_service.clone();
            async move {
                service
                    .handle_renderer_action(&display_id, action, monotonic(origin), Utc::now())
                    .await
                    .map_err(|error| error.to_string())
            }
        },
    ));
    let stopping = Arc::new(AtomicBool::new(false));
    let reconcile_task = tokio::spawn(reconcile_loop(
        Arc::clone(&backend),
        Arc::clone(&scheduler),
        Arc::clone(&history),
        Arc::clone(&stopping),
        origin,
        settings.display_policy,
    ));
    let handler_service = service.clone();
    let handler = move |envelope| {
        let service = handler_service.clone();
        async move {
            service
                .handle_envelope(&envelope, monotonic(origin), Utc::now())
                .await
                .and_then(|response| serde_json::to_value(response).map_err(Into::into))
                .map_err(|error| error.to_string())
        }
    };
    let cleanup_scheduler = Arc::clone(&scheduler);
    let cleanup_backend = Arc::clone(&backend);
    let cleanup_history = Arc::clone(&history);
    let cleanup_stopping = Arc::clone(&stopping);
    let (shutdown, force_shutdown) = shutdown_signals(identity.tmux_socket().to_owned());
    let result = serve_with_renderers_and_force_shutdown(
        owned,
        RuntimeLimits::default(),
        handler,
        renderer_broker,
        shutdown,
        force_shutdown,
        move |reason| async move {
            cleanup_stopping.store(true, Ordering::Release);
            let snapshots = {
                let mut scheduler = cleanup_scheduler.lock().await;
                let _ = scheduler.shutdown(shutdown_reason(reason), monotonic(origin), Utc::now());
                scheduler.drain_closed()
            };
            persist_all(&cleanup_history, snapshots).await;
            let _ = cleanup_backend
                .lock()
                .await
                .reconcile(&DisplayPlan::default());
            if let Ok(task) = cleanup_history.flush() {
                let _ = task.wait().await;
            }
        },
    )
    .await;
    stopping.store(true, Ordering::Release);
    reconcile_task.abort();
    result?;
    Ok(())
}

#[derive(Clone)]
struct ProductionJump {
    backend: Arc<Mutex<ProductionBackend>>,
}

impl JumpExecutor for ProductionJump {
    fn jump(
        &self,
        source: SourceContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.backend
                .lock()
                .await
                .jump(&JumpTarget {
                    pane_id: PaneId(source.pane_id().to_owned()),
                    likely_client: None,
                })
                .map_err(|error| error.to_string())
        })
    }

    fn jump_from_attention(
        &self,
        source: SourceContext,
        attention_window: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let mut backend = self.backend.lock().await;
            let topology = backend.topology().map_err(|error| error.to_string())?;
            let likely_client = topology
                .likely_client_for_window(&WindowId(attention_window))
                .map(str::to_owned);
            backend
                .jump(&JumpTarget {
                    pane_id: PaneId(source.pane_id().to_owned()),
                    likely_client,
                })
                .map_err(|error| error.to_string())
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DaemonSettings {
    scheduler_limits: SchedulerLimits,
    display_policy: WindowDisplayPolicy,
}

impl DaemonSettings {
    fn from_config(config: &Config, environment: &Environment) -> Result<Self, SchedulerError> {
        let term_is_dumb = environment.get("TERM").is_some_and(|value| value == "dumb");
        let unicode = config.display.unicode != FeatureMode::Never && !term_is_dumb;
        let color = config.display.color != FeatureMode::Never
            && !term_is_dumb
            && environment.get("NO_COLOR").is_none();
        Ok(Self {
            scheduler_limits: SchedulerLimits::new(
                usize::try_from(config.queue.max_pending).unwrap_or(10_000),
                usize::try_from(config.toast.max_visible).unwrap_or(100),
            )?,
            display_policy: WindowDisplayPolicy {
                placement: config.toast.position,
                toast_width: u16::try_from(config.toast.width).unwrap_or(u16::MAX),
                toast_height: u16::try_from(config.toast.height).unwrap_or(u16::MAX),
                toast_gap: u16::try_from(config.toast.gap).unwrap_or(u16::MAX),
                max_visible_toasts: usize::try_from(config.toast.max_visible).unwrap_or(100),
                stack_order: config.toast.stack_order,
                body: config.toast.body,
                unicode,
                color,
            },
        })
    }
}

async fn reconcile_loop(
    backend: Arc<Mutex<ProductionBackend>>,
    scheduler: Arc<Mutex<LiveScheduler>>,
    history: Arc<History>,
    stopping: Arc<AtomicBool>,
    origin: Instant,
    policy: WindowDisplayPolicy,
) {
    let mut reconciler = WindowReconciler::new(policy, MonotonicTime::default());
    while !stopping.load(Ordering::Acquire) {
        let snapshots = {
            let mut backend = backend.lock().await;
            let mut scheduler = scheduler.lock().await;
            let _ = reconciler.tick(&mut *backend, &mut scheduler, monotonic(origin), Utc::now());
            scheduler.drain_closed()
        };
        enqueue_all(&history, snapshots);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn enqueue_all(history: &History, snapshots: Vec<Notification>) {
    for notification in snapshots {
        // The worker owns SQLite retries. Dropping the receipt here keeps the
        // display loop independent from database latency while retaining the
        // bounded History channel as backpressure.
        let _ = history.persist(&notification);
    }
}

async fn persist_all(history: &History, snapshots: Vec<Notification>) {
    for notification in snapshots {
        if let Ok(task) = history.persist(&notification) {
            let _ = task.wait().await;
        }
    }
}

fn monotonic(origin: Instant) -> MonotonicTime {
    MonotonicTime::from_duration(origin.elapsed())
}

fn shutdown_reason(reason: RuntimeShutdown) -> ShutdownReason {
    match reason {
        RuntimeShutdown::Signal => ShutdownReason::Stopped,
        RuntimeShutdown::ServerEnded => ShutdownReason::ServerEnded,
    }
}

fn shutdown_signals(
    tmux_socket: PathBuf,
) -> (
    impl Future<Output = RuntimeShutdown>,
    impl Future<Output = ()>,
) {
    let (graceful_tx, graceful_rx) = oneshot::channel();
    let (forced_tx, forced_rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut graceful_tx = Some(graceful_tx);
        let mut forced_tx = Some(forced_tx);
        let mut interrupt_count = 0_u8;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler installation failed");
        loop {
            tokio::select! {
                interrupt = tokio::signal::ctrl_c() => {
                    if interrupt.is_err() {
                        break;
                    }
                    interrupt_count = interrupt_count.saturating_add(1);
                    if interrupt_count == 1 {
                        if let Some(sender) = graceful_tx.take() {
                            let _ = sender.send(RuntimeShutdown::Signal);
                        }
                    } else {
                        if let Some(sender) = forced_tx.take() {
                            let _ = sender.send(());
                        }
                        break;
                    }
                }
                signal = terminate.recv() => {
                    if signal.is_none() {
                        break;
                    }
                    if let Some(sender) = graceful_tx.take() {
                        let _ = sender.send(RuntimeShutdown::Signal);
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(500)), if graceful_tx.is_some() => {
                    if std::fs::symlink_metadata(&tmux_socket).is_err()
                        && let Some(sender) = graceful_tx.take()
                    {
                        let _ = sender.send(RuntimeShutdown::ServerEnded);
                    }
                }
            }
        }
    });
    (
        async move { graceful_rx.await.unwrap_or(RuntimeShutdown::Signal) },
        async move {
            if forced_rx.await.is_err() {
                std::future::pending::<()>().await;
            }
        },
    )
}

/// Opaque daemon failure. Scheduler, IPC, tmux protocol, renderer, and storage
/// details remain implementation concerns while their diagnostics are kept.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ApplicationFailure);

#[derive(Debug, Error)]
enum ApplicationFailure {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Ingress(#[from] crate::notification::IngressError),
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::config::{BodyPresentation, Placement, StackOrder};

    #[test]
    fn application_seam_translates_product_configuration_into_owned_policy() {
        let mut config = Config::default();
        config.queue.max_pending = 37;
        config.toast.max_visible = 6;
        config.toast.width = 55;
        config.toast.height = 4;
        config.toast.gap = 2;
        config.toast.position = Placement::BottomLeft;
        config.toast.stack_order = StackOrder::NewestFirst;
        config.toast.body = BodyPresentation::Wrap;
        let environment = Environment::from_pairs(std::iter::empty::<(OsString, OsString)>());

        let settings = DaemonSettings::from_config(&config, &environment).unwrap();

        assert_eq!(settings.scheduler_limits.max_pending, 37);
        assert_eq!(settings.scheduler_limits.max_visible_toasts, 6);
        assert_eq!(settings.display_policy.placement, Placement::BottomLeft);
        assert_eq!(settings.display_policy.toast_width, 55);
        assert_eq!(settings.display_policy.toast_height, 4);
        assert_eq!(settings.display_policy.toast_gap, 2);
        assert_eq!(settings.display_policy.max_visible_toasts, 6);
        assert_eq!(settings.display_policy.stack_order, StackOrder::NewestFirst);
        assert_eq!(settings.display_policy.body, BodyPresentation::Wrap);
        assert!(settings.display_policy.unicode);
        assert!(settings.display_policy.color);
    }

    #[test]
    fn application_seam_applies_terminal_capabilities_and_shutdown_lifecycle() {
        let config = Config::default();
        let environment = Environment::from_pairs([
            (OsString::from("TERM"), OsString::from("dumb")),
            (OsString::from("NO_COLOR"), OsString::from("1")),
        ]);

        let settings = DaemonSettings::from_config(&config, &environment).unwrap();

        assert!(!settings.display_policy.unicode);
        assert!(!settings.display_policy.color);
        assert_eq!(
            shutdown_reason(RuntimeShutdown::Signal),
            ShutdownReason::Stopped
        );
        assert_eq!(
            shutdown_reason(RuntimeShutdown::ServerEnded),
            ShutdownReason::ServerEnded
        );
    }
}
