#!/usr/bin/env bash
# install.sh — user installation of the Rust TTS daemon/CLI on Linux.
#
# Default mode: release install.
# - Linux only (explicit check); x86_64 / aarch64 GNU targets, matching
#   .github/workflows/release.yml asset naming:
#   yandex-tts-${VERSION}-${target}.tar.gz and checksums-${target}.txt.
# - Verifies the SHA256 checksum with an exact filename/hash line match
#   before anything is installed, and extracts only the two expected member
#   files (never a symlink/hardlink) — traversal-safe extraction.
# - Versions are validated as strict semver and compared numerically with
#   correct semver pre-release ordering (no sort -V pitfalls). Install is
#   skipped (with a clear message) only if the installed version is >= the
#   release; an unknown/unparseable installed version is always updated.
# - The systemd unit and .env.example are always taken from the release tag
#   (raw.githubusercontent), never from a possibly mismatched local checkout.
# - For `curl | bash` (no repository checkout) no git/cargo is needed.
# - --install-dir PATH overrides the binary directory (default
#   $HOME/.local/bin); the systemd unit's ExecStart is rewritten to point at
#   the chosen path, with systemd-safe quoting/escaping for spaces, quotes,
#   backslashes and % specifiers.
# - --build-from-source opts in to building from source with cargo; it
#   requires an interactive confirmation (unless --force is given) before
#   anything is cloned or built. There is no automatic fallback to building
#   from source when the release download fails.
# - OpenClaw skill installation is provided by the CLI (`tts skill_install`),
#   independently of this installer.
# - Transactional: existing binaries and unit are backed up (private temp
#   dir); the transaction flag is set only after the complete snapshot, so a
#   failure during snapshotting restores nothing and deletes nothing. On any
#   failure after that, the previous files are restored, daemon-reload runs,
#   and the previous service state is re-established (restart if it was
#   running, enable if only enabled, stop+disable if there was no previous
#   unit). An old Python yandex-stationd user service is restored only if it
#   was previously running. Nothing is ever deleted permanently and the
#   failure paths never run twice (single EXIT-trap rollback).
# - Never overwrites an existing $HOME/.config/yandex-stationd/.env; only
#   normalizes its mode to 0600. If it is missing, creates it from
#   .env.example (mode 0600) and stops, asking the user to fill in the
#   required credentials. Configuration is always validated before the
#   (source-mode) build and before any service change.
# - Secrets are never printed or passed in command arguments.
#
# Usage:
#   install.sh [OPTIONS]                    install/upgrade from a release
#   install.sh --help                       this help
#
#   curl -fsSL https://raw.githubusercontent.com/MrUndead1996/yandex_tts_cli/main/install.sh | bash -s -- [OPTIONS]
#   wget -qO- https://raw.githubusercontent.com/MrUndead1996/yandex_tts_cli/main/install.sh | bash -s -- [OPTIONS]
#
# Options (default install mode):
#   --install-dir PATH      Install binaries into PATH (default:
#                           $HOME/.local/bin). The systemd unit ExecStart is
#                           rewritten to the chosen path.
#   --build-from-source     Build the workspace from source with cargo
#                           instead of installing a release binary. Requires
#                           an interactive confirmation before clone/build;
#                           --force skips the confirmation (for scripted
#                           runs). Configuration is validated before the
#                           build starts.
#   --force                 Confirmation bypass for --build-from-source only
#                           (documented escape hatch for automation); it
#                           does not force a reinstall of an up-to-date
#                           release.
#   -h, --help              Show this help and exit.
#
# Skill mode:
# Examples:
#   install.sh                                  # latest release to ~/.local/bin
#   install.sh --install-dir /opt/tts/bin       # custom binary dir
#   tts skill_install ~/.openclaw/skills        # install OpenClaw skill offline
#   install.sh --build-from-source              # build from source (ask first)

set -euo pipefail

REPO_URL="https://github.com/MrUndead1996/yandex_tts_cli"
RAW_URL="https://raw.githubusercontent.com/MrUndead1996/yandex_tts_cli"

