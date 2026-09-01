---
name: agit-login
description: Sign in to the AgentGit Hub.
---

# agit login

## Purpose

Create Hub credentials for commands such as `clone`, `import`, `push`, `repo`, and `rc`.

## Synopsis

```bash
agit login [--hub <url>] [--with-token | --device]
```

## Options

| Option | Meaning |
|---|---|
| `--hub <url>` | Hub URL; precedence is option, `AGIT_HUB_URL`, `config hub.url`, then the built-in public Hub |
| `--with-token` | Read a PAT from stdin, suitable for CI and agents |
| `--device` | Use device-code flow directly |
| `--json` | Emit the unified CLI JSON envelope |
| `-y, --yes` | Skip confirmation |
| `-q, --quiet` | Reduce output |
| `-C, --directory <dir>` | Use the given working directory |
| `--no-color` | Disable color |
| `-h, --help`, `-V, --version` | Show help or version |

## Examples

```bash
agit login
printf '%s\n' "$AGIT_PAT" | agit login --with-token
agit login --hub https://staging.agent-git.com --device
```

Login does not create an Agent repo or bind a workspace.
