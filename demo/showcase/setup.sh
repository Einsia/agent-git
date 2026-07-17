#!/usr/bin/env bash
# Stage for the merge showcase: one repo, TWO agents, and a conflict neither can see alone.
#   ratelimit: added a login rate limiter that buckets on `user_id`   (branch feature-a)
#   identity:  renamed the identity field `user_id` -> `uid`          (branch feature-b)
#
# They are two AGENTS, not two branches of one store — that is the whole model. An agent is a memory,
# named for what it knows, keyed by an identity, living at $AGIT_HOME/agents/<aid>/. `agit a merge
# identity` revives both (read-only, each in its own branch's worktree), lets them reconcile by reading
# the code, and leaves BOTH memories intact — they are different agents, so their histories stay
# separate. That needs REAL, resumable sessions, so we generate them with `claude -p` when claude is
# present. Without claude, the branches/code/agents are still staged and the merge act is skipped.

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

PROJ="$DEMO_HOME/showcase"
_ensure_agit
B=$'\033[1m'; G=$'\033[32m'; DIM=$'\033[2m'; Y=$'\033[33m'; N=$'\033[0m'
slug(){ echo "$1" | sed 's/[^a-zA-Z0-9]/-/g'; }
gc(){ git -c user.name="$1" -c user.email="$1@payments.io" -c commit.gpgsign=false "${@:2}"; }

# The stores live in $AGIT_HOME (lib.sh points it at the demo's own dir), so a re-run must clear both
# or the agents from last time are still there.
rm -rf "$PROJ" "$AGIT_HOME"
mkdir -p "$PROJ/src"; cd "$PROJ"
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

# Two agents, each named for what it KNOWS. Minting is the whole setup: no store is placed in this
# repo, and `agit a new` writes the committed .agit.toml binding that tells a teammate's clone which
# agents this repo works with.
"$AGIT" a new ratelimit >/dev/null
"$AGIT" a new identity  >/dev/null
# Minting activates, so `identity` is active by virtue of going last. The story is told from the
# ratelimit agent's side, and `use` is what sets this worktree's default.
"$AGIT" a use ratelimit >/dev/null
RL_STORE="$("$AGIT" a info ratelimit | awk '/^store/{print $2}')"
ID_STORE="$("$AGIT" a info identity  | awk '/^store/{print $2}')"
gc dev add -A; gc dev commit -qm 'agit: bind ratelimit + identity' >/dev/null

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
  gc dev checkout -q feature-a

  # File each session into ITS OWN agent's store — the layout `agit snap` writes.
  for pair in "ratelimit:$RL_STORE:$A_JSONL" "identity:$ID_STORE:$B_JSONL"; do
    name="${pair%%:*}"; rest="${pair#*:}"; store="${rest%%:*}"; jsonl="${rest#*:}"
    S="$store/sessions/claude-code"
    mkdir -p "$S"
    cp "$jsonl" "$S/$name-session.jsonl"
    # AGIT_AGENT selects per-shell — rung 2 of resolution, and how you drive two agents at once.
    AGIT_AGENT="$name" "$AGIT" a add -A >/dev/null
    AGIT_AGENT="$name" "$AGIT" a commit -qm "$name: its session on this codebase" >/dev/null
  done
fi

cat <<EOF

${B}Stage ready.${N}

  ${G}export PATH="$BIN_DIR:\$PATH"${N}
  ${G}export AGIT_HOME="$AGIT_HOME"${N}   ${DIM}(the demo's agents live here, not in your real ~/.agit)${N}

  Repo        ${G}$PROJ${N}
  feature-a   the ${B}ratelimit${N} agent added a limiter keyed on ${B}user_id${N}   (checked out now)
  feature-b   the ${B}identity${N} agent renamed the field ${B}user_id → uid${N}   (the cross-cutting conflict)
EOF

if [[ $HAVE_CLAUDE -eq 1 ]]; then
cat <<EOF
  Agents      ${G}ratelimit${N} and ${G}identity${N} — two memories, two stores, one repo

${DIM}Follow the host script act by act:${N}
  ${G}\$EDITOR demo/showcase/讲稿.md${N}
${DIM}Then, from feature-a, reconcile the two agents by dialogue:${N}
  ${G}cd $PROJ && agit a merge identity${N}
EOF
else
cat <<EOF

${Y}claude not found — the branches, code and agents are staged, but the live merge act needs a real
resumable session, so it's skipped. Install claude to run \`agit a merge identity\`.${N}
EOF
fi
