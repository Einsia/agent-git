---
sidebar_position: 9
title: Identity and signing keys
---

# Identity and signing keys

Each machine holds one ed25519 signing key. agit signs the sessions this machine captures with it, which
ties a session to the machine that produced it. Enrolling that key with a hub account turns "signed by
this key" into "signed by this person", and the same enrolled key is what key-based hub authentication
uses.

## The machine signing key

The key is an ed25519 keypair, created on first use and stored under `$AGIT_HOME/identity/` (the private
key `ed25519` is `0600`). It is per-machine, not per-agent: one key signs everything this machine
captures. Show it:

```bash
agit identity show          # this machine's ed25519 + x25519 public keys, where stored, enrollment status
agit provenance key         # just the signing public key
```

`agit identity show` also reports the machine's X25519 public key, which is the encryption half used to
seal session content to you as a recipient. See [Encryption](./encryption.md).

## What signing gives you

When agit captures a session it signs it with this key and records the signature in the session's
committed sidecar, alongside the transcript digest, the agent's aid, your committer email, and the start
time. Verification recomputes the digest, rebuilds the signed message, and checks the signature:

```bash
agit provenance verify <session>
```

Self-verify proves the content is intact and the signature matches the recorded key. It does not say who
the key belongs to, which is why the verdict is "verified", never "trusted". An unsigned session reports
"unverified" and never blocks; a present signature that does not check out is a hard, non-zero failure.
The person a session is attributed to is your git committer identity, which agit resolves the way git
does. See [Provenance](../integration/provenance.md) for the full verdict table.

## Register this machine with a hub

`agit identity register` publishes this machine's public keys so a hub account can vouch for them. It runs
offline: it derives the ed25519 and X25519 public halves, self-signs an enroll message, and prints a
paste block. No secret leaves the machine.

```bash
agit identity register you
```

The output is a one-line JSON block plus instructions:

```
{"ed25519_pub":"...","x25519_pub":"...","epoch":...,"enroll_sig":"...","label":"..."}

paste this into the hub: Account -> Signing keys -> Add a signing key
```

Paste the block into the hub web UI to enroll the key under your account. `--label <name>` names the
device; without it agit picks a default. Registering also remembers the hub account locally so the git
credential helper knows which account to authenticate as.

Inspect what is enrolled:

```bash
agit identity show           # this machine's keys and its enrollment status
agit identity show alice     # another account's enrolled device keys, from the hub
agit identity keys           # this machine's key details
agit identity revoke <fpr-or-label>
```

Once your machine's key is enrolled and your committer email maps to your account, `agit provenance
verify` upgrades to `VERIFIED AS <you>`. The hub's signing-keys page is documented at
[Signing keys](../hub/signing-keys.md).

## The same key does key-auth

The enrolled ed25519 key is also your credential for a private hub. `agit a push`, `agit a pull`, `agit a
fetch`, and `agit a clone` wire agit as a git credential helper for hub hosts: the helper mints a
short-lived token from this key by answering a challenge, so you push, pull, and clone a private store
without pasting a token. The key you enrolled here is the key that signs those challenges. See
[Authentication](../integration/authentication.md).
