//! Yandex authentication for the Glagol API.
//!
//! Exchanges an x-token for a Yandex Music OAuth token and uses it to fetch
//! per-device Glagol tokens, caching both until shortly before expiry.
//! Endpoints are injectable so tests can run against a local mock server
//! without secrets or network access.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use tokio::sync::Mutex as AsyncMutex;

pub const GLAGOL_TOKEN_URL: &str = "https://quasar.yandex.net/glagol/token";
pub const MUSIC_TOKEN_URL: &str = "https://oauth.mobile.yandex.net/1/token";

const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// The Yandex API rejected or could not process an authentication request.
///
/// Error messages never contain tokens or other secrets.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// The configured Yandex x-token is invalid or expired.
    #[error("Yandex x-token was rejected (HTTP 401)")]
    XTokenUnauthorized,
    /// The Yandex Music token was rejected.
    #[error("Yandex Music token was rejected (HTTP 401)")]
    MusicUnauthorized,
    /// Yandex denied access to the Glagol API.
    #[error("Yandex denied access to the Glagol API (HTTP 403)")]
    Forbidden,
    /// Yandex denied access to the requested device.
    #[error("Yandex denied access to the device token (HTTP 403)")]
    DeviceForbidden,
    /// Yandex temporarily refused further token requests.
    #[error("Yandex token request is rate limited (HTTP 429)")]
    RateLimit { retry_after: Option<Duration> },
    /// A non-2xx response other than 401/403/429.
    #[error("Yandex token request failed with HTTP {status}")]
    Http { status: u16 },
    /// The request could not be completed (network, timeout).
    #[error("Yandex token request failed")]
    Transport,
    /// Inputs were invalid or the response was not the expected JSON.
    #[error("Yandex auth configuration or response is invalid")]
    Invalid,
}

#[derive(Debug, Default)]
struct MusicCache {
    token: Option<String>,
    expires_at: Option<SystemTime>,
}

impl MusicCache {
    fn valid(&self, now: SystemTime) -> bool {
        match (&self.token, self.expires_at) {
            (Some(_), Some(exp)) => now < exp.checked_sub(EXPIRY_MARGIN).unwrap_or(UNIX_EPOCH),
            (Some(_), None) => true,
            (None, _) => false,
        }
    }
}

struct State {
    music: MusicCache,
    device_token: Option<String>,
    device_expires_at: Option<SystemTime>,
}

/// Fetch and cache the device token used by a local Glagol connection.
#[derive(Clone)]
pub struct YandexAuth {
    inner: Arc<Inner>,
}

struct Inner {
    x_token: String,
    device_id: String,
    platform: String,
    music_client_id: String,
    music_client_secret: String,
    endpoint: String,
    music_token_endpoint: String,
    client: reqwest::Client,
    state: AsyncMutex<State>,
}

impl YandexAuth {
    /// Create an authenticator with the production endpoints and a 10 s timeout.
    ///
    /// `music_client_id` and `music_client_secret` are the Yandex Music OAuth
    /// client credentials; they must come from daemon-side configuration (see
    /// `YANDEX_MUSIC_CLIENT_ID` / `YANDEX_MUSIC_CLIENT_SECRET` in
    /// `.env.example`). There is no embedded fallback.
    pub fn new(
        x_token: impl Into<String>,
        device_id: impl Into<String>,
        platform: impl Into<String>,
        music_client_id: impl Into<String>,
        music_client_secret: impl Into<String>,
    ) -> Result<Self, AuthError> {
        Self::with_options(
            x_token,
            device_id,
            platform,
            music_client_id,
            music_client_secret,
            GLAGOL_TOKEN_URL,
            MUSIC_TOKEN_URL,
            Duration::from_secs(10),
        )
    }

