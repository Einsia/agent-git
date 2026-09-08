---
name: agit-resume
description: Strictly resume a writable session branch.
---

# agit resume

## Purpose

Continue the current head of an existing branch. It does not turn a tag, historical commit, sealed branch, or someone else's branch into a new line; use `fork` or `run` for those cases.

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
| `--force` | Replace an active runtime claim; known unintegrated tracking history still refuses |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface. `--tui` overrides the agent-session check but not `--json` / `-q` / `-y` |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

Preparing the same branch tip, runtime, and directory again reuses the existing runtime session. A materialized instance whose branch advanced is superseded only when its recorded baseline proves that no new content exists; otherwise resume refuses before creating another writer.

Resume checks the tracking ref configured for the explicitly selected branch, even when the primary checkout is on another branch. The tracking check uses only local objects and does not fetch updates; absent tracking refs are allowed. A known tracking tip must already be an ancestor of the local tip; remote advances and divergence refuse both native reuse and materialization, including `--force`. Tracking identity and tip changes across confirmation prompts also require a retry.

The refusal prints a manual merge command for that exact known tracking version and selected branch. It does not assume the remote is `origin` or that its branch has the same name. Add `--dry-run` instead of `--manual` to inspect the graph without opening a merge transaction. On Windows, copy the printed command into PowerShell.

## Examples

```bash
agit resume feature-a
agit resume @ --as codex --cwd ~/Projects/p1
agit resume handoff --no-launch
```

On failure, run `agit status` to check `AGIT_SESSION`, sealing, and the real ref.

Without a target, `resume` offers the candidates it finds; when there is no terminal to choose with, it lists them on stderr and exits 8 — name the branch instead. A branch whose history contains a `revert`, `cherry-pick`, or `merge` is always materialized from its head VIEW rather than reusing the native session, because those commits change what the agent should see without changing the underlying log.

Turn commits record a compact `cwd_state` Git summary. Before launching either resume path, AgentGit compares that summary with the selected `--cwd`. On a mismatch it shows the recorded and current states and offers three choices: continue, continue with an environment notice injected as runtime system/developer instructions, or cancel. If the selected cwd is not a Git repository, comparison is unavailable; AgentGit warns and continues normally.
