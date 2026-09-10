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
It prints the shared-file working patch and inspects pending native activity for the exact
branch in `AGIT_SESSION`, even when the repository checkout is on another branch. Inspection
does not settle turns, migrate storage, fetch history, or check for CLI updates.

The pending summary verifies committed native records or the recorded materialization digest
and source commit before counting appended records. Events count native records, while tool
calls count `ToolUse` events. User turns include both newly started turns and existing turns
with appended activity. The native transcript is parsed with its complete context so retained
compaction history is not counted again. Unclassified native activity makes the semantic counts
explicit lower bounds; an incomplete trailing record is reported separately.

Missing or ambiguous local claims, unreadable transcripts, rewritten or truncated prefixes,
and stale materialization evidence produce an unavailable summary and a precondition exit.
They never imply that no work is pending. OpenCode uses a bounded read-only database snapshot
without refreshing its materialization cache. It compares native identities against the latest
saved LOG occurrences and reports added, updated, and missing records separately. Streaming
text and completion of an existing tool are updates, not newly started turns or tool calls.
User messages are counted once across their text parts; assistant activity follows explicit
parent-message links. Missing parents or unmodeled activity make semantic counts lower bounds.
Missing saved records remain visible and never imply a clean snapshot. This comparison does
not promise that settlement will accept a native rewrite of already saved evidence.

A resumed OpenCode session has reminted native identities. Its current prefix must still
match the recorded materialization digest and source tip before new rows can be compared.
A changed prefix cannot be reconstructed from that digest or from the old VIEW, so this case
is explicitly unavailable. Both JSON envelope versions retain the text result format.
Repositories configured for lazy object fetching or clean/process filters are refused before
inspection, and filesystem-monitor callbacks are disabled for the shared-file patch.
The same check includes initialized submodules and registered checkouts. Git environment
variables that redirect the repository, index, or object store must be cleared first.

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
