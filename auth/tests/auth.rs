//! Port of the Python `tests/test_auth.py` semantics against a local mock HTTP
//! server (no real network, no real secrets).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use yandex_tts_auth::{AuthError, YandexAuth};

/// A scripted response: (status, extra headers, body).
type ScriptedResponse = (u16, Vec<(&'static str, String)>, String);

/// A request captured by the mock server.
#[derive(Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A scripted mock server: one JSON response per incoming request, in order.
struct MockServer {
    addr: String,
    handle: tokio::task::JoinHandle<()>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockServer {
    /// `responses`: (status, extra headers, JSON body) applied per request.
    async fn start(responses: Vec<ScriptedResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handle = {
            let requests = Arc::clone(&requests);
            tokio::spawn(async move {
                for (status, headers, body) in responses {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let request = read_request(&mut stream).await;
                    stream
                        .write_all(&http_response(status, &headers, &body))
                        .await
                        .expect("write");
                    stream.shutdown().await.expect("shutdown");
                    requests.lock().expect("requests lock").push(request);
                }
            })
        };
        Self {
            addr,
            handle,
            requests,
        }
    }

    fn auth(&self, x_token: &str, device_id: &str, platform: &str) -> YandexAuth {
        YandexAuth::with_options(
            x_token,
            device_id,
            platform,
            TEST_MUSIC_CLIENT_ID,
            TEST_MUSIC_CLIENT_SECRET,
            format!("http://{}/glagol/token", self.addr),
            format!("http://{}/1/token", self.addr),
            Duration::from_secs(5),
        )
        .expect("auth")
    }

    /// Consume the server; returns the recorded requests in arrival order.
    async fn finish(self) -> Vec<RecordedRequest> {
        self.handle.await.expect("server ended cleanly");
        Arc::try_unwrap(self.requests)
            .expect("sole owner")
            .into_inner()
            .expect("requests lock")
    }
}

/// Consume the request (kept simple: mock always reads to end of body).
async fn read_request(stream: &mut TcpStream) -> RecordedRequest {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await.expect("read");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_headers_end(&buf) {
            if let Some(len) = content_length(&buf[..pos]) {
                if buf.len() >= pos + 4 + len {
                    break;
                }
            } else {
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .map(|(head, body)| (head.to_string(), body.to_string()))
        .unwrap_or((text, String::new()));
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();
    RecordedRequest {
        method,
        path,
        headers,
        body,
    }
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(head: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(head);
    for line in text.lines() {
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

fn http_response(status: u16, headers: &[(&str, String)], body: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        302 => "Found",
        401 => "Unauthorized",
        403 => "Forbidden",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

fn music_ok() -> ScriptedResponse {
    (
        200,
        vec![],
        r#"{"access_token":"music-token","expires_in":3600}"#.to_string(),
    )
}

fn device_ok(token: &str) -> ScriptedResponse {
    (200, vec![], format!(r#"{{"token":"{token}"}}"#))
}

fn err(status: u16) -> ScriptedResponse {
    (status, vec![], r#"{"error":"rejected"}"#.to_string())
}

/// Fake OAuth client credentials, as a daemon would supply from its config.
/// Tests must not rely on any embedded or production fallback values.
const TEST_MUSIC_CLIENT_ID: &str = "test-client-id";
const TEST_MUSIC_CLIENT_SECRET: &str = "test-client-secret";

/// Asserts over the recorded Music-token exchange. Panic messages never
/// include the raw request, so tokens cannot leak through test output.
fn assert_music_exchange(x_token: &str, request: &RecordedRequest) {
    assert_eq!(request.method, "POST", "music token must be POSTed");
    assert_eq!(
        request.path, "/1/token",
        "music token must hit the token endpoint"
    );
    assert_eq!(
        request.header("Content-Type"),
        Some("application/x-www-form-urlencoded"),
        "music token must be sent as a form"
    );
    let expected = format!(
        "client_id={TEST_MUSIC_CLIENT_ID}&client_secret={TEST_MUSIC_CLIENT_SECRET}&grant_type=x-token&access_token={}",
        urlencode(x_token)
    );
    assert!(
        request.body == expected,
        "music token form body mismatch (client_id/grant_type/access_token)"
    );
    assert!(
        !request
            .header("Authorization")
            .is_some_and(|v| !v.is_empty()),
        "music exchange must not carry an Authorization header"
    );
}

/// Asserts over the recorded Glagol device-token request.
fn assert_glagol_request(
    device_id: &str,
    platform: &str,
    music_token: &str,
    request: &RecordedRequest,
) {
    assert_eq!(request.method, "GET", "device token must be GET");
    assert_eq!(
        request.path,
        format!(
            "/glagol/token?device_id={}&platform={}",
            urlencode(device_id),
            urlencode(platform)
        ),
        "device token query must carry url-encoded device_id/platform"
    );
    assert_eq!(
        request.header("Authorization"),
        Some("OAuth music-token"),
        "device token must use OAuth music token"
    );
    assert!(
        !request.body.to_lowercase().contains("token"),
        "device token request must not carry a token in the body"
    );
    let _ = music_token;
}

/// Minimal percent-encoding matching the crate under test.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[tokio::test]
async fn fetches_and_caches_device_token() {
    let server = MockServer::start(vec![music_ok(), device_ok("device-token")]).await;
    let auth = server.auth("oauth-token", "device-id", "yandexstation");

    let first = auth.get_device_token(false).await.expect("first");
    let second = auth.get_device_token(false).await.expect("second");
    assert_eq!(first, "device-token");
    assert_eq!(second, "device-token");

    // Server scripts exactly two responses; a third request would fail the
    // accept/serve loop, so reaching here proves caching worked.
    drop(auth);
    let requests = server.finish().await;
    assert_eq!(requests.len(), 2, "exactly two requests expected");
    assert_music_exchange("oauth-token", &requests[0]);
    assert_glagol_request("device-id", "yandexstation", "music-token", &requests[1]);
}

#[tokio::test]
async fn glagol_query_is_url_encoded() {
    let server = MockServer::start(vec![music_ok(), device_ok("device-token")]).await;
    let auth = server.auth("oauth-token", "device id+/&", "yandex station");
    auth.get_device_token(false).await.expect("token");

    drop(auth);
    let requests = server.finish().await;
    assert_eq!(requests.len(), 2, "exactly two requests expected");
    assert_glagol_request(
        "device id+/&",
        "yandex station",
        "music-token",
        &requests[1],
    );
}

#[tokio::test]
async fn no_secrets_in_captured_traffic_headers_or_errors() {
    let server = MockServer::start(vec![err(401)]).await;
    let auth = server.auth("secret-x-token", "device-id", "platform");
    let error = auth.get_device_token(false).await.expect_err("typed");
    assert_eq!(error, AuthError::XTokenUnauthorized);
    let rendered = error.to_string();
    assert!(!rendered.contains("secret-x-token"), "no x-token in error");
    assert!(
        !rendered.to_lowercase().contains("token value"),
        "no token value"
    );

    drop(auth);
    let requests = server.finish().await;
    assert_eq!(requests.len(), 1, "only the music exchange expected");
}

#[tokio::test]
async fn invalidated_token_is_refreshed_once_for_parallel_requests() {
    let server = MockServer::start(vec![music_ok(), device_ok("first"), device_ok("second")]).await;
    let auth = server.auth("oauth-token", "device-id", "platform");
    let first = auth.get_device_token(false).await.expect("initial");
    assert_eq!(first, "first");

    auth.invalidate_device_token().await;
    let (a, b) = tokio::join!(auth.get_device_token(false), auth.get_device_token(false));
    assert_eq!(a.expect("a"), "second");
    assert_eq!(b.expect("b"), "second");

    drop(auth);
    let requests = server.finish().await;
    assert_eq!(
        requests.len(),
        3,
        "music token must be reused, no extra exchange"
    );
    assert_music_exchange("oauth-token", &requests[0]);
}

#[tokio::test]
async fn http_authentication_errors_are_typed() {
    let cases = [
        (401u16, AuthError::MusicUnauthorized),
        (403, AuthError::DeviceForbidden),
    ];
    for (status, expected) in cases {
        let server = MockServer::start(vec![music_ok(), err(status)]).await;
        let auth = server.auth("oauth-token", "device-id", "platform");
        let error = auth.get_device_token(false).await.expect_err("typed error");
        assert_eq!(error, expected);
    }
}

#[tokio::test]
async fn rate_limit_exposes_retry_after() {
    let server = MockServer::start(vec![(
        429,
        vec![("Retry-After", "12".to_string())],
        "{}".to_string(),
    )])
    .await;
    let auth = server.auth("oauth-token", "device-id", "platform");
    match auth.get_device_token(false).await {
        Err(AuthError::RateLimit {
            retry_after: Some(d),
        }) => assert_eq!(d, Duration::from_secs(12)),
        other => panic!("expected rate limit, got {other:?}"),
    }
}

#[tokio::test]
async fn x_token_errors_surface_when_music_exchange_fails() {
    let server = MockServer::start(vec![err(401)]).await;
    let auth = server.auth("oauth-token", "device-id", "platform");
    let error = auth.get_device_token(false).await.expect_err("typed");
    assert_eq!(error, AuthError::XTokenUnauthorized);
    assert!(!error.to_string().contains("oauth-token"));
}

#[tokio::test]
async fn redirects_are_not_followed_for_either_endpoint() {
    // 302 on the glagol request: must surface as Http{302}, no follow-up call.
    let server = MockServer::start(vec![
        music_ok(),
        (
            302,
            vec![("Location", "http://evil.example/glagol/token".to_string())],
            String::new(),
        ),
    ])
    .await;
    let auth = server.auth("oauth-token", "device-id", "platform");
    let error = auth.get_device_token(false).await.expect_err("no redirect");
    assert_eq!(error, AuthError::Http { status: 302 });

    drop(auth);
    let requests = server.finish().await;
    assert_eq!(requests.len(), 2, "redirect must not be followed");
    assert_glagol_request("device-id", "platform", "music-token", &requests[1]);
}

#[tokio::test]
async fn redirects_are_not_followed_for_music_exchange() {
    // 302 on the music exchange itself: no OAuth credentials may reach the
    // redirect target and the error must be typed Http{302}.
    let server = MockServer::start(vec![(
        302,
        vec![("Location", "http://evil.example/1/token".to_string())],
        String::new(),
    )])
    .await;
    let auth = server.auth("oauth-token", "device-id", "platform");
    let error = auth.get_device_token(false).await.expect_err("no redirect");
    assert_eq!(error, AuthError::Http { status: 302 });

    drop(auth);
    let requests = server.finish().await;
    assert_eq!(requests.len(), 1, "redirect must not be followed");
}

#[tokio::test]
async fn music_token_is_reused_across_device_refreshes() {
    let server = MockServer::start(vec![music_ok(), device_ok("first"), device_ok("second")]).await;
    let auth = server.auth("oauth-token", "device-id", "platform");
    assert_eq!(auth.get_device_token(false).await.expect("first"), "first");
    auth.invalidate_device_token().await;
    assert_eq!(
        auth.get_device_token(false).await.expect("second"),
        "second"
    );

    drop(auth);
    server.finish().await;
}

#[tokio::test]
async fn expired_cache_is_refreshed() {
    // Music token expires immediately (expires_at in the past); device token
    // has expires_at in the past too, so both are refetched on the second call.
    let server = MockServer::start(vec![
        (
            200,
            vec![],
            r#"{"access_token":"music-1","expires_in":0}"#.to_string(),
        ),
        (
            200,
            vec![],
            r#"{"token":"device-1","expires_at":1}"#.to_string(),
        ),
        (
            200,
            vec![],
            r#"{"access_token":"music-2","expires_in":3600}"#.to_string(),
        ),
        (
            200,
            vec![],
            r#"{"token":"device-2","expires_at":4000000000}"#.to_string(),
        ),
    ])
    .await;
    let auth = server.auth("oauth-token", "device-id", "platform");
    assert_eq!(
        auth.get_device_token(false).await.expect("first"),
        "device-1"
    );
    assert_eq!(
        auth.get_device_token(false).await.expect("second"),
        "device-2"
    );

    drop(auth);
    server.finish().await;
}

#[tokio::test]
async fn jwt_fallback_expiry_caches_token() {
    // Device token is a JWT whose exp is far in the future; no explicit expiry
    // fields. Second call must be served from cache (only 2 responses scripted).
    let payload = base64_url(br#"{"exp":4000000000}"#);
    let jwt = format!("h.{payload}.s");
    let server = MockServer::start(vec![music_ok(), device_ok(&jwt)]).await;
    let auth = server.auth("oauth-token", "device-id", "platform");
    assert_eq!(auth.get_device_token(false).await.expect("first"), jwt);
    assert_eq!(auth.get_device_token(false).await.expect("second"), jwt);

    drop(auth);
    server.finish().await;
}

#[test]
fn constructor_rejects_empty_inputs() {
    let mk = YandexAuth::with_options;
    let cases: [(bool, &str, &str); 3] = [(true, "d", "p"), (false, "", "p"), (false, "d", "")];
    for (ok_x, device, platform) in cases {
        let x = if ok_x { "x" } else { "" };
        let result = mk(
            x,
            device,
            platform,
            TEST_MUSIC_CLIENT_ID,
            TEST_MUSIC_CLIENT_SECRET,
            "e",
            "m",
            Duration::from_secs(1),
        );
        if ok_x {
            assert!(result.is_ok(), "valid inputs must be accepted");
        } else {
            assert_eq!(result.err().expect("invalid"), AuthError::Invalid);
        }
    }
}

#[test]
fn constructor_rejects_empty_music_client_credentials() {
    let result = YandexAuth::with_options(
        "x",
        "d",
        "p",
        "",
        TEST_MUSIC_CLIENT_SECRET,
        "e",
        "m",
        Duration::from_secs(1),
    );
    assert_eq!(result.err().expect("invalid"), AuthError::Invalid);
    let result = YandexAuth::with_options(
        "x",
        "d",
        "p",
        TEST_MUSIC_CLIENT_ID,
        "",
        "e",
        "m",
        Duration::from_secs(1),
    );
    assert_eq!(result.err().expect("invalid"), AuthError::Invalid);
}

#[test]
fn constructor_rejects_zero_timeout_with_valid_inputs() {
    let result = YandexAuth::with_options(
        "x",
        "d",
        "p",
        TEST_MUSIC_CLIENT_ID,
        TEST_MUSIC_CLIENT_SECRET,
        "e",
        "m",
        Duration::ZERO,
    );
    assert_eq!(result.err().expect("invalid"), AuthError::Invalid);
}

#[tokio::test]
async fn failed_music_exchange_error_does_not_contain_client_credentials() {
    let server = MockServer::start(vec![err(401)]).await;
    let auth = server.auth("secret-x-token", "device-id", "platform");
    let error = auth.get_device_token(false).await.expect_err("typed");
    assert_eq!(error, AuthError::XTokenUnauthorized);
    let rendered = format!("{error:?}: {error}");
    assert!(
        !rendered.contains(TEST_MUSIC_CLIENT_SECRET),
        "no music client secret in error"
    );
    assert!(
        !rendered.contains(TEST_MUSIC_CLIENT_ID),
        "no music client id in error"
    );

    drop(auth);
    server.finish().await;
}

fn base64_url(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

// base64 is only needed by this test file.
use base64 as _;
