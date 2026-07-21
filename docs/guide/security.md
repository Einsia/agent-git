---
title: Security
nav_order: 8
---

# Security

A session is a record of what an agent read and ran, so it can carry two things worth handling with
care: a secret the agent happened to see (a `.env` it opened, a token it printed), and the question of
which machine actually produced it. agit adds two layers on top of git for these: a secret gate that
runs before anything is committed or pushed, and provenance that signs each captured session.

## The secret gate

agit scans for secrets in-process at its own entry points, before it hands off to git. `agit a commit`
scans the staged index (what the commit will write), `agit a push` scans the working tree (what the
push will publish), and every snapshot the daemon or `agit snap` takes is scanned the same way. Only
when the scan is clean does the commit or push proceed.

This runs in addition to the pre-commit and pre-push hooks that `agit init` installs on the store. A
raw `git commit`/`git push` fires those hooks, and `git --no-verify` skips them. agit's own commit and
push cannot be waved through that way: they scan in the wrapper first, then pass `--no-verify` to the
git call they make, so the wrapper's scan always runs even though git's redundant hook does not. The
gate is the last check before a secret in a transcript is committed to the store or pushed to the team.

### When the gate finds something

The gate prints each suspected secret (the file, line, rule, and an excerpt) and refuses, stating
plainly that the action did not happen: a blocked `agit a commit` creates no commit, and a blocked push
sends nothing. You then have three ways forward:

- **Fix it.** Remove the secret from the session and try again.
- **Mark a false positive.** If a hit is known-safe (a documentation example, a placeholder), exempt it
  with an `agit:allow-secret` pragma on that line, or add an entry to the store's `.agit-allow-secrets`
  allowlist file. The pragma exempts one physical line; the allowlist exempts a matched string. Both are
  read by the hub's gate too (the pragma travels in the commit; the allowlist lives in the bare repo),
  so a false positive marked this way clears the local *and* the server check.
- **Override the local gate.** Re-run with `AGIT_ALLOW_SECRETS=1` (`true` and `yes` also count) to let
  the action through despite the findings. This clears agit's *local* gate only; a push to a hub still
  meets the hub's own server-side gate, which the flag cannot reach ([On the hub](#on-the-hub)).

`AGIT_ALLOW_SECRETS` is a visible, auditable override, not a silent bypass. agit discloses it every
time it honors it: which action, which findings, and that you own the consequences. That is the
difference from git's `--no-verify`, which leaves no trace. The escape stays on the record instead of
being hidden at a coarser grain.

Run the scan by hand any time with `agit scan`.

### On the hub

When you push to a [hub](../hub.html), it scans again server-side on the way in and rejects a push that
carries a secret, so the backstop holds even for a client that never ran the gate, or one that cleared
its own gate with `AGIT_ALLOW_SECRETS`, which is a client-side flag and does not travel to the server.
A genuine false positive gets past the hub gate the same way it gets past the local one: the
`agit:allow-secret` pragma rides along in the commit, or the operator adds the string to the bare repo's
`.agit-allow-secrets`. Secret scanning recognizes known secret shapes; it is a strong backstop, not a
guarantee against a novel one.

## Provenance

Provenance ties a captured session to the machine that produced it. Each machine has an ed25519 signing
key, minted on first use and stored at `$AGIT_HOME/identity/ed25519` (the private key is `0600`). Show
this machine's public identity with:

```
agit provenance key
```

When a session is captured, agit signs it with that key and records the signature in the session's
committed sidecar, alongside the transcript digest, the agent's aid, the committer email, and the start
time. The public key is safe to show and share; the signature travels with the session in the store.

### Your committer identity is your provenance handle

The committer email recorded in a session's provenance is your git identity, resolved exactly the way
git resolves it: the store inherits `user.email` from your local then global git config. agit does not
invent one for you. Set it once like any git repo:

