---
title: Capture agent sessions
nav_order: 3
---

# Capture agent sessions

agit records each session an agent produces into the agent's store, a git repository under `~/.agit`.
A recorded session holds the full transcript: the prompts you gave, the agent's replies, the tools it
ran, and the files it edited. You capture sessions automatically with the daemon, or by hand with
`agit snap`.

## What agit records

Claude Code and Codex each write the full session to their own directory as you work. agit mirrors that
session dump into the agent's store and commits it. Nothing about your code repository changes except
the committed `.agit.toml` binding.

Each session is attributed to your git identity, so set it once before you capture (see
[Get started](quickstart.html)). agit refuses to record a session while your identity is unset.

## Capture automatically with the daemon

```
agit watch --daemon
```

This starts a background process that watches both runtimes and does two things as you work:

- Records each new session into the agent's store.
- Converts each session between Claude Code and Codex, so a session recorded in one tool can resume in
  the other. See [Move a session between runtimes](runtimes.html).

Set it once and leave it running. You do not run a capture command again.

| Command | Result |
|---|---|
| `agit watch --daemon` | Start the daemon in the background. |
| `agit watch --status` | Show whether it is running and what it has captured. |
| `agit watch --stop` | Stop it. |
| `agit watch` | Run it in the foreground instead of the background. |
| `agit watch --no-convert` | Record sessions only; skip runtime conversion. |

## Capture by hand with `agit snap`

If you would rather not run the daemon, capture the current session with:

```
agit snap
```

This mirrors the runtime's current session files into the store and commits them in one step. It runs
the secret scan first, exactly as the daemon does. Name a runtime with `--from claude-code` or `--from
codex` when both have sessions here; otherwise `agit snap` captures every runtime that has sessions in
this repository.

A suspected secret is mirrored to disk but held out of git history until you resolve it. See
[Keep secrets out of shared history](secrets.html).

## Review what was recorded

```
agit a log            # the agent's sessions, most recent first
agit a status         # this repository's agents, session counts, last activity, watcher state
agit a diff           # the prompts and edits added since your last push
```

`agit a log` and `agit a diff` render the session view of the store. Pass `--raw` (or `--git`) to fall
back to a plain `git log` or `git diff` on the store.
