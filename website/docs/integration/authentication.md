---
sidebar_position: 2
title: Authentication
---

# Authentication

The hub accepts three kinds of credential. Each fits a different caller.

| Credential | For | Grants |
|---|---|---|
| Key-auth | your machines (push, pull, fetch, clone) | short-lived token, minted from an enrolled key |
| Token | scripts, or a machine with no key enrolled | scoped, expiring bearer credential |
| Cookie session | people in the browser | full account, including admin actions |

## Key-auth

agit acts as a git credential helper for your hub. When git needs credentials for a hub host, it runs
`agit credential get`, which performs a challenge, sign, exchange handshake and hands git back a
freshly minted, short-lived token. You copy nothing per push.

The handshake:

1. `GET /api/auth/challenge` returns a single-use nonce.
2. agit signs an assertion binding the nonce, your username, this machine's public key, the hub as
   audience, and a near-future expiry, using the machine's ed25519 key.
3. `POST /api/auth/key` verifies the signature against the account's enrolled keys and mints a bearer
   token that lives minutes, not days.

The token is a normal row in the hub's tokens table, write-scoped and owned by your account, and legible
in the web UI token list. Within one push git may ask twice (info/refs, then receive-pack); agit caches
the token on disk owner-only so a push signs once, not once per request.

### What key-auth is scoped to

agit wires the credential helper for hub hosts only: the host of `AGIT_HUB_URL`, plus the hosts of the
active agent's bound store remotes. github, gitlab, and other remotes fall through to git's own helpers
untouched. The same wiring covers `push`, `pull`, `fetch`, and `clone`.

`clone` is stricter. A clone URL is untrusted input, so agit auto-mints for it only when the URL's host
is already a declared hub for this machine. An arbitrary https URL, or github, or gitlab, yields no
signed challenge, so the account username and public key are never posted to a host you have not already
trusted. Declare the hub first, with `AGIT_HUB_URL` or a prior push.

The signed assertion names the hub as its audience, which binds it to one hub and stops a signature from
being replayed against another. On the server side the operator pins that audience with
`AGIT_HUB_PUBLIC_URL`; see [self-hosting configuration](../self-hosting/configuration.md).

The helper is deliberately forgiving. For any non-hub host, an unknown account, or any error in the
handshake, it prints nothing and exits 0, so git falls back to its normal Basic prompt. A push never
hard-fails because key-auth was unavailable.

Enroll a machine's key with [`agit identity register`](../cli/identity.md) and paste the block into the
web UI. See [connecting the CLI](./connect-cli-to-hub.md) for the first-connect flow and
[signing keys](../hub/signing-keys.md) for managing enrolled keys.

## Tokens

A token is a scoped, expiring bearer credential you create in the web UI. Use one from a script, or on a
machine where no key is enrolled. Scope it to a single agent, give it a time limit, revoke it at any
time. A token is a ceiling on permission, never a source of it: a read-only token still only reads.

Supply a token through git's password prompt, in the store remote URL's password field, or through the
environment.

| Variable | Meaning |
|---|---|
| `AGIT_HUB_URL` | the hub's address; also declares the host so key-auth and clone auto-mint recognize it |
| `AGIT_HUB_TOKEN` | a bearer token; overrides any credential parsed from the remote URL |
| `AGIT_HUB_USER` | the account to authenticate as; overrides the enrolled account for the credential helper |

See [tokens](../hub/tokens.md) for creating and revoking them.

## Cookie sessions

People sign in through the browser. A password login returns a session cookie that expires and is revoked
by logging out. Admin actions, changing an agent's visibility, managing members, renaming, deleting,
require a login. They are never available to a token, even an admin's. If your hub enables self-service
registration you create your own account; otherwise an administrator creates it. See
[accounts](../hub/accounts.md).
