#!/usr/bin/env bash
set -euo pipefail

# Installs the toolchain and components that rust-toolchain.toml names. It runs before any other
# rustup or cargo call, so the only toolchain download of the job happens here, where an error from
# the dist server costs a retry instead of the job. A configured mirror (RUSTUP_DIST_SERVER) is
# tried first and the official server after it. With the toolchain installed it changes nothing.
official=https://static.rust-lang.org
servers=("${RUSTUP_DIST_SERVER:-$official}")
[ "${servers[0]}" != "$official" ] && servers+=("$official")
for server in "${servers[@]}"; do
    for attempt in 1 2 3; do
        if RUSTUP_DIST_SERVER="$server" rustup toolchain install --no-self-update; then
            exit 0
        fi
        [ "$attempt" -lt 3 ] && sleep $((attempt * 15))
    done
    echo "Rust toolchain installation from $server failed" >&2
done
exit 1
