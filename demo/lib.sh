#!/usr/bin/env bash
# Shared demo helper. It does exactly one thing: locate (building if needed) the agit binary and
# drop a clean symlink under $DEMO_HOME/bin. There is no "demo" logic here — the commands are the
# ones you type, and each demo's host script spells them out.

set -uo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
DEMO_HOME="${DEMO_HOME:-/tmp/agit-demo}"
BIN_DIR="$DEMO_HOME/bin"

B=$'\033[1m'; DIM=$'\033[2m'; G=$'\033[32m'; Y=$'\033[33m'; N=$'\033[0m'

# Find (compiling if necessary) agit and symlink it under $DEMO_HOME/bin.
# Prefer release, but fall back to a newer debug build so someone iterating on debug isn't served a
# stale release binary (which once silently ran an old meaning of `sync`).
_ensure_agit() {
  local rel="$ROOT/target/release/agit" dbg="$ROOT/target/debug/agit" agit=""
  if [[ -x "$rel" && -x "$dbg" ]]; then
    [[ "$dbg" -nt "$rel" ]] && agit="$dbg" || agit="$rel"
  elif [[ -x "$rel" ]]; then
    agit="$rel"
  elif [[ -x "$dbg" ]]; then
    agit="$dbg"
  fi
  if [[ -z "$agit" ]]; then
    echo "no agit binary found — building it first…" >&2
    "$ROOT/build.sh" --release >&2 || exit 1
    agit="$rel"
  fi
  mkdir -p "$BIN_DIR"
  ln -sf "$agit" "$BIN_DIR/agit"
  ln -sf "$DEMO_DIR/state.sh" "$BIN_DIR/agit-state"
  AGIT="$BIN_DIR/agit"
}
