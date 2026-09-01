#!/usr/bin/env bash
#
# One-shot build and install for agit.
#
# Why this exists: building this crate has three "you only find out afterwards" traps — it
# needs rustc >= 1.88, rusqlite's bundled feature compiles C, and the cargo that rustup
# installs is often not on PATH. All three point the wrong way in their native errors (a
# syntax error, a link failure, command not found), so they are caught up front and stated
# in one sentence a person can read.
#
# Usage: see --help.

set -euo pipefail

# The script's own directory is the crate root. Not $PWD: this script is called from CI,
# from the home directory, from an editor, and cargo only looks at the Cargo.toml in the
# current directory.
#
# Parameter expansion rather than dirname: a dirname failure (which really happens when
# PATH is incomplete) silently degrades the path to the empty string, and `cd ""` is a
# successful no-op in bash — which lands back on $PWD, exactly what this avoids. Pure bash
# expansion depends on no external command.
_self="${BASH_SOURCE[0]}"
case "$_self" in
	*/*) _self_dir="${_self%/*}" ;;
	# No slash means it was invoked from the current directory (bash setup.sh).
	*)   _self_dir='.' ;;
esac
SCRIPT_DIR="$(cd -- "$_self_dir" && pwd -P)"

# The observed floor, one notch above the 1.85 that edition 2024 itself asks for: the
# locked darling 0.23 and instability 0.3.12 (a ratatui dependency) both declare
# rust-version = 1.88, and on 1.85.1 cargo refuses to build at all. Neither error (the
# edition one, the rust-version one) points at "upgrade the toolchain", so this blocks it
# explicitly.
readonly MIN_RUSTC_MAJOR=1
readonly MIN_RUSTC_MINOR=88

BUILD_PROFILE=release
RUN_TESTS=0
VERIFY=1

# ── Output ──────────────────────────────────────────────────────────────
# Color and symbols only when stdout is attached to a terminal. The other end of a pipe may
# be a CI log or grep, where ANSI sequences and multi-byte symbols are only noise — the same
# policy the crate's own src/ui/ follows.
if [ -t 1 ]; then
	C_RESET=$'\033[0m'; C_DIM=$'\033[90m'; C_RED=$'\033[31m'
	C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'; C_BOLD=$'\033[1m'
	SYM_OK='✓'; SYM_ERR='✗'; SYM_WARN='!'; SYM_ARROW='→'
else
	C_RESET=''; C_DIM=''; C_RED=''; C_GREEN=''; C_YELLOW=''; C_BOLD=''
	SYM_OK='[ok]'; SYM_ERR='[err]'; SYM_WARN='[warn]'; SYM_ARROW='->'
fi

ok()   { printf '%s %s\n' "${C_GREEN}${SYM_OK}${C_RESET}" "$1"; }
info() { printf '%s\n' "${C_DIM}${SYM_ARROW} $1${C_RESET}"; }
warn() { printf '%s %s\n' "${C_YELLOW}${SYM_WARN}${C_RESET}" "$1" >&2; }

# Errors go to stderr and exit immediately: every precondition failure is unrecoverable.
die() {
	printf '%s %s\n' "${C_RED}${SYM_ERR}${C_RESET}" "$1" >&2
	shift
	# The remaining arguments are "how to fix it" — more useful than the error itself, and
	# the indentation sets them apart.
	for line in "$@"; do
		printf '  %s\n' "${C_DIM}${line}${C_RESET}" >&2
	done
	exit 1
}

usage() {
	cat <<'EOF'
Usage: setup.sh [options]

Build agit and install it to ~/.local/bin/agit.

Options:
  --debug        Build debug instead of release. release turns on lto + opt-level=z,
                 so a full build takes a few minutes; use this while changing code.
  --test         Run cargo test --lib before installing.
  --no-verify    Skip the post-install `agit --version` self-check.
  -h, --help     Show this help.

Environment variables:
  AGIT_INSTALL_DIR   Install directory, default ~/.local/bin
  CARGO_TARGET_DIR   cargo output directory; the script follows it to the binary
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
		--debug)     BUILD_PROFILE=debug ;;
		--test)      RUN_TESTS=1 ;;
		--no-verify) VERIFY=0 ;;
		-h|--help)   usage; exit 0 ;;
		*)           die "unknown argument: $1" "see setup.sh --help" ;;
	esac
	shift
done

# ── Preconditions ───────────────────────────────────────────────────────

# rustup installs cargo in ~/.cargo/bin, and a non-login shell (cron, docker exec, some
# editor terminals) often has not sourced ~/.cargo/env. Source it here first, then decide
# "not installed".
if ! command -v cargo >/dev/null 2>&1; then
	if [ -f "$HOME/.cargo/env" ]; then
		# shellcheck disable=SC1091
		. "$HOME/.cargo/env"
	fi
fi

command -v cargo >/dev/null 2>&1 || die "cargo not found" \
	"install Rust: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" \
	"if it is already installed: source \$HOME/.cargo/env"
command -v rustc >/dev/null 2>&1 || die "rustc not found (but cargo is)" \
	"the toolchain is broken; run rustup default stable to repair it"

# rustc --version has the form `rustc 1.91.0 (hash date)`; take the first two parts of the
# second field.
RUSTC_VERSION="$(rustc --version | awk '{print $2}')"
RUSTC_MAJOR="${RUSTC_VERSION%%.*}"
RUSTC_REST="${RUSTC_VERSION#*.}"
RUSTC_MINOR="${RUSTC_REST%%.*}"
# Strip the nightly/beta suffix (1.92.0-nightly) before comparing, or the numeric comparison
# blows up.
RUSTC_MINOR="${RUSTC_MINOR%%-*}"

if [ "$RUSTC_MAJOR" -lt "$MIN_RUSTC_MAJOR" ] ||
   { [ "$RUSTC_MAJOR" -eq "$MIN_RUSTC_MAJOR" ] && [ "$RUSTC_MINOR" -lt "$MIN_RUSTC_MINOR" ]; }; then
	die "rustc is too old: got ${RUSTC_VERSION}, need >= ${MIN_RUSTC_MAJOR}.${MIN_RUSTC_MINOR}" \
		"agit uses edition 2024; an old toolchain reports an unintelligible edition error" \
		"upgrade: rustup update stable"
fi

# rusqlite has bundled on, so the cc crate compiles sqlite's C source in. With no C compiler
# the error comes from inside a build script, buried deep, so this catches it one layer
# earlier.
CC_FOUND=''
for candidate in "${CC:-}" cc clang gcc; do
	[ -n "$candidate" ] || continue
	if command -v "$candidate" >/dev/null 2>&1; then
		CC_FOUND="$candidate"
		break
	fi
done
[ -n "$CC_FOUND" ] || die "no C compiler found (cc / clang / gcc)" \
	"rusqlite's bundled feature compiles the sqlite source itself; there is no way around it" \
	"macOS: xcode-select --install" \
	"Debian/Ubuntu: sudo apt install build-essential"

ok "toolchain rustc ${RUSTC_VERSION}, C compiler ${CC_FOUND}"

# ── Build ───────────────────────────────────────────────────────────────

cd "$SCRIPT_DIR"

# No Cargo.toml beside it means the script was moved away or turned into a symlink (pwd -P
# resolves the directory, not the symlink itself). Say so early, or cargo complains in a
# harder-to-read way.
[ -f "$SCRIPT_DIR/Cargo.toml" ] || die "no Cargo.toml under $SCRIPT_DIR" \
	"setup.sh stays in the agent-git crate root; do not copy it away and do not symlink it"

# With a Cargo.lock present, lock to it. This repository commits the lockfile: whoever
# installs gets the same dependency versions as the author, not whatever cargo upgrades on
# the way past. A scalar rather than an array: under the bash 3.2 that ships with macOS,
# set -u reports an empty array expansion as an unbound variable.
LOCKED=''
if [ -f Cargo.lock ]; then
	LOCKED='--locked'
fi

if [ "$RUN_TESTS" -eq 1 ]; then
	# --lib only. Without --lib, doctests and integration tests run too and time out on NFS
	# (see docs/01_setup.md), while the lib tests already cover the whole logic.
	info "running tests (cargo test --lib)"
	cargo test ${LOCKED:+$LOCKED} --lib
fi

TARGET_DIR="${CARGO_TARGET_DIR:-$SCRIPT_DIR/target}"

if [ "$BUILD_PROFILE" = release ]; then
	info "building release (lto + opt-level=z, a few minutes the first time)"
	cargo build ${LOCKED:+$LOCKED} --release
else
	info "building debug"
	cargo build ${LOCKED:+$LOCKED}
fi

BUILT_BIN="$TARGET_DIR/$BUILD_PROFILE/agit"
[ -x "$BUILT_BIN" ] || die "the build reported success, but $BUILT_BIN is missing or not executable" \
	"if CARGO_TARGET_DIR is set, check that it points where you think it does"

# ── Install ─────────────────────────────────────────────────────────────

INSTALL_DIR="${AGIT_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$INSTALL_DIR"
DEST="$INSTALL_DIR/agit"

# Write a temporary file in the same directory, then mv: within one filesystem mv is an
# atomic rename, and a running old agit process keeps reading its own inode, which is never
# written over. Overwriting with cp directly can let a running process read half a file.
TMP_DEST="$(mktemp "$INSTALL_DIR/.agit.XXXXXX")"
trap 'rm -f "$TMP_DEST"' EXIT
cp "$BUILT_BIN" "$TMP_DEST"
chmod 755 "$TMP_DEST"
mv -f "$TMP_DEST" "$DEST"
trap - EXIT

ok "installed ${C_BOLD}${DEST}${C_RESET}"

# ── Self-check ──────────────────────────────────────────────────────────

if [ "$VERIFY" -eq 1 ]; then
	# --version rather than a bare run: clap sets arg_required_else_help, so no arguments
	# prints help and exits non-zero, which set -e misreads as a failure.
	if VERSION_OUT="$("$DEST" --version 2>&1)"; then
		ok "self-check passed: ${VERSION_OUT}"
	else
		die "the installed binary does not run: ${VERSION_OUT}" \
			"architecture mismatch or a missing dependency; rebuild with --debug for the full error"
	fi
fi

# ── PATH ────────────────────────────────────────────────────────────────

# The hint is skipped only when the directory really is on PATH. Match with a colon on both
# sides so ~/.local/bin does not match a prefix like ~/.local/bin-old.
case ":${PATH}:" in
	*":${INSTALL_DIR}:"*) ;;
	*)
		# The rc file follows $SHELL. $SHELL is the user's registered login shell, which is
		# more reliable than the current process's shell — this script itself always runs
		# under bash.
		_login_shell="${SHELL:-}"
		case "${_login_shell##*/}" in
			zsh)  RC="$HOME/.zshrc" ;;
			bash) if [ "$(uname -s)" = Darwin ]; then RC="$HOME/.bash_profile"; else RC="$HOME/.bashrc"; fi ;;
			fish) RC="$HOME/.config/fish/config.fish" ;;
			*)    RC="your shell startup file" ;;
		esac
		warn "${INSTALL_DIR} is not on PATH, so agit cannot be called by name yet"
		printf '  %s\n' "${C_DIM}add it to ${RC}, then open a new terminal:${C_RESET}" >&2
		if [ "${RC##*/}" = config.fish ]; then
			printf '  %s\n' "${C_BOLD}fish_add_path ${INSTALL_DIR}${C_RESET}" >&2
		else
			printf '  %s\n' "${C_BOLD}export PATH=\"${INSTALL_DIR}:\$PATH\"${C_RESET}" >&2
		fi
		printf '  %s\n' "${C_DIM}for now, use the full path: ${DEST} --help${C_RESET}" >&2
		;;
esac
