---
title: Quickstart
nav_order: 2
---

# Quickstart

agit versions the sessions your coding agent produces — what it read, ran, and concluded — in a git
repo alongside your code. This runs the whole loop: create an agent, turn on the daemon, work, share,
and reconcile. `agit a <git-command>` is git on the agent's store; everything you know about git
applies.

## Create an agent

Inside your code repo:

```
agit init --agent frontend
```

That mints a new agent named `frontend`, binds it to this repo through a committed `.agit.toml`, and
installs a secret-scanning hook on its store. Name it for what it knows (`frontend`, `payments-api`),
not for you or the folder.

`agit init` sets up the repo, so run it once. To add a second agent to a repo you've already set up,
use `agit a init <name>`.

## Turn on the daemon

```
agit watch --daemon
```

Set this once. It runs in the background and does two things as you work: it snapshots new sessions
into the agent's store, and it converts each one between Claude Code and Codex so a session recorded in
either CLI stays resumable in the other. After this you never run a capture command again.

| Command | Effect |
| --- | --- |
| `agit watch --daemon` | start it in the background |
| `agit watch --status` | whether it's running and what it has captured |
| `agit watch --stop` | stop it |
| `agit watch` | run it in the foreground |

Pass `--no-convert` to snapshot only and skip the runtime conversion.

Without the daemon you capture by hand: `agit snap` copies the runtime's current session files into the
store, then `agit a commit -am "auth flow"` commits them. The daemon just does both for you.

## Work

```
agit start
```

This launches a session already carrying the agent's context from this repo. Work normally — the daemon
captures as you go.

- `--agent <name>` runs a specific agent; `agit a switch <name>` sets the worktree default so you can
  omit it. Selection is per command, so `agit start --agent frontend` and `agit start --agent api` can
  run side by side in two terminals.
- `--as <runtime>` chooses Claude Code or Codex. Commands that read sessions use the runtime you name,
  else the only one installed, else they ask. See [Runtimes](runtimes.html).

## Share it

Sharing is plain git on the store:

```
agit a remote add origin https://hub.example.com/frontend.git
agit a push -u origin HEAD
git add .agit.toml && git commit -m "declare the frontend agent"
```

`agit a push` records the remote into `.agit.toml` as it pushes, with credentials stripped. Commit that
file so teammates get the agent and know where to clone it. Later pushes send only new sessions.
`agit a pull` fast-forwards; if the histories have diverged it routes you to `agit a merge`.

For a shared server with a web UI and per-agent permissions, see [The hub](../hub.html).

## Pick it up elsewhere

A teammate clones the code repo, then runs `agit init`. It reads `.agit.toml` and clones every agent
the binding declares, so one command sets them up:

```
agit init
agit start
```

To pull a single agent instead of all of them, use `agit a clone frontend`. Either way it's the same
agent, carrying the same identity (its aid), not a copy.

## Reconcile

When two people's sessions have diverged:

```
agit a merge frontend
```

Both sides' latest sessions are revived read-only, compare their work against the code, and reconcile
what they can. The output is a resumable merged session (`claude --resume <id>` /
`codex exec resume <id>`) plus the list of genuine conflicts left for you. It uses a model, so it's a
real semantic merge rather than a line-by-line one, and it isn't deterministic. See
[Merging](merging.html).
