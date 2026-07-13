#!/usr/bin/env bash
# 大合集 demo 的舞台搭建。建两个开发者的代码仓库 + 启动 Hub + 注册 agent。
# 不替你跑 agit init/import/... —— 那些是你上台要演的。
#
#   ./demo/showcase/setup.sh            搭台
#   SUMMARIZE=1 ./demo/showcase/setup.sh   （无影响，标志给 rehearse 用）

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

PORT="${HUB_PORT:-8180}"
HUB_ROOT="$DEMO_HOME/hub-data"
AGENT="payments-api"
ALICE="$DEMO_HOME/showcase-alice"
LIN="$DEMO_HOME/showcase-lin"
HUB="$BIN_DIR/agit-hub"

_ensure_agit
# 指向真实二进制，别指向 bin/ 里的软链（否则自指成环）
HUB_BIN="$ROOT/target/release/agit-hub"
[[ -x "$HUB_BIN" ]] || HUB_BIN="$ROOT/target/debug/agit-hub"
ln -sf "$HUB_BIN" "$HUB"

B=$'\033[1m'; G=$'\033[32m'; DIM=$'\033[2m'; N=$'\033[0m'

# ── 建一个假支付服务代码仓库 ──
seed_code() {
  local dir="$1" who="$2"
  rm -rf "$dir"; mkdir -p "$dir"; cd "$dir"
  git init -q -b main .
  git config user.name "$who"; git config user.email "$who@payments.io"; git config commit.gpgsign false
  cp -r "$SEED/." "$dir/"
  mv env.seed .env
  printf '.env\nnode_modules/\n' > .gitignore
  git add -A && git commit -qm "支付服务：初始代码"
}

# ── Hub ──
pkill -f "agit-hub serve.*$PORT" 2>/dev/null || true
sleep 0.5
rm -rf "$HUB_ROOT"
"$HUB" add "$AGENT" --root "$HUB_ROOT" >/dev/null
nohup "$HUB" serve --root "$HUB_ROOT" --port "$PORT" >/tmp/agit-showcase-hub.log 2>&1 &
echo $! > /tmp/agit-showcase-hub.pid
sleep 1

# ── Alice 的仓库 + 一份真实结构的 Claude Code 会话 ──
seed_code "$ALICE" alice
cat > "$ALICE/session.jsonl" <<EOF
{"type":"mode","sessionId":"demo"}
{"type":"user","cwd":"$ALICE","gitBranch":"main","uuid":"u1","message":{"role":"user","content":"用户标识字段叫什么？订单列表为什么慢？"}}
{"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"我读一下 models/user.ts 和 services/order.ts。"},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"$ALICE/models/user.ts","offset":4,"limit":1}}]}}
{"type":"assistant","uuid":"a2","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"$ALICE/services/order.ts","offset":5,"limit":4}},{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"grep -n user_id models/user.ts"}}]}}
{"type":"assistant","uuid":"a3","message":{"role":"assistant","content":[{"type":"text","text":"字段叫 user_id；OrderService.list 里对每个订单又查一次 items，是个 N+1。"},{"type":"tool_use","id":"t4","name":"Write","input":{"file_path":"$ALICE/notes.md","content":"x"}}]}}
EOF

# ── Lin 的仓库（同样的代码，他会从 Hub 拉 Alice 的 context）──
seed_code "$LIN" lin

# ── 提示 ──
cat <<EOF

${B}舞台就绪。${N}

  ${G}export PATH="$BIN_DIR:\$PATH"${N}

  Hub 已启动： ${G}http://localhost:$PORT/${N}   （日志 /tmp/agit-showcase-hub.log）
  Alice 仓库： ${G}$ALICE${N}
  Lin 仓库：   ${G}$LIN${N}

${DIM}照着演示脚本一幕一幕敲：${N}
  ${G}\$EDITOR demo/showcase/README.md${N}

${DIM}先进 Alice 的仓库开演：${N}
  ${G}cd $ALICE${N}
EOF
