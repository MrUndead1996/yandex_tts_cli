//! End-to-end tests of `tts skill_install <SKILLS_ROOT>` against a real
//! binary, with a temp skills root. The command is fully offline: no daemon
//! socket is needed (and none is contacted — the tests set `SOCKET_PATH` to
//! a nonexistent path to prove that).

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn run_skill_install(root: &Path) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tts"));
    cmd.arg("skill_install").arg(root);
    // Prove the command never talks to the daemon.
    cmd.env("SOCKET_PATH", "/nonexistent/tts-skill-test.sock");
    cmd.env_remove("XDG_RUNTIME_DIR");
    cmd.output().expect("run tts binary")
}

fn skill_dir(root: &Path) -> PathBuf {
    root.join("yandex-station-tts")
}

fn read_skill_md(root: &Path) -> String {
    std::fs::read_to_string(skill_dir(root).join("SKILL.md")).expect("read installed SKILL.md")
}

#[test]
fn skill_install_writes_manifest_and_rendered_skill_into_root() {
    let root = TempDir::new().unwrap();
    let output = run_skill_install(root.path());
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let manifest = std::fs::read_to_string(skill_dir(root.path()).join("SKILL.toml")).unwrap();
    assert!(manifest.contains("name = \"yandex-station-tts\""));
    assert!(manifest.contains("version = \"0.1.0\""));
    assert!(manifest.contains("template = \"SKILL.md\""));

    let skill = read_skill_md(root.path());
    assert!(skill.starts_with("---\nname: yandex-station-tts\n"));
    assert!(
        !skill.contains("{{TTS_BIN}}"),
        "placeholder must be rendered"
    );

    // The skill references the real installed CLI binary (the sibling
    // `yandex-tts` next to the `tts` alias in the cargo target dir) as a
    // shell-quoted absolute path; no placeholder and no path guessed from a
    // developer home remain.
    let expected = PathBuf::from(env!("CARGO_BIN_EXE_yandex-tts"))
        .canonicalize()
        .unwrap();
    assert!(expected.is_absolute());
    let expected = expected.to_string_lossy().to_string();
    assert!(
        skill.contains(&expected),
        "skill should reference {expected}"
    );
}

#[test]
fn skill_install_survives_root_path_with_spaces_and_apostrophe() {
    let base = TempDir::new().unwrap();
    let root = base.path().join("my skills' root");
    std::fs::create_dir_all(&root).unwrap();

    let output = run_skill_install(&root);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(skill_dir(&root).join("SKILL.md").is_file());
    assert!(skill_dir(&root).join("SKILL.toml").is_file());
    let skill = read_skill_md(&root);
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_yandex-tts"))
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(skill.contains(&exe));
}

#[test]
fn skill_install_is_offline_and_never_touches_daemon() {
    // If skill_install tried to connect, it would fail (missing socket) —
    // success itself proves it stayed offline.
    let root = TempDir::new().unwrap();
    let output = run_skill_install(root.path());
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());
}

#[test]
fn skill_install_reinstall_updates_owned_files_and_preserves_unrelated() {
    let root = TempDir::new().unwrap();
    assert!(run_skill_install(root.path()).status.success());

    let unrelated_root = root.path().join("other-skill");
    std::fs::create_dir_all(&unrelated_root).unwrap();
    std::fs::write(unrelated_root.join("SKILL.md"), "keep me").unwrap();

    let keep_note = skill_dir(root.path()).join("NOTES.local.md");
    std::fs::write(&keep_note, "user data").unwrap();

    // Modify the owned manifest so reinstall must overwrite it.
    let manifest_path = skill_dir(root.path()).join("SKILL.toml");
    std::fs::write(&manifest_path, "# clobbered").unwrap();

    assert!(run_skill_install(root.path()).status.success());

    let manifest = std::fs::read_to_string(&manifest_path).unwrap();
    assert!(manifest.contains("name = \"yandex-station-tts\""));
    assert_eq!(
        std::fs::read_to_string(unrelated_root.join("SKILL.md")).unwrap(),
        "keep me"
    );
    assert_eq!(std::fs::read_to_string(&keep_note).unwrap(), "user data");
}

#[test]
fn skill_install_rejects_symlinked_skill_dir_instead_of_following() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = outside.path().join("victim");
    std::fs::create_dir_all(&target).unwrap();
    let link = root.path().join("yandex-station-tts");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let output = run_skill_install(root.path());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("symlink"));
    assert!(
        !target.join("SKILL.md").exists(),
        "must not write through the symlink"
    );
}

#[test]
fn skill_install_requires_exactly_one_argument() {
    let exe = env!("CARGO_BIN_EXE_tts");
    for args in [
        vec!["skill_install"],
        vec!["skill_install", ""],
        vec!["skill_install", "a", "b"],
    ] {
        let output = Command::new(exe).args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "args: {args:?}");
    }
}

/// POSIX single-quote shell escaping, mirroring the installer.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Extract the `ping` example command from the installed SKILL.md.
fn rendered_ping_command(skill: &str) -> String {
    let fence = skill
        .split_once("```sh\n")
        .and_then(|(_, rest)| rest.split_once("\n```"))
        .unwrap_or_else(|| panic!("no sh code fence in skill:\n{skill}"));
    fence.0.trim().to_owned()
}

#[test]
fn rendered_command_is_shell_valid_even_from_executable_path_with_quotes() {
    // Simulate a real install where the CLI lives in a directory whose name
    // contains both a space and an apostrophe: copy the binaries there and
    // run the installed `tts` from that location.
    let dir = TempDir::new().unwrap();
    let bin_dir = dir.path().join("my bin's dir");
    std::fs::create_dir_all(&bin_dir).unwrap();
    for name in ["tts", "yandex-tts"] {
        std::fs::copy(env!("CARGO_BIN_EXE_tts"), bin_dir.join(name)).unwrap();
    }

    let root = TempDir::new().unwrap();
    let output = Command::new(bin_dir.join("tts"))
        .arg("skill_install")
        .arg(root.path())
        .env("SOCKET_PATH", "/nonexistent/tts-skill-test.sock")
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The rendered path must be the copied sibling, shell-quoted (its raw
    // form cannot appear literally because the directory name contains an
    // apostrophe — correctness is proven by executing the command below).
    let expected = bin_dir.join("yandex-tts").canonicalize().unwrap();
    let skill = read_skill_md(root.path());
    assert!(
        skill.contains(&shell_quote(&expected.to_string_lossy())),
        "expected quoted {expected:?} in skill:\n{skill}"
    );

    // Execute the generated command exactly as an agent would (via the
    // shell). It must reach the CLI and fail on the socket connection — not
    // on shell syntax. Never invokes `say`.
    let ping = rendered_ping_command(&skill);
    assert!(ping.contains("ping"));
    let sh = Command::new("sh")
        .arg("-c")
        .arg(&ping)
        .env("SOCKET_PATH", "/nonexistent/tts-skill-test.sock")
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();
    assert!(!sh.status.success(), "ping without daemon must fail");
    let stderr = String::from_utf8_lossy(&sh.stderr);
    assert!(
        stderr.contains("cannot connect"),
        "expected connect error, got: {stderr}"
    );
    assert!(
        !stderr.contains("syntax error"),
        "shell must parse the rendered command, got: {stderr}"
    );
}
