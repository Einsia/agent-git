#!/usr/bin/env bash
# Stage for the sync showcase: one repo with two diverged agent branches.
#   feature-a: an agent added a login rate limiter keyed on `user_id`
#   feature-b: an agent renamed the identity field `user_id` -> `uid`
# The two are about to merge — and their change is a cross-cutting conflict.
#
# `agit -a sync` revives BOTH agents (read-only, each in its own branch's worktree) and lets them
# reconcile by reading the code. That needs REAL, resumable sessions, so we generate them with
# `claude -p` when claude is present. Without claude, the branches/code are still staged and the
# sync act is skipped.

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

PROJ="$DEMO_HOME/showcase"
_ensure_agit
B=$'\033[1m'; G=$'\033[32m'; DIM=$'\033[2m'; Y=$'\033[33m'; N=$'\033[0m'
slug(){ echo "$1" | sed 's/[/.]/-/g'; }
gc(){ git -c user.name="$1" -c user.email="$1@payments.io" -c commit.gpgsign=false "${@:2}"; }

rm -rf "$PROJ"; mkdir -p "$PROJ/src"; cd "$PROJ"
gc dev init -q -b main .
printf 'export function getUser(req){ return req.body.user_id; }\n' > src/auth.js
gc dev add -A; gc dev commit -qm 'payments: base'
BASE=$(git branch --show-current)

# feature-a: rate limiter keyed on user_id
gc dev checkout -q -b feature-a
printf 'export function getUser(req){ return req.body.user_id; }\nexport function loginKey(req){ return req.body.user_id; } // rate-limit bucket key\n' > src/auth.js
gc dev add -A; gc dev commit -qm 'feature-a: login rate limiter on user_id'

# feature-b: rename user_id -> uid
gc dev checkout -q "$BASE"; gc dev checkout -q -b feature-b
printf 'export function getUser(req){ return req.body.uid; }\n' > src/auth.js
gc dev add -A; gc dev commit -qm 'feature-b: rename user_id -> uid'

gc dev checkout -q feature-a
"$AGIT" init >/dev/null

HAVE_CLAUDE=0
if command -v claude >/dev/null 2>&1; then
  HAVE_CLAUDE=1
  CDIR="$HOME/.claude/projects/$(slug "$PROJ")"
  rm -rf "$CDIR"   # fresh slug so we don't pick up sessions from earlier runs

  # Generate two REAL, resumable sessions — one per branch — and capture each file deterministically.
  claude -p "Record for later: on this branch you added loginKey() in src/auth.js, a login rate limiter that buckets requests on the user_id field. Reply 'ok'." >/dev/null 2>&1
  A_JSONL="$(ls -t "$CDIR"/*.jsonl 2>/dev/null | head -1)"

  gc dev checkout -q feature-b
  claude -p "Record for later: on this branch you renamed the auth identity field from user_id to uid across src/auth.js. Reply 'ok'." >/dev/null 2>&1
  B_JSONL="$(ls -t "$CDIR"/*.jsonl 2>/dev/null | grep -v "$(basename "$A_JSONL")" | head -1)"

  # Build the Agent Store deterministically: main = agent A (feature-a), bob = agent B (feature-b).
  gc dev checkout -q feature-a
  S=".agit/agent/sessions/claude-code"
  mkdir -p "$S"
  cp "$A_JSONL" "$S/alice-session.jsonl"
  gc dev -C .agit/agent add -A && gc dev -C .agit/agent commit -qm 'agent A: feature-a session' >/dev/null
  gc dev -C .agit/agent checkout -q -b bob
  cp "$B_JSONL" "$S/bob-session.jsonl"
  gc dev -C .agit/agent add -A && gc dev -C .agit/agent commit -qm 'agent B: feature-b session' >/dev/null
  gc dev -C .agit/agent checkout -q main
fi

cat <<EOF

${B}Stage ready.${N}

  ${G}export PATH="$BIN_DIR:\$PATH"${N}

  Repo        ${G}$PROJ${N}
  feature-a   an agent added a login rate limiter keyed on ${B}user_id${N}   (checked out now)
  feature-b   an agent renamed the field ${B}user_id → uid${N}   (the cross-cutting conflict)
EOF

if [[ $HAVE_CLAUDE -eq 1 ]]; then
cat <<EOF
  Agent Store ${G}main${N} = agent A's session · ${G}bob${N} = agent B's session

${DIM}Follow the host script act by act:${N}
  ${G}\$EDITOR demo/showcase/host-script.md${N}
${DIM}Then, from feature-a, reconcile the two agents by dialogue:${N}
  ${G}cd $PROJ && agit -a sync bob${N}
EOF
else
cat <<EOF

${Y}claude not found — the branches and code are staged, but the live sync act needs a real
resumable session, so it's skipped. Install claude to run \`agit -a sync bob\`.${N}
EOF
fi
