//! Integration tests for the daemon wiring: the real socket server backed by
//! a [`ConnectionManager`] with mock auth and mock Glagol connection (no
//! network, no real credentials).

use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use yandex_tts_protocol::{Client, Request, Response};
use yandex_ttsd::connection::ConnectionManager;
use yandex_ttsd::glagol::GlagolError;
use yandex_ttsd::server::Server;
use yandex_ttsd::testing::{MockAuth, MockConn, MockDialer};

type TestManager = ConnectionManager<MockAuth, MockDialer>;

struct TestDaemon {
    path: std::path::PathBuf,
    manager: Arc<TestManager>,
    conn: Arc<MockConn>,
    stop: Arc<tokio::sync::Notify>,
    _dir: TempDir,
    task: JoinHandle<()>,
}

impl TestDaemon {
    fn client(&self) -> Client {
        Client::connect(&self.path).expect("connect to test daemon")
    }

    async fn shutdown(self) {
        // Order mirrors main: stop the server first, then close the manager.
        self.stop.notify_one();
        self.task.await.unwrap();
        assert!(!self.path.exists(), "socket must be removed on shutdown");
        self.manager.close().await;
        assert!(self.manager.start().is_err(), "manager must be closed");
    }
}

/// Starts the socket server over a manager connected to a single mock
/// Glagol connection. `manager.start()` runs in the background, exactly
/// like the daemon.
async fn start_daemon() -> TestDaemon {
    let conn = MockConn::new();
    let dialer = MockDialer::new();
    dialer.push_conn(Arc::clone(&conn));
    start_daemon_with(auth_and_dialer(dialer), conn).await
}

fn auth_and_dialer(dialer: MockDialer) -> (MockAuth, MockDialer) {
    (MockAuth::new("mock-device-token"), dialer)
}

async fn start_daemon_with(
    (auth, dialer): (MockAuth, MockDialer),
    conn: Arc<MockConn>,
) -> TestDaemon {
    start_daemon_opts((auth, dialer), conn, Duration::from_secs(10)).await
}

