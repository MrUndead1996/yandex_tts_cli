#!/usr/bin/env bash
# install.sh — user installation of the Rust TTS daemon/CLI on Linux.
#
# - Builds the workspace with `cargo build --release --locked --workspace`.
# - Installs yandex-ttsd, yandex-tts, tts to $HOME/.local/bin (paths matching
#   ExecStart=%h/.local/bin/yandex-ttsd in the systemd user unit).
# - Installs the systemd user unit to $HOME/.config/systemd/user/yandex-ttsd.service,
#   runs daemon-reload and enables/starts (or restarts) the service.
# - Transactional: existing binaries and unit are backed up (private temp dir,
#   cleaned up on exit); on any failure after the transaction begins the
#   previous files are restored, daemon-reload runs, and the previous service
#   state is re-established (restart if it was running, enable if only
#   enabled, stop+disable if there was no previous unit). An old Python
#   yandex-stationd user service is restored only if it was previously
#   running. Nothing is ever deleted permanently and the failure paths never
#   run twice (single EXIT-trap rollback).
# - Never overwrites an existing $HOME/.config/yandex-stationd/.env; only
#   normalizes its mode to 0600. If it is missing, creates it from .env.example
#   (mode 0600) and stops, asking the user to fill in the required credentials.
# - The old Python service is stopped (cutover) only after build, file
#   installation and the offline skill installation; if stopping it fails,
#   the installation aborts and everything is rolled back.
# - Secrets are never printed or passed in command arguments.
#
# Usage: install.sh [--skills-root PATH]
#   --skills-root PATH  Explicitly request OpenClaw skill installation into
#                       PATH via `tts skill_install PATH`. Without the flag the
#                       script does not touch OpenClaw at all.

set -euo pipefail

usage() {
	echo "usage: $0 [--skills-root PATH]" >&2
}

SKILLS_ROOT=""
SKILLS_ROOT_GIVEN=0
while [ $# -gt 0 ]; do
	case "$1" in
	--skills-root)
		[ $# -ge 2 ] || usage
		SKILLS_ROOT="$2"
		SKILLS_ROOT_GIVEN=1
		shift 2
		;;
	-h | --help)
		# Usage/help must work without HOME, cargo or any other prerequisites.
		usage
		exit 0
		;;
	*)
		echo "install.sh: unknown argument: $1" >&2
		usage
		exit 2
		;;
	esac
done

if [ -z "${HOME:-}" ]; then
	echo "install.sh: HOME is not set" >&2
	exit 1
fi

# An explicitly requested but empty skills root is an error, not a silent skip.
if [ "$SKILLS_ROOT_GIVEN" -eq 1 ] && [ -z "$SKILLS_ROOT" ]; then
	echo "install.sh: --skills-root requires a non-empty PATH argument" >&2
	exit 2
fi

# Resolve the repository root from the script location so the script works
# from any cwd. $PWD is never assumed to be the repo.
SCRIPT_SOURCE="${BASH_SOURCE[0]}"
while [ -L "$SCRIPT_SOURCE" ]; do
	SCRIPT_DIR="$(cd -P "$(dirname "$SCRIPT_SOURCE")" && pwd)"
	SCRIPT_SOURCE="$(readlink "$SCRIPT_SOURCE")"
	[ "${SCRIPT_SOURCE#/}" != "$SCRIPT_SOURCE" ] || SCRIPT_SOURCE="$SCRIPT_DIR/$SCRIPT_SOURCE"
done
REPO_ROOT="$(cd -P "$(dirname "$SCRIPT_SOURCE")" && pwd)"

BIN_DIR="$HOME/.local/bin"
CONFIG_DIR="$HOME/.config/yandex-stationd"
ENV_FILE="$CONFIG_DIR/.env"
UNIT_DIR="$HOME/.config/systemd/user"
UNIT_NAME="yandex-ttsd.service"
OLD_UNIT_NAME="yandex-stationd.service"
UNIT_SRC="$REPO_ROOT/systemd/$UNIT_NAME"
ENV_EXAMPLE="$REPO_ROOT/.env.example"

