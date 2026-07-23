---
sidebar_position: 1
title: CLI overview
---

# CLI overview

`agit` is a git-compatible CLI that versions an agent's session history the way git versions code. It
wraps two repositories at once:

- The **Environment**: your code repository. `agit <git-args>` runs git against it, unchanged.
- The **Agent Store**: a separate git repository of session transcripts, under `$AGIT_HOME`. `agit a
  <git-args>` (alias for `agit agent`) runs the isomorphic git operation against it.

The scope selector is the first token after `agit`. `agit a commit` commits into the store; `agit commit
-a` is plain git on the code repo, where `-a` is git's stage-all. Because `a` is a subcommand and not a
flag, the two cannot be transposed. The old `agit -a <args>` flag survives as a deprecated alias.

Anything `agit` does not recognize as a native verb passes through to git on the selected scope. So `agit
a log`, `agit a add -A`, `agit a push`, and `agit a diff` all work, and only the verbs that add value are
intercepted.

## Command surface

The native commands group by what they do.

### Capture

Record the runtime's live session into the store.

- `agit a snap` mirrors and commits the current session, gated by the secret scanner.
- `agit watch` runs hands-off: auto-snap plus auto-convert, in the foreground or as a background daemon.

See [Capturing sessions](./capturing.md).

### Resume

Load recorded context back into a runtime.

- `agit start` launches a fresh session already carrying the agent's latest context.
- `agit resume` continues a specific recorded session.
- `agit convert` rewrites a session into the other runtime's format so either CLI can resume it.

See [Resuming sessions](./resuming.md) and [Runtimes](./runtimes.md).

### Reconcile

Bring diverged histories back together.

- `agit a pull` fast-forwards, and routes to merge on divergence.
- `agit a merge` reconciles two diverged sessions by dialogue.
- `agit a log` renders the divergence DAG.

See [Merging sessions](./merging.md) and [Divergence](./divergence.md).

### Share

Publish the store and pull the team's work back.

- `agit a push` / `agit a pull` / `agit a fetch` move sessions over a shared remote.
- `agit a clone` clones an agent by identity.

See [Sharing an agent](../integration/sharing.md) and [Connecting the CLI to a hub](../integration/connect-cli-to-hub.md).

### Secrets

Keep credentials out of shared history.

- `agit a scan` scans session dumps for secrets.
- The commit, push, and merge gates scan before anything reaches git.
- `agit a purge-history` rewrites secrets out of past commits.

See [Secrets](./secrets.md).

### Encryption

Encrypt session content at rest.

- `agit a encrypt` enables the per-session keybox.
- `agit a readers` / `agit a rekey` manage recipients and rotate keys.

See [Encryption](./encryption.md).

### Identity

Sign sessions and authenticate to a hub with a key.

- `agit identity register <you>` enrolls this machine's key with a hub account.
- `agit provenance verify` checks who produced a session.

See [Identity and signing keys](./identity.md) and [Authentication](../integration/authentication.md).

### Diagnostics

Inspect and report on the install.

- `agit doctor` runs a fast health check.
- `agit debug` writes a redacted diagnostic bundle.
- `agit relocate` moves sessions captured in the wrong directory into this repo.

See [Diagnostics](./diagnostics.md) and [Relocating sessions](./relocating.md).

## Full list

Every native verb, with a one-line description, is in the [command reference](./command-reference.md).
For environment variables and on-disk files, see [Configuration](./configuration.md).
