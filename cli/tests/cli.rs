//! End-to-end tests of the actual `yandex-tts` executable against a mock
//! daemon speaking the JSON Lines protocol — no network, no Python.
//!
//! The mock daemon is an owned fixture: the temp directory, the listener and
//! the serving thread are all shut down and joined on `Drop`, so no test leaks
//! directories or background threads.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use tempfile::TempDir;

struct MockDaemon {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    _dir: TempDir,
}

impl MockDaemon {
    /// Answers one response line per request line; responses are chosen by
    /// the requested action.
    fn spawn(ping_response: &'static str, say_response: &'static str) -> MockDaemon {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("station.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            // Wake the blocking accept on shutdown by connecting to the socket.
            while !thread_stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while !thread_stop.load(Ordering::SeqCst)
                    && reader.read_line(&mut line).unwrap_or(0) > 0
                {
                    let response = if line.contains("\"ping\"") {
                        ping_response
                    } else {
                        say_response
                    };
                    if stream.write_all(response.as_bytes()).is_err() {
                        break;
                    }
                    line.clear();
                }
            }
        });
        MockDaemon {
            path,
            stop,
            handle: Some(handle),
            _dir: dir,
        }
    }
}

impl Drop for MockDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept so the thread can observe the stop flag.
        let _ = UnixStream::connect(&self.path);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_cli(daemon: &MockDaemon, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_yandex-tts"))
        .args(args)
        .env("SOCKET_PATH", &daemon.path)
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .expect("run yandex-tts binary")
}

#[test]
fn ping_prints_response_and_exits_zero() {
    let daemon = MockDaemon::spawn("{\"ok\": true, \"connected\": true}\n", "{}\n");
    let output = run_cli(&daemon, &["ping"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "{\"connected\":true,\"ok\":true}"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn say_succeeds_only_on_ok_response() {
    let daemon = MockDaemon::spawn("{}", "{\"ok\": true}\n");
    let output = run_cli(&daemon, &["say", "Привет"]);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn say_rejects_connected_response_as_success() {
    // A broken daemon answering ping's shape for say must not exit 0.
    let daemon = MockDaemon::spawn("{}", "{\"ok\": true, \"connected\": true}\n");
    let output = run_cli(&daemon, &["say", "hello"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected daemon response"));
}

#[test]
fn ping_rejects_ok_response_as_success() {
    let daemon = MockDaemon::spawn("{\"ok\": true}\n", "{}\n");
    let output = run_cli(&daemon, &["ping"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected daemon response"));
}

#[test]
fn daemon_error_response_exits_non_zero() {
    let daemon = MockDaemon::spawn(
        "{}",
        "{\"ok\": false, \"error\": \"station_not_connected\"}\n",
    );
    let output = run_cli(&daemon, &["say", "hello"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("station_not_connected"));
    assert!(output.stdout.is_empty());
}

#[test]
fn missing_daemon_fails_with_connect_error() {
    let dir = TempDir::new().unwrap();
    let daemon = MockDaemon {
        path: dir.path().join("absent.sock"),
        stop: Arc::new(AtomicBool::new(false)),
        handle: None,
        _dir: dir,
    };
    let output = run_cli(&daemon, &["ping"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot connect"));
}
