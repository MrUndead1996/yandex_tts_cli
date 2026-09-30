//! Keep a Glagol connection available across network and token failures.
//!
//! Rust port of `yandex_stationd/connection.py`: [`ConnectionManager`] owns
//! the device token and the Glagol client, starts background recovery
//! without requiring a live network, reconnects with exponential backoff and
//! jitter (capped), honours auth `Retry-After`, invalidates and retries the
//! device token when the Station closes the socket with code 4000, refreshes
//! a rotated token, and never replays a command whose outcome is unknown.
//!
//! Errors and [`Debug`] output never contain the device token or TTS text.
//!
//! Auth and the Glagol transport are injectable ([`TokenSource`] and
//! [`GlagolDialer`]) so tests drive connection, failure, recovery and token
//! refresh deterministically without a real network. Production bindings are
//! [`YandexAuth`] and [`GlagolWsDialer`] over [`GlagolClient`].

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use yandex_tts_auth::{AuthError, YandexAuth};

use crate::glagol::{GlagolClient, GlagolError, GlagolTls};
use crate::station::{Station, StationError};

/// Jitter is at most this fraction of the current backoff delay.
const MAX_JITTER_FRACTION: f64 = 0.2;

/// Errors reported by [`ConnectionManager`].
///
/// Variants never carry the device token, request payloads or TTS text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ManagerError {
    /// The manager was closed and will not reconnect.
    #[error("connection manager is closed")]
    Closed,
    /// The caller-provided timeout elapsed while waiting for readiness.
    #[error("timed out waiting for the station connection")]
    Timeout,
    /// The manager is configured with invalid intervals.
    #[error("retry and refresh intervals must be positive")]
    InvalidConfig,
    /// The underlying Glagol exchange failed.
    #[error(transparent)]
    Glagol(#[from] GlagolError),
}

/// Source of the per-device Glagol token.
///
/// Implemented by [`YandexAuth`]; mocked in tests.
pub trait TokenSource: Send + Sync + 'static {
    fn get_device_token(
        &self,
        force_refresh: bool,
    ) -> impl Future<Output = Result<String, AuthError>> + Send;
    fn invalidate_device_token(&self) -> impl Future<Output = ()> + Send;
}

impl TokenSource for YandexAuth {
    async fn get_device_token(&self, force_refresh: bool) -> Result<String, AuthError> {
        YandexAuth::get_device_token(self, force_refresh).await
    }

    async fn invalidate_device_token(&self) {
        YandexAuth::invalidate_device_token(self).await
    }
}

/// A live or dialable Glagol connection. Implemented by [`GlagolClient`].
pub trait GlagolConnection: Send + Sync + 'static {
    fn connect(&self) -> impl Future<Output = Result<(), GlagolError>> + Send;
    fn send(&self, payload: Value) -> impl Future<Output = Result<Value, GlagolError>> + Send;
    fn close(&self) -> impl Future<Output = Result<(), GlagolError>> + Send;
    fn wait_closed(&self) -> impl Future<Output = Result<(), GlagolError>> + Send;

    /// Make the Station say `phrase` (same payload as [`GlagolClient::say`]).
    fn say(&self, phrase: &str) -> impl Future<Output = Result<Value, GlagolError>> + Send {
        let payload = crate::glagol::say_payload(phrase);
        async move { self.send(payload).await }
    }
}

impl GlagolConnection for GlagolClient {
    async fn connect(&self) -> Result<(), GlagolError> {
        GlagolClient::connect(self).await
    }

    async fn send(&self, payload: Value) -> Result<Value, GlagolError> {
        GlagolClient::send(self, payload).await
    }

    async fn close(&self) -> Result<(), GlagolError> {
        GlagolClient::close(self).await
    }

    async fn wait_closed(&self) -> Result<(), GlagolError> {
        GlagolClient::wait_closed(self).await
    }
}

/// Creates Glagol connections for a freshly obtained device token.
///
/// Production implementation: [`GlagolWsDialer`]. Test implementations hand
/// out in-memory mock connections.
pub trait GlagolDialer: Send + Sync + 'static {
    type Conn: GlagolConnection;
    fn dial(&self, device_token: &str) -> Result<Self::Conn, GlagolError>;
}

/// Production dialer over [`GlagolClient`] (real WSS to the Station).
#[derive(Clone)]
pub struct GlagolWsDialer {
    uri: String,
    tls: GlagolTls,
    timeout: Duration,
    ping_interval: Duration,
    ping_timeout: Duration,
}

impl GlagolWsDialer {
    pub fn new(uri: impl Into<String>, tls: GlagolTls) -> Self {
        Self {
            uri: uri.into(),
            tls,
            timeout: crate::glagol::DEFAULT_TIMEOUT,
            ping_interval: crate::glagol::DEFAULT_PING_INTERVAL,
            ping_timeout: crate::glagol::DEFAULT_PING_TIMEOUT,
        }
    }

    pub fn with_options(
        uri: impl Into<String>,
        tls: GlagolTls,
        timeout: Duration,
        ping_interval: Duration,
        ping_timeout: Duration,
    ) -> Self {
        Self {
            uri: uri.into(),
            tls,
            timeout,
            ping_interval,
            ping_timeout,
        }
    }
}

