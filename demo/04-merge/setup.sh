#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 04-merge --with-agit
A="$REPO/.agit/agent"

# alice 分支：从代码读出结论（活证据）
git -C "$A" checkout -q -b alice
"$AGIT" -a new api/user/id-field-name -e file:models/user.ts:4 -m '用户标识字段叫 user_id。' >/dev/null
"$AGIT" -a add -A >/dev/null; git -C "$A" commit -q -m 'alice：查字段名'

# bob 分支：从 2024 老文档读出相反结论；外加一条不相干的退款结论
git -C "$A" checkout -q main
git -C "$A" checkout -q -b bob
mkdir -p "$A/state/facts/api/user" "$A/state/facts/refund/flow"
printf -- '---\nsubject: api/user/id-field-name\ntier: reversible\nauthor: bob\ncreated: 2026-07-13T00:00:00Z\nevidence:\n- '"'"'doc:docs/api-v1.md@2024-03-11'"'"'\n---\n\n用户标识字段叫 uid。\n' > "$A/state/facts/api/user/id-field-name.md"
"$AGIT" -a new refund/flow/services -e file:services/refund.ts:8-10 -m '退款穿过三个服务。' >/dev/null 2>&1 || \
printf -- '---\nsubject: refund/flow/services\ntier: reversible\nauthor: bob\ncreated: 2026-07-13T00:00:00Z\nevidence:\n- '"'"'file:services/refund.ts:8'"'"'\n---\n\n退款穿过三个服务。\n' > "$A/state/facts/refund/flow/services.md"
"$AGIT" -a add -A >/dev/null; git -C "$A" commit -q -m 'bob：查字段名与退款'

git -C "$A" checkout -q alice

cat <<EOF

Agent Store 里建了两条分支：
  alice  api/user/id-field-name = user_id  证据 file:models/user.ts:4 （活代码）
  bob    api/user/id-field-name = uid      证据 doc:docs/api-v1.md@2024-03-11（2024 老文档）
         refund/flow/services                （只有 bob 有，和 alice 不相干）
你现在在 alice。
EOF
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
