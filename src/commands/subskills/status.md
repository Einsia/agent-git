---
name: agit-status
description: Show current context, directory bindings, adopted sessions, and local Agent repo state.
---

# agit status

## Purpose

Answer “who am I, am I in a session, and is the repo synchronized?” It reports workspace bindings, adopted runtime sessions, Agent repo version counts, and push state.

## Synopsis

```bash
agit status
```

## Options

| Option | Meaning |
|---|---|
| `--check-missing` | Also scan runtime directories for unadopted sessions (slower) |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Reading the output

- `not inside an agent session`: no `AGIT_SESSION` and no uniquely resolvable session.
- `bound repo`: the cwd's Agent repo route; it does not prove that a branch exists.
- `pinned: <branch>`: the directory's default branch set by `agit switch`.
- `never pushed`: the local Agent repo has commits/refs that have not reached the Hub.
- `in sync`: local and known remote state agree.

## Examples

```bash
agit status
agit status --check-missing
agit -C ~/Projects/p1 status
```

If a repo is bound but the branch is `(none)`, do not push yet. Create or adopt a branch with `new`, `import`, or `resume`, then verify `refs/heads/<branch>`.
