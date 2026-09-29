#!/usr/bin/env bash
set -euo pipefail

: "${CARGO_HOME:?CARGO_HOME must select the job Cargo directory}"
mkdir -p "$CARGO_HOME"
printf '[source.crates-io]\nreplace-with = "rsproxy-sparse"\n\n[source.rsproxy-sparse]\nregistry = "sparse+https://rsproxy.cn/index/"\n\n[net]\ngit-fetch-with-cli = true\n' > "$CARGO_HOME/config.toml"
cargo_target="${AGIT_ARTIFACT_TARGET:-$(rustc -vV | awk '/^host:/ {print $2}')}"
test -n "$cargo_target"

# Both registries must satisfy the committed lockfile before any build proceeds.
if CARGO_HTTP_TIMEOUT=20 CARGO_NET_RETRY=1 cargo fetch --locked --target "$cargo_target"; then
    exit 0
fi
echo "Cargo mirror unavailable; retrying locked dependencies from crates.io"
printf '[net]\ngit-fetch-with-cli = true\n' > "$CARGO_HOME/config.toml"
CARGO_HTTP_TIMEOUT=20 CARGO_NET_RETRY=1 cargo fetch --locked --target "$cargo_target"
