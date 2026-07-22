---
title: Resume a session
nav_order: 4
---

# Resume a session

Resuming loads an agent's recorded context into a runtime so the agent continues where it left off,
instead of starting from an empty conversation. Use `agit start` to begin new work with the agent's
context, and `agit resume` to continue a specific recorded session.

## Start work with the agent's context

```
agit start
```

This launches a session that already carries the agent's context from this repository. Whatever you run
in it, the daemon records.

- `--agent <name>` runs a specific agent. Selection is per command, so `agit start --agent frontend` and
  `agit start --agent api` can run at the same time in two terminals. To set a default for the worktree
  instead, use `agit a switch <name>`.
- `--as <runtime>` chooses Claude Code or Codex. See [Move a session between runtimes](runtimes.html).

## Continue a recorded session

```
agit resume
```

With no argument, `agit resume` loads the active agent's latest session. To continue a different one,
name it:

- `agit resume <agent>` loads that agent's latest session.
- `agit resume <session-id>` loads that specific session from the resolved agent's store.

Options:

| Option | Result |
|---|---|
| `--as <runtime>` | Load the session into Claude Code or Codex. |
| `--exec` | Launch the runtime on the session rather than only preparing it. |
| `--cwd <path>` | Resume in a different working directory. |
| `--env <path>` | Pair the session with a specific code checkout. |
| `--relocate` | Rewrite the session's recorded paths onto the current checkout. |

## Bring the agent's tools with it

An agent captures its harness (its MCP servers, skills, and config) alongside its sessions. Apply that
harness to the current repository so a resumed session has the same tools:

```
agit harness          # show the captured harness
agit harness apply     # apply it to this repository
```
