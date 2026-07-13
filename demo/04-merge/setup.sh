#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 04-merge --with-agit

# ── Alice：读代码 ────────────────────────────────────────────
git checkout -q -b alice main; gitcfg alice
"$AGIT" new api/user/id-field-name -e file:models/user.ts:4 -m '用户标识字段叫 user_id。' >/dev/null
"$AGIT" new latency/order-service/n-plus-1 -e file:services/order.ts:7-10 -m 'OrderService.list 有 N+1 查询。' >/dev/null
"$AGIT" add >/dev/null && git commit -qm 'alice：查清字段名与延迟成因' >/dev/null

# ── Bob：读 2024 年的老文档，得出相反结论 ──────────────────────
git checkout -q -b bob main; gitcfg bob
"$AGIT" new api/user/id-field-name -e doc:docs/api-v1.md@2024-03-11 -m '用户标识字段叫 uid。' >/dev/null
"$AGIT" new refund/flow/services -e file:services/refund.ts:8-10 -m '退款穿过三个服务。' >/dev/null
"$AGIT" add >/dev/null && git commit -qm 'bob：查清字段名与退款流程' >/dev/null

# ── carol / dave：证据强度完全相同的一对，用来看「拒绝猜」 ────────
git checkout -q -b carol main; gitcfg carol
"$AGIT" new api/user/role-field -e doc:docs/api-v1.md@2026-07-01 -m '角色字段叫 role。' >/dev/null
"$AGIT" add >/dev/null && git commit -qm carol >/dev/null
git checkout -q -b dave main; gitcfg dave
"$AGIT" new api/user/role-field -e doc:docs/api-v1.md@2026-07-02 -m '角色字段叫 roles。' >/dev/null
"$AGIT" add >/dev/null && git commit -qm dave >/dev/null

git checkout -q alice; gitcfg alice

cat <<'EOF'

setup 建了四条分支：

  alice  api/user/id-field-name = user_id   证据 file:models/user.ts:4       （活代码）
         latency/order-service/n-plus-1                                      （只有 alice 有）
  bob    api/user/id-field-name = uid       证据 doc:docs/api-v1.md@2024-03-11（两年前的文档）
         refund/flow/services                                                （只有 bob 有）
  carol  api/user/role-field = role         证据 doc:...@2026-07-01
  dave   api/user/role-field = roles        证据 doc:...@2026-07-02          （强度和 carol 相同）

你现在在 alice 上。
EOF
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
