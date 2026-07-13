#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 05-workspace --with-agit
# 让两个库各动一次，生成配对
"$AGIT" -a new api/user/id-field-name -e file:models/user.ts:4 -m '字段叫 user_id。' >/dev/null
"$AGIT" -a add -A >/dev/null; "$AGIT" -a commit -m 'context: 字段名' >/dev/null 2>&1 || (cd "$REPO/.agit/agent" && git commit -q -m 'context: 字段名')
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
