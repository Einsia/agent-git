---
title: Home
nav_order: 1
---

English | [中文](zh/index.html)

# agit

agit saves each coding session your AI agent produces (what the agent read, ran, and changed) into a
git repository, so the session is versioned, shared, and reconciled the same way your code is. It works
with Claude Code and Codex.

`agit <git command>` runs git against your code repository, unchanged. Put `a` after `agit` and the same
command runs against the agent's store, a separate git repository of session transcripts. So `agit log`
shows your code history and `agit a log` shows the agent's sessions.

## What do you want to do?

| Goal | Guide |
|---|---|
| Install agit and record your first session | [Get started](guide/quickstart.html) |
| Record sessions automatically as you work | [Capture agent sessions](guide/capture.html) |
| Continue a session with its context loaded | [Resume a session](guide/resume.html) |
| Move a session between Claude Code and Codex | [Move a session between runtimes](guide/runtimes.html) |
| Give a teammate an agent and its history | [Share an agent with your team](guide/sharing.html) |
| Combine two people's diverged sessions | [Reconcile diverged sessions](guide/merging.html) |
| Stop a secret from reaching shared history | [Keep secrets out of shared history](guide/secrets.html) |
| Confirm which person produced a session | [Verify who produced a session](guide/provenance.html) |
| Browse agents and sessions in a web UI | [Browse agents on the hub](hub.html) |
| Run a hub for your team | [Self-host the hub](deploying-the-hub.html) |
| Point an agent at a recreated remote or fork | [Rebind an agent's identity](guide/migration.html) |

## Reference

- [Command reference](guide/command-reference.html): every command, one line each.
- [Configuration](guide/configuration.html): environment variables and files.
- [Concepts](guide/concepts.html): the vocabulary (agent, aid, store, environment).

## Install

```
npm install -g @einsia/agentgit
```

See [Get started](guide/quickstart.html) for the first run.
