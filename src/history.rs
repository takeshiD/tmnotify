//! Durable, shared Notification History.
//!
//! A [`History`] handle is tied to one tmux server and one dedicated blocking
//! worker. Submitting work never waits for SQLite: the bounded queue either
//! accepts it immediately or reports overload. Callers may asynchronously wait
//! for the result when acknowledgement semantics require it.

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::oneshot;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::HistoryConfig;
use crate::notification::{
    AgentEventKind, CloseReason, DeliveryState, Level, NormalizedMetadata, Notification,
    NotificationId, NotificationKey, Presentation, Priority, Provider, SourceContext, Timeout,
    TmuxServerId,
};
use crate::platform::{PathError, ensure_private_directory};

const SCHEMA_VERSION: i64 = 1;
const DEFAULT_QUEUE_CAPACITY: usize = 128;
const BUSY_TIMEOUT: Duration = Duration::from_millis(20);
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(10);
const BUSY_ATTEMPTS: usize = 5;

const MIGRATION_1: &str = r#"
CREATE TABLE notifications (
    id TEXT PRIMARY KEY,
    notification_key TEXT,
    tmux_server_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    level TEXT NOT NULL,
    priority TEXT NOT NULL,
    presentation TEXT NOT NULL,
    delivery_state TEXT NOT NULL,
    close_reason TEXT,
    timeout_ms INTEGER,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    provider TEXT,
    provider_session_id TEXT,
    session_id TEXT,
    window_id TEXT,
    pane_id TEXT,
    cwd TEXT,
    command TEXT,
    pane_title TEXT,
    hidden_at INTEGER,
    last_jumped_at INTEGER,
    normalized_metadata_json TEXT NOT NULL
);
CREATE INDEX notifications_updated_at_idx
    ON notifications(updated_at DESC, id DESC);
CREATE INDEX notifications_server_updated_at_idx
    ON notifications(tmux_server_id, updated_at DESC, id DESC);
CREATE INDEX notifications_pane_id_idx ON notifications(pane_id);
CREATE INDEX notifications_live_key_idx
    ON notifications(tmux_server_id, notification_key)
    WHERE notification_key IS NOT NULL AND delivery_state != 'closed';
PRAGMA user_version = 1;
"#;

/// Result of a successful persistence request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceStatus {
    Persisted,
    Disabled,
}

/// A queued result. Waiting is async; submitting it was nonblocking.
pub struct HistoryTask<T> {
    receiver: oneshot::Receiver<Result<T, HistoryError>>,
}

impl<T> HistoryTask<T> {
    pub async fn wait(self) -> Result<T, HistoryError> {
        self.receiver
            .await
            .map_err(|_| HistoryError::WorkerStopped)?
    }

    /// Waits from a dedicated blocking boundary such as the History UI action
    /// worker. This must not be called by the daemon scheduler or a renderer.
    pub(crate) fn blocking_wait(self) -> Result<T, HistoryError> {
        self.receiver
            .blocking_recv()
            .map_err(|_| HistoryError::WorkerStopped)?
    }
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("History path has no parent directory: {0}")]
    InvalidPath(PathBuf),
    #[error("History worker queue is full")]
    QueueFull,
    #[error("History worker has stopped")]
    WorkerStopped,
    #[error("History schema version {found} is newer than supported version {supported}")]
    NewerSchema { found: i64, supported: i64 },
    #[error("Notification Source Pane belongs to a different tmux server")]
    ServerMismatch,
    #[error("invalid persisted History value in {field}: {value}")]
    InvalidValue { field: &'static str, value: String },
    #[error("SQLite History operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("History filesystem operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("History metadata is invalid: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("persisted History text failed normalization: {0}")]
    Ingress(#[from] crate::notification::IngressError),
    #[error(transparent)]
    PlatformPath(#[from] PathError),
}

/// Query defaults deliberately match the product's normal History view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryQuery {
    pub include_hidden: bool,
    pub all_servers: bool,
    pub limit: u32,
}

impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            include_hidden: false,
            all_servers: false,
            limit: 10_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HistoryEntry {
    pub id: NotificationId,
    pub key: Option<NotificationKey>,
    pub tmux_server_id: TmuxServerId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub level: Level,
    pub priority: Priority,
    pub presentation: Presentation,
    pub delivery: DeliveryState,
    pub close_reason: Option<CloseReason>,
    pub timeout: Timeout,
    pub title: String,
    pub body: String,
    pub source: Option<SourceContext>,
    pub hidden_at: Option<DateTime<Utc>>,
    pub last_jumped_at: Option<DateTime<Utc>>,
    pub metadata: NormalizedMetadata,
}

/// A Clear operation cannot exist without one destructive selector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClearFilter {
    Hidden,
    Before(DateTime<Utc>),
    All,
}

/// Concrete SQLite History module for one daemon/current-server context.
pub struct History {
    enabled: bool,
    sender: Option<SyncSender<Command>>,
}

impl History {
    /// Opens the shared database, migrates it, recovers this server's stale
    /// live rows, then transfers the connection to a dedicated worker.
    pub fn open(
        path: impl AsRef<Path>,
        current_server: TmuxServerId,
        config: &HistoryConfig,
        now: DateTime<Utc>,
    ) -> Result<Self, HistoryError> {
        Self::open_with_capacity(path, current_server, config, now, DEFAULT_QUEUE_CAPACITY)
    }

