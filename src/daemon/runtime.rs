//! Lazy per-tmux-server daemon ownership and bounded Unix-socket IPC.
//!
//! This module owns process/socket mechanics. Product request handling and
//! display cleanup are injected, keeping notification content out of spawned
//! process arguments and environment variables.

use std::fs;
use std::future::Future;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};

use crate::platform::{PathError, ensure_private_directory, validate_runtime_socket};
use crate::protocol::{
    CacheLookup, FrameDecoder, MAX_REQUEST_BYTES, PROTOCOL_VERSION, RequestEnvelope,
    RequestResultCache,
};

const SOCKET_MODE: u32 = 0o600;

/// Stable identity for one canonical tmux server socket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerIdentity {
    canonical_tmux_socket: PathBuf,
    server_id: String,
}

impl ServerIdentity {
    pub fn resolve(tmux_socket: &Path) -> Result<Self, RuntimeError> {
        let canonical_tmux_socket =
            fs::canonicalize(tmux_socket).map_err(|source| RuntimeError::Io {
                path: tmux_socket.to_owned(),
                source,
            })?;
        Ok(Self::from_canonical(
            canonical_tmux_socket,
            effective_user_id(),
        ))
    }

    fn from_canonical(canonical_tmux_socket: PathBuf, user_id: u32) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"tmnotify-server-v1\0");
        digest.update(user_id.to_be_bytes());
        digest.update(canonical_tmux_socket.as_os_str().as_bytes());
        let bytes = digest.finalize();
        let server_id = bytes[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self {
            canonical_tmux_socket,
            server_id,
        }
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn tmux_socket(&self) -> &Path {
        &self.canonical_tmux_socket
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RuntimeLimits {
    pub max_connections: usize,
    pub max_inflight: usize,
    pub max_inflight_per_connection: usize,
    pub connection_idle: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_inflight: 128,
            max_inflight_per_connection: 16,
            connection_idle: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(2),
        }
    }
}

