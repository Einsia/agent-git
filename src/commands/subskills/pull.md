---
name: agit-pull
description: Fast-forward remote Agent refs into local branches without rebase or conflict merging.
---

# agit pull

## Synopsis

```bash
agit pull [REPO] [options]
```

## Options

| Option | Meaning |
|---|---|
| `[REPO]` | `<owner/name>`; context resolution when omitted |
| `-b, --branch <branch>` | Branches to pull; repeatable |
| `--all` | Pull every local branch |
| `--prune` | Remove missing remote refs when supported by the CLI |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Selection

Without `--all` or `-b`, a context branch limits the pull to that branch; without branch context, the command may pull all local branches. Only fast-forward is accepted. Missing local branches, missing upstreams, and divergence are skipped with warnings.

## Examples

```bash
agit pull szh/p1 -b feature-a
agit pull --all
```
