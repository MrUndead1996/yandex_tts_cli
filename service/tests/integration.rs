//! End-to-end tests: protocol server + mock station + shared client (the same
//! client code path used by the `yandex-tts` CLI), no network, no Python.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use yandex_tts_protocol::{Client, Request, Response};
use yandex_ttsd::server::Server;
use yandex_ttsd::station::mock::MockStation;
use yandex_ttsd::station::{NotConnectedStation, StationError};

struct TestDaemon {
    path: std::path::PathBuf,
    station: Arc<MockStation>,
    stop: Arc<tokio::sync::Notify>,
    _dir: TempDir,
    task: JoinHandle<()>,
}

impl TestDaemon {
    fn client(&self) -> Client {
        Client::connect(&self.path).expect("connect to test daemon")
    }

    async fn shutdown(self) {
        self.stop.notify_one();
        self.task.await.unwrap();
        assert!(!self.path.exists(), "socket must be removed on shutdown");
    }
}

async fn start_daemon_with(
    station: Arc<MockStation>,
    timeout: Duration,
    max_message_size: usize,
) -> TestDaemon {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("station.sock");
    let mut server = Server::with_options(&path, Arc::clone(&station), timeout, max_message_size);
    server.start().await.expect("bind test socket");
    let stop = server.stop_handle();
    let task = tokio::spawn(async move {
        let _ = server.serve_forever().await;
    });
    // Wait until the socket accepts connections.
    for _ in 0..100 {
        if StdUnixStream::connect(&path).is_ok() {
            return TestDaemon {
                path,
                station,
                stop,
                _dir: dir,
                task,
            };
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("test daemon never became connectable");
}

async fn start_daemon() -> TestDaemon {
    start_daemon_with(
        Arc::new(MockStation::default()),
        Duration::from_secs(10),
        65536,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn permissions_are_owner_only() {
    let daemon = start_daemon().await;
    let mode = std::fs::metadata(&daemon.path)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_and_say_over_one_connection() {
    let daemon = start_daemon().await;
    daemon.station.set_connected(true);
    let mut client = daemon.client();
    assert_eq!(
        client.request(&Request::Ping),
        Ok(Response::Connected(true))
    );
    assert_eq!(
        client.request(&Request::Say("hello".into())),
        Ok(Response::Ok)
    );
    assert_eq!(
        client.request(&Request::Ping),
        Ok(Response::Connected(true))
    );
    assert_eq!(daemon.station.spoken(), vec!["hello".to_owned()]);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_clients_are_served() {
    let daemon = start_daemon().await;
    daemon.station.set_connected(true);
    let path = daemon.path.clone();
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut client = Client::connect(&path).unwrap();
                assert_eq!(
                    client.request(&Request::Ping),
                    Ok(Response::Connected(true))
                );
                assert_eq!(
                    client.request(&Request::Say(format!("msg {i}"))),
                    Ok(Response::Ok)
                );
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(daemon.station.spoken().len(), 8);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn validation_errors_match_python_server() {
    let daemon = start_daemon().await;
    let mut stream = StdUnixStream::connect(&daemon.path).unwrap();
    use std::io::{Read, Write};
    let cases: &[(&[u8], &str)] = &[
        (b"[]\n", "invalid_request"),
        (b"{}\n", "invalid_request"),
        (b"{\"action\":1}\n", "invalid_request"),
        (b"{\"action\":\"say\",\"text\":\" \"}\n", "invalid_request"),
        (b"{\"action\":\"say\",\"text\":1}\n", "invalid_request"),
        (
            b"{\"action\":\"say\",\"text\":\"hi\",\"extra\":1}\n",
            "invalid_request",
        ),
        (b"{\"action\":\"ping\",\"extra\":1}\n", "invalid_request"),
        (b"{\"action\":\"volume\"}\n", "unknown_action"),
        (b"not json\n", "invalid_json"),
    ];
    for (message, expected) in cases {
        stream.write_all(message).unwrap();
        let mut buf = [0u8; 256];
        let n = stream.read(&mut buf).unwrap();
        let response = Response::from_slice(&buf[..n]).unwrap();
        assert_eq!(response, Response::error(expected), "for {message:?}");
    }
    // Trimmed-but-nonempty text is accepted; the raw text is spoken.
    stream
        .write_all(b"{\"action\":\"say\",\"text\":\"  hi  \"}\n")
        .unwrap();
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).unwrap();
    assert_eq!(Response::from_slice(&buf[..n]), Ok(Response::Ok));
    assert_eq!(daemon.station.spoken(), vec!["  hi  ".to_owned()]);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_line_is_rejected_and_connection_closed() {
    let daemon = start_daemon_with(
        Arc::new(MockStation::default()),
        Duration::from_secs(10),
        128,
    )
    .await;
    let mut stream = StdUnixStream::connect(&daemon.path).unwrap();
    use std::io::{Read, Write};
    stream.write_all(&[b'x'; 130]).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).unwrap();
    assert_eq!(
        Response::from_slice(&buf[..n]),
        Ok(Response::error("message_too_large"))
    );
    // The server closes the connection after this error.
    assert_eq!(stream.read(&mut buf).unwrap(), 0);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unterminated_line_is_rejected() {
    let daemon = start_daemon().await;
    let mut stream = StdUnixStream::connect(&daemon.path).unwrap();
    use std::io::{Read, Write};
    stream.write_all(b"{\"action\":\"ping\"}").unwrap();
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).unwrap();
    assert_eq!(
        Response::from_slice(&buf[..n]),
        Ok(Response::error("message_too_large"))
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn station_failure_maps_to_station_not_connected() {
    let daemon = start_daemon().await;
    daemon.station.set_failure(Some(StationError::NotConnected));
    let mut client = daemon.client();
    assert_eq!(
        client.request(&Request::Say("hello".into())),
        Ok(Response::error("station_not_connected"))
    );
    assert!(daemon.station.spoken().is_empty());
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn speaking_station_rejects_say_with_distinct_error() {
    let daemon = start_daemon().await;
    daemon.station.set_failure(Some(StationError::Speaking));
    let mut client = daemon.client();
    assert_eq!(
        client.request(&Request::Say("hello".into())),
        Ok(Response::error("station_speaking"))
    );
    assert!(daemon.station.spoken().is_empty());
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn read_timeout_responds_timeout_and_closes() {
    let daemon = start_daemon_with(
        Arc::new(MockStation::default()),
        Duration::from_millis(100),
        65536,
    )
    .await;
    let mut stream = StdUnixStream::connect(&daemon.path).unwrap();
    use std::io::Read;
    std::thread::sleep(Duration::from_millis(250));
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).unwrap();
    assert_eq!(
        Response::from_slice(&buf[..n]),
        Ok(Response::error("timeout"))
    );
    assert_eq!(stream.read(&mut buf).unwrap(), 0);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn placeholder_backend_reports_not_connected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("station.sock");
    let mut server = Server::new(&path, Arc::new(NotConnectedStation));
    server.start().await.unwrap();
    let stop = server.stop_handle();
    let task = tokio::spawn(async move {
        let _ = server.serve_forever().await;
    });
    let mut client = Client::connect(&path).unwrap();
    // No real station link exists: never claim success for say.
    assert_eq!(
        client.request(&Request::Ping),
        Ok(Response::Connected(false))
    );
    assert_eq!(
        client.request(&Request::Say("hi".into())),
        Ok(Response::error("station_not_connected"))
    );
    stop.notify_one();
    task.await.unwrap();
    assert!(!path.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_socket_is_replaced_but_live_socket_is_preserved() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("station.sock");

    let station = Arc::new(MockStation::default());
    let mut first = Server::new(&path, Arc::clone(&station));
    first.start().await.unwrap();
    let first_stop = first.stop_handle();
    let first_task = tokio::spawn(async move {
        let _ = first.serve_forever().await;
    });

    // A second server must refuse to take a live socket.
    let mut second = Server::new(&path, Arc::clone(&station));
    assert!(matches!(
        second.start().await,
        Err(yandex_ttsd::server::ServerError::SocketInUse(_))
    ));

    first_stop.notify_one();
    first_task.await.unwrap();
    assert!(!path.exists());

    // A leftover socket file with no listener is replaced.
    std::os::unix::net::UnixListener::bind(&path).unwrap();
    let mut third = Server::new(&path, Arc::clone(&station));
    third.start().await.unwrap();
    assert!(path.exists());
    let third_stop = third.stop_handle();
    let third_task = tokio::spawn(async move {
        let _ = third.serve_forever().await;
    });
    third_stop.notify_one();
    third_task.await.unwrap();
    assert!(!path.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn pipelined_requests_in_single_write_are_all_answered() {
    // Bytes after the first newline must be preserved across reads.
    let daemon = start_daemon().await;
    let mut stream = StdUnixStream::connect(&daemon.path).unwrap();
    use std::io::{Read, Write};
    stream
        .write_all(
            b"{\"action\":\"ping\"}\n{\"action\":\"say\",\"text\":\"one\"}\n{\"action\":\"ping\"}\n",
        )
        .unwrap();
    let mut buf = [0u8; 512];
    let mut received = String::new();
    while received.matches('\n').count() < 3 {
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0, "connection closed after {received:?}");
        received.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    let mut lines = received.lines();
    assert_eq!(
        Response::from_slice(lines.next().unwrap().as_bytes()),
        Ok(Response::Connected(false))
    );
    assert_eq!(
        Response::from_slice(lines.next().unwrap().as_bytes()),
        Ok(Response::Ok)
    );
    assert_eq!(
        Response::from_slice(lines.next().unwrap().as_bytes()),
        Ok(Response::Connected(false))
    );
    // The last (third) request in the single write was spoken.
    assert_eq!(daemon.station.spoken(), vec!["one".to_owned()]);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_lets_in_flight_request_finish() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use yandex_ttsd::station::{Station, StationError};

    /// Station whose `say` takes a while: the request is in flight while
    /// shutdown starts.
    struct SlowStation {
        delay: Duration,
        in_flight: AtomicBool,
    }

    impl Station for SlowStation {
        fn connected(&self) -> bool {
            true
        }

        async fn say(&self, _text: &str) -> Result<(), StationError> {
            self.in_flight.store(true, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            Ok(())
        }
    }

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("station.sock");
    let station = Arc::new(SlowStation {
        delay: Duration::from_millis(300),
        in_flight: AtomicBool::new(false),
    });
    let mut server = Server::new(&path, Arc::clone(&station));
    server.start().await.unwrap();
    let stop = server.stop_handle();
    let task = tokio::spawn(async move {
        let _ = server.serve_forever().await;
    });

    // A request goes out and reaches the (slow) station.
    let say = std::thread::spawn({
        let path = path.clone();
        move || {
            let mut client = Client::connect(&path).unwrap();
            client.request(&Request::Say("slow".into()))
        }
    });
    while !station.in_flight.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Shutdown must not abort the handler: the drain waits for the request
    // to finish and the client still receives its `ok` response.
    stop.notify_one();
    task.await.unwrap();
    assert_eq!(say.join().unwrap(), Ok(Response::Ok));
    assert!(!path.exists(), "socket must be removed after the drain");
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_client_connection_does_not_block_other_clients() {
    let daemon = start_daemon().await;
    let _idle = daemon.client();
    let mut client = daemon.client();
    assert_eq!(
        client.request(&Request::Ping),
        Ok(Response::Connected(false))
    );
    daemon.shutdown().await;
}
