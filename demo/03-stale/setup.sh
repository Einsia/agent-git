#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 03-stale --with-agit

# 预置三条结论，让你直接从「验证」开始
"$AGIT" new api/user/id-field-name    -e file:models/user.ts:4        -m '用户标识字段叫 user_id。'          >/dev/null
"$AGIT" new latency/order-service/n-plus-1 -e file:services/order.ts:7-10 -m 'OrderService.list 有 N+1 查询。' >/dev/null
"$AGIT" new perf/orders/db-calls      -e "cmd:grep -c 'await db' services/order.ts" -m 'OrderService 里有 2 处 await db 调用。' >/dev/null
"$AGIT" add >/dev/null && git commit -qm '三条结论' >/dev/null

handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
