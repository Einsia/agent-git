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

## Options

| Option | Meaning |
|---|---|
| `[session]` | Session ID, prefix, or ref; when omitted, show the latest settled session in the AgentGit repo bound to the current directory. If no repo is bound, refuse instead of falling back to the global newest session |
| `--agent <owner/agent>` | Restrict output to one local agent's sessions |
| `--tui` | Interactive terminal UI |
| `--max-chars <count>` | Maximum characters per segment; default 2000 |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface. `--tui` overrides the agent-session check but not `--json` / `-q` / `-y` |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit show @
agit show 132bf69f-22a --agent szh/p1 --max-chars 4000
agit show 132bf69f-22a --tui
```

With no argument, agit uses the AgentGit repo bound to the current directory. If no repo is bound, name a session or ref explicitly. `@` still means the current runtime context.
