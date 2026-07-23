---
sidebar_position: 2
title: Capturing sessions
---

# Capturing sessions

Capturing records a session an agent produced into that agent's store. A recorded session holds the full
transcript: the prompts you gave, the agent's replies, the tools it ran, and the files it edited. Capture
by hand with `agit a snap`, or leave `agit watch` running to capture continuously.

## What gets committed

Claude Code and Codex each write the live session to their own directory as you work. A capture mirrors
that session dump into the agent's store and commits it, along with the agent's harness (its MCP servers,
skills, and config, with secrets redacted). Nothing in your code repository changes except the committed
`.agit.toml` binding.

Each committed session is attributed to your git committer identity. A store commit carries a session,
and a session with no committer could never be attributed, so agit refuses to capture while your identity
is unset:

```bash
git config --global user.email you@example.com
git config --global user.name  "Your Name"
```

agit's own bookkeeping commits (minting an agent, a rename) are authored by `agit@local` and never need
your identity, so a new machine can create agents before you set one. See
[Identity and signing keys](./identity.md) for the signing that attribution rides on.

Every capture runs the secret scanner first. A suspected secret is mirrored to disk but held out of git
history until you resolve it. See [Secrets](./secrets.md).

## Snap by hand

```bash
agit a snap
```

This mirrors the runtime's current session files into the store and commits them in one gated step. With
no runtime named, snap captures every runtime that has sessions in this repository. Name one when both
are present:

```bash
agit a snap codex
agit a snap --from claude-code
```

| Flag | Effect |
|---|---|
| `--from <runtime>` | Capture one runtime. A bare positional (`agit a snap codex`) is shorthand for the same. |
| `--no-harness` | Capture sessions only; skip the MCP/skills/config harness. |
| `--watch` | Run snap continuously for the named runtime (see below). |
| `--interval <n>` | Poll interval in seconds for `--watch` (default 5). |

## Auto-snap

`agit a snap --watch` polls one runtime's dump and snaps each new session as it appears. Naming a runtime
watches that one; the unnamed both-runtimes loop is `agit watch` (below).

```bash
agit a snap --from codex --watch
```

## Hands-off with `agit watch`

```bash
agit watch
```

`agit watch` is the fully hands-off path. It watches both runtimes' dumps and does two things as you work:

- Auto-snaps each new session into the store.
- Auto-converts each session between Claude Code and Codex, so a session recorded in one is resumable in
  the other. See [Runtimes](./runtimes.md).

Run it in the foreground, or as a background daemon:

| Command | Result |
|---|---|
| `agit watch` | Run in the foreground. |
| `agit watch --daemon` (alias `--background`) | Run forever in the background. |
| `agit watch --status` | Report whether the daemon is running and what it has captured. |
| `agit watch --stop` | Stop the daemon. |
| `agit watch --no-convert` | Auto-snap only; skip runtime conversion. |
| `agit watch --no-harness` | Capture sessions only; skip the harness. |
| `agit watch --interval <n>` | Poll interval in seconds (default 5). |

Set it once and leave it running; you do not run a capture command again. `agit a list` and `agit a
status` mark an agent with a live watcher.

## Review what was captured

```bash
agit a log            # the agent's sessions, most recent first
agit a status         # this repo's agents, session counts, last activity, watcher state
agit a diff           # the prompts and edits added since your last push
```

`agit a log` and `agit a diff` render the session view of the store. Pass `--raw` (or `--git`) to fall
back to a plain `git log` or `git diff`. See [Divergence](./divergence.md) for how `agit a log` renders
branched sessions.
