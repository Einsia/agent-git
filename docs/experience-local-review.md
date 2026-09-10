# Experience review

This review covers the CLI, agent integration, TUI, Profile & Settings, and
search. The changes are developed in isolated worktrees and synchronized with
upstream main before review. The companion backend change is tracked in
[AgentGit-backend !104](https://git.xiaoaojianghu.fun:114/dev/agentgit/AgentGit-backend/-/merge_requests/104).
Each merge requires bot approval, no unresolved discussions, no conflicts, and
a successful pipeline for the reviewed head. Existing work is retained in the
implementation below.

## Command model

| Intent | Command |
| --- | --- |
| Choose a conversation to continue | `agit` or `agit resume` |
| Start a fresh conversation | `agit new` |
| Open a saved source, continuing or forking as appropriate | `agit open <owner/repo>@<ref>` |
| Continue the exact local session lineage | `agit resume <owner/repo>@<branch>` |
| Inspect or publish a selected session from a terminal | `agit log`, `agit push`, `agit share` |
| Address the agent's own session explicitly | `agit commit <owner/repo>@<branch>` |

`run` remains a hidden compatibility alias for `open`. `switch` and directory
branch pins are removed. Existing-session commands require an explicit target
or a valid `AGIT_SESSION`; a workspace binding, a discovered runtime ID, or the
only repo on a machine does not silently choose the target. A runtime ID checks
whether an inherited environment is stale. Its store link is read directly
instead of enumerating all adopted sessions.

The no-argument terminal flows for config, init, import, new, and resume were
already present. Log, push, and share now use transient session selection where
appropriate. Bare `agit` keeps the resume picker. Pipes, agent sessions, JSON,
quiet mode, and unattended operation do not unexpectedly open a full-screen UI.
Each named no-argument flow is exercised through a real PTY. Invalid session
names stay in the naming flow, import preserves the selected runtime's working
directory, and naming remains usable in narrow terminals. The new-session picker
shows local repositories immediately and discovers the signed-in account's Hub
repositories asynchronously. Remote selection uses the existing explicit clone
and creation path; discovery does not rotate authentication credentials.

The share wizard displays full repository and branch identities, then lets the
user choose encryption/public visibility, expiry, a view limit, and a
passphrase. It hands the explicit choice to the existing scan and confirmation
path. Saved refs share their selected VIEW; `--full-log` is deliberate. Native
runtime selectors remain explicitly labeled live-transcript sources.

## Agent context and output

Search supports repeated `--query`, shared filters, bounded concurrency, and
structured results. Redirected search stdout is JSON. Global `--json` uses the
common envelope and keeps structured search data under `result.value`.
Successful batch entries survive partial failure, while the exit code and MCP
`isError` report that failure. Noninteractive calls make no incidental startup
version request. `--author`, `--since`, and `--before` filter saved versions before
indexing, deduplication, ranking, counts, and pagination. A bounded immutable
metadata cache avoids repeated reads, and the Hub acknowledges the applied
predicates so an older server cannot silently ignore them. See
[the search audit](agent-cli-search-audit.md) for exact author/time semantics,
limits, and output coverage.

```bash
agit search --query "cache failure" --query "build timeout" \
  --repo alice/service --author alice@example.org --since 2026-09-01 \
  --before 2026-10-01 --runtime codex --in tool --json
```

Status, config, log, and branch reads provide command-specific JSON fields.
Status preserves complete runtime identities and pagination, including discovery
before first adoption. Config separates stored values, effective defaults, and
environment overrides. History uses complete OIDs and captured ref snapshots;
plain branch listings avoid unnecessary history traversal. Native JSON capture preserves stdin and original arguments without dispatching
a command twice. Windows captures CRT descriptors and Win32 handles together;
JSON v2 keeps typed recovery actions, with an explicit v1 compatibility option.
Noninteractive macOS credential-store operations suppress authorization dialogs
and return actionable errors. The process interaction setting is serialized and
restored, and authorization failures do not suggest replacing an existing vault's
keystore. A live protected-vault check returned promptly without changing its
history ref or encrypted vault. Interactive Keychain authorization remains under
the user's control.

Bundled Skill YAML and command examples are validated independently. A disposable
Codex workflow installs the exact bundle and project instructions, adopts an
explicit runtime, settles an explicit target, searches, reads VIEW, and prepares
a resume. Import does not pretend to change the calling process's environment.

Current turn commits can record origin, HEAD, branch, status counts, and a status
digest. Imported history does not acquire a fabricated historical code state.
The summary does not store file contents or status paths, and matching dirty
summaries do not prove that uncommitted contents match.

Resume retains the existing cwd comparison choices: continue, continue with an
environment notice, or cancel. Different or uncertain comparable states require
an explicit choice; unattended calls exit with code 8 unless `--yes` is supplied.
`--yes` continues without the CLI notice. A non-Git directory warns and continues.
Selecting the notice appends it to Claude system instructions. Configured native
SessionStart hooks provide historical context independently of that CLI choice.
Codex uses trusted AgentGit hooks, preserving configured developer instructions
and the user's prompt.
The hook also covers native TUI session changes, supplies the active explicit
AgentGit identity and historical state, and preserves a user's session title.
Unclaimed runtime sessions stay available for explicit naming/adoption.

URL credentials, URL query/fragment data, and recognized remote-helper payloads are
omitted from captured origins. Historical snapshots are sanitized again when
rendered. Literal local/SCP repository path characters remain intact. No code
checkout or restoration is performed by these notices.

Codex delivery requires installed, enabled, trusted hooks: run
`agit setup --hooks --runtime codex`, then review `/hooks` inside Codex.
Contracts were checked against the official
[Claude Code](https://code.claude.com/docs/en/hooks) and
[Codex](https://learn.chatgpt.com/docs/hooks) documentation. The supplied Claude
share page was unavailable, so it was not treated as implementation evidence.

## Profile & Settings

The corresponding backend branch is `codex/profile-settings-experience`.
Existing profile, avatar, email, password, identity-provider, device, organization,
storage, sharing, and repository administration concepts were audited.

- Failed profile, device, storage, share, usage, and collaborator reads can be
  retried in place. Stale responses cannot replace a newer account or repo.
- Connected sign-in providers are grouped with authentication settings. Existing
  email-page OAuth callback URLs still work. Failed provider discovery can be
  retried, and unlink guards count actual usable sign-in methods under the
  account write lock rather than stale credential projections.
- Organization owners can reach repository settings using backend-supplied
  capabilities. Writes still reauthorize. Unsupported organization visibility
  changes are not offered.
- Collaborator mutations update from successful responses without an extra GET.
  Organization management capability lookup uses one indexed SQL query instead
  of two sequential reads, without caching authorization.
- A compact mobile section selector exposes the settings content immediately.
  Content-driven row sizing keeps long collaborator lists clear of deletion
  controls. Upload inputs have accessible names, and organization team grants
  have an explicit route from repository access settings.
- Share management shows encryption, passphrase protection, and view limits.
  Encrypted shares require the original keyed URL instead of offering an unusable
  keyless link. Future expiry dates show an actual date and time.

## Validation and remaining boundaries

CLI validation covers the complete locked unit and integration suites,
strict all-target clippy, formatting, and comment/diff checks. The
integration coverage includes real PTYs, mock HTTP/MCP processes, explicit
target selection, saved VIEW sharing, and native hook payloads. Test homes and
Git configuration were isolated from personal state. All 41 bundled frontmatters
also passed independent YAML parsing.

Backend validation covers the complete locked test suite, strict all-target
clippy, identity boundary and source/build gates. The separately invoked
CLI/HTTP pairing test passed against the integrated binaries. Frontend
validation includes its complete test suite, TypeScript checks, and production
build. Browser checks used real components
with local mock API data on desktop and at mobile width, including keyboard
navigation, retry, focus restoration, uploads, saves, and organization selection.
No production account or deployment was used for that visual check.

Saved author/time filters apply to session search, including CLI batches and
MCP; other search categories and aggregate counts reject those filters explicitly.
Windows JSON capture has native integration coverage that must pass on the
current head pipeline; local macOS runs do not establish Windows behavior. Native context delivery still depends on the runtime executing its
trusted hooks. See [the completion checklist](experience-completion-checklist.md)
for the requirement-to-evidence mapping.