impl GlagolDialer for GlagolWsDialer {
    type Conn = GlagolClient;

    fn dial(&self, device_token: &str) -> Result<Self::Conn, GlagolError> {
        GlagolClient::with_options(
            &self.uri,
            device_token,
            self.tls,
            self.timeout,
            self.ping_interval,
            self.ping_timeout,
        )
    }
}

struct Shared<C> {
    /// The live connection while the manager considers itself ready.
    client: Option<Arc<C>>,
    closed: bool,
}

/// Owns the device token and reconnects without requiring callers to
/// reinitialize. Cloneable; clones share one background recovery task.
pub struct ConnectionManager<T: TokenSource, D: GlagolDialer> {
    shared: Arc<SharedInner<T, D>>,
}

struct SharedInner<T: TokenSource, D: GlagolDialer> {
    auth: T,
    dialer: D,
    retry_interval: Duration,
    max_retry_interval: Duration,
    refresh_interval: Duration,
    /// Scheduling jitter only; never derived from or applied to secrets.
    jitter: Arc<dyn Fn(Duration) -> Duration + Send + Sync>,
    /// Bumped on every observable state change (client set/removed,
    /// shutdown); waiters re-check state after each change. A `watch` is
    /// used instead of `Notify` so a wakeup can never be lost between a
    /// state check and the wait.
    progress: tokio::sync::watch::Sender<u64>,
    /// Bumped when a command fails with an unknown outcome; the monitor
    /// loop for the affected connection rebuilds it immediately.
    restart: tokio::sync::watch::Sender<u64>,
    state: Mutex<Shared<D::Conn>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl<T: TokenSource, D: GlagolDialer> std::fmt::Debug for ConnectionManager<T, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose tokens; state is described coarsely on purpose.
        f.debug_struct("ConnectionManager")
            .field("connected", &ConnectionManager::connected(self))
            .finish_non_exhaustive()
    }
}

impl<T: TokenSource, D: GlagolDialer> Clone for ConnectionManager<T, D> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

/// Scheduling jitter in `[0, 20%]` of `delay`, derived from the clock.
/// Not cryptographic and not used for anything secret.
fn default_jitter(delay: Duration) -> Duration {
    if delay.is_zero() {
        return Duration::ZERO;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0u32, |d| d.subsec_nanos());
    let fraction = f64::from(nanos % 1000) / 1000.0;
    Duration::try_from_secs_f64(delay.as_secs_f64() * MAX_JITTER_FRACTION * fraction)
        .unwrap_or(Duration::ZERO)
}

/// The next reconnect wait: auth `Retry-After` wins when larger; otherwise
/// the current delay plus jitter. Pure helper so the policy is unit-testable.
fn next_wait(
    delay: Duration,
    retry_after: Option<Duration>,
    jitter: impl Fn(Duration) -> Duration,
) -> Duration {
    let base = delay + jitter(delay);
    match retry_after {
        Some(after) => after.max(base),
        None => base,
    }
}

/// Doubling backoff capped at `max`. Pure helper for tests.
fn next_delay(delay: Duration, max: Duration) -> Duration {
    delay.checked_mul(2).unwrap_or(max).min(max)
}

impl<T: TokenSource, D: GlagolDialer> ConnectionManager<T, D> {
    pub fn new(
        auth: T,
        dialer: D,
        retry_interval: Duration,
        max_retry_interval: Duration,
        refresh_interval: Duration,
    ) -> Result<Self, ManagerError> {
        Self::with_jitter(
            auth,
            dialer,
            retry_interval,
            max_retry_interval,
            refresh_interval,
            default_jitter,
        )
    }

    /// Same as [`ConnectionManager::new`] with an injectable jitter function
    /// (tests pass a constant, e.g. `Duration::ZERO`).
    pub fn with_jitter(
        auth: T,
        dialer: D,
        retry_interval: Duration,
        max_retry_interval: Duration,
        refresh_interval: Duration,
        jitter: impl Fn(Duration) -> Duration + Send + Sync + 'static,
    ) -> Result<Self, ManagerError> {
        if retry_interval.is_zero()
            || max_retry_interval < retry_interval
            || refresh_interval.is_zero()
        {
            return Err(ManagerError::InvalidConfig);
        }
        Ok(Self {
            shared: Arc::new(SharedInner {
                auth,
                dialer,
                retry_interval,
                max_retry_interval,
                refresh_interval,
                jitter: Arc::new(jitter),
                progress: tokio::sync::watch::channel(0).0,
                restart: tokio::sync::watch::channel(0).0,
                state: Mutex::new(Shared {
                    client: None,
                    closed: false,
                }),
                task: Mutex::new(None),
            }),
        })
    }

