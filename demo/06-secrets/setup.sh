#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 06-secrets --with-agit
touch "$REPO/server.pem"
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
