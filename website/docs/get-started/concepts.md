---
sidebar_position: 4
title: Concepts
---

# Concepts

The mental model behind agit. Read this once; the rest of the docs assume it.

## Two scopes

Every `agit` command runs git against one of two repositories. The word right after `agit` selects which.

- **`agit <git command>`** runs git against your **code repository** (the environment), unchanged. The
  only file agit adds to it is `.agit.toml`. So `agit log`, `agit status`, and `agit commit` behave
  exactly like git.
- **`agit a <git command>`** runs git against the **agent's store**, a separate git repository of session
  transcripts. So `agit a log` shows the agent's sessions and `agit a commit` commits to the store.

`a` is a subcommand, not a flag, so it cannot be transposed: `agit a commit` acts on the store, while
`agit commit -a` is git's stage-all on the code repo. They differ by more than a space.

## Agent

An **agent** is a git repository of session transcripts. It lives at `$AGIT_HOME/agents/<aid>/` (by
default `~/.agit/agents/<aid>/`), separate from your code. Name an agent for what it works on
(`frontend`, `payments-api`), not for a person or a folder. One agent can work across several
repositories, and one repository can host several agents.

## Store

The **store** is the agent's git repository, the transcripts themselves. `agit a <git command>` runs git
against it. `agit a log` and `agit a diff` render a session view of it; most other `agit a` commands are
plain git on the store.

## aid

The **aid** is an agent's stable identity, `agt_<uuid>`, minted once and committed inside the store. The
name and the remote URL are labels that can change; the aid is the identity. Because `.agit.toml` records
the aid, a remote recreated under the same name cannot silently bind you to a different agent, and a
[merge](../cli/merging.md) uses the aid to decide whether two sides are the same agent.

## Binding (`.agit.toml`)

The **binding** is a committed file in your code repository that ties one or more aids to this repo and
records where to clone each agent's store. Commit it so a teammate's fresh clone reads it and pulls the
same agents. See [Sharing](../integration/sharing.md).

## Session

A **session** is one recorded run of an agent: the prompts, replies, tool calls, and edits, as the
runtime dumped them. Sessions are the objects agit versions. A hub renders a session as a readable
conversation; see [Reading a session](../hub/reading-a-session.md).

## Runtime

A **runtime** is the coding tool that produces a session, either **Claude Code** (`claude-code`) or
**Codex** (`codex`). agit can convert a session from one runtime to the other so it resumes in either;
see [Runtimes](../cli/runtimes.md).

## How agit selects an agent

A command that acts on an agent resolves which one in this order:

1. `--agent <name>` on the command
2. `$AGIT_AGENT` in the environment
3. the worktree's active agent, set by `agit a switch <name>`
4. the binding's default in `.agit.toml`

If none of these resolves, the command reports an error instead of guessing.

## Next

- [Quickstart](./quickstart.md): the concepts in a first run.
- [CLI overview](../cli/overview.md): the full command set.
- [Configuration](../cli/configuration.md): `$AGIT_HOME`, `$AGIT_AGENT`, and the other settings.
