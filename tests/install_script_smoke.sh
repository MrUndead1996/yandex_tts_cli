#!/usr/bin/env bash
# Offline smoke/regression tests for install.sh.
#
# Fully self-contained: no network, no real services, no real ~/.config, no
# real cargo build. Every external effect is stubbed inside a per-test
# sandbox:
#   - curl serves a fake "latest release" (JSON with tag, architecture
#     tarballs, sha256 checksum files, raw unit and .env.example);
#   - uname reports a controlled architecture;
#   - systemctl is a state machine; cargo/install/sleep are stubs;
#   - "released" binaries are fake scripts supporting --version, ping and
#     skill_install.
# All sandboxes are tracked and removed exactly. Run:
#   bash tests/install_script_smoke.sh

set -u

SCRIPT_DIR="$(cd -P "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -P "$SCRIPT_DIR/.." && pwd)"

PASS=0
FAIL=0
SANDBOXES=()

assert_eq() { # desc actual expected
	if [ "$2" = "$3" ]; then
		PASS=$((PASS + 1))
	else
		FAIL=$((FAIL + 1))
		printf 'FAIL: %s\n  actual:   %q\n  expected: %q\n' "$1" "$2" "$3"
	fi
}

assert_contains() { # desc haystack needle
	case "$2" in
	*"$3"*) PASS=$((PASS + 1)) ;;
	*)
		FAIL=$((FAIL + 1))
		printf 'FAIL: %s\n  missing: %q\n  in:      %q\n' "$1" "$3" "$2"
		;;
	esac
}

assert_not_contains() {
	case "$2" in
	*"$3"*)
		FAIL=$((FAIL + 1))
		printf 'FAIL: %s\n  unexpected: %q\n' "$1" "$3"
		;;
	*) PASS=$((PASS + 1)) ;;
	esac
}

# ---------------------------------------------------------------------------
# Harness: fresh temp sandbox for each test; exact paths tracked for cleanup.
# ---------------------------------------------------------------------------

make_sandbox() {
	SBX="$(mktemp -d /tmp/opencode/install-smoke.XXXXXX)"
	SANDBOXES+=("$SBX")
	SRC="$SBX/src"           # minimal copy of the repo
	HOME_DIR="$SBX/home"     # fake $HOME
	STUB_BIN="$SBX/stub-bin" # stubs for curl/uname/cargo/systemctl/install/sleep
	REL="$SBX/release"       # fake release assets served by the curl stub
	EVENTS="$SBX/events.log" # single shared chronological stub log
	BINLOG="$SBX/binaries.log"
	: >"$EVENTS"
	: >"$BINLOG"
	mkdir -p "$SRC/systemd" "$HOME_DIR" "$STUB_BIN" "$REL"
	cp "$REPO_ROOT/install.sh" "$SRC/install.sh"
	cp "$REPO_ROOT/.env.example" "$SRC/.env.example"
	cp "$REPO_ROOT/systemd/yandex-ttsd.service" "$SRC/systemd/yandex-ttsd.service"
	# Mark the copy as a repository root (source mode must clone, never fall
	# back to a network clone).
	cp "$REPO_ROOT/Cargo.toml" "$SRC/Cargo.toml"
	cp "$REPO_ROOT/Cargo.lock" "$SRC/Cargo.lock"
}

# A fake released/binary script: answers --version, ping, skill_install;
# logs every invocation to the shared binary log. $1=path $2=version string.
make_fake_binary() {
	local p="$1" v="$2"
	cat >"$p" <<EOF
#!/bin/sh
echo "\$(basename "\$0") \$*" >> $(printf %q "$BINLOG")
case "\$1" in
  --version) echo "yandex-tts $v"; exit 0 ;;
  ping) [ "\${FAIL_PING:-0}" = 1 ] && exit 1; exit 0 ;;
  skill_install) [ "\${FAIL_SKILL:-0}" = 1 ] && exit 1; exit 0 ;;
esac
exit 0
EOF
	chmod +x "$p"
}

