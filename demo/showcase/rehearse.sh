#!/usr/bin/env bash
# 彩排:非交互跑一遍新模型的三幕,确认上台不翻车。需要本机 claude(reconcile 用)。
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO_HOME="${DEMO_HOME:-/tmp/agit-demo}"
BIN="$DEMO_HOME/bin"
ALICE="$DEMO_HOME/showcase-alice"
BOB="$DEMO_HOME/showcase-bob"
ORIGIN="$DEMO_HOME/showcase-origin.git"
G=$'\033[32m'; R=$'\033[31m'; B=$'\033[1m'; N=$'\033[0m'
ok(){ echo "  ${G}✓ $*${N}"; }
bad(){ echo "  ${R}✗ $*${N}"; FAIL=1; }
FAIL=0

"$HERE/setup.sh" >/tmp/agit-rehearse-setup.log 2>&1
export PATH="$BIN:$PATH"

echo "${B}第一幕 · Alice sync + push${N}"
cd "$ALICE"
agit init >/dev/null && ok "init 两个库" || bad init
agit -a sync >/dev/null 2>&1 && [[ -n "$(find .agit/agent/sessions -name '*.jsonl')" ]] && ok "sync 镜像了原始 session" || bad sync
# 注入真密钥,确认 push 前扫描拦得住
printf '{"content":"AKIAIOSFODNN7EXAMPLE"}\n' >> .agit/agent/sessions/claude-code/alice-sess.jsonl
agit -a add -A >/dev/null
OUT="$(agit -a commit -m leak 2>&1)"; echo "$OUT" | grep -q '疑似密钥\|aws' && ok "密钥被 pre-commit 拦" || bad "secret block"
# 清掉再正常提交 + push
sed -i '/AKIAIOSFODNN7EXAMPLE/d' .agit/agent/sessions/claude-code/alice-sess.jsonl
agit -a add -A >/dev/null; ( cd .agit/agent && git commit -q -m alice )
agit -a remote add origin "$ORIGIN" 2>/dev/null
agit -a push -u origin main >/dev/null 2>&1 && ok "push 到远端" || bad push
[[ -n "$(git -C "$ORIGIN" ls-tree -r --name-only HEAD | grep jsonl)" ]] && ok "远端收到原始 session" || bad "remote sessions"

echo "${B}第二幕 · Bob clone + sync${N}"
cd "$BOB"
agit clone "$ORIGIN" >/dev/null 2>&1 && ok "clone 团队 Agent Store" || bad clone
agit -a sync >/dev/null 2>&1; agit -a add -A >/dev/null; ( cd .agit/agent && git commit -q -m bob )
agit -a fetch origin >/dev/null 2>&1 && ok "fetch 到 Alice 的会话" || bad fetch

echo "${B}第三幕 · reconcile(agent 合并,真冲突才问人)${N}"
if command -v claude >/dev/null; then
  OUT="$(agit -a reconcile origin/main 2>&1)"
  echo "$OUT" | grep -q '统一上下文' && ok "生成统一上下文 → CLAUDE.md" || bad "reconcile context"
  grep -q 'agit:begin' CLAUDE.md && ok "写入 CLAUDE.md 受管区块" || bad "claude.md"
  echo "$OUT" | grep -q '矛盾' && ok "拎出真冲突(user_id vs uid)交人裁决" || bad "conflict surfaced"
else
  echo "  (无 claude,跳过 reconcile;设 AGIT_LLM_CMD 可换后端)"
fi

echo
[[ $FAIL -eq 0 ]] && echo "${G}${B}彩排全过 —— 可以上台。${N}" || echo "${R}${B}有环节翻车。${N}"
exit $FAIL
