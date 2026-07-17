#!/usr/bin/env bash
# Rehearsal: run the showcase non-interactively to confirm it won't break on stage.
# The live merge act needs a local `claude`; without it, that act is skipped (like on stage).
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"        # DEMO_HOME, BIN_DIR, and $AGIT_HOME pointed at the demo's own dir
BIN="$BIN_DIR"
PROJ="$DEMO_HOME/showcase"
G=$'\033[32m'; R=$'\033[31m'; B=$'\033[1m'; N=$'\033[0m'
ok(){ echo "  ${G}✓ $*${N}"; }
bad(){ echo "  ${R}✗ $*${N}"; FAIL=1; }
FAIL=0

"$HERE/setup.sh" >/tmp/agit-rehearse-setup.log 2>&1
export PATH="$BIN:$PATH"
cd "$PROJ"

echo "${B}Act 1 · an agent is a memory, keyed by identity${N}"
# The store is NOT in this repo: it is at $AGIT_HOME/agents/<aid>/, which is what lets the same agent
# carry its memory into another repo.
RL_STORE="$(agit a info ratelimit | awk '/^store/{print $2}')"
[[ "$RL_STORE" == "$AGIT_HOME/agents/agt_"* ]] \
  && ok "the store is keyed by the agent's identity, not by this repo's path" || bad "id-keyed store (got: $RL_STORE)"
[[ ! -e .agit/agent ]] \
  && ok "nothing nested in the code repo — the old .agit/agent store is gone" || bad "no nested store"
grep -q 'id     = "agt_' .agit.toml \
  && ok ".agit.toml is committed, so a teammate's clone learns which agents this repo uses" || bad "binding committed"

echo "${B}Act 2 · raw sessions versioned + the secret gate${N}"
[[ -n "$(find "$RL_STORE/sessions" -name '*.jsonl' 2>/dev/null)" ]] \
  && ok "the raw session is versioned in the agent's store" || bad "session captured"
# a real AWS key in a session must be blocked by the pre-commit hook
LEAK="$RL_STORE/sessions/LEAK.jsonl"
echo '{"type":"user","message":{"content":"AKIAIOSFODNN7EXAMPLE"}}' > "$LEAK"
AGIT_AGENT=ratelimit agit a add -A >/dev/null
OUT="$(AGIT_AGENT=ratelimit agit a commit -m leak 2>&1)"
echo "$OUT" | grep -qiE "suspected secrets|aws" && ok "pre-commit blocked the leaked secret" || bad "secret block"
rm -f "$LEAK"; AGIT_AGENT=ratelimit agit a add -A >/dev/null; git -C "$RL_STORE" reset -q

echo "${B}Act 3 · two diverged branches, two agents${N}"
git rev-parse --verify -q feature-a >/dev/null && git rev-parse --verify -q feature-b >/dev/null \
  && ok "feature-a (user_id limiter) and feature-b (uid rename) both present" || bad "branches"
[[ "$(agit a info ratelimit | awk '/^aid/{print $2}')" != "$(agit a info identity | awk '/^aid/{print $2}')" ]] \
  && ok "ratelimit and identity are different agents — different aids, different stores" || bad "two agents"

echo "${B}Act 4 · merge — the two agents reconcile by dialogue${N}"
ID_STORE="$(agit a info identity | awk '/^store/{print $2}')"
if command -v claude >/dev/null 2>&1 && [[ -n "$(find "$ID_STORE/sessions" -name '*.jsonl' 2>/dev/null)" ]]; then
  ok "both agents carry a session on this codebase"
  OUT="$(agit a merge identity 2>&1)"   # non-interactive: surfaces open conflicts and exits non-zero
  echo "$OUT" | grep -q "is a different agent" && ok "chose the mode by IDENTITY: different agent → dialogue only" || bad "mode by identity"
  echo "$OUT" | grep -q "Two worktrees" && ok "each agent grounded in its own branch's worktree" || bad "worktrees"
  echo "$OUT" | grep -q "Merged state" && ok "produced a resumable merged session" || bad "merged state"
  echo "$OUT" | grep -qiE "CONFLICT|user_id|uid" && ok "surfaced the cross-cutting conflict (user_id vs uid)" || bad "conflict surfaced"
  echo "$OUT" | grep -q "claude --resume" && ok "printed the resume command" || bad "resume command"
  # A different agent must survive the merge whole: dialogue reconciles memories, it does not consume one.
  agit a info identity >/dev/null 2>&1 && ok "both agents still intact after the merge" || bad "both intact"
else
  echo "  (no claude, or no staged session — skipping the live merge act)"
fi

echo
[[ $FAIL -eq 0 ]] && echo "${G}${B}Rehearsal passed — ready for the stage.${N}" || echo "${R}${B}Something broke.${N}"
exit $FAIL
