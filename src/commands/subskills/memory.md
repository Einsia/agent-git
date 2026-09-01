---
name: agit-memory
description: Memory between the runtime directory, the session branch and main — status, diff, distill, sync.
---

# agit memory

## Purpose

Memory has three homes: the runtime's own memory directory (Claude Code: `~/.claude/projects/<project>/memory/`), the session branch's `memory/`, and `main`'s `memory/`. The first two sync by themselves — `new`/`resume` place the branch's files under `agit/<owner>/<name>/<branch>/` in the runtime directory and index them in `MEMORY.md`; every `agit commit` collects what changed **since that baseline** (new, edited or deleted top-level files, and edits or deletions in the mirror) back onto the branch — memory that was already there and untouched stays local. Only top-level `*.md` files are managed. `main` only moves when you distill; a file inherited from main and deleted on the branch distills as a deletion.

## Synopsis

```
agit memory [status] [--into <owner/repo@branch>]
agit memory diff [<file>]
agit memory distill [<file>…] [-y]
agit memory sync
agit distill [<file>…] [-y]          # alias of `agit memory distill`
```

## Subcommands

| Subcommand | Effect | Notes |
|---|---|---|
| `status` | One row per file: branch vs main, and the runtime directory vs branch | Default when no subcommand is given |
| `diff [<file>]` | `git diff` of `memory/` between main and this branch | — |
| `distill [<file>…]` | Carry files from this branch into main as one file commit | Every file is secret-scanned; each is confirmed unless `-y` |
| `sync` | Collect now (runtime → branch), then re-place (branch → runtime) | Also what `commit` and `resume` do on their own |
| `--into` | Target a specific `<owner/repo>@<branch>` instead of the current session | Global |

## Examples

```bash
agit memory status
agit memory diff refund-path.md
agit distill                      # everything that differs from main, one confirmation each
agit distill refund-path.md -y
agit memory sync --into szh/p1@refund-fix
```

## Notes

- Only Claude Code keeps a per-project memory directory; for other runtimes `sync` is a no-op while `status`/`distill` still work on the branch.
- Files that hit the secret scanner are never collected or distilled; clean them first.
- `memory.track = off` (`agit config`) turns collection off for this machine, on every path including `sync`.
- A session not started through agit has no baseline: `commit` only records one; `memory sync` collects every top-level file.