# Transaction state (used by the single EXIT-trap rollback).
TXN_STARTED=0
BACKUP_DIR=""
old_rust_active=0
old_rust_enabled=0
old_python_active=0
old_python_enabled=0

log() { printf 'install.sh: %s\n' "$*"; }
die() {
	printf 'install.sh: error: %s\n' "$*" >&2
	exit 1
}

need_cmd() {
	command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

need_cmd cargo
need_cmd systemctl
need_cmd install

# ---------------------------------------------------------------------------
# Rollback (single EXIT trap; runs at most once, never on success).
# ---------------------------------------------------------------------------

finish() {
	rc=$?
	trap - EXIT
	if [ "$rc" -eq 0 ] || [ "$TXN_STARTED" -eq 0 ]; then
		# Success or failure before anything was modified: nothing to restore.
		if [ -n "$BACKUP_DIR" ] && [ -d "$BACKUP_DIR" ]; then
			rm -rf "$BACKUP_DIR"
		fi
		exit "$rc"
	fi

	log "installation failed (exit $rc); rolling back to the previous state"

	# Restore previous binaries: from backup if they existed, otherwise
	# remove the freshly installed ones.
	for b in yandex-ttsd yandex-tts tts; do
		if [ -f "$BACKUP_DIR/$b" ]; then
			install -m 755 "$BACKUP_DIR/$b" "$BIN_DIR/$b"
		elif [ -e "$BIN_DIR/$b" ]; then
			rm -f "$BIN_DIR/$b"
		fi
	done

	# Restore previous unit (or remove it if there was none), then reload.
	if [ -f "$BACKUP_DIR/$UNIT_NAME" ]; then
		install -m 644 "$BACKUP_DIR/$UNIT_NAME" "$UNIT_DIR/$UNIT_NAME"
	else
		rm -f "$UNIT_DIR/$UNIT_NAME"
	fi
	systemctl --user daemon-reload >/dev/null 2>&1 || true

	# Re-establish the previous Rust service state. A restart brings back the
	# restored (previous) binary and unit definition.
	if [ "$old_rust_active" -eq 1 ]; then
		systemctl --user restart "$UNIT_NAME" >/dev/null 2>&1 ||
			log "warning: could not restart $UNIT_NAME; check 'journalctl --user -u $UNIT_NAME'"
	elif [ "$old_rust_enabled" -eq 1 ]; then
		systemctl --user enable "$UNIT_NAME" >/dev/null 2>&1 || true
	else
		systemctl --user stop "$UNIT_NAME" >/dev/null 2>&1 || true
		systemctl --user disable "$UNIT_NAME" >/dev/null 2>&1 || true
	fi

	# Restore the old Python service only if it was running before cutover.
	if [ "$old_python_active" -eq 1 ]; then
		log "restoring old Python service $OLD_UNIT_NAME"
		if [ "$old_python_enabled" -eq 1 ]; then
			systemctl --user enable "$OLD_UNIT_NAME" >/dev/null 2>&1 || true
		fi
		systemctl --user start "$OLD_UNIT_NAME" >/dev/null 2>&1 ||
			log "warning: could not restart $OLD_UNIT_NAME; check 'journalctl --user -u $OLD_UNIT_NAME'"
	fi

	if [ -n "$BACKUP_DIR" ] && [ -d "$BACKUP_DIR" ]; then
		rm -rf "$BACKUP_DIR"
	fi
	log "rollback complete"
	exit "$rc"
}
trap finish EXIT

# ---------------------------------------------------------------------------
# 1. Configuration: preserve/validate before any build or service changes.
# ---------------------------------------------------------------------------

if [ ! -e "$ENV_FILE" ]; then
	if [ ! -f "$ENV_EXAMPLE" ]; then
		die "$ENV_EXAMPLE not found in the repository"
	fi
	mkdir -p "$CONFIG_DIR"
	install -m 600 "$ENV_EXAMPLE" "$ENV_FILE"
	log "created $ENV_FILE from .env.example"
	log "edit it now and fill in the required variables, then re-run this script:"
	log "  YANDEX_X_TOKEN"
	log "  YANDEX_MUSIC_CLIENT_ID"
	log "  YANDEX_MUSIC_CLIENT_SECRET"
	log "values must not be passed on the command line; edit the file directly."
	exit 1
fi

# Existing config is preserved exactly; only the mode is normalized.
chmod 600 "$ENV_FILE"

# Check required variables against the documented systemd EnvironmentFile
# syntax (`KEY=value`, optional surrounding quotes) without printing values and
# without sourcing the (untrusted) .env file. An `export ` prefix is rejected
# because systemd's EnvironmentFile does not support it; a quoted empty value
# counts as missing.
check_required_env() {
	missing=""
	exported=""
	for var in YANDEX_X_TOKEN YANDEX_MUSIC_CLIENT_ID YANDEX_MUSIC_CLIENT_SECRET; do
		# shellcheck disable=SC2016
		if grep -Eq '^[[:space:]]*export[[:space:]]+"?'"$var"'\"?[[:space:]]*=' "$ENV_FILE"; then
			exported="$exported $var"
			continue
		fi
		# shellcheck disable=SC2016
		value="$(sed -n -E \
			's/^[[:space:]]*"?'"$var"'\"?[[:space:]]*=[[:space:]]*(.*)$/\1/p' \
			"$ENV_FILE" | tail -n 1 || true)"
		case "$value" in
		'"'*)
			value="${value%\"}"
			value="${value#\"}"
			;;
		"'"*)
			value="${value%\'}"
			value="${value#\'}"
			;;
		esac
		if [ -z "$value" ]; then
			missing="$missing $var"
		fi
	done
	if [ -n "$exported" ]; then
		die "'export' prefix is not supported by systemd EnvironmentFile (see $ENV_FILE); remove 'export' for:${exported}"
	fi
	[ -z "$missing" ]
}

