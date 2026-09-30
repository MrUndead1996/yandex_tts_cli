//! Local JSON Lines API over a Unix stream socket.
//!
//! Rust port of `yandex_stationd/server.py`: one JSON object per line in, one
//! JSON response per request. Error codes, size limit (65536), timeouts (10s)
//! and socket lifecycle semantics match the Python implementation.

use crate::station::{Station, StationError};
use serde_json::{Value, json};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use yandex_tts_protocol::Response;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_MESSAGE_SIZE: usize = 65536;

#[derive(Debug)]
pub enum ServerError {
    Io(io::Error),
    /// Another daemon owns the socket, or the path is not a replaceable socket.
    SocketInUse(PathBuf),
    /// The path exists but is not a socket owned by the current user.
    RefuseReplace(PathBuf),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerError::Io(e) => write!(f, "{e}"),
            ServerError::SocketInUse(path) => write!(f, "Socket is in use: {}", path.display()),
            ServerError::RefuseReplace(path) => {
                write!(f, "Refusing to replace {}", path.display())
            }
        }
    }
}

impl std::error::Error for ServerError {}

pub struct Server<S: Station> {
    path: PathBuf,
    station: Arc<S>,
    timeout: Duration,
    max_message_size: usize,
    listener: Option<UnixListener>,
    socket_identity: Option<(u64, u64)>,
    stop: Arc<tokio::sync::Notify>,
    /// Cooperative stop flag for client handlers: idle handlers exit
    /// immediately, in-flight requests are never interrupted mid-dispatch.
    stopping: Arc<tokio::sync::watch::Sender<bool>>,
    clients: Mutex<Vec<JoinHandle<()>>>,
}

impl<S: Station> Server<S> {
    pub fn new(path: impl AsRef<Path>, station: Arc<S>) -> Server<S> {
        Self::with_options(path, station, DEFAULT_TIMEOUT, MAX_MESSAGE_SIZE)
    }

    pub fn with_options(
        path: impl AsRef<Path>,
        station: Arc<S>,
        timeout: Duration,
        max_message_size: usize,
    ) -> Server<S> {
        Server {
            path: path.as_ref().to_owned(),
            station,
            timeout,
            max_message_size,
            listener: None,
            socket_identity: None,
            stop: Arc::new(tokio::sync::Notify::new()),
            stopping: Arc::new(tokio::sync::watch::channel(false).0),
            clients: Mutex::new(Vec::new()),
        }
    }