    /// Start recovery in the background, even if the network is down.
    pub fn start(&self) -> Result<(), ManagerError> {
        // Lock order is always `state` before `task` (see `close`), so a
        // concurrent `start`/`close` can never deadlock. Spawning under the
        // lock also keeps check-and-install atomic: concurrent `start`s can
        // never create a second, detached recovery loop.
        {
            let state = self.shared.state.lock().expect("state lock");
            if state.closed {
                return Err(ManagerError::Closed);
            }
            let mut task = self.shared.task.lock().expect("task lock");
            if task.is_some() {
                return Ok(());
            }
            let shared = Arc::clone(&self.shared);
            *task = Some(tokio::spawn(async move { run(shared).await }));
        }
        Ok(())
    }

    /// Whether a Station connection is currently available.
    pub fn connected(&self) -> bool {
        let state = self.shared.state.lock().expect("state lock");
        !state.closed && state.client.is_some()
    }

    /// Wait for a working connection; subsequent recovery is automatic.
    pub async fn connect(&self) -> Result<(), ManagerError> {
        self.start()?;
        self.wait_ready(None).await.map(|_| ())
    }

    /// Wait until a connection is ready (or the manager is closed). With
    /// `timeout`, readiness is bounded by the caller — the daemon's request
    /// handler uses this so `say` fails fast instead of hanging.
    async fn wait_ready(&self, timeout: Option<Duration>) -> Result<Arc<D::Conn>, ManagerError> {
        let mut progress = self.shared.progress.subscribe();
        let wait = async {
            loop {
                {
                    let state = self.shared.state.lock().expect("state lock");
                    if state.closed {
                        return Err(ManagerError::Closed);
                    }
                    if let Some(client) = &state.client {
                        return Ok(Arc::clone(client));
                    }
                }
                // Subscribe happened before the check above; every later
                // state change bumps the counter, so no wakeup is lost.
                let _ = progress.changed().await;
            }
        };
        match timeout {
            Some(limit) => tokio::time::timeout(limit, wait)
                .await
                .map_err(|_| ManagerError::Timeout)?,
            None => wait.await,
        }
    }

    /// Wait for availability, send, and never replay a command with an
    /// uncertain outcome: any failure marks the connection for rebuild and is
    /// returned to the caller.
    pub async fn send(&self, payload: Value) -> Result<Value, ManagerError> {
        self.send_within(None, payload).await
    }

    /// [`ConnectionManager::send`] with a caller-provided bound on how long
    /// waiting for readiness may take (used by the daemon's request handler).
    pub async fn send_within(
        &self,
        timeout: Option<Duration>,
        payload: Value,
    ) -> Result<Value, ManagerError> {
        self.start()?;
        let client = self.wait_ready(timeout).await?;
        let result = client.send(payload).await;
        self.after_result(&result);
        result.map_err(ManagerError::from)
    }

    pub async fn say(&self, phrase: &str) -> Result<Value, ManagerError> {
        self.say_within(None, phrase).await
    }

    /// [`ConnectionManager::say`] with a bounded readiness wait.
    pub async fn say_within(
        &self,
        timeout: Option<Duration>,
        phrase: &str,
    ) -> Result<Value, ManagerError> {
        self.start()?;
        let client = self.wait_ready(timeout).await?;
        let result = GlagolConnection::say(&*client, phrase).await;
        self.after_result(&result);
        result.map_err(ManagerError::from)
    }

    /// A failed command leaves the outcome unknown: mark the connection for
    /// an immediate rebuild, but never resend automatically. Token rejection
    /// (close code 4000) is picked up by the background `wait_closed`
    /// watcher instead, so it does not need a restart kick here.
    fn after_result(&self, result: &Result<Value, GlagolError>) {
        if let Err(error @ (GlagolError::Closed | GlagolError::Timeout | GlagolError::Transport)) =
            result
        {
            let _ = error;
            self.shared.restart.send_modify(|version| *version += 1);
        }
    }

    /// Stop retries, close the active socket and wake every waiter so it can
    /// observe shutdown. Idempotent.
    pub async fn close(&self) {
        // Lock order: `state` before `task` (mirrors `start`).
        let (task, client) = {
            let mut state = self.shared.state.lock().expect("state lock");
            if state.closed {
                return;
            }
            state.closed = true;
            let task = self.shared.task.lock().expect("task lock").take();
            (task, state.client.take())
        };
        // Close the active socket before killing the loop, so the socket is
        // always shut down even when the loop is parked in a backoff sleep
        // or blocked mid-connect.
        if let Some(client) = client {
            let _ = client.close().await;
        }
        // Wake everyone waiting for readiness; they now observe `closed`.
        self.shared.progress.send_modify(|version| *version += 1);
        if let Some(task) = task {
            task.abort();
            // Wait until the background loop finished cancelling so no late
            // reconnect or cleanup outlives `close`.
            let _ = task.await;
        }
    }

    /// Resolve when the manager is closed.
    pub async fn wait_closed(&self) {
        let mut progress = self.shared.progress.subscribe();
        loop {
            if self.shared.state.lock().expect("state lock").closed {
                return;
            }
            let _ = progress.changed().await;
        }
    }
}

