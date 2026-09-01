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

## Options

| Option | Meaning |
|---|---|
| `[branch|@]` | Branch name; `@` means the current session branch |
| `--as <runtime>` | Runtime to use |
| `--cwd <dir>` | Runtime working directory |
| `--no-launch` | Resolve/materialize without starting |
| `--force` | Force recovery from an already-running or otherwise abnormal state (use carefully) |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface. `--tui` overrides the agent-session check but not `--json` / `-q` / `-y` |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit resume feature-a
agit resume @ --as codex --cwd ~/Projects/p1
agit resume handoff --no-launch
```

On failure, run `agit status` to check `AGIT_SESSION`, sealing, and the real ref.

Without a target, `resume` offers the candidates it finds; when there is no terminal to choose with, it lists them on stderr and exits 8 — name the branch instead. A branch whose history contains a `revert`, `cherry-pick`, or `merge` is always materialized from its head VIEW rather than reusing the native session, because those commits change what the agent should see without changing the underlying log.

Turn commits record a compact `cwd_state` Git summary. Before launching either resume path, AgentGit compares that summary with the selected `--cwd`. On a mismatch it shows the recorded and current states and offers three choices: continue, continue with an environment notice injected as runtime system/developer instructions, or cancel. If the selected cwd is not a Git repository, comparison is unavailable; AgentGit warns and continues normally.
