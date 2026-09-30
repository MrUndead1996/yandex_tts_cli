#!/usr/bin/env bash
# Smoke tests for install.sh using temporary sandboxes and stubs for
# cargo/systemctl/install. Never touches real services, real ~/.config or the
# real cargo build: only install.sh, .env.example and the unit file are copied
# into a temp dir; all external effects are faked and every sandbox is tracked
# and removed exactly. Run: bash tests/install_script_smoke.sh

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
	STUB_BIN="$SBX/stub-bin" # stubs for cargo/systemctl/install
	EVENTS="$SBX/events.log" # single shared chronological stub log
	BINLOG="$SBX/binaries.log"
	: >"$EVENTS"
	: >"$BINLOG"
	mkdir -p "$SRC/systemd" "$HOME_DIR" "$STUB_BIN"
	cp "$REPO_ROOT/install.sh" "$SRC/install.sh"
	cp "$REPO_ROOT/.env.example" "$SRC/.env.example"
	cp "$REPO_ROOT/systemd/yandex-ttsd.service" "$SRC/systemd/yandex-ttsd.service"
}

write_stubs() {
	# cargo: "build" -> create fake binaries in the copied repo's target dir;
	# logs to the shared chronological log.
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

mark_new_rust_active() { # existing Rust service: active, enabled, with binaries and unit
	mkdir -p "$SBX/systemctl-state" "$HOME_DIR/.local/bin" "$HOME_DIR/.config/systemd/user"
	echo yes >"$SBX/systemctl-state/yandex-ttsd.service.active"
	echo yes >"$SBX/systemctl-state/yandex-ttsd.service.enabled"
	printf 'old-ttsd-binary' >"$HOME_DIR/.local/bin/yandex-ttsd"
	printf 'old-tts-binary' >"$HOME_DIR/.local/bin/yandex-tts"
	printf 'old-tts-alias' >"$HOME_DIR/.local/bin/tts"
	printf 'old-unit-content\n' >"$HOME_DIR/.config/systemd/user/yandex-ttsd.service"
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

T "missing config: created from example 0600, helpful message, no build/services"
make_sandbox
write_stubs
run_install
assert_eq "exit code" "$RC" 1
assert_eq "env mode" "$(stat -c %a "$HOME_DIR/.config/yandex-stationd/.env")" 600
assert_eq "env copied from example" "$(cmp -s "$HOME_DIR/.config/yandex-stationd/.env" "$REPO_ROOT/.env.example" && echo same)" same
assert_contains "names required vars" "$(cat "$OUT")" "YANDEX_MUSIC_CLIENT_SECRET"
assert_not_contains "no build ran" "$(cat "$EVENTS")" "cargo build"
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

T "full install: binaries+unit installed, daemon-reload, enable --now"
make_sandbox
write_stubs
env_with_secrets
run_install
assert_eq "exit code" "$RC" 0
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

T "restart path when service already active"
make_sandbox
write_stubs
env_with_secrets
mark_new_rust_active
run_install
assert_contains "restart instead of enable --now" "$(cat "$EVENTS")" "restart yandex-ttsd.service"
assert_not_contains "no enable --now on upgrade" "$(cat "$EVENTS")" "enable --now yandex-ttsd.service"

T "old python service stopped only at cutover: build precedes stop in ONE chronological log"
make_sandbox
write_stubs
env_with_secrets
mark_old_python_active
run_install
assert_eq "exit code" "$RC" 0
stop_line="$(grep -n 'disable --now yandex-stationd' "$EVENTS" | cut -d: -f1)"
build_line="$(grep -n 'cargo build' "$EVENTS" | cut -d: -f1)"
[ -n "$stop_line" ] && [ -n "$build_line" ] && [ "$build_line" -lt "$stop_line" ] &&
	PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: old service not stopped after build (stop=$stop_line build=$build_line)"
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
mark_new_rust_active
FAIL_NEW_START=1 run_install
assert_eq "exit code" "$RC" 1
assert_eq "old yandex-ttsd binary restored" "$(cat "$HOME_DIR/.local/bin/yandex-ttsd")" "old-ttsd-binary"
assert_eq "old tts binary restored" "$(cat "$HOME_DIR/.local/bin/yandex-tts")" "old-tts-binary"
assert_eq "old tts alias restored" "$(cat "$HOME_DIR/.local/bin/tts")" "old-tts-alias"
assert_eq "old unit restored" "$(cat "$HOME_DIR/.config/systemd/user/yandex-ttsd.service")" "old-unit-content"
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

T "skill_install failure before cutover: old python never stopped, nothing switched"
make_sandbox
write_stubs
env_with_secrets
mark_old_python_active
export FAIL_SKILL=1
run_install --skills-root "$SBX/skills"
unset FAIL_SKILL
assert_eq "exit code" "$RC" 1
assert_not_contains "old python not stopped" "$(cat "$EVENTS")" "now yandex-stationd"
assert_not_contains "new unit not enabled" "$(cat "$EVENTS")" "enable --now yandex-ttsd.service"
assert_not_contains "new unit not restarted" "$(cat "$EVENTS")" "restart yandex-ttsd.service"
assert_contains "skill_install attempted" "$(cat "$BINLOG")" "skill_install"

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

T "--skills-root invokes installed tts skill_install with the exact path"
make_sandbox
write_stubs
env_with_secrets
run_install --skills-root "$SBX/skills"
assert_contains "skill_install invoked" "$(cat "$BINLOG")" "skill_install"
grep -q "skill_install.*$SBX/skills" "$BINLOG" && PASS=$((PASS + 1)) || {
	FAIL=$((FAIL + 1))
	echo "FAIL: skills root path not passed: $(grep skill_install "$BINLOG")"
}

T "without --skills-root OpenClaw untouched"
make_sandbox
write_stubs
env_with_secrets
run_install
assert_not_contains "no skill_install" "$(cat "$BINLOG")" "skill_install"

T "unknown argument rejected"
make_sandbox
write_stubs
run_install --bogus
assert_eq "exit code" "$RC" 2

T "--help prints usage and exits 0 without HOME or cargo"
OUT="$(mktemp /tmp/opencode/install-help.XXXXXX)"
SANDBOXES+=("$(dirname "$OUT")/$(basename "$OUT")")
( cd / && env -i /usr/bin/env bash "$REPO_ROOT/install.sh" --help ) >"$OUT" 2>&1
RC=$?
assert_eq "exit code" "$RC" 0
assert_contains "usage printed" "$(cat "$OUT")" "--skills-root PATH"
assert_not_contains "no prerequisites required" "$(cat "$OUT")" "not found"

T "--skills-root '' fails before build instead of silently skipping"
make_sandbox
write_stubs
env_with_secrets
run_install --skills-root ''
assert_eq "exit code" "$RC" 2
assert_contains "actionable message" "$(cat "$OUT")" "--skills-root requires a non-empty PATH"
assert_not_contains "no build ran" "$(cat "$EVENTS")" "cargo build"

T "export prefix rejected with variable named, no build"
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
assert_not_contains "no build ran" "$(cat "$EVENTS")" "cargo build"

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

T "service-start failure: journal command suggested, no excerpt printed"
make_sandbox
write_stubs
env_with_secrets
mark_old_python_active
FAIL_INACTIVE_START=1 run_install
assert_eq "exit code" "$RC" 1
assert_contains "manual journal command" "$(cat "$OUT")" "journalctl --user -u yandex-ttsd.service"
assert_contains "old python restored" "$(cat "$EVENTS")" "start yandex-stationd.service"

T "no backup dirs left behind"
leftover="$(find "${TMPDIR:-/tmp}" -maxdepth 1 -name 'yandex-tts-install-backup.*' 2>/dev/null | wc -l)"
assert_eq "no leftover backup dirs" "$leftover" "0"

# Cleanup only the exact sandboxes this run created (EXIT trap also covers
# interruption).
cleanup_sandboxes

printf '\nsmoke: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
