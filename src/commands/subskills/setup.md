---
name: agit-setup
description: Install AgentGit hooks, skill, MCP, AGENTS.md integration, or shell completion for local runtimes.
---

# agit setup

## Synopsis

```bash
agit setup [options]
```

## Options

| Option | Meaning |
|---|---|
| `--runtime <all\|claude-code\|codex\|cursor\|opencode>` | Runtime to integrate |
| `--hooks` | Install or update runtime hooks |
| `--skill` | Install the progressive-disclosure AgentGit Skill bundle (`SKILL.md`, `VERSION`, and command references) in the selected runtime's native global Skill directory; does not expand the full guide into `AGENTS.md` |
| `--mcp` | Configure the MCP server |
| `--agents-md` | Write/update the AGENTS.md integration block |
| `--completions <shell>` | Generate completion for a shell |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit setup --runtime codex --skill --mcp --agents-md
agit setup --runtime claude-code --hooks
agit setup --completions zsh
```

Run `agit doctor` after installation. Hooks may commit at Stop/turn boundaries, but ordinary CLI use still needs an explicit `push`.

The native Skill targets are:

- Claude Code: `~/.claude/skills/agit/`
- Codex: `$CODEX_HOME/skills/agit/` (default `~/.codex/skills/agit/`)
- OpenCode: `~/.config/opencode/skills/agit/`
- Cursor: `~/.cursor/skills/agit/`

Use `--agents-md` separately when you want the short, marked project integration block.
