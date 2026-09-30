//! `yandex-ttsd` — local Yandex Station TTS daemon.
//!
//! This slice (docs/tasks.md этап 2) serves the local JSON Lines API with a
//! placeholder station backend: `ping` reports `connected:false` and `say`
//! answers `station_not_connected` until the real Glagol connection exists.

use std::sync::Arc;
use yandex_ttsd::server::Server;
use yandex_ttsd::station::NotConnectedStation;

#[tokio::main]
async fn main() {
    let mut server = Server::new(
        yandex_ttsd::config::socket_path(),
        Arc::new(NotConnectedStation),
    );
    if let Err(err) = server.start().await {
        eprintln!("yandex-ttsd: {err}");
        std::process::exit(1);
    }
    println!(
        "yandex-ttsd: listening on {}",
        yandex_ttsd::config::socket_path().display()
    );
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    tokio::select! {
        res = server.serve_forever() => {
            if let Err(err) = res {
                eprintln!("yandex-ttsd: {err}");
            }
        }
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
    server.shutdown().await;
}
