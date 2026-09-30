//! In-memory mocks for the connection manager's injectable ports.
//!
//! Compiled unconditionally (like [`crate::station::mock`]) so integration
//! tests can drive the real daemon stack — `ConnectionManager` behind the
//! Unix socket server — without network, real Yandex credentials or a real
//! Station. Not part of the production wiring.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use yandex_tts_auth::AuthError;

use crate::connection::{GlagolConnection, GlagolDialer, TokenSource};
use crate::glagol::GlagolError;

/// Mock [`TokenSource`]: scripted results, cached token and rotation.
#[derive(Default)]
struct AuthState {
    token: String,
    /// Returned instead of `token` after an invalidation (simulates the
    /// auth service issuing a fresh token for the rejected device).
    rotated: Option<String>,
    queue: VecDeque<Result<String, AuthError>>,
    invalidated: bool,
}

pub struct MockAuth {
    state: Mutex<AuthState>,
    invalidations: AtomicUsize,
}

impl MockAuth {
    pub fn new(token: &str) -> Self {
        Self {
            state: Mutex::new(AuthState {
                token: token.to_owned(),
                rotated: None,
                queue: VecDeque::new(),
                invalidated: false,
            }),
            invalidations: AtomicUsize::new(0),
        }
    }

    pub fn push(&self, result: Result<String, AuthError>) {
        self.state.lock().unwrap().queue.push_back(result);
    }

    pub fn set_token(&self, token: &str) {
        self.state.lock().unwrap().token = token.to_owned();
    }

    pub fn set_rotated(&self, token: &str) {
        self.state.lock().unwrap().rotated = Some(token.to_owned());
    }

    pub fn invalidations(&self) -> usize {
        self.invalidations.load(Ordering::SeqCst)
    }
}

impl TokenSource for MockAuth {
    async fn get_device_token(&self, _force_refresh: bool) -> Result<String, AuthError> {
        let mut state = self.state.lock().unwrap();
        // Scripted results are consumed first (rate limit, rotation,
        // post-invalidation refresh); otherwise the cached token.
        if let Some(result) = state.queue.pop_front() {
            return result;
        }
        if state.invalidated {
            state.invalidated = false;
            if let Some(rotated) = &state.rotated {
                return Ok(rotated.clone());
            }
        }
        Ok(state.token.clone())
    }

    async fn invalidate_device_token(&self) {
        self.invalidations.fetch_add(1, Ordering::SeqCst);
        self.state.lock().unwrap().invalidated = true;
    }
}

/// In-memory [`GlagolConnection`]: forwards envelopes to the test, plays
/// scripted answers and simulates disconnects.
pub struct MockConn {
    connected: AtomicBool,
    /// Envelopes forwarded by `send` (sender) and observed by tests
    /// (receiver side stays reachable through `next_sent`).
    sent_tx: mpsc::UnboundedSender<Value>,
    sent: Mutex<mpsc::UnboundedReceiver<Value>>,
    /// Scripted answers; `send` receives from it.
    responses_tx: mpsc::UnboundedSender<Result<Value, GlagolError>>,
    responses: tokio::sync::Mutex<mpsc::UnboundedReceiver<Result<Value, GlagolError>>>,
    close_error: Mutex<Option<GlagolError>>,
    closed_tx: tokio::sync::watch::Sender<bool>,
    closed_rx: tokio::sync::watch::Receiver<bool>,
    /// Tests hold this lock to block `connect` (simulating a slow dial).
    connect_gate: Arc<tokio::sync::Mutex<()>>,
    connect_started: AtomicBool,
}

