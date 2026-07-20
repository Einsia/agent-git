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

## `.agit.toml` is attacker-controlled input

The binding is written by whoever authored the repo, so agit treats a remote it declares as untrusted.
Before cloning a remote from `.agit.toml`, agit checks it against a transport allowlist, because a URL
like `ext::<cmd>` would otherwise have git execute `<cmd>`.