/// Background recovery loop (port of `ConnectionManager._run`).
async fn run<T: TokenSource, D: GlagolDialer>(shared: Arc<SharedInner<T, D>>) {
    let mut delay = shared.retry_interval;
    loop {
        if shared.state.lock().expect("state lock").closed {
            return;
        }
        let mut retry_after: Option<Duration> = None;
        let mut token_changed = false;

        match shared.auth.get_device_token(false).await {
            Ok(token) => match shared.dialer.dial(&token) {
                Ok(conn) => {
                    let conn = Arc::new(conn);
                    match conn.connect().await {
                        Ok(()) => {
                            // `close` may have happened while connecting:
                            // never publish a client after close (that would
                            // leave a stale, unmanaged connection behind).
                            let restart_version = *shared.restart.borrow();
                            let closed = {
                                let mut state = shared.state.lock().expect("state lock");
                                if state.closed {
                                    true
                                } else {
                                    state.client = Some(Arc::clone(&conn));
                                    false
                                }
                            };
                            if closed {
                                let _ = conn.close().await;
                                return;
                            }
                            shared.progress.send_modify(|version| *version += 1);
                            delay = shared.retry_interval;
                            // Watch the connection until it closes, the
                            // manager requests a rebuild, or the token
                            // rotates underneath us. `restart_version` was
                            // captured *before* the client was published: a
                            // send that obtained the client can only fail
                            // (and bump the counter) after this point, so a
                            // rebuild request can never be missed.
                            //
                            // Test hook: widen the publish→subscribe window
                            // so the race is deterministically exercisable.
                            #[cfg(test)]
                            tokio::task::yield_now().await;
                            let mut restart = shared.restart.subscribe();
                            let restart_requested = *restart.borrow_and_update() != restart_version;
                            // A rebuild requested in that window must go
                            // through the normal cleanup below — never a
                            // `break` of the outer run loop (that would
                            // leave the client published and open).
                            if !restart_requested {
                                loop {
                                    tokio::select! {
                                    closed = conn.wait_closed() => {
                                        if closed == Err(GlagolError::InvalidToken) {
                                            // Station rejected the token with
                                            // close code 4000: drop it so the
                                            // next attempt fetches a new one.
                                            shared.auth.invalidate_device_token().await;
                                        }
                                        break;
                                    }
                                    _ = restart.changed() => {
                                        if *restart.borrow() != restart_version {
                                            break;
                                        }
                                    }
                                    _ = tokio::time::sleep(shared.refresh_interval) => {
                                        match shared.auth.get_device_token(false).await {
                                            Ok(current) => {
                                                if current != token {
                                                    token_changed = true;
                                                    break;
                                                }
                                            }
                                            // A rate-limited probe ends this
                                            // connection and the wait below
                                            // honours Retry-After.
                                            Err(AuthError::RateLimit { retry_after: after }) => {
                                                retry_after = after;
                                                break;
                                            }
                                            // Other probe failures keep the
                                            // connection; the next tick
                                            // retries.
                                            Err(_) => {}
                                        }
                                    }
                                    }
                                }
                            }
                        }
                        Err(GlagolError::InvalidToken) => {
                            shared.auth.invalidate_device_token().await;
                        }
                        Err(_) => {}
                    }
                }
                Err(GlagolError::InvalidToken) => {
                    shared.auth.invalidate_device_token().await;
                }
                // Dial failures (network down, bad URI) retry with backoff.
                Err(_) => {}
            },
            Err(AuthError::RateLimit { retry_after: after }) => {
                retry_after = after;
            }
            // Other auth failures (network, 401/403, malformed) retry with
            // backoff; details are never logged — they can carry URLs.
            Err(_) => {}
        }

        // `finally`: drop whatever connection we held, close it, and wake
        // waiters so they re-read the state.
        let previous = {
            let mut state = shared.state.lock().expect("state lock");
            state.client.take()
        };
        shared.progress.send_modify(|version| *version += 1);
        if let Some(client) = previous {
            let _ = client.close().await;
        }
        if shared.state.lock().expect("state lock").closed {
            return;
        }
        if token_changed {
            delay = shared.retry_interval;
            continue;
        }
        let wait = next_wait(delay, retry_after, |d| (shared.jitter)(d));
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown_requested(&shared) => return,
        }
        delay = next_delay(delay, shared.max_retry_interval);
    }
}

/// Resolves as soon as the manager is closed.
async fn shutdown_requested<T: TokenSource, D: GlagolDialer>(shared: &SharedInner<T, D>) {
    let mut progress = shared.progress.subscribe();
    loop {
        if shared.state.lock().expect("state lock").closed {
            return;
        }
        let _ = progress.changed().await;
    }
}

impl<T: TokenSource, D: GlagolDialer> Station for ConnectionManager<T, D> {
    fn connected(&self) -> bool {
        ConnectionManager::connected(self)
    }

