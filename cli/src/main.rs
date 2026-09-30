//! `yandex-tts` — CLI for the local `yandex-ttsd` Unix socket API.
//!
//! Usage:
//!   yandex-tts say "Привет"
//!   yandex-tts ping
//!
//! Socket path resolution (`SOCKET_PATH`, `$XDG_RUNTIME_DIR`) is shared with
//! the daemon via the `yandex-tts-protocol` crate. Errors never masquerade as
//! success: any failure exits non-zero.

use std::path::Path;
use std::process::ExitCode;
use yandex_tts_protocol::{Client, Request, socket_path};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Say(String),
    Ping,
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let Some(sub) = args.first() else {
        return Err("usage: yandex-tts <say <text> | ping>".to_owned());
    };
    match sub.as_str() {
        "say" => {
            let text = args[1..].join(" ");
            if text.trim().is_empty() {
                return Err("usage: yandex-tts say <text>".to_owned());
            }
            Ok(Command::Say(text))
        }
        "ping" if args.len() == 1 => Ok(Command::Ping),
        _ => Err("usage: yandex-tts <say <text> | ping>".to_owned()),
    }
}

fn run(command: &Command, socket: &Path) -> Result<(), String> {
    let request = match command {
        Command::Say(text) => Request::Say(text.clone()),
        Command::Ping => Request::Ping,
    };
    let mut client = Client::connect(socket)
        .map_err(|e| format!("cannot connect to daemon socket {}: {e}", socket.display()))?;
    let response = client
        .request(&request)
        .map_err(|e| format!("daemon request failed: {e}"))?;
    use yandex_tts_protocol::Response;
    match (command, response) {
        // Each command accepts only its own success shape.
        (Command::Say(_), Response::Ok) => Ok(()),
        (Command::Ping, Response::Connected(connected)) => {
            println!(
                "{}",
                serde_json::json!({"ok": true, "connected": connected})
            );
            Ok(())
        }
        (_, Response::Error(code)) => Err(format!("daemon error: {code}")),
        (_, response) => Err(format!("unexpected daemon response: {response:?}")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(command) => command,
        Err(usage) => {
            eprintln!("yandex-tts: {usage}");
            return ExitCode::from(2);
        }
    };
    match run(&command, &socket_path()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("yandex-tts: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_say_with_text() {
        assert_eq!(
            parse_args(&args(&["say", "Привет", "мир"])).unwrap(),
            Command::Say("Привет мир".to_owned())
        );
    }

    #[test]
    fn parses_ping() {
        assert_eq!(parse_args(&args(&["ping"])).unwrap(), Command::Ping);
    }

    #[test]
    fn rejects_empty_and_unknown() {
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["say"])).is_err());
        assert!(parse_args(&args(&["say", "   "])).is_err());
        assert!(parse_args(&args(&["ping", "extra"])).is_err());
        assert!(parse_args(&args(&["volume"])).is_err());
    }
}
