#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

rm -rf "$DEMO_HOME/08-lin" "$DEMO_HOME/08-newbie"
bare_origin 08-origin

seed_repo 08-remote --with-agit
git remote add origin "$ORIGIN"
git push -q -u origin main 2>/dev/null

# Alice 在旧金山，一下午的工作成果，还没 push
git checkout -q -b alice main; gitcfg alice
"$AGIT" new latency/order-service/n-plus-1 -e file:services/order.ts:7-10 \
  -m 'OrderService.list 有 N+1 查询，某次改动引进来的。' >/dev/null
"$AGIT" new api/user/id-field-name -e file:models/user.ts:4 \
  -m '用户标识字段叫 user_id。' >/dev/null
"$AGIT" add >/dev/null && git commit -qm '查清线上延迟成因' >/dev/null

cat <<EOF

团队远端： $ORIGIN
你是 Alice，在 alice 分支上，两条结论还没 push。
后面会用到的两个目录（还不存在，你自己 clone）：
  /tmp/agit-demo/08-lin      小林
  /tmp/agit-demo/08-newbie   新人
EOF
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
