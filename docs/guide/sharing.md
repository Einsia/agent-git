---
title: Share an agent with your team
nav_order: 6
---

# Share an agent with your team

Sharing an agent is git-native: you add a remote and push. After this, a teammate can clone the agent
and open a session that already carries its context. You can push to any git host that serves the store;
for a web UI and per-agent permissions, run [the hub](../hub.html).

## Push the agent to a remote

```
agit a remote add origin https://hub.example.com/frontend.git
agit a push -u origin HEAD
git add .agit.toml && git commit -m "declare the frontend agent"
```

`agit a push` runs a real git push on the store, and records the store's remote into `.agit.toml` as it
pushes. Any credential in the URL is stripped before it reaches that file; the full URL stays in the
store's local git config. Commit `.agit.toml` so teammates get the agent and know where to clone it.

After the remote is bound, later pushes are a bare `agit a push`, which sends the current branch and only
new sessions. With more than one remote bound, a bare `agit a push` pushes to every one. Push to a single
named remote with `agit a push --to <name>`.

If a hub rejects the push for authentication, agit points you at its token page.

## Set up a teammate

A teammate who has cloned the code repository already has the `.agit.toml` binding. One command sets them
up:

```
agit init            # clone every agent .agit.toml declares
agit start           # open a session carrying the agent's context
```

To clone a single agent instead of all of them:

```
agit a clone frontend
```

A bare name resolves through `.agit.toml`. Either way it is the same agent, carrying the same identity
(its aid), not a copy. Bring its tools over with `agit harness apply` (see
[Resume a session](resume.html)).

## Pull in a teammate's work

```
agit a pull
```

This fast-forwards when the histories allow it. When the two sides have diverged, it stops and routes you
to `agit a merge`. See [Reconcile diverged sessions](merging.html).

## What travels with the agent

`.agit.toml` in your code repository records which agents the repository uses and where to clone them.
It is the one file agit adds to your code repository, and it is committed. The agent's identity (its aid)
travels inside the store, so a remote recreated under the same name cannot silently bind you to a
different agent. See [Concepts](concepts.html).

Because a teammate wrote `.agit.toml`, agit treats a remote it declares as untrusted. Before cloning a
remote from `.agit.toml`, agit checks it against a transport allowlist, because a URL like `ext::<cmd>`
would otherwise have git execute `<cmd>`.
