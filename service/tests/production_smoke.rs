//! Offline production-path smoke test.
//!
//! Drives the real production bindings end to end without network, secrets
//! or a real Station:
//! - `YandexAuth` (reqwest + rustls) against a local mock HTTP server;
//! - `GlagolWsDialer`/`GlagolClient` (tokio-tungstenite + rustls) against a
//!   local mock Station over `wss://` with a self-signed TLS certificate
//!   (static, non-secret test fixture; `GlagolTls::AcceptSelfSigned`);
//! - `ConnectionManager` recovery/token-invalidation semantics;
//! - the Unix socket `Server` behind a real `ConnectionManager`, queried
//!   with the shared `protocol::Client` (the CLI transport).
//!
//! Everything runs on loopback with throwaway values; nothing here touches
//! `~/.config/yandex-stationd/.env`, systemd units or real credentials.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use yandex_tts_auth::{AuthError, YandexAuth};
use yandex_tts_protocol::{Client, Request};
use yandex_ttsd::connection::{ConnectionManager, GlagolWsDialer};
use yandex_ttsd::glagol::GlagolTls;
use yandex_ttsd::server::Server;

const X_TOKEN: &str = "smoke-x-token";
const CLIENT_ID: &str = "smoke-client-id";
const CLIENT_SECRET: &str = "smoke-client-secret";
const MUSIC_TOKEN: &str = "smoke-music-token";
const DEVICE_ID: &str = "smoke-device-1";
const PLATFORM: &str = "yandexstation";

// ----- mock auth HTTP server -----

#[derive(Default)]
struct AuthRecords {
    music: Mutex<Vec<Value>>,
    device: Mutex<Vec<Value>>,
}

impl AuthRecords {
    fn music(&self) -> Vec<Value> {
        self.music.lock().unwrap().clone()
    }

    fn device(&self) -> Vec<Value> {
        self.device.lock().unwrap().clone()
    }
}

struct AuthMock {
    records: Arc<AuthRecords>,
    /// Current Glagol device token handed out by `/glagol/token`.
    device_token: Mutex<String>,
}

impl AuthMock {
    fn new(token: &str) -> Arc<Self> {
        Arc::new(Self {
            records: Arc::default(),
            device_token: Mutex::new(token.to_owned()),
        })
    }

    fn set_device_token(&self, token: &str) {
        *self.device_token.lock().unwrap() = token.to_owned();
    }

    fn device_token(&self) -> String {
        self.device_token.lock().unwrap().clone()
    }
}

/// Reads an HTTP/1.1 request head (and body) from the socket. Returns
/// `(method, path, authorization, body)` or `None` on EOF/parse failure.
async fn read_request(stream: &mut TcpStream) -> Option<(String, String, String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("head terminator");
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut content_length = 0usize;
    let mut authorization = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => content_length = value.parse().unwrap_or(0),
            "authorization" => authorization = value.to_owned(),
            _ => {}
        }
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Some((method, path, authorization, body))
}

async fn respond(stream: &mut TcpStream, status: &str, body: Value) {
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Minimal form/query value lookup (values in this test contain no reserved
/// characters, so exact matching is enough).
fn form_value<'a>(data: &'a str, key: &str) -> Option<&'a str> {
    data.split('&').find_map(|pair| {
        pair.split_once('=')
            .filter(|(name, _)| *name == key)
            .map(|(_, value)| value)
    })
}

