//! `yandex-tts` / `tts` — CLI for the local `yandex-ttsd` Unix socket API.
//!
//! Usage:
//!   yandex-tts say "Привет"
//!   yandex-tts ping
//!   tts skill_install <PATH>
//!
//! `say`/`ping` talk to the daemon over the Unix socket (socket path
//! resolution — `SOCKET_PATH`, `$XDG_RUNTIME_DIR` — is shared with the daemon
//! via the `yandex-tts-protocol` crate). `skill_install` is fully offline:
//! it only writes skill files into the given skills root. Errors never
//! masquerade as success: any failure exits non-zero.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use yandex_tts_protocol::{Client, Request, socket_path};

/// Portable SKILL.md template: `{{TTS_BIN}}` is replaced with the resolved
/// executable path at install time. Embedded at compile time so the binary
/// install does not depend on a repo checkout.
const SKILL_TEMPLATE: &str = include_str!("../../skills/yandex-station-tts/SKILL.md");
const SKILL_MANIFEST: &str = include_str!("../../skills/yandex-station-tts/skill.toml");
const SKILL_DIR_NAME: &str = "yandex-station-tts";
const TEMPLATE_PLACEHOLDER: &str = "{{TTS_BIN}}";

const USAGE: &str = "usage: yandex-tts <say <text> | ping> | tts skill_install <PATH>";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Say(String),
    Ping,
    SkillInstall(PathBuf),
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let Some(sub) = args.first() else {
        return Err(USAGE.to_owned());
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
        "skill_install" if args.len() == 2 && !args[1].is_empty() => {
            Ok(Command::SkillInstall(PathBuf::from(&args[1])))
        }
        _ => Err(USAGE.to_owned()),
    }
}

fn run(command: &Command, socket: &Path) -> Result<(), String> {
    let request = match command {
        Command::Say(text) => Request::Say(text.clone()),
        Command::Ping => Request::Ping,
        // skill_install is dispatched in `main` and never reaches the socket.
        Command::SkillInstall(root) => return install_skill_cmd(root),
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

/// Quote an arbitrary string as a single POSIX shell word.
fn shell_escape(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

/// Actual executable to reference from the generated skill: a sibling
/// `yandex-tts` binary of the current executable if present (the `tts` alias
/// and the canonical CLI are installed side by side), otherwise the current
/// executable itself. No hardcoded home paths.
fn resolve_cli(exe: &Path) -> PathBuf {
    if let Some(dir) = exe.parent() {
        let sibling = dir.join("yandex-tts");
        if sibling.is_file() {
            return sibling;
        }
    }
    exe.to_path_buf()
}

/// Offline `skill_install`: resolve the real CLI executable and install the
/// embedded skill files into the parsed skills root. Never contacts the
/// daemon.
fn install_skill_cmd(root: &Path) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("skill_install: cannot resolve current executable: {e}"))?;
    install_skill_into(root, &resolve_cli(&exe))
}

/// Install the embedded skill into `<root>/yandex-station-tts/`. `root` is
/// treated strictly as a SKILLS ROOT directory. Only files owned by this
/// skill are written; unrelated files in the root or the skill directory are
/// left untouched. Existing symlinks at the destination (directory or files)
/// are refused instead of being followed.
fn install_skill_into(root: &Path, cli: &Path) -> Result<(), String> {
    if root.as_os_str().is_empty() {
        return Err("skill_install: skills root path is empty".to_owned());
    }
    if root.symlink_metadata().is_ok() && root.is_symlink() {
        return Err(format!(
            "skill_install: skills root {} is a symlink; refusing to follow it",
            root.display()
        ));
    }
    std::fs::create_dir_all(root).map_err(|e| {
        format!(
            "skill_install: cannot create skills root {}: {e}",
            root.display()
        )
    })?;

    let skill_dir = root.join(SKILL_DIR_NAME);
    if let Ok(meta) = skill_dir.symlink_metadata() {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "skill_install: {} is a symlink; refusing to follow it",
                skill_dir.display()
            ));
        }
        if !meta.is_dir() {
            return Err(format!(
                "skill_install: {} exists and is not a directory",
                skill_dir.display()
            ));
        }
    }
    std::fs::create_dir_all(&skill_dir).map_err(|e| {
        format!(
            "skill_install: cannot create skill directory {}: {e}",
            skill_dir.display()
        )
    })?;

    let cli_path = cli.canonicalize().map_err(|e| {
        format!(
            "skill_install: cannot resolve executable path {}: {e}",
            cli.display()
        )
    })?;
    if !cli_path.is_file() {
        return Err(format!(
            "skill_install: resolved executable {} does not exist",
            cli_path.display()
        ));
    }
    let cli_escaped = shell_escape(&cli_path.to_string_lossy());
    let rendered = SKILL_TEMPLATE.replace(TEMPLATE_PLACEHOLDER, &cli_escaped);

    write_owned_file(&skill_dir.join("SKILL.md"), &rendered)?;
    write_owned_file(&skill_dir.join("skill.toml"), SKILL_MANIFEST)?;
    println!(
        "installed {} (SKILL.md, skill.toml) with CLI {}",
        skill_dir.display(),
        cli_path.display()
    );
    Ok(())
}

/// Write an installer-owned file, refusing to write through symlinks.
fn write_owned_file(path: &Path, contents: &str) -> Result<(), String> {
    if path.is_symlink() {
        return Err(format!(
            "skill_install: {} is a symlink; refusing to overwrite it",
            path.display()
        ));
    }
    std::fs::write(path, contents)
        .map_err(|e| format!("skill_install: cannot write {}: {e}", path.display()))
}

/// Entry point shared by the `yandex-tts` and `tts` binaries.
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(command) => command,
        Err(usage) => {
            eprintln!("yandex-tts: {usage}");
            return ExitCode::from(2);
        }
    };
    let result = run(&command, &socket_path());
    match result {
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
    fn parses_skill_install_with_root() {
        assert_eq!(
            parse_args(&args(&["skill_install", "/opt/skills"])).unwrap(),
            Command::SkillInstall(PathBuf::from("/opt/skills"))
        );
    }

    #[test]
    fn rejects_empty_and_unknown() {
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["say"])).is_err());
        assert!(parse_args(&args(&["say", "   "])).is_err());
        assert!(parse_args(&args(&["ping", "extra"])).is_err());
        assert!(parse_args(&args(&["volume"])).is_err());
        assert!(parse_args(&args(&["skill_install"])).is_err());
        assert!(parse_args(&args(&["skill_install", "a", "b"])).is_err());
    }

    #[test]
    fn rejects_empty_skill_install_path() {
        assert!(parse_args(&args(&["skill_install", ""])).is_err());
    }

    #[test]
    fn shell_escape_handles_spaces_and_quotes() {
        assert_eq!(shell_escape("plain"), "'plain'");
        assert_eq!(shell_escape("with space"), "'with space'");
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn resolve_cli_prefers_sibling_yandex_tts() {
        let dir = tempfile::TempDir::new().unwrap();
        let alias = dir.path().join("tts");
        let sibling = dir.path().join("yandex-tts");
        std::fs::write(&alias, b"").unwrap();
        std::fs::write(&sibling, b"").unwrap();
        assert_eq!(resolve_cli(&alias), sibling);
        std::fs::remove_file(&sibling).unwrap();
        assert_eq!(resolve_cli(&alias), alias);
    }
}
