---
title: Verify who produced a session
nav_order: 9
---

# Verify who produced a session

Provenance ties a captured session to the machine that produced it, and to the person who owns that
machine's key. Each machine signs the sessions it captures. To confirm a session was produced by a
specific person, you register that machine's key with their hub account and verify against the hub.

## Set your git identity

The person a session is attributed to is your git committer identity, resolved the way git resolves it
(local config, then global). agit does not invent one. Set it once, the same as any git repository:

```
git config --global user.email you@example.com
git config --global user.name  "Your Name"
```

You can also set it per agent with `agit a config user.email you@example.com`, which is plain git on the
store. agit refuses to record a session while your identity is unset, because a session is the thing being
attributed. agit's own bookkeeping commits (creating an agent, a rename) are authored by `agit@local` and
never need your identity, so a new machine can create agents before you set one.

## Show this machine's signing key

Each machine has an ed25519 signing key, created on first use and stored at
`$AGIT_HOME/identity/ed25519` (the private key is `0600`). Show its public identity:

```
agit provenance key
```

When agit captures a session, it signs it with that key and records the signature in the session's
committed sidecar, alongside the transcript digest, the agent's aid, your committer email, and the start
time. The public key is safe to share; the signature travels with the session in the store.

## Verify a session

```
agit provenance verify <session>
```

The argument is a session path or id in the resolved agent's store. Verification recomputes the transcript
digest, rebuilds the signed message, and checks the signature against the public key the record carries.
It reports one of:

| Outcome | Meaning | Exit |
|---|---|---|
| verified | The content is intact and the signature matches its recorded key. | 0 |
| unverified, no signature | The session carries no signature (captured before signing existed, or with no key available). | 0 |
| tampered | The transcript changed after it was signed: its current digest differs from the signed one. | non-zero |
| bad signature | A signature is present but does not verify against its recorded key. | non-zero |

An unsigned session reports "unverified" and never blocks. A signature that is present but does not check
out is a hard failure with a non-zero exit.

Self-verify proves the content is intact and the signature matches the recorded key. It does not say who
that key belongs to, which is why the verdict is "verified", never "trusted".

## Verify as a person

To turn "signed by this key" into "signed by this person", register the machine's key with a hub account.
`agit provenance verify` then looks the committer email up in the hub's registry before it reports a
verdict.

1. Set your git identity so snapped sessions carry your email (above).
2. Snap a session, or let the daemon capture it. Its provenance now binds your email and this machine's
   signing key.
3. Register this device key with your hub account:

   ```
   agit identity register <you>
   ```

   This prints a paste-able enroll block. Paste it into the hub web UI to enroll the key under your
   account. `agit identity show` reports what this machine has enrolled.
4. Verify:

   ```
   agit provenance verify <session>
   ```

   When your email maps to a hub account whose registered keys include the session's signing key, the
   verdict upgrades to VERIFIED AS you.

The registry-aware verdicts:

| Outcome | Meaning | Exit |
|---|---|---|
| VERIFIED AS `<user>` | Self-verify passed and the committer email maps to a hub account whose registered keys include the signing key. The only "verified as a person" verdict. | 0 |
| signed, unregistered | Self-verify passed, but the committer email maps to no account (or no hub was reachable). Attributed to a key, not yet to a person. | 0 |
| KEY MISMATCH | Self-verify passed and the email maps to an account, but the signing key is none of that account's registered keys: a possible forgery. | non-zero |

The registered key set is pinned on first sighting, so the hub cannot later swap in a key to manufacture
a false "verified as". A wrong or spoofed committer email never produces a false VERIFIED AS: it degrades
to "signed, unregistered" or KEY MISMATCH. The signature against a registered key is the real boundary;
the email is only the lookup handle.