    fn open_with_capacity(
        path: impl AsRef<Path>,
        current_server: TmuxServerId,
        config: &HistoryConfig,
        now: DateTime<Utc>,
        queue_capacity: usize,
    ) -> Result<Self, HistoryError> {
        if !config.enabled {
            return Ok(Self {
                enabled: false,
                sender: None,
            });
        }

        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| HistoryError::InvalidPath(path.to_owned()))?;
        ensure_private_directory(parent)?;
        let mut connection = open_connection(path)?;
        migrate(&mut connection)?;
        recover_stale(&connection, &current_server, now)?;
        enforce_retention(&connection, config.max_entries)?;

        let (sender, receiver) = mpsc::sync_channel(queue_capacity.max(1));
        let server = current_server;
        let max_entries = config.max_entries;
        thread::Builder::new()
            .name("tmnotify-history".to_owned())
            .spawn(move || worker_loop(connection, receiver, server, max_entries))
            .map_err(|source| HistoryError::Io {
                path: path.to_owned(),
                source,
            })?;

        Ok(Self {
            enabled: true,
            sender: Some(sender),
        })
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Inserts a row at acceptance or updates the same Notification snapshot.
    pub fn persist(
        &self,
        notification: &Notification,
    ) -> Result<HistoryTask<PersistenceStatus>, HistoryError> {
        if !self.enabled {
            return Ok(ready(Ok(PersistenceStatus::Disabled)));
        }
        self.submit(|reply| Command::Persist(Box::new(notification.clone()), reply))
    }

    pub fn hide(
        &self,
        id: NotificationId,
        hidden_at: DateTime<Utc>,
    ) -> Result<HistoryTask<PersistenceStatus>, HistoryError> {
        if !self.enabled {
            return Ok(ready(Ok(PersistenceStatus::Disabled)));
        }
        self.submit(|reply| Command::Hide(id, Some(hidden_at), reply))
    }

    pub fn unhide(
        &self,
        id: NotificationId,
    ) -> Result<HistoryTask<PersistenceStatus>, HistoryError> {
        if !self.enabled {
            return Ok(ready(Ok(PersistenceStatus::Disabled)));
        }
        self.submit(|reply| Command::Hide(id, None, reply))
    }

    /// Records a History jump without changing lifecycle or update ordering.
    pub fn mark_jumped(
        &self,
        id: NotificationId,
        jumped_at: DateTime<Utc>,
    ) -> Result<HistoryTask<PersistenceStatus>, HistoryError> {
        if !self.enabled {
            return Ok(ready(Ok(PersistenceStatus::Disabled)));
        }
        self.submit(|reply| Command::MarkJumped(id, jumped_at, reply))
    }

    pub fn list(
        &self,
        query: HistoryQuery,
    ) -> Result<HistoryTask<Vec<HistoryEntry>>, HistoryError> {
        if !self.enabled {
            return Ok(ready(Ok(Vec::new())));
        }
        self.submit(|reply| Command::List(query, reply))
    }

    /// Physically removes matching rows in one transaction. Confirmation is a
    /// CLI concern and must happen before this method is called.
    pub fn clear(
        &self,
        filter: ClearFilter,
        all_servers: bool,
    ) -> Result<HistoryTask<u64>, HistoryError> {
        if !self.enabled {
            return Ok(ready(Ok(0)));
        }
        self.submit(|reply| Command::Clear(filter, all_servers, reply))
    }

    /// Acts as the shutdown durability barrier. The worker processes commands
    /// serially, so completion means all earlier writes have reached SQLite.
    pub fn flush(&self) -> Result<HistoryTask<()>, HistoryError> {
        if !self.enabled {
            return Ok(ready(Ok(())));
        }
        self.submit(Command::Flush)
    }

    fn submit<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, HistoryError>>) -> Command,
    ) -> Result<HistoryTask<T>, HistoryError> {
        let (sender, receiver) = oneshot::channel();
        let command = command(sender);
        match self
            .sender
            .as_ref()
            .ok_or(HistoryError::WorkerStopped)?
            .try_send(command)
        {
            Ok(()) => Ok(HistoryTask { receiver }),
            Err(TrySendError::Full(_)) => Err(HistoryError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(HistoryError::WorkerStopped),
        }
    }
}

fn ready<T>(result: Result<T, HistoryError>) -> HistoryTask<T> {
    let (sender, receiver) = oneshot::channel();
    let _ = sender.send(result);
    HistoryTask { receiver }
}

enum Command {
    Persist(
        Box<Notification>,
        oneshot::Sender<Result<PersistenceStatus, HistoryError>>,
    ),
    Hide(
        NotificationId,
        Option<DateTime<Utc>>,
        oneshot::Sender<Result<PersistenceStatus, HistoryError>>,
    ),
    MarkJumped(
        NotificationId,
        DateTime<Utc>,
        oneshot::Sender<Result<PersistenceStatus, HistoryError>>,
    ),
    List(
        HistoryQuery,
        oneshot::Sender<Result<Vec<HistoryEntry>, HistoryError>>,
    ),
    Clear(
        ClearFilter,
        bool,
        oneshot::Sender<Result<u64, HistoryError>>,
    ),
    Flush(oneshot::Sender<Result<(), HistoryError>>),
}

