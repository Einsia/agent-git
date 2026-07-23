---
sidebar_position: 3
title: Resuming sessions
---

# Resuming sessions

Resuming loads recorded context back into a runtime so an agent continues where it left off instead of
starting from an empty conversation. Use `agit start` to begin new work carrying the agent's context, and
`agit resume` to continue a specific recorded session.

## Start with the agent's context

```bash
agit start
```

This launches a session that already carries the agent's latest context, from whatever repository it was
last in. Whatever you run in it, capture records.

| Flag | Effect |
|---|---|
| `--agent <name>` | Run a specific agent for this invocation only. It does not flip the worktree default, so `agit start --agent frontend` and `agit start --agent api` can run at once in two terminals. |
| `--as <runtime>` | Launch in Claude Code or Codex. |

To set a default agent for the worktree instead, use `agit a switch <name>`.

## Continue a recorded session

```bash
agit resume
```

With no argument, `agit resume` loads the active agent's latest session. Name a different one to load it:

- `agit resume <agent>` loads that agent's latest session.
- `agit resume <session-id>` loads that specific session from the resolved agent's store.

| Flag | Effect |
|---|---|
| `--as <runtime>` | Load into Claude Code or Codex instead of the source runtime. |
| `--exec` | Launch the runtime on the session rather than only preparing it and printing the resume command. |
| `--cwd <path>` | Resume in a different working directory. |
| `--env <path>` | Pair the session with a specific code checkout (a different repo). |
| `--relocate` | Rewrite the session's recorded paths onto the current checkout when it is the same project moved. See [Relocating sessions](./relocating.md). |

`agit start` launches a fresh runtime here carrying the latest context; `agit resume` targets one exact
session and, without `--exec`, prints the native resume command rather than launching.

## Resume by name

When agit materializes a session for a runtime that resolves sessions by name, it names it
`<branch-slug>-<6hex>` (for example `feature-login-535719`) rather than a bare UUID. The name is
deterministic: the same source session always installs under the same name, so re-installing overwrites
rather than piling up copies. Codex accepts these names (`codex resume feature-login-535719` loads the
session); Claude Code requires a UUID, so Claude installs keep a fresh UUID.

## Resume by name across runtimes

A session recorded in one runtime can be resumed in the other after it is converted to that runtime's
format.

```bash
agit convert <session> --to codex --write
agit convert <session> --to claude-code --write
```

Without `--write`, the command reports what the conversion would produce. With `--write`, it installs the
result as a session the target runtime can resume, under a fresh id.

- A same-runtime conversion is a byte-for-byte copy.
- A cross-runtime conversion carries the prompts, replies, and tool activity across. It drops what the
  target has no equivalent for, such as encrypted reasoning and runtime-specific tool encodings.

The argument can be a session id or path, or an agent name (which converts that agent's latest session).
With no argument, `agit convert --to <runtime>` converts the active agent's latest session.

`--to` is required; a convert with no target runtime is a usage error.

## Convert automatically

The daemon converts sessions between runtimes as it records them, so you rarely run `convert` yourself:

```bash
agit watch --daemon
```

You can also run the auto-convert worker on its own, without the snap loop:

```bash
agit convert --watch
```

`agit convert --watch` keeps both runtimes' session sets in sync both ways, converting each new session
into the other's format. `--interval <n>` sets the poll interval in seconds (default 5). After either
worker runs, a session recorded in one runtime is always available to resume in the other. See
[Runtimes](./runtimes.md) for the per-runtime details, and [Capturing sessions](./capturing.md) for the
full `agit watch` loop.

## Bring the agent's tools with it

An agent captures its harness (its MCP servers, skills, and config) alongside its sessions. Apply that
harness so a resumed session has the same tools:

```bash
agit harness          # show the captured harness
agit harness apply     # apply it to this repository (asks first; --force to skip)
```