    /// Binds the socket so it is owner-only (`0600`) from creation onward:
    /// the listener is created under a private temporary name, restricted, and
    /// then atomically published via `link` (which never clobbers an existing
    /// path). A stale socket left by a previous run of this user is replaced;
    /// a live one is never taken over.
    pub async fn start(&mut self) -> Result<(), ServerError> {
        if self.listener.is_some() {
            return Ok(());
        }
        self.remove_stale_socket()?;
        let temp_path = self.temp_path();
        // A leftover temp file from a crashed run of this pid; nothing else
        // can use this unpredictable name in the user-owned directory.
        let _ = std::fs::remove_file(&temp_path);
        let listener = match UnixListener::bind(&temp_path) {
            Ok(listener) => listener,
            Err(e) => return Err(ServerError::Io(e)),
        };
        if let Err(e) = std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600))
        {
            drop(listener);
            let _ = std::fs::remove_file(&temp_path);
            return Err(ServerError::Io(e));
        }
        // Publish atomically: hard_link fails instead of replacing a socket
        // that appeared between the stale check and now.
        if let Err(e) = std::fs::hard_link(&temp_path, &self.path) {
            drop(listener);
            let _ = std::fs::remove_file(&temp_path);
            return Err(if e.kind() == io::ErrorKind::AlreadyExists {
                ServerError::SocketInUse(self.path.clone())
            } else {
                ServerError::Io(e)
            });
        }
        let _ = std::fs::remove_file(&temp_path);
        let meta = match std::fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(e) => {
                drop(listener);
                let _ = std::fs::remove_file(&self.path);
                return Err(ServerError::Io(e));
            }
        };
        self.socket_identity = Some((meta.dev(), meta.ino()));
        self.listener = Some(listener);
        Ok(())
    }

    /// Private unpublished name for the listening socket.
    fn temp_path(&self) -> PathBuf {
        let unique = format!(
            ".{}.{}.yandex-stationd.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        self.path.with_file_name(unique)
    }

    fn remove_stale_socket(&self) -> Result<(), ServerError> {
        let Ok(meta) = std::fs::symlink_metadata(&self.path) else {
            return Ok(());
        };
        if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::getuid() } {
            return Err(ServerError::RefuseReplace(self.path.clone()));
        }
        if !stale(&self.path) {
            return Err(ServerError::SocketInUse(self.path.clone()));
        }
        std::fs::remove_file(&self.path).map_err(ServerError::Io)
    }

    /// Accepts clients until [`Server::shutdown`] is called.
    pub async fn serve_forever(&mut self) -> io::Result<()> {
        let listener = match self.listener.take() {
            Some(listener) => listener,
            None => {
                self.start()
                    .await
                    .map_err(|e| io::Error::other(e.to_string()))?;
                self.listener.take().expect("just started")
            }
        };
        loop {
            let accept = listener.accept();
            tokio::select! {
                _ = self.stop.notified() => {
                    self.shutdown().await;
                    return Ok(());
                }
                result = accept => match result {
                Ok((stream, _)) => {
                    let station = Arc::clone(&self.station);
                    let client_timeout = self.timeout;
                    let max = self.max_message_size;
                    let stopping = self.stopping.subscribe();
                    let handle = tokio::spawn(async move {
                        handle_client(stream, station, client_timeout, max, stopping).await;
                    });
                    self.clients.lock().unwrap().push(handle);
                }
                    Err(e) => return Err(e),
                },
            }
        }
    }

    /// Handle used to ask [`Server::serve_forever`] to return.
    pub fn stop_handle(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.stop)
    }

    /// Stops accepting, waits up to `timeout` for in-flight client requests
    /// to finish (they are each already bounded by the request timeout, so
    /// this drain is bounded too), aborts stragglers and removes the socket
    /// file only if it is still the one this server created.
    pub async fn shutdown(&mut self) {
        self.listener = None;
        // Tell every handler to stop reading new lines; in-flight requests
        // finish and are then drained with a bounded budget below.
        let _ = self.stopping.send(true);
        let mut clients: Vec<_> = std::mem::take(&mut *self.clients.lock().unwrap());
        let deadline = tokio::time::Instant::now() + self.timeout;
        for handle in &mut clients {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            // Poll the handler without consuming the handle, so stragglers
            // can still be aborted after the drain budget. Each handle is
            // awaited exactly once here.
            if tokio::time::timeout(remaining, &mut *handle).await.is_err() {
                handle.abort();
                let _ = (&mut *handle).await;
            }
        }
        if let Some((dev, ino)) = self.socket_identity.take()
            && let Ok(meta) = std::fs::symlink_metadata(&self.path)
            && meta.file_type().is_socket()
            && (meta.dev(), meta.ino()) == (dev, ino)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Probes whether nothing is listening behind the socket file: only a refused
/// connection counts as stale, exactly like `server.py:_remove_stale_socket`.
fn stale(path: &Path) -> bool {
    match StdUnixStream::connect(path) {
        Err(err) => matches!(
            err.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
        ),
        Ok(_) => false,
    }
}

async fn handle_client<S: Station>(
    stream: UnixStream,
    station: Arc<S>,
    request_timeout: Duration,
    max_message_size: usize,
    stopping: tokio::sync::watch::Receiver<bool>,
) {
    // Bytes past the first newline of a read are preserved so pipelined
    // requests on one connection are handled one by one.
    let mut reader = LineReader::new(stream);
    let mut stopping = stopping;
    loop {
        // Shutdown closes idle connections immediately (no line is being
        // read); a request already being dispatched is never interrupted.
        let stopped = async {
            if *stopping.borrow_and_update() {
                return;
            }
            let _ = stopping.changed().await;
        };
        let line = tokio::select! {
            _ = stopped => break,
            line = timeout(request_timeout, reader.next_line(max_message_size)) => {
                match line {
                    Err(_) => {
                        respond(reader.stream_mut(), Response::error("timeout")).await;
                        break;
                    }
                    Ok(Err(ReadLineError::Io(err))) => {
                        log::debug!("Client connection failed: {err}");
                        break;
                    }
                    Ok(Err(ReadLineError::TooLarge)) => {
                        respond(reader.stream_mut(), Response::error("message_too_large")).await;
                        break;
                    }
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(line))) => line,
                }
            }
        };
        let response = match serde_json::from_slice::<Value>(&line) {
            Err(_) => Response::error("invalid_json"),
            Ok(request) => {
                match timeout(request_timeout, dispatch(station.as_ref(), request)).await {
                    Err(_) => {
                        if std::env::var_os("YANDEX_TTS_DIAGNOSTICS").is_some() {
                            eprintln!("yandex-ttsd: request deadline reached");
                        }
                        Response::error("station_not_connected")
                    }
                    Ok(Ok(response)) => response,
                    Ok(Err(StationError::NotConnected)) => Response::error("station_not_connected"),
                    Ok(Err(StationError::Internal(kind))) => {
                        log::error!("Request failed ({kind})");
                        Response::error("internal_error")
                    }
                }
            }
        };
        respond(reader.stream_mut(), response).await;
    }
    let _ = reader.stream_mut().shutdown().await;
}

