---
name: agit-config
description: Manage AgentGit global configuration.
---

# agit config

## Purpose

Read, set, or remove global settings such as `hub.url`, `runtime.default`, `push.visibility`, and `commit.auto`. `push.visibility` is the default for a first publish when neither a push flag nor a repo preference (`agit init --private`) says otherwise.

## Synopsis

```bash
agit config [<key> [<value>]]
agit config --list
agit config --unset <key>
```

## Options

| Option | Meaning |
|---|---|
| `<key>` | Configuration key; read it when no value is supplied |
| `<value>` | Value to write |
| `--unset` | Remove the key |
| `--list` | List all settings |
| `--json` | Emit the unified CLI JSON envelope |
| `-y, --yes` | Skip confirmation |
| `-q, --quiet` | Reduce output |
| `-C, --directory <dir>` | Use the given directory |
| `--no-color` | Disable color |
| `-h, --help`, `-V, --version` | Show help or version |

## Examples

```bash
agit config hub.url
agit config runtime.default codex
agit config commit.auto false
agit config --list
agit config --unset runtime.default
```

Configuration is not session metadata; changing it does not change an existing branch or runtime link.
