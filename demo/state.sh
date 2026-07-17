#!/usr/bin/env bash
# Run inside any agit repo to answer "what does it look like right now" — the code repo, the agents it
# works with, and the pairing between them.

set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)"
B=$'\033[1m'; DIM=$'\033[2m'; N=$'\033[0m'
AGIT="$(command -v agit || echo agit)"

echo "${DIM}Environment (code repo): $PWD${N}"
echo "${B}1. Environment — your code, untouched${N}"
echo "     HEAD $(git rev-parse --short HEAD 2>/dev/null || echo '(no commits)')  branch $(git branch --show-current 2>/dev/null)"
# Asked about a path inside it: the `.agit/` pattern has a trailing slash, so it only matches a
# directory that already exists — and .agit/ now holds only local state, created on demand.
echo "     .agit/ ignored? $(git check-ignore -q .agit/workspace/log.jsonl && echo yes || echo no)"

echo
echo "${B}2. Binding (.agit.toml) — COMMITTED: which agents this repo works with${N}"
if [[ -f .agit.toml ]]; then
  grep -vE '^\s*#|^\s*$' .agit.toml | sed 's/^/     /'
  echo "${DIM}     A teammate who clones this repo reads exactly this, then: agit a track <name>${N}"
else
  echo "     (none yet — run agit a new <name>)"
fi

echo
echo "${B}3. The agents — each a store of its own at \$AGIT_HOME/agents/<aid>/${N}"
if ! "$AGIT" a list 2>/dev/null | grep -q .; then
  echo "     (none yet — run agit a new <name>)"
else
  "$AGIT" a list 2>/dev/null | sed 's/^/     /'
  # `agit a <git…>` is plain git on whichever agent this worktree resolves to, so git itself will say
  # where that store is. An agent is a memory: keyed by an identity that never moves, reachable from
  # every repo that tracks it — never welded to this one.
  STORE="$("$AGIT" a rev-parse --show-toplevel 2>/dev/null)"
  if [[ -n "${STORE:-}" && -d "$STORE/.git" ]]; then
    echo
    echo "${DIM}     the agent this worktree resolves to:${N}"
    echo "     store  $STORE"
    echo "     HEAD   $(git -C "$STORE" rev-parse --short HEAD 2>/dev/null || echo '(no commits)')  branch $(git -C "$STORE" branch --show-current 2>/dev/null)"
    n=$(find "$STORE/sessions" -name '*.jsonl' 2>/dev/null | wc -l)
    echo "     sessions: $n   ${DIM}(across every environment this agent has worked in)${N}"
    m=$(find "$STORE/sessions/sync" -name '*.md' 2>/dev/null | wc -l)
    echo "     merge transcripts: $m"
    echo "     secret hook: $([[ -x "$STORE/.git/hooks/pre-commit" ]] && echo installed || echo 'not installed (run agit init)')"
  fi
fi

echo
echo "${B}4. WorkspaceRevision (.agit/workspace) — the Agent<->Environment pairing${N}"
if [[ -f .agit/workspace/HEAD.json ]]; then
  python3 -c "import json;d=json.load(open('.agit/workspace/HEAD.json'));print('     latest:',d.get('trigger'),' agent',d.get('agent_rev','')[:8],' env',d.get('env',{}).get('head_commit','')[:8])" 2>/dev/null
  echo "     history: $(wc -l < .agit/workspace/log.jsonl 2>/dev/null || echo 0) revisions"
else
  echo "     (none yet — generated automatically after either repo commits)"
fi
echo
echo "${DIM}Nowhere else. It's all git objects.${N}"
