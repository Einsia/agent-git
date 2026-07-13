#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 01-init            # 一个普通 git 仓库，还没有 agit
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
