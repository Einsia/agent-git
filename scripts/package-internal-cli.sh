#!/usr/bin/env bash
set -euo pipefail

: "${AGIT_RELEASE_CHANNEL:?set AGIT_RELEASE_CHANNEL to dev or staging}"
: "${AGIT_DEFAULT_HUB_URL:?set AGIT_DEFAULT_HUB_URL}"
: "${AGIT_BUILD_SHA:?set AGIT_BUILD_SHA to the source commit}"
: "${AGIT_ARTIFACT_TARGET:?set AGIT_ARTIFACT_TARGET to the runner target triple}"

case "$AGIT_RELEASE_CHANNEL" in
  dev|staging) ;;
  *) echo "internal packages only support dev or staging" >&2; exit 2 ;;
esac

base_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
test -n "$base_version"
short_sha="${AGIT_BUILD_SHA:0:12}"
export AGIT_BUILD_VERSION="${base_version}-${AGIT_RELEASE_CHANNEL}+${short_sha}"

cargo build --locked --release --bin agit

binary="target/release/agit"
test -x "$binary"
test "$("$binary" --version)" = "agit $AGIT_BUILD_VERSION"

package_dir="dist/package"
archive="dist/agit-${AGIT_RELEASE_CHANNEL}-${short_sha}-${AGIT_ARTIFACT_TARGET}.tar.gz"
rm -rf "$package_dir"
mkdir -p "$package_dir"
cp "$binary" "$package_dir/agit"
printf '%s\n' \
  "channel=$AGIT_RELEASE_CHANNEL" \
  "version=$AGIT_BUILD_VERSION" \
  "commit=$AGIT_BUILD_SHA" \
  "hub=$AGIT_DEFAULT_HUB_URL" \
  "target=$AGIT_ARTIFACT_TARGET" > "$package_dir/BUILD-INFO.txt"

case "$AGIT_ARTIFACT_TARGET" in
  x86_64-unknown-linux-gnu)
    platform_name="Linux x64"
    checksum_command="sha256sum -c SHA256SUMS"
    quarantine_command=""
    ;;
  aarch64-apple-darwin)
    platform_name="macOS ARM64"
    checksum_command="shasum -a 256 -c SHA256SUMS"
    quarantine_command="xattr -d com.apple.quarantine agit 2>/dev/null || true"
    ;;
  *) echo "unsupported internal artifact target: $AGIT_ARTIFACT_TARGET" >&2; exit 2 ;;
esac

{
  printf 'AgentGit %s CLI installation (%s)\n\n' "$AGIT_RELEASE_CHANNEL" "$platform_name"
  printf '  %s\n' "$checksum_command"
  if [ -n "$quarantine_command" ]; then
    printf '  %s\n' "$quarantine_command"
  fi
  printf '  sudo mkdir -p /usr/local/bin\n'
  printf '  sudo install -m 0755 agit /usr/local/bin/agit\n'
  printf '  agit setup\n'
  printf '  agit --version\n'
  if [ -n "$quarantine_command" ]; then
    printf '\nThe quarantine workaround is only for trusted internal dev/staging artifacts.\n'
    printf 'Production macOS releases should use Developer ID signing and Apple notarization.\n'
  fi
} > "$package_dir/INSTALL.txt"

(
  cd "$package_dir"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum agit > SHA256SUMS
  else
    shasum -a 256 agit > SHA256SUMS
  fi
)
tar -C "$package_dir" -czf "$archive" agit BUILD-INFO.txt INSTALL.txt SHA256SUMS
printf 'built %s\n' "$archive"
