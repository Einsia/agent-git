---
name: agit-show
description: Display a session VIEW or selected AgentGit history content.
---

# agit show

## Synopsis

```bash
agit show [session] [options]
```

`--tui` opens the transcript browser. It needs a terminal: in a pipe it exits with `Interactive` (8) rather than degrading silently.

An explicit file-line ref such as `agit show owner/repo@main` prints that point's top-level tree
and recent commits. Use `agit show owner/repo@main:README.md` to read a selected file verbatim.

## Options

| Option | Meaning |
|---|---|
| `[session]` | Session ID, prefix, or ref; omitted targets require `AGIT_SESSION` and show its exact branch |
| `--agent <owner/agent>` | Restrict output to one local agent's sessions |
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
