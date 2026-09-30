//! Station backend abstraction.
//!
//! Only the mock backend is functional in this migration slice: the real
//! Glagol/WSS connection is этап 4 of docs/tasks.md and is intentionally not
//! implemented. [`NotConnectedStation`] never reports success for `say`: the
//! daemon answers `station_not_connected` and `ping` reports `connected:false`
//! until a real station link exists.

use std::future::Future;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StationError {
    /// The station link is down or the command was not confirmed.
    NotConnected,
    /// Unexpected backend failure; reported to clients as `internal_error`
    /// without leaking details.
    Internal(&'static str),
}

pub trait Station: Send + Sync + 'static {
    fn connected(&self) -> bool;
    /// Sends TTS text. Must return `Ok` only after the station confirmed the
    /// command; there is no retry for commands with unknown outcome.
    fn say(&self, text: &str) -> impl Future<Output = Result<(), StationError>> + Send;
}

/// Placeholder backend used until auth/Glagol (docs/tasks.md этап 3–4) exist.
pub struct NotConnectedStation;

impl Station for NotConnectedStation {
    fn connected(&self) -> bool {
        false
    }

    async fn say(&self, _text: &str) -> Result<(), StationError> {
        Err(StationError::NotConnected)
    }
}

/// In-memory station backend for tests; never touches the network. Compiled
/// unconditionally so integration tests (and only tests) can use it.
pub mod mock {
    use super::{Station, StationError};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// In-memory station for tests; never touches the network.
    #[derive(Default)]
    pub struct MockStation {
        connected: AtomicBool,
        spoken: Mutex<Vec<String>>,
        failure: Mutex<Option<StationError>>,
    }

    impl MockStation {
        pub fn set_connected(&self, connected: bool) {
            self.connected.store(connected, Ordering::SeqCst);
        }

        pub fn set_failure(&self, failure: Option<StationError>) {
            *self.failure.lock().unwrap() = failure;
        }

        pub fn spoken(&self) -> Vec<String> {
            self.spoken.lock().unwrap().clone()
        }
    }

    impl Station for MockStation {
        fn connected(&self) -> bool {
            self.connected.load(Ordering::SeqCst)
        }

        async fn say(&self, text: &str) -> Result<(), StationError> {
            let failure = self.failure.lock().unwrap().clone();
            match failure {
                Some(err) => Err(err),
                None => {
                    self.spoken.lock().unwrap().push(text.to_owned());
                    Ok(())
                }
            }
        }
    }
}
