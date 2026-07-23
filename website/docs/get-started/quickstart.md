---
sidebar_position: 3
title: Quickstart
---

# Quickstart

From install to a resumed session, then optionally onto a hub. Run it once inside a code repository. Each
step links to its deep-dive page.

This assumes `agit` and a runtime are installed and your git identity is set. If not, see
[Install](./install.md).

## 1. Set up the repo

Run this inside your code repository:

```bash
agit init --agent frontend
```

This creates an agent named `frontend`, writes a `.agit.toml` binding it to the repository, and installs
a secret-scanning hook on the agent's store. Commit `.agit.toml` so teammates get the agent on clone.
Name the agent for what it works on (`frontend`, `payments-api`), not for you or the folder. See
[Concepts](./concepts.md) for what an agent and a binding are.

## 2. Capture a session

Turn on hands-off capture, then work:

```bash
agit watch --daemon
```

`agit watch` starts a background process that records new sessions into the store as you work, and
converts each one between Claude Code and Codex so it can resume in either. Set it once and leave it
running. Manage it with `agit watch --status` and `agit watch --stop`. To record by hand instead of in
the background, use `agit a snap`. See [Capturing](../cli/capturing.md).

Now run a session:

```bash
agit start
```

`agit start` launches a session carrying the agent's context. Work as you normally would; the daemon
records it.

## 3. Confirm it was recorded

```bash
agit a log
```

This lists the agent's sessions, most recent first: the runtime, when and where it ran, its opening
prompt, and its tool activity. Yours is at the top. When sessions have diverged, `agit a log` draws them
as a tree; see [Divergence](../cli/divergence.md).

## 4. Resume it

```bash
agit resume
```

This loads the agent's latest session back into a runtime and continues it, context intact. Add
`--as codex` or `--as claude-code` to resume in the other runtime. See [Resuming](../cli/resuming.md).

## 5. Publish to a hub (optional)

To share the agent with your team, register with a hub, then push:

```bash
agit identity register you
agit a push
```

`agit identity register` enrolls this machine's signing key with the hub so pushes and pulls authenticate
without a password; see [Connect the CLI to a hub](../integration/connect-cli-to-hub.md). `agit a push`
records the store's remote in the binding, so a teammate's clone finds the agent. They pull it with
`agit a clone`; see [Sharing](../integration/sharing.md).

## 6. Browse it

Open the agent on the hub in your browser to read a session as a conversation, not raw JSON. See
[Reading a session](../hub/reading-a-session.md).

## Next

- [Concepts](./concepts.md): the two scopes, the aid, and what a session is.
- [CLI overview](../cli/overview.md): every command.
- [Merging](../cli/merging.md): reconcile two people's diverged sessions.