fn worker_loop(
    mut connection: Connection,
    receiver: mpsc::Receiver<Command>,
    current_server: TmuxServerId,
    max_entries: u32,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Persist(notification, reply) => {
                let result = persist(&connection, &current_server, &notification, max_entries)
                    .map(|()| PersistenceStatus::Persisted);
                let _ = reply.send(result);
            }
            Command::Hide(id, timestamp, reply) => {
                let result =
                    set_hidden(&connection, id, timestamp).map(|()| PersistenceStatus::Persisted);
                let _ = reply.send(result);
            }
            Command::MarkJumped(id, timestamp, reply) => {
                let result =
                    mark_jumped(&connection, id, timestamp).map(|()| PersistenceStatus::Persisted);
                let _ = reply.send(result);
            }
            Command::List(query, reply) => {
                let _ = reply.send(list_entries(&connection, &current_server, query));
            }
            Command::Clear(filter, all_servers, reply) => {
                let _ = reply.send(clear_entries(
                    &mut connection,
                    &current_server,
                    filter,
                    all_servers,
                ));
            }
            Command::Flush(reply) => {
                let _ = reply.send(Ok(()));
            }
        }
    }
}

fn clear_entries(
    connection: &mut Connection,
    current_server: &TmuxServerId,
    filter: ClearFilter,
    all_servers: bool,
) -> Result<u64, HistoryError> {
    with_busy_retry(|| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = match (filter, all_servers) {
            (ClearFilter::Hidden, true) => {
                transaction.execute("DELETE FROM notifications WHERE hidden_at IS NOT NULL", [])?
            }
            (ClearFilter::Hidden, false) => transaction.execute(
                "DELETE FROM notifications WHERE hidden_at IS NOT NULL AND tmux_server_id = ?1",
                [current_server.as_str()],
            )?,
            (ClearFilter::Before(before), true) => transaction.execute(
                "DELETE FROM notifications WHERE updated_at < ?1",
                [before.timestamp_millis()],
            )?,
            (ClearFilter::Before(before), false) => transaction.execute(
                "DELETE FROM notifications WHERE updated_at < ?1 AND tmux_server_id = ?2",
                params![before.timestamp_millis(), current_server.as_str()],
            )?,
            (ClearFilter::All, true) => transaction.execute("DELETE FROM notifications", [])?,
            (ClearFilter::All, false) => transaction.execute(
                "DELETE FROM notifications WHERE tmux_server_id = ?1",
                [current_server.as_str()],
            )?,
        };
        transaction.commit()?;
        Ok(u64::try_from(deleted).unwrap_or(u64::MAX))
    })
}

/// Writes one untruncated JSON object per line and never adds terminal styling.
pub fn write_ndjson(
    entries: &[HistoryEntry],
    mut output: impl Write,
) -> Result<(), HistoryOutputError> {
    for entry in entries {
        serde_json::to_writer(&mut output, entry)?;
        output.write_all(b"\n")?;
    }
    Ok(())
}

/// Writes one borderless, terminal-cell-truncated row per Notification.
pub fn write_plain(
    entries: &[HistoryEntry],
    width: usize,
    mut output: impl Write,
) -> Result<(), io::Error> {
    for entry in entries {
        let body = entry
            .body
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        let title_separator = if entry.title.is_empty() { "" } else { " — " };
        let row = format!(
            "{} {:<7} {}{}{}",
            entry.updated_at.to_rfc3339(),
            history_level(entry.level),
            entry.title,
            title_separator,
            body
        );
        writeln!(output, "{}", truncate_cells(&row, width))?;
    }
    Ok(())
}

fn history_level(level: Level) -> &'static str {
    match level {
        Level::Info => "info",
        Level::Success => "success",
        Level::Warning => "warning",
        Level::Error => "error",
    }
}

fn truncate_cells(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let content_width = width - 1;
    let mut used = 0;
    let mut output = String::new();
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > content_width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

#[derive(Debug, Error)]
pub enum HistoryOutputError {
    #[error("failed to encode History NDJSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to write History output: {0}")]
    Io(#[from] io::Error),
}

fn open_connection(path: &Path) -> Result<Connection, HistoryError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| HistoryError::Io {
                path: path.to_owned(),
                source,
            },
        )?;
    }
    connection.busy_timeout(BUSY_TIMEOUT)?;
    let journal_mode: String = with_busy_retry(|| {
        connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
    })?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(HistoryError::InvalidValue {
            field: "journal_mode",
            value: journal_mode,
        });
    }
    with_busy_retry(|| connection.pragma_update(None, "synchronous", "NORMAL"))?;
    Ok(connection)
}

fn migrate(connection: &mut Connection) -> Result<(), HistoryError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(HistoryError::NewerSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == 0 {
        with_busy_retry(|| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Another daemon may have migrated while this connection waited
            // for the write lock. Re-read under the lock before applying SQL.
            let locked_version: i64 =
                transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if locked_version == 0 {
                transaction.execute_batch(MIGRATION_1)?;
            } else if locked_version > SCHEMA_VERSION {
                return Err(rusqlite::Error::InvalidQuery);
            }
            transaction.commit()
        })?;
    }
    Ok(())
}

