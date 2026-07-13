#!/usr/bin/env bash
# 彩排：非交互地把 README 的每一幕跑一遍，确认上台不会翻车。
# 默认跳过 --summarize（省得依赖 claude 登录）；SUMMARIZE=1 连它一起跑。
#
#   ./demo/showcase/rehearse.sh
#   SUMMARIZE=1 ./demo/showcase/rehearse.sh

set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO_HOME="${DEMO_HOME:-/tmp/agit-demo}"
PORT="${HUB_PORT:-8180}"
BIN="$DEMO_HOME/bin"
ALICE="$DEMO_HOME/showcase-alice"
LIN="$DEMO_HOME/showcase-lin"
G=$'\033[32m'; R=$'\033[31m'; B=$'\033[1m'; N=$'\033[0m'
step(){ echo "${B}▸ $*${N}"; }
ok(){ echo "  ${G}✓ $*${N}"; }
bad(){ echo "  ${R}✗ $*${N}"; FAIL=1; }
FAIL=0

"$HERE/setup.sh" >/tmp/agit-rehearse-setup.log 2>&1
export PATH="$BIN:$PATH"

# ── 第一幕 ──
cd "$ALICE"
step "① init 两个库"
agit init >/dev/null && [[ -d .agit/agent/.git ]] && ok "Agent Store 建好" || bad "init"

step "③ import（确定性：目标 + 证据池，不建 fact）"
agit -a import session.jsonl >/dev/null 2>&1
[[ -s .agit/agent/state/goals.md && -s .agit/agent/state/_evidence_pool.md ]] && ok "抽出 goals + 证据池" || bad "import"

if [[ "${SUMMARIZE:-}" == "1" ]]; then
  step "③b --summarize（可选：调本机 claude 归纳成带出处的 fact）"
  OUT="$(agit -a import --summarize session.jsonl 2>&1)"
  echo "$OUT" | grep -q '归纳出' && ok "claude 归纳出 fact" || bad "summarize"
  # 清掉自动生成的 fact，让后面手写那条不撞车（演示里两者取其一）
  rm -f .agit/agent/state/facts/api/user/id-field-name.md 2>/dev/null || true
fi

step "④ new 手写 fact"
agit -a new api/user/id-field-name -e file:models/user.ts:4 -m '用户标识字段叫 user_id，不是 uid。' >/dev/null \
  && ok "fact 落在 Agent Store" || bad "new"

step "⑤ verify FRESH"
agit -a verify 2>&1 | grep -q FRESH && ok "证据新鲜" || bad "verify fresh"

step "⑥ 密钥被采集拒绝"
# agit 故意非零退出；先收集输出再判断（避免 pipefail 误读）
OUT="$(agit -a new db/pw -e file:.env:1 -m 'x' 2>&1)"
echo "$OUT" | grep -q denylist && ok "denylist 拦截" || bad "secret denylist"

step "⑦ commit context"
agit -a add -A >/dev/null && agit -a commit -m 'alice' >/dev/null 2>&1 && ok "已提交" || bad "commit"

step "⑧ workspace / validate / portable"
agit workspace 2>&1 | grep -q head_commit && ok "配对已生成" || bad "workspace"
agit -a validate >/dev/null 2>&1 && ok "schema 校验通过" || bad "validate"
agit -a portable 2>&1 | grep -q agent_state_ref && ok "portable" || bad "portable"

step "⑨ 发布到 Hub"
agit -a remote add origin "http://localhost:$PORT/payments-api.git" 2>/dev/null
agit -a push -u origin main >/dev/null 2>&1 && ok "push 到 Hub 成功" || bad "push"
curl -s -o /dev/null -w '%{http_code}' "http://localhost:$PORT/agent/payments-api" | grep -q 200 && ok "Hub 前端可读" || bad "hub front"

# ── 第二幕 ──
step "② 证据过期"
sed -i 's/  user_id: string;/  userId: string;/' models/user.ts
git commit -qam '重命名' >/dev/null
OUT="$(agit -a verify 2>&1)"    # verify 检测到 STALE 时非零退出，先收集
echo "$OUT" | grep -q STALE && ok "改代码 → STALE" || bad "staleness"

# ── 第三幕（Alice 本地两分支的干净冲突）──
step "③ 合并 + 证据裁决"
agit -a checkout -b teammate >/dev/null 2>&1
agit -a new refund/status-field -e doc:docs/api-v1.md@2024-03-11 -m '退款状态字段叫 status。' >/dev/null 2>&1
agit -a add -A >/dev/null; agit -a commit -m teammate >/dev/null 2>&1
agit -a checkout main >/dev/null 2>&1
agit -a new refund/status-field -e file:services/refund.ts:8 -m '退款状态字段叫 state。' >/dev/null 2>&1
agit -a add -A >/dev/null; agit -a commit -m alice-refund >/dev/null 2>&1
agit -a merge teammate >/dev/null 2>&1
f=.agit/agent/state/facts/refund/status-field.md
grep -q '建议采纳: ours' "$f" && ok "冲突带证据裁决，建议 ours" || bad "merge adjudication"
agit -a resolve refund/status-field --take ours >/dev/null 2>&1 && ok "resolve" || bad "resolve"

# ── 第四幕 ──
cd "$LIN"
step "⑩ Lin 一条命令消费"
agit clone "http://localhost:$PORT/payments-api.git" >/dev/null 2>&1 && ok "clone 团队 context" || bad "clone"
step "⑪ 对自己基线复验"
agit -a verify 2>&1 | grep -qE 'FRESH|条 fact' && ok "verify 对 Lin 基线" || bad "lin verify"
step "⑫ 装回 Claude Code"
agit -a export --to claude-code >/dev/null 2>&1
grep -q 'agit:begin' CLAUDE.md && grep -q '依据' CLAUDE.md && ok "写入 CLAUDE.md 受管区块" || bad "export claude"
step "⑬ Hub claude.md 端点"
curl -s "http://localhost:$PORT/agent/payments-api/claude.md" | grep -q '已知事实' && ok "Hub 供 claude.md" || bad "hub claude.md"

echo
[[ $FAIL -eq 0 ]] && echo "${G}${B}彩排全过 —— 可以上台。${N}" || echo "${R}${B}有环节翻车，见上。${N}"
kill "$(cat /tmp/agit-showcase-hub.pid 2>/dev/null)" 2>/dev/null
exit $FAIL
