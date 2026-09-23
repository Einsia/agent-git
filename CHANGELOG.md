# Changelog

Every notable change to agit, the AgentGit CLI, by release. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). A version's section here is the body of
its [GitHub Release](https://github.com/Einsia/agent-git/releases), and the
`@einsia/agent-git` npm package ships this file.

## [0.2.6] - 2026-09-23

### Changed

- **Download counting on the Hub.** Git and Git LFS requests that agit sends to
  the configured Hub carry `X-AgentGit-Command` (the top-level command, such as
  `clone` or `pull`) and `X-AgentGit-Operation` (a random ID generated once per
  process), so the Hub counts one download per invocation and repository
  instead of one per negotiation round or credential retry. The headers contain
  no arguments, names, paths or machine identifiers, and other Git remotes never
  receive them. See [Hub download attribution](docs/telemetry.md#hub-download-attribution).
- Session page links printed by `agit show` and `agit file link` add
  `sharer=<username>` when you are signed in to that Hub, so visits through a
  link you paste are credited to you. Signed out, links are unchanged.

## [0.2.5] - 2026-09-23

### Added

- **Invite links from the CLI.** `agit repo invite <owner/repo>` prints a link
  that adds whoever opens it as a collaborator (`--role read|write|owner`,
  default `read`). `agit repo invite <owner/repo>@<branch>` (or `-b <branch>`)
  also lands the invitee on that branch's session page after they accept. Only
  repository owners can create links; they do not expire and can be revoked in
  the repository's settings (Invite by link). A session link requires the
  session to be pushed already. `--json` reports the link, role, repository,
  invitation ID and, for a session, its page URL.

### Changed

- Remote Control history projection reuses compiled persona patterns, so long
  shared histories page in faster.

### Fixed

- `agit rc stop` now waits for the daemon to exit before reporting success, so
  an immediate `agit rc start` no longer races the previous daemon.
- Remote Control discovers native sessions when the same project folder is
  bound through equivalent directory paths.
- The `npx create-agit` installer's closing hint no longer suggests an
  outdated `agit import` invocation; it points to the quickstart instead.

## [0.2.4] - 2026-09-21

### Fixed

- Return an existing native Codex inbox receipt before probing the executable,
  so reconnect retries remain observable when Codex is temporarily unavailable.
  Retries with the same message ID do not enqueue another message.
- Coalesce repeated native history protection failures and keep internal error
  details in daemon logs instead of repeating them in conversation history.
- Preserve native conversation titles in session pickers and omit runtime
  bookkeeping from message previews and counts.
- Settle multiple newly detected heuristic secrets in one pass, and bound identity
  evidence to the provenance budget when scanning large inputs.

## [0.2.3] - 2026-09-20

### Added

- **Send to externally owned Codex conversations.** Authorized operators can
  enqueue text through the installed Codex CLI while its original process keeps
  the writer lock. Capability discovery is nonmutating, and durable receipts
  prevent duplicate submission after reconnect. Queue acceptance does not mean
  execution; Codex controls consumption. Live controls still require ownership.
- Include Cloud connection diagnostics and executor history phase timings to
  help locate connection and history-loading delays.

### Changed

- Batch native history protection and watch projection, and coalesce concurrent
  history captures while retaining session identity and turn order.
- Remove the standalone `rc cloud enroll` command. `agit login` followed by
  `agit rc start --detach` performs registration automatically.

### Fixed

- Resume Codex sessions promptly after the native writer releases ownership.
- Preserve empty Codex sessions before publishing them to a workspace.
- Keep unadopted native previews available with secret protection.
- Compare hydrated native content when resolving resume identity, and ignore
  bookkeeping after a settled turn when checking for unsettled work.

## [0.2.2] - 2026-09-19

### Added

- **Shared workspace controllers.** Accept project- and session-scoped authority
  from the Hub controller, so collaborators can operate a shared session without
  separate device grants. This requires a Hub with shared controller support;
  personal peer/Cloud connections retain their existing protocol.
- Expose native model and reasoning-effort controls through peer remote control.
- Protect detected secrets automatically with reversible, repository-local
  placeholders before saving or publishing session content.

### Changed

- Keep native controls responsive while prepared settlement completes, reuse
  healthy session worktrees and Hub connections, and replace repeated Git child
  processes with bounded local object reads.
- Start tunnel transport independently of repository storage and overlap Cloud
  admission and peer setup to reduce connection overhead.
- Official Hub usage requires usage statistics; installation attempts and stages
  are included in the documented collection policy. Other Hubs retain their
  opt-out controls. See [usage statistics](docs/telemetry.md).

### Fixed

- Keep shared native subscriptions attached to controller authority, isolate each
  viewer's replay, and suppress duplicate events on Cloud utility routes.
- Preserve early session events, uncertain command receipt identities, native
  titles, and saved session identities outside the active catalog.
- Correct Windows path component handling and directory navigation.
- Distinguish stale daemon records from reused process IDs, recover stopped
  daemons after upgrades, and enforce private state permissions under permissive
  umasks.
- Preserve fresh native history during model reads and settlement without
  blocking read-only controls on repository metadata work.

## [0.2.1] - 2026-09-17

### Added

- **Native Windows peer remote control.** Windows supports the same controller,
  executor and Cloud tunnel protocol as Linux and macOS, using a current-user
  named pipe for local owner RPC.
- **Bundled Git and Git LFS.** Official distribution binaries include their Git
  runtime, so session version control does not require a separate Git installation.

### Changed

- **Owner-only Cloud access by default.** Run `agit login`, then
  `agit rc start --detach`. Startup registers the device and enables remote control
  for the signed-in owner, without a separate enrollment command. Use the same Hub
  account in Web Workspaces or the desktop app. This does not grant public access.
- **Remove the paired RC transport.** Startup and device management use peer/Cloud
  only. Users of the paired default in 0.2.0 must upgrade; the existing Linux 0.2.0
  peer/Cloud protocol remains supported. Server retirement follows verification of
  the new Windows and Linux release artifacts.
- Reuse Cloud HTTP connections, overlap admission checks with tunnel setup, and
  race resolved TCP addresses to avoid waiting on a slow address before trying another.
- Reduce repeated repository checks and keep repository preparation and background
  metadata work off the native session command loop. Record startup phase timings
  without logging prompt contents.

### Fixed

- Prevent detached Windows daemon helpers from opening console windows; release
  inherited caller pipes when starting in the background.
- Preserve tunnel failure details and renew Cloud authority without repeatedly
  reconnecting healthy idle sessions.
- Recover a revoked device registration when its owner explicitly starts RC again;
  background reconnects do not undo revocation.
- Read history and goals from fresh Codex sessions before a transcript file exists,
  and isolate history readers from snapshot capture and session control.
- Preserve native history identity and snapshot consistency, and protect generated
  session observations with repository secret rules before settlement.

## [0.2.0] - 2026-09-16

### Added

- **Daemon peers and Cloud tunnels.** `agitd` manages local harness sessions while
  independent SSH or Cloud tunnel workers transport peer messages. Controllers can
  discover and operate sessions on another executor through the same peer protocol.
  Peer hosting is available on Linux and macOS. Cloud connections require a Hub
  with peer relay support enabled.
- **Independent control and inbound access.** A local controller can connect to
  another device while its own inbound access stays disabled. Cloud admission and
  executor session permissions are enforced separately. Adapters can constrain
  requests to a lower role, enforced by the executor when a queued write runs.
- **More native conversation sources.** Discover and import OpenClaw, Hermes and
  WorkBuddy sessions, with runtime-specific setup and transcript handling.
- **Large session files.** Stage binary deliverables with `agit file add --lfs`;
  verify payloads during upload and download before publishing or materializing them.
- **Local and scoped search.** Search saved local history without Hub requests, or
  limit remote session searches to an organization or the current code origin.
- **Optional automatic publishing.** Configure `push.auto` per user or repository
  to publish settled turns, while keeping explicit publication available.
- **Usage statistics controls.** Inspect collection with `agit telemetry`, disable
  it with `agit telemetry disable` or `DO_NOT_TRACK=1`, and preview the field policy.
  Setup discloses the default-on choice; a recorded opt-out is preserved. See the
  [collection and privacy details](docs/telemetry.md).

### Changed

- **Use `agit run` for saved sources.** `agit open` is removed. Update scripts to
  use `agit run owner/repo@ref`; `agit resume` still continues a selected session.
- Review a frozen publication in an interactive agent before pushing, including
  explicit handling of credential findings.
- Status includes bounded native session details, shared files, merge progress and
  project metadata without unbounded transcript scans.
- Interactive startup can offer to install an available update after confirmation.
  Non-interactive update notices go to stderr and preserve JSON output.
- Repository secret dictionaries keep their encryption keys in local Git metadata,
  avoiding repeated Keychain prompts after migration. Neither the dictionary nor
  its key is uploaded by push.

### Fixed

- Recover from expired CLI credentials and retry browser sign-in choices without
  losing the selected Hub or repository.
- Keep session discovery and native watch preparation responsive, hide internal
  runtime sessions, and preserve transcript identity across polling and replay.
- Correlate canonical Codex user history and prevent duplicate Claude prompts.
- Preserve native Codex session titles and recover remote session history reliably.
- Keep controller requests, tunnel failures and harness failures within their
  ownership boundaries, with bounded queues and diagnostic logs for recovery.
- Support Hub SOCKS proxies and preserve Windows browser launch, process ownership
  and named-pipe behavior.
- Scope installed AgentGit skills to explicit session operations and retire legacy
  home-directory instructions without replacing unrelated user content.

## [0.1.2] - 2026-09-12

### Added

- **Session files with explicit staging.** Use `agit file` to add, inspect, commit,
  retrieve and link deliverables in a selected branch without changing its conversation
  VIEW. File staging remains separate from automatic turn settlement.
- **Windows x64 distribution.** Install the native Windows CLI through npm or
  download the executable from the GitHub Release, including remote-control support.
- **More remote-control workflows.** Connect existing Codex conversations through
  the native inbox, use OpenCode remote control and transcript snapshots, and navigate
  between connected machines and their workspaces.
- **Richer history inspection.** Inspect saved VIEWs and LOGs, raw native JSONL and
  archived evidence; compare semantic prefixes and unsettled native turns with `diff`.
- **Scoped search and integrity checks.** Search authenticated repository scopes,
  inspect incomplete-result diagnostics, and run bounded, read-only `doctor` checks.
- **Safer import and review.** Choose native-session lineage explicitly, preview and
  name sessions interactively, and review committed VIEWs with guarded scan remedies.

### Changed

- **Explicit session targeting.** Use `owner/repo@branch` or `AGIT_SESSION` for
  automation. Interactive commands offer target selection; `agit switch` and implicit
  workspace targeting are removed. Update scripts that depended on those defaults.
- **Structured agent output.** JSON output includes typed recovery actions, while
  human output identifies verified targets and quiet mode suppresses progress output.
- Merge-agent exploration remains available as archived evidence and visible session
  history without adding that exploration to the merged VIEW.

### Fixed

- Preserve Codex fork history, portable provider metadata, mixed-runtime sharing and
  paired tool evidence during capture, resume and export.
- Preserve reference, network, authentication, policy and cancellation error categories;
  bind credentials to the selected Hub and validate setup before applying changes.
- Keep failed remote API turns responsive, preserve shared-message authors and native
  execution feedback, and support HTTP CONNECT proxies between `agitd` and the Hub.
- Validate imports before adoption, prevent duplicate runtime claims, guard resume
  against tracking divergence, and preserve private-publication checks.
- Avoid repository-wide migration scans on clean stores and skip tags already present
  on a verified remote during push.

## [0.1.1] - 2026-09-04

### Added

- **A terminal interface for people.** `agit`, `agit resume`, `agit new`, `agit log`,
  `agit import`, `agit init` and `agit config` open a full-screen interface when run in
  an interactive terminal without their key argument: browse sessions, repositories,
  the timeline and conversation content; name and adopt sessions that are not tracked
  yet; an `init` wizard and a `config` editor; hand the terminal to Claude Code or
  Codex and come back to refreshed lists. Pipes, CI, scripts and agent sessions keep
  the existing output, and `--no-tui`, `AGIT_TUI=0` or any machine-output flag
  (`--json`, `-q`, `-y`) turns it off. See [docs/07_tui.md](docs/07_tui.md).
- **Update check.** On user-facing startup agit checks, at most once a day, whether the
  hub announces a newer release and prints a reminder; `agit upgrade` installs it.
  Nothing upgrades on its own.
- **A file keystore for machines without a credential store.** On an SSH login or a CI
  runner no Secret Service answers, so the secret-filter key had nowhere to go and the
  first `agit commit` whose transcript carried a heuristic finding failed with "cannot
  open the operating-system credential store". `agit config secrets.keystore file` (or
  `AGIT_SECRETS_KEYSTORE=file`) keeps the key in a private file under
  `$AGIT_HOME/keystore/` instead. Unix only, chosen explicitly and never a silent
  fallback; its protection is the file mode, so a backup of `$AGIT_HOME` carries the key
  along with the global vault — the boundary is drawn in
  [docs/05_global_secret_filter.md](docs/05_global_secret_filter.md).
- **`agit doctor` reports the secret keystore.** It probes the configured store the way
  a commit uses it and unlocks the vault if one exists, so a machine that cannot hold
  the key shows up at setup time rather than at the first commit that finds a secret.

### Fixed

- `agit fork` of a sealed branch no longer produces a sealed branch: the seal marker is
  branch-local and is dropped when the fork gets its identity (issue 23).
- Codex sessions under a custom `CODEX_HOME` are discovered and settled, and the
  SessionStart and Stop hooks locate the session and settle it correctly.
- Missing local branches, phantom `origin` branches, the log limit's performance, and
  cursor restoration after leaving the interface.
- When the OS credential store is unavailable, the error names both remedies — install
  and configure a credential store, or select the file keystore. Every other keyring
  error keeps its own meaning.
- Hints are highlighted in bright magenta so they stand out from ordinary output.

### Internal

- The GitHub mirror job clears stale replace refs before planting the graft, so a reused
  runner checkout no longer aborts the mirror.

## [0.1.0] - 2026-09-01

First public release.

- Lossless version control for agent sessions: `agit import`, `commit`, `push`, `clone`
  and `resume` for Claude Code, Codex, OpenCode and Cursor.
- Session lines as branches, workspaces, forks and merges across several people.
- Secret scanning before publishing, a device-local filter for registered low-entropy
  secrets, and reversible repository-local placeholders.
- Distribution through npm — `npx -y create-agit` or `npm i -g @einsia/agent-git` — with
  per-platform packages for Linux and macOS on x64 and arm64, and GitHub Release
  artifacts with `SHA256SUMS`.

[0.2.1]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.2.1
[0.2.0]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.2.0
[0.1.2]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.1.2
[0.1.1]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.1.1
[0.1.0]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.1.0
