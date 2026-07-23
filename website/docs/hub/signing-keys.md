---
sidebar_position: 7
title: Signing keys
---

# Signing keys

A signing key is the device key that attributes your sessions to you. Each machine holds an ed25519 key
whose private half never leaves it; the hub stores only the public half. Once your key is enrolled and your
email is verified, a session you signed reads as `verified as you` on the hub. This is the HTTPS analog of
adding an SSH key to a code host: one web login enrolls the key, and from then on the machine proves
itself with the key.

## Enroll a key

Enrollment is offline on the client and pasted into the web UI, so no private key or token crosses the
wire.

1. On the machine, print an enroll block for your hub account:

   ```bash
   agit identity register <you> [--label <name>]
   ```

   This makes no network call. It prints a signed block carrying only public key material: your ed25519
   and X25519 public keys, a self-asserted committer email, an optional device label (your hostname by
   default), and a signature proving the machine holds the matching private key.

2. In the hub, open **Account -> Signing keys -> Add a signing key** and paste the block. Your logged-in
   session authorizes the add, exactly like adding an SSH key on a code host. The block is signed for a
   specific account, so a block printed for one account cannot be pasted under another.

The block encodes a monotonically increasing epoch, so re-running `agit identity register` and pasting the
newer block updates the key rather than being rejected as a replay.

For the client side in full (rotating, showing, and pinning keys), see
[Identity](../cli/identity.md).

## Multiple keys per account

An account holds any number of device keys, one per machine, each with its own label. Enrolling a key on a
new machine adds to the set; it never replaces the keys you already enrolled. A session attributes to you
when its signing key matches any of your enrolled keys, so you keep the same identity across laptops and CI
runners.

Revoke a key you no longer trust from the same Signing keys page (or with `agit identity revoke` on a
machine that holds it). A revoked key is dropped from the set and no longer attributes anything.

## What enrolling unlocks

**Attribution.** With a key enrolled and its email verified, the hub upgrades a signed, intact session to
`verified as <you>`: the signature checks out, the committer email maps to your account, and the signing
key is one of your keys. Without a matching key the same session reads only as `signed, unregistered`, and
a signed session whose email claims your account but whose key is not yours reads as `key mismatch`, never
verified. See [Verify who produced a session](../integration/provenance.md) and
[Reading a session](./reading-a-session.md).

The committer email you assert at enroll is the bridge from a session's git committer to your account, so
it must be verified before attribution trusts it. Changing the email re-arms verification. See
[Accounts](./accounts.md).

**Key-based hub auth.** The same enrolled key lets git authenticate to the hub without a copy-pasted token:
a challenge-response handshake mints a short-lived bearer token from the key on the fly, so push, pull,
fetch, and clone all authenticate with the key. See [Authentication](../integration/authentication.md).

## Related

- [Identity](../cli/identity.md): the `agit identity` commands.
- [Verify who produced a session](../integration/provenance.md): the attribution model.
- [Accounts](./accounts.md): verifying the email your key asserts.
