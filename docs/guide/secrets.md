---
title: Keep secrets out of shared history
nav_order: 8
---

# Keep secrets out of shared history

A session is a record of what an agent read and ran, so it can carry a secret the agent happened to see:
a `.env` it opened, a token it printed. agit scans for secrets before it commits or pushes anything, so
a secret in a transcript does not reach the store or the team by accident.

## When the scan runs

agit scans in-process at its own entry points, before it hands off to git:

- `agit a commit` scans the staged index (what the commit will write).
- `agit a push` scans the commit range about to be published.
- Every snapshot the daemon or `agit snap` takes is scanned the same way.

Only when the scan is clean does the commit or push proceed. `agit init` also installs pre-commit and
pre-push hooks on the store, so a raw `git commit` or `git push` is scanned too. agit's own commit and
push cannot be waved through with `git --no-verify`: they scan in the wrapper first, then pass
`--no-verify` to the git call they make, so the wrapper's scan always runs even though git's own hook is
skipped.

Run the scan by hand any time:

```
agit scan
```

## When the scan finds something

The scan prints each suspected secret (the file, line, rule, and an excerpt) and refuses. A blocked
commit creates no commit; a blocked push sends nothing. You have three ways forward.

### Remove the secret

Take the secret out of the session and try again.

### Mark a false positive

If a hit is known-safe (a documentation example, a placeholder), exempt it:

- Add an `agit:allow-secret` pragma on that line. The pragma exempts one physical line and travels in the
  commit.
- Or add the string to the store's `.agit-allow-secrets` allowlist file, which exempts a matched string.

The hub honors both, so a false positive marked this way clears the local scan and the server scan.

### Override the local scan

```
AGIT_ALLOW_SECRETS=1 agit a push
```

`AGIT_ALLOW_SECRETS` (also `true` or `yes`) lets the action through despite the findings. It is a
visible, auditable override: agit discloses it every time it honors it, naming the action and the
findings. This is the difference from `git --no-verify`, which leaves no trace. The override clears
agit's local scan only. A push to a hub still meets the hub's own scan, which the flag cannot reach.

## The hub scans again

When you push to a [hub](../hub.html), it scans server-side on the way in and rejects a push that carries
a secret. This backstop holds even for a client that never ran the local scan, or one that cleared it
with `AGIT_ALLOW_SECRETS`, because that flag is client-side and does not travel to the server. A genuine
false positive gets past the hub scan the same way it gets past the local one: the `agit:allow-secret`
pragma rides along in the commit, or the operator adds the string to the bare repo's `.agit-allow-secrets`.

Secret scanning recognizes known secret shapes. It is a strong backstop, not a guarantee against a novel
one.
