---
name: agit-run
description: Fetch any frozen ref, decide whether to resume or fork, and start a runtime.
---

# agit run

## Purpose

Run a branch, tag, historical commit, or turn ref. The command fetches or locates the ref, resumes only a writable unsealed session-branch head, and forks all other refs.

## Synopsis

```bash
agit run <owner/repo>@<ref> [options]
agit run <local-ref> [options]
```

## Options

| Option | Meaning |
|---|---|
| `<source>` | `owner/repo@ref` for explicit network access, or a local ref |
| `--mine` | Copy into your namespace before forking |
| `--as <runtime>` | Runtime to use |
| `--cwd <dir>` | Working directory for the runtime |
| `-b, --branch <branch>` | New branch name when a fork occurs; required non-interactively |
| `--no-launch` | Fetch/fork/materialize without starting |
| `--json` | Emit the unified CLI JSON envelope |
| `-y, --yes` | Skip confirmation |
| `-q, --quiet` | Reduce output |
| `-C, --directory <dir>` | Use the given command directory |
| `--no-color` | Disable color |
| `-h, --help`, `-V, --version` | Show help or version |

## Examples

```bash
agit run szh/p1@version0 -b experiment --no-launch
agit run szh/p1@feature-x
agit run alice/notes@v2 --mine -b my-v2 --as codex
```

Use `resume` when you know the session should continue, and `fork` when you know an old point should start a new line.
