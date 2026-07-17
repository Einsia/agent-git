# Versioning the harness (MCP + skills + config) as Workspace State

Status: implemented and shipped. Kept as the record of the design.

## Problem

`AgentGit State = Agent State + Environment State + Relations`. Today agit versions only the raw
**session dump** and the code. It does **not** version the *harness*: the MCP servers, skills,
slash-commands, hooks, and memory files that shape how the agent behaves. So when a session is
resumed, cloned, or synced on another machine, the tools and skills it assumed aren't there — the
reconstructed Workspace State is incomplete and can break (a session that called an MCP tool resumes
into a runtime that has no such tool).

The harness is part of **Agent State**. It must be captured, versioned, and re-applied like the rest.

## Model — a `harness/` tree in the Agent Store

```
.agit/agent/
  sessions/<rt>/…               ← already versioned (raw session dumps)
  harness/
    claude-code/
      project/                  ← scope: this repo
        mcp.json                  project .mcp.json
        settings.json             hooks / permissions (whitelisted keys only)
        skills/<name>/SKILL.md
        commands/<name>.md
        CLAUDE.md
      user/                     ← scope: this machine's user config, filtered to what this repo used
        mcp.json                  user-scope MCP servers referenced by this project's sessions
        skills/<name>/SKILL.md    user-scope skills referenced by this project's sessions
    codex/
      project/                    AGENTS.md, project MCP
      user/                       ~/.codex/config.toml [mcp_servers] (filtered), user skills
    manifest.json               ← what was captured, from where, and which secrets were redacted
```

The harness is versioned by git like `sessions/`. A `WorkspaceRevision` pins the harness revision
alongside the agent/session revision, so a pairing reconstructs the *whole* Agent State.

## Decisions (settled)

1. **Scope: both project and user.** Project-scope harness is captured wholesale. User-scope
   (`~/.claude/skills`, user MCP servers, `~/.codex/config.toml`) is captured **filtered to what this
   project actually used** — a user may have dozens of global servers/skills; we only version the ones
   this repo's sessions referenced, plus any project-required ones. `manifest.json` records origin +
   scope for each captured item so restore knows where it came from.
2. **Secrets: redact and prompt.** MCP configs carry tokens (`env` values, `Authorization`/header
   values, `*_API_KEY`, `*_TOKEN`). Capture does **field-aware redaction**: keep the server *shape*
   (command, args, the *names* of the env vars/headers it needs), replace each secret *value* with a
   placeholder, and record the required-secret name in `manifest.json`. On apply, agit **prompts** for
   each required secret (or reads it from local env / a local secret store) and fills it in. Secrets
   never enter the store. The existing commit/push scanner remains the backstop.
3. **Apply: ask.** `resume` / `clone` **ask** before writing the harness back ("Apply captured harness?
   N MCP servers, M skills, K secrets to provide"). Applying rewrites local `.mcp.json` / `.claude/`,
   which needs explicit consent — never silent.

## Capture — folded into `snap`

`agit -a snap` captures the harness in the same pass as the session dump, via a **strict whitelist**
(never sweep caches, `settings.local.json`, logs, node_modules-style trees):

- **Claude Code** — project: `.mcp.json`, whitelisted keys of `.claude/settings.json`, `.claude/skills/`,
  `.claude/commands/`, `CLAUDE.md`. User: the user-scope MCP servers and skills the project's sessions
  referenced (cross-referenced against session tool-use records).
- **Codex** — project: `AGENTS.md`, project MCP. User: `~/.codex/config.toml` `[mcp_servers]` filtered to
  referenced servers.

Each captured file is run through field-aware redaction before it lands in the working tree, and the
redactions are logged to `manifest.json`.

`agit -a snap --no-harness` skips it; `agit -a snap --harness-only` captures just the harness.

## Restore — `resume` / `clone` apply-with-merge

On `resume`/`clone`, after asking (decision 3), agit **union-merges** the captured harness into the
local one:

- MCP servers, skills, commands: **union** — add what's missing, keep local, and **flag** a real
  conflict (same server name, different definition) for the human rather than clobbering.
- For every redacted secret in `manifest.json`, prompt (or pull from env / local secret store) and
  write the real value into the applied config only.
- `settings.json`: merge whitelisted keys; never import hooks silently without showing them (a hook is
  executable — applying a teammate's hook runs their command).

## Sync — mechanical, unlike the session merge

Harness files are text/JSON/TOML, so `agit -a sync` reconciles them with a **plain union/text merge**,
not the live-agent dialogue. It surfaces only genuine conflicts (two different definitions of the same
MCP server, divergent `CLAUDE.md` sections). This runs *beside* the session dialogue: sync reconciles
the *reasoning* by dialogue and the *harness* by merge.

## Security notes

- Field-aware redaction is the load-bearing safety property. It must be a **default-deny** allowlist of
  which fields survive verbatim, not a blocklist of known-secret field names — an unknown secret-bearing
  field must redact by default.
- Applying a captured harness can install **hooks and MCP servers that execute commands**. Apply must
  show what will run and require consent (decision 3 covers the gate; the display of executable content
  is part of it).
- User-scope capture must never widen exposure: only items *this project referenced* leave the machine,
  so an unrelated global server's token can't ride along.

## Build plan

1. `harness/` tree + `manifest.json` schema + field-aware redactor (with tests: a config with a token
   must redact to a placeholder and record the required-secret name).
2. Claude Code capture (project scope) in `snap`.
3. Restore-with-ask + union-merge + secret prompt in `resume`.
4. User-scope capture (filtered by session references).
5. Codex capture/restore parity.
6. `sync` harness union-merge.

## Open

- Exact whitelist of `settings.json` keys worth versioning (hooks: version but never auto-apply).
- How "referenced by this project's sessions" is computed for user-scope filtering (scan session
  tool-use records for MCP tool names / skill invocations).
- Local secret store integration vs prompt-only for the first cut (prompt-only is fine to start).
