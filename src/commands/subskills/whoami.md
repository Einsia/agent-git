---
name: agit-whoami
description: Show the current AgentGit Hub identity.
---

# agit whoami

## Purpose

Show the current Hub, account, email, and credential expiry offline by default. Add `--check` to verify the token online: it calls an authenticated endpoint, so a revoked or forged token is reported as rejected (exit 5) rather than as a healthy chain; an unreachable Hub is exit 6.

## Synopsis

```bash
agit whoami [--check]
```

## Options

| Option | Meaning |
|---|---|
| `--check` | Verify credentials online |
| `--json` | Emit the unified CLI JSON envelope |
| `-y, --yes` | Skip confirmation |
| `-q, --quiet` | Reduce output |
| `-C, --directory <dir>` | Use the given working directory |
| `--no-color` | Disable color |
| `-h, --help`, `-V, --version` | Show help or version |

## Examples

```bash
agit whoami
agit whoami --check
```