if ! check_required_env; then
	die "required variables are missing or empty in $ENV_FILE (file was NOT modified or printed):${missing}"
fi
log "config $ENV_FILE validated (values not shown)"

# ---------------------------------------------------------------------------
# 2. Build.
# ---------------------------------------------------------------------------

log "building workspace (cargo build --release --locked --workspace)"
(cd "$REPO_ROOT" && cargo build --release --locked --workspace) || die "cargo build failed"

for b in yandex-ttsd yandex-tts tts; do
	[ -f "$REPO_ROOT/target/release/$b" ] || die "build did not produce target/release/$b"
done

# ---------------------------------------------------------------------------
# 3. Transaction: capture state, back up existing files. TXN_STARTED is set
#    only after the backup snapshot is complete, so a failure before that can
#    never trigger a rollback with an empty/partial backup.
# ---------------------------------------------------------------------------

if systemctl --user is-enabled "$UNIT_NAME" >/dev/null 2>&1; then
	old_rust_enabled=1
fi
if systemctl --user is-active --quiet "$UNIT_NAME" 2>/dev/null; then
	old_rust_active=1
fi

if systemctl --user list-unit-files "$OLD_UNIT_NAME" 2>/dev/null | grep -q "$OLD_UNIT_NAME"; then
	# Only treat it as the old Python daemon if its ExecStart mentions python.
	if systemctl --user cat "$OLD_UNIT_NAME" 2>/dev/null | grep -qi '^ExecStart=.*python'; then
		if systemctl --user is-enabled "$OLD_UNIT_NAME" >/dev/null 2>&1; then
			old_python_enabled=1
		fi
		if systemctl --user is-active --quiet "$OLD_UNIT_NAME" 2>/dev/null; then
			old_python_active=1
		fi
	fi
fi

