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
agit privacy policy allow-memory decisions.md --repo szh/p1
agit memory status
agit memory diff refund-path.md
agit distill                      # everything that differs from main, one confirmation each
agit distill refund-path.md -y
agit memory sync --into szh/p1@refund-fix
```

## Notes

- Only Claude Code keeps a per-project memory directory; for other runtimes `sync` is a no-op while `status`/`distill` still work on the branch.
- A Codex conversation that agit starts (`agit new`, `agit run` or `agit resume` with `--as codex`) is given the branch's memory as an index when it starts: each file's name and first heading, and the folder of copies to read. The index states no decision itself; read the file it names before answering from it. agit starts that Codex outside its shared background server (`--no-daemon`, when the installed Codex has it), since only a conversation in the process agit started carries the launch to its hooks. A `codex` you start yourself gets no index.
- Files that hit the secret scanner are never collected or distilled; clean them first.
- `distill` reaches only the session's own repository's `main`, which its questions and result name; it doesn't carry memory into another repository, such as a team's. The `distill` manual shows how to add a file to another repository's main yourself.
- `distill` first reads the Hub's `main`. When it has changes this clone's `main` lacks, `distill` stops with nothing written and names `agit pull <owner>/<repo>@main`; after that pull a change made to the same file on main is merged or reported as a clash instead of missed. When the Hub can't be reached it warns and goes on. After landing it says, per file, whether it was carried as the branch has it or merged with main's changes. At a terminal, after the changes were confirmed one by one, it asks whether to publish main now (`agit push <owner>/<repo>@main`, default no); with `-y` or without a terminal it prints that command instead.
- Memory commands read the session's own branch in this clone: one only the Hub holds is refused as not in this clone, so a colleague's session is distilled on the computer that holds it. A command given no target lists this folder's conversations as `--into` targets. `status` and `diff` are the exception: without a target they read the folder's only conversation and name it on their first line (`target: <owner>/<repo>@<branch> (via this folder)`); with several, a terminal asks which, and anything else gets the list. `distill` and `sync` write, so they always need `--into` or `AGIT_SESSION`.
- The repository privacy policy is checked before the secret scanner. Memory is private by default;
  add an explicit pattern with `agit privacy policy allow-memory <pattern> --repo <owner/repo>`
  before a file can enter a branch. A branch restriction can narrow that allowlist, and rejected
  candidates remain eligible for a later retry after the policy changes.
- `status` labels policy-excluded changes as not collected and prints the authorization command
  for a missing allowlist entry. Eligible changes still need to pass content privacy checks.
- `memory.track = off` (`agit config`) turns collection off for this machine, on every path including `sync`.
- A session not started through agit has no baseline: `commit` only records one; `memory sync` collects every top-level file.

## How memory flows

- Memory flows by itself between the runtime's memory directory and the session branch (materialized at `new`/`resume`, collected at every `agit commit`). `main` only moves when you distill: at a milestone run `agit memory status`, then `agit distill` (or `agit memory distill <file>…`) to carry the facts worth sharing into `main`; `commit --milestone` and `push` remind you when files are pending.