async fn spawn_mock_auth() -> (String, Arc<AuthMock>) {
    let mock = AuthMock::new("smoke-device-token-1");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind auth");
    let addr = listener.local_addr().expect("auth addr");
    let server_mock = Arc::clone(&mock);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mock = Arc::clone(&server_mock);
            tokio::spawn(async move {
                let Some((method, path, authorization, body)) = read_request(&mut stream).await
                else {
                    return;
                };
                if path.starts_with("/1/token") && method == "POST" {
                    let body = String::from_utf8_lossy(&body).into_owned();
                    let record = json!({
                        "grant_type": form_value(&body, "grant_type"),
                        "client_id": form_value(&body, "client_id"),
                        "client_secret": form_value(&body, "client_secret"),
                        "access_token": form_value(&body, "access_token"),
                    });
                    mock.records.music.lock().unwrap().push(record);
                    // Rejected x-token: exercise the 401 path too.
                    if form_value(&body, "access_token") == Some("bad-x-token") {
                        respond(&mut stream, "401 Unauthorized", json!({"error": "bad"})).await;
                        return;
                    }
                    let ok = form_value(&body, "grant_type") == Some("x-token")
                        && form_value(&body, "client_id") == Some(CLIENT_ID)
                        && form_value(&body, "client_secret") == Some(CLIENT_SECRET)
                        && form_value(&body, "access_token") == Some(X_TOKEN);
                    if ok {
                        respond(
                            &mut stream,
                            "200 OK",
                            json!({"access_token": MUSIC_TOKEN, "expires_in": 3600}),
                        )
                        .await;
                    } else {
                        respond(&mut stream, "401 Unauthorized", json!({"error": "bad"})).await;
                    }
                } else if path.starts_with("/glagol/token") && method == "GET" {
                    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
                    let record = json!({
                        "authorization": authorization,
                        "device_id": form_value(query, "device_id"),
                        "platform": form_value(query, "platform"),
                    });
                    mock.records.device.lock().unwrap().push(record);
                    let ok = authorization == format!("OAuth {MUSIC_TOKEN}")
                        && form_value(query, "device_id") == Some(DEVICE_ID)
                        && form_value(query, "platform") == Some(PLATFORM);
                    if ok {
                        respond(
                            &mut stream,
                            "200 OK",
                            json!({"token": mock.device_token(), "expires_in": 3600}),
                        )
                        .await;
                    } else {
                        respond(&mut stream, "403 Forbidden", json!({"error": "bad"})).await;
                    }
                } else {
                    respond(&mut stream, "404 Not Found", json!({"error": "no route"})).await;
                }
            });
        }
    });
    (format!("http://{addr}"), mock)
}

// ----- mock Station (self-signed TLS WSS) -----

/// Commands sent to every live mock Station connection.
const KILL_CONNECTION: u16 = 1;
const REJECT_TOKEN: u16 = 4000;

struct StationMock {
    envelopes: Mutex<Vec<Value>>,
    connections: AtomicUsize,
    silent: AtomicBool,
    commands: broadcast::Sender<u16>,
}

impl StationMock {
    fn new() -> Arc<Self> {
        let (commands, _) = broadcast::channel(64);
        Arc::new(Self {
            envelopes: Mutex::new(Vec::new()),
            connections: AtomicUsize::new(0),
            silent: AtomicBool::new(false),
            commands,
        })
    }

    fn envelopes(&self) -> Vec<Value> {
        self.envelopes.lock().unwrap().clone()
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn set_silent(&self, silent: bool) {
        self.silent.store(silent, Ordering::SeqCst);
    }

    /// Abruptly drop every live connection (simulates a Station crash).
    fn drop_connections(&self) {
        let _ = self.commands.send(KILL_CONNECTION);
    }

    /// Close every connection with code 4000 (token rejected).
    fn reject_token(&self) {
        let _ = self.commands.send(REJECT_TOKEN);
    }
}

async fn spawn_mock_station() -> (String, Arc<StationMock>) {
    let mock = StationMock::new();
    let cert = CertificateDer::from(include_bytes!("fixtures/test_cert.der").to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        include_bytes!("fixtures/test_key_pkcs8.der").to_vec(),
    ));
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("test TLS fixture is valid");
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind station");
    let addr = listener.local_addr().expect("station addr");
    let server_mock = Arc::clone(&mock);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let Ok(tls) = acceptor.accept(stream).await else {
                continue;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(tls).await else {
                continue;
            };
            let mock = Arc::clone(&server_mock);
            mock.connections.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(handle_station_connection(ws, mock));
        }
    });
    (format!("wss://{addr}"), mock)
}

async fn handle_station_connection(
    ws: WebSocketStream<tokio_rustls::server::TlsStream<TcpStream>>,
    mock: Arc<StationMock>,
) {
    let (mut sink, mut stream) = ws.split();
    let mut commands = mock.commands.subscribe();
    loop {
        tokio::select! {
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let Ok(envelope) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    mock.envelopes.lock().unwrap().push(envelope.clone());
                    if mock.silent.load(Ordering::SeqCst) {
                        continue;
                    }
                    let id = envelope["id"].as_str().unwrap_or_default().to_owned();
                    let reply = json!({"requestId": id, "status": "ok"});
                    if sink.send(Message::text(reply.to_string())).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    if sink.send(Message::Pong(payload)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            command = commands.recv() => match command {
                Ok(REJECT_TOKEN) => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code: REJECT_TOKEN.into(),
                            reason: "invalid token".into(),
                        })))
                        .await;
                    break;
                }
                Ok(_) => break, // abrupt drop: sink is dropped without a Close frame
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    let _ = sink.close().await;
}

