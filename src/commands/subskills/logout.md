---
name: agit-logout
description: Sign out of the AgentGit Hub.
---

# agit logout

## Purpose

Revoke the server session and remove local credentials. It does not delete `~/.agit/repos`, `~/.agit/store`, or workspace records.

## Synopsis

```bash
agit logout [--all]
```

## Options

| Option | Meaning |
|---|---|
| `--all` | Revoke the server session on every signed-in Hub, then delete all local credentials; one Hub failing to revoke does not stop the others |
| `--json` | Emit the unified CLI JSON envelope |
| `-y, --yes` | Skip confirmation |
| `-q, --quiet` | Reduce output |
| `-C, --directory <dir>` | Use the given working directory |
| `--no-color` | Disable color |
| `-h, --help`, `-V, --version` | Show help or version |

## Examples

```bash
agit logout
agit logout --all
```
