//! Daemon wiring (port of `yandex_stationd/__main__.py:run`).
//!
//! Reads the daemon-only credentials from the process environment (a systemd
//! `EnvironmentFile` feeds the environment later; there is no `.env` parser),
//! resolves the Station (manual config or mDNS off the blocking thread pool)
//! and builds the production [`ConnectionManager`] over [`YandexAuth`] and a
//! real WSS dialer.
//!
//! Errors never contain the x-token, client secret, device token or TTS
//! text; they name the missing variable or describe the failure coarsely.

use std::time::Duration;

use yandex_tts_auth::YandexAuth;

use crate::connection::{ConnectionManager, GlagolWsDialer, ManagerError};
use crate::discovery::{self, DiscoveryError, Station};
use crate::glagol::GlagolTls;

/// Reconnect backoff start (Python: `retry_interval=1.0`).
pub const RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// Reconnect backoff cap (Python: `max_retry_interval=30.0`).
pub const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(30);
/// Token refresh probe interval (Python: `refresh_interval=30.0`).
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Daemon-only credentials; held in memory, never logged or formatted.
#[derive(Clone)]
pub struct Credentials {
    pub x_token: String,
    pub music_client_id: String,
    pub music_client_secret: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose the secrets through Debug.
        f.debug_struct("Credentials")
            .field("x_token", &"***")
            .field("music_client_id", &"***")
            .field("music_client_secret", &"***")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A required environment variable is missing or empty.
    #[error("{0} is required")]
    MissingEnv(&'static str),
    /// The Station could not be discovered or manually configured.
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    /// The station lookup task failed (never carries credentials).
    #[error("station lookup task failed")]
    LookupFailed,
    /// Yandex rejected the daemon configuration.
    #[error("daemon authentication configuration is invalid")]
    InvalidAuth,
    /// The connection manager was configured with invalid intervals.
    #[error(transparent)]
    Manager(#[from] ManagerError),
}

impl Credentials {
    /// Reads the credentials through `lookup` (pure so tests never touch the
    /// real environment). Empty values count as missing.
    pub fn from_lookup(
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        fn take(
            key: &'static str,
            lookup: &mut impl FnMut(&str) -> Option<String>,
        ) -> Result<String, ConfigError> {
            lookup(key)
                .filter(|value| !value.is_empty())
                .ok_or(ConfigError::MissingEnv(key))
        }
        Ok(Self {
            x_token: take("YANDEX_X_TOKEN", &mut lookup)?,
            music_client_id: take("YANDEX_MUSIC_CLIENT_ID", &mut lookup)?,
            music_client_secret: take("YANDEX_MUSIC_CLIENT_SECRET", &mut lookup)?,
        })
    }

    /// Reads the credentials from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }
}

/// Resolves the Station (manual config or mDNS) without blocking the async
/// runtime: `station_config` may browse mDNS for seconds.
pub async fn resolve_station() -> Result<Station, ConfigError> {
    resolve_station_with(discovery::station_config).await
}

/// [`resolve_station`] with an injectable lookup (tests use it; the lookup
/// closure sees no daemon credentials).
pub async fn resolve_station_with(
    lookup: impl FnOnce() -> Result<Station, DiscoveryError> + Send + 'static,
) -> Result<Station, ConfigError> {
    match tokio::task::spawn_blocking(lookup).await {
        Ok(result) => result.map_err(ConfigError::from),
        // The lookup task panicked; nothing secret can surface here.
        Err(_) => Err(ConfigError::LookupFailed),
    }
}

/// TLS for the Station WSS dial: the local Station's certificate is usually
/// self-signed. This policy never applies to Yandex auth HTTP traffic.
pub fn station_tls() -> GlagolTls {
    GlagolTls::AcceptSelfSigned
}

/// The Glagol WSS endpoint of the Station.
pub fn station_uri(station: &Station) -> String {
    format!("wss://{}:{}", station.host, station.port)
}

/// Builds the production connection manager for the resolved Station.
pub fn build_manager(
    credentials: Credentials,
    station: &Station,
) -> Result<ConnectionManager<YandexAuth, GlagolWsDialer>, ConfigError> {
    let auth = YandexAuth::new(
        credentials.x_token,
        station.device_id.clone(),
        station.platform.clone(),
        credentials.music_client_id,
        credentials.music_client_secret,
    )
    // Configuration problems are startup errors; the details (which never
    // carry secrets anyway) are not needed to run the daemon.
    .map_err(|_| ConfigError::InvalidAuth)?;
    let dialer = GlagolWsDialer::new(station_uri(station), station_tls());
    Ok(ConnectionManager::new(
        auth,
        dialer,
        RETRY_INTERVAL,
        MAX_RETRY_INTERVAL,
        REFRESH_INTERVAL,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_ok() -> impl FnMut(&str) -> Option<String> {
        |key| {
            Some(
                match key {
                    "YANDEX_X_TOKEN" => "test-x-token",
                    "YANDEX_MUSIC_CLIENT_ID" => "test-client-id",
                    "YANDEX_MUSIC_CLIENT_SECRET" => "test-client-secret",
                    _ => "",
                }
                .to_owned(),
            )
        }
    }

    #[test]
    fn credentials_load_all_three_variables() {
        let credentials = Credentials::from_lookup(lookup_ok()).expect("all set");
        assert_eq!(credentials.x_token, "test-x-token");
        assert_eq!(credentials.music_client_id, "test-client-id");
        assert_eq!(credentials.music_client_secret, "test-client-secret");
    }

    #[test]
    fn missing_or_empty_credentials_fail_before_socket_creation() {
        for missing in [
            "YANDEX_X_TOKEN",
            "YANDEX_MUSIC_CLIENT_ID",
            "YANDEX_MUSIC_CLIENT_SECRET",
        ] {
            let result =
                Credentials::from_lookup(|key| (key != missing).then(|| "value".to_owned()));
            assert_eq!(result.unwrap_err(), ConfigError::MissingEnv(missing));
        }
        // Empty values count as missing.
        let result = Credentials::from_lookup(|key| (!key.is_empty()).then(String::new));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::MissingEnv("YANDEX_X_TOKEN")
        );
    }

    #[test]
    fn missing_variable_error_names_the_variable_not_the_value() {
        let error = Credentials::from_lookup(|key| (key != "YANDEX_X_TOKEN").then(|| "x".into()))
            .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("YANDEX_X_TOKEN"), "{text}");
        assert!(!text.contains("value"), "{text}");
    }

    #[test]
    fn credentials_debug_hides_secrets() {
        let credentials = Credentials::from_lookup(lookup_ok()).expect("all set");
        let debug = format!("{credentials:?}");
        for secret in ["test-x-token", "test-client-id", "test-client-secret"] {
            assert!(!debug.contains(secret), "{debug}");
        }
    }

    #[test]
    fn station_uri_and_tls_target_only_the_station_endpoint() {
        let station = Station {
            host: "192.0.2.10".to_owned(),
            port: 1961,
            device_id: "device-1".to_owned(),
            platform: "yandexstation".to_owned(),
        };
        assert_eq!(station_uri(&station), "wss://192.0.2.10:1961");
        // Explicit opt-in for the local Station only; auth HTTP never sees it.
        assert_eq!(station_tls(), GlagolTls::AcceptSelfSigned);
    }

    #[test]
    fn build_manager_accepts_valid_configuration() {
        let credentials = Credentials::from_lookup(lookup_ok()).expect("all set");
        let station = Station {
            host: "192.0.2.10".to_owned(),
            port: 1961,
            device_id: "device-1".to_owned(),
            platform: "yandexstation".to_owned(),
        };
        let manager = build_manager(credentials, &station).expect("valid config");
        // The manager starts disconnected; the background loop owns recovery.
        assert!(!manager.connected());
        let _ = format!("{manager:?}");
    }

    fn test_station() -> Station {
        Station {
            host: "192.0.2.10".to_owned(),
            port: 1961,
            device_id: "device-1".to_owned(),
            platform: "yandexstation".to_owned(),
        }
    }

    #[tokio::test]
    async fn resolve_station_off_the_runtime() {
        let station = resolve_station_with(|| Ok(test_station()))
            .await
            .expect("manual station resolved");
        assert_eq!(station, test_station());
    }

    #[tokio::test]
    async fn lookup_failure_maps_to_error_without_panicking_or_secrets() {
        let error = resolve_station_with(|| Err(DiscoveryError::NoStations))
            .await
            .expect_err("discovery failure surfaces");
        assert_eq!(error, ConfigError::Discovery(DiscoveryError::NoStations));
        // A panicking lookup never brings the daemon down with a panic; the
        // error is coarse and carries no credentials or station data.
        let error = resolve_station_with(|| -> Result<Station, DiscoveryError> {
            panic!("boom");
        })
        .await
        .expect_err("join failure surfaces");
        assert_eq!(error, ConfigError::LookupFailed);
        let text = error.to_string();
        assert!(!text.to_lowercase().contains("boom"), "{text}");
        assert!(!text.contains("device-1"), "{text}");
    }
}
