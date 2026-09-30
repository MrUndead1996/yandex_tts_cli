//! Minimal persistent Glagol WebSocket client.
//!
//! Port of `yandex_stationd/glagol.py`: one WSS connection to a Station,
//! envelopes with `conversationToken`/UUID `id`/`sentTime`, a background
//! reader that matches responses by `requestId`, heartbeat pings, request
//! timeouts and cleanup of pending requests on disconnect.
//!
//! TLS here applies only to the local Station's WSS endpoint. The
//! self-signed certificate of a Station is accepted only when
//! [`GlagolTls::AcceptSelfSigned`] is chosen explicitly; Yandex auth HTTP
//! traffic never goes through this module.

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{Connector, WebSocketStream, connect_async_tls_with_config};
use uuid::Uuid;

/// Default time budget for connecting and for awaiting a response.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default interval between heartbeat pings.
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(20);
/// Default budget for the Station to answer a heartbeat ping; an unresponsive
/// peer is treated as disconnected (`ping_timeout` in the Python client).
pub const DEFAULT_PING_TIMEOUT: Duration = Duration::from_secs(20);

/// TLS policy for the Station WSS connection.
///
/// This never applies to Yandex auth HTTP requests; it is only used when
/// dialing the `wss://` endpoint of a discovered or manually configured
/// local Station.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlagolTls {
    /// Validate the certificate against system root certificates.
    SystemRoots,
    /// Accept any certificate, including the Station's self-signed one.
    ///
    /// Intended only for a local Station whose certificate cannot be
    /// validated against public roots; do not use it for anything but the
    /// Station WSS endpoint.
    AcceptSelfSigned,
}

/// Errors returned by [`GlagolClient`].
///
/// Variants never contain the device token, request payloads or TTS text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GlagolError {
    /// The URI is not a `wss://` WebSocket endpoint.
    #[error("Glagol requires a wss:// URI")]
    InvalidUri,
    /// Options are invalid: timeout or ping interval must be positive.
    #[error("Glagol timeout and ping interval must be positive")]
    InvalidOptions,
    /// The client is not connected.
    #[error("Glagol is not connected")]
    NotConnected,
    /// The Station did not answer within the configured timeout.
    #[error("Glagol request timed out")]
    Timeout,
    /// The Station closed the connection with code 4000: the device token
    /// was rejected.
    #[error("Station rejected the device token")]
    InvalidToken,
    /// The connection was closed (by us, by the Station or by the network).
    #[error("Glagol connection closed")]
    Closed,
    /// A transport or TLS failure while dialing or talking to the Station.
    #[error("Glagol transport error")]
    Transport,
}

trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

/// Outgoing command for the background reader task.
enum Writer {
    Send(Message),
}

struct Shared {
    uri: String,
    device_token: String,
    tls: GlagolTls,
    timeout: Duration,
    ping_interval: Duration,
    /// How long a heartbeat ping may go unanswered before the peer counts as
    /// unresponsive; zero disables the check.
    ping_timeout: Duration,
    /// Serializes connect/close so they cannot interleave.
    lifecycle: tokio::sync::Mutex<()>,
    /// Bumped on every new reader; a stale reader with an older generation
    /// must not touch the writer, pending requests or `close_error` of the
    /// current connection.
    generation: std::sync::atomic::AtomicU64,
    /// Channel to the reader task; `None` while disconnected.
    writer: Mutex<Option<mpsc::Sender<Writer>>>,
    /// Requests awaiting responses, keyed by request id.
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, GlagolError>>>>,
    /// Error observed by the reader, set exactly once when it stops.
    close_error: watch::Sender<Option<GlagolError>>,
}

/// Removes the request's pending entry when the `send` future is dropped for
/// any reason — completion, timeout, error or cancellation.
struct PendingGuard<'a> {
    shared: &'a Shared,
    request_id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.shared
            .pending
            .lock()
            .expect("pending lock")
            .remove(&self.request_id);
    }
}

/// Client for the Glagol WebSocket API of a single Yandex Station.
///
/// Clones share the same connection, which makes the client usable from a
/// future connection manager without re-dialing per request. Reconnect and
/// token refresh are intentionally out of scope here.
#[derive(Clone)]
pub struct GlagolClient {
    shared: Arc<Shared>,
}

