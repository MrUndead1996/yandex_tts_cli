//! `yandex-ttsd` — local Yandex Station TTS daemon.
//!
//! Serves the local JSON Lines API over a Unix socket with the real
//! [`ConnectionManager`] backend: daemon-only credentials are validated from
//! the environment *before* the socket is created, the Station is resolved
//! off the async runtime (manual config or mDNS), the manager recovers the
//! Glagol WSS connection in the background, and shutdown stops accepting,
//! closes the station link and removes the socket.

use std::sync::Arc;
use yandex_ttsd::daemon;
use yandex_ttsd::server::Server;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("yandex-ttsd: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Fail before touching the socket: missing credentials are a startup
    // error, not a runtime one.
    let credentials = daemon::Credentials::from_env()?;
    let station = daemon::resolve_station().await?;
    let manager = Arc::new(daemon::build_manager(credentials, &station)?);

    // Start recovery in the background; the socket API never waits for the
    // Station (`ping` reflects actual readiness instead).
    manager.start()?;

    let socket_path = yandex_ttsd::config::socket_path();
    let mut server = Server::new(&socket_path, Arc::clone(&manager));
    if let Err(err) = server.start().await {
        // No listener was published (or it failed mid-setup): shut the
        // background manager down so nothing outlives this run.
        manager.close().await;
        return Err(err.into());
    }
    println!("yandex-ttsd: listening on {}", socket_path.display());

    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    let mut served = Ok(());
    tokio::select! {
        res = server.serve_forever() => {
            served = res;
        }
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
    // Stop accepting, let in-flight requests finish within the drain budget
    // (each handler is already bounded by the request timeout), remove the
    // socket, then close the station connection.
    server.shutdown().await;
    manager.close().await;
    // A serving failure must not look like a clean shutdown.
    served?;
    Ok(())
}
