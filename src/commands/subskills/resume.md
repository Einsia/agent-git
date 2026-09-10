---
name: agit-resume
description: Strictly resume a writable session branch.
---

# agit resume

## Purpose

Continue the current head of an existing branch. It does not turn a tag, historical commit, sealed branch, or someone else's branch into a new line; use `fork` or `open` for those cases.

## Synopsis

```bash
agit resume [branch|@] [options]
```

With no target at a terminal, this opens a session picker; choosing one hands the terminal to the runtime and takes it back when that exits. It stays text in pipes, in CI, and inside an agent session. Bare `agit` is the same thing.

The picker groups branches with the same recorded code repository across forge SSH and HTTPS
remote spellings. Repository paths remain case-sensitive, and custom service ports remain distinct.
Home-relative SSH paths outside the `git` service account and explicitly named SSH homes retain
their login and literal path identity.
This grouping offers candidates; it does not select a session target.

## Options

| Option | Meaning |
|---|---|
| `[branch|@]` | Explicit owner/repo@branch, or a branch in the repo supplied through `AGIT_SESSION`; `@` requires `AGIT_SESSION`. Omitted targets open an interactive picker |
| `--as <runtime>` | Runtime to use |
| `--cwd <dir>` | Runtime working directory |
| `--no-launch` | Resolve/materialize without starting |
| `--force` | Explicitly replace local active claims; known unintegrated tracking history still refuses |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface. `--tui` overrides the agent-session check but not `--json` / `-q` / `-y` |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

Preparing the same branch tip, runtime, and directory again reuses the existing runtime session when its recorded baseline remains valid. Without `--force`, a materialized instance whose branch advanced is superseded only when its recorded baseline proves that no new content exists; otherwise resume refuses before creating another writer.

Instance ownership is scoped to the selected local AgentGit store (`AGIT_HOME`, default `~/.agit`). New claims in `store/<runtime>/<native-id>.json` bind the native session to its owner/repository and branch; materialized claims also record the baseline used to detect unsettled content. Resume rechecks active claims and the selected branch head under local locks. Separate stores have separate claims, even on the same machine. A serialized `runtime_instances` field is retained for compatibility, but is not populated as an active registry or consulted for ownership.

Resume can prepare locally available history offline and does not acquire a Hub branch lease. It cannot establish whether another machine is still running the same branch. If machines advance independently, reconcile the histories explicitly with merge or fork. `--force` deliberately replaces local active claims even when their transcripts cannot be proven settled; it leaves those transcripts available for recovery. It cannot revoke an instance on another machine, and the selected branch and tracking-history checks still apply. Remote Control operates through the connected machine's daemon and does not turn this local claim mechanism into a global branch lease.

Resume checks the tracking ref configured for the explicitly selected branch, even when the primary checkout is on another branch. The tracking check uses only local objects and does not fetch updates; absent tracking refs are allowed. A known tracking tip must already be an ancestor of the local tip; remote advances and divergence refuse both native reuse and materialization, including `--force`. Tracking identity and tip changes across confirmation prompts also require a retry.

The refusal prints a manual merge command for that exact known tracking version and selected branch. It does not assume the remote is `origin` or that its branch has the same name. Add `--dry-run` instead of `--manual` to inspect the graph without opening a merge transaction. On Windows, copy the printed command into PowerShell.

## Examples

When both Git snapshots are available, different states or an uncertain worktree comparison require a decision.
Without an interactive terminal it reports the states and choices on stderr and exits with
code `8` before native session reuse or materialization. Use a terminal to choose whether to
inject an environment notice, or pass `--yes` to continue without that notice.

```bash
agit resume szh/p1@feature-a
agit resume szh/p1@feature-a --as codex --cwd ~/Projects/p1
agit resume szh/p1@handoff --no-launch --json
```

On failure, run `agit status` to check `AGIT_SESSION`, sealing, and the real ref.

Agent and script calls use `--no-launch --json` to prepare a runtime without
starting a nested TUI. Preparation may write a native transcript, session claim,
and shared memory files. Use `agit show szh/p1@handoff --json` or
`agit view szh/p1@handoff --json` for read-only inspection.

Without a target, `resume` offers the candidates it finds; when there is no terminal to choose with, it lists them on stderr and exits 8 — name the branch instead. A branch whose history contains a `revert`, `cherry-pick`, or `merge` is always materialized from its head VIEW rather than reusing the native session, because those commits change what the agent should see without changing the underlying log.

Turn commits record a compact `cwd_state` Git summary: origin, HEAD, branch, staged/unstaged/untracked/conflict counts, and a status digest. Before launching either resume path, AgentGit compares that summary with the selected cwd. Different or uncertain comparable states require the choice described above. Selecting the environment notice appends it to Claude system instructions. `--yes` continues without that CLI notice; a non-Git directory warns and continues. Configured native SessionStart hooks provide historical context independently of the CLI choice. Codex receives historical state through the AgentGit SessionStart hook: install with `agit setup --hooks --runtime codex`, then enable and trust it through Codex `/hooks`. AgentGit does not override configured Codex developer instructions or invent a user message. No code files are checked out or restored. Older sessions without a saved summary remain resumable. Matching status counts and digests do not prove that uncommitted file contents match; inspect the current checkout before relying on earlier code changes.
