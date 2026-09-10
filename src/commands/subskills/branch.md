---
name: agit-branch
description: List, rename, remove, or seal Agent repo session branches.
---

# agit branch

## Purpose

Operate on the branches of one Agent repo. Name the repo with `--repo <owner/repo>` or supply `AGIT_SESSION`. Directory bindings do not select the target.

The ordinary text listing reads current ref metadata without walking history. `-v` adds
sync comparisons; `--json` includes the session's opening subject and therefore reads history.

## Synopsis

```bash
agit branch --repo <owner/repo> [--all] [-v]
agit branch rename <OLD> <NEW>
agit branch rm [--force] <NAME>
agit branch seal <NAME>
```

## Options

| Option | Meaning |
|---|---|
| `--repo <owner/repo>` | Explicit Agent repo; overrides `AGIT_SESSION` and is accepted with every subcommand |
| `-v, --verbose` | Show more branch details |
| `--all` | Include remote-tracking branches |
| `rename <OLD> <NEW>` | Rename a branch alias without changing history |
| `rm <NAME>` | Delete a local ref and its worktree; published history cannot be deleted, and an unpublished ref or a worktree with uncommitted changes needs `--force`; a branch targeted by an open merge is refused |
| `seal <NAME>` | Seal a branch; it can no longer be resumed, only forked or viewed |
| `--force` | Permit deleting an unpublished branch |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit branch --repo alice/payments --all -v
agit branch --repo alice/payments rename experiment experiment-v2
agit branch --repo alice/payments seal handoff
agit branch --repo alice/payments rm --force scratch
```

Before removing or sealing a branch, save any needed ref with `agit log`/`agit show`. After sealing, `agit open` takes the fork path instead of treating it as writable.

## Structured listing

`agit branch --repo alice/payments --json --all` returns typed records in
`result.value.branches`. Each record includes its full `ref` and `oid`, `name`, `local`,
`current` (the Agent repo checkout), declared `line`, complete `session_id` and `runtime`,
`turns`, `committed_at` (Unix seconds), `opening_subject`, `sealed`, and `code_anchor`.
`opening_subject` is the first numbered turn in that session's first-parent lineage. Birth
and claim commits are skipped; an empty session or a fork with no own turn has an empty
opening subject. A runtime change within the same durable session retains its opener.
`turns` is the settled conversation counter from metadata; file, birth, and merge commits do
not inflate it. An absent declaration has null `line`, `session_id`, and `runtime`.

`sync` contains `upstream_ref`, `ahead`, and `behind`. Null counts mean the upstream is
unavailable; zero counts mean the refs are aligned. The comparison uses local tracking refs
without fetching. `--all` includes origin tracking refs as separate records even when a
same-name local branch exists. Empty repositories return an empty array. Corrupt metadata
produces a failed envelope instead of being reported as an undeclared session.

Listings batch their metadata, marker, and graph reads from a captured ref snapshot and do
not open a transcript. Only the selected branches' first-parent metadata is read; a damaged
remote branch excluded from the view cannot block a local listing.