    async fn say(&self, text: &str) -> Result<(), StationError> {
        // The daemon's request handler bounds the readiness wait with its
        // own timeout; any failure surfaces as `station_not_connected` and
        // the command is never replayed. A correlated response that carries
        // an explicit Station-side failure is also not a success.
        match ConnectionManager::say(self, text).await {
            Ok(response) if say_response_accepted(&response) => Ok(()),
            Ok(response) => {
                if std::env::var_os("YANDEX_TTS_DIAGNOSTICS").is_some() {
                    let status = response
                        .get("status")
                        .and_then(Value::as_str)
                        .filter(|value| {
                            value.len() <= 32
                                && value
                                    .bytes()
                                    .all(|c| c.is_ascii_alphanumeric() || c == b'_')
                        })
                        .unwrap_or("missing_or_nonstandard");
                    let error_code = match response.get("errorCode") {
                        None => "absent",
                        Some(value) if value.is_null() => "null",
                        Some(value) if value == 0 || value == "0" => "zero",
                        Some(_) => "nonzero_or_other",
                    };
                    eprintln!(
                        "yandex-ttsd: say rejected: status={status} error_code={error_code} error_present={}",
                        response.get("error").is_some_and(|v| !v.is_null())
                    );
                }
                Err(StationError::NotConnected)
            }
            Err(error) => {
                if std::env::var_os("YANDEX_TTS_DIAGNOSTICS").is_some() {
                    let reason = match error {
                        ManagerError::Closed => "closed",
                        ManagerError::Timeout => "readiness_timeout",
                        ManagerError::InvalidConfig => "invalid_config",
                        ManagerError::Glagol(GlagolError::Timeout) => "response_timeout",
                        ManagerError::Glagol(_) => "connection_failed",
                    };
                    eprintln!("yandex-ttsd: say failed: {reason}");
                }
                Err(StationError::NotConnected)
            }
        }
    }
}

