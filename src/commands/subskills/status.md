---
name: agit-status
description: Show current context, directory bindings, adopted sessions, and local Agent repo state.
---

# agit status

## Purpose

Answer "who am I, am I in a session, and is the repo synchronized?" It reports workspace bindings, adopted runtime sessions, local branches, and their relationship to fetched tracking refs.

Default status reads local state without checking for updates, creating storage, or migrating history.
Remote state means the refs already fetched locally. If interrupted storage recovery is pending,
status refuses. Complete recovery through the original AgentGit store before inspecting status
again; its normal `agit doctor` startup can recover supported local checkouts. Status does not
change the migration rules for repositories linked from another store.

## Synopsis

```bash
agit status
```

## Options

| Option | Meaning |
|---|---|
| `--check-missing` | Also inspect runtime indexes for unadopted sessions; SQLite may maintain WAL sidecar files |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Reading the output

The explicit `--check-missing` option uses the runtime index readers and may cause SQLite to
maintain its sidecar files. Use default status when inspection must leave local files unchanged.
Neither form performs a startup migration or an update check.

- `no session target supplied through AGIT_SESSION`: no usable explicit process identity.
- `bound repo`: the cwd's Agent repo route; it does not prove that a branch exists.

The repository table lists branches even when no runtime session has been adopted. Each row
includes the last commit and its tracking ref. An explicit upstream takes precedence; otherwise
a fetched `origin` branch with the same name supplies the comparison. Remote-only branches from
any remote remain visible. Status does not contact those remotes.

- `in sync`: the local branch and its known tracking ref identify the same commit.
- `ahead`, `behind`, or `diverged`: complete local commit ancestry establishes the difference.
- `no known tracking ref`: no upstream or same-name fetched `origin` branch is known locally.
- `tracking ref unavailable locally`: the configured upstream is missing or cannot be mapped to a local ref; status does not substitute an origin branch.
- `comparison unavailable`: local ancestry is missing, malformed, or exceeds the inspection budget.
- `remote only`: a fetched remote branch has no corresponding local branch in the table.

Status bounds the displayed rows and ancestry inspection. An incomplete display includes a warning;
use `agit branch --repo <owner/repo> --all` to inspect another repository's refs. A missing tracking
ref does not establish whether the branch has ever been published. Shallow or grafted history
produces counts only when the immutable parent graph can be reconstructed completely.

## Examples

```bash
agit status
agit status --check-missing
agit -C ~/Projects/p1 status
```

A bound repo does not select a session target. Use a full target or set `AGIT_SESSION` to
an existing local branch. Use `new` to create a session or `import` to adopt one;
`resume` continues an existing session. Verify `refs/heads/<branch>` before publishing it.
