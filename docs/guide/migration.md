---
title: Migration
nav_order: 6
---

# Migration

## Rebind

`agit a rebind` overrides the binding's integrity check, and re-mints an agent's identity. It has two
forms.

`--remote <url>` corrects the binding when the store an agent resolves to holds a different aid than
the binding records, which happens when a remote is recreated with a fresh identity, or when DNS
changes what a name resolves to. Resolution refuses this case by default; rebind is the explicit
acceptance of it:

```
agit a rebind frontend --remote https://hub.example.com/frontend.git
```

The binding entry is rewritten to the identity the store actually holds, and the store's origin is set
to the given URL. As with publishing, any credential in the URL is kept out of the committed binding.

`--new-id` gives a store a fresh identity:

```
agit a rebind --new-id
```

This is used after forking an agent. A clone of a fork carries the source's aid; `--new-id` mints a new
aid for it, so it becomes an independent agent rather than a second claimant on the source's identity.
Re-minting moves the store (it is keyed by aid), so it is refused while a watcher is running against
it, and it reports that other repositories bound to the old aid must re-track the fork.

## Import

`agit a import` adopts a store created by a version of agit that kept the store inside the code
repository, at `<env>/.agit/agent`. Those stores no longer resolve. Import mints an identity for the
store, moves it into `$AGIT_HOME`, writes the binding, and preserves its history:

```
agit watch --stop     # if a watcher is running
agit a import
```

Import moves the store rather than copying it, so it is refused while a watcher is running against the
old location; stop the watcher first. Every agent-scoped command run against a legacy store reports the
same instruction to import it, so the message appears regardless of which command is run.
