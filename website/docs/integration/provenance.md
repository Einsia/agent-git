---
sidebar_position: 4
title: End-to-end provenance
---

# End-to-end provenance

Provenance ties a captured session to the machine that produced it, and to the person who owns that
machine's key. Each machine signs the sessions it captures with its ed25519 key. To turn "signed by this
key" into "signed by this person", register the key with a hub account and verify against the hub.

## How signing works

When agit captures a session it signs it with this machine's key and records the signature in the
session's committed sidecar, alongside the transcript digest, the agent's aid, your committer email, and
the start time. The public key is safe to share; the signature travels with the session in the store.

The person a session is attributed to is your git committer identity, resolved the way git resolves it.
Set it once, as for any repository:

```bash
git config --global user.email you@example.com
git config --global user.name  "Your Name"
```

agit refuses to record a session while your identity is unset, because a session is the thing being
attributed.

Show this machine's signing key:

```bash
agit provenance key
```

## Self-verify

```bash
agit provenance verify [<session|agent>]
```

With no argument it verifies the active agent's latest session. A session path or id verifies that one;
an agent name verifies every session in that agent's store. Verification recomputes the transcript
digest, rebuilds the signed message, and checks the signature against the public key the record carries.

| Verdict | Meaning | Exit |
|---|---|---|
| verified | the content is intact and the signature matches its recorded key | 0 |
| unverified, no signature | the session carries no signature (captured before signing, or with no key available) | 0 |
| tampered | the transcript changed after it was signed: its current digest differs from the signed one | non-zero |
| bad signature | a signature is present but does not verify against its recorded key | non-zero |

An unsigned session reports "unverified" and never blocks. A signature that is present but does not check
out is a hard failure. Self-verify proves the content is intact and the signature matches the recorded
key. It does not say who that key belongs to, which is why the verdict is "verified", never "trusted".

## Verify as a person

Register this machine's key with your hub account:

```bash
agit identity register you
```

This prints a paste-able enroll block. Paste it into the web UI under Account, then Signing keys, to
enroll the key. See [signing keys](../hub/signing-keys.md) and [`agit identity`](../cli/identity.md).

With a key enrolled, `agit provenance verify` looks the committer email up in the hub's registry before
it reports a verdict. When your email maps to a hub account whose registered keys include the session's
signing key, the verdict upgrades:

| Verdict | Meaning | Exit |
|---|---|---|
| VERIFIED AS `<user>` | self-verify passed and the committer email maps to an account whose registered keys include the signing key; the only "verified as a person" verdict | 0 |
| signed, unregistered | self-verify passed, but the email maps to no account (or no hub was reachable); attributed to a key, not yet a person | 0 |
| KEY MISMATCH | self-verify passed and the email maps to an account, but the signing key is none of that account's registered keys: a possible forgery | non-zero |

## Why the verdicts are hard to forge

The hub attributes a session to the account whose registered key signed it, tied to the committer email
as the lookup handle. Two properties keep this honest:

- The account's registered key set is pinned on first sighting. A hub cannot later swap in a key to
  manufacture a false VERIFIED AS; a changed set fails rather than silently re-attributes. A key enrolled
  on a second machine is still VERIFIED AS you, not a false KEY MISMATCH.

  When a pinned key changes for a legitimate rotation, verification blocks and prints a re-pin
  instruction. Confirm the new fingerprint out of band, then accept it with `--repin`:

  ```bash
  agit provenance verify <session> --repin
  ```
- The signature against a registered key is the real boundary. A wrong or spoofed committer email never
  produces a false VERIFIED AS: it degrades to "signed, unregistered" or KEY MISMATCH.

An offline verify, where no hub is reachable, degrades to "signed, unregistered", never a false
attribution. The hub renders the same badge on the session page; see
[reading a session](../hub/reading-a-session.md).