    /// Create an authenticator with explicit endpoints and timeout (for tests).
    #[allow(clippy::too_many_arguments)]
    pub fn with_options(
        x_token: impl Into<String>,
        device_id: impl Into<String>,
        platform: impl Into<String>,
        music_client_id: impl Into<String>,
        music_client_secret: impl Into<String>,
        endpoint: impl Into<String>,
        music_token_endpoint: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, AuthError> {
        let x_token = x_token.into();
        if x_token.is_empty() {
            return Err(AuthError::Invalid);
        }
        let device_id = device_id.into();
        if device_id.is_empty() {
            return Err(AuthError::Invalid);
        }
        let platform = platform.into();
        if platform.is_empty() {
            return Err(AuthError::Invalid);
        }
        let music_client_id = music_client_id.into();
        if music_client_id.is_empty() {
            return Err(AuthError::Invalid);
        }
        let music_client_secret = music_client_secret.into();
        if music_client_secret.is_empty() {
            return Err(AuthError::Invalid);
        }
        if timeout.is_zero() {
            return Err(AuthError::Invalid);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| AuthError::Transport)?;
        Ok(Self {
            inner: Arc::new(Inner {
                x_token,
                device_id,
                platform,
                music_client_id,
                music_client_secret,
                endpoint: endpoint.into(),
                music_token_endpoint: music_token_endpoint.into(),
                client,
                state: AsyncMutex::new(State {
                    music: MusicCache::default(),
                    device_token: None,
                    device_expires_at: None,
                }),
            }),
        })
    }

    /// Return a cached device token, fetching one only when necessary.
    ///
    /// Concurrent callers share a single refresh: whoever holds the state lock
    /// fetches while the others wait and then reuse the fresh cache.
    pub async fn get_device_token(&self, force_refresh: bool) -> Result<String, AuthError> {
        let inner = &self.inner;
        let mut state = inner.state.lock().await;
        if !force_refresh && let Some(token) = &state.device_token {
            let fresh = state.device_expires_at.is_none_or(|expires_at| {
                let margin = expires_at.checked_sub(EXPIRY_MARGIN).unwrap_or(UNIX_EPOCH);
                SystemTime::now() < margin
            });
            if fresh {
                return Ok(token.clone());
            }
        }
        let (token, expires_at) = self.request_device_token(&mut state).await?;
        state.device_token = Some(token.clone());
        state.device_expires_at = expires_at;
        Ok(token)
    }

    /// Discard a rejected token so the next request obtains a new one.
    pub async fn invalidate_device_token(&self) {
        let mut state = self.inner.state.lock().await;
        state.device_token = None;
        state.device_expires_at = None;
    }

    async fn request_device_token(
        &self,
        state: &mut State,
    ) -> Result<(String, Option<SystemTime>), AuthError> {
        let music_token = self.get_music_token(state).await?;
        let url = format!(
            "{}?device_id={}&platform={}",
            self.inner.endpoint,
            urlencode(&self.inner.device_id),
            urlencode(&self.inner.platform)
        );
        let response = self
            .inner
            .client
            .get(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("OAuth {music_token}"),
            )
            .send()
            .await
            .map_err(|_| AuthError::Transport)?;

        let status = response.status();
        match status {
            reqwest::StatusCode::UNAUTHORIZED => return Err(AuthError::MusicUnauthorized),
            reqwest::StatusCode::FORBIDDEN => return Err(AuthError::DeviceForbidden),
            reqwest::StatusCode::TOO_MANY_REQUESTS => {
                return Err(AuthError::RateLimit {
                    retry_after: parse_retry_after(response.headers()),
                });
            }
            status if !status.is_success() => {
                return Err(AuthError::Http {
                    status: status.as_u16(),
                });
            }
            _ => {}
        }

        let payload: serde_json::Value = response.json().await.map_err(|_| AuthError::Invalid)?;
        let token = payload
            .get("token")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
            .ok_or(AuthError::Invalid)?
            .to_string();
        let expires_at = token_expiry(&payload, &token);
        Ok((token, expires_at))
    }

    async fn get_music_token(&self, state: &mut State) -> Result<String, AuthError> {
        if state.music.valid(SystemTime::now()) {
            return Ok(state.music.token.clone().expect("valid implies present"));
        }
        let response = self
            .inner
            .client
            .post(&self.inner.music_token_endpoint)
            .form(&[
                ("client_id", self.inner.music_client_id.as_str()),
                ("client_secret", self.inner.music_client_secret.as_str()),
                ("grant_type", "x-token"),
                ("access_token", self.inner.x_token.as_str()),
            ])
            .send()
            .await
            .map_err(|_| AuthError::Transport)?;

        let status = response.status();
        match status {
            reqwest::StatusCode::UNAUTHORIZED => return Err(AuthError::XTokenUnauthorized),
            reqwest::StatusCode::FORBIDDEN => return Err(AuthError::Forbidden),
            reqwest::StatusCode::TOO_MANY_REQUESTS => {
                return Err(AuthError::RateLimit {
                    retry_after: parse_retry_after(response.headers()),
                });
            }
            status if !status.is_success() => {
                return Err(AuthError::Http {
                    status: status.as_u16(),
                });
            }
            _ => {}
        }

        let payload: serde_json::Value = response.json().await.map_err(|_| AuthError::Invalid)?;
        let token = payload
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
            .ok_or(AuthError::Invalid)?
            .to_string();
        state.music = MusicCache {
            expires_at: token_expiry(&payload, &token),
            token: Some(token.clone()),
        };
        Ok(token)
    }
}

/// Derive the absolute expiry from `expires_at`, `expires_in` or a JWT `exp` claim.
fn token_expiry(payload: &serde_json::Value, token: &str) -> Option<SystemTime> {
    if let Some(expires_at) = payload.get("expires_at").and_then(|v| v.as_f64()) {
        return unix_to_system_time(expires_at);
    }
    if let Some(expires_in) = payload.get("expires_in").and_then(|v| v.as_f64()) {
        return unix_to_system_time(unix_now()? + expires_in);
    }
    jwt_exp(token)
}

fn unix_now() -> Option<f64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
}