INSTALL_DIR=""
INSTALL_DIR_GIVEN=0
BUILD_FROM_SOURCE=0
FORCE=0

usage() {
	cat <<EOF
usage: $0 [OPTIONS]                    install/upgrade from the latest release
       $0 --help                       detailed help

Options (install mode):
  --install-dir PATH   binary directory (default: \$HOME/.local/bin)
  --build-from-source  build with cargo instead of downloading a release
                       (interactive confirmation unless --force)
  --force              confirmation bypass for --build-from-source (automation)
  -h, --help           detailed help
EOF
}

help_full() {
	cat <<EOF
install.sh — install/upgrade the yandex-tts daemon/CLI on Linux (x86_64/aarch64).

usage: $0 [OPTIONS]
       $0 --help

Default mode (release install):
  Downloads the latest GitHub release for the detected architecture,
  verifies its SHA256 checksum, compares versions with the installed
  binaries, installs yandex-ttsd, yandex-tts and the tts alias, installs
  the systemd user unit (ExecStart rewritten to the actual binary path)
  and switches the service transactionally with full rollback.

Options (install mode):
  --install-dir PATH   Install binaries into PATH (default: \$HOME/.local/bin).
                       The systemd unit ExecStart is rewritten accordingly;
                       spaces and special characters in PATH are escaped.
  --build-from-source  Build the workspace from source instead (requires the
                       repository or a fresh clone; runs
                       'cargo build --release --locked --workspace').
                       Asks for confirmation before cloning/building;
                       configuration is validated before the build.
  --force              Skip the interactive confirmation for
                       --build-from-source (intended for scripted runs).
                       It does NOT force a reinstall when already up to date.
  -h, --help           Show this help.

OpenClaw skill installation is provided by the CLI: tts skill_install PATH

Examples:
  $0                                            # latest release, ~/.local/bin
  $0 --install-dir /opt/tts/bin                 # custom binary directory
  tts skill_install ~/.openclaw/skills          # install skill (offline)
  $0 --build-from-source                        # build from source (asks first)
  $0 --build-from-source --force                # same, no confirmation (CI)

curl | bash:
  curl -fsSL https://raw.githubusercontent.com/MrUndead1996/yandex_tts_cli/main/install.sh | bash -s -- [OPTIONS]
EOF
}

# ---------------------------------------------------------------------------
# Argument parsing.
# ---------------------------------------------------------------------------
while [ $# -gt 0 ]; do
		case "$1" in
		--install-dir)
			[ $# -ge 2 ] || {
				usage
				exit 2
			}
			INSTALL_DIR="$2"
			INSTALL_DIR_GIVEN=1
			shift 2
			;;
		--build-from-source)
			BUILD_FROM_SOURCE=1
			shift
			;;
		--force)
			FORCE=1
			shift
			;;
		-h | --help)
			# Usage/help must work without HOME, cargo or any other prerequisites.
			help_full
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

# An explicitly requested but empty path is an error, not a silent skip.
if [ "$INSTALL_DIR_GIVEN" -eq 1 ] && [ -z "$INSTALL_DIR" ]; then
	echo "install.sh: --install-dir requires a non-empty PATH argument" >&2
	exit 2
fi