```
git config --global user.email you@example.com
git config --global user.name  "Your Name"
```

You can also set it per agent with `agit a config user.email you@example.com`, which is plain git on the
agent store.

That email is the handle the hub later uses to attribute a signature to an account, so agit will not
snap a session under an unset identity. If no `user.email` is configured, `agit snap` (and the auto-snap
the daemon and `agit a merge` perform) refuses git-style: it makes no commit and leaves nothing staged,
and prints how to set the identity. Set your git identity and snap again.

agit's own bookkeeping commits (minting an agent, a rename, enabling encryption) are authored by
`agit <agit@local>` on purpose, so they are never attributed to you and never depend on you having a git
identity at all. A brand-new machine with no git identity configured can still create agents; only a
session snap needs your identity, because a session is the thing being attributed.

### Verifying a session

```
agit provenance verify <session>
```

The argument is a session path or a session id in the resolved agent's store. Verification recomputes
the transcript digest, rebuilds the signed message, and checks the signature against the public key the
record itself carries. It reports one of:

| Outcome | Meaning | Exit |
|---|---|---|
| **verified** | The content is intact and the signature matches its recorded key. | 0 |
| **unverified, no signature** | The session carries no signature (captured before signing existed, or with no key available). | 0 |
| **tampered** | The transcript changed after it was signed: its current digest differs from the signed one. | non-zero |
| **bad signature** | A signature is present but does not verify against its recorded key. | non-zero |

An unsigned session is a soft "unverified" and never blocks, mirroring the rest of agit's attribution
handling: a missing signal is reported, never fatal. A signature that is present but does not check out
is a hard failure worth a non-zero exit.

Self-verify proves that the content is intact and the signature matches the recorded key. It does not
assert who that key belongs to, which is why the verdict is "verified", never "trusted".

### Verifying as a person (the hub registry)

Self-verify says "signed by this key". To turn that into "signed by this person", the key has to be tied
to an account. That link lives on the [hub](../hub.html): each account registers the ed25519 device keys
that belong to it, and `agit provenance verify` looks the committer email up in that registry before it
reports a verdict.

The end-to-end flow:

1. **Set your git identity** so snapped sessions carry your email (above).
2. **Snap** a session (`agit snap`, or let the daemon capture it). Its provenance now binds your email
   and this machine's signing key.
3. **Register this device key** with your hub account. `agit identity register <you>` prints a paste-able
   enroll block; paste it into the hub web UI to enroll the key under your account. `agit identity show`
   reports what this machine has enrolled.
4. **Verify.** `agit provenance verify <session>` (or `agit a provenance verify`) consults the hub and,
   when your email maps to an account whose registered keys include the session's signing key, upgrades
   the verdict to **VERIFIED AS you**.

The extra registry-aware verdicts:

| Outcome | Meaning | Exit |
|---|---|---|
| **VERIFIED AS `<user>`** | Self-verify passed AND the committer email maps to a hub account whose registered keys include the signing key. The only "verified as a person" verdict. | 0 |
| **signed, unregistered** | Self-verify passed, but the committer email maps to no account (or no hub was reachable). Attributed to a key, not yet to a person. | 0 |
| **KEY MISMATCH** | Self-verify passed and the email maps to an account, but the signing key is NONE of that account's registered keys: a possible forgery. | non-zero |

The registered key set is TOFU-pinned on first sighting, so the hub cannot later swap in a key to
manufacture a false "verified as". A wrong or spoofed committer email never produces a false VERIFIED AS:
it degrades to "signed, unregistered" or KEY MISMATCH. The signature against a registered key is the real
boundary; the email is only the lookup handle.

## `.agit.toml` is attacker-controlled input

The binding is written by whoever authored the repo, so agit treats a remote it declares as untrusted.
Before cloning a remote from `.agit.toml`, agit checks it against a transport allowlist, because a URL
like `ext::<cmd>` would otherwise have git execute `<cmd>`.