fn unix_to_system_time(unix: f64) -> Option<SystemTime> {
    if unix.is_finite() && unix >= 0.0 {
        UNIX_EPOCH.checked_add(Duration::from_secs_f64(unix))
    } else {
        None
    }
}

/// Read the `exp` claim from a JWT-shaped token without verifying it.
fn jwt_exp(token: &str) -> Option<SystemTime> {
    let mut parts = token.split('.');
    parts.next()?;
    let payload = parts.next()?;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    let exp = claims.get("exp").and_then(|v| v.as_f64())?;
    unix_to_system_time(exp)
}

/// Parse a `Retry-After` header value (delay seconds or HTTP-date).
/// Parse a `Retry-After` header value (delay seconds or HTTP-date).
///
/// Non-finite or unrepresentable numeric values yield `None` instead of
/// panicking; negative delays clamp to zero.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.trim().parse::<f64>() {
        return Duration::try_from_secs_f64(seconds.max(0.0)).ok();
    }
    let when = httpdate::parse_http_date(value).ok()?;
    Some(
        when.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_exp_reads_standard_claim() {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp": 1700000000}"#);
        let token = format!("eyJhbGciOiJIUzI1NiJ9.{payload}.sig");
        let exp = jwt_exp(&token).expect("exp parsed");
        assert_eq!(
            exp.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_700_000_000
        );
    }

    #[test]
    fn jwt_exp_returns_none_for_opaque_token() {
        assert_eq!(jwt_exp("plain-token"), None);
    }

    #[test]
    fn expiry_prefers_expires_at_over_expires_in() {
        let payload = serde_json::json!({"expires_at": 100.0, "expires_in": 9999.0});
        let exp = token_expiry(&payload, "opaque").unwrap();
        assert_eq!(exp.duration_since(UNIX_EPOCH).unwrap().as_secs(), 100);
    }

    #[test]
    fn expiry_uses_jwt_fallback_when_fields_missing() {
        let payload = serde_json::json!({});
        let payload_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp": 1700000000}"#);
        let token = format!("h.{payload_b64}.s");
        let exp = token_expiry(&payload, &token).unwrap();
        assert_eq!(
            exp.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_700_000_000
        );
    }

    #[test]
    fn retry_after_seconds_and_dates() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Retry-After", "12".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(12)));

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Retry-After",
            httpdate::fmt_http_date(UNIX_EPOCH).parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&headers), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_never_panics_on_hostile_numeric_values() {
        // Non-representable delays are dropped instead of panicking.
        for value in ["inf", "1e300"] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("Retry-After", value.parse().unwrap());
            assert_eq!(
                parse_retry_after(&headers),
                None,
                "huge value must not panic"
            );
        }
        // NaN clamps to zero through f64::max semantics; negatives clamp too.
        for value in ["NaN", "-inf", "-1e300", "-5"] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("Retry-After", value.parse().unwrap());
            assert_eq!(parse_retry_after(&headers), Some(Duration::ZERO));
        }
    }
}
