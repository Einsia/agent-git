---
name: agit-status
description: Show current context, directory bindings, adopted sessions, and local Agent repo state.
---

# agit status

## Purpose

Answer “who am I, am I in a session, and is the repo synchronized?” It reports workspace bindings, adopted runtime sessions, Agent repo version counts, and push state.

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
- `never pushed`: the local Agent repo has commits/refs that have not reached the Hub.
- `in sync`: local and known remote state agree.

## Examples

```bash
agit status
agit status --check-missing
agit -C ~/Projects/p1 status
```

A bound repo does not select a session target. Use a full target or set `AGIT_SESSION` to
an existing local branch. Use `new` to create a session or `import` to adopt one;
`resume` continues an existing session. Verify `refs/heads/<branch>` before publishing it.
