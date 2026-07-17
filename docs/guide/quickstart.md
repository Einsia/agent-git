---
title: Quickstart
nav_order: 2
---

# Quickstart

## Create an agent

Inside your code repository:

```
agit init --agent frontend
```

This creates the agent `frontend`, binds it to this repo (in a committed `.agit.toml`), and installs a
secret-scanning hook on its store.

## Capture what it did

Start a session that carries the agent's context, work as usual, then snapshot the transcript into the
agent's store and commit it:

```
agit start
agit snap
agit a commit -am "auth flow"
```

`agit a <git-command>` runs git against the agent's store. `agit snap` copies the runtime's session
files in. To capture continuously in the background instead of running `snap` by hand:

```
agit watch --daemon
```

## Share it

Point the store at a remote and push. `agit a` commands are git on the store, so this is the git you
already know:

```
agit a remote add origin https://hub.example.com/frontend.git
agit a push -u origin main
git add .agit.toml && git commit -m "declare the frontend agent"
```

`agit a push` records the remote in `.agit.toml` (credentials stripped) as it pushes, so a teammate's
clone can find the agent — commit that file so they get it. Later pushes just send new sessions.

## Pick it up on another machine

A teammate clones the code repo and runs:

```
agit a clone frontend      # reads .agit.toml, clones the agent's store
agit start                 # continue where it left off
```

`agit init` in a fresh clone does the same automatically for every agent the binding declares. The
agent keeps its identity, so it's the same agent, not a copy.

## Reconcile divergent work

When two people's sessions have diverged:

```
agit a merge frontend
```

The two sessions are revived, compare their work against the code, and reconcile what they can; only
genuine conflicts stop to ask you. The result is a resumable session (`claude --resume <id>` /
`codex exec resume <id>`). See [Merging](merging.html) for the details.

## Two agents at once

Selection is per command, so you can run two in the same repo:

```
agit start --agent frontend    # terminal 1
agit start --agent api         # terminal 2
```

`agit a switch <name>` sets a default for the worktree; `--agent` overrides it per command.
