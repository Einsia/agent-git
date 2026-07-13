#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 02-claim --with-agit
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
