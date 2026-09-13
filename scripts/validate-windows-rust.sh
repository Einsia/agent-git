#!/usr/bin/env bash
set -euo pipefail

version=0.23.1
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    host=x86_64-unknown-linux-musl
    sha256=c492c6dfb7e5ac0eee586b796e0fc950ba13077e7f0bbb046f445d71790d5360
    ;;
  Linux-aarch64)
    host=aarch64-unknown-linux-musl
    sha256=dc0a0fb28fdd90405f69e7365bce91d2504ea7491eb4d79d5655ffa106358925
    ;;
  *) echo "Windows cross-validation requires a Linux x86_64 or aarch64 host" >&2; exit 1 ;;
esac

cache_dir="${CI_PROJECT_DIR:-$PWD}/.cache/windows-clippy"
mkdir -p "$cache_dir/downloads" "$cache_dir/bin"
archive="$cache_dir/downloads/cargo-xwin-v$version.$host.tar.gz"
if ! echo "$sha256  $archive" | sha256sum --check --status 2>/dev/null; then
  curl --fail --location --retry 3 \
    "https://github.com/rust-cross/cargo-xwin/releases/download/v$version/cargo-xwin-v$version.$host.tar.gz" \
    --output "$archive"
fi
echo "$sha256  $archive" | sha256sum --check
tar -xzf "$archive" -C "$cache_dir/bin" cargo-xwin
export PATH="$cache_dir/bin:$PATH"

# The CI cache key includes this script so SDK pins cannot reuse another version.
export XWIN_CACHE_DIR="$cache_dir/sdk"
export XWIN_ARCH=x86_64
export XWIN_VERSION=17
export XWIN_SDK_VERSION=10.0.26100
export XWIN_CRT_VERSION=14.44.17.14

rustup component add clippy llvm-tools
rustup target add x86_64-pc-windows-msvc
cargo xwin clippy --locked --target x86_64-pc-windows-msvc --all-targets -- -D warnings
cargo xwin clippy --locked --target x86_64-pc-windows-msvc --no-default-features --lib -- -D warnings
