#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 02-import --with-agit

# 造一份最小但真实结构的 Claude Code session，读了 seed 里真实存在的文件
cat > "$REPO/session.jsonl" <<EOF
{"type":"mode","sessionId":"demo"}
{"type":"user","cwd":"$REPO","gitBranch":"main","uuid":"u1","message":{"role":"user","content":"user 的标识字段叫什么？退款走哪几个服务？"}}
{"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"我读一下 models/user.ts 和 services/refund.ts。"},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"$REPO/models/user.ts","offset":4,"limit":1}}]}}
{"type":"assistant","uuid":"a2","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"$REPO/services/refund.ts","offset":8,"limit":3}},{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"grep -n user_id models/user.ts"}}]}}
{"type":"assistant","uuid":"a3","message":{"role":"assistant","content":[{"type":"text","text":"字段叫 user_id；退款穿过 PaymentGateway、LedgerService、NotifyService 三个服务。"},{"type":"tool_use","id":"t4","name":"Write","input":{"file_path":"$REPO/notes.md","content":"x"}}]}}
EOF

echo
echo "已在仓库根放了一份 session.jsonl（模拟 Claude Code 的一次会话）。"
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