fn recover_stale(
    connection: &Connection,
    server: &TmuxServerId,
    now: DateTime<Utc>,
) -> Result<(), HistoryError> {
    with_busy_retry(|| {
        connection.execute(
            "UPDATE notifications
             SET delivery_state = 'closed', close_reason = 'daemon_interrupted', updated_at = ?1
             WHERE tmux_server_id = ?2 AND delivery_state IN ('pending', 'visible')",
            params![now.timestamp_millis(), server.as_str()],
        )?;
        Ok(())
    })
}

fn persist(
    connection: &Connection,
    server: &TmuxServerId,
    notification: &Notification,
    max_entries: u32,
) -> Result<(), HistoryError> {
    if notification
        .source()
        .is_some_and(|source| source.tmux_server_id() != server)
    {
        return Err(HistoryError::ServerMismatch);
    }
    with_busy_retry(|| {
        let transaction = connection.unchecked_transaction()?;
        persist_in(&transaction, server, notification)?;
        enforce_retention_in(&transaction, max_entries)?;
        transaction.commit()
    })
}

fn persist_in(
    connection: &Connection,
    server: &TmuxServerId,
    notification: &Notification,
) -> rusqlite::Result<()> {
    let source = notification.source();
    let timeout_ms = match notification.timeout() {
        Timeout::After(duration) => Some(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)),
        Timeout::Never => None,
    };
    let metadata = serde_json::to_string(notification.metadata())
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    connection.execute(
        "INSERT INTO notifications (
            id, notification_key, tmux_server_id, created_at, updated_at, level, priority,
            presentation, delivery_state, close_reason, timeout_ms, title, body, provider,
            provider_session_id, session_id, window_id, pane_id, cwd, command, pane_title,
            hidden_at, last_jumped_at, normalized_metadata_json
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
         ) ON CONFLICT(id) DO UPDATE SET
            notification_key=excluded.notification_key, updated_at=excluded.updated_at,
            level=excluded.level, priority=excluded.priority, presentation=excluded.presentation,
            delivery_state=excluded.delivery_state, close_reason=excluded.close_reason,
            timeout_ms=excluded.timeout_ms, title=excluded.title, body=excluded.body,
            provider=excluded.provider, provider_session_id=excluded.provider_session_id,
            session_id=excluded.session_id, window_id=excluded.window_id, pane_id=excluded.pane_id,
            cwd=excluded.cwd, command=excluded.command, pane_title=excluded.pane_title,
            hidden_at=COALESCE(excluded.hidden_at, notifications.hidden_at),
            last_jumped_at=COALESCE(excluded.last_jumped_at, notifications.last_jumped_at),
            normalized_metadata_json=excluded.normalized_metadata_json",
        params![
            notification.id().to_string(),
            notification.key().map(NotificationKey::as_str),
            server.as_str(),
            notification.created_at().timestamp_millis(),
            notification.updated_at().timestamp_millis(),
            enum_name(notification.level()),
            enum_name(notification.priority()),
            enum_name(notification.presentation()),
            enum_name(notification.delivery()),
            notification.close_reason().map(enum_name),
            timeout_ms,
            notification.title(),
            notification.body(),
            source.and_then(|source| source.provider()).map(enum_name),
            source.and_then(SourceContext::provider_session_id),
            source.map(SourceContext::session_id),
            source.map(SourceContext::window_id),
            source.map(SourceContext::pane_id),
            source
                .and_then(SourceContext::cwd)
                .map(|path| path.to_string_lossy()),
            source.and_then(SourceContext::command),
            source.and_then(SourceContext::pane_title),
            notification.hidden_at().map(|time| time.timestamp_millis()),
            notification
                .last_jumped_at()
                .map(|time| time.timestamp_millis()),
            metadata,
        ],
    )?;
    Ok(())
}

fn enforce_retention(connection: &Connection, max_entries: u32) -> Result<(), HistoryError> {
    with_busy_retry(|| enforce_retention_in(connection, max_entries))
}

fn enforce_retention_in(connection: &Connection, max_entries: u32) -> rusqlite::Result<()> {
    connection.execute(
        "DELETE FROM notifications WHERE id IN (
                SELECT id FROM notifications
                ORDER BY (hidden_at IS NULL) ASC, created_at ASC, id ASC
                LIMIT MAX((SELECT COUNT(*) FROM notifications) - ?1, 0)
             )",
        [max_entries],
    )?;
    Ok(())
}

fn set_hidden(
    connection: &Connection,
    id: NotificationId,
    hidden_at: Option<DateTime<Utc>>,
) -> Result<(), HistoryError> {
    with_busy_retry(|| {
        connection.execute(
            "UPDATE notifications SET hidden_at = ?1 WHERE id = ?2",
            params![
                hidden_at.map(|time| time.timestamp_millis()),
                id.to_string()
            ],
        )?;
        Ok(())
    })
}

fn mark_jumped(
    connection: &Connection,
    id: NotificationId,
    jumped_at: DateTime<Utc>,
) -> Result<(), HistoryError> {
    with_busy_retry(|| {
        connection.execute(
            "UPDATE notifications SET last_jumped_at = ?1 WHERE id = ?2",
            params![jumped_at.timestamp_millis(), id.to_string()],
        )?;
        Ok(())
    })
}

