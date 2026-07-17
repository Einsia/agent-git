---
title: Home
nav_order: 1
---

# agit

agit is a version-control tool for the sessions produced by AI coding agents. An agent's code changes
are already shared through Git; its session, the transcript of what it read, ran, concluded, and
intended, is not. agit stores that transcript in a git repository, synchronizes it with the standard
push and pull operations, and reconciles divergent sessions by having the agents themselves compare
their work.

It supports Claude Code and Codex, and treats them as equivalent runtimes.

## Model

The unit agit manages is an **agent**: a git repository containing the raw session transcripts an
agent has produced, identified by a stable identifier rather than by a name or a location. An agent is
distinct from the code repository it works in, which agit calls an **environment**. The relationship
is many-to-many: one agent can work across several environments, and one environment can host several
agents.

For a developer familiar with Git, the mapping is direct:

| Git | agit |
|-----|------|
| A repository of source files | An agent: a repository of session transcripts |
| `git clone <url>` | `agit a track <name>` (resolved from the environment's binding) |
| `git push` / `git pull` | `agit a push` / `agit a pull` |
| `git merge`, textual | `agit a merge <agent>`, a semantic reconciliation performed by the agents |
| `git log`, `git diff`, ... | `agit a log`, `agit a diff`, ... (git, run against the agent's store) |

`agit <args>` runs git against the environment (the code repository) unchanged. `agit a <args>` runs
git against the resolved agent's store. Commands that are not git, such as creating an agent or
capturing a session, are described in this guide.

## Installation

```
npm install -g @agentgit/agit
```

This installs the `agit` client and the `agit-hub` server. To build from source instead:

```
git clone https://github.com/Einsia/agent-git && cd agent-git
./build.sh --release
cp target/release/agit ~/.local/bin/
```

`build.sh` is used in place of `cargo build` because the project targets Rust edition 2024 and a v4
`Cargo.lock`, which require cargo 1.78 or later; the script locates a suitable cargo.

## Guide

1. [Quickstart](guide/quickstart.html) — create, capture, share, and merge an agent.
2. [How it works](guide/concepts.html) — the four ideas behind the tool.
3. [Merging](guide/merging.html) — reconciling divergent sessions.
4. [Runtimes](guide/runtimes.html) — Claude Code and Codex; session conversion.
5. [Migration](guide/migration.html) — rebinding identity and importing legacy stores.
6. [Configuration](guide/configuration.html) — environment variables and files.
7. [Command reference](guide/command-reference.html) — every command.

## Hosting

- [The hub](hub.html) — what `agit-hub` is, and how its permissions and API work.
- [Deploying the hub](deploying-the-hub.html) — running one for your team.
- [Architecture](architecture.html) — the session model, for contributors.