// ----- helpers -----

async fn wait_until<F: Fn() -> bool>(condition: F, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn phrase_envelopes(envelopes: &[Value], phrase: &str) -> Vec<Value> {
    envelopes
        .iter()
        .filter(|envelope| {
            envelope["payload"]["serverActionEventPayload"]["payload"]["form_update"]["slots"]
                .as_array()
                .is_some_and(|slots| {
                    slots
                        .iter()
                        .any(|slot| slot["name"] == "phrase_to_repeat" && slot["value"] == phrase)
                })
        })
        .cloned()
        .collect()
}

// ----- the smoke test -----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_path_offline_smoke() {
    // The dependency graph carries two rustls providers (aws-lc-rs via
    // reqwest, ring via tokio-tungstenite); production code installs ring
    // before dialing. The mock Station's server-side TLS config needs the
    // same guarantee, so install it before anything builds a config.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (auth_base, auth_mock) = spawn_mock_auth().await;
    let (station_uri, station) = spawn_mock_station().await;

    // Real production bindings: YandexAuth against the mock HTTP endpoints
    // and a real WSS dialer with the production self-signed policy.
    let auth = YandexAuth::with_options(
        X_TOKEN,
        DEVICE_ID,
        PLATFORM,
        CLIENT_ID,
        CLIENT_SECRET,
        format!("{auth_base}/glagol/token"),
        format!("{auth_base}/1/token"),
        Duration::from_secs(5),
    )
    .expect("valid auth config");
    let dialer = GlagolWsDialer::with_options(
        station_uri,
        GlagolTls::AcceptSelfSigned,
        Duration::from_millis(500),
        Duration::from_millis(100),
        Duration::from_millis(500),
    );
    let manager = ConnectionManager::with_jitter(
        auth,
        dialer,
        Duration::from_millis(20),
        Duration::from_millis(200),
        Duration::from_millis(120),
        |_| Duration::ZERO,
    )
    .expect("valid manager config");

    // Ping readiness: background recovery connects over real WSS + TLS.
    assert!(!manager.connected());
    manager.start().expect("manager starts");
    wait_until(|| manager.connected(), "manager becomes connected").await;

    // A rejected x-token surfaces as the proper auth error (401 path).
    let rejected = YandexAuth::with_options(
        "bad-x-token",
        DEVICE_ID,
        PLATFORM,
        CLIENT_ID,
        CLIENT_SECRET,
        format!("{auth_base}/glagol/token"),
        format!("{auth_base}/1/token"),
        Duration::from_secs(5),
    )
    .expect("valid auth config");
    assert_eq!(
        rejected.get_device_token(true).await.unwrap_err(),
        AuthError::XTokenUnauthorized
    );

    // say through the manager: real envelope over real TLS, correlated reply.
    manager
        .say_within(Some(Duration::from_secs(3)), "Привет")
        .await
        .expect("say through production path");

    // Auth requests hit the mock endpoints with the exact production shape.
    let music = auth_mock.records.music();
    assert_eq!(music.len(), 2, "music token fetched once per x-token");
    assert_eq!(music[0]["grant_type"], "x-token");
    assert_eq!(music[0]["client_id"], CLIENT_ID);
    assert_eq!(music[0]["client_secret"], CLIENT_SECRET);
    assert_eq!(music[0]["access_token"], X_TOKEN);
    assert_eq!(music[1]["access_token"], "bad-x-token");
    let device = auth_mock.records.device();
    assert_eq!(device.len(), 1, "device token fetched once and cached");
    assert_eq!(device[0]["authorization"], format!("OAuth {MUSIC_TOKEN}"));
    assert_eq!(device[0]["device_id"], DEVICE_ID);
    assert_eq!(device[0]["platform"], PLATFORM);

    // The envelope carries the device token, a UUID id, sentTime and the
    // exact say payload.
    let envelopes = station.envelopes();
    assert_eq!(envelopes.len(), 1);
    assert_eq!(envelopes[0]["conversationToken"], "smoke-device-token-1");
    let id = envelopes[0]["id"].as_str().expect("envelope id");
    assert!(uuid::Uuid::parse_str(id).is_ok(), "id is a UUID");
    assert!(envelopes[0]["sentTime"].as_u64().unwrap_or(0) > 0);
    assert_eq!(
        phrase_envelopes(&envelopes, "Привет").len(),
        1,
        "say payload observed by the mock Station"
    );

    // Recovery: the Station crashes (abrupt drop); the manager reconnects.
    let connections_before = station.connections();
    station.drop_connections();
    wait_until(
        || station.connections() > connections_before,
        "manager re-dials after the Station crash",
    )
    .await;
    wait_until(|| manager.connected(), "manager is connected again").await;
    manager
        .say_within(Some(Duration::from_secs(3)), "после обрыва")
        .await
        .expect("say works after recovery");

    // Token invalidation: the Station rejects the token with close 4000,
    // the manager invalidates and re-fetches the rotated token via auth.
    let connections_before = station.connections();
    auth_mock.set_device_token("smoke-device-token-2");
    station.reject_token();
    wait_until(
        || station.connections() > connections_before,
        "manager re-dials after the 4000 rejection",
    )
    .await;
    manager
        .say_within(Some(Duration::from_secs(3)), "после ротации")
        .await
        .expect("say works with the rotated token");
    let last = station.envelopes().pop().expect("envelope after rotation");
    assert_eq!(
        last["conversationToken"], "smoke-device-token-2",
        "the rotated device token is used after invalidation"
    );

    // Unix socket server in front of the real manager, queried through the
    // shared protocol client (the CLI transport).
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("smoke.sock");
    let mut server = Server::new(&socket_path, Arc::new(manager.clone()));
    server.start().await.expect("server binds the socket");
    let stop_handle = server.stop_handle();
    let serve = tokio::spawn(async move {
        server.serve_forever().await.expect("server serves");
    });
    wait_until(|| socket_path.exists(), "socket file is published").await;

    let mut client = Client::connect(&socket_path).expect("client connects");
    assert_eq!(
        client.request(&Request::Ping).expect("ping reply"),
        yandex_tts_protocol::Response::Connected(true),
        "ping reports real readiness"
    );
    assert_eq!(
        client
            .request(&Request::Say("через сокет".to_owned()))
            .expect("say reply"),
        yandex_tts_protocol::Response::Ok,
        "say is confirmed by the mock Station before ok"
    );
    assert_eq!(
        client.request(&Request::Ping).expect("ping reply"),
        yandex_tts_protocol::Response::Connected(true)
    );

    // No replay: a command sent while the Station stays silent fails and is
    // never re-sent by the recovery loop.
    station.set_silent(true);
    let error = manager
        .say_within(Some(Duration::from_secs(3)), "не повторять")
        .await
        .expect_err("say without an answer must fail");
    let rendered = format!("{error:?}");
    assert!(
        !rendered.contains("не повторять") && !rendered.contains("smoke-device-token"),
        "errors never carry TTS text or tokens: {rendered}"
    );
    station.set_silent(false);
    // Wait out the rebuild triggered by the unknown-outcome failure.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let replayed = phrase_envelopes(&station.envelopes(), "не повторять");
    assert_eq!(replayed.len(), 1, "the failed command is never replayed");
    // The manager recovers and serves further commands.
    wait_until(|| manager.connected(), "manager recovers after rebuild").await;
    manager
        .say_within(Some(Duration::from_secs(3)), "финал")
        .await
        .expect("say works after the rebuild");

    // Shutdown: server stops accepting, removes its socket; manager closes.
    stop_handle.notify_one();
    tokio::time::timeout(Duration::from_secs(5), serve)
        .await
        .expect("server stops")
        .expect("serve task");
    assert!(!socket_path.exists(), "socket file is removed on shutdown");
    assert!(Client::connect(&socket_path).is_err(), "socket is gone");
    manager.close().await;
    assert!(!manager.connected());
    let station_connections = station.connections();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        station.connections(),
        station_connections,
        "no reconnects after shutdown"
    );
}
