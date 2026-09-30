//! Station discovery over mDNS (port of `yandex_stationd/discovery.py` and
//! `__main__.py:station_config`).
//!
//! Stations advertise themselves via `_yandexio._tcp.local.` with TXT records
//! carrying `deviceId`/`platform`. Incomplete advertisements are ignored
//! instead of guessing device credentials.

use std::time::{Duration, Instant};

use mdns_sd::{ServiceEvent, ServiceInfo};
use thiserror::Error;

/// mDNS service type advertised by Yandex Stations.
pub const SERVICE_TYPE: &str = "_yandexio._tcp.local.";

const DEFAULT_TIMEOUT: f64 = 5.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Station {
    pub host: String,
    pub port: u16,
    pub device_id: String,
    pub platform: String,
}

#[derive(Debug, Error, PartialEq)]
pub enum DiscoveryError {
    #[error("timeout must be positive")]
    InvalidTimeout,
    #[error("No Yandex Stations found")]
    NoStations,
    #[error("Multiple Stations found; specify device_id")]
    Ambiguous,
    #[error("Station {0:?} not found")]
    DeviceNotFound(String),
    #[error("YANDEX_STATION_PORT must be a valid port")]
    InvalidManualPort,
    #[error("mDNS failure: {0}")]
    Mdns(String),
}

/// `mdns-sd` TXT keys are matched case-insensitively; empty values are treated
/// as missing, mirroring the Python `or` fallback between `device_id`/`deviceid`.
fn property<'a>(info: &'a ServiceInfo, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .filter_map(|key| info.get_property_val_str(key))
        .find(|value| !value.is_empty())
}

/// Ignore incomplete advertisements instead of guessing device credentials.
pub fn station_from_info(info: &ServiceInfo) -> Option<Station> {
    let device_id = property(info, &["device_id", "deviceid"])?;
    let platform = property(info, &["platform"])?;
    let port = info.get_port();
    if port == 0 {
        return None;
    }
    let mut host = info
        .get_addresses()
        .iter()
        .find(|address| address.is_ipv4())
        .map(|address| address.to_string());
    if host.is_none() {
        let hostname = info.get_hostname();
        if !hostname.is_empty() {
            host = Some(hostname.trim_end_matches('.').to_string());
        }
    }
    let host = host?;
    Some(Station {
        host,
        port,
        device_id: device_id.to_string(),
        platform: platform.to_string(),
    })
}

/// Select a Station by ID, or require an unambiguous single discovery.
pub fn select_station(
    stations: &[Station],
    device_id: Option<&str>,
) -> Result<Station, DiscoveryError> {
    if let Some(device_id) = device_id {
        return stations
            .iter()
            .find(|station| station.device_id == device_id)
            .cloned()
            .ok_or_else(|| DiscoveryError::DeviceNotFound(device_id.to_string()));
    }
    match stations {
        [] => Err(DiscoveryError::NoStations),
        [station] => Ok(station.clone()),
        _ => Err(DiscoveryError::Ambiguous),
    }
}

/// Browse for Stations until the deadline, deduplicating repeated announcements.
pub fn discover_stations(timeout: f64) -> Result<Vec<Station>, DiscoveryError> {
    // Reject non-finite and out-of-range values instead of panicking in
    // `Duration::try_from_secs_f64` (NaN, ±inf, values beyond Duration::MAX).
    let timeout = Duration::try_from_secs_f64(timeout)
        .ok()
        .filter(|duration| !duration.is_zero())
        .ok_or(DiscoveryError::InvalidTimeout)?;
    let daemon =
        mdns_sd::ServiceDaemon::new().map_err(|err| DiscoveryError::Mdns(err.to_string()))?;
    let result = browse(&daemon, timeout);
    // Always dispose the daemon's threads, including after browse errors.
    let _ = daemon.shutdown();
    result
}

