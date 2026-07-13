#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 06-fork-reset --with-agit
git checkout -q -b payments main
"$AGIT" new refund/flow/services -e file:services/refund.ts:8-10 -m '退款穿过三个服务。' >/dev/null
"$AGIT" add >/dev/null && git commit -qm '共享分支上的已有结论' >/dev/null
echo
echo "你在 payments 分支上，它已经有一条结论。"
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
