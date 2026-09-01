---
name: agit-upgrade
description: Check for and update the AgentGit CLI.
---

# agit upgrade

## Synopsis

```bash
agit upgrade [--check]
```

## Options

| Option | Meaning |
|---|---|
| `--check` | Report available updates without installing |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit upgrade --check
agit upgrade
```

Upgrading the CLI does not migrate or delete `~/.agit/repos`. Run `agit doctor` afterwards to verify runtime integrations.
