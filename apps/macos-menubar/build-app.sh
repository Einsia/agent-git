#!/usr/bin/env bash
# Builds AgentGit.app from this package and signs it ad hoc, for running on this Mac.
#
#   ./build-app.sh            # release build into ./build/AgentGit.app
#   ./build-app.sh --debug    # debug build
#
# Needs Swift 5.9 or later (Xcode or the Command Line Tools); no Xcode project is involved.
set -euo pipefail

cd "$(dirname "$0")"
configuration=release
[ "${1:-}" = --debug ] && configuration=debug

swift build -c "$configuration"
binary="$(swift build -c "$configuration" --show-bin-path)/AgentGitMenuBar"

app=build/AgentGit.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp Resources/Info.plist "$app/Contents/Info.plist"
cp "$binary" "$app/Contents/MacOS/AgentGitMenuBar"

# An ad hoc signature is enough to run on the machine that built it; distribution needs a
# Developer ID signature and notarization (see README.md).
codesign --force --sign - --timestamp=none "$app"
echo "$PWD/$app"
