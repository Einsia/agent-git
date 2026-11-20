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
| `--installed-only` | With `--skill`, refresh existing native or legacy Skill installations; with `--hooks`, repair existing hooks naming this executable without installing new ones. Does not change MCP, project integration, or auto-push preferences |
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

Claude hooks are merged into the selected profile's `settings.json` (`CLAUDE_CONFIG_DIR`,
normally `~/.claude`). Codex hooks are merged into
`$CODEX_HOME/hooks.json` (default `~/.codex/hooks.json`) only when `codex features list` reports the
hooks capability. Codex runs a hook only after the user approves its exact command, and nothing
from Codex is saved until then; agit does not grant that approval on the user's behalf. Setup leaves
an existing hook that runs the `agit` on `PATH` unchanged, so an approval survives another copy of
agit running setup. Setup, `agit doctor` and `agit project status` ask Codex (`codex app-server`,
`hooks/list`) and name the AgentGit hooks it will not run: not yet approved, changed since approval,
or disabled. The user approves them in Codex's startup hooks review or in `/hooks`.

## Hook installation and controls

The global npm installer and create-agit normally run setup after installing
the executable. npm may hide setup's output or block its install script. A local
npm dependency/prefix installation or npm exec does not run the global setup
path. `AGIT_SKIP_SETUP=1` skips setup and its reminders; it does not disable an
already-installed hook or skip other installation work.

Bare setup selects hooks, Skill and MCP for installed runtimes. An ordinary
setup reminder previews changes and asks before installing; an unattended
reminder only reports what is missing. `agit upgrade` refreshes installed Skills
and can set up newly installed runtimes after an earlier setup, respecting
declined setup and Skill opt-outs. Existing Claude RC integration may separately
receive the SessionStart inbox relay during ordinary startup; that migration
does not add Stop and respects Claude's global hook-disable setting.

Setup names the hook configuration before writing and reports `updated` or
`already configured`, with the selected file and applicable actions:

| Event | AgentGit action | Purpose |
|---|---|---|
| SessionStart | `hooks ingest` | Connect session state and show capture/upload notices |
| Stop | `hooks settle` | Save eligible completed turns; upload follows folder/repository settings |
| Claude on Unix: PermissionRequest | `hooks native-permission` | Relay remote-control permission requests while the native prompt remains available |
| Claude on Unix: SessionStart | `hooks relay` | Receive remote-control messages asynchronously |

Codex commands include `--runtime codex` and still need your approval. WorkBuddy
uses the common SessionStart/Stop actions; Gemini uses SessionStart, AfterAgent
and SessionEnd. Other runtimes use their native plugin/extension integration.
The output lists configured actions, not proof that the runtime executed them.

| Intent | Action | Scope |
|---|---|---|
| Inspect configured hooks | Open only the hook entries in the reported file; use Codex's `/hooks` for its review and controls | Claude's `agit doctor` row checks configuration presence only, not whether hooks are enabled, executed or saved successfully |
| Pause this folder's automatic capture | `agit off [DIR]` | Keeps hooks/history; explicitly linked sessions can still save locally |
| Stop automatic turn saving | `agit config --global commit.auto false` (restore with `true`) | Keeps hooks, SessionStart and explicit commits; does not undo queued work or prevent every hook operation |
| Stop a hook invocation | Disable/remove only that AgentGit entry in the runtime's hook configuration | Preserve other hooks and surrounding configuration; a later full setup can add the entry again |

There is no `agit setup --remove-hooks` option. `--remove-skill` does not remove
Claude/Codex hooks. Do not share the whole runtime configuration to troubleshoot
a hook. Installing hooks does not enroll an arbitrary folder for upload.

Stop saves conversation history, not a code commit. It can wait on local work
or upload in synchronous paths; its detached Claude reply wait is not a deadline
for the whole hook. The relay's async setting and permission timeout are separate
from Stop. See the hooks reference for event and failure behavior.

## Other integrations

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

## Skill installation layout

`agit setup --skill` installs the same progressive-disclosure bundle for every
supported runtime. Each target is a real Skill directory containing one
entrypoint and a reference for every top-level command:

```text
<runtime skill root>/agit/
├── SKILL.md
├── VERSION
└── references/commands/<command>.md
```

The global target directories are:

| Runtime | Directory |
|---|---|
| Claude Code | `~/.claude/skills/agit/` |
| Codex | `$CODEX_HOME/skills/agit/` (default `~/.codex/skills/agit/`) |
| OpenCode | `~/.config/opencode/skills/agit/` |
| Cursor | `~/.cursor/skills/agit/` |

The runtime should load `SKILL.md` first and read only the reference needed for
the current command or scenario. `--skill` no longer expands the full manual in
`AGENTS.md`; use the separate `agit setup --agents-md` option when a project
needs the short, marked session-integration block. Re-running setup replaces
only AgentGit-owned Skill files, removes stale `references/commands/*.md`, and
does not overwrite user content. It also removes a version-marked legacy inline
Skill block from older releases while preserving surrounding `AGENTS.md` text.

The English command references are embedded at build time from
`src/commands/subskills/*.md`; read the installed reference when exact
arguments, scenarios, or examples are needed.
