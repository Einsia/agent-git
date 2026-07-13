#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 01-two-stores            # 一个代码仓库（假支付服务），还没 agit init
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
