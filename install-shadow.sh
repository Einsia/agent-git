#!/usr/bin/env bash
# Install a "git shadow": in your interactive shell, `git` runs through `agit`, so every git
# command also versions your agent context — while the cases where agit intentionally differs from
# git (init / clone / --version / global -flags) fall straight through to real git, so plain git
# keeps working with zero surprises.
#
# Usage:  ./install-shadow.sh [path-to-rc]     (default: your shell's rc file)
#         command git ...                       always runs pure git, bypassing the shadow.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BEGIN="# >>> agit shadow >>>"
END="# <<< agit shadow <<<"

# 1. Locate the agit binary (prefer one already on PATH, else a local build).
AGIT="$(command -v agit || true)"
if [[ -z "$AGIT" ]]; then
  for c in "$ROOT/target/release/agit" "$ROOT/target/debug/agit"; do
    [[ -x "$c" ]] && AGIT="$c" && break
  done
fi
if [[ -z "$AGIT" ]]; then
  echo "agit not found. Build it first:  ./build.sh --release" >&2
  exit 1
fi

# 2. Put agit on PATH via ~/.local/bin (so the shadow works from any directory).
BIN="$HOME/.local/bin"
mkdir -p "$BIN"
[[ "$AGIT" == "$BIN/agit" ]] || ln -sf "$AGIT" "$BIN/agit"

# 3. The shadow shell function. `-*` covers global flags AND --version/-h; init/clone/version/help
#    are where agit repurposes a git verb; everything else is forwarded to agit, which passes any
#    non-native verb straight through to git (and records the Agent<->Environment pairing).
read -r -d '' BLOCK <<SH || true
$BEGIN
export PATH="\$HOME/.local/bin:\$PATH"
git() {
  case "\${1:-}" in
    -*|init|clone|version|help|"") command git "\$@" ;;
    *) agit "\$@" ;;
  esac
}
$END
SH

# 4. Pick the rc file, install idempotently (replace an existing block).
RC="${1:-}"
if [[ -z "$RC" ]]; then
  case "${SHELL##*/}" in
    zsh) RC="$HOME/.zshrc" ;;
    *) RC="$HOME/.bashrc" ;;
  esac
fi
touch "$RC"
if grep -qF "$BEGIN" "$RC"; then
  # replace the existing block in place
  tmp="$(mktemp)"
  awk -v b="$BEGIN" -v e="$END" '
    $0==b {skip=1} skip && $0==e {skip=0; next} !skip' "$RC" > "$tmp"
  printf '\n%s\n' "$BLOCK" >> "$tmp"
  cat "$tmp" > "$RC"
  rm -f "$tmp"
  echo "Updated the git shadow in $RC."
else
  printf '\n%s\n' "$BLOCK" >> "$RC"
  echo "Installed the git shadow into $RC."
fi

cat <<EOF

Done. Start a new shell, or:  source "$RC"

  git status / commit / push …   now run through agit (agent context versioned alongside)
  git init / clone / --version   stay pure git
  command git …                  pure git any time, explicitly

One-time per project:  agit init   (create the Agent Store)
EOF