impl fmt::Debug for GlagolClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never expose the device token.
        let connected = self.shared.writer.lock().expect("writer lock").is_some();
        f.debug_struct("GlagolClient")
            .field("uri", &self.shared.uri)
            .field("connected", &connected)
            .finish_non_exhaustive()
    }
}

impl GlagolClient {
    /// Build a client for `uri` (must be `wss://`) authenticated with
    /// `device_token`. Call [`GlagolClient::connect`] before sending.
    pub fn new(uri: &str, device_token: &str, tls: GlagolTls) -> Result<Self, GlagolError> {
        Self::with_options(
            uri,
            device_token,
            tls,
            DEFAULT_TIMEOUT,
            DEFAULT_PING_INTERVAL,
            DEFAULT_PING_TIMEOUT,
        )
    }

    /// Same as [`GlagolClient::new`] with explicit request timeout, heartbeat
    /// interval and pong timeout. A zero `ping_timeout` disables the
    /// unresponsive-peer check; the other durations must be positive.
    pub fn with_options(
        uri: &str,
        device_token: &str,
        tls: GlagolTls,
        timeout: Duration,
        ping_interval: Duration,
        ping_timeout: Duration,
    ) -> Result<Self, GlagolError> {
        if !uri.starts_with("wss://") {
            return Err(GlagolError::InvalidUri);
        }
        if timeout.is_zero() || ping_interval.is_zero() {
            return Err(GlagolError::InvalidOptions);
        }
        let (close_error, _) = watch::channel(None);
        Ok(Self {
            shared: Arc::new(Shared {
                uri: uri.to_owned(),
                device_token: device_token.to_owned(),
                tls,
                timeout,
                ping_interval,
                ping_timeout,
                lifecycle: tokio::sync::Mutex::new(()),
                generation: std::sync::atomic::AtomicU64::new(0),
                writer: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                close_error,
            }),
        })
    }

    /// Open the WSS connection and start the background response reader.
    ///
    /// Concurrent `connect` and `close` calls are serialized; the second
    /// caller sees the state left by the first.
    pub async fn connect(&self) -> Result<(), GlagolError> {
        let _lifecycle = self.shared.lifecycle.lock().await;
        {
            let guard = self.shared.writer.lock().expect("writer lock");
            if guard.is_some() {
                return Ok(());
            }
        }
        let connector = Connector::Rustls(Arc::new(station_tls_config(self.shared.tls)?));
        let request = self
            .shared
            .uri
            .clone()
            .into_client_request()
            .map_err(|_| GlagolError::InvalidUri)?;
        let (ws, _response) = tokio::time::timeout(
            self.shared.timeout,
            connect_async_tls_with_config(request, None, false, Some(connector)),
        )
        .await
        .map_err(|_| GlagolError::Timeout)?
        .map_err(|_| GlagolError::Transport)?;
        self.start_reader(ws);
        Ok(())
    }

