#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 05-diff --with-agit
"$AGIT" new api/user/id-field-name -e file:models/user.ts:4 -m '用户标识字段叫 user_id。' >/dev/null
"$AGIT" new refund/flow/services -e file:services/refund.ts:8-10 -m '退款穿过三个服务：PaymentGateway → LedgerService → NotifyService。' >/dev/null
"$AGIT" add >/dev/null && git commit -qm '两条结论' >/dev/null
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
