# Changelog

Every notable change to agit, the AgentGit CLI, by release. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). A version's section here is the body of
its [GitHub Release](https://github.com/Einsia/agent-git/releases), and the
`@einsia/agent-git` npm package ships this file.

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

[0.1.2]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.1.2
[0.1.1]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.1.1
[0.1.0]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.1.0
