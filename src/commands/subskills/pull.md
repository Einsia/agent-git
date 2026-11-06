---
name: agit-pull
description: Fetch and fast-forward branches, automatically reconciling divergent file branches.
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

An explicit `owner/repo@branch` pulls that branch and cannot be combined with `-b` or `--all`. For a bare repo, `-b` selects branches; otherwise all local branches are considered. When the repo is omitted, `AGIT_SESSION` selects both repo and branch unless `-b` or `--all` overrides the branch selection. Missing local branches and missing upstreams are skipped with warnings.

Branches fast-forward when possible. If both sides explicitly declare a file branch, pull
three-way merges divergent changes with a common ancestor. A clean merge lands automatically,
without a runtime or a summary prompt. Save uncommitted and staged changes before pulling.

Conflicts leave the target branch unchanged and open a manual transaction. The output lists
the conflict paths and, for each, the ways out: keep yours (`agit merge --into <owner/repo>@<branch>
--continue --keep-mine <path>`), take theirs (`--continue --take-theirs <path>`), or merge by hand
(`--edit <path>` opens it with both sides marked, then `--continue --resolved <path>`); `--abort`
cancels and keeps checkout edits. In a terminal it also shows each conflicting place, both sides
and who last changed them; that text is not printed when the output is captured, such as an
agent's tool call.
Session divergence still requires explicit reconciliation; transcripts are never auto-merged.

## Examples

```bash
agit pull szh/p1 -b feature-a
agit pull szh/p1 --all
```
