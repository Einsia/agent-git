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
| `--installed-only` | With `--skill`, refresh only existing native or legacy Skill installations; does not change hooks, MCP, project integration, or auto-push preferences |
| `--mcp` | Configure the MCP server |
| `--agents-md` | Write/update the AGENTS.md integration block |
| `--completions <shell>` | Generate completion for a shell |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit setup --runtime codex --skill --mcp --agents-md
agit setup --runtime codex --hooks
agit setup --runtime claude-code --hooks
agit setup --completions zsh
```

Run `agit doctor` after installation. Hooks may commit at Stop/turn boundaries. Full interactive setup also asks whether to push settled turns automatically; the default is off. Use `--auto-push` or `--auto-push=false` to set that user preference explicitly. Repositories without a local override inherit it.

Setup first explains usage statistics and asks with a default Yes. Noninteractive
setup and `--yes` enable statistics with a visible notice. Existing opt-outs stay
disabled. Use `agit telemetry disable` to stop collection or `agit telemetry
schema` to inspect the field policies. Completion generation and dependency
postinstall defer this choice. `--yes` skips the automatic-push question without
changing that separate preference.

Claude hooks are merged into `~/.claude/settings.json`. Codex hooks are merged into
`$CODEX_HOME/hooks.json` (default `~/.codex/hooks.json`) only when `codex features list` reports the
hooks capability. Codex may ask the user to trust the installed commands before running them; agit
does not grant that trust on the user's behalf.

The native Skill targets are:

- Claude Code: `~/.claude/skills/agit/`
- Codex: `$CODEX_HOME/skills/agit/` (default `~/.codex/skills/agit/`)
- OpenCode: `~/.config/opencode/skills/agit/`
- Cursor: `~/.cursor/skills/agit/`

Use `--agents-md` separately when you want the short, marked project integration block.

After verifying the native bundle, Skill setup removes complete, version-marked
legacy manuals from known runtime instruction paths and `~/AGENTS.md`, even when
setup runs in another directory. It preserves user text and unversioned project
rules, and saves the original beside the file as `AGENTS.md.agit-backup-<hash>`
before replacing it. The home manual is a Cursor entrypoint: a runtime-filtered
refresh preserves it until the Cursor bundle is verified. Incomplete markers and
fenced examples are left alone.
Run `agit setup --skill --installed-only` to repair an existing installation.
