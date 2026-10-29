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
4. `~/Library/Application Support/AgentGitMenuBar/agit-dev`;
5. `command -v agit` in a login shell.

The folder list needs agit 0.2.30 or later. When the agit the app runs is older, the app reads the
list through the first other agit above that has it, and runs every action through its own agit
still; with none, it says which release brings the list.

It inherits the environment it was started with, so `AGIT_HOME` and `AGIT_HUB_URL` work as they
do for the CLI.

For each agit program and version it runs, the app runs `agit setup --hooks --installed-only`
until it succeeds once. That repairs agit hooks your agent runtimes already have but cannot start,
such as one whose path holds a space, and installs none. Only the broken command text changes; the
file as it was is kept beside it, readable only by you, with `.agit-backup` added to its name, and
the app tells you once what it repaired.

Where every agit hook in your Claude Code settings runs the app's agit and that agit has the
permission hook, the app asks once for each Claude Code settings file whether to add it. The hook
lets approvals you answer in AgentGit allow or deny Claude Code's permission requests on this
computer, which Claude Code's security default otherwise keeps from AgentGit; Claude Code's own
dialog still answers as before. Cancel is the default. Add runs
`agit setup --hooks --runtime claude-code`, which rewrites the settings file and keeps the file as
it was before AgentGit first changed it beside it, with `.agit-backup` added to the name.

## What it shows

| Icon | Meaning |
| --- | --- |
| `checkmark.icloud` | Every folder that uploads automatically is up to date. |
| `arrow.clockwise.icloud` | A conversation is waiting for, or in, an upload, or the app is running an agit command. |
| `exclamationmark.icloud` | Something needs you: sign-in expired, uploads from a folder fail, or agit is missing or too old. |
| `icloud.slash` | No folder uploads automatically (nothing set up, or paused). |

The menu lists each folder from `agit project list --json` with its state ("Uploads
automatically", "Upload blocked", "Paused", "Saved on this Mac only"), its destination repository,
and its newest conversations with the title Claude Code or Codex gave them, when they last
changed, and whether they are uploaded. A failed upload shows agit's reason and a retry button. A
folder set to upload whose repository has automatic publication turned off on this Mac
(`agit config --repo OWNER/NAME push.auto false`) reads "Upload blocked" and needs attention: it
says why and how many of its conversations are saved but not uploaded, and offers to turn
automatic upload on. Turning it on, from there or from the folder's switch, asks first: the
question says the repository's automatic upload is off, how many conversations it releases, and
who can read them, and Cancel is its default answer.

Actions and the commands behind them:

| Action | Command |
| --- | --- |
| Turn uploads on for a listed folder (asks first) | `agit --yes project bind <folder> --repo <owner/name> --history none --auto-upload`, preceded by `agit config --repo <owner/name> push.auto true` when the repository's automatic upload is off on this Mac, which the question names |
| Turn automatic upload on for a blocked folder's repository (asks first) | `agit config --repo <owner/name> push.auto true` |
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
