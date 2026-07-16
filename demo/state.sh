#!/usr/bin/env bash
# Run inside any agit repo to answer "what does it look like right now" — the two repos + the pairing.

set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)"
B=$'\033[1m'; DIM=$'\033[2m'; N=$'\033[0m'
A=".agit/agent"

echo "${DIM}Environment (code repo): $PWD${N}"
echo "${B}1. Environment — your code, untouched${N}"
echo "     HEAD $(git rev-parse --short HEAD 2>/dev/null || echo '(no commits)')  branch $(git branch --show-current 2>/dev/null)"
echo "     .agit/ ignored? $(git check-ignore -q .agit && echo yes || echo no)"

echo
echo "${B}2. Agent Store (.agit/agent) — an independent git repo holding the raw sessions${N}"
if [[ -d "$A/.git" ]]; then
  echo "     HEAD $(git -C "$A" rev-parse --short HEAD 2>/dev/null || echo '(no commits)')  branch $(git -C "$A" branch --show-current 2>/dev/null)"
  for rt in claude-code codex; do
    n=$(find "$A/sessions/$rt" -maxdepth 1 -name '*.jsonl' 2>/dev/null | wc -l)
    echo "     sessions/$rt: $n"
  done
  s=$(find "$A/sessions/sync" -name '*.md' 2>/dev/null | wc -l)
  echo "     sessions/sync (merge transcripts): $s"
  echo "     secret hook: $([[ -x "$A/.git/hooks/pre-commit" ]] && echo installed || echo 'not installed (run agit init after a clone)')"
else
  echo "     (none yet — run agit init)"
fi

echo
echo "${B}3. WorkspaceRevision (.agit/workspace) — the Agent<->Environment pairing${N}"
if [[ -f .agit/workspace/HEAD.json ]]; then
  python3 -c "import json;d=json.load(open('.agit/workspace/HEAD.json'));print('     latest:',d.get('trigger'),' agent',d.get('agent_rev','')[:8],' env',d.get('env',{}).get('head_commit','')[:8])" 2>/dev/null
  echo "     history: $(wc -l < .agit/workspace/log.jsonl 2>/dev/null || echo 0) revisions"
else
  echo "     (none yet — generated automatically after either repo commits)"
fi
echo
echo "${DIM}Nowhere else. It's all git objects.${N}"