/// Whether a correlated Station response can count as accepted.
///
/// Only an explicit failure marker turns the response into a failure
/// (`Ok(())` stays conservative otherwise). Inspects coarse fields only and
/// never echoes the payload back, so no TTS text can leak through errors.
pub(crate) fn say_response_accepted(response: &Value) -> bool {
    let Some(obj) = response.as_object() else {
        return false;
    };
    // An explicit error object rejects the command. Real Station responses
    // may include a non-zero/non-numeric errorCode alongside status=SUCCESS;
    // status is authoritative when present (the Python client accepted any
    // correlated response, regardless of this metadata).
    if obj.get("error").is_some_and(|value| !value.is_null()) {
        return false;
    }
    match obj.get("status") {
        // A `status` field must name a recognized confirmation; anything
        // else (unknown value, wrong type) is not proof of success.
        Some(status) => matches!(
            status.as_str().map(str::to_ascii_lowercase).as_deref(),
            Some("ok" | "success" | "accepted" | "done" | "ack")
        ),
        // Without a status, a non-zero errorCode is an explicit failure.
        None => !obj
            .get("errorCode")
            .is_some_and(|value| !value.is_null() && value != 0 && value != "0"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MockAuth, MockConn, MockDialer};
    use serde_json::json;

    const RETRY: Duration = Duration::from_millis(20);
    const MAX_RETRY: Duration = Duration::from_millis(80);
    const REFRESH: Duration = Duration::from_millis(40);

    type TestManager = ConnectionManager<MockAuth, MockDialer>;

    pub(crate) fn manager(auth: MockAuth, dialer: MockDialer) -> TestManager {
        ConnectionManager::with_jitter(auth, dialer, RETRY, MAX_RETRY, REFRESH, |_| Duration::ZERO)
            .expect("valid intervals")
    }

    async fn wait_until_connected(manager: &TestManager) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !manager.connected() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "manager never became connected"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // ----- tests -----

    #[tokio::test]
    async fn connects_in_background_and_sends() {
        let conn = MockConn::new();
        let dialer = MockDialer::new();
        dialer.push_conn(Arc::clone(&conn));
        let manager = manager(MockAuth::new("tok-1"), dialer);
        assert!(!manager.connected());

        manager.start().expect("start");
        wait_until_connected(&manager).await;

        let send = tokio::spawn({
            let manager = manager.clone();
            async move { manager.send(json!({"command": "ping"})).await }
        });
        let payload = conn.next_sent().await;
        assert_eq!(payload["command"], "ping");
        conn.answer(Ok(json!({"requestId": "x", "ok": true})));
        let response = tokio::time::timeout(Duration::from_secs(2), send)
            .await
            .expect("send finished")
            .expect("send task")
            .expect("send ok");
        assert_eq!(response["ok"], true);
        manager.close().await;
    }

    #[tokio::test]
    async fn start_works_without_live_network_and_reports_disconnected() {
        let dialer = MockDialer::new();
        dialer.push_failure(GlagolError::Transport);
        let manager = manager(MockAuth::new("tok"), dialer);
        manager.start().expect("start without network");
        assert!(!manager.connected());
        manager.close().await;
        assert!(!manager.connected());
    }

    #[tokio::test]
    async fn invalid_token_close_4000_invalidates_and_retries_with_new_token() {
        let first = MockConn::new();
        let dialer = MockDialer::new();
        dialer.push_conn(Arc::clone(&first));
        let auth = MockAuth::new("stale-token");
        auth.set_rotated("fresh-token");
        let manager = manager(auth, dialer);
        manager.start().expect("start");
        wait_until_connected(&manager).await;

        // Station rejects the token with close code 4000.
        first.disconnect(Some(GlagolError::InvalidToken));
        // The manager stays `connected` until the monitor notices; wait for
        // the re-dial with the rotated token instead.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "manager never redialled with the rotated token"
            );
            let seen = manager.shared.dialer.tokens_seen();
            if seen.last().map(String::as_str) == Some("fresh-token") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert!(manager.shared.auth.invalidations() >= 1);
        assert_eq!(
            manager.shared.dialer.tokens_seen().last().unwrap(),
            "fresh-token"
        );
        manager.close().await;
    }

    #[tokio::test]
    async fn periodic_refresh_detects_rotated_token_and_reconnects() {
        let dialer = MockDialer::new();
        let auth = MockAuth::new("tok-a");
        let manager = manager(auth, dialer);
        manager.start().expect("start");
        wait_until_connected(&manager).await;
        let dials_before = manager.shared.dialer.dials();

        // The cached token rotates server-side.
        manager.shared.auth.set_token("tok-b");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while manager.shared.dialer.dials() == dials_before {
            assert!(
                tokio::time::Instant::now() < deadline,
                "refresh probe never redialled"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(manager.shared.dialer.tokens_seen().last().unwrap(), "tok-b");
        assert!(manager.connected());
        manager.close().await;
    }

    #[tokio::test]
    async fn rate_limit_retry_after_is_respected() {
        let dialer = MockDialer::new();
        let auth = MockAuth::new("tok");
        auth.push(Err(AuthError::RateLimit {
            retry_after: Some(Duration::from_millis(150)),
        }));
        let manager = manager(auth, dialer);
        let started = tokio::time::Instant::now();
        manager.connect().await.expect("eventually connects");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150),
            "must wait out Retry-After, took {elapsed:?}"
        );
        manager.close().await;
    }

    #[tokio::test]
    async fn failed_send_is_not_replayed() {
        let conn = MockConn::new();
        let dialer = MockDialer::new();
        dialer.push_conn(Arc::clone(&conn));
        let manager = manager(MockAuth::new("tok"), dialer);
        manager.start().expect("start");
        wait_until_connected(&manager).await;

        // A command goes out and the connection dies before any answer:
        // the outcome is unknown and must surface to the caller as-is.
        let send = tokio::spawn({
            let manager = manager.clone();
            async move { manager.send(json!({"command": "say"})).await }
        });
        conn.next_sent().await;
        conn.disconnect(None);

        let error = tokio::time::timeout(Duration::from_secs(2), send)
            .await
            .expect("send finished")
            .expect("send task")
            .expect_err("send must fail");
        assert_eq!(error, ManagerError::Glagol(GlagolError::Closed));
        // The error must not carry the token.
        assert!(!format!("{error:?}").contains("tok"), "{error:?}");

        // Exactly one payload was sent; nothing was replayed.
        assert!(conn.sent_drained());
        // The failed command marks the connection for an immediate rebuild.
        assert!(!manager.connected());
        manager.close().await;
    }

    #[tokio::test]
    async fn recovers_after_connection_drop() {
        let first = MockConn::new();
        let dialer = MockDialer::new();
        dialer.push_conn(Arc::clone(&first));
        let manager = manager(MockAuth::new("tok"), dialer);
        manager.start().expect("start");
        wait_until_connected(&manager).await;

        first.disconnect(None);
        // Wait for the re-dial (the manager reports connected until the
        // monitor notices the drop).
        let dials_before = manager.shared.dialer.dials();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while manager.shared.dialer.dials() == dials_before {
            assert!(
                tokio::time::Instant::now() < deadline,
                "manager never redialled"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Wait out the rebuild (close of the old socket, connect of the new).
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Traffic flows on the new (default) connection.
        let send = tokio::spawn({
            let manager = manager.clone();
            async move { manager.send(json!({"command": "after"})).await }
        });
        // The new connection is the dialer default; grab it from state.
        let current = manager
            .shared
            .state
            .lock()
            .expect("state lock")
            .client
            .clone()
            .expect("reconnected");
        current.next_sent().await;
        current.answer(Ok(json!({"requestId": "y"})));
        let response = tokio::time::timeout(Duration::from_secs(2), send)
            .await
            .expect("send finished")
            .expect("send task")
            .expect("send ok");
        assert_eq!(response["requestId"], "y");
        manager.close().await;
    }

    #[tokio::test]
    async fn send_within_bounds_readiness_wait() {
        let dialer = MockDialer::new();
        dialer.push_failure(GlagolError::Transport);
        dialer.push_failure(GlagolError::Transport);
        let manager = manager(MockAuth::new("tok"), dialer);
        manager.start().expect("start");
        let started = tokio::time::Instant::now();
        let error = manager
            .send_within(Some(Duration::from_millis(50)), json!({"command": "x"}))
            .await
            .expect_err("readiness must time out");
        assert_eq!(error, ManagerError::Timeout);
        assert!(started.elapsed() < Duration::from_millis(500));
        manager.close().await;
    }

    #[tokio::test]
    async fn close_wakes_waiters_and_reports_closed() {
        let dialer = MockDialer::new();
        dialer.push_failure(GlagolError::Transport);
        let manager = manager(MockAuth::new("tok"), dialer);
        manager.start().expect("start");

        let waiter = tokio::spawn({
            let manager = manager.clone();
            async move { manager.connect().await }
        });
        // Let the waiter park, then close.
        tokio::time::sleep(Duration::from_millis(20)).await;
        manager.close().await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), waiter)
                .await
                .expect("waiter woke")
                .expect("waiter task")
                .unwrap_err(),
            ManagerError::Closed
        );
        // Further operations report closed.
        assert_eq!(manager.start().unwrap_err(), ManagerError::Closed);
        assert_eq!(
            manager.send(json!({})).await.unwrap_err(),
            ManagerError::Closed
        );
        // Idempotent.
        manager.close().await;
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        assert_eq!(next_delay(RETRY, MAX_RETRY), Duration::from_millis(40));
        assert_eq!(
            next_delay(Duration::from_millis(40), MAX_RETRY),
            Duration::from_millis(80)
        );
        assert_eq!(next_delay(Duration::from_millis(80), MAX_RETRY), MAX_RETRY);
        assert_eq!(next_delay(Duration::from_secs(100), MAX_RETRY), MAX_RETRY);
    }

    #[test]
    fn wait_policy_prefers_retry_after_and_applies_jitter_cap() {
        let fixed = |_d: Duration| Duration::from_millis(10);
        assert_eq!(
            next_wait(Duration::from_secs(5), None, fixed),
            Duration::from_secs(5) + Duration::from_millis(10)
        );
        // Retry-After wins when larger than delay + jitter.
        assert_eq!(
            next_wait(Duration::from_secs(5), Some(Duration::from_secs(30)), fixed),
            Duration::from_secs(30)
        );
        // ...but never shortens below delay + jitter.
        assert_eq!(
            next_wait(
                Duration::from_secs(5),
                Some(Duration::from_millis(1)),
                fixed
            ),
            Duration::from_secs(5) + Duration::from_millis(10)
        );
    }

    #[test]
    fn default_jitter_is_bounded_by_twenty_percent() {
        for delay in [Duration::from_millis(100), Duration::from_secs(30)] {
            for _ in 0..200 {
                let jitter = default_jitter(delay);
                assert!(
                    jitter
                        <= Duration::try_from_secs_f64(delay.as_secs_f64() * MAX_JITTER_FRACTION)
                            .unwrap()
                );
            }
        }
        assert_eq!(default_jitter(Duration::ZERO), Duration::ZERO);
    }

    #[tokio::test]
    async fn rejects_invalid_intervals() {
        assert_eq!(
            ConnectionManager::<MockAuth, MockDialer>::with_jitter(
                MockAuth::new("tok"),
                MockDialer::new(),
                Duration::ZERO,
                MAX_RETRY,
                REFRESH,
                |_| Duration::ZERO,
            )
            .unwrap_err(),
            ManagerError::InvalidConfig
        );
        assert_eq!(
            ConnectionManager::<MockAuth, MockDialer>::with_jitter(
                MockAuth::new("tok"),
                MockDialer::new(),
                MAX_RETRY,
                RETRY,
                REFRESH,
                |_| Duration::ZERO,
            )
            .unwrap_err(),
            ManagerError::InvalidConfig
        );
    }

    #[tokio::test]
    async fn start_after_close_is_rejected() {
        let manager = manager(MockAuth::new("tok"), MockDialer::new());
        manager.close().await;
        assert_eq!(manager.start().unwrap_err(), ManagerError::Closed);
    }

    #[tokio::test]
    async fn concurrent_start_and_close_never_deadlock_or_leak() {
        for _ in 0..50 {
            let conn = MockConn::new();
            let dialer = MockDialer::new();
            dialer.push_conn(Arc::clone(&conn));
            let manager = manager(MockAuth::new("tok"), dialer);

            let starter = tokio::spawn({
                let manager = manager.clone();
                async move { manager.start() }
            });
            let closer = tokio::spawn({
                let manager = manager.clone();
                async move { manager.close().await }
            });
            // Either outcome is fine; hanging is not.
            let (started, ()) = tokio::time::timeout(Duration::from_secs(2), async {
                (
                    starter.await.expect("starter"),
                    closer.await.expect("closer"),
                )
            })
            .await
            .expect("start/close race must not deadlock");
            // If start won the race, close's shutdown is complete by now; if
            // close won, start reports closed or exits without a task.
            let _ = started;

            // Settle: an already-spawned loop observes `closed` and stops.
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(!manager.connected());
            assert!(!conn.is_connected());
        }
    }

    #[tokio::test]
    async fn close_during_blocked_connect_publishes_nothing_and_leaks_no_socket() {
        let conn = MockConn::new();
        let dialer = MockDialer::new();
        dialer.push_conn(Arc::clone(&conn));
        let manager = manager(MockAuth::new("secret-token"), dialer);

        // Block `connect` before its critical section.
        let gate = conn.hold_gate().await;
        manager.start().expect("start");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !conn.connect_started() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "connect never started"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // Close while the connect is parked.
        let closer = tokio::spawn({
            let manager = manager.clone();
            async move { manager.close().await }
        });
        tokio::time::timeout(Duration::from_secs(2), closer)
            .await
            .expect("close must not wait on a blocked connect")
            .expect("closer task");

        // Release the gate: whether the aborted loop resumes or not, the
        // connection must never become managed or stay open.
        drop(gate);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!manager.connected());
        assert!(!conn.is_connected());

        // Nothing leaks into later use either.
        assert_eq!(
            manager.send(json!({})).await.unwrap_err(),
            ManagerError::Closed
        );
    }

    #[tokio::test]
    async fn concurrent_starts_spawn_exactly_one_loop() {
        let conn = MockConn::new();
        let dialer = MockDialer::new();
        dialer.push_conn(Arc::clone(&conn));
        let manager = manager(MockAuth::new("tok"), dialer);

        // Hold the gate so a hypothetical duplicate loop would stay parked
        // in its own `connect` and show up as a second dial later.
        let gate = conn.hold_gate().await;
        let starters: Vec<_> = (0..8)
            .map({
                let manager = manager.clone();
                move |_| {
                    let manager = manager.clone();
                    tokio::spawn(async move { manager.start() })
                }
            })
            .collect();
        for starter in starters {
            tokio::time::timeout(Duration::from_secs(2), starter)
                .await
                .expect("start must not deadlock")
                .expect("starter task")
                .expect("start ok");
        }

        // Let the single loop through the gate.
        drop(gate);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !manager.connected() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "manager never became connected"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Exactly one recovery loop exists: one dial only.
        assert_eq!(manager.shared.dialer.dials(), 1);
        manager.close().await;
    }

    #[tokio::test]
    async fn restart_requested_in_publish_window_goes_through_cleanup() {
        let conn = MockConn::new();
        let dialer = MockDialer::new();
        dialer.push_conn(Arc::clone(&conn));
        let manager = manager(MockAuth::new("tok"), dialer);

        // Park `connect` so we control exactly when the loop resumes into
        // the publish→subscribe window.
        let gate = conn.hold_gate().await;
        manager.start().expect("start");

        // As soon as the client becomes visible (publish), bump the restart
        // counter — before the monitor subscribes (the loop yields there in
        // test builds).
        let bumper = tokio::spawn({
            let manager = manager.clone();
            async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                loop {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "client was never published"
                    );
                    if manager.connected() {
                        manager.after_result(&Err(GlagolError::Closed));
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            }
        });

        drop(gate);
        tokio::time::timeout(Duration::from_secs(2), bumper)
            .await
            .expect("bumper finished")
            .expect("bumper task");

        // The rebuild must flow through the normal cleanup: the old client
        // is unpublished/closed and a fresh dial happens — the manager does
        // not get stuck, and the loop does not die.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let seen = manager.shared.dialer.tokens_seen().len();
            if seen >= 2 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "restart in publish window never triggered a rebuild"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!conn.is_connected());
        while !manager.connected() {
            assert!(
                tokio::time::Instant::now() < deadline + Duration::from_secs(2),
                "manager never recovered after the rebuild"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        manager.close().await;
    }

    #[test]
    fn say_response_failure_detection() {
        // An explicit error object fails even with a success status.
        assert!(!say_response_accepted(
            &json!({"status": "ok", "error": "boom"})
        ));
        assert!(!say_response_accepted(&json!({"status": "error"})));
        assert!(!say_response_accepted(&json!({"error": "x"})));
        assert!(!say_response_accepted(&json!({"errorCode": 1})));
        assert!(!say_response_accepted(&json!({"errorCode": "REJECTED"})));
        // Unknown or non-string status is not a confirmation.
        assert!(!say_response_accepted(&json!({"status": "pending"})));
        assert!(!say_response_accepted(&json!({"status": 3})));
        assert!(!say_response_accepted(&json!("plain string")));
        // Correlated answers without failure markers count as accepted.
        assert!(say_response_accepted(&json!({"status": "ok"})));
        assert!(say_response_accepted(&json!({"status": "Success"})));
        assert!(say_response_accepted(
            &json!({"status": "SUCCESS", "errorCode": 0})
        ));
        assert!(say_response_accepted(
            &json!({"status": "SUCCESS", "errorCode": "OTHER"})
        ));
        assert!(say_response_accepted(
            &json!({"status": "SUCCESS", "errorCode": null, "error": null})
        ));
        assert!(say_response_accepted(&json!({"result": "done"})));
        assert!(say_response_accepted(&json!({})));
    }

    #[tokio::test]
    async fn station_trait_say_maps_errors_and_debug_hides_token() {
        let manager = Arc::new(manager(MockAuth::new("secret-token"), MockDialer::new()));
        assert!(!ConnectionManager::connected(&manager));

        // `say` waits for readiness; close in the background unblocks it.
        let say = tokio::spawn({
            let manager = Arc::clone(&manager);
            async move { Station::say(&*manager, "Привет").await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        manager.close().await;
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), say)
                .await
                .expect("say finished")
                .expect("say task"),
            Err(StationError::NotConnected)
        ));
        // Debug output must not leak the token.
        assert!(!format!("{manager:?}").contains("secret-token"));
    }
}