fn browse(
    daemon: &mdns_sd::ServiceDaemon,
    timeout: Duration,
) -> Result<Vec<Station>, DiscoveryError> {
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .map_err(|err| DiscoveryError::Mdns(err.to_string()))?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<Station> = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(station) = station_from_info(&info)
                    && !found
                        .iter()
                        .any(|existing| existing.device_id == station.device_id)
                {
                    found.push(station);
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    Ok(found)
}

/// Select a Station by ID, or require an unambiguous single discovery.
pub fn discover_station(device_id: Option<&str>) -> Result<Station, DiscoveryError> {
    select_station(&discover_stations(DEFAULT_TIMEOUT)?, device_id)
}

/// Manual override; requires all four fields with a valid port.
pub fn manual_station(
    host: &str,
    port: &str,
    device_id: &str,
    platform: &str,
) -> Result<Station, DiscoveryError> {
    let Ok(number) = port.parse::<u16>() else {
        return Err(DiscoveryError::InvalidManualPort);
    };
    if number == 0 {
        return Err(DiscoveryError::InvalidManualPort);
    }
    Ok(Station {
        host: host.to_string(),
        port: number,
        device_id: device_id.to_string(),
        platform: platform.to_string(),
    })
}

/// Resolve the daemon's Station: manual override when all four environment
/// variables are set (`YANDEX_STATION_HOST`, `YANDEX_STATION_PORT`,
/// `YANDEX_DEVICE_ID`, `YANDEX_PLATFORM`), otherwise mDNS discovery with
/// optional selection by `YANDEX_DEVICE_ID`.
pub fn station_config() -> Result<Station, DiscoveryError> {
    let host = std::env::var("YANDEX_STATION_HOST").unwrap_or_default();
    let port = std::env::var("YANDEX_STATION_PORT").unwrap_or_default();
    let device_id = std::env::var("YANDEX_DEVICE_ID").unwrap_or_default();
    let platform = std::env::var("YANDEX_PLATFORM").unwrap_or_default();
    if !host.is_empty() && !port.is_empty() && !device_id.is_empty() && !platform.is_empty() {
        return manual_station(&host, &port, &device_id, &platform);
    }
    let device_id = if device_id.is_empty() {
        None
    } else {
        Some(device_id.as_str())
    };
    discover_station(device_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(device_id: Option<&str>, platform: Option<&str>) -> ServiceInfo {
        let mut properties: Vec<(&str, &str)> = Vec::new();
        if let Some(device_id) = device_id {
            properties.push(("deviceId", device_id));
        }
        if let Some(platform) = platform {
            properties.push(("platform", platform));
        }
        ServiceInfo::new(
            "_yandexio._tcp.local.",
            "station",
            "station.local.",
            "192.0.2.10",
            1961,
            properties.as_slice(),
        )
        .unwrap()
    }

    fn station() -> Station {
        Station {
            host: "192.0.2.10".to_string(),
            port: 1961,
            device_id: "device-1".to_string(),
            platform: "yandexstation".to_string(),
        }
    }

    #[test]
    fn extract_station_and_skip_incomplete_advertisement() {
        assert_eq!(
            station_from_info(&service(Some("device-1"), Some("yandexstation"))),
            Some(station())
        );
        assert_eq!(station_from_info(&service(Some("device-1"), None)), None);
    }

    #[test]
    fn hostname_fallback_and_invalid_port() {
        let info = ServiceInfo::new(
            "_yandexio._tcp.local.",
            "station",
            "station.local.",
            "::1",
            1961,
            &[("deviceId", "device-1"), ("platform", "yandexstation")][..],
        )
        .unwrap();
        assert_eq!(
            station_from_info(&info),
            Some(Station {
                host: "station.local".to_string(),
                ..station()
            })
        );
        let info = ServiceInfo::new(
            "_yandexio._tcp.local.",
            "station",
            "station.local.",
            "192.0.2.10",
            0,
            &[("deviceId", "device-1"), ("platform", "yandexstation")][..],
        )
        .unwrap();
        assert_eq!(station_from_info(&info), None);
    }

    #[test]
    fn select_station_and_ambiguity() {
        let stations = vec![
            Station {
                host: "host-1".to_string(),
                port: 1961,
                device_id: "one".to_string(),
                platform: "a".to_string(),
            },
            Station {
                host: "host-2".to_string(),
                port: 1961,
                device_id: "two".to_string(),
                platform: "b".to_string(),
            },
        ];
        assert_eq!(select_station(&stations, Some("two")).unwrap(), stations[1]);
        assert_eq!(
            select_station(&stations, Some("absent")),
            Err(DiscoveryError::DeviceNotFound("absent".to_string()))
        );
        assert_eq!(
            select_station(&stations, None),
            Err(DiscoveryError::Ambiguous)
        );
        assert_eq!(select_station(&[], None), Err(DiscoveryError::NoStations));
    }

    #[test]
    fn manual_station_requires_valid_port() {
        assert_eq!(
            manual_station("host", "1961", "device-1", "yandexstation").unwrap(),
            Station {
                host: "host".to_string(),
                port: 1961,
                device_id: "device-1".to_string(),
                platform: "yandexstation".to_string(),
            }
        );
        assert_eq!(
            manual_station("host", "0", "device-1", "yandexstation"),
            Err(DiscoveryError::InvalidManualPort)
        );
        assert_eq!(
            manual_station("host", "65536", "device-1", "yandexstation"),
            Err(DiscoveryError::InvalidManualPort)
        );
        assert_eq!(
            manual_station("host", "abc", "device-1", "yandexstation"),
            Err(DiscoveryError::InvalidManualPort)
        );
    }

    #[test]
    fn txt_keys_match_case_insensitively() {
        for device_key in ["deviceId", "DEVICEID", "device_id", "Device_Id"] {
            let info = ServiceInfo::new(
                "_yandexio._tcp.local.",
                "station",
                "station.local.",
                "192.0.2.10",
                1961,
                &[(device_key, "device-1"), ("PLATFORM", "yandexstation")][..],
            )
            .unwrap();
            assert_eq!(
                station_from_info(&info),
                Some(station()),
                "key {device_key} should match case-insensitively"
            );
        }
    }

    #[test]
    fn empty_txt_values_are_treated_as_missing() {
        // mdns-sd returns Some("") for a key without a value; Python's
        // `properties.get(...) or ...` treats that as missing.
        let info = ServiceInfo::new(
            "_yandexio._tcp.local.",
            "station",
            "station.local.",
            "192.0.2.10",
            1961,
            &[
                ("deviceId", "device-1"),
                ("deviceid", ""),
                ("platform", "yandexstation"),
            ][..],
        )
        .unwrap();
        assert_eq!(station_from_info(&info), Some(station()));
        let info = ServiceInfo::new(
            "_yandexio._tcp.local.",
            "station",
            "station.local.",
            "192.0.2.10",
            1961,
            &[("deviceId", ""), ("platform", "yandexstation")][..],
        )
        .unwrap();
        assert_eq!(station_from_info(&info), None);
    }

    #[test]
    fn invalid_timeout_is_rejected() {
        for timeout in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e30] {
            assert_eq!(
                discover_stations(timeout),
                Err(DiscoveryError::InvalidTimeout),
                "timeout {timeout} should be rejected"
            );
        }
    }
}
