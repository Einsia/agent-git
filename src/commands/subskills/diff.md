---
name: agit-diff
description: Compare current state or frozen points in local Agent repositories.
---

# agit diff

## Synopsis

```bash
agit diff [RANGE] [options]
```

## Options

| Option | Meaning |
|---|---|
| `[RANGE]` | `A`, `A..B`, or `A...B`; omitted compares the working state of the local repository selected through `AGIT_SESSION` |
| `--turns` | Report settled turns added after the selected base; default mode |
| `--view` | Compare the VIEW sequence of two refs |
| `--files` | Compare shared-file text; it is not a file-name-only listing |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit diff
agit diff @#3..@#8 --turns
agit diff szh/p1@feature-a..szh/p1@main --view
agit diff szh/p1@feature-a..szh/p1@main --files
agit diff szh/p1@feature-a...other/p1@topic --turns
```

Zero-argument diff describes AgentGit state, not `git diff` in the project code repository.

Each endpoint can name its own local repository with `owner/repo@ref`. An unqualified right
branch, including a slash branch such as `topic/work`, belongs to the left repository. `@`
selects both the repository and branch of the current session; it requires session identity
and does not borrow a branch from another repository. Diff reads existing local repositories
without cloning or importing objects into either source.

A single `A` compares that point with the selected repository's `HEAD`.
`A..B` compares the selected points. `A...B` compares their verified common Git ancestor with
`B`, and the turn report also shows what `A` added after that ancestor. If the histories have
no common Git ancestor, diff says so and compares the explicit points. Multiple common
ancestors require an explicit base and a two-dot comparison. Damaged ancestry is an error.
Matching transcript content alone does not establish a Git ancestor.

Historical whole-commit selectors such as `#n`, `#-1`, `~n`, and annotated tags are supported.
Event, turn-range, and path selectors do not identify whole commits and are rejected as diff
endpoints. Use `--files` for shared-file contents and `--view` for the saved event sequence.
