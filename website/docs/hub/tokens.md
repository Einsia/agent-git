---
sidebar_position: 6
title: Tokens
---

# Tokens

A token is a credential for git and scripts. Where a person signs in with a password and holds a session
cookie, an automated client sends a token. Create one in the web UI, use it as a git password or bearer
header, and revoke it when you are done.

A device key can mint tokens automatically, so you may never need to create one by hand; see
[Authentication](../integration/authentication.md). Create a token by hand when a script or CI job needs a
long-lived, narrowly scoped credential.

## A token is a permission ceiling, never a source

A token can only ever narrow what its owner can already do. A read-only token still only reads; a token
scoped to one agent cannot touch another. Two rules follow:

- **Admin actions require a login.** Issuing a token, managing a roster, transferring or deleting an org:
  these take your own login session and are refused when presented with a token, even an admin's write
  token. A token can never mint another token, so one leaked token cannot spawn a standing foothold.
- **A token adds no authority.** Handing someone a token grants them only the subset of your access the
  token's scope allows, for as long as it lives.

## Create a token

From the Account page, create a token with:

- a **name**, so you can recognize it in the list later,
- a **scope**, either read or write,
- an optional **agent** binding, `owner/name`, to restrict the token to a single agent, and
- an optional **expiry** in days.

The hub shows the token string once, at creation. It stores only a sha256 digest, which cannot be turned
back into the token, so copy it now. A write-scoped token bound to an agent you can only read is refused at
creation rather than issued to fail on the first push.

The equivalent admin CLI is `agit-hub token add <name> [--user <owner>] [--agent <owner>/<name>]
[--read|--write] [--ttl-days N]`, which likewise prints the token once.

## Use a token

Point a remote at the agent's `.git` URL and put the token in git's password field (the username can be
anything):

```bash
agit a remote add origin https://hub.example.com/alice/frontend.git
agit a push -u origin main
#   username: anything
#   password: the token
```

A script can send it as a bearer token instead:

```bash
curl -H "Authorization: Bearer $AGIT_TOKEN" https://hub.example.com/api/agents
```

For the full client setup, see [Connect the CLI to a hub](../integration/connect-cli-to-hub.md).

## List and revoke

The token list on the Account page shows each token's name, scope, agent binding, creation time, expiry,
and last-used time, but never the secret. You see your own tokens; a site admin sees all of them. Revoke a
token to disable it immediately. Old ownerless tokens are shown as unusable rather than silently working.

The admin CLI mirrors this with `agit-hub token list` and `agit-hub token rm <id>`.

## Related

- [Accounts](./accounts.md): the login session that issuing a token requires.
- [Signing keys](./signing-keys.md): enroll a device key that auto-mints short-lived tokens.
- [Authentication](../integration/authentication.md): key-based auth on push, pull, fetch, and clone.
