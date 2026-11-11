#!/usr/bin/env bash
# The Windows target's Clippy, run on Linux: rust:clippy:windows in merge request pipelines and
# `preturn-check --only clippy-windows` before one, so code that only Windows compiles is linted
# without taking the Windows runner. Same target, features and flags as rust:test:windows's build,
# so the lint covers the code that job compiles. Build scripts compile C and assembly for the
# target, which needs an MSVC toolchain Linux doesn't have; clippy links nothing and reads none of
# that code, so a stand-in writes the files those tools would have written.
set -euo pipefail

target=x86_64-pc-windows-msvc
# rust:test:windows's flags for this target; target features are cfgs, so they can change what is linted.
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS=-Ctarget-feature=+crt-static
rustup target add "$target"
rustup component add clippy
# The stand-in is on PATH only inside this script.
tools="$PWD/target/windows-lint-tools"
mkdir -p "$tools"
for name in cl.exe lib.exe ml64.exe; do
  ln -sf "$(cd "$(dirname "$0")" && pwd)/windows-lint-tool.sh" "$tools/$name"
done
export PATH="$tools:$PATH"
export CC_x86_64_pc_windows_msvc="$tools/cl.exe" CXX_x86_64_pc_windows_msvc="$tools/cl.exe"
export AR_x86_64_pc_windows_msvc="$tools/lib.exe"
command -v python3 >/dev/null || { apt-get update -qq && apt-get install -y --no-install-recommends python3; }
python3 scripts/prepare-git-runtime.py "$target" .cache/git-runtime/windows-payload.tar.gz --cache .cache/test-lfs
export AGIT_GIT_RUNTIME_ARCHIVE="$PWD/.cache/git-runtime/windows-payload.tar.gz"
cargo clippy --locked --features bundled-git --target "$target" --lib --bins -- -D warnings
