#!/usr/bin/env bash
# Rehearsal: run the showcase non-interactively to confirm it won't break on stage.
# The live sync act needs a local `claude`; without it, that act is skipped (like on stage).
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO_HOME="${DEMO_HOME:-/tmp/agit-demo}"
BIN="$DEMO_HOME/bin"
PROJ="$DEMO_HOME/showcase"
G=$'\033[32m'; R=$'\033[31m'; B=$'\033[1m'; N=$'\033[0m'
ok(){ echo "  ${G}✓ $*${N}"; }
bad(){ echo "  ${R}✗ $*${N}"; FAIL=1; }
FAIL=0

"$HERE/setup.sh" >/tmp/agit-rehearse-setup.log 2>&1
export PATH="$BIN:$PATH"
cd "$PROJ"

echo "${B}Act 1 · raw sessions versioned + the secret gate${N}"
[[ -n "$(find .agit/agent/sessions -name '*.jsonl')" ]] \
  && ok "the raw session is versioned in the Agent Store (via agit -a snap)" || bad "session captured"
# a real AWS key in a session must be blocked by the pre-commit hook
echo '{"type":"user","message":{"content":"AKIAIOSFODNN7EXAMPLE"}}' > .agit/agent/sessions/claude-code/LEAK.jsonl
agit -a add -A >/dev/null
OUT="$(agit -a commit -m leak 2>&1)"
echo "$OUT" | grep -qiE "suspected secrets|aws" && ok "pre-commit blocked the leaked secret" || bad "secret block"
rm -f .agit/agent/sessions/claude-code/LEAK.jsonl; agit -a add -A >/dev/null; git -C .agit/agent reset -q

echo "${B}Act 2 · two diverged agent branches${N}"
git rev-parse --verify -q feature-a >/dev/null && git rev-parse --verify -q feature-b >/dev/null \
  && ok "feature-a (user_id limiter) and feature-b (uid rename) both present" || bad "branches"

echo "${B}Act 3 · sync — the two agents reconcile by dialogue${N}"
if command -v claude >/dev/null 2>&1 && git -C .agit/agent rev-parse --verify -q bob >/dev/null; then
  ok "both agents' sessions staged (store main = A, bob = B)"
  OUT="$(agit -a merge bob 2>&1)"   # non-interactive: surfaces open conflicts and exits non-zero
  echo "$OUT" | grep -q "Two worktrees" && ok "each agent grounded in its own branch's worktree" || bad "worktrees"
  echo "$OUT" | grep -q "Merged state" && ok "produced a resumable merged session" || bad "merged state"
  echo "$OUT" | grep -qiE "CONFLICT|user_id|uid" && ok "surfaced the cross-cutting conflict (user_id vs uid)" || bad "conflict surfaced"
  echo "$OUT" | grep -q "claude --resume" && ok "printed the resume command" || bad "resume command"
else
  echo "  (no claude, or no staged session — skipping the live sync act)"
fi

echo
[[ $FAIL -eq 0 ]] && echo "${G}${B}Rehearsal passed — ready for the stage.${N}" || echo "${R}${B}Something broke.${N}"
exit $FAIL
