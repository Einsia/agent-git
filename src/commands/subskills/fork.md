---
name: agit-fork
description: Create a new session branch from a branch, tag, commit, or turn ref.
---

# agit fork

## Purpose

Copy context from an existing point and create a new branch. This is how to open another session in the same Agent repo; use `init` or `clone` for a new repo. `fork` is not repo creation.

## Synopsis

```bash
agit fork <source> -b <branch> [--resume] [options]
```

## Options

| Option | Meaning |
|---|---|
| `<source>` | Branch, tag, commit, `<repo>@<ref>`, or turn ref; `@` means the branch of the session hosting this process (`AGIT_SESSION` or a harness session id) and never falls back to the workspace pin or this directory |
| `-b, --branch <branch>` | New branch name (required) |
| `--resume` | Start a runtime after forking; does not start by default |
| `--as <runtime>` | Runtime used with `--resume` |
| `--cwd <dir>` | Runtime working directory |
| `--no-launch` | Materialize without starting |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit fork @#12 -b experiment
agit fork szh/p1@main -b review --resume --as codex
agit fork szh/p1@v1.2 -b hotfix --resume --no-launch
```

Verify `refs/heads/<branch>` after completion. Fork history remains in the same Agent repo and does not create a second repo.