# Normalize --install-dir to an absolute path: a relative directory would
# produce an invalid (cwd-dependent) systemd ExecStart and an unusable skill
# reference. Spaces are fine — the value is always handled quoted.
normalize_install_dir() {
	INSTALL_DIR="${INSTALL_DIR%/}"
	case "$INSTALL_DIR" in
	/*) ;;
	*) INSTALL_DIR="$(pwd -P)/$INSTALL_DIR" ;;
	esac
}

# Default binary directory; must be set BEFORE normalization so that an
# empty INSTALL_DIR does not get expanded to "$PWD".
[ -n "$INSTALL_DIR" ] || INSTALL_DIR="$HOME/.local/bin"
normalize_install_dir
log() { printf 'install.sh: %s\n' "$*"; }
die() {
	printf 'install.sh: error: %s\n' "$*" >&2
	exit 1
}

need_cmd() {
	command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# Temporary download/staging directory (cleaned on exit, even on failure
# before the transaction starts).
# ---------------------------------------------------------------------------
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/yandex-tts-install-work.XXXXXX")"
chmod 700 "$WORK_DIR"
cleanup_work() { rm -rf "$WORK_DIR"; }
trap cleanup_work EXIT

# ---------------------------------------------------------------------------
# Resolve the repository root (source mode only). $PWD is never assumed to
# be the repo.
#
# When run via `curl ... | bash` BASH_SOURCE is empty — guard against that
# under `set -u` by checking element existence, not its value.
# ---------------------------------------------------------------------------
if [ "${BASH_SOURCE[0]+set}" = set ] && [ -n "${BASH_SOURCE[0]}" ]; then
	SCRIPT_SOURCE="${BASH_SOURCE[0]}"
	while [ -L "$SCRIPT_SOURCE" ]; do
		SCRIPT_DIR="$(cd -P "$(dirname "$SCRIPT_SOURCE")" && pwd)"
		SCRIPT_SOURCE="$(readlink "$SCRIPT_SOURCE")"
		[ "${SCRIPT_SOURCE#/}" != "$SCRIPT_SOURCE" ] || SCRIPT_SOURCE="$SCRIPT_DIR/$SCRIPT_SOURCE"
	done
	ROOT_CANDIDATE="$(cd -P "$(dirname "$SCRIPT_SOURCE")" && pwd)"
else
	# `curl ... | bash`: the script is read from stdin, no repo next to it.
	ROOT_CANDIDATE=""
fi
if [ -n "$ROOT_CANDIDATE" ] && [ -f "$ROOT_CANDIDATE/Cargo.toml" ]; then
	REPO_ROOT="$ROOT_CANDIDATE"
else
	REPO_ROOT=""
fi

# ---------------------------------------------------------------------------
# --build-from-source: explicit opt-in with interactive confirmation before
# any clone/build. Non-interactive runs must pass --force explicitly.
# There is never an automatic fallback to building from source.
# ---------------------------------------------------------------------------
if [ "$BUILD_FROM_SOURCE" -eq 1 ] && [ "$FORCE" -ne 1 ]; then
	log "--build-from-source will clone the repository and run 'cargo build --release --locked --workspace'."
	printf 'install.sh: proceed? [y/N] '
	if [ -r /dev/tty ] &&
		read -r answer </dev/tty 2>/dev/null; then
		case "$answer" in
		y | Y | yes | YES) ;;
		*)
			die "aborted by user (use --force to skip the confirmation)"
			;;
		esac
	else
		die "--build-from-source requires an interactive confirmation; re-run with --force to proceed non-interactively"
	fi
fi

# ---------------------------------------------------------------------------
# Version handling: strict semver only.
# ---------------------------------------------------------------------------

# True if $1 is a strict semver version (MAJOR.MINOR.PATCH with an optional
# -PRERELEASE; no build metadata, no leading zeros, no 'v' prefix).
is_semver() {
	local re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z][0-9A-Za-z.-]*|-0)?$'
	[[ "$1" =~ $re ]]
}

# Compare two strict semver versions; true if installed ($1) >= release ($2).
# Pre-release ordering follows semver: 1.2.3-rc.1 < 1.2.3. To never silently
# skip an update, equal cores with two different pre-release strings are
# treated as "installed is older" (update).
version_ge() {
	local imin irest imaj ipatch ipre
	local rmin rrest rpatch rpre
	imaj="${1%%.*}"
	irest="${1#*.}"
	imin="${irest%%.*}"
	irest="${irest#*.}"
	ipatch="${irest%%-*}"
	if [ "$irest" = "$ipatch" ]; then ipre=""; else ipre="${irest#*-}"; fi
	rmaj="${2%%.*}"
	rrest="${2#*.}"
	rmin="${rrest%%.*}"
	rrest="${rrest#*.}"
	rpatch="${rrest%%-*}"
	if [ "$rrest" = "$rpatch" ]; then rpre=""; else rpre="${rrest#*-}"; fi
	if [ "$imaj" -ne "$rmaj" ]; then [ "$imaj" -gt "$rmaj" ]; return; fi
	if [ "$imin" -ne "$rmin" ]; then [ "$imin" -gt "$rmin" ]; return; fi
	if [ "$ipatch" -ne "$rpatch" ]; then [ "$ipatch" -gt "$rpatch" ]; return; fi
	# Cores equal (semver: a pre-release sorts before the stable release).
	if [ -z "$ipre" ]; then
		return 0 # installed stable >= anything with equal core
	fi
	# Equal pre-release strings are the same version -> skip.
	if [ "$ipre" = "$rpre" ]; then
		return 0
	fi
	# Installed pre-release vs stable release, or two distinct pre-release
	# strings: conservatively older (update rather than skip).
	return 1
}

# Installed version, parsed strictly from `yandex-tts --version` output:
# the output must be exactly "yandex-tts <semver>"; anything else (including
# a non-executable/absent binary) counts as "unknown version".
installed_version() {
	local out token
	[ -x "$INSTALL_DIR/yandex-tts" ] || return 0
	out="$("$INSTALL_DIR/yandex-tts" --version 2>/dev/null || true)"
	[ "$(printf '%s\n' "$out" | wc -l)" -eq 1 ] || return 0
	case "$out" in
	"yandex-tts "*)
		token="${out#"yandex-tts "}"
		if is_semver "$token"; then
			printf '%s\n' "$token"
		fi
		;;
	esac
}

# Map uname to the release target triple (Linux GNU only, matching
# .github/workflows/release.yml).
detect_target() {
	[ "$(uname -s)" = "Linux" ] ||
		die "unsupported operating system: $(uname -s) (only Linux is supported)"
	case "$(uname -m)" in
	x86_64 | amd64) printf 'x86_64-unknown-linux-gnu\n' ;;
	aarch64 | arm64) printf 'aarch64-unknown-linux-gnu\n' ;;
	*) die "unsupported architecture: $(uname -m) (only Linux x86_64 and aarch64 GNU are supported)" ;;
	esac
}

# The systemd ExecStart value for a binary path: percent-escaped and, for
# paths with spaces/quotes, double-quoted.
execstart_string() {
	local path="$1"
	path="${path//%/%%}"
	path="${path//\\/\\\\}"
	case "$path" in
	*[[:space:]\"\']*)
		path="\"${path//\"/\\\"}\""
		;;
	esac
	printf '%s' "$path"
}

# A skipped update is only safe when the previous install is complete and
# consistent: the tts alias exists and the installed unit's ExecStart refers
# to the current absolute binary path. Otherwise the "up-to-date" binary
# would leave the CLI alias and service pointing at a stale/missing install,
# so the caller repairs by reinstalling.
install_is_consistent() {
	local expected
	[ -x "$INSTALL_DIR/yandex-tts" ] || return 1
	[ -x "$INSTALL_DIR/tts" ] || return 1
	[ -f "$UNIT_DIR/$UNIT_NAME" ] || return 1
	expected="ExecStart=$(execstart_string "$INSTALL_DIR/yandex-ttsd")"
	grep -Fqx "$expected" "$UNIT_DIR/$UNIT_NAME"
}

# Replace the unit's ExecStart with the chosen binary path, systemd-safely
# (percent escaping, quoting for spaces/special characters).
rendered_unit() {
	UNIT_EXEC_START="$(execstart_string "$1")" awk '
		/^ExecStart=/ { print "ExecStart=" ENVIRON["UNIT_EXEC_START"]; next }
		{ print }
	' "$2"
}

# ---------------------------------------------------------------------------
# Configuration validation. Runs before the source-mode build and before any
# service change; never prints values.
# ---------------------------------------------------------------------------
CONFIG_DIR="$HOME/.config/yandex-stationd"
ENV_FILE="$CONFIG_DIR/.env"
UNIT_DIR="$HOME/.config/systemd/user"
UNIT_NAME="yandex-ttsd.service"
OLD_UNIT_NAME="yandex-stationd.service"

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

validate_config() {
	if [ ! -e "$ENV_FILE" ]; then
		if [ ! -f "$WORK_DIR/.env.example" ]; then
			die ".env.example not available"
		fi
		mkdir -p "$CONFIG_DIR"
		install -m 600 "$WORK_DIR/.env.example" "$ENV_FILE"
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

	if ! check_required_env; then
		die "required variables are missing or empty in $ENV_FILE (file was NOT modified or printed):${missing}"
	fi
	log "config $ENV_FILE validated (values not shown)"
}

RELEASE_VERSION=""
if [ "$BUILD_FROM_SOURCE" -eq 0 ]; then
	# =========================================================================
	# Release mode
	# =========================================================================
	need_cmd systemctl
	need_cmd install
	need_cmd tar
	need_cmd sha256sum
	if command -v curl >/dev/null 2>&1; then
		fetch() { curl -fsSL -o "$2" "$1"; }
	elif command -v wget >/dev/null 2>&1; then
		fetch() { wget -qO "$2" "$1"; }
	else
		die "required command not found: curl (or wget)"
	fi

	TARGET="$(detect_target)"

	log "resolving latest release (target $TARGET)"
	api_json="$WORK_DIR/release.json"
	fetch "https://api.github.com/repos/MrUndead1996/yandex_tts_cli/releases/latest" "$api_json" ||
		die "cannot resolve the latest release (network error or no release published yet)"
	RELEASE_TAG="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$api_json" | head -n 1)"
	[ -n "$RELEASE_TAG" ] || die "cannot parse the latest release tag from the GitHub API response"
	# The tag is used in URLs and archive member names: strict charset only.
	case "$RELEASE_TAG" in
	*[!A-Za-z0-9._-]*) die "invalid characters in release tag: $RELEASE_TAG" ;;
	esac
	RELEASE_VERSION="${RELEASE_TAG#v}"
	is_semver "$RELEASE_VERSION" ||
		die "release tag '$RELEASE_TAG' is not a strict semver version"

	# Version comparison: skip only an up-to-date AND consistent install
	# (tts alias present, unit ExecStart pointing at the current absolute
	# binary path); anything else is repaired with a full reinstall.
	# Unknown installed versions are always updated.
	CURRENT_VERSION="$(installed_version)"
	if [ -n "$CURRENT_VERSION" ] && version_ge "$CURRENT_VERSION" "$RELEASE_VERSION" &&
		install_is_consistent; then
		log "installed version $CURRENT_VERSION is >= release $RELEASE_VERSION; nothing to update"
		exit 0
	fi

	ASSET_BASE="yandex-tts-${RELEASE_VERSION}-${TARGET}"
	TARBALL="$WORK_DIR/${ASSET_BASE}.tar.gz"
	CHECKSUMS="$WORK_DIR/checksums-${TARGET}.txt"
	BASE_URL="$REPO_URL/releases/download/$RELEASE_TAG"

	log "downloading $ASSET_BASE.tar.gz"
	fetch "$BASE_URL/${ASSET_BASE}.tar.gz" "$TARBALL" ||
		die "cannot download ${ASSET_BASE}.tar.gz from release $RELEASE_TAG"
	log "downloading checksums-${TARGET}.txt"
	fetch "$BASE_URL/checksums-${TARGET}.txt" "$CHECKSUMS" ||
		die "cannot download checksums-${TARGET}.txt from release $RELEASE_TAG"

	# Verify the checksum line with an exact filename match, exactly two
	# whitespace-separated tokens and a strict 64-hex-digit hash (any extra
	# token makes the entry ambiguous -> refuse).
	log "verifying SHA256 checksum"
	expected=""
	while read -r sum name rest; do
		if [ "$name" = "${ASSET_BASE}.tar.gz" ]; then
			if [ -n "$rest" ]; then
				die "malformed checksum entry for ${ASSET_BASE}.tar.gz (extra tokens)"
			fi
			expected="$sum"
			break
		fi
	done <"$CHECKSUMS"
	[ -n "$expected" ] ||
		die "checksum line for ${ASSET_BASE}.tar.gz not found in checksums-${TARGET}.txt"
	[[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]] ||
		die "malformed checksum entry for ${ASSET_BASE}.tar.gz"
	# sha256sum -c exits non-zero on mismatch; output is suppressed so no
	# partial hash is echoed (and pipefail has nothing to bite on).
	if ! printf '%s  %s\n' "$expected" "$TARBALL" | sha256sum -c --status; then
		die "SHA256 checksum mismatch for ${ASSET_BASE}.tar.gz (download corrupted or tampered)"
	fi

	# Traversal-safe extraction: pull ONLY the two expected member files from
	# the well-defined archive directory, refusing to follow any symlink.
	mkdir -p "$WORK_DIR/pkg" "$WORK_DIR/stage"
	if ! tar -xzf "$TARBALL" -C "$WORK_DIR/pkg" --no-same-owner \
		"${ASSET_BASE}/yandex-tts" "${ASSET_BASE}/yandex-ttsd"; then
		die "cannot extract the expected files from ${ASSET_BASE}.tar.gz (unexpected archive layout)"
	fi
	for b in yandex-ttsd yandex-tts; do
		f="$WORK_DIR/pkg/$ASSET_BASE/$b"
		if [ -L "$f" ] || [ ! -f "$f" ]; then
			die "archive member $b is missing or not a regular file; refusing to install"
		fi
		cp "$f" "$WORK_DIR/stage/$b"
	done
	# The release ships only yandex-tts and yandex-ttsd; `tts` is a local copy.
	cp "$WORK_DIR/stage/yandex-tts" "$WORK_DIR/stage/tts"
	chmod 755 "$WORK_DIR/stage/"*

	# Unit and .env.example: always from the release tag so they match the
	# downloaded binaries exactly (a local checkout may be a different
	# version, especially when upgrading).
	log "fetching systemd unit and .env.example from tag $RELEASE_TAG"
	fetch "$RAW_URL/$RELEASE_TAG/systemd/yandex-ttsd.service" "$WORK_DIR/yandex-ttsd.service" ||
		die "cannot download systemd/yandex-ttsd.service from tag $RELEASE_TAG"
	fetch "$RAW_URL/$RELEASE_TAG/.env.example" "$WORK_DIR/.env.example" ||
		die "cannot download .env.example from tag $RELEASE_TAG"

	# Configuration is validated before any file installation or service
	# change (release binaries are already staged and verified).
	validate_config
else
	# =========================================================================
	# Source mode (explicit --build-from-source)
	# =========================================================================
	need_cmd cargo
	need_cmd systemctl
	need_cmd install
	if [ -z "$REPO_ROOT" ]; then
		# curl|bash: clone into the work dir (already confirmed above).
		need_cmd git
		log "cloning repository into $WORK_DIR/src"
		git clone -q --depth 1 "$REPO_URL.git" "$WORK_DIR/src" ||
			die "cannot clone repository"
		REPO_ROOT="$WORK_DIR/src"
	fi
	[ -f "$REPO_ROOT/systemd/yandex-ttsd.service" ] ||
		die "$REPO_ROOT/systemd/yandex-ttsd.service not found"
	cp "$REPO_ROOT/systemd/yandex-ttsd.service" "$WORK_DIR/yandex-ttsd.service"
	[ -f "$REPO_ROOT/.env.example" ] ||
		die "$REPO_ROOT/.env.example not found"
	cp "$REPO_ROOT/.env.example" "$WORK_DIR/.env.example"

	# Configuration is validated BEFORE the (potentially long) build.
	validate_config

	log "building workspace (cargo build --release --locked --workspace)"
	(cd "$REPO_ROOT" && cargo build --release --locked --workspace) || die "cargo build failed"

	mkdir -p "$WORK_DIR/stage"
	for b in yandex-ttsd yandex-tts tts; do
		f="$REPO_ROOT/target/release/$b"
		if [ -L "$f" ] || [ ! -f "$f" ]; then
			die "build did not produce target/release/$b"
		fi
		cp "$f" "$WORK_DIR/stage/$b"
	done
fi

# Render the final unit with the actual binary path in ExecStart.
rendered_unit "$INSTALL_DIR/yandex-ttsd" "$WORK_DIR/yandex-ttsd.service" >"$WORK_DIR/unit.rendered"

# ---------------------------------------------------------------------------
# Transaction: capture state, back up existing files. TXN_STARTED is set
# only after the backup snapshot is fully complete, so a failure before or
# during the snapshot can never trigger a rollback: nothing is restored and
# nothing existing is removed (the backup dir is only ever deleted wholesale).
# ---------------------------------------------------------------------------

TXN_STARTED=0
BACKUP_DIR=""
old_rust_active=0
old_rust_enabled=0
old_python_active=0
old_python_enabled=0

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

finish() {
	rc=$?
	trap - EXIT
	cleanup_work
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
			install -m 755 "$BACKUP_DIR/$b" "$INSTALL_DIR/$b"
		elif [ -e "$INSTALL_DIR/$b" ]; then
			rm -f "$INSTALL_DIR/$b"
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

BACKUP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/yandex-tts-install-backup.XXXXXX")"
chmod 700 "$BACKUP_DIR"
for b in yandex-ttsd yandex-tts tts; do
	if [ -f "$INSTALL_DIR/$b" ]; then
		cp -p "$INSTALL_DIR/$b" "$BACKUP_DIR/$b"
	fi
done
if [ -f "$UNIT_DIR/$UNIT_NAME" ]; then
	cp -p "$UNIT_DIR/$UNIT_NAME" "$BACKUP_DIR/$UNIT_NAME"
fi

# Snapshot complete: from this point on, failures are rolled back.
TXN_STARTED=1

# Install new binaries and unit (the service is not restarted yet, so a
# running old instance keeps its in-memory image until the switch below).
mkdir -p "$INSTALL_DIR" "$UNIT_DIR"
for b in yandex-ttsd yandex-tts tts; do
	install -m 755 "$WORK_DIR/stage/$b" "$INSTALL_DIR/$b"
done
install -m 644 "$WORK_DIR/unit.rendered" "$UNIT_DIR/$UNIT_NAME"
systemctl --user daemon-reload

# ---------------------------------------------------------------------------
# Cutover: stop the old Python service; abort if that fails.
# ---------------------------------------------------------------------------

if [ "$old_python_active" -eq 1 ]; then
	log "stopping old Python service $OLD_UNIT_NAME"
	if ! systemctl --user disable --now "$OLD_UNIT_NAME"; then
		die "failed to stop/disable $OLD_UNIT_NAME; aborting cutover"
	fi
fi

# ---------------------------------------------------------------------------
# Switch the Rust service: restart if running, otherwise enable --now.
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
# Non-audio verification (ping only; no 'say', no repeated TTS).
# ---------------------------------------------------------------------------

if "$INSTALL_DIR/yandex-tts" ping >/dev/null 2>&1; then
	log "ping: daemon reachable"
else
	log "note: ping failed or daemon not connected yet (network/station may still be connecting);"
	log "check later with: $INSTALL_DIR/yandex-tts ping && journalctl --user -u $UNIT_NAME"
fi

if [ -n "$RELEASE_VERSION" ]; then
	log "installed yandex-tts $RELEASE_VERSION into $INSTALL_DIR"
else
	log "installed yandex-tts (from source) into $INSTALL_DIR"
fi
exit 0
