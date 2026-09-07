---
name: agit-show
description: Display a session VIEW or selected AgentGit history content.
---

# agit show

## Synopsis

```bash
agit show [session] [options]
```

`--tui` opens the transcript browser for a session point or a complete turn such as
`owner/repo@work#2`. Bare branch names and qualified refs select the same evidence as line
output. It needs a terminal: in a pipe it exits with `Interactive` (8) rather than degrading
silently. File lines, file paths, and individual events require line output; combining
those selectors with an active `--tui` request exits with `Usage` (2).
Turn ranges are unsupported in either mode; select a complete turn with `<ref>#<turn>`.

Repository session selections render the saved VIEW, including when selected through `--agent`.
Add `--log-only` to render the complete saved LOG, including events removed from VIEW.
The selected point stays frozen; a missing or corrupt LOG is an error. This mode can read an
intact LOG even when the saved VIEW is corrupt. A complete turn selector still shows only that
turn, and Timeline drill-down remains turn-scoped. File lines, paths, individual events and
ranges cannot be combined with `--log-only`. Native session IDs retain the separately labelled
live transcript source. Without `--log-only`, a corrupt VIEW is an error and never broadens
the display to LOG. The LOG browser opens the selected frozen session point.
Saved records are parsed by their own runtime and source session, preserving interleaved
conversation order. An unsupported saved runtime is reported instead of guessed.

`--raw` emits the selected evidence as native JSONL: each stored envelope contributes its
`content` value in the same order, without envelope fields, headings or message truncation.
The default selects the frozen VIEW; `--log-only --raw` selects the complete LOG. Repository
JSONL is canonically serialized, so whitespace and object-key order need not match the live
source file. An explicit native ID emits that live file directly. A complete turn remains
scoped to that turn; an event or `:path` keeps its existing verbatim semantics.
Raw output rejects an explicit `--max-chars` and an active TUI. Automation that suppresses the
TUI can still use raw output. Global `--json` retains the unified CLI envelope and exposes
native values through its existing JSON/JSONL result representation; it is not raw stdout.
File-line points have no native transcript and reject `--raw`.

An explicit file-line ref such as `agit show owner/repo@main` prints that point's top-level tree
and recent commits. Use `agit show owner/repo@main:README.md` to read a selected file verbatim.

## Options

| Option | Meaning |
|---|---|
| `[session]` | Session ID, prefix, or ref; omitted targets require `AGIT_SESSION` and show its exact branch |
| `--agent <owner/agent>` | Restrict output to one local agent's sessions |
| `--log-only` | Render the complete LOG instead of the saved VIEW; a complete turn remains scoped to that turn |
| `--raw` | Emit native JSONL for the selected VIEW, LOG or turn, without headers or truncation |
| `--max-chars <count>` | Maximum characters per segment; default 2000 |
| `--tui` / `--no-tui` | Open or forbid the full-screen interface. `--tui` overrides the agent-session check but not `--json`, `-q`, `-y`, or `AGIT_TUI=0` |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit show @
agit show 132bf69f-22a --agent szh/p1 --max-chars 4000
agit show 132bf69f-22a --tui
```

With no argument, agit uses the exact branch named by `AGIT_SESSION`. Without that variable, name a session or fully qualified ref explicitly. `@` also requires `AGIT_SESSION`.