fn list_entries(
    connection: &Connection,
    current_server: &TmuxServerId,
    query: HistoryQuery,
) -> Result<Vec<HistoryEntry>, HistoryError> {
    with_busy_retry(|| list_entries_in(connection, current_server, query))
}

fn list_entries_in(
    connection: &Connection,
    current_server: &TmuxServerId,
    query: HistoryQuery,
) -> rusqlite::Result<Vec<HistoryEntry>> {
    let mut sql = String::from(
        "SELECT id, notification_key, tmux_server_id, created_at, updated_at,
        level, priority, presentation, delivery_state, close_reason, timeout_ms, title, body,
        provider, provider_session_id, session_id, window_id, pane_id, cwd, command, pane_title,
        hidden_at, last_jumped_at, normalized_metadata_json FROM notifications WHERE 1=1",
    );
    if !query.include_hidden {
        sql.push_str(" AND hidden_at IS NULL");
    }
    if !query.all_servers {
        sql.push_str(" AND tmux_server_id = ?1");
    }
    sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT ?2");

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        params![
            if query.all_servers {
                None
            } else {
                Some(current_server.as_str())
            },
            query.limit.min(100_000),
        ],
        read_entry,
    )?;
    rows.collect::<Result<Vec<_>, _>>()
}

fn read_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    let text = |index| row.get::<_, String>(index);
    let optional_text = |index| row.get::<_, Option<String>>(index);
    let id_text = text(0)?;
    let server_text = text(2)?;
    let provider = optional_text(13)?
        .map(|value| parse_enum("provider", &value))
        .transpose()?;
    let session_id = optional_text(15)?;
    let window_id = optional_text(16)?;
    let pane_id = optional_text(17)?;
    let source = match (session_id, window_id, pane_id) {
        (Some(session), Some(window), Some(pane)) => {
            let mut source = SourceContext::new(
                TmuxServerId::new(&server_text).map_err(sql_conversion)?,
                &session,
                &window,
                &pane,
            )
            .map_err(sql_conversion)?;
            if let (Some(provider), Some(provider_session)) = (provider, optional_text(14)?) {
                source = source
                    .with_provider_session(provider, &provider_session)
                    .map_err(sql_conversion)?;
            }
            if let Some(cwd) = optional_text(18)? {
                source = source.with_cwd(&cwd).map_err(sql_conversion)?;
            }
            if let Some(command) = optional_text(19)? {
                source = source.with_command(&command).map_err(sql_conversion)?;
            }
            if let Some(title) = optional_text(20)? {
                source = source.with_pane_title(&title).map_err(sql_conversion)?;
            }
            Some(source)
        }
        (None, None, None) => None,
        _ => {
            return Err(sql_conversion(HistoryError::InvalidValue {
                field: "source_context",
                value: "partially populated".to_owned(),
            }));
        }
    };
    let metadata_json = text(23)?;
    let metadata_row: MetadataRow = serde_json::from_str(&metadata_json)
        .map_err(|error| sql_conversion(HistoryError::Metadata(error)))?;
    let mut metadata = NormalizedMetadata::new(metadata_row.agent_event_kind);
    if let Some(name) = metadata_row.agent_name {
        metadata = metadata.with_agent_name(&name).map_err(sql_conversion)?;
    }
    if let Some(name) = metadata_row.tool_name {
        metadata = metadata.with_tool_name(&name).map_err(sql_conversion)?;
    }
    let timeout_ms: Option<i64> = row.get(10)?;
    let timeout = match timeout_ms {
        Some(value) if value >= 0 => Timeout::After(Duration::from_millis(value as u64)),
        Some(value) => {
            return Err(sql_conversion(HistoryError::InvalidValue {
                field: "timeout_ms",
                value: value.to_string(),
            }));
        }
        None => Timeout::Never,
    };
    Ok(HistoryEntry {
        id: NotificationId::from_str(&id_text).map_err(sql_conversion)?,
        key: optional_text(1)?
            .map(|key| NotificationKey::new(&key))
            .transpose()
            .map_err(sql_conversion)?,
        tmux_server_id: TmuxServerId::new(&server_text).map_err(sql_conversion)?,
        created_at: timestamp(row.get(3)?, "created_at")?,
        updated_at: timestamp(row.get(4)?, "updated_at")?,
        level: parse_enum("level", &text(5)?)?,
        priority: parse_enum("priority", &text(6)?)?,
        presentation: parse_enum("presentation", &text(7)?)?,
        delivery: parse_enum("delivery_state", &text(8)?)?,
        close_reason: optional_text(9)?
            .map(|value| parse_enum("close_reason", &value))
            .transpose()?,
        timeout,
        title: text(11)?,
        body: text(12)?,
        source,
        hidden_at: row
            .get::<_, Option<i64>>(21)?
            .map(|value| timestamp(value, "hidden_at"))
            .transpose()?,
        last_jumped_at: row
            .get::<_, Option<i64>>(22)?
            .map(|value| timestamp(value, "last_jumped_at"))
            .transpose()?,
        metadata,
    })
}

