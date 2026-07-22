# Key-based hub auth via a git credential helper

Date: 2026-07-22
Status: accepted

## Problem

To push an agent store to a hub today, a user creates a token in the hub web UI and hands it to git
(URL userinfo, git's password prompt, or `AGIT_HUB_TOKEN`). There is no `agit login`. The token is a
copy-paste secret with a manual lifecycle.

The user already enrolls an ed25519 key with the hub (`agit identity register <you>`), used today only
to attribute provenance (`VERIFIED AS`). That same key should authenticate pushes, the way GitHub lets an
enrolled SSH key push with no token. This document specifies key-based auth as the first-class path;
tokens remain as a fallback.

## Constraint

GitHub key-auth rides SSH transport (`git@github.com:...`). The hub has no SSH daemon: it is git-smart-http
behind nginx. So the mechanism must be the HTTPS analog, not SSH.

## What already exists

- Client private key: `$AGIT_HOME/identity/` ed25519, `agent::machine_signing_key()`. Already signs
  provenance; `agent::sign_hex` is the signer.
- Server public key: the `identity_keys` table, composite `(username, key_fpr)`,
  `store::ed25519_fingerprint()` deterministic. Enrolled while logged into the web UI. One account, many
  device keys.
- Push transport: git-smart-http passthrough; hub auth is `credentials()` -> `auth::authenticate()`
  (cookie session, or a token via Basic/Bearer). A token resolves to `user_caller(&u, Some(grant))` — the
  token IS the account, capped by scope.

Only the handshake that turns "I hold the enrolled private key" into "authenticated as that account" is
missing.

## Design

`agit` becomes a git credential helper for hub hosts. When git needs credentials for a hub URL, `agit`
signs a server challenge with the enrolled key, exchanges the signature for a short-lived bearer token, and
hands that token back to git. The git-smart-http auth path is unchanged — the token is auto-minted from the
key instead of copy-pasted.

### New hub endpoints

- `GET /api/auth/challenge` -> `{ "nonce": "<random>", "expires_at": <unix> }`. The nonce is single-use
  and short-lived (e.g. 60s), tracked server-side until consumed or expired.
- `POST /api/auth/key` with `{ "username", "ed25519_pub", "nonce", "audience", "expiry", "sig" }`.
  The hub:
  1. rejects an unknown/expired/already-consumed nonce;
  2. rejects a mismatched `audience` (must equal this hub's canonical URL) or an `expiry` in the past;
  3. looks up `(username, ed25519_fingerprint(ed25519_pub))` in `identity_keys`; a missing or revoked row
     is a 401;
  4. verifies `sig` over the canonical assertion bytes against `ed25519_pub`;
  5. on success mints a short-lived bearer token (minutes) scoped to that account and returns
     `{ "token", "expires_at" }`.

The signed assertion is a fixed canonical byte string over `{audience, username, ed25519_pub, nonce,
expiry}` — a domain-separated message, same construction style as `agent::identity_enroll_message`, so an
enroll signature can never be replayed as an auth signature and vice-versa.

### Token minting: reuse the tokens table, no schema migration

The minted token is a normal row in the existing `tokens` table with a short expiry and an owner, so
`auth::authenticate()` accepts it with zero change. No new column, no legacy-shape migration — this keeps
the change off the redeploy-risk path (schema migrations against the live Postgres are the known
crash-loop risk). The token carries a marker in its note/scope so it is legible in the web UI as
device-key-minted and auto-expiring.

### Client side

- `agit credential <get|store|erase>` implements git's credential-helper protocol (reads
  `protocol`/`host`/`path` key=value lines on stdin, writes `username`/`password` on stdout). `get` runs the
  challenge -> sign -> exchange flow and returns the minted token as the password; `store`/`erase` are
  no-ops (the token is ephemeral, git may cache it via its own cache helper). Only hub hosts are handled;
  for any other host it prints nothing so git falls through to its normal helpers.
- Wiring: when `agit a push` / passthrough targets a hub remote, `agit` invokes git with
  `-c credential.https://<hubhost>.helper=agit credential` scoped to the hub host, so github/gitlab remotes
  are never touched. The hub host is the one `agit` already resolves for the bound remote / `AGIT_HUB_URL`.
- Username: persist `<you>` at `agit identity register` time to `$AGIT_HOME/identity/hub-account` (plus an
  `AGIT_HUB_USER` override), so the helper is zero-config after the one-time enroll. If the file is absent
  and the env is unset, the helper falls back to git's normal Basic prompt rather than failing the push.
- Token cache: within one push git may make several requests; `agit` caches the minted token in memory for
  the process and, optionally, briefly on disk (0600 under `$AGIT_HOME`) keyed by host, so it signs once
  per push, not once per request.

## Trust model

- A key under account X could only have been enrolled by someone logged into X's web session (Account ->
  Signing keys). That enroll-time login is the trust root, exactly like adding an SSH key on github.com.
- Replay safety: the nonce is single-use and short-lived; the assertion binds `audience` (this hub) and
  `expiry`, so a captured signature cannot be reused here after expiry, nor replayed to another hub.
- Blast radius: the minted token lives for minutes and is scoped to the account; a leak is a short window,
  not a standing secret. Admin actions still require a cookie login — a token, however minted, can never
  perform them.
- Revocation: revoking the enrolled key (web UI) stops new auth immediately; already-minted tokens expire
  on their own short TTL and are revocable in the tokens list.

## Bootstrap

First enrollment still needs one web login to paste the `agit identity register` block — the same one-time
step as adding an SSH key on github.com. After that, key-auth needs no token and no prompt. A device-code
`agit login` that removes even the one paste is possible later and is out of scope here.

## Edge cases

- Key not enrolled / wrong hub: `POST /api/auth/key` 401s; the helper returns nothing and git falls back
  to its normal Basic prompt, so a push never hard-fails because of this feature.
- Same key enrolled under two accounts: the assertion names the `username`, so `(username, fpr)` is
  unambiguous; the persisted `hub-account` (or `AGIT_HUB_USER`) selects which.
- Clock skew: `expiry` is client-set with a small window; the nonce TTL is the server-side bound, so a
  client with a fast clock still cannot outlive the nonce.

## Testing

- Hub: a valid signature over a live nonce mints a usable token; an expired nonce, a reused nonce, a wrong
  `audience`, a past `expiry`, an unenrolled fingerprint, a revoked key, and a signature by a different key
  are each rejected. A minted token authenticates a subsequent request as the account.
- Client: `agit credential get` on a hub host emits `password=<token>`; on a non-hub host emits nothing.
  The assertion bytes are domain-separated from the enroll message (a cross-replay test).
- The existing token and cookie auth paths are unchanged (regression).

## Out of scope

- Device-code `agit login` (removes the one-time web paste).
- SSH transport.
- Reusing the key for content encryption (that is the separate x25519 keybox).
