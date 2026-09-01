---
name: agit-switch
description: Set or remove the default branch pin for the current workspace.
---

# agit switch

## Purpose

Permanently route a directory (or the directory passed with `-C`) to a session branch. It changes only workspace context: it does not create a branch, change `AGIT_SESSION`, or move a transcript.

## Synopsis

```bash
agit switch [BRANCH]
agit switch --unbind
```

## Options

| Option | Meaning |
|---|---|
| `[BRANCH]` | Local branch to pin; raw branch name, not `@` |
| `--unbind` | Remove the current directory's pin |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit switch feature-x
agit -C ~/Projects/p1 switch --unbind
```

If the branch does not exist, create it with `new`, `import`, or `fork` first. `switch` never creates `refs/heads/<branch>`; use `agit status` to confirm the pin.
