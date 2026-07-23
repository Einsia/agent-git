---
sidebar_position: 1
title: Connect the CLI to a hub
---

# Connect the CLI to a hub

The hub is a server your team runs to host agents and browse them in a web UI. You reach it through the
ordinary client commands: there are no hub-specific verbs. This page covers connecting a machine to a hub
once, so `agit a push`, `agit a pull`, and `agit a clone` authenticate on their own.

Two credential paths exist. Enroll a signing key once and every push and pull authenticates with no token
to copy. Where no key is enrolled, or in a script, a token is the fallback. Prefer the key path.

## Key path: enroll once, push forever

On each machine you push from, register that machine's public key with your hub account.

1. Print this machine's enroll block:

   ```bash
   agit identity register you
   ```

   Replace `you` with your hub username. The command is offline: it derives this machine's public keys,
   self-signs an enroll block, and prints it. Nothing leaves the machine. Your private key never leaves
   `$AGIT_HOME/identity`.

2. Copy the printed block. In the hub web UI, open Account, then Signing keys, and paste it. The hub
   verifies the self-signature and enrolls the key under your account.

3. Confirm the machine's state:

   ```bash
   agit identity show
   ```

After the key is enrolled, `agit a push` (and `pull`, `fetch`, `clone`) authenticate by signing a
server challenge with the enrolled key and exchanging it for a short-lived token. You paste nothing per
push. agit acts as a git credential helper for your hub's host only; github, gitlab, and other remotes
are never touched. See [authentication](./authentication.md) for the full model, and
[signing keys](../hub/signing-keys.md) for managing enrolled keys.

:::note
Enroll each machine you push from. A key is per machine, so a laptop and a CI runner each register their
own. Revoke one in the web UI without disturbing the others.
:::

## Naming the hub

Key-auth fires only for a host agit already knows as a hub: the host of `AGIT_HUB_URL`, or the host of a
store remote the active agent is bound to. Once you have pushed an agent to the hub, its bound remote
makes the host known. Before the first push, set `AGIT_HUB_URL`:

```bash
export AGIT_HUB_URL=https://agit.anggita.org
```

A `git clone` of a hub store auto-mints a token only when the URL's host is already a declared hub for
this machine. An arbitrary URL never triggers a signed challenge. This is why the clone path requires the
host to be known first.

## Token path: the fallback

Use a token where no key is enrolled, or from a script that should not carry a personal key.

1. In the hub web UI, create a token. Scope it to a single agent, give it a time limit, and it can be
   revoked at any time. See [tokens](../hub/tokens.md).

2. Supply it one of three ways:

   - Type it into git's password prompt when a push or pull asks. Your username is the account name.
   - Put it in the store remote URL's password field.
   - Export it:

     ```bash
     export AGIT_HUB_URL=https://agit.anggita.org
     export AGIT_HUB_TOKEN=<token>
     export AGIT_HUB_USER=you
     ```

`AGIT_HUB_TOKEN` overrides any credential parsed from the remote URL. A token is a ceiling on
permission, never a source of it: a read-only token still only reads, and admin actions in the web UI
require a login, never a token.

## Verify the connection

```bash
agit a push
```

If the hub rejects the push, it names the account it authenticated as and what it lacks, so a wrong
token, a missing grant, and a read-only scope are easy to tell apart. To publish an agent for the first
time and record its origin, see [sharing](./sharing.md).
