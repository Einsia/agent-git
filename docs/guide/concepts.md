---
title: Concepts
nav_order: 14
---

# Concepts

Plain definitions of the terms agit uses. Consult this when a guide uses a word you want pinned down.

## Agent

A git repository of session transcripts. It lives at `~/.agit/agents/<aid>/` (under `$AGIT_HOME`),
separate from your code. Name an agent for what it works on (`frontend`, `payments-api`), not for a person
or a folder. One agent can work across several repositories, and one repository can host several agents.

## aid

An agent's stable identity, `agt_<uuid>`, minted once and committed inside the store in `agent.toml`. The
name and the remote URL are labels that can change; the aid is the identity. Because `.agit.toml` records
the aid, a remote recreated under the same name cannot silently bind you to a different agent, and a merge
uses the aid to decide whether two sides are the same agent (see
[Reconcile diverged sessions](merging.html)).

## Store

The agent's git repository (the transcripts themselves). `agit a <git command>` runs git against the
store. `agit a log` and `agit a diff` render a session view of it; most other `agit a` commands are plain
git.

## Environment

Your code repository. `agit <git command>` runs git against it, unchanged. The only file agit adds is
`.agit.toml`.

## Binding (`.agit.toml`)

A committed file in your code repository that declares which agents the repository uses and where to clone
them. A teammate's fresh clone reads it to pull the same agents. See
[Share an agent with your team](sharing.html).

## Session

One recorded run of an agent: the prompts, replies, tool calls, and edits, as the runtime dumped them.
Sessions are the objects agit versions.

## Runtime

The coding tool that produces a session, either Claude Code or Codex. See
[Move a session between runtimes](runtimes.html).

## How agit selects an agent

A command that acts on an agent resolves which one in this order:

1. `--agent <name>` on the command
2. `$AGIT_AGENT` in the environment
3. the worktree's active agent, set by `agit a switch <name>`
4. the binding's default in `.agit.toml`

If none of these resolves, the command reports an error instead of guessing.
