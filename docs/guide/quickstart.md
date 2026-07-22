---
title: Get started
nav_order: 2
---

# Get started

This guide takes you from install to your first recorded session in about five minutes. You run it once
inside a code repository. After this, agit records every session that agent produces.

## 1. Install agit

```
npm install -g @einsia/agentgit
```

This installs the `agit` client. Confirm it is on your path:

```
agit --version
```

## 2. Set your git identity

agit attributes each recorded session to your git identity. It does not invent one, and it refuses to
record a session while your identity is unset. Set it once, the same as any git repository:

```
git config --global user.email you@example.com
git config --global user.name  "Your Name"
```

## 3. Create an agent for the repo

Run this inside your code repository:

```
agit init --agent frontend
```

This creates an agent named `frontend`, writes a committed `.agit.toml` binding it to the repository,
and installs a secret-scanning hook on the agent's store. Name the agent for what it works on
(`frontend`, `payments-api`), not for you or the folder.

Run `agit init` once per repository. To add another agent to a repository you have already set up, use
`agit a init <name>`.

## 4. Turn on automatic capture

```
agit watch --daemon
```

This starts a background process that records new sessions into the agent's store as you work, and
converts each one between Claude Code and Codex so a session recorded in either tool can resume in the
other. Set it once and leave it running.

```
agit watch --status    # show whether it is running and what it has captured
agit watch --stop      # stop it
```

## 5. Work

```
agit start
```

This launches a session with the agent's context already loaded. Work as you normally would. The daemon
records the session as you go.

## 6. Check that the session was recorded

```
agit a log
```

This lists the agent's sessions, most recent first: the runtime, when it ran, where it ran, its opening
prompt, and its tool activity. Your session appears at the top. When sessions have diverged, `agit a
log` draws them as a tree.

## Next steps

- [Capture agent sessions](capture.html): what agit records, and capturing by hand with `agit snap`.
- [Share an agent with your team](sharing.html): push the agent to a remote so teammates can clone it.
- [Reconcile diverged sessions](merging.html): combine two people's work on the same agent.
- [Command reference](command-reference.html): every command.