enum ReadLineError {
    Io(io::Error),
    TooLarge,
}

/// Buffered line reader over one client connection. Surplus bytes received
/// after a newline (pipelined requests) are kept for subsequent calls.
struct LineReader {
    stream: UnixStream,
    buf: Vec<u8>,
    consumed: usize,
}

impl LineReader {
    fn new(stream: UnixStream) -> LineReader {
        LineReader {
            stream,
            buf: Vec::with_capacity(4096),
            consumed: 0,
        }
    }

    fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    /// Reads one `\n`-terminated line, enforcing the size limit exactly like
    /// the Python `StreamReader` (`limit = max_message_size + 1`). Returns
    /// `Ok(None)` on clean EOF at a line boundary.
    async fn next_line(
        &mut self,
        max_message_size: usize,
    ) -> Result<Option<Vec<u8>>, ReadLineError> {
        loop {
            if let Some(idx) = self.buf[self.consumed..].iter().position(|&b| b == b'\n') {
                let end = self.consumed + idx;
                let line = self.buf[self.consumed..=end].to_vec();
                self.consumed = end + 1;
                // Reclaim memory once the consumed prefix dominates the buffer.
                if self.consumed > 4096 && self.consumed * 2 > self.buf.len() {
                    self.buf.drain(..self.consumed);
                    self.consumed = 0;
                }
                if line.len() > max_message_size {
                    return Err(ReadLineError::TooLarge);
                }
                return Ok(Some(line));
            }
            if self.buf.len() - self.consumed > max_message_size {
                return Err(ReadLineError::TooLarge);
            }
            let mut chunk = [0u8; 4096];
            let n = self
                .stream
                .read(&mut chunk)
                .await
                .map_err(ReadLineError::Io)?;
            if n == 0 {
                return if self.buf.len() == self.consumed {
                    Ok(None)
                } else {
                    // Unterminated trailing data: server.py rejects it as too large.
                    Err(ReadLineError::TooLarge)
                };
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Dispatches one parsed request. Mirrors `server.py:_dispatch` validation.
async fn dispatch<S: Station>(station: &S, request: Value) -> Result<Response, StationError> {
    let Some(obj) = request.as_object() else {
        return Ok(Response::error("invalid_request"));
    };
    let Some(action) = obj.get("action").and_then(Value::as_str) else {
        return Ok(Response::error("invalid_request"));
    };
    match action {
        "ping" => {
            if obj.len() != 1 {
                return Ok(Response::error("invalid_request"));
            }
            Ok(Response::Connected(station.connected()))
        }
        "say" => {
            if obj.len() != 2 {
                return Ok(Response::error("invalid_request"));
            }
            let Some(text) = obj.get("text").and_then(Value::as_str) else {
                return Ok(Response::error("invalid_request"));
            };
            if text.trim().is_empty() {
                return Ok(Response::error("invalid_request"));
            }
            // ok only after the station confirms the command.
            station.say(text).await?;
            Ok(Response::Ok)
        }
        _ => Ok(Response::error("unknown_action")),
    }
}

async fn respond(stream: &mut UnixStream, response: Response) {
    let line = match response {
        Response::Ok => json!({"ok": true}),
        Response::Connected(connected) => json!({"ok": true, "connected": connected}),
        Response::Error(error) => json!({"ok": false, "error": error}),
    };
    let mut data = line.to_string().into_bytes();
    data.push(b'\n');
    let _ = stream.write_all(&data).await;
    let _ = stream.flush().await;
}
