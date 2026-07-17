---
title: Home
nav_order: 1
---

# agit

agit is version control for the sessions an AI coding agent produces (Claude Code and Codex). Your
code is already shared through Git; the session — what the agent read, ran, concluded, and intended —
is not. agit stores that session in a git repository, syncs it with push and pull, and reconciles
divergent sessions by having the agents compare their own work against the code.

## The two scopes

agit wraps git twice. `agit <git-args>` runs git against your code repository, transparently — what
you already type keeps working. `agit a <git-args>` runs git against the resolved agent's store, a
normal git repository of session transcripts kept under `~/.agit`. The agent scope maps to git
directly:

| Git (your code) | agit (the agent's store) |
|---|---|
| `git clone <url>` | `agit a clone <name>` — resolves the URL from `.agit.toml` |
| `git push` / `git pull` | `agit a push` / `agit a pull` |
| `git merge` (textual) | `agit a merge <agent>` — a semantic merge the agents perform (see [Merging](guide/merging.html)) |
| `git log`, `git diff`, ... | `agit a log`, `agit a diff`, ... — plain git against the store |

One committed file, `.agit.toml`, declares which agents a repo uses and where to clone them, so a
teammate's fresh clone can pull the same agents. One agent can work across many repos, and one repo
can host many.

## Install

```
npm install -g @einsia/agentgit
```

This installs the `agit` client. Optionally route `git` through `agit`, so ordinary git commands also
version your agent's sessions:

```
agit shadow install     # bash, zsh, fish, or PowerShell; undo with agit shadow uninstall
```

The `agit-hub` server is distributed separately — teams host it with Docker or build it from source
(see [Deploying the hub](deploying-the-hub.html)).

## The daemon

Run `agit watch --daemon` once and leave it: it snapshots new sessions into the agent's store as you
work, and converts them between Claude Code and Codex so a session recorded in one is resumable in the
other.

## Guide

1. [Quickstart](guide/quickstart.html) — create, capture, share, and merge an agent.
2. [How it works](guide/concepts.html) — the ideas behind the tool.
3. [Merging](guide/merging.html) — reconciling divergent sessions.
4. [Runtimes](guide/runtimes.html) — Claude Code and Codex, and session conversion.
5. [Migration](guide/migration.html) — rebinding an agent's identity for recreated remotes and forks.
6. [Configuration](guide/configuration.html) — environment variables and files.
7. [Command reference](guide/command-reference.html) — every command.

## Hosting

- [The hub](hub.html) — what `agit-hub` is, and how its permissions and API work.
- [Deploying the hub](deploying-the-hub.html) — running one for your team.
