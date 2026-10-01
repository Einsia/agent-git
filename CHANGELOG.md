# Changelog

Every notable change to agit, the AgentGit CLI, by release. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). A version's section here is the body of
its [GitHub Release](https://github.com/Einsia/agent-git/releases), and the
`@einsia/agent-git` npm package ships this file.

## [Unreleased]

### Fixed

- `agit push <owner/repo@branch> --to <existing repository>` (and `agit project sync`
  copies of sessions claimed elsewhere) no longer push the source repository's
  `main` file line. The destination keeps its own `main`; before, the copy failed
  as a rejected non-fast-forward, or fast-forwarded the destination's shared files
  to the source's. A destination the push creates still receives the source's `main`.
- A Claude Code prompt whose record starts with a runtime-injected block, such as the
  worktree reminder the desktop app puts ahead of a session's first message, is
  kept as the turn's prompt instead of being dropped with the injection, whether
  the reminder is a separate text block or leads the same string. Such a session
  lost its opening prompt, and one with a single prompt had no turn to save or
  upload.

## [0.2.16] - 2026-10-02

### Added

- Each settled commit records the runtime's own name for the session in
  `session/meta.json` as `title`: the latest Claude Code title (`custom-title`,
  falling back to an older transcript's `summary`) or the Codex thread name from
  `session_index.jsonl`. A rename appears from the next settlement on. The
  `agit: unnamed` and `agit <owner/repo@branch>` placeholders that agit's own
  SessionStart hook assigns are not recorded. The title is protected like other
  metadata observations and is not part of encrypted metadata projection.

### Changed

- Privacy processing runs in a bounded local worker. Detected secrets receive
  reversible placeholders when their originals can be saved. Incomplete scans
  keep completed replacements; unavailable storage, scan failures and timeouts
  skip the affected privacy work and let conversation operations continue.
- Signed-in accounts obtain an automatically generated cloud dictionary key.
  Without a usable key, mappings remain in private local pending storage. Push,
  login and fetch schedule encrypted dictionary synchronization independently;
  another signed-in device can restore mappings after synchronization succeeds.
- Local default, global user and repository user rules share one policy path.
  Explicit blocks override allows, and allows override heuristic findings.
  Recovery records survive rule changes without becoming permanent block rules.
- Uploads require no secret-finding review or cloud privacy-policy resolution.
  Existing Git history is preserved. LFS payloads are outside privacy processing;
  their authorization, availability and integrity checks remain in place.
- This release requires the matching backend update to remove older server
  privacy gates. A skipped local transformation may retain original sensitive
  content, and pending dictionaries become recoverable on other devices only
  after successful encrypted synchronization.

- Project Stop hooks start session uploads in a background process and return
  immediately. Project status reports the resulting upload state.

### Fixed

- Automatic project binding installs hooks for available runtimes that support
  them; other runtime sessions remain publishable through project sync.

- Live output preserves every emitted fragment and the remaining stream tail
  when local privacy processing is unavailable.
- Legacy native message identifiers stored as secret placeholders can be restored
  without changing unmasked carrier identities.

- `agit project sync` publishes a session that another repository already claims, such
  as one started from a desktop RC project, as a separate copy of that claim's branch
  in the project repository, instead of failing with "session already belongs to another
  repository". The original claim and the RC project's own publication target are
  unchanged; each later sync refreshes the copy. `--history none` keeps excluding such
  earlier sessions.

## [0.2.15] - 2026-10-01

### Changed

- **New repositories use ordinary publication by default again.** Without
  `--encryption` or a `privacy.encryption` preference, `agit push`, `agit init`,
  `agit repo create` and the init wizard create repositories that publish original
  history and need no viewing password. Encryption is an explicit opt-in chosen at
  creation with `--encryption=true` or `agit config privacy.encryption true`, after
  which `agit privacy init` sets the viewing password. `agit privacy init` still
  creates a missing repository encrypted unless `false` is chosen, and existing
  repositories keep the mode they were created with. A separate `agit push --to`
  destination created from an encrypted repository stays encrypted unless
  `--encryption=false` or a `privacy.encryption` preference chooses otherwise.
- Local repositories set up by 0.2.13 or 0.2.14 without an explicit `--encryption`
  choice follow the new default. If `push.auto` is on and the repository does not
  exist on the Hub yet, the next automatic push creates it as an ordinary repository,
  private unless `push.visibility` says otherwise. To keep such a repository
  encrypted, run `agit privacy init OWNER/REPO` first, or set
  `agit config --global privacy.encryption true`.
- A checkout initialized with `agit init --encryption=true` refuses its first push
  to an existing repository that uses ordinary publication, instead of silently
  publishing original history there; pass `--encryption=false` to publish anyway,
  or push to a new repository name to create an encrypted one.
- Encrypted `agit share` links require an encrypted repository. Sharing from an
  ordinary repository needs `--public`; without it the command now says so and
  uploads nothing, instead of asking for a publication that cannot exist. Encrypted
  links for ordinary repositories are not available in this release.

### Fixed

- Ordinary pushes from scripts, CI and agent sessions no longer require `--yes`;
  a person at a terminal is still asked to confirm the destination, and declining
  or cancelling that prompt publishes nothing. Secret findings still block
  publication unless `--allow-secrets` is given, and encrypted publication keeps
  its confirmation.
- Automatic ordinary pushes after settlement, and Remote Control publication to an
  ordinary repository, no longer need a prior explicit push. `push.auto` authorizes
  them and they pass the same identity, access and secret checks as a manual push.
  Outside Remote Control, a missing repository is created with the visibility a
  non-interactive first push uses. Encrypted repositories still require an explicit
  push to confirm their policy and viewing recipient before automatic publication.
- Remote Control retires temporary runtime sources automatically once their
  directories disappear.

## [0.2.14] - 2026-09-30

### Added

- `agit project bind` enrolls a selected directory and its unbound subdirectories
  in an existing repository, with explicit history import and automatic upload
  choices. `project sync` retries synchronization, `project status` reports local
  results, and `project unbind` pauses project capture. Codex and Claude Code hooks
  preserve existing session claims and enforce the current project policy before
  publishing.

### Fixed

- Repeated Remote Control history reads reuse bounded, authenticated secret
  dictionary records and compiled protection patterns. Changed ciphertext,
  keys, or protection rules invalidate the corresponding cached state.
- Release builds optimize for runtime speed to reduce history projection and
  secret scanning latency.
- Remote Control retries failed local session saves while idle, without requiring
  another user turn to resume archiving.
- Push uses authenticated remote history to avoid rescanning already-published
  objects, while continuing to inspect new objects and their selected LFS payloads.
  Use `--audit` when a complete history review is needed.
- Native history reads take authenticated, bounded dictionary snapshots without
  waiting for a dictionary writer. Conservative pattern filtering avoids scanning
  history for secret patterns that cannot occur while preserving redaction rules.
- Compatible Cloud servers can read native history through a scoped project
  controller without opening a separate session controller. Project controllers
  remain read-only and cannot subscribe to or change a conversation.
- Completed RC turns retain durable archive jobs until protected publication has
  a receipt. A daemon restart retries pending jobs without starting the model or
  requiring a viewer to open the conversation.
- Settlement subprocesses retry temporary executable-busy failures within a
  bounded window while retaining the current authority lease.

## [0.2.13] - 2026-09-30

### Changed

- **Installations that skip install scripts are counted.** An installation whose
  installer recorded no install receipt, such as one run through `npx`, with
  npm `--ignore-scripts`, or by a package manager that skips install scripts,
  now records one from its first user command, marked
  `receipt_origin=first_command`; installer receipts are marked
  `receipt_origin=installer`. Setup, hooks, MCP, RC, background processes and
  commands an installer runs never record it, so a create-agit receipt keeps its
  acquisition key. Installations whose statistics state predates this release,
  or whose `AGIT_HOME` already held older state, are never reported this way. See
  [First-command receipt](docs/telemetry.md#acquisition-funnel-contract).

- **Session reuse and invitation counts on the Hub.** When `agit run` continues
  a session line or forks from a point on one, it sends the Hub one receipt
  naming the repository, the source session ID and commit, whether the run
  continues or forks, and the per-process operation ID, so the Hub can count
  how often sessions are picked up. A continue is reported once the session is
  ready to launch, a fork once its branch exists, and a `--mine` fork to the
  repository it copied. The receipt is sent in the background only for
  checkouts of the configured Hub, only from a settled session, and never with
  renewed credentials: an expired sign-in sends nothing. A Hub that is
  unreachable or refuses it never changes the run's output or exit code.
  `agit fork` and `agit resume` stay offline.
  `agit repo invite <owner/repo>@<branch>` also tells the Hub which session its
  link opens. The download attribution headers still carry no arguments or
  names. See
  [Session reuse and invitations](docs/telemetry.md#session-reuse-and-invitations).

### Fixed

- Remote Control can return a fresh, bounded native history page with watch
  admission, avoiding a separate network round trip when opening a conversation.
  Native history transfers negotiate compression, preserving complete tool
  results and compatibility with older clients and servers. Optional history
  reads do not block the daemon or prevent a successful live subscription.
- Device name changes synchronize to Cloud without enrolling another device.
- Failed session publication retries while the conversation is idle, so a
  transient upload failure does not require another user message to recover.

- `agit login` checks that it can save credentials under `AGIT_HOME` before it
  asks the Hub for anything. An agent sandbox that refuses writes there no
  longer uses up the human's approval: login stops with the directory to allow
  and says no login request was created, and `agit login --complete` leaves the
  approved sign-in unclaimed so the same command finishes it later. If saving
  still fails after sign-in, the new Hub session is signed out again.
- Signing in from an agent no longer fails when the agent runtime stops the
  waiting `agit login`. Every browser and device-code request is recorded
  privately under `AGIT_HOME` when the Hub creates it, and
  `agit login --complete` without a value finishes it from any later process,
  including an interrupted `agit login --device`. `--complete` now waits for
  the approval for up to 90 seconds (`--wait <seconds>`, `0` checks once)
  instead of checking once, keeps waiting through a failed poll, keeps the
  request when the wait runs out, and forgets it when the Hub says it expired
  or was already used. Running the printed `--complete` command again after
  the sign-in finished reports success instead of "no longer valid". A new
  `agit login` finishes a request the human already approved instead of
  replacing it. `agit whoami --check`, `agit commit` and commands that stop
  with "not logged in" first claim an approved request once, and otherwise
  point to `agit login --complete` instead of a new login. Signing out cancels
  a sign-in still being claimed: a session approved after `agit logout` is
  signed out again instead of saved, and a session a concurrent sign-in saved
  while `agit logout` ran is revoked with the credentials it removes. A second
  `agit login --complete` or a new `agit login` started while another process
  claims the same sign-in waits for it and reports success instead of "no
  longer valid" or a new login link.
- Local state problems read as local problems. `agit whoami --check` reports
  an unwritable home or a held credential lock instead of "can't reach" the
  Hub, `agit doctor` checks that `AGIT_HOME` is writable, and every command
  that meets one exits with the precondition code. Messages name the agit lock
  another process holds and say that agit's lock files are released when their
  process exits and must not be deleted; credential locks give up after a
  bounded wait, and session locks say what they are waiting for.
- Re-importing a session describes its claim: a registration is saved by the
  import, a saved session says where and how to record later turns, and a
  session saved elsewhere says how to move it. The link file path is no longer
  offered as a next step, and a failed import restores the session's previous
  link, including none.
- A session targeted at `main` is refused before anything is adopted or
  created, with the reason and an example branch in the caller's syntax.
- `agit push` leaves an `origin` that already points at the destination alone
  and writes a changed one under the repository lock, so a manual push and a
  hook's auto-push no longer collide on `.git/config`. Git failures on a held
  lock say not to delete it while a git process runs; a refused write says the
  checkout is not writable instead.
- `agit setup` recognizes its own hooks by their arguments, so installing from
  another executable path replaces the old entry instead of adding a second
  one.

## [0.2.12] - 2026-09-28

### Fixed

- Retry RC daemon reconciliation when a global npm installation's lifecycle
  scripts are disabled. The next CLI invocation schedules the guarded handoff
  in the background, while project-local and temporary npm packages cannot
  replace a persistent daemon.

## [0.2.11] - 2026-09-28

### Fixed

- Reconcile the local RC daemon after self-upgrade, global npm installation, or
  `npx create-agit`, including installations at a different executable path.
  Idle sessions attached to a persistent Codex server can detach for a safe
  restart; busy daemons retry after their work clears.
- Stage the `create-agit` executable before replacing it, so an installation
  failure preserves the previous CLI. Report when another `agit` on `PATH`
  shadows the installed copy.
- Reach the Hub without a fixed stall when one of its addresses is unreachable
  from your network. agit dialed the resolved addresses one after another and
  gave the first one most of the connection budget, so every command that talked
  to the Hub could wait about 20 seconds before trying the next address. The
  addresses now race: the next one starts after 250 ms or as soon as the
  previous one fails. Hub API requests, SOCKS proxy connections, LFS
  availability checks and `agit upgrade` downloads all use it.

## [0.2.10] - 2026-09-28

### Fixed

- Preserve native catalog scan health through delegated permission filtering,
  including incremental responses with no changed conversations. Remote Control
  can distinguish a running scan from an unavailable source while source
  identities remain scoped to readable conversations.

## [0.2.9] - 2026-09-27

### Fixed

- Accept start and steer instructions for attached, source-qualified Codex
  conversations through Cloud controllers. Managed conversations retain their
  executor identity and control policy; catalog entries alone do not authorize
  these operations.
- Keep runtime source identities when local builds differ in directory creation
  time support. Device and inode checks remain required, and conflicting known
  creation times still reject a replaced directory.

- Preserve the enrolled runtime source when resolving a saved conversation after
  a daemon restart, so Cloud controllers resume the same native conversation.

## [0.2.8] - 2026-09-27

### Added

- Discover conversations in registered Codex runtime homes independently of RC
  attachment. `agit rc sources add` accepts custom directory names and selected
  executables or endpoints. Same-account native processes and recognized profiles
  can enroll their runtime homes automatically.
- Connect RC and native Codex clients to the same persistent app-server, using
  its standard socket when supported. Both launch orders preserve one native
  conversation and keep the service alive after a subscriber disconnects.
- Persist source-qualified catalog identities and incremental discovery cursors,
  including reconnect recovery, removals, source health, and missing-index fallback.
- Retain native parent relationships and internal-conversation provenance for
  scoped expanded catalog views.

### Fixed

- Queue messages into the existing native conversation and execute them without
  starting a competing writer. Durable message receipts prevent repeated delivery.
- Read and update supported native model, reasoning effort, and permission
  settings through the selected runtime source, preserving its current policy
  during attachment. Shared approvals reconcile across subscribers.
- Release acquired private fallback control after 15 minutes without a human
  instruction only when native background unloading is verified. Running work
  and transcript observation continue; shared service connections do not expire.
- Revalidate source, project, caller, and delegated authority across reconnects,
  control transitions, pending instructions, and approval replies.

## [0.2.7] - 2026-09-25

### Fixed

- Validate lexical and canonical private-state ancestors consistently for Remote
  Control and bundled Git. Sticky shared home directories remain usable while
  replaceable ancestors and writable private state are rejected.
- Page large protected conversations without loading the entire history into
  memory or exhausting the snapshot byte budget.
- Preserve real device paths in Remote Control and allow authorized folder
  browsing outside the home directory. Report unreadable folders as errors and
  reject binding the filesystem root as a project.
- Expose the observed model, reasoning effort, and permission mode while
  following a native Codex conversation without acquiring its writer. Setting
  changes remain under the original controller's authority.
- Protect imported and newly created session metadata, retain structured MCP
  image context, and handle escaped repository placeholders correctly.
- Apply consistent publication inspection to binary artifacts and Git LFS content.

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