impl MockConn {
    pub fn new() -> Arc<Self> {
        let (sent_tx, sent_rx) = mpsc::unbounded_channel();
        let (responses_tx, responses_rx) = mpsc::unbounded_channel();
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            connected: AtomicBool::new(false),
            sent_tx,
            sent: Mutex::new(sent_rx),
            responses_tx,
            responses: tokio::sync::Mutex::new(responses_rx),
            close_error: Mutex::new(None),
            closed_tx,
            closed_rx,
            connect_gate: Arc::new(tokio::sync::Mutex::new(())),
            connect_started: AtomicBool::new(false),
        })
    }

    /// Hold the gate so `connect` blocks until the guard is dropped.
    pub async fn hold_gate(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.connect_gate).lock_owned().await
    }

    pub fn connect_started(&self) -> bool {
        self.connect_started.load(Ordering::SeqCst)
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// True when every forwarded envelope has been consumed by tests.
    pub fn sent_drained(&self) -> bool {
        self.sent.lock().unwrap().try_recv().is_err()
    }

    pub async fn next_sent(&self) -> Value {
        loop {
            if let Ok(payload) = self.sent.lock().unwrap().try_recv() {
                return payload;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    pub fn answer(&self, response: Result<Value, GlagolError>) {
        self.responses_tx.send(response).expect("response channel");
    }

    pub fn set_close_error(&self, error: GlagolError) {
        *self.close_error.lock().unwrap() = Some(error);
    }

    /// Simulate the peer (or network) dropping the connection.
    pub fn disconnect(&self, error: Option<GlagolError>) {
        if let Some(error) = error {
            self.set_close_error(error);
        }
        self.connected.store(false, Ordering::SeqCst);
        let _ = self.closed_tx.send(true);
    }
}

impl GlagolConnection for MockConn {
    async fn connect(&self) -> Result<(), GlagolError> {
        self.connect_started.store(true, Ordering::SeqCst);
        // Tests may block here by holding the gate.
        let _gate = self.connect_gate.lock().await;
        if self.close_error.lock().unwrap().is_some() {
            return Err(GlagolError::Closed);
        }
        self.connected.store(true, Ordering::SeqCst);
        let _ = self.closed_tx.send(false);
        Ok(())
    }

    async fn send(&self, payload: Value) -> Result<Value, GlagolError> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(GlagolError::NotConnected);
        }
        // Forward the envelope, then wait for the Station's answer; if
        // the connection dies first the outcome is lost — exactly the
        // situation in which the manager must not replay.
        let _ = self.sent_tx.send(payload);
        let mut closed = self.closed_rx.clone();
        // Mark the current state as seen so `changed()` only fires on a
        // real close after this send started.
        let _ = closed.borrow_and_update();
        let mut responses = self.responses.lock().await;
        tokio::select! {
            response = responses.recv() => match response {
                Some(response) => response,
                None => Err(GlagolError::Closed),
            },
            _ = closed.changed() => {
                // An answer may have raced with the close.
                match responses.try_recv() {
                    Ok(response) => response,
                    Err(_) => Err(
                        self.close_error
                            .lock()
                            .unwrap()
                            .clone()
                            .unwrap_or(GlagolError::Closed),
                    ),
                }
            }
        }
    }

    async fn close(&self) -> Result<(), GlagolError> {
        self.connected.store(false, Ordering::SeqCst);
        let _ = self.closed_tx.send(true);
        match self.close_error.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn wait_closed(&self) -> Result<(), GlagolError> {
        let mut closed = self.closed_rx.clone();
        loop {
            if *closed.borrow_and_update() {
                return match self.close_error.lock().unwrap().clone() {
                    Some(GlagolError::InvalidToken) => Err(GlagolError::InvalidToken),
                    _ => Ok(()),
                };
            }
            closed.changed().await.map_err(|_| GlagolError::Closed)?;
        }
    }
}

// The dialer hands out shared connections; the manager stores `Arc<C>`.
impl GlagolConnection for Arc<MockConn> {
    async fn connect(&self) -> Result<(), GlagolError> {
        GlagolConnection::connect(&**self).await
    }

    async fn send(&self, payload: Value) -> Result<Value, GlagolError> {
        GlagolConnection::send(&**self, payload).await
    }

    async fn close(&self) -> Result<(), GlagolError> {
        GlagolConnection::close(&**self).await
    }

    async fn wait_closed(&self) -> Result<(), GlagolError> {
        GlagolConnection::wait_closed(&**self).await
    }
}

/// Mock [`GlagolDialer`]: hands out scripted connections or failures;
/// default is a fresh working [`MockConn`].
pub struct MockDialer {
    queue: Mutex<VecDeque<Result<Arc<MockConn>, GlagolError>>>,
    /// Persistent failure replacing the queue when set.
    failure: Mutex<Option<GlagolError>>,
    dials: AtomicUsize,
    tokens_seen: Mutex<Vec<String>>,
}

impl Default for MockDialer {
    fn default() -> Self {
        Self::new()
    }
}

impl MockDialer {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            failure: Mutex::new(None),
            dials: AtomicUsize::new(0),
            tokens_seen: Mutex::new(Vec::new()),
        }
    }

    pub fn push_conn(&self, conn: Arc<MockConn>) {
        self.queue.lock().unwrap().push_back(Ok(conn));
    }

    pub fn push_failure(&self, error: GlagolError) {
        self.queue.lock().unwrap().push_back(Err(error));
    }

    /// Every dial fails, forever (simulates a permanently unreachable
    /// Station); dials are still counted.
    pub fn always_fail(&self, error: GlagolError) {
        *self.queue.lock().unwrap() = VecDeque::new();
        *self.failure.lock().unwrap() = Some(error);
    }

    pub fn dials(&self) -> usize {
        self.dials.load(Ordering::SeqCst)
    }

    pub fn tokens_seen(&self) -> Vec<String> {
        self.tokens_seen.lock().unwrap().clone()
    }
}

impl GlagolDialer for MockDialer {
    type Conn = Arc<MockConn>;

    fn dial(&self, device_token: &str) -> Result<Self::Conn, GlagolError> {
        self.dials.fetch_add(1, Ordering::SeqCst);
        self.tokens_seen
            .lock()
            .unwrap()
            .push(device_token.to_owned());
        if let Some(error) = self.failure.lock().unwrap().clone() {
            return Err(error);
        }
        match self.queue.lock().unwrap().pop_front() {
            Some(Ok(conn)) => Ok(conn),
            Some(Err(error)) => Err(error),
            // Default: a fresh, working connection.
            None => Ok(MockConn::new()),
        }
    }
}