#[derive(Deserialize)]
struct MetadataRow {
    agent_event_kind: Option<AgentEventKind>,
    agent_name: Option<String>,
    tool_name: Option<String>,
}

fn timestamp(value: i64, field: &'static str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(value).ok_or_else(|| {
        sql_conversion(HistoryError::InvalidValue {
            field,
            value: value.to_string(),
        })
    })
}

fn parse_enum<T>(field: &'static str, value: &str) -> rusqlite::Result<T>
where
    T: FromHistoryName,
{
    T::from_history_name(value).ok_or_else(|| {
        sql_conversion(HistoryError::InvalidValue {
            field,
            value: value.to_owned(),
        })
    })
}

fn sql_conversion(error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn with_busy_retry<T>(
    mut operation: impl FnMut() -> rusqlite::Result<T>,
) -> Result<T, HistoryError> {
    for attempt in 0..BUSY_ATTEMPTS {
        match operation() {
            Err(error) if is_busy(&error) && attempt + 1 < BUSY_ATTEMPTS => {
                thread::sleep(BUSY_RETRY_DELAY);
            }
            result => return result.map_err(HistoryError::from),
        }
    }
    unreachable!("bounded retry loop always returns")
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(inner.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn enum_name(value: impl HistoryName) -> &'static str {
    value.history_name()
}

trait HistoryName: Copy {
    fn history_name(self) -> &'static str;
}

trait FromHistoryName: Sized {
    fn from_history_name(value: &str) -> Option<Self>;
}

macro_rules! history_names {
    ($type:ty { $($variant:ident => $name:literal),+ $(,)? }) => {
        impl HistoryName for $type {
            fn history_name(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }
        }
        impl FromHistoryName for $type {
            fn from_history_name(value: &str) -> Option<Self> {
                match value { $($name => Some(Self::$variant)),+, _ => None }
            }
        }
    };
}

history_names!(Level { Info => "info", Success => "success", Warning => "warning", Error => "error" });
history_names!(Priority { Low => "low", Normal => "normal", High => "high", Critical => "critical" });
history_names!(Presentation { Toast => "toast", Attention => "attention" });
history_names!(DeliveryState { Pending => "pending", Visible => "visible", Closed => "closed" });
history_names!(CloseReason {
    TimedOut => "timed_out", Dismissed => "dismissed", Jumped => "jumped",
    RenderSuppressed => "render_suppressed", RenderFailed => "render_failed",
    DaemonInterrupted => "daemon_interrupted", DaemonStopped => "daemon_stopped",
    ServerEnded => "server_ended"
});
history_names!(Provider { Claude => "claude", Codex => "codex" });

impl fmt::Debug for History {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("History")
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::{NotificationDraft, Presentation};
    use tempfile::TempDir;

    fn server(value: &str) -> TmuxServerId {
        TmuxServerId::new(value).unwrap()
    }

    fn notification(server_id: &str, body: &str, at: DateTime<Utc>) -> Notification {
        let source = SourceContext::new(server(server_id), "$1", "@2", "%3").unwrap();
        Notification::from_draft(
            NotificationDraft::new(Presentation::Toast, "build", body, Some(source)).unwrap(),
            at,
        )
    }

    fn config(max_entries: u32) -> HistoryConfig {
        HistoryConfig {
            enabled: true,
            max_entries,
        }
    }

    fn history_path(temporary: &TempDir) -> PathBuf {
        temporary.path().join("state/history.sqlite3")
    }

    #[tokio::test]
    async fn migrates_to_wal_and_round_trips_normalized_notifications() {
        let temporary = TempDir::new().unwrap();
        let path = history_path(&temporary);
        let now = DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
        let history = History::open(&path, server("alpha"), &config(10_000), now).unwrap();
        let notification = notification("alpha", "done", now);

        assert_eq!(
            history
                .persist(&notification)
                .unwrap()
                .wait()
                .await
                .unwrap(),
            PersistenceStatus::Persisted
        );
        history.flush().unwrap().wait().await.unwrap();
        let entries = history
            .list(HistoryQuery::default())
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].body, "done");

        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name IN (
                    'notifications_updated_at_idx',
                    'notifications_server_updated_at_idx',
                    'notifications_pane_id_idx',
                    'notifications_live_key_idx'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(mode, "wal");
        assert_eq!(index_count, 4);
    }

    #[tokio::test]
    async fn default_scope_and_sort_are_current_server_updated_descending() {
        let temporary = TempDir::new().unwrap();
        let path = history_path(&temporary);
        let first = DateTime::from_timestamp_millis(1_000).unwrap();
        let second = DateTime::from_timestamp_millis(2_000).unwrap();
        let alpha = History::open(&path, server("alpha"), &config(100), first).unwrap();
        let beta = History::open(&path, server("beta"), &config(100), first).unwrap();
        alpha
            .persist(&notification("alpha", "old", first))
            .unwrap()
            .wait()
            .await
            .unwrap();
        beta.persist(&notification("beta", "other", second))
            .unwrap()
            .wait()
            .await
            .unwrap();
        alpha
            .persist(&notification("alpha", "new", second))
            .unwrap()
            .wait()
            .await
            .unwrap();

        let scoped = alpha
            .list(HistoryQuery::default())
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(
            scoped
                .iter()
                .map(|entry| entry.body.as_str())
                .collect::<Vec<_>>(),
            ["new", "old"]
        );
        let all = alpha
            .list(HistoryQuery {
                all_servers: true,
                ..HistoryQuery::default()
            })
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn hide_is_reversible_and_history_jump_preserves_lifecycle_ordering() {
        let temporary = TempDir::new().unwrap();
        let path = history_path(&temporary);
        let now = DateTime::from_timestamp_millis(1_000).unwrap();
        let history = History::open(&path, server("alpha"), &config(100), now).unwrap();
        let item = notification("alpha", "one", now);
        history.persist(&item).unwrap().wait().await.unwrap();
        history.hide(item.id(), now).unwrap().wait().await.unwrap();
        assert!(
            history
                .list(HistoryQuery::default())
                .unwrap()
                .wait()
                .await
                .unwrap()
                .is_empty()
        );
        history.unhide(item.id()).unwrap().wait().await.unwrap();
        let jump_time = DateTime::from_timestamp_millis(9_000).unwrap();
        history
            .mark_jumped(item.id(), jump_time)
            .unwrap()
            .wait()
            .await
            .unwrap();
        let entry = history
            .list(HistoryQuery::default())
            .unwrap()
            .wait()
            .await
            .unwrap()
            .remove(0);
        assert_eq!(entry.delivery, DeliveryState::Pending);
        assert_eq!(entry.close_reason, None);
        assert_eq!(entry.updated_at, now);
        assert_eq!(entry.last_jumped_at, Some(jump_time));
    }

    #[tokio::test]
    async fn retention_removes_oldest_hidden_before_any_visible_row() {
        let temporary = TempDir::new().unwrap();
        let path = history_path(&temporary);
        let history = History::open(&path, server("alpha"), &config(2), Utc::now()).unwrap();
        let other_server = History::open(&path, server("beta"), &config(2), Utc::now()).unwrap();
        let old_visible = notification(
            "alpha",
            "old-visible",
            DateTime::from_timestamp_millis(1_000).unwrap(),
        );
        let newer_hidden = notification(
            "alpha",
            "newer-hidden",
            DateTime::from_timestamp_millis(2_000).unwrap(),
        );
        history.persist(&old_visible).unwrap().wait().await.unwrap();
        history
            .persist(&newer_hidden)
            .unwrap()
            .wait()
            .await
            .unwrap();
        history
            .hide(newer_hidden.id(), Utc::now())
            .unwrap()
            .wait()
            .await
            .unwrap();
        other_server
            .persist(&notification(
                "beta",
                "newest",
                DateTime::from_timestamp_millis(3_000).unwrap(),
            ))
            .unwrap()
            .wait()
            .await
            .unwrap();
        let entries = history
            .list(HistoryQuery {
                include_hidden: true,
                all_servers: true,
                ..HistoryQuery::default()
            })
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.body.as_str())
                .collect::<Vec<_>>(),
            ["newest", "old-visible"]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn independent_workers_write_the_shared_wal_database() {
        let temporary = TempDir::new().unwrap();
        let path = history_path(&temporary);
        let now = Utc::now();
        let alpha = History::open(&path, server("alpha"), &config(100), now).unwrap();
        let beta = History::open(&path, server("beta"), &config(100), now).unwrap();
        let (left, right) = tokio::join!(
            alpha
                .persist(&notification("alpha", "a", now))
                .unwrap()
                .wait(),
            beta.persist(&notification("beta", "b", now))
                .unwrap()
                .wait(),
        );
        left.unwrap();
        right.unwrap();
        let all = alpha
            .list(HistoryQuery {
                all_servers: true,
                ..HistoryQuery::default()
            })
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn concurrent_first_open_applies_migration_once() {
        let temporary = TempDir::new().unwrap();
        let path = history_path(&temporary);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = ["alpha", "beta"].map(|server_id| {
            let path = path.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                History::open(&path, server(server_id), &config(100), Utc::now())
            })
        });

        for handle in handles {
            let result = handle.join().unwrap();
            assert!(result.is_ok(), "{result:?}");
        }
        let connection = Connection::open(path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn worker_retries_real_write_contention_until_lock_is_released() {
        let temporary = TempDir::new().unwrap();
        let path = history_path(&temporary);
        let now = Utc::now();
        let history = History::open(&path, server("alpha"), &config(100), now).unwrap();
        let (locked, wait_for_lock) = mpsc::channel();
        let lock_path = path.clone();
        let locker = thread::spawn(move || {
            let connection = Connection::open(lock_path).unwrap();
            connection.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked.send(()).unwrap();
            thread::sleep(Duration::from_millis(60));
            connection.execute_batch("COMMIT").unwrap();
        });
        wait_for_lock.recv().unwrap();

        assert_eq!(
            history
                .persist(&notification("alpha", "after contention", now))
                .unwrap()
                .wait()
                .await
                .unwrap(),
            PersistenceStatus::Persisted
        );
        locker.join().unwrap();
    }

    #[tokio::test]
    async fn reopening_recovers_only_current_servers_live_rows() {
        let temporary = TempDir::new().unwrap();
        let path = history_path(&temporary);
        let now = DateTime::from_timestamp_millis(1_000).unwrap();
        {
            let alpha = History::open(&path, server("alpha"), &config(100), now).unwrap();
            let beta = History::open(&path, server("beta"), &config(100), now).unwrap();
            alpha
                .persist(&notification("alpha", "a", now))
                .unwrap()
                .wait()
                .await
                .unwrap();
            beta.persist(&notification("beta", "b", now))
                .unwrap()
                .wait()
                .await
                .unwrap();
            alpha.flush().unwrap().wait().await.unwrap();
            beta.flush().unwrap().wait().await.unwrap();
        }
        let recovered_at = DateTime::from_timestamp_millis(2_000).unwrap();
        let alpha = History::open(&path, server("alpha"), &config(100), recovered_at).unwrap();
        let all = alpha
            .list(HistoryQuery {
                all_servers: true,
                ..HistoryQuery::default()
            })
            .unwrap()
            .wait()
            .await
            .unwrap();
        let recovered = all
            .iter()
            .find(|entry| entry.tmux_server_id.as_str() == "alpha")
            .unwrap();
        let untouched = all
            .iter()
            .find(|entry| entry.tmux_server_id.as_str() == "beta")
            .unwrap();
        assert_eq!(recovered.delivery, DeliveryState::Closed);
        assert_eq!(recovered.close_reason, Some(CloseReason::DaemonInterrupted));
        assert_eq!(untouched.delivery, DeliveryState::Pending);
    }

    #[tokio::test]
    async fn disabled_history_does_not_touch_the_database() {
        let temporary = TempDir::new().unwrap();
        let path = history_path(&temporary);
        let history = History::open(
            &path,
            server("alpha"),
            &HistoryConfig {
                enabled: false,
                max_entries: 10_000,
            },
            Utc::now(),
        )
        .unwrap();
        assert!(!history.is_enabled());
        assert_eq!(
            history
                .persist(&notification("alpha", "ignored", Utc::now()))
                .unwrap()
                .wait()
                .await
                .unwrap(),
            PersistenceStatus::Disabled
        );
        assert!(!path.exists());
    }

    #[test]
    fn busy_retries_are_bounded() {
        let attempts = std::cell::Cell::new(0);
        let result = with_busy_retry::<()>(|| {
            attempts.set(attempts.get() + 1);
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                None,
            ))
        });
        assert!(matches!(result, Err(HistoryError::Sqlite(_))));
        assert_eq!(attempts.get(), BUSY_ATTEMPTS);
    }

    #[tokio::test]
    async fn clear_is_scoped_and_each_selector_removes_only_matching_rows() {
        let temporary = TempDir::new().unwrap();
        let path = history_path(&temporary);
        let early = DateTime::from_timestamp_millis(1_000).unwrap();
        let late = DateTime::from_timestamp_millis(3_000).unwrap();
        let alpha = History::open(&path, server("alpha"), &config(100), early).unwrap();
        let beta = History::open(&path, server("beta"), &config(100), early).unwrap();
        let hidden = notification("alpha", "hidden", early);
        alpha.persist(&hidden).unwrap().wait().await.unwrap();
        alpha.hide(hidden.id(), late).unwrap().wait().await.unwrap();
        alpha
            .persist(&notification("alpha", "late", late))
            .unwrap()
            .wait()
            .await
            .unwrap();
        beta.persist(&notification("beta", "other", early))
            .unwrap()
            .wait()
            .await
            .unwrap();

        assert_eq!(
            alpha
                .clear(ClearFilter::Hidden, false)
                .unwrap()
                .wait()
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            alpha
                .clear(ClearFilter::Before(late), false)
                .unwrap()
                .wait()
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            alpha
                .clear(ClearFilter::All, false)
                .unwrap()
                .wait()
                .await
                .unwrap(),
            1
        );
        let remaining = beta
            .list(HistoryQuery {
                all_servers: true,
                ..HistoryQuery::default()
            })
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].body, "other");
    }

    #[tokio::test]
    async fn plain_is_cell_bounded_and_ndjson_is_complete_one_object_per_line() {
        let temporary = TempDir::new().unwrap();
        let path = history_path(&temporary);
        let now = DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
        let history = History::open(&path, server("alpha"), &config(100), now).unwrap();
        history
            .persist(&notification("alpha", "日本語🙂 complete body", now))
            .unwrap()
            .wait()
            .await
            .unwrap();
        let entries = history
            .list(HistoryQuery::default())
            .unwrap()
            .wait()
            .await
            .unwrap();

        let mut plain = Vec::new();
        write_plain(&entries, 24, &mut plain).unwrap();
        let plain = String::from_utf8(plain).unwrap();
        assert_eq!(plain.lines().count(), 1);
        assert!(plain.trim_end().width() <= 24);
        assert!(plain.contains('…'));
        assert!(!plain.contains("\u{1b}"));

        let mut json = Vec::new();
        write_ndjson(&entries, &mut json).unwrap();
        let json = String::from_utf8(json).unwrap();
        assert_eq!(json.lines().count(), 1);
        let decoded: serde_json::Value = serde_json::from_str(json.trim_end()).unwrap();
        assert_eq!(decoded["body"], "日本語🙂 complete body");
        assert_eq!(decoded["tmux_server_id"], "alpha");
    }
}
