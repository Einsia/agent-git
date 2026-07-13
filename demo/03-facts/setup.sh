#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 03-facts --with-agit
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