BACKUP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/yandex-tts-install-backup.XXXXXX")"
chmod 700 "$BACKUP_DIR"
for b in yandex-ttsd yandex-tts tts; do
	if [ -f "$BIN_DIR/$b" ]; then
		cp -p "$BIN_DIR/$b" "$BACKUP_DIR/$b"
	fi
done
if [ -f "$UNIT_DIR/$UNIT_NAME" ]; then
	cp -p "$UNIT_DIR/$UNIT_NAME" "$BACKUP_DIR/$UNIT_NAME"
fi

# Snapshot complete: from this point on, failures are rolled back.
TXN_STARTED=1

# Install new binaries and unit (the service is not restarted yet, so a
# running old instance keeps its in-memory image until the switch below).
mkdir -p "$BIN_DIR" "$UNIT_DIR"
install -m 755 "$REPO_ROOT/target/release/yandex-ttsd" "$BIN_DIR/yandex-ttsd"
install -m 755 "$REPO_ROOT/target/release/yandex-tts" "$BIN_DIR/yandex-tts"
install -m 755 "$REPO_ROOT/target/release/tts" "$BIN_DIR/tts"
[ -f "$UNIT_SRC" ] || die "$UNIT_SRC not found in the repository"
install -m 644 "$UNIT_SRC" "$UNIT_DIR/$UNIT_NAME"
systemctl --user daemon-reload

# ---------------------------------------------------------------------------
# 4. Optional explicit skill installation (offline; done before any service
#    switch so a failure here cannot leave a broken cutover behind — the
#    EXIT trap restores the previous binaries/unit).
# ---------------------------------------------------------------------------

if [ -n "$SKILLS_ROOT" ]; then
	log "installing OpenClaw skill into $SKILLS_ROOT"
	"$BIN_DIR/tts" skill_install "$SKILLS_ROOT" ||
		die "tts skill_install $SKILLS_ROOT failed"
	log "OpenClaw skill installed"
fi

# ---------------------------------------------------------------------------
# 5. Cutover: stop the old Python service; abort if that fails.
# ---------------------------------------------------------------------------

if [ "$old_python_active" -eq 1 ]; then
	log "stopping old Python service $OLD_UNIT_NAME"
	if ! systemctl --user disable --now "$OLD_UNIT_NAME"; then
		die "failed to stop/disable $OLD_UNIT_NAME; aborting cutover"
	fi
fi

# ---------------------------------------------------------------------------
# 6. Switch the Rust service: restart if running, otherwise enable --now.
# ---------------------------------------------------------------------------

if [ "$old_rust_active" -eq 1 ]; then
	log "restarting $UNIT_NAME"
	systemctl --user restart "$UNIT_NAME" || die "failed to restart $UNIT_NAME"
else
	log "enabling and starting $UNIT_NAME"
	systemctl --user enable --now "$UNIT_NAME" || die "failed to enable/start $UNIT_NAME"
fi

# Wait briefly for the service to be active (it must not crash on startup).
sleep 1
if ! systemctl --user is-active --quiet "$UNIT_NAME" 2>/dev/null; then
	# Do not print journal contents (other units' output may contain secrets);
	# point the user at the command instead.
	log "$UNIT_NAME is not active after start; inspect the journal manually:"
	log "  journalctl --user -u $UNIT_NAME -n 20 --no-pager"
	die "installation failed; rolling back"
fi

# ---------------------------------------------------------------------------
# 7. Non-audio verification (ping only; no 'say', no repeated TTS).
# ---------------------------------------------------------------------------

if "$BIN_DIR/yandex-tts" ping >/dev/null 2>&1; then
	log "ping: daemon reachable"
else
	log "note: ping failed or daemon not connected yet (network/station may still be connecting);"
	log "check later with: $BIN_DIR/yandex-tts ping && journalctl --user -u $UNIT_NAME"
fi

# Success: drop the rollback (the trap itself removes the backup dir).
exit 0
