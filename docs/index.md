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

agit adds a layer on top of git rather than replacing it. `agit <git command>` runs git against your
code repository, transparently — everything you already type keeps working, `agit commit` and
`agit push` included. Put `a` after `agit` and the same command runs against the agent's store instead,
a normal git repository of session transcripts kept under `~/.agit`:

| You type | Runs git against |
|---|---|
| `agit <git command>` | your code repo — ordinary git |
| `agit a <git command>` | the agent's store — plain git, plus a few agent-aware verbs |

Most `agit a` commands are plain git on the store. The ones that do more are git verbs that mean
something specific for an agent: `agit a clone <name>` clones by identity (resolving the URL from
`.agit.toml`), `agit a push` records the store's remote into `.agit.toml`, and `agit a merge <agent>`
reconciles two sessions by dialogue rather than textually (see [Merging](guide/merging.html)).

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
7. [Security](guide/security.html): the secret gate and session provenance.
8. [Command reference](guide/command-reference.html) — every command.

## Hosting

- [The hub](hub.html) — what `agit-hub` is, and how its database, permissions, and API work.
- [Deploying the hub](deploying-the-hub.html) — running one for your team.
