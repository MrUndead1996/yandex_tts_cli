//! Shared wire protocol for `yandex-ttsd` (Unix socket, JSON Lines) and `yandex-tts`.
//!
//! Contract (see docs/tasks.md): one JSON object per line, one JSON response
//! per request. Default socket path is `$XDG_RUNTIME_DIR/yandex-stationd.sock`,
//! falling back to `/run/user/<uid>/yandex-stationd.sock`, overridable via
//! `SOCKET_PATH`.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_MESSAGE_SIZE: usize = 65536;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default socket path, compatible with the Python daemon in `__main__.py`.
pub fn socket_path() -> PathBuf {
    if let Some(configured) = std::env::var_os("SOCKET_PATH")
        && !configured.is_empty()
    {
        return PathBuf::from(configured);
    }
    let runtime = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })),
    };
    runtime.join("yandex-stationd.sock")
}

/// A client request. Serialization order and validation mirror `server.py`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Ping,
    Say(String),
}

impl Request {
    pub fn to_line(&self) -> String {
        let value = match self {
            Request::Ping => json!({"action": "ping"}),
            Request::Say(text) => json!({"action": "say", "text": text}),
        };
        let mut line = value.to_string();
        line.push('\n');
        line
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Ok,
    Connected(bool),
    Error(String),
}

impl Response {
    pub fn error(code: &str) -> Response {
        Response::Error(code.to_owned())
    }

    pub fn is_ok(&self) -> bool {
        !matches!(self, Response::Error(_))
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Response, String> {
        let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let obj = value.as_object().ok_or("response is not an object")?;
        match obj.get("ok").and_then(Value::as_bool) {
            Some(true) => {
                if let Some(connected) = obj.get("connected") {
                    let connected = connected.as_bool().ok_or("connected is not a bool")?;
                    Ok(Response::Connected(connected))
                } else {
                    Ok(Response::Ok)
                }
            }
            Some(false) => {
                let error = obj
                    .get("error")
                    .and_then(Value::as_str)
                    .ok_or("missing error code")?
                    .to_owned();
                Ok(Response::Error(error))
            }
            None => Err("missing ok field".to_owned()),
        }
    }
}

#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    /// Peer closed the connection without a response.
    Disconnected,
    /// Response was not a valid protocol message.
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "{e}"),
            ClientError::Disconnected => write!(f, "connection closed without response"),
            ClientError::Protocol(msg) => write!(f, "unexpected daemon response: {msg}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl PartialEq for ClientError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ClientError::Io(a), ClientError::Io(b)) => {
                a.kind() == b.kind() && a.to_string() == b.to_string()
            }
            (ClientError::Disconnected, ClientError::Disconnected) => true,
            (ClientError::Protocol(a), ClientError::Protocol(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for ClientError {}

/// Blocking JSON Lines client over a Unix stream socket.
#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub fn connect(path: &Path) -> std::io::Result<Client> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(DEFAULT_TIMEOUT))?;
        stream.set_write_timeout(Some(DEFAULT_TIMEOUT))?;
        Ok(Client { stream })
    }

    /// Sends one request and reads exactly one response line. The connection
    /// stays open so several requests can share one socket.
    pub fn request(&mut self, request: &Request) -> Result<Response, ClientError> {
        let line = request.to_line();
        self.stream
            .write_all(line.as_bytes())
            .map_err(ClientError::Io)?;
        self.stream.flush().map_err(ClientError::Io)?;
        // Read byte-by-byte so no bytes beyond the response line are consumed
        // (the connection may serve several requests), bounded so a broken or
        // malicious daemon cannot make us buffer unbounded data.
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = self.stream.read(&mut byte).map_err(ClientError::Io)?;
            if n == 0 {
                return Err(ClientError::Disconnected);
            }
            if byte[0] == b'\n' {
                break;
            }
            buf.push(byte[0]);
            if buf.len() > MAX_MESSAGE_SIZE {
                return Err(ClientError::Protocol(format!(
                    "response exceeds {MAX_MESSAGE_SIZE} bytes"
                )));
            }
        }
        if buf.is_empty() {
            return Err(ClientError::Disconnected);
        }
        Response::from_slice(&buf).map_err(ClientError::Protocol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_honors_socket_path_override() {
        // SAFETY: tests run single-threaded per process for this env var.
        unsafe { std::env::set_var("SOCKET_PATH", "/tmp/custom.sock") };
        assert_eq!(socket_path(), PathBuf::from("/tmp/custom.sock"));
        unsafe { std::env::remove_var("SOCKET_PATH") };
    }

    #[test]
    fn request_lines_match_contract() {
        assert_eq!(Request::Ping.to_line(), "{\"action\":\"ping\"}\n");
        assert_eq!(
            Request::Say("Привет".to_owned()).to_line(),
            "{\"action\":\"say\",\"text\":\"Привет\"}\n"
        );
    }

    #[test]
    fn response_parsing() {
        assert_eq!(
            Response::from_slice(b"{\"ok\": true, \"connected\": true}\n"),
            Ok(Response::Connected(true))
        );
        assert_eq!(Response::from_slice(b"{\"ok\": true}\n"), Ok(Response::Ok));
        assert_eq!(
            Response::from_slice(b"{\"ok\": false, \"error\": \"timeout\"}\n"),
            Ok(Response::error("timeout"))
        );
        assert!(Response::from_slice(b"not json\n").is_err());
        assert!(Response::from_slice(b"[]\n").is_err());
        assert!(Response::from_slice(b"{\"connected\": true}\n").is_err());
    }
}