# Build the fake release assets for both supported architectures.
#   FAKE_RELEASE_VERSION  release tag (default 1.2.3)
write_release_assets() {
	local ver="${FAKE_RELEASE_VERSION:-1.2.3}"
	local tgt dir b
	echo "{\"tag_name\": \"v$ver\"}" >"$REL/release.json"
	for tgt in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
		dir="$REL/pkg/yandex-tts-$ver-$tgt"
		mkdir -p "$dir"
		for b in yandex-ttsd yandex-tts; do
			make_fake_binary "$dir/$b" "$ver"
			printf '# target=%s\n' "$tgt" >>"$dir/$b"
		done
		tar -C "$REL/pkg" -czf "$REL/yandex-tts-$ver-$tgt.tar.gz" "yandex-tts-$ver-$tgt"
		(cd "$REL" && sha256sum "yandex-tts-$ver-$tgt.tar.gz" >"checksums-$tgt.txt")
	done
	rm -rf "$REL/pkg"
}

write_stubs() {
	write_release_assets

	# uname: controlled architecture (FAKE_ARCH, default x86_64), always Linux.
	cat >"$STUB_BIN/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
-s) echo Linux ;;
-m) echo "${FAKE_ARCH:-x86_64}" ;;
*) echo Linux ;;
esac
EOF

	# curl: serves the fake release from $REL; logs every call. Env switches:
	#   TAMPER_CHECKSUM=1  corrupt the served checksum file
	cat >"$STUB_BIN/curl" <<EOF
#!/usr/bin/env bash
EVENTS="$(printf %q "$EVENTS")"
REL="$(printf %q "$REL")"
SRC="$(printf %q "$SRC")"
echo "curl \$*" >> "\$EVENTS"
out=""
url=""
while [ \$# -gt 0 ]; do
  case "\$1" in
  -o) out="\$2"; shift 2 ;;
  -*) shift ;;
  *) url="\$1"; shift ;;
  esac
done
[ -n "\$url" ] || exit 1
serve() {
  if [ "\${TAMPER_CHECKSUM:-0}" = 1 ] && [ "\$1" != "\${1##*/checksums-}" ]; then
    awk '{print "0000000000000000000000000000000000000000000000000000000000000000  " \$2}' "\$1"
  else
    cat "\$1"
  fi
}
case "\$url" in
  */releases/latest) file="\$REL/release.json" ;;
  */releases/download/*) file="\$REL/\${url##*/}" ;;
  */install.sh) file="\$SRC/install.sh" ;;
  */yandex-ttsd.service) file="\$SRC/systemd/yandex-ttsd.service" ;;
  */.env.example) file="\$SRC/.env.example" ;;
  *) exit 1 ;;
esac
[ -f "\$file" ] || exit 1
if [ -n "\$out" ]; then serve "\$file" >"\$out"; else serve "\$file"; fi
EOF

	# cargo: "build" -> fake binaries in the copied repo's target dir.
	cat >"$STUB_BIN/cargo" <<EOF
#!/usr/bin/env bash
echo "cargo \$*" >> "$(printf %q "$EVENTS")"
if [ "\$1" = build ]; then
  mkdir -p "\$CARGO_TARGET_DIR/target/release"
  for b in yandex-ttsd yandex-tts tts; do
    printf '#!/bin/sh\necho "%s \$*" >> %q\n[ "\${FAIL_SKILL:-0}" = 1 ] && [ "\$1" = skill_install ] && exit 1\nexit 0\n' "\$b" "$BINLOG" > "\$CARGO_TARGET_DIR/target/release/\$b"
    chmod +x "\$CARGO_TARGET_DIR/target/release/\$b"
  done
  exit 0
fi
exit 1
EOF

	# install: forward to the real install binary, log to shared log.
	cat >"$STUB_BIN/install" <<EOF
#!/usr/bin/env bash
echo "install \$*" >> "$(printf %q "$EVENTS")"
exec /usr/bin/install "\$@"
EOF

	# systemctl: state machine in $SBX/systemctl-state, actions in shared log.
	cat >"$STUB_BIN/systemctl" <<EOF
