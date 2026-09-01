---
name: agit-branch
description: List, rename, remove, or seal Agent repo session branches.
---

# agit branch

## Purpose

Operate on the branches of one Agent repo. The repo comes from the session environment or this directory’s binding, so a freshly `init`-ed directory with no session yet can already list and manage branches.

## Synopsis

```bash
agit branch [--all] [-v]
agit branch rename <OLD> <NEW>
agit branch rm [--force] <NAME>
agit branch seal <NAME>
```

## Options

| Option | Meaning |
|---|---|
| `-v, --verbose` | Show more branch details |
| `--all` | Include remote-tracking branches |
| `rename <OLD> <NEW>` | Rename a branch alias without changing history |
| `rm <NAME>` | Delete a local ref and its worktree; published history cannot be deleted, and an unpublished ref or a worktree with uncommitted changes needs `--force`; a branch targeted by an open merge is refused |
| `seal <NAME>` | Seal a branch; it can no longer be resumed, only forked or viewed |
| `--force` | Permit deleting an unpublished branch |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit branch --all -v
agit branch rename experiment experiment-v2
agit branch seal handoff
agit branch rm --force scratch
```

Before removing or sealing a branch, save any needed ref with `agit log`/`agit show`. After sealing, `agit run` takes the fork path instead of treating it as writable.