async fn start_daemon_opts(
    (auth, dialer): (MockAuth, MockDialer),
    conn: Arc<MockConn>,
    request_timeout: Duration,
) -> TestDaemon {
    let manager = Arc::new(
        ConnectionManager::with_jitter(
            auth,
            dialer,
            Duration::from_millis(20),
            Duration::from_millis(80),
            Duration::from_millis(40),
            |_| Duration::ZERO,
        )
        .expect("valid intervals"),
    );
    manager.start().expect("background start");
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("station.sock");
    let mut server = Server::with_options(&path, Arc::clone(&manager), request_timeout, 65536);
    server.start().await.expect("bind test socket");
    let stop = server.stop_handle();
    let task = tokio::spawn(async move {
        let _ = server.serve_forever().await;
    });
    for _ in 0..100 {
        if StdUnixStream::connect(&path).is_ok() {
            return TestDaemon {
                path,
                manager,
                conn,
                stop,
                _dir: dir,
                task,
            };
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("test daemon never became connectable");
}

async fn wait_connected(manager: &TestManager) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !manager.connected() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "manager never became connected"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits for the mock station to receive the say payload and answers the
/// pending request (mock answers resolve directly, no envelope id involved).
async fn answer_say(conn: &MockConn, response: Value) -> Value {
    let payload = conn.next_sent().await;
    assert_eq!(payload["command"], "serverAction");
    conn.answer(Ok(response));
    payload
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_reflects_manager_readiness() {
    // Park `connect` on the mock so the manager stays pending until we
    // release the gate (a real dial takes longer than one ping).
    let conn = MockConn::new();
    let gate = conn.hold_gate().await;
    let dialer = MockDialer::new();
    dialer.push_conn(Arc::clone(&conn));
    let daemon = start_daemon_with(auth_and_dialer(dialer), conn).await;
    let mut client = daemon.client();

    // Not yet connected: ping is honest about it.
    assert_eq!(
        client.request(&Request::Ping),
        Ok(Response::Connected(false))
    );

    // Release the dial; ping follows the manager's readiness.
    drop(gate);
    wait_connected(&daemon.manager).await;
    assert_eq!(
        client.request(&Request::Ping),
        Ok(Response::Connected(true))
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn say_succeeds_only_after_correlated_response() {
    let daemon = start_daemon().await;
    wait_connected(&daemon.manager).await;
    let mut client = daemon.client();

    // The say blocks until the Station answers the correlated request.
    let say = {
        let mut say_client = daemon.client();
        std::thread::spawn(move || say_client.request(&Request::Say("Привет".into())))
    };
    let payload = answer_say(&daemon.conn, json!({"result": "ok"})).await;

    // The payload carries the phrase via the serverAction structure; the
    // text never leaks to the client on success.
    assert_eq!(
        payload["serverActionEventPayload"]["payload"]["form_update"]["slots"][0]["value"],
        "Привет"
    );

    let response = say
        .join()
        .expect("say thread")
        .expect("say request completed");
    assert_eq!(response, Response::Ok);
    // The connection stays usable for subsequent requests.
    assert_eq!(
        client.request(&Request::Ping),
        Ok(Response::Connected(true))
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_station_failure_is_not_reported_as_success() {
    let daemon = start_daemon().await;
    wait_connected(&daemon.manager).await;

    let say = {
        let mut client = daemon.client();
        std::thread::spawn(move || client.request(&Request::Say("hi".into())))
    };
    // The Station answers the correlated request with an explicit failure:
    // the daemon must answer `station_not_connected`, never ok.
    answer_say(&daemon.conn, json!({"status": "error"})).await;
    let response = say.join().expect("say thread").expect("completed");
    assert_eq!(response, Response::error("station_not_connected"));
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn station_failure_with_ok_status_is_not_success() {
    let daemon = start_daemon().await;
    wait_connected(&daemon.manager).await;

    let say = {
        let mut client = daemon.client();
        std::thread::spawn(move || client.request(&Request::Say("hi".into())))
    };
    // status claims ok but the response carries an explicit error field:
    // still a failure for the client.
    answer_say(&daemon.conn, json!({"status": "ok", "error": "denied"})).await;

    let response = say.join().expect("say thread").expect("completed");
    assert_eq!(response, Response::error("station_not_connected"));
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn say_before_connection_reports_station_not_connected() {
    // The dialer always fails: the manager stays disconnected but started.
    let dialer = MockDialer::new();
    dialer.always_fail(GlagolError::Transport);
    let (auth, dialer) = auth_and_dialer(dialer);
    let conn = MockConn::new();
    // A short request bound proves say fails fast instead of hanging.
    let daemon = start_daemon_opts((auth, dialer), conn, Duration::from_millis(300)).await;

    let mut client = daemon.client();
    assert_eq!(
        client.request(&Request::Say("hi".into())),
        Ok(Response::error("station_not_connected"))
    );
    assert!(!daemon.manager.connected());
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_drop_fails_pending_say_without_replay() {
    let daemon = start_daemon().await;
    wait_connected(&daemon.manager).await;

    let say = {
        let mut client = daemon.client();
        std::thread::spawn(move || client.request(&Request::Say("once".into())))
    };
    // Wait for the envelope to go out, then drop the connection before any
    // answer: unknown outcome — never replayed, reported as an error.
    let _envelope = daemon.conn.next_sent().await;
    daemon.conn.disconnect(None);

    let response = say.join().expect("say thread").expect("completed");
    assert_eq!(response, Response::error("station_not_connected"));
    // The server answered within its bounded request timeout.
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_map_to_station_not_connected_after_close() {
    let daemon = start_daemon().await;
    wait_connected(&daemon.manager).await;
    // Manager closed underneath a connected daemon: requests fail cleanly.
    daemon.manager.close().await;
    let mut client = daemon.client();
    assert_eq!(
        client.request(&Request::Say("hi".into())),
        Ok(Response::error("station_not_connected"))
    );
    assert_eq!(
        client.request(&Request::Ping),
        Ok(Response::Connected(false))
    );
    daemon.shutdown().await;
}
