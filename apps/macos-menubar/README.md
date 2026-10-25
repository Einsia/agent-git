# AgentGit menu bar app (macOS)

A small macOS menu bar app that shows what agit is doing on this Mac: which folders upload
their conversations, the conversations being saved there, recent uploads and failures, and
the signed-in Hub account. It is a window onto the `agit` CLI, not a second implementation:
every number it shows comes from the CLI's JSON output, and every action it offers runs an
`agit` command.

## Build and run

Requirements: macOS 13 or later, Swift 5.9 or later (Xcode, or only the Command Line Tools),
and an `agit` that has `agit project list` (check with `agit project list --json`); with an
older agit the app says so and offers the update command.

```sh
cd apps/macos-menubar
./build-app.sh                 # writes build/AgentGit.app, signed ad hoc for this Mac
open build/AgentGit.app
```

The app has no Dock icon; look for the cloud icon in the menu bar. To start it at login, add
`AgentGit.app` under System Settings → General → Login Items.

For development, `swift run` starts the same app without a bundle (no notifications then).
macOS draws menu bar items in a separate process, so the app cannot open its own menu, and a
crowded menu bar can hide the icon; `open --env AGIT_MENUBAR_OPEN=menu build/AgentGit.app`
(or `guide`, `enroll`) also shows that view in an ordinary window, for screenshots and UI checks.

### Which agit it runs

The app looks for agit, in order, at:

1. the program chosen in the app (gear menu → "Choose the agit program…"), stored as
   `defaults write com.agentgit.menubar agitPath /path/to/agit`;
2. `$AGIT_MENUBAR_CLI`;
3. `~/.local/bin/agit`, `/usr/local/bin/agit`, `/opt/homebrew/bin/agit`,
   `~/.npm-global/bin/agit`;
4. `command -v agit` in a login shell.

It inherits the environment it was started with, so `AGIT_HOME` and `AGIT_HUB_URL` work as they
do for the CLI.

## What it shows

| Icon | Meaning |
| --- | --- |
| `checkmark.icloud` | Every folder that uploads automatically is up to date. |
| `arrow.clockwise.icloud` | A conversation is waiting for, or in, an upload, or the app is running an agit command. |
| `exclamationmark.icloud` | Something needs you: sign-in expired, uploads from a folder fail, or agit is missing or too old. |
| `icloud.slash` | No folder uploads automatically (nothing set up, or paused). |

The menu lists each folder from `agit project list --json` with its state ("Uploads
automatically", "Paused", "Saved on this Mac only"), its destination repository, and its newest
conversations with the title Claude Code or Codex gave them, when they last changed, and whether
they are uploaded. A failed upload shows agit's reason and a retry button. A folder whose
repository has automatic publication turned off on this Mac (`agit config --repo OWNER/NAME
push.auto false`) shows "Upload turned off" instead of a failure, explains it on hover, and its
menu copies the command that turns it back on.

Actions and the commands behind them:

| Action | Command |
| --- | --- |
| Turn uploads on for a listed folder (asks first) | `agit --yes project bind <folder> --repo <owner/name> --history none --auto-upload` |
| Turn uploads off | `agit project unbind <folder>` |
| Add a folder (asks first) | `agit --yes repo create <name> --private` when asked to create it, then the bind above |
| Retry uploads / upload again now | `agit --yes project sync <folder>` |
| Pause / resume syncing | unbind every folder that uploads / bind the same folders again (asks first) |
| Sign in | `agit login` (opens the browser), then the `complete_command` it returns |
| Open on the Hub, copy a link | the `repository_url` and `url` fields of `agit project list` |

Turning uploads on is publication, so the app always asks with the destination, Hub and account
spelled out before it passes `--yes`; nothing is enrolled or uploaded silently. `project bind
--auto-upload` also turns on the repository's own automatic publication on this Mac, which every
conversation saved in that repository inherits, and the confirmation says so. Creating a missing
repository always makes it private.

Notifications are posted only for things the user must act on (sign-in expired, an upload
refused with its reason), at most once per problem every six hours.

## Data and credentials

The app reads state only through `agit whoami --json` (token states and expiry times, never
token values) and `agit project list --json`. It stores no credentials. It keeps two small
values in its own defaults: the uploads it saw finish (agit itself records only each folder's
last result) and which folders it paused.

## Distribution (not done yet)

The local build is signed ad hoc and runs only on the Mac that built it (Gatekeeper rejects it
elsewhere). Shipping it with the npm installer would need:

1. **Signing**: a Developer ID Application certificate in CI, `codesign --options runtime
   --timestamp` on the app (hardened runtime; no extra entitlements are needed, since the app only
   launches `agit` and opens URLs).
2. **Notarization**: `xcrun notarytool submit AgentGit.zip --wait` with an App Store Connect API
   key kept as a CI secret, then `xcrun stapler staple AgentGit.app` so it opens offline.
3. **Building**: a macOS runner (the CLI pipeline already has one for `dev:macos-arm64`) building a
   universal binary (`swift build -c release --arch arm64 --arch x86_64`) into the same bundle.
4. **Packaging**: a notarized, stapled `AgentGit.app.zip` attached to the release, and an opt-in
   step in `create-agit` / the npm postinstall (macOS only) that downloads it, checks its
   SHA-256 against the release manifest, unpacks it into `~/Applications`, and offers to open it.
   It must stay opt-in and never register a login item without asking.
5. **Updates**: either reuse `agit upgrade` to replace the app next to the CLI, or Sparkle with
   an EdDSA-signed appcast; the app should refuse to run against an agit too old for
   `project list`, as it does now.
