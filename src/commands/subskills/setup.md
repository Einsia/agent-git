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
| `--runtime <all\|claude-code\|codex\|cursor\|opencode>` | Runtime to integrate, installed or not; plain `agit setup` integrates the runtimes installed on this computer (program on PATH, or configuration folder present) |
| `--all` | Integrate every runtime agit supports, installed or not |
| `--remove-skill` | Remove agit's Skill from `--runtime`'s runtime, or from every runtime, and keep it off; later setups and upgrades leave it out until `agit setup --skill --runtime <runtime>` |
| `--hooks` | Install or update runtime hooks |
| `--skill` | Install the progressive-disclosure AgentGit Skill bundle (`SKILL.md`, `VERSION`, and command references) in the selected runtime's native global Skill directory; does not expand the full guide into `AGENTS.md` |
| `--installed-only` | With `--skill`, refresh only existing native or legacy Skill installations; does not change hooks, MCP, project integration, or auto-push preferences |
| `--mcp` | Configure the MCP server |
| `--agents-md` | Write/update the AGENTS.md integration block at the top of the current Git repository; plain `agit setup` writes none, and outside a repository or in the home folder nothing is written |
| `--remove-agents-md [DIR]` | Remove agit's section from the `AGENTS.md` and `CLAUDE.md` of `DIR` (default: the current folder), keeping a backup beside each; writes nothing else |
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

Usage statistics are required when the selected Hub is `https://agent-git.com`
(or its `www` alias on the default HTTPS port). Setup does not ask for a telemetry
choice on that Hub. Self-hosted Hubs keep their optional setup choice and need an
explicit analytics destination. See `docs/telemetry.md` and
`src/telemetry/registry.json` for the collection policy and field inventory.
`--yes` skips the automatic-push question without changing that separate preference.

Claude hooks are merged into `~/.claude/settings.json`. Codex hooks are merged into
`$CODEX_HOME/hooks.json` (default `~/.codex/hooks.json`) only when `codex features list` reports the
hooks capability. Codex runs a hook only after the user approves its exact command, and nothing
from Codex is saved until then; agit does not grant that approval on the user's behalf. Setup leaves
an existing hook that runs the `agit` on `PATH` unchanged, so an approval survives another copy of
agit running setup. Setup, `agit doctor` and `agit project status` ask Codex (`codex app-server`,
`hooks/list`) and name the AgentGit hooks it will not run: not yet approved, changed since approval,
or disabled. The user approves them in Codex's startup hooks review or in `/hooks`.

Claude Code's MCP server is registered at user scope (`claude mcp add --scope user`), so every
folder sees it. A project-local `agit` entry is removed only when the user entry fully replaces it;
any other project-local entry overrides the user one in its folder, stays, and is named with the
command that removes it. Codex, OpenCode and Cursor receive the server in their user-level
configuration files.

The native Skill targets are:

- Claude Code: `~/.claude/skills/agit/`
- Codex: `$CODEX_HOME/skills/agit/` (default `~/.codex/skills/agit/`)
- OpenCode: `~/.config/opencode/skills/agit/`
- Cursor: `~/.cursor/skills/agit/`

Use `--agents-md` separately, inside the project, when you want the short, marked project integration block. A skill install also removes such a block from the home-level instruction files (`~/AGENTS.md`, `~/CLAUDE.md`, `~/.claude/CLAUDE.md`, the Codex and OpenCode `AGENTS.md`), since every conversation reads those; `agit doctor` reports one it finds. That includes the section setup wrote from 0.8.0 until 09-10, which told agents to run `agit status` at the start of every session, also where that setup doubled its markers; `agit upgrade` refreshes the skill and so removes it too.

Setup never edits a project's own instruction files by itself, since they are often committed. When a folder agit set up still holds that old section, `agit doctor` and `agit` name the file and the command that removes it, `agit setup --remove-agents-md DIR`; if Git tracks the file, commit the change so teammates' agents stop reading it too.

After verifying the native bundle, Skill setup removes complete, version-marked
legacy manuals from known runtime instruction paths and `~/AGENTS.md`, even when
setup runs in another directory. It preserves user text and unversioned project
rules, and saves the original beside the file as `AGENTS.md.agit-backup-<hash>`
before replacing it. The home manual is a Cursor entrypoint: a runtime-filtered
refresh preserves it until the Cursor bundle is verified. Incomplete markers and
fenced examples are left alone.
Run `agit setup --skill --installed-only` to repair an existing installation.
