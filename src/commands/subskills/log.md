---
name: agit-log
description: View Agent repo turn, merge, VIEW, and file history from a session perspective.
---

# agit log

## Synopsis

```bash
agit log [ref|owner/repo] [options] [-- <path>...]
```

With no arguments at a terminal, this opens a full-screen browser instead of printing. It stays text in pipes, in CI, and inside an agent session — the last is why you will normally not see it. `--no-tui` forces text; so does any of `--json`, `-q`, `-y`, or any narrowing option (`-n`, `--kind`, `--grep`, `--since`, `--oneline`, `--graph`, `--branches`).

## Options

| Option | Meaning |
|---|---|
| `[ref|owner/repo]` | Branch, tag, commit, `@`, or repo; the branch supplied through `AGIT_SESSION` when omitted |
| `-n, --limit <count>` | Maximum entries; default 20 |
| `--graph` | Show the branch graph |
| `--branches` | Group/show by branch |
| `--kind <turn\|merge\|view\|file>` | Filter by event kind |
| `--grep <text>` | Search messages/content |
| `--since <duration>` | Show only a recent period, such as `24h`, `7d`, or `4w` |
| `--oneline` | One-line summaries |
| `[-- <path>...]` | Filter by path |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface. `--tui` overrides the agent-session check but not `--json` / `-q` / `-y` |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

Turn rows show `events` and `ToolUse` in both text and Timeline. Events count the stored
LOG envelope records added by that turn, matching the records addressed by `#n.k`.
The `ToolUse` column counts only IR `EventKind::ToolUse`. `ToolResult` and `FileEdit`
are separate categories and are excluded, including native file calls classified as `FileEdit`.
The counts follow each record's source runtime and that turn's frozen LOG, including records
hidden from the current VIEW. Native records may use earlier messages in that LOG as context;
only calls anchored to the turn's added records count. Later commits cannot change an earlier
turn's projection. Filters and limits keep the original turn ordinals and counts.

File, VIEW and merge commits have no turn activity. Undeclared Git history shows `?` for
unavailable counts; missing or corrupt declared LOG evidence is an error, never a zero count.

## Examples

```bash
agit log @ --oneline -n 30
agit log szh/p1 --branches --graph
agit log @ --kind merge --grep "auth"
```

`agit log` is AgentGit context history. Use `git -C <project> log` for project-code history.