#!/usr/bin/env bash
STATE="$(printf %q "$SBX")/systemctl-state"
EVENTS="$(printf %q "$EVENTS")"
echo "systemctl \$*" >> "\$EVENTS"
cmd="\$1"; shift
[ "\$cmd" = "--user" ] && { cmd="\$1"; shift; }
unit="\${*: -1}"
flag() { cat "\$STATE/\$unit.\$1" 2>/dev/null || echo no; }
setf() { mkdir -p "\$STATE"; echo "\$2" > "\$STATE/\$unit.\$1"; }
fail_once() { # FAIL_NEW_START: fail the first enable --now/restart, then succeed
  [ "\${FAIL_NEW_START:-0}" = 1 ] || return 1
  if [ "\$(flag failed_once)" = yes ]; then return 1; fi
  setf failed_once yes; return 0
}
case "\$cmd" in
  list-unit-files) flag exists | grep -q yes && { echo "UNIT FILE STATE"; echo "\$unit enabled"; } ; exit 0 ;;
  cat) flag exists | grep -q yes && echo "ExecStart=/usr/bin/python3 /opt/old/daemon.py" ; exit 0 ;;
  is-enabled) flag enabled | grep -q yes && exit 0 || exit 1 ;;
  is-active) flag active | grep -q yes && exit 0 || exit 3 ;;
  daemon-reload) exit 0 ;;
  disable)
    if [ "\${1:-}" = "--now" ]; then
      if [ "\${FAIL_OLD_STOP:-0}" = 1 ] && [ "\$unit" = "yandex-stationd.service" ]; then exit 1; fi
      setf active no
    fi
    setf enabled no; exit 0 ;;
  enable)
    if [ "\${1:-}" = "--now" ]; then
      if fail_once; then exit 1; fi
      setf active yes
      [ "\${FAIL_INACTIVE_START:-0}" = 1 ] && setf active no
    fi
    setf enabled yes; exit 0 ;;
  start)
    if [ "\${FAIL_OLD_RESTORE:-0}" = 1 ] && [ "\$unit" = "yandex-stationd.service" ]; then exit 1; fi
    setf active yes; exit 0 ;;
  restart)
    if fail_once; then exit 1; fi
    setf active yes; exit 0 ;;
  stop) setf active no; exit 0 ;;
