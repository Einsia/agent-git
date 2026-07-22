---
title: Rebind an agent's identity
nav_order: 15
---

# Rebind an agent's identity

An agent is keyed by its aid, and agit refuses to connect a binding to a store whose aid does not match
what `.agit.toml` records (see [Concepts](concepts.html)). That check stops a recreated remote from
silently swapping in a different agent under the same name. When you actually mean to change the mapping,
`agit a rebind` overrides it. There are two forms.

## Point a binding at a recreated remote

Use `--remote` when the store a name resolves to holds a different aid than the binding records, such as a
remote recreated with a fresh identity, or DNS pointing the name somewhere new. Resolution refuses that by
default; rebind is how you accept it:

```
agit a rebind frontend --remote https://hub.example.com/frontend.git
```

The binding entry is rewritten to the aid the store actually holds, and the store's origin is set to the
URL. As with `agit a push`, any credential in the URL is kept out of the committed `.agit.toml`.

## Give a fork its own identity

A clone of a fork carries the source's aid, so it reads as the same agent (a second claimant on one
identity). `--new-id` mints a fresh aid, making the fork an independent agent:

```
agit a rebind --new-id
```

Re-minting moves the store, because the store is keyed by aid. Two consequences follow:

- It is refused while a watcher is running against the agent. Stop the watcher first with
  `agit watch --stop`.
- Other repositories bound to the old aid do not follow the fork. Each must run `agit a clone` on the fork
  again to pick it up.
