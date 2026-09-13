#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo 'usage: install-test-lfs.sh <isolated-bin-directory> [archive-cache-directory]' >&2
  exit 2
fi

version=3.8.0
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64)
    platform=linux-amd64
    archive=tar.gz
    checksum=e455e00f15d9b95661b8d53498ffb0c3367962cf1ec73c31ab7369516cd6ab8d
    ;;
  Linux/aarch64|Linux/arm64)
    platform=linux-arm64
    archive=tar.gz
    checksum=ac9c8efac980bb0505ead384d087e2acb6486fd8498691a2165fa174ec6118c2
    ;;
  Darwin/arm64)
    platform=darwin-arm64
    archive=zip
    checksum=caff76a7d070d8160c89bc39b6e85d98f24135b6fed038a3b4de2590d25102d8
    ;;
  Darwin/x86_64)
    platform=darwin-amd64
    archive=zip
    checksum=f1c17aeca0b4eaab9ea606226477dbed3b84b56fe0811a9f967d2ea2b2393c53
    ;;
  *)
    echo 'the LFS fixture installer does not support this platform' >&2
    exit 1
    ;;
esac

destination="$1"
cache_directory="${2:-}"
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
asset="git-lfs-$platform-v$version.$archive"
verify_archive() {
  local actual
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$1") || return 1
  else
    actual=$(shasum -a 256 "$1") || return 1
  fi
  [[ "${actual%% *}" == "$checksum" ]]
}
if [[ -n "$cache_directory" && -f "$cache_directory/$checksum-$asset" ]]; then
  cp "$cache_directory/$checksum-$asset" "$temporary/$asset"
fi
if [[ ! -f "$temporary/$asset" ]] || ! verify_archive "$temporary/$asset"; then
  curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
    --retry 3 --retry-all-errors --connect-timeout 20 --max-time 180 \
    "https://github.com/git-lfs/git-lfs/releases/download/v$version/$asset" -o "$temporary/$asset"
fi
if ! verify_archive "$temporary/$asset"; then
  echo 'Git LFS release checksum does not match its pinned value' >&2
  exit 1
fi
if [[ -n "$cache_directory" ]]; then
  mkdir -p "$cache_directory"
  install -m 644 "$temporary/$asset" "$cache_directory/$checksum-$asset"
fi
if [[ "$archive" == zip ]]; then
  unzip -q "$temporary/$asset" -d "$temporary/extracted"
else
  mkdir "$temporary/extracted"
  tar -xzf "$temporary/$asset" -C "$temporary/extracted"
fi
mkdir -p "$destination"
install -m 755 "$temporary/extracted/git-lfs-$version/git-lfs" "$destination/git-lfs"
"$destination/git-lfs" version