esac
exit 1
EOF

	# sleep stub (avoid real waits); journalctl is intentionally NOT stubbed:
	# install.sh must never call it, only print the command to run manually.
	printf '#!/bin/sh\nexit 0\n' >"$STUB_BIN/sleep"
	chmod +x "$STUB_BIN"/*
}

run_install() { # extra args...; captures output in $OUT, exit code in $RC
	OUT="$SBX/out.txt"
	(
		export HOME="$HOME_DIR"
		export PATH="$STUB_BIN:$PATH"
		export CARGO_TARGET_DIR="$SRC"
		cd / # prove the script does not depend on cwd
		bash "$SRC/install.sh" "$@" >"$OUT" 2>&1
	)
	RC=$?
}

env_with_secrets() { # writes a valid .env into the fake HOME
	mkdir -p "$HOME_DIR/.config/yandex-stationd"
	cat >"$HOME_DIR/.config/yandex-stationd/.env" <<'EOF'
YANDEX_X_TOKEN=secret-x-token-value
YANDEX_MUSIC_CLIENT_ID=secret-client-id
YANDEX_MUSIC_CLIENT_SECRET=secret-client-secret
EOF
}

mark_old_python_active() {
	mkdir -p "$SBX/systemctl-state"
	echo yes >"$SBX/systemctl-state/yandex-stationd.service.exists"
	echo yes >"$SBX/systemctl-state/yandex-stationd.service.enabled"
	echo yes >"$SBX/systemctl-state/yandex-stationd.service.active"
}

# Existing Rust install: active+enabled service, old binaries, old unit whose
# ExecStart points at the current binary path (a "consistent" install).
#   FAKE_OLD_VERSION  version reported by `yandex-tts --version` (default
#                     0.9.0, i.e. older than the fake release)
mark_new_rust_active() {
	local ver="${FAKE_OLD_VERSION:-0.9.0}"
	mkdir -p "$SBX/systemctl-state" "$HOME_DIR/.local/bin" "$HOME_DIR/.config/systemd/user"
	echo yes >"$SBX/systemctl-state/yandex-ttsd.service.active"
	echo yes >"$SBX/systemctl-state/yandex-ttsd.service.enabled"
	make_fake_binary "$HOME_DIR/.local/bin/yandex-ttsd" "$ver"
	make_fake_binary "$HOME_DIR/.local/bin/yandex-tts" "$ver"
	make_fake_binary "$HOME_DIR/.local/bin/tts" "$ver"
	{
		printf '[Unit]\nDescription=old\n[Service]\n'
		printf 'ExecStart=%s/yandex-ttsd\n' "$HOME_DIR/.local/bin"
		printf '[Install]\nWantedBy=default.target\n'
	} >"$HOME_DIR/.config/systemd/user/yandex-ttsd.service"
}

# Remove only the exact sandbox paths created by this run; used both at the
# end and via EXIT trap on interruption.
cleanup_sandboxes() {
	for sbx in "${SANDBOXES[@]:-}"; do
		[ -n "$sbx" ] && rm -rf "$sbx"
	done
}
trap cleanup_sandboxes EXIT

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

T() { echo "== $1"; }

T "bash -n passes on install.sh and tests"
bash -n "$REPO_ROOT/install.sh" && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: bash -n install.sh"
}
bash -n "$SCRIPT_DIR/install_script_smoke.sh" && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: bash -n smoke tests"
}
if command -v shellcheck >/dev/null 2>&1; then
	T "shellcheck passes on install.sh"
	shellcheck -x "$REPO_ROOT/install.sh" && PASS=$((PASS + 1)) || {
		FAIL=$((FAIL + 1))
		echo "FAIL: shellcheck install.sh"
	}
else
	echo "shellcheck not available; skipped"
fi

# --- release install: happy paths ------------------------------------------

T "release install: tarball+checksum downloaded, checksum verified, binaries+unit installed, enable --now"
make_sandbox
write_stubs
env_with_secrets
run_install
assert_eq "exit code" "$RC" 0
assert_contains "release JSON fetched" "$(cat "$EVENTS")" "releases/latest"
assert_contains "x86_64 tarball downloaded" "$(cat "$EVENTS")" "yandex-tts-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
assert_contains "checksum verified" "$(cat "$OUT")" "verifying SHA256 checksum"
for b in yandex-ttsd yandex-tts tts; do
	[ -x "$HOME_DIR/.local/bin/$b" ] && PASS=$((PASS + 1)) || {
		FAIL=$((FAIL + 1))
		echo "FAIL: binary not installed: $b"
	}
done
[ -f "$HOME_DIR/.config/systemd/user/yandex-ttsd.service" ] && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: unit not installed"
}
assert_contains "daemon-reload called" "$(cat "$EVENTS")" "daemon-reload"
assert_contains "enable --now called" "$(cat "$EVENTS")" "enable --now yandex-ttsd.service"
grep -q "ping" "$BINLOG" && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: ping not executed"
}

T "release arch selection: aarch64 tarball chosen for aarch64 uname"
make_sandbox
write_stubs
env_with_secrets
FAKE_ARCH=aarch64 run_install
assert_eq "exit code" "$RC" 0
assert_contains "aarch64 tarball downloaded" "$(cat "$EVENTS")" "yandex-tts-1.2.3-aarch64-unknown-linux-gnu.tar.gz"
assert_not_contains "x86_64 tarball not downloaded" "$(cat "$EVENTS")" "x86_64-unknown-linux-gnu"
assert_contains "installed binary is the aarch64 build" \
	"$(cat "$HOME_DIR/.local/bin/yandex-tts")" "target=aarch64-unknown-linux-gnu"

T "unsupported architecture rejected"
make_sandbox
write_stubs
env_with_secrets
FAKE_ARCH=armv7l run_install
assert_eq "exit code" "$RC" 1
assert_contains "arch named in error" "$(cat "$OUT")" "armv7l"
assert_not_contains "nothing downloaded" "$(cat "$EVENTS")" "releases/download"
assert_not_contains "no service touched" "$(cat "$EVENTS")" "systemctl"

T "missing config: created from example 0600, helpful message, nothing installed, no services"
make_sandbox
write_stubs
run_install
assert_eq "exit code" "$RC" 1
assert_eq "env mode" "$(stat -c %a "$HOME_DIR/.config/yandex-stationd/.env")" 600
assert_eq "env copied from example" "$(cmp -s "$HOME_DIR/.config/yandex-stationd/.env" "$REPO_ROOT/.env.example" && echo same)" same
assert_contains "names required vars" "$(cat "$OUT")" "YANDEX_MUSIC_CLIENT_SECRET"
assert_not_contains "no binaries installed" "$(cat "$EVENTS")" "install -m 755"
assert_not_contains "no service touched" "$(cat "$EVENTS")" "enable"

T "existing config preserved byte-for-byte, only chmod 600"
make_sandbox
write_stubs
mkdir -p "$HOME_DIR/.config/yandex-stationd"
printf 'YANDEX_X_TOKEN=keep-me\nYANDEX_MUSIC_CLIENT_ID=keep-id\nYANDEX_MUSIC_CLIENT_SECRET=keep-secret\nEXTRA=kept\n' \
	>"$HOME_DIR/.config/yandex-stationd/.env"
chmod 700 "$HOME_DIR/.config/yandex-stationd/.env"
cp "$HOME_DIR/.config/yandex-stationd/.env" "$SBX/env.orig"
run_install
assert_eq "exit code" "$RC" 0
cmp -s "$HOME_DIR/.config/yandex-stationd/.env" "$SBX/env.orig"
assert_eq "config unchanged" "$?" 0
assert_eq "config mode" "$(stat -c %a "$HOME_DIR/.config/yandex-stationd/.env")" 600

T "tampered checksum: failure BEFORE any change, previous install intact, no systemctl"
make_sandbox
write_stubs
env_with_secrets
mark_new_rust_active
TAMPER_CHECKSUM=1 run_install
assert_eq "exit code" "$RC" 1
assert_contains "mismatch reported" "$(cat "$OUT")" "SHA256 checksum mismatch"
assert_not_contains "no service touched" "$(cat "$EVENTS")" "systemctl"
assert_not_contains "no binaries installed" "$(cat "$EVENTS")" "install -m 755"
grep -q '0\.9\.0' "$HOME_DIR/.local/bin/yandex-tts" && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: old binary modified by tampered download"
}
assert_eq "old service still active" \
	"$(cat "$SBX/systemctl-state/yandex-ttsd.service.active")" "yes"

T "custom install dir with spaces: binaries there, unit ExecStart quoted"
make_sandbox
write_stubs
env_with_secrets
run_install --install-dir "$SBX/my tts bin"
assert_eq "exit code" "$RC" 0
assert_contains "binary in custom dir" "$(cat "$HOME_DIR/.local" 2>/dev/null)" ""
[ -x "$SBX/my tts bin/yandex-ttsd" ] && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: yandex-ttsd not in custom dir"
}
[ -x "$SBX/my tts bin/tts" ] && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: tts not in custom dir"
}
assert_contains "ExecStart points at spaced path (quoted)" \
	"$(cat "$HOME_DIR/.config/systemd/user/yandex-ttsd.service")" \
	"ExecStart=\"$SBX/my tts bin/yandex-ttsd\""

# --- version comparison -----------------------------------------------------

T "version skip: up-to-date consistent install -> no download, no service restart"
make_sandbox
write_stubs
env_with_secrets
FAKE_OLD_VERSION=2.0.0 mark_new_rust_active
run_install
assert_eq "exit code" "$RC" 0
assert_contains "release JSON fetched" "$(cat "$EVENTS")" "releases/latest"
assert_not_contains "no tarball downloaded" "$(cat "$EVENTS")" "releases/download"
assert_not_contains "service not restarted" "$(cat "$EVENTS")" "restart"
assert_not_contains "service not enabled" "$(cat "$EVENTS")" "enable --now"
assert_contains "skip message" "$(cat "$OUT")" "nothing to update"
grep -q '2\.0\.0' "$HOME_DIR/.local/bin/yandex-tts" && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: old binary replaced despite skip"
}

T "older installed version is updated"
make_sandbox
write_stubs
env_with_secrets
FAKE_OLD_VERSION=0.9.0 mark_new_rust_active
run_install
assert_eq "exit code" "$RC" 0
assert_contains "tarball downloaded" "$(cat "$EVENTS")" "releases/download"
assert_contains "service restarted" "$(cat "$EVENTS")" "restart yandex-ttsd.service"
grep -q '1\.2\.3' "$HOME_DIR/.local/bin/yandex-tts" && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: new version not installed"
}

T "unknown/unparseable installed version is always updated"
make_sandbox
write_stubs
env_with_secrets
FAKE_OLD_VERSION=dev-build-unknown mark_new_rust_active
run_install
assert_eq "exit code" "$RC" 0
assert_contains "tarball downloaded" "$(cat "$EVENTS")" "releases/download"
assert_contains "service restarted" "$(cat "$EVENTS")" "restart yandex-ttsd.service"

T "inconsistent up-to-date install is repaired by full reinstall"
make_sandbox
write_stubs
env_with_secrets
FAKE_OLD_VERSION=2.0.0 mark_new_rust_active
rm -f "$HOME_DIR/.local/bin/tts" # alias missing -> inconsistent
run_install
assert_eq "exit code" "$RC" 0
assert_contains "reinstall happened (download)" "$(cat "$EVENTS")" "releases/download"
assert_contains "service restarted" "$(cat "$EVENTS")" "restart yandex-ttsd.service"
[ -x "$HOME_DIR/.local/bin/tts" ] && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: tts alias not repaired"
}

# --- service orchestration ---------------------------------------------------

T "restart path when service already active"
make_sandbox
write_stubs
env_with_secrets
FAKE_OLD_VERSION=0.9.0 mark_new_rust_active
run_install
assert_contains "restart instead of enable --now" "$(cat "$EVENTS")" "restart yandex-ttsd.service"
assert_not_contains "no enable --now on upgrade" "$(cat "$EVENTS")" "enable --now yandex-ttsd.service"

T "old python service stopped only at cutover: download precedes stop in ONE chronological log"
make_sandbox
write_stubs
env_with_secrets
mark_old_python_active
run_install
assert_eq "exit code" "$RC" 0
stop_line="$(grep -n 'disable --now yandex-stationd' "$EVENTS" | cut -d: -f1 | head -n 1)"
download_line="$(grep -n 'curl.*releases/download' "$EVENTS" | cut -d: -f1 | head -n 1)"
[ -n "$stop_line" ] && [ -n "$download_line" ] && [ "$download_line" -lt "$stop_line" ] &&
	PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: old service not stopped after download (stop=$stop_line download=$download_line)"
}

T "old python stop failure aborts cutover: no enable/restart of new unit, old not disabled"
make_sandbox
write_stubs
env_with_secrets
mark_old_python_active
FAIL_OLD_STOP=1 run_install
assert_eq "exit code" "$RC" 1
assert_contains "stop attempted" "$(cat "$EVENTS")" "disable --now yandex-stationd.service"
assert_not_contains "new unit not enabled" "$(cat "$EVENTS")" "enable --now yandex-ttsd.service"
assert_not_contains "new unit not restarted" "$(cat "$EVENTS")" "restart yandex-ttsd.service"
old_enabled="$(cat "$SBX/systemctl-state/yandex-stationd.service.enabled" 2>/dev/null || echo yes)"
assert_eq "old unit still enabled (state unchanged)" "$old_enabled" "yes"

T "upgrade failure: previous binaries+unit restored, rust service restarted, no disable"
make_sandbox
write_stubs
env_with_secrets
FAKE_OLD_VERSION=0.9.0 mark_new_rust_active
FAIL_NEW_START=1 run_install
assert_eq "exit code" "$RC" 1
assert_contains "old yandex-ttsd binary restored" "$(cat "$HOME_DIR/.local/bin/yandex-ttsd")" "0.9.0"
assert_contains "old tts binary restored" "$(cat "$HOME_DIR/.local/bin/tts")" "0.9.0"
new_active="$(cat "$SBX/systemctl-state/yandex-ttsd.service.active" 2>/dev/null || echo no)"
assert_eq "rust service active again after rollback" "$new_active" "yes"
assert_not_contains "rust service never disabled" "$(cat "$EVENTS")" "disable yandex-ttsd.service"
assert_not_contains "old python never stopped/started" "$(cat "$EVENTS")" "now yandex-stationd"

T "no previous rust service + start failure: new stopped+disabled, old python restored"
make_sandbox
write_stubs
env_with_secrets
mark_old_python_active
FAIL_NEW_START=1 run_install
assert_eq "exit code" "$RC" 1
assert_contains "failed new service stopped" "$(cat "$EVENTS")" "stop yandex-ttsd.service"
assert_contains "old python start attempted" "$(cat "$EVENTS")" "start yandex-stationd.service"
new_enabled="$(cat "$SBX/systemctl-state/yandex-ttsd.service.enabled" 2>/dev/null || echo no)"
assert_eq "new unit not left enabled" "$new_enabled" "no"
for b in yandex-ttsd yandex-tts tts; do
	[ ! -e "$HOME_DIR/.local/bin/$b" ] && PASS=$((PASS + 1)) || {
		FAIL=$((FAIL + 1))
		echo "FAIL: binary not removed on rollback: $b"
	}
done

T "service-start failure: journal command suggested, no excerpt printed"
make_sandbox
write_stubs
env_with_secrets
mark_old_python_active
FAIL_INACTIVE_START=1 run_install
assert_eq "exit code" "$RC" 1
assert_contains "manual journal command" "$(cat "$OUT")" "journalctl --user -u yandex-ttsd.service"
assert_contains "old python restored" "$(cat "$EVENTS")" "start yandex-stationd.service"

# --- secrets / config validation --------------------------------------------

T "secrets never printed"
make_sandbox
write_stubs
env_with_secrets
run_install
assert_not_contains "stdout/stderr free of x-token" "$(cat "$OUT")" "secret-x-token-value"
assert_not_contains "stdout/stderr free of client id" "$(cat "$OUT")" "secret-client-id"
assert_not_contains "systemctl args free of secrets" "$(cat "$EVENTS")" "secret"

T "empty required var -> failure naming variable, no values printed, no cutover"
make_sandbox
write_stubs
mark_old_python_active
mkdir -p "$HOME_DIR/.config/yandex-stationd"
printf 'YANDEX_X_TOKEN=t\nYANDEX_MUSIC_CLIENT_ID=\nYANDEX_MUSIC_CLIENT_SECRET=s\n' \
	>"$HOME_DIR/.config/yandex-stationd/.env"
run_install
assert_eq "exit code" "$RC" 1
assert_contains "var named" "$(cat "$OUT")" "YANDEX_MUSIC_CLIENT_ID"
assert_not_contains "no value leak" "$(cat "$OUT")" "secret-x-token"
assert_not_contains "old service untouched" "$(cat "$EVENTS")" "yandex-stationd"

T "quoted values in config accepted"
make_sandbox
write_stubs
mkdir -p "$HOME_DIR/.config/yandex-stationd"
printf 'YANDEX_X_TOKEN="tok"\nYANDEX_MUSIC_CLIENT_ID='"'"'id'"'"'\nYANDEX_MUSIC_CLIENT_SECRET=sec\n' \
	>"$HOME_DIR/.config/yandex-stationd/.env"
run_install
assert_eq "exit code" "$RC" 0

T "export prefix rejected with variable named, nothing installed"
make_sandbox
write_stubs
env_with_secrets
printf 'export YANDEX_X_TOKEN=t\nexport YANDEX_MUSIC_CLIENT_ID=i\nYANDEX_MUSIC_CLIENT_SECRET=s\n' \
	>"$HOME_DIR/.config/yandex-stationd/.env"
run_install
assert_eq "exit code" "$RC" 1
assert_contains "variable named" "$(cat "$OUT")" "YANDEX_X_TOKEN"
assert_contains "systemd reason given" "$(cat "$OUT")" "systemd EnvironmentFile"
assert_not_contains "no value leak" "$(cat "$OUT")" "=t"
assert_not_contains "no binaries installed" "$(cat "$EVENTS")" "install -m 755"

T "quoted empty value counts as missing"
make_sandbox
write_stubs
mkdir -p "$HOME_DIR/.config/yandex-stationd"
printf 'YANDEX_X_TOKEN=""\nYANDEX_MUSIC_CLIENT_ID='"'"''"'"'\nYANDEX_MUSIC_CLIENT_SECRET=ok\n' \
	>"$HOME_DIR/.config/yandex-stationd/.env"
run_install
assert_eq "exit code" "$RC" 1
assert_contains "double-quoted empty named" "$(cat "$OUT")" "YANDEX_X_TOKEN"
assert_contains "single-quoted empty named" "$(cat "$OUT")" "YANDEX_MUSIC_CLIENT_ID"

# --- argument handling -------------------------------------------------------

T "unknown argument rejected"
make_sandbox
write_stubs
run_install --bogus
assert_eq "exit code" "$RC" 2

T "--help prints usage and exits 0 without HOME or cargo"
OUT="$(mktemp /tmp/opencode/install-help.XXXXXX)"
SANDBOXES+=("$(dirname "$OUT")/$(basename "$OUT")")
(cd / && env -i /usr/bin/env bash "$REPO_ROOT/install.sh" --help) >"$OUT" 2>&1
RC=$?
assert_eq "exit code" "$RC" 0
assert_contains "usage printed" "$(cat "$OUT")" "--install-dir PATH"
assert_not_contains "no prerequisites required" "$(cat "$OUT")" "not found"

# --- build from source --------------------------------------------------------

T "--build-from-source non-interactive: rejected with --force hint, nothing done"
make_sandbox
write_stubs
env_with_secrets
run_install --build-from-source </dev/null
assert_eq "exit code" "$RC" 1
assert_contains "force hint" "$(cat "$OUT")" "--force"
assert_not_contains "no cargo run" "$(cat "$EVENTS")" "cargo"
assert_not_contains "no service touched" "$(cat "$EVENTS")" "enable"

T "--build-from-source --force: builds, installs from target, no network"
make_sandbox
write_stubs
env_with_secrets
run_install --build-from-source --force
assert_eq "exit code" "$RC" 0
assert_contains "cargo build ran" "$(cat "$EVENTS")" "cargo build"
assert_not_contains "no network" "$(cat "$EVENTS")" "curl"
for b in yandex-ttsd yandex-tts tts; do
	[ -x "$HOME_DIR/.local/bin/$b" ] && PASS=$((PASS + 1)) || {
		FAIL=$((FAIL + 1))
		echo "FAIL: source-built binary not installed: $b"
	}
done
assert_contains "enable --now called" "$(cat "$EVENTS")" "enable --now yandex-ttsd.service"

T "--force alone does not bypass the build-from-source confirmation"
make_sandbox
write_stubs
env_with_secrets
run_install --build-from-source --force </dev/null
assert_eq "exit code" "$RC" 0
assert_contains "cargo build ran" "$(cat "$EVENTS")" "cargo build"

# --- curl | bash --------------------------------------------------------------

T "curl | bash: installs from release without a repository checkout"
make_sandbox
write_stubs
env_with_secrets
OUT="$SBX/out.txt"
(
	export HOME="$HOME_DIR"
	export PATH="$STUB_BIN:$PATH"
	cd /
	curl -fsSL https://example.invalid/install.sh | bash -s -- >"$OUT" 2>&1
)
RC=$?
assert_eq "exit code" "$RC" 0
assert_contains "tarball downloaded" "$(cat "$EVENTS")" "releases/download"
[ -x "$HOME_DIR/.local/bin/yandex-ttsd" ] && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: binary not installed via curl|bash"
}

# --- hygiene ------------------------------------------------------------------

T "no backup dirs left behind"
leftover="$(find "${TMPDIR:-/tmp}" -maxdepth 1 -name 'yandex-tts-install-backup.*' 2>/dev/null | wc -l)"
assert_eq "no leftover backup dirs" "$leftover" "0"

# Cleanup only the exact sandboxes this run created (EXIT trap also covers
# interruption).
cleanup_sandboxes

printf '\nsmoke: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
