#!/usr/bin/env bash
# 在任意 agit 仓库里跑，回答「现在是什么样」——两个库 + 配对。

set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)"
B=$'\033[1m'; DIM=$'\033[2m'; N=$'\033[0m'
A=".agit/agent"

echo "${DIM}Environment（代码仓库）: $PWD${N}"
echo "${B}1. Environment —— 你的代码，原封不动${N}"
echo "     HEAD $(git rev-parse --short HEAD 2>/dev/null || echo '(无提交)')  分支 $(git branch --show-current 2>/dev/null)"
echo "     .agit/ 被忽略？ $(git check-ignore -q .agit && echo 是 || echo 否)"

echo
echo "${B}2. Agent Store（.agit/agent）—— 独立 git 仓库，装 AgentState${N}"
if [[ -d "$A/.git" ]]; then
  echo "     HEAD $(git -C "$A" rev-parse --short HEAD 2>/dev/null || echo '(无提交)')  分支 $(git -C "$A" branch --show-current 2>/dev/null)"
  n=$(find "$A/state/facts" -name '*.md' ! -name '.*' 2>/dev/null | wc -l)
  echo "     fact: $n 条"
  find "$A/state/facts" -name '*.md' ! -name '.*' 2>/dev/null | sort | sed "s|^$A/state/facts/||; s|\.md$||" | sed 's/^/       /'
  echo "     merge driver: $(git -C "$A" config --get merge.agit.driver >/dev/null && echo 已装 || echo '未装（clone 后需 agit init）')"
else
  echo "     （还没有。跑 agit init）"
fi

echo
echo "${B}3. WorkspaceRevision（.agit/workspace）—— Agent↔Environment 配对${N}"
if [[ -f .agit/workspace/HEAD.json ]]; then
  python3 -c "import json;d=json.load(open('.agit/workspace/HEAD.json'));print('     最新:',d.get('trigger'),' agent',d.get('agent_rev','')[:8],' env',d.get('env',{}).get('head_commit','')[:8])" 2>/dev/null
  echo "     历史: $(wc -l < .agit/workspace/log.jsonl 2>/dev/null || echo 0) 条"
else
  echo "     （还没有。任一库 commit 后自动生成）"
fi
echo
echo "${DIM}没有别的地方。全部是 git 对象。${N}"
