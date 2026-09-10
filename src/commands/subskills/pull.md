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
| `[REPO]` | `<owner/name>@<branch>` or a bare repo; an omitted repo requires `AGIT_SESSION` |
| `-b, --branch <branch>` | Branches to pull; repeatable |
| `--all` | Pull every local branch |
| `--prune` | Remove missing remote refs when supported by the CLI |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Selection

An explicit `owner/repo@branch` pulls that branch and cannot be combined with `-b` or `--all`. For a bare repo, `-b` selects branches; otherwise all local branches are considered. When the repo is omitted, `AGIT_SESSION` selects both repo and branch unless `-b` or `--all` overrides the branch selection. Only fast-forward is accepted. Missing local branches, missing upstreams, and divergence are skipped with warnings.

## Examples

```bash
agit pull szh/p1 -b feature-a
agit pull szh/p1 --all
```
