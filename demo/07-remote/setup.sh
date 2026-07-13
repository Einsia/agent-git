#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

# 团队共享的 Agent Store 远端（裸仓库）
bare_origin 07-agent-origin        # → $ORIGIN

seed_repo 07-remote --with-agit    # alice 的工作仓库
A="$REPO/.agit/agent"

# alice 建一条 context，Agent Store 指向团队远端
"$AGIT" -a new latency/order-service/n-plus-1 -e file:services/order.ts:7-10 \
  -m 'OrderService.list 有 N+1 查询，某次改动引进来的。' >/dev/null
"$AGIT" -a add -A >/dev/null; git -C "$A" commit -q -m 'alice：查清延迟成因'
git -C "$A" remote add origin "$ORIGIN"

echo
echo "团队 Agent Store 远端： $ORIGIN"
echo "你是 alice，Agent Store 里有一条 context，还没推。"
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
