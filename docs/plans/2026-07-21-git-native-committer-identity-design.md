# Git-native committer identity (make `VERIFIED AS` reachable)

Date: 2026-07-21
Status: approved (user chose "make it like git where the user sets their email themselves")

## Problem

`scaffold_store` sets a **local** `user.email = agit@local` on every agent store
(`src/agent.rs:872`). That local value shadows the user's real git identity, so:

- Every session commit is authored by `agit@local`.
- `committer_email()` therefore returns `agit@local`, which maps to no hub account.
- Provenance attribution can never leave `SignedUnregistered`: `agit a provenance`
  can not print `VERIFIED AS <user>` for anyone, end to end.

The signing key half already works (`machine_signing_key`, `identity register` paste
flow, `classify` → `VerifiedAs`/`KeyMismatch`/`SignedUnregistered`). Only the committer
email — the lookup handle that bridges a signature to a hub account — is broken, because
agit hardcodes it.

## Decision

Make the committer identity **git-native**: the user sets their own email (exactly like
`git config user.email`), and agit stops injecting `agit@local`. agit's own bookkeeping
commits keep an explicit `agit` identity so they never depend on — or get attributed to —
the user.

This is safe: the committer email is only a *lookup handle*. `classify` upgrades to
`VerifiedAs` only when the email maps to an account **and** the signing key matches one of
its registered keys. A wrong/spoofed email degrades to `SignedUnregistered` or
`KeyMismatch`, never a false `VERIFIED AS`. The key signature is the real boundary.

## Changes

1. **`scaffold_store` (`src/agent.rs`)** — stop persisting `user.email = agit@local`
   (and `user.name = agit`) as store-local config. The store then inherits the user's git
   identity the way any git repo does (local → global resolution).

2. **agit's metadata commits** — mint (`agent.rs:877`), rename (`agent.rs:1387`), adopt
   (`agent.rs:1551`), and any other agit-authored bookkeeping commit passes an explicit
   `-c user.name=agit -c user.email=agit@local` per invocation. So these commits:
   - stay labeled as agit's own bookkeeping regardless of the user's identity, and
   - **never fail** even on a brand-new machine with no git identity at all (agent
     creation must always work).

3. **`committer_email()` (`src/commands.rs`)** — drop the `agit@local` fallback. Return the
   git-resolved `user.email` (local → global), or empty when nothing is configured.

4. **Snap / merge-auto-snap gate (`gated_commit`, `src/session.rs`)** — before a session
   commit, resolve the committer email; when it is empty, refuse **git-style** and do NOT
   commit:

   ```
   agit: your committer identity is unset, so a session can't be attributed.
     set it like git:  git config --global user.email you@example.com
                       git config --global user.name  "Your Name"
     (or per agent:    agit a config user.email you@example.com)
   this email is your provenance identity; register your device key with
   `agit identity register <you>` so `agit a provenance` can verify it.
   ```

   `agit a config user.email ...` already works — `agit a <git-args>` passes through to the
   store's git — so no new command is needed.

5. **Test harness (`session.rs::testenv::with`)** — write an **isolated** git identity into
   the temp `$HOME/.gitconfig` (e.g. `tester@agit.test`), so stores created under a test
   HOME inherit a resolvable email. This fixes every `testenv`-based snap test at once and
   honors the "no global git config in tests" rule (it is the test's own isolated HOME,
   never the developer's real `~/.gitconfig`). The real-binary e2e harness already writes
   `$ISO/.gitconfig`, so it is unaffected.

## Proof obligations

- **Unit**: a store created under a HOME with `user.email = x@y` snaps a session whose
  provenance record carries `email == x@y` (not `agit@local`); feeding that record to
  `classify` with a `RegisteredIdentity{ email x@y, keys ∋ signing-key }` returns
  `VerifiedAs` — the previously-unreachable state, now reachable.
- **Gate**: a store under a HOME with **no** git identity refuses to snap with the message
  above and creates **no** commit; a mint/rename still succeeds (explicit `-c` identity).
- **Metadata stays agit**: the mint commit's author email is `agit@local` even when the
  user's git identity is `x@y`.
- **Regression**: full `cargo test` (lib + cli + hub), `clippy -D warnings`, e2e QA green.
- **Old-vs-new (real binary)**: fresh agent + a set user email → `snap` → the session's
  provenance email is the user's, and (against a matching registered key) provenance
  verifies as that person; before this change the same flow reports `agit@local` /
  unregistered.

## Docs

Update the provenance/identity guide + README to describe: set your git identity → snap →
`agit identity register <you>` (paste to hub) → `agit a provenance` shows `VERIFIED AS`.
No em dashes in prose.