impl RuntimeLimits {
    fn validate(self) -> Result<Self, RuntimeError> {
        if self.max_connections == 0
            || self.max_inflight == 0
            || self.max_inflight_per_connection == 0
            || self.connection_idle.is_zero()
            || self.shutdown_timeout.is_zero()
        {
            return Err(RuntimeError::InvalidLimits);
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeShutdown {
    Signal,
    ServerEnded,
}

/// Pure reconnect policy seam. Disconnection never proves server loss.
#[derive(Clone, Debug)]
pub struct ControlReconnectState {
    attempts: u8,
    initial: Duration,
    maximum: Duration,
}

impl Default for ControlReconnectState {
    fn default() -> Self {
        Self {
            attempts: 0,
            initial: Duration::from_millis(100),
            maximum: Duration::from_secs(5),
        }
    }
}

impl ControlReconnectState {
    pub fn disconnected(&mut self) -> Duration {
        let shift = self.attempts.min(10);
        self.attempts = self.attempts.saturating_add(1);
        self.initial
            .saturating_mul(1_u32 << shift)
            .min(self.maximum)
    }

    pub fn connected(&mut self) {
        self.attempts = 0;
    }
}

/// A listener plus inode identity. Drop removes only the exact socket bound by
/// this owner, never a replacement created after a race or crash.
pub struct OwnedSocket {
    listener: UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl OwnedSocket {
    pub fn bind(path: &Path) -> Result<Self, RuntimeError> {
        let parent = path
            .parent()
            .ok_or_else(|| RuntimeError::InvalidSocketPath(path.into()))?;
        ensure_private_directory(parent)?;

        match std::os::unix::net::UnixListener::bind(path) {
            Ok(listener) => Self::finish_bind(path, listener),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                recover_stale_socket(path)?;
                let listener = std::os::unix::net::UnixListener::bind(path).map_err(|source| {
                    if source.kind() == io::ErrorKind::AddrInUse {
                        RuntimeError::AlreadyRunning
                    } else {
                        RuntimeError::Io {
                            path: path.into(),
                            source,
                        }
                    }
                })?;
                Self::finish_bind(path, listener)
            }
            Err(source) => Err(RuntimeError::Io {
                path: path.into(),
                source,
            }),
        }
    }

    fn finish_bind(
        path: &Path,
        listener: std::os::unix::net::UnixListener,
    ) -> Result<Self, RuntimeError> {
        fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE)).map_err(|source| {
            RuntimeError::Io {
                path: path.into(),
                source,
            }
        })?;
        let metadata = fs::symlink_metadata(path).map_err(|source| RuntimeError::Io {
            path: path.into(),
            source,
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|source| RuntimeError::Io {
                path: path.into(),
                source,
            })?;
        let listener = UnixListener::from_std(listener).map_err(|source| RuntimeError::Io {
            path: path.into(),
            source,
        })?;
        Ok(Self {
            listener,
            path: path.into(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for OwnedSocket {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn recover_stale_socket(path: &Path) -> Result<(), RuntimeError> {
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(RuntimeError::AlreadyRunning);
    }
    validate_runtime_socket(path)?;
    // The final connect is intentionally after every ownership/type/mode check.
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(RuntimeError::AlreadyRunning);
    }
    let before = fs::symlink_metadata(path).map_err(|source| RuntimeError::Io {
        path: path.into(),
        source,
    })?;
    let after = fs::symlink_metadata(path).map_err(|source| RuntimeError::Io {
        path: path.into(),
        source,
    })?;
    if before.dev() != after.dev() || before.ino() != after.ino() || !after.file_type().is_socket()
    {
        return Err(RuntimeError::SocketChanged);
    }
    fs::remove_file(path).map_err(|source| RuntimeError::Io {
        path: path.into(),
        source,
    })
}

/// Serve NDJSON until the supplied shutdown future resolves. Cleanup is given
/// the reason and is bounded together with active connection teardown.
pub async fn serve<H, HF, S, C, CF>(
    owned: OwnedSocket,
    limits: RuntimeLimits,
    handler: H,
    shutdown: S,
    cleanup: C,
) -> Result<RuntimeShutdown, RuntimeError>
where
    H: Fn(RequestEnvelope) -> HF + Clone + Send + Sync + 'static,
    HF: Future<Output = Result<Value, String>> + Send + 'static,
    S: Future<Output = RuntimeShutdown> + Send,
    C: FnOnce(RuntimeShutdown) -> CF,
    CF: Future<Output = ()>,
{
    let limits = limits.validate()?;
    let connections = Arc::new(Semaphore::new(limits.max_connections));
    let inflight = Arc::new(Semaphore::new(limits.max_inflight));
    let cache = Arc::new(Mutex::new(RequestResultCache::<Vec<u8>>::default()));
    let mut tasks = JoinSet::new();
    tokio::pin!(shutdown);

    let reason = loop {
        tokio::select! {
            reason = &mut shutdown => break reason,
            accepted = owned.listener.accept() => {
                let (stream, _) = accepted.map_err(|source| RuntimeError::Io { path: owned.path.clone(), source })?;
                let Ok(permit) = connections.clone().try_acquire_owned() else { continue };
                tasks.spawn(connection_loop(stream, limits, handler.clone(), inflight.clone(), cache.clone(), permit));
            }
        }
    };

    tasks.abort_all();
    let deadline = async {
        while tasks.join_next().await.is_some() {}
        cleanup(reason).await;
    };
    timeout(limits.shutdown_timeout, deadline)
        .await
        .map_err(|_| RuntimeError::ShutdownTimedOut)?;
    drop(owned);
    Ok(reason)
}

async fn connection_loop<H, HF>(
    stream: UnixStream,
    limits: RuntimeLimits,
    handler: H,
    inflight: Arc<Semaphore>,
    cache: Arc<Mutex<RequestResultCache<Vec<u8>>>>,
    _connection: tokio::sync::OwnedSemaphorePermit,
) where
    H: Fn(RequestEnvelope) -> HF + Clone + Send + Sync + 'static,
    HF: Future<Output = Result<Value, String>> + Send + 'static,
{
    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let per_connection = Arc::new(Semaphore::new(limits.max_inflight_per_connection));
    let mut decoder = FrameDecoder::default();
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(result) = timeout(limits.connection_idle, reader.read(&mut buffer)).await else {
            break;
        };
        let Ok(read) = result else { break };
        if read == 0 {
            break;
        }
        let Ok(frames) = decoder.push(&buffer[..read]) else {
            break;
        };
        for frame in frames {
            let Ok(local) = per_connection.clone().try_acquire_owned() else {
                return;
            };
            let Ok(global) = inflight.clone().try_acquire_owned() else {
                return;
            };
            let writer = writer.clone();
            let cache = cache.clone();
            let handler = handler.clone();
            tokio::spawn(async move {
                let _local = local;
                let _global = global;
                let response = process_frame(&frame, handler, cache).await;
                if let Ok(mut response) = response {
                    response.push(b'\n');
                    let _ = writer.lock().await.write_all(&response).await;
                }
            });
        }
    }
}

async fn process_frame<H, HF>(
    frame: &[u8],
    handler: H,
    cache: Arc<Mutex<RequestResultCache<Vec<u8>>>>,
) -> Result<Vec<u8>, RuntimeError>
where
    H: Fn(RequestEnvelope) -> HF,
    HF: Future<Output = Result<Value, String>>,
{
    let request = RequestEnvelope::decode(frame)?;
    let now = Instant::now();
    match cache.lock().await.lookup(&request, now) {
        CacheLookup::Replay(response) => return Ok(response),
        CacheLookup::PayloadMismatch => {
            return encode_error(
                request.request_id,
                "request ID reused with different payload",
            );
        }
        CacheLookup::Miss => {}
    }
    let request_id = request.request_id;
    let result = handler(request.clone()).await;
    let response = match result {
        Ok(value) => serde_json::to_vec(
            &json!({ "version": PROTOCOL_VERSION, "request_id": request_id, "result": value }),
        )?,
        Err(message) => encode_error(request_id, &message)?,
    };
    if response.len() > MAX_REQUEST_BYTES {
        return Err(RuntimeError::ResponseTooLarge);
    }
    cache.lock().await.insert(&request, response.clone(), now);
    Ok(response)
}

fn encode_error(request_id: uuid::Uuid, message: &str) -> Result<Vec<u8>, RuntimeError> {
    Ok(serde_json::to_vec(
        &json!({ "version": PROTOCOL_VERSION, "request_id": request_id, "accepted": false, "error": message }),
    )?)
}

/// Connect first; only absent/refused sockets trigger a hidden daemon spawn.
/// Every retry writes the caller-provided bytes unchanged, preserving request ID.
pub async fn submit_lazy(
    socket: &Path,
    tmux_socket: &Path,
    request: &[u8],
) -> Result<Vec<u8>, RuntimeError> {
    if request.len() > MAX_REQUEST_BYTES {
        return Err(RuntimeError::RequestTooLarge);
    }
    match exchange(socket, request).await {
        Ok(response) => return Ok(response),
        Err(RuntimeError::Connect(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => return Err(error),
    }
    spawn_hidden_daemon(tmux_socket)?;
    for attempt in 0..40_u32 {
        match exchange(socket, request).await {
            Ok(response) => return Ok(response),
            Err(RuntimeError::Connect(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                sleep(Duration::from_millis(10 + u64::from(attempt.min(20)) * 5)).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(RuntimeError::StartupTimedOut)
}

async fn exchange(socket: &Path, request: &[u8]) -> Result<Vec<u8>, RuntimeError> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(RuntimeError::Connect)?;
    stream
        .write_all(request)
        .await
        .map_err(RuntimeError::Connect)?;
    stream
        .write_all(b"\n")
        .await
        .map_err(RuntimeError::Connect)?;
    let mut decoder = FrameDecoder::default();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = timeout(Duration::from_secs(2), stream.read(&mut chunk))
            .await
            .map_err(|_| RuntimeError::ResponseTimedOut)??;
        if read == 0 {
            return Err(RuntimeError::ConnectionClosed);
        }
        let mut frames = decoder.push(&chunk[..read])?;
        if let Some(frame) = frames.pop() {
            return Ok(frame);
        }
    }
}

fn spawn_hidden_daemon(tmux_socket: &Path) -> Result<(), RuntimeError> {
    let executable = std::env::current_exe().map_err(|source| RuntimeError::Io {
        path: PathBuf::from("current executable"),
        source,
    })?;
    if !executable.is_absolute() {
        return Err(RuntimeError::ExecutableNotAbsolute);
    }
    std::process::Command::new(executable)
        .arg("--socket-path")
        .arg(tmux_socket)
        .arg("__daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|source| RuntimeError::Io {
            path: tmux_socket.into(),
            source,
        })
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("daemon already owns this tmux server")]
    AlreadyRunning,
    #[error("runtime limits must be positive")]
    InvalidLimits,
    #[error("invalid runtime socket path: {0}")]
    InvalidSocketPath(PathBuf),
    #[error("runtime socket changed during stale recovery")]
    SocketChanged,
    #[error("request exceeds the protocol limit")]
    RequestTooLarge,
    #[error("response exceeds the protocol limit")]
    ResponseTooLarge,
    #[error("daemon startup timed out")]
    StartupTimedOut,
    #[error("daemon cleanup exceeded two seconds")]
    ShutdownTimedOut,
    #[error("daemon response timed out")]
    ResponseTimedOut,
    #[error("daemon closed the connection without a response")]
    ConnectionClosed,
    #[error("current executable path is not absolute")]
    ExecutableNotAbsolute,
    #[error("socket connection failed: {0}")]
    Connect(#[source] io::Error),
    #[error("runtime filesystem operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Protocol(#[from] crate::protocol::ProtocolError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<io::Error> for RuntimeError {
    fn from(error: io::Error) -> Self {
        Self::Connect(error)
    }
}

fn effective_user_id() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener as StdListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;
    use uuid::Uuid;

    fn private_dir() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        temp
    }

    #[test]
    fn identity_is_stable_and_scoped_by_uid() {
        let path = PathBuf::from("/tmp/tmux.sock");
        let first = ServerIdentity::from_canonical(path.clone(), 10);
        assert_eq!(first, ServerIdentity::from_canonical(path.clone(), 10));
        assert_ne!(
            first.server_id(),
            ServerIdentity::from_canonical(path, 11).server_id()
        );
        assert_eq!(first.server_id().len(), 32);
    }

    #[tokio::test]
    async fn live_listener_wins_bind_election() {
        let temp = private_dir();
        let path = temp.path().join("daemon.sock");
        let owner = OwnedSocket::bind(&path).unwrap();
        assert!(matches!(
            OwnedSocket::bind(&path),
            Err(RuntimeError::AlreadyRunning)
        ));
        assert!(path.exists());
        drop(owner);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn stale_socket_is_recovered_but_unsafe_entries_are_rejected() {
        let temp = private_dir();
        let stale = temp.path().join("stale.sock");
        let listener = StdListener::bind(&stale).unwrap();
        fs::set_permissions(&stale, fs::Permissions::from_mode(0o600)).unwrap();
        drop(listener);
        let owner = OwnedSocket::bind(&stale).unwrap();
        drop(owner);

        let regular = temp.path().join("regular.sock");
        fs::write(&regular, b"x").unwrap();
        fs::set_permissions(&regular, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            OwnedSocket::bind(&regular),
            Err(RuntimeError::Path(PathError::UnexpectedFileType(_)))
        ));

        let target = temp.path().join("target");
        fs::write(&target, b"x").unwrap();
        let link = temp.path().join("link.sock");
        std::os::unix::fs::symlink(target, &link).unwrap();
        assert!(matches!(
            OwnedSocket::bind(&link),
            Err(RuntimeError::Path(PathError::SymbolicLink(_)))
        ));
    }

    #[tokio::test]
    async fn server_replays_same_request_once_and_cleans_up() {
        let temp = private_dir();
        let path = temp.path().join("daemon.sock");
        let owner = OwnedSocket::bind(&path).unwrap();
        let (stop_tx, stop_rx) = oneshot::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let server = tokio::spawn(serve(
            owner,
            RuntimeLimits::default(),
            move |_| {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"accepted": true}))
                }
            },
            async { stop_rx.await.unwrap() },
            |_| async {},
        ));

        let id = Uuid::now_v7();
        let request =
            serde_json::to_vec(&json!({"version":1,"request_id":id,"type":"history"})).unwrap();
        let first = exchange(&path, &request).await.unwrap();
        let second = exchange(&path, &request).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        stop_tx.send(RuntimeShutdown::Signal).unwrap();
        assert_eq!(server.await.unwrap().unwrap(), RuntimeShutdown::Signal);
        assert!(!path.exists());
    }

    #[test]
    fn control_reconnect_is_bounded_and_resets() {
        let mut state = ControlReconnectState::default();
        assert_eq!(state.disconnected(), Duration::from_millis(100));
        for _ in 0..20 {
            assert!(state.disconnected() <= Duration::from_secs(5));
        }
        state.connected();
        assert_eq!(state.disconnected(), Duration::from_millis(100));
    }
}
