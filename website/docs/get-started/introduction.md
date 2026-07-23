---
slug: /
sidebar_position: 1
title: Introduction
---

# agit

agit version-controls the raw session transcripts your AI coding agent produces. When you work with
Claude Code or Codex, each session is what the agent read, ran, and changed, together with the prompts
and replies that got there. agit saves that transcript into a git repository, so a session is versioned,
resumed, shared, and reconciled the same way your code is.

Today an agent session is throwaway. The diff survives in your code history; the reasoning that produced
it does not. agit keeps the reasoning, not just the diff. From a saved session you can resume the agent
with its full context loaded, hand the agent and its history to a teammate, and reconcile two people's
runs that diverged from the same starting point.

## Two surfaces

agit has two surfaces you use at different times.

**The CLI (`agit`)** runs on your machine. It records sessions into a local git repository (the agent's
store), resumes them, moves a session between Claude Code and Codex, and pushes to and pulls from a
remote. You run `agit` everywhere you run `git`. `agit <git command>` runs git against your code
repository, unchanged. Put `a` after `agit` and the same command runs against the agent's store, so
`agit log` shows your code history and `agit a log` shows the agent's sessions. See
[CLI overview](../cli/overview.md).

**The hub (`agit-hub`)** is a server your team pushes to. It stores agents, authenticates pushes and
pulls with signing keys, and gives you a web UI to browse an agent's sessions and read a transcript as a
conversation instead of raw JSON. Use it when more than one person works on the same agent, or when you
want to read a session in a browser. See [hub overview](../hub/overview.md). You can use a [hosted hub](https://agit.anggita.org) or
[run your own](../self-hosting/deploying.md).

## What agit solves

- **Sessions are disposable.** Close the terminal and the agent's context is gone. agit records each
  session so you can [resume](../cli/resuming.md) it later with its context intact.
- **Only the diff is shared.** A teammate sees what changed, not why the agent changed it. Push an agent
  to a hub and they [clone](../integration/sharing.md) the sessions, prompts, and tool activity.
- **Parallel runs diverge.** Two people resume the same agent and each produce new sessions. agit tracks
  the [divergence](../cli/divergence.md) and reconciles the two sides by
  [dialogue merge](../cli/merging.md).

## Where to go next

| You want to | Read |
|---|---|
| Install the CLI and the runtime prerequisites | [Install](./install.md) |
| Record, resume, and publish your first session | [Quickstart](./quickstart.md) |
| Learn the vocabulary (agent, aid, store, session) | [Concepts](./concepts.md) |
| See every CLI command | [CLI overview](../cli/overview.md) |
| Browse and read sessions in a web UI | [Hub overview](../hub/overview.md) |
| Point the CLI at a hub | [Connect the CLI to a hub](../integration/connect-cli-to-hub.md) |