    /// Send `payload` and wait for the response whose `requestId` matches
    /// the generated envelope id. Fails with [`GlagolError::Timeout`] when
    /// the Station does not answer in time; the pending entry is removed.
    pub async fn send(&self, payload: Value) -> Result<Value, GlagolError> {
        let writer = {
            let guard = self.shared.writer.lock().expect("writer lock");
            match guard.as_ref() {
                Some(tx) => tx.clone(),
                None => return Err(GlagolError::NotConnected),
            }
        };
        let request_id = Uuid::new_v4().to_string();
        let envelope = json!({
            "conversationToken": self.shared.device_token,
            "id": request_id,
            "payload": payload,
            "sentTime": unix_millis(),
        });
        let (tx, rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .expect("pending lock")
            .insert(request_id.clone(), tx);
        let _guard = PendingGuard {
            shared: &self.shared,
            request_id: request_id.clone(),
        };

        let result = tokio::time::timeout(self.shared.timeout, async {
            writer
                .send(Writer::Send(Message::text(envelope.to_string())))
                .await
                .map_err(|_| GlagolError::Closed)?;
            rx.await.map_err(|_| GlagolError::Closed)?
        })
        .await;

        // The guard removes the pending entry on every exit path, including
        // cancellation (the future being dropped mid-await).
        match result {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(error),
            Err(_elapsed) => Err(GlagolError::Timeout),
        }
    }

    /// Make the Station say `phrase`.
    ///
    /// The returned value is whatever response the Station correlated with
    /// this request's `requestId` — matching the Python client. It proves
    /// the command was sent and answered, **not** that audio actually
    /// played; the daemon must still validate the response before reporting
    /// success to its own clients (and remains a placeholder for now).
    pub async fn say(&self, phrase: &str) -> Result<Value, GlagolError> {
        self.send(say_payload(phrase)).await
    }

    /// Close the connection and fail all requests still awaiting responses.
    /// Serialized against [`GlagolClient::connect`]; closing a client that
    /// was never connected (or whose reader already stopped) is a no-op
    /// unless the Station rejected the token.
    pub async fn close(&self) -> Result<(), GlagolError> {
        let _lifecycle = self.shared.lifecycle.lock().await;
        let writer = self.shared.writer.lock().expect("writer lock").take();
        match writer {
            Some(writer) => {
                let _ = writer.send(Writer::Send(Message::Close(None))).await;
                // Dropping the sender unblocks the reader and it drains.
                self.wait_closed().await
            }
            None => {
                // Never connected, or the reader already observed the close.
                if self.shared.close_error.borrow().is_some() {
                    self.wait_closed().await
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Wait until the background reader observed a disconnected socket.
    /// Returns [`GlagolError::InvalidToken`] if the Station rejected the
    /// device token with close code 4000.
    pub async fn wait_closed(&self) -> Result<(), GlagolError> {
        let mut receiver = self.shared.close_error.subscribe();
        loop {
            if let Some(error) = receiver.borrow().as_ref() {
                return if *error == GlagolError::InvalidToken {
                    Err(error.clone())
                } else {
                    Ok(())
                };
            }
            receiver.changed().await.map_err(|_| GlagolError::Closed)?;
        }
    }

    /// Install a new reader over `ws`. The caller must hold the lifecycle
    /// lock (tests attach directly and do so implicitly by being the only
    /// user of the client at that moment).
    fn start_reader<S>(&self, ws: WebSocketStream<S>)
    where
        S: AsyncReadWrite + 'static,
    {
        let generation = self
            .shared
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let (tx, mut rx) = mpsc::channel::<Writer>(16);
        // Requests left over from a superseded connection can never be
        // answered by the new one; fail them instead of letting them hang
        // until their timeout.
        {
            let mut pending = self.shared.pending.lock().expect("pending lock");
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(GlagolError::Closed));
            }
        }
        *self.shared.writer.lock().expect("writer lock") = Some(tx);
        self.shared.close_error.send_replace(None);
        let shared = Arc::clone(&self.shared);
        tokio::spawn(async move {
            reader(shared, ws, &mut rx, generation).await;
        });
    }
}

/// The `serverAction` → `update_form` → `repeat_phrase` payload sent by
/// [`GlagolClient::say`]; shared with the connection manager's trait.
pub(crate) fn say_payload(phrase: &str) -> Value {
    json!({
        "command": "serverAction",
        "serverActionEventPayload": {
            "type": "server_action",
            "name": "update_form",
            "payload": {
                "form_update": {
                    "name": "personal_assistant.scenarios.quasar.iot.repeat_phrase",
                    "slots": [
                        {
                            "type": "string",
                            "name": "phrase_to_repeat",
                            "value": phrase,
                        }
                    ],
                },
                "resubmit": true,
            },
        },
    })
}

/// Background task: forwards outgoing messages, answers heartbeat pings and
/// dispatches responses to pending requests by `requestId`.
///
/// Cleanup applies only while `generation` is still current, so a stale
/// reader winding down after a reconnect cannot clear the new connection's
/// writer, pending requests or `close_error`.
async fn reader<S>(
    shared: Arc<Shared>,
    mut ws: WebSocketStream<S>,
    rx: &mut mpsc::Receiver<Writer>,
    generation: u64,
) where
    S: AsyncReadWrite + 'static,
{
    let mut error = GlagolError::Closed;
    let mut heartbeat = tokio::time::interval(shared.ping_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await; // the first tick completes immediately
    // Set right after a heartbeat ping; if no Pong arrives within
    // `ping_timeout`, the peer counts as unresponsive.
    let mut pong_deadline: Option<Pin<Box<tokio::time::Sleep>>> = None;

    loop {
        tokio::select! {
            message = ws.next() => match message {
                Some(Ok(Message::Text(text))) => dispatch(&shared, text.as_str()),
                Some(Ok(Message::Pong(_))) => pong_deadline = None,
                Some(Ok(Message::Ping(payload))) => {
                    if ws.send(Message::Pong(payload)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    if frame.map(|f| u16::from(&f.code)) == Some(4000) {
                        error = GlagolError::InvalidToken;
                    }
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            outgoing = rx.recv() => match outgoing {
                Some(Writer::Send(Message::Close(frame))) => {
                    let _ = ws.send(Message::Close(frame)).await;
                    break;
                }
                Some(Writer::Send(message)) => {
                    if ws.send(message).await.is_err() {
                        break;
                    }
                }
                None => break, // the client dropped its sender
            },
            _ = heartbeat.tick() => {
                if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
                if pong_deadline.is_none() && shared.ping_timeout > Duration::ZERO {
                    pong_deadline = Some(Box::pin(tokio::time::sleep(shared.ping_timeout)));
                }
            }
            _ = async {
                match pong_deadline.as_mut() {
                    Some(deadline) => deadline.as_mut().await,
                    None => std::future::pending().await,
                }
            } => break, // unresponsive peer: no Pong within ping_timeout
        }
    }

    // Fail everyone still waiting; their responses will never arrive. Only
    // the current generation may clean up shared state.
    if shared.generation.load(std::sync::atomic::Ordering::Relaxed) == generation {
        *shared.writer.lock().expect("writer lock") = None;
        {
            let mut pending = shared.pending.lock().expect("pending lock");
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(error.clone()));
            }
        }
        shared.close_error.send_replace(Some(error));
    }
    let _ = ws.close(None).await;
}
/// Match a text frame against pending requests. Malformed or unmatched
/// frames are ignored.
fn dispatch(shared: &Shared, text: &str) {
    let Ok(response) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let Some(request_id) = response.get("requestId").and_then(Value::as_str) else {
        return;
    };
    if let Some(sender) = shared
        .pending
        .lock()
        .expect("pending lock")
        .remove(request_id)
    {
        let _ = sender.send(Ok(response));
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// rustls configuration for the Station WSS dial.
fn station_tls_config(tls: GlagolTls) -> Result<rustls::ClientConfig, GlagolError> {
    match tls {
        GlagolTls::SystemRoots => {
            let mut roots = rustls::RootCertStore::empty();
            let certs = rustls_native_certs::load_native_certs();
            for cert in certs.certs {
                let _ = roots.add(cert);
            }
            Ok(rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth())
        }
        // Explicit opt-in: a local Station uses a self-signed certificate
        // that cannot be validated against public roots. This verifier is
        // only ever installed for the Station WSS connection, never for
        // Yandex auth HTTP traffic.
        GlagolTls::AcceptSelfSigned => Ok(rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptStationCert))
            .with_no_client_auth()),
    }
}

/// Certificate verifier that accepts everything. Only ever installed for a
/// local Station with a self-signed certificate.
#[derive(Debug)]
struct AcceptStationCert;

impl ServerCertVerifier for AcceptStationCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            ED25519,
            ECDSA_SHA1_Legacy,
            RSA_PKCS1_SHA1,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::DuplexStream;
    use tokio_tungstenite::tungstenite::protocol::Role;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    const TOKEN: &str = "secret-token";

    type MockWs = WebSocketStream<DuplexStream>;

    /// Client connected over an in-memory duplex to a mock Station socket.
    async fn client_and_mock(timeout: Duration, ping_interval: Duration) -> (GlagolClient, MockWs) {
        attach_mock(timeout, ping_interval, DEFAULT_PING_TIMEOUT).await
    }

    /// Client attached to a fresh in-memory mock Station socket.
    async fn attach_mock(
        timeout: Duration,
        ping_interval: Duration,
        ping_timeout: Duration,
    ) -> (GlagolClient, MockWs) {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let client = GlagolClient::with_options(
            "wss://station:1961",
            TOKEN,
            GlagolTls::SystemRoots,
            timeout,
            ping_interval,
            ping_timeout,
        )
        .expect("valid options");
        let ws = WebSocketStream::from_raw_socket(client_side, Role::Client, None).await;
        client.start_reader(ws);
        let server = WebSocketStream::from_raw_socket(server_side, Role::Server, None).await;
        (client, server)
    }

    async fn next_text(server: &mut MockWs) -> Value {
        loop {
            match server.next().await {
                Some(Ok(Message::Text(text))) => {
                    break serde_json::from_str(text.as_str()).expect("mock sent valid JSON");
                }
                Some(Ok(Message::Ping(payload))) => {
                    server.send(Message::Pong(payload)).await.expect("pong");
                }
                Some(Ok(message)) => panic!("unexpected message: {message:?}"),
                other => panic!("unexpected stream event: {other:?}"),
            }
        }
    }

    async fn send_text(server: &mut MockWs, text: &str) {
        server.send(Message::text(text)).await.expect("mock send");
    }

    fn say_envelope(phrase: &str) -> Value {
        json!({
            "command": "serverAction",
            "serverActionEventPayload": {
                "type": "server_action",
                "name": "update_form",
                "payload": {
                    "form_update": {
                        "name": "personal_assistant.scenarios.quasar.iot.repeat_phrase",
                        "slots": [
                            {"type": "string", "name": "phrase_to_repeat", "value": phrase}
                        ],
                    },
                    "resubmit": true,
                },
            },
        })
    }

    #[tokio::test]
    async fn say_sends_repeat_phrase_action_and_returns_response() {
        let (client, mut server) = client_and_mock(DEFAULT_TIMEOUT, DEFAULT_PING_INTERVAL).await;
        let handle = tokio::spawn({
            let client = client.clone();
            async move { client.say("Привет").await }
        });

        let envelope = next_text(&mut server).await;
        assert_eq!(envelope["conversationToken"], TOKEN);
        assert!(Uuid::parse_str(envelope["id"].as_str().unwrap()).is_ok());
        assert!(envelope["sentTime"].as_u64().unwrap_or(0) > 0);
        assert_eq!(envelope["payload"], say_envelope("Привет"));

        let id = envelope["id"].as_str().unwrap().to_owned();
        send_text(
            &mut server,
            &format!(r#"{{"requestId":"{id}","result":"ok"}}"#),
        )
        .await;
        assert_eq!(
            handle.await.expect("say task").expect("say ok")["result"],
            "ok"
        );
        client.close().await.expect("close");
    }

    #[tokio::test]
    async fn envelopes_correlate_out_of_order_responses() {
        let (client, mut server) = client_and_mock(DEFAULT_TIMEOUT, DEFAULT_PING_INTERVAL).await;
        let first = tokio::spawn({
            let client = client.clone();
            async move { client.send(json!({"command": "one"})).await }
        });
        let second = tokio::spawn({
            let client = client.clone();
            async move { client.send(json!({"command": "two"})).await }
        });

        let envelope_one = next_text(&mut server).await;
        let envelope_two = next_text(&mut server).await;
        assert_eq!(envelope_one["conversationToken"], TOKEN);
        assert!(envelope_one["sentTime"].as_u64().unwrap_or(0) > 0);
        assert_ne!(envelope_one["id"], envelope_two["id"]);
        assert_eq!(envelope_one["payload"]["command"], "one");
        assert_eq!(envelope_two["payload"]["command"], "two");

        // Answer the second request first.
        let id_one = envelope_one["id"].as_str().unwrap().to_owned();
        let id_two = envelope_two["id"].as_str().unwrap().to_owned();
        send_text(
            &mut server,
            &format!(r#"{{"requestId":"{id_two}","result":2}}"#),
        )
        .await;
        send_text(
            &mut server,
            &format!(r#"{{"requestId":"{id_one}","result":1}}"#),
        )
        .await;
        assert_eq!(
            first.await.expect("first task").expect("first ok")["result"],
            1
        );
        assert_eq!(
            second.await.expect("second task").expect("second ok")["result"],
            2
        );
        client.close().await.expect("close");
    }

    #[tokio::test]
    async fn timeout_cleans_up_pending_request() {
        let (client, mut server) =
            client_and_mock(Duration::from_millis(50), DEFAULT_PING_INTERVAL).await;
        let error = client
            .send(json!({"command": "silent"}))
            .await
            .expect_err("must time out");
        assert_eq!(error, GlagolError::Timeout);
        assert!(client.shared.pending.lock().unwrap().is_empty());

        // A late response for the abandoned request must not panic.
        send_text(&mut server, r#"{"requestId":"whatever"}"#).await;
        assert_eq!(
            client.send(json!({"command": "next"})).await.unwrap_err(),
            GlagolError::Timeout
        );
        client.close().await.expect("close");
    }

    #[tokio::test]
    async fn close_fails_waiting_request() {
        let (client, _server) = client_and_mock(DEFAULT_TIMEOUT, DEFAULT_PING_INTERVAL).await;
        let request = tokio::spawn({
            let client = client.clone();
            async move { client.send(json!({"command": "pending"})).await }
        });
        // Give the task a chance to register and send.
        tokio::time::sleep(Duration::from_millis(50)).await;
        client.close().await.expect("close");
        assert_eq!(
            request.await.expect("request task").unwrap_err(),
            GlagolError::Closed
        );
        assert_eq!(
            client.send(json!({"command": "again"})).await.unwrap_err(),
            GlagolError::NotConnected
        );
    }

    #[tokio::test]
    async fn close_code_4000_is_invalid_token() {
        let (client, mut server) = client_and_mock(DEFAULT_TIMEOUT, DEFAULT_PING_INTERVAL).await;
        let request = tokio::spawn({
            let client = client.clone();
            async move { client.send(json!({"command": "pending"})).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        server
            .send(Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: CloseCode::from(4000u16),
                    reason: "invalid token".into(),
                },
            )))
            .await
            .expect("mock close");

        assert_eq!(
            request.await.expect("request task").unwrap_err(),
            GlagolError::InvalidToken
        );
        assert_eq!(
            client.wait_closed().await.unwrap_err(),
            GlagolError::InvalidToken
        );
        client
            .close()
            .await
            .expect_err("close after rejection returns InvalidToken");
    }

    #[tokio::test]
    async fn malformed_and_unmatched_responses_are_ignored() {
        let (client, mut server) = client_and_mock(DEFAULT_TIMEOUT, DEFAULT_PING_INTERVAL).await;
        let request = tokio::spawn({
            let client = client.clone();
            async move { client.send(json!({"command": "one"})).await }
        });

        // Server noise: not JSON, non-object JSON, missing requestId,
        // requestId nobody is waiting for.
        send_text(&mut server, "garbage").await;
        send_text(&mut server, "[1,2,3]").await;
        send_text(&mut server, r#"{"result":"no id"}"#).await;
        send_text(&mut server, r#"{"requestId":"unknown","result":"stray"}"#).await;

        let envelope = next_text(&mut server).await;
        let id = envelope["id"].as_str().unwrap().to_owned();
        send_text(
            &mut server,
            &format!(r#"{{"requestId":"{id}","result":"done"}}"#),
        )
        .await;
        assert_eq!(
            request.await.expect("request task").expect("request ok")["result"],
            "done"
        );
        client.close().await.expect("close");
    }

    #[tokio::test]
    async fn heartbeat_pings_and_answers_server_pings() {
        let (client, mut server) =
            client_and_mock(DEFAULT_TIMEOUT, Duration::from_millis(30)).await;

        // Client heartbeat: mock receives a Ping shortly.
        let ping = loop {
            match server.next().await {
                Some(Ok(Message::Ping(_))) => break true,
                Some(Ok(_)) => continue,
                other => panic!("unexpected stream event: {other:?}"),
            }
        };
        assert!(ping);

        // Server heartbeat: client must answer with a Pong.
        server
            .send(Message::Ping(b"hb".to_vec().into()))
            .await
            .expect("mock ping");
        match server.next().await {
            Some(Ok(Message::Pong(payload))) => assert_eq!(&payload[..], b"hb"),
            other => panic!("expected pong, got {other:?}"),
        }
        client.close().await.expect("close");
    }

    #[tokio::test]
    async fn rejects_bad_uri_and_zero_durations() {
        assert_eq!(
            GlagolClient::new("ws://station:1961", TOKEN, GlagolTls::SystemRoots).unwrap_err(),
            GlagolError::InvalidUri
        );
        for (timeout, ping_interval) in [
            (Duration::ZERO, DEFAULT_PING_INTERVAL),
            (DEFAULT_TIMEOUT, Duration::ZERO),
        ] {
            assert_eq!(
                GlagolClient::with_options(
                    "wss://station:1961",
                    TOKEN,
                    GlagolTls::SystemRoots,
                    timeout,
                    ping_interval,
                    DEFAULT_PING_TIMEOUT
                )
                .unwrap_err(),
                GlagolError::InvalidOptions
            );
        }
    }

    #[tokio::test]
    async fn send_without_connect_is_not_connected() {
        let client = GlagolClient::new("wss://station:1961", TOKEN, GlagolTls::SystemRoots)
            .expect("valid options");
        assert_eq!(
            client.send(json!({"command": "x"})).await.unwrap_err(),
            GlagolError::NotConnected
        );
    }

    #[tokio::test]
    async fn aborted_request_does_not_leak_pending_entry() {
        let (client, mut server) = client_and_mock(DEFAULT_TIMEOUT, DEFAULT_PING_INTERVAL).await;
        let task = tokio::spawn({
            let client = client.clone();
            async move { client.send(json!({"command": "abort-me"})).await }
        });

        let envelope = next_text(&mut server).await;
        let id = envelope["id"].as_str().unwrap().to_owned();
        task.abort();
        // Let the runtime drop the aborted future (running its guard).
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!client.shared.pending.lock().unwrap().contains_key(&id));

        // A late response for the cancelled request is ignored silently.
        send_text(
            &mut server,
            &format!(r#"{{"requestId":"{id}","result":"late"}}"#),
        )
        .await;

        // The connection stays usable and leaves nothing behind.
        let handle = tokio::spawn({
            let client = client.clone();
            async move { client.send(json!({"command": "after"})).await }
        });
        let envelope = next_text(&mut server).await;
        let id = envelope["id"].as_str().unwrap().to_owned();
        send_text(
            &mut server,
            &format!(r#"{{"requestId":"{id}","result":"ok"}}"#),
        )
        .await;
        assert_eq!(handle.await.expect("task").expect("ok")["result"], "ok");
        assert!(client.shared.pending.lock().unwrap().is_empty());
        client.close().await.expect("close");
    }

    #[tokio::test]
    async fn unresponsive_peer_fails_requests_via_pong_timeout() {
        let (client, mut server) = attach_mock(
            Duration::from_secs(5),
            Duration::from_millis(30),
            Duration::from_millis(100),
        )
        .await;
        let request = tokio::spawn({
            let client = client.clone();
            async move { client.send(json!({"command": "silent"})).await }
        });

        // Receive the heartbeat ping but never answer it; skip the request
        // envelope that arrives first.
        loop {
            match server.next().await {
                Some(Ok(Message::Ping(_))) => break,
                Some(Ok(_)) => continue,
                other => panic!("expected ping, got {other:?}"),
            }
        }

        // The reader stops without a Pong and fails the pending request
        // well before the request timeout (5 s) elapses.
        let started = tokio::time::Instant::now();
        assert_eq!(
            request.await.expect("request task").unwrap_err(),
            GlagolError::Closed
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(client.shared.pending.lock().unwrap().is_empty());
        assert_eq!(
            client.send(json!({"command": "again"})).await.unwrap_err(),
            GlagolError::NotConnected
        );
        let _ = server.close(None).await;
    }

    #[tokio::test]
    async fn stale_reader_does_not_clobber_newer_connection() {
        let (client_side_a, server_side_a) = tokio::io::duplex(64 * 1024);
        let (client_side_b, server_side_b) = tokio::io::duplex(64 * 1024);
        let client = GlagolClient::new("wss://station:1961", TOKEN, GlagolTls::SystemRoots)
            .expect("valid options");
        let mut mock_a = WebSocketStream::from_raw_socket(server_side_a, Role::Server, None).await;
        let mut mock_b = WebSocketStream::from_raw_socket(server_side_b, Role::Server, None).await;

        // Two overlapping attachments: B supersedes A (generation bumps).
        let ws_a = WebSocketStream::from_raw_socket(client_side_a, Role::Client, None).await;
        client.start_reader(ws_a);
        let ws_b = WebSocketStream::from_raw_socket(client_side_b, Role::Client, None).await;
        client.start_reader(ws_b);

        // The stale connection A closes; its reader must not clear B's
        // writer, pending requests or close_error.
        mock_a
            .send(Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: CloseCode::from(4000u16),
                    reason: "stale".into(),
                },
            )))
            .await
            .expect("mock close");
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Traffic still flows over B, and A's 4000 close must NOT surface.
        let handle = tokio::spawn({
            let client = client.clone();
            async move { client.send(json!({"command": "via-b"})).await }
        });
        let envelope = next_text(&mut mock_b).await;
        assert_eq!(envelope["payload"]["command"], "via-b");
        let id = envelope["id"].as_str().unwrap().to_owned();
        send_text(
            &mut mock_b,
            &format!(r#"{{"requestId":"{id}","result":"ok"}}"#),
        )
        .await;
        assert_eq!(handle.await.expect("task").expect("ok")["result"], "ok");

        client.close().await.expect("close on B");
    }
}
