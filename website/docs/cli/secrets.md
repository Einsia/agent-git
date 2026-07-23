---
sidebar_position: 7
title: Secrets
---

# Secrets

A session records what an agent read and ran, so it can carry a secret the agent happened to see: a
`.env` it opened, a token it printed. agit scans for secrets before it commits or pushes anything, so a
secret in a transcript does not reach the store or the team by accident.

## The scan and the gates

agit scans in-process at its own entry points, before it hands off to git:

- `agit a commit` scans the staged index (what the commit will write). It stages the commit's inputs
  first, so a `agit a commit -a` or a pathspec commit is scanned exactly as it will be written.
- `agit a push` scans the commit range about to be published.
- Every snapshot from `agit a snap`, `agit watch`, or a completed `agit a merge` is scanned the same way.

Only a clean scan lets the commit, push, or snapshot proceed. `agit init` also installs pre-commit and
pre-push hooks on the store, so a raw `git commit` or `git push` is scanned too.

agit's own commit and push cannot be waved through with `git --no-verify`: they scan in the wrapper first,
then pass `--no-verify` to the git call they make, so the wrapper's scan always runs even though git's own
hook is skipped.

Scan the agent's sessions by hand any time:

```bash
agit a scan
agit a scan --staged      # scan the staged index instead of the working tree
```

A dash flag agit does not recognize is rejected, never treated as a scan target, so `agit a scan
--no-verify` cannot silently scan a nonexistent file and report "no secrets found".

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

A hub honors both, so a false positive marked this way clears the local scan and the server scan.

### Override the local scan

```bash
AGIT_ALLOW_SECRETS=1 agit a push
```

`AGIT_ALLOW_SECRETS` (also `true` or `yes`) lets the action through despite the findings. It is a
visible, auditable override: agit discloses it every time it honors it, naming the action and the
findings. This is the difference from `git --no-verify`, which leaves no trace. The override clears agit's
local scan only. A push to a hub still meets the hub's own scan, which the flag cannot reach.

## Store scope versus environment scope

The scan runs on the **Agent Store**, the git repository of transcripts under `$AGIT_HOME`. That is where
secrets end up, because a transcript is what records the token an agent read. The commit, push, and merge
gates and the `.agit-allow-secrets` allowlist all live on the store. Your **Environment** (the code
repository) keeps its own secrets discipline; agit does not scan it, and running plain `agit <git-args>`
there is git untouched.

## Scrub secrets already committed

If a secret reached history before you caught it, `agit a purge-history` rewrites the store so no
plaintext survives in any commit. It is guard-railed: it checks preconditions, requires a clean tree, uses
`git filter-repo` when available (falling back to `git filter-branch`), and never auto-pushes. It prints
the exact force-push command to run after you review the rewrite. See also [Encryption](./encryption.md),
which uses the same command to scrub pre-encryption plaintext.

## The hub scans again

When you push to a hub it scans server-side on the way in and rejects a push that carries a secret. This
backstop holds even for a client that never ran the local scan, or one that cleared it with
`AGIT_ALLOW_SECRETS`, because that flag is client-side and does not travel to the server. See
[Reporting problems](../hub/reporting-problems.md) for the hub side.

Secret scanning recognizes known secret shapes. It is a strong backstop, not a guarantee against a novel
one.
