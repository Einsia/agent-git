---
sidebar_position: 3
title: Publishing and retrieving agents
---

# Publishing and retrieving agents

Sharing an agent is git-native: push its store to the hub, and a teammate clones it and opens a session
that already carries its context. The hub hosts each agent as a git repository. These commands work
against any git host that serves the store; the hub adds a web UI and per-agent permissions.

Key-auth covers every command here for a declared hub, so no token is typed once a machine's key is
enrolled. See [authentication](./authentication.md).

## Publish an agent

Bind a remote, then push:

```bash
agit a remote add origin https://agit.anggita.org/frontend.git
agit a push -u origin HEAD
git add .agit.toml && git commit -m "declare the frontend agent"
```

`agit a push` runs a real git push on the store, and on success records the store's origin into
`.agit.toml`, the one file agit adds to your code repository. Any credential in the URL is stripped
before it reaches that file; the full URL stays in the store's local git config. Commit `.agit.toml` so
teammates get the agent and know where to clone it.

After the remote is bound, later pushes are a bare `agit a push`, which sends the current branch and only
new sessions. With more than one remote bound, a bare `agit a push` pushes to every one; push to a single
named remote with `agit a push --to <name>`.

## Retrieve an agent

A teammate who cloned the code repository already has the `.agit.toml` binding. One command clones every
agent it declares:

```bash
agit init
agit start
```

To clone a single agent:

```bash
agit a clone frontend
```

A bare name resolves through `.agit.toml`; a URL clones that store and adopts its identity. `agit a clone`
is a smart clone: where a raw `git clone` of a store URL makes a nested repo that resolves to no agent,
this adopts the store as an agent, carrying the same identity (its aid), not a copy. `agit clone` in the
default scope detects a hub store URL and does the same, printing a one-line note; `--git` forces a raw
clone.

Because a teammate wrote `.agit.toml`, agit treats a remote it declares as untrusted and checks it
against a transport allowlist before cloning, so a URL like `ext::<cmd>` cannot make git execute a
command.

## Pull a teammate's work

```bash
agit a pull
```

This fast-forwards when the histories allow it. `agit a fetch` moves the remote-tracking refs and reports
which sessions arrived without integrating them. When the two sides have diverged, pull refuses rather
than let git textually merge the transcripts, and routes you to `agit a merge`. See
[divergence](../cli/divergence.md) and [merging](../cli/merging.md).

## Public and private stores

Every agent has a visibility, private by default, and members granted read, write, or admin. A private
agent is indistinguishable from one that does not exist, so agent names cannot be enumerated. A public
store is readable by anyone, including anonymous clones; writing still requires a grant. What a push or
pull is allowed to do follows your grant on that agent, not the credential itself. See
[repositories](../hub/repositories.md) and [organizations](../hub/organizations.md).
