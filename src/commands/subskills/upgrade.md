---
name: agit-upgrade
description: Check for and update the AgentGit CLI.
---

# agit upgrade

## Synopsis

```bash
agit upgrade [--check]
```

## Options

| Option | Meaning |
|---|---|
| `--check` | Report available updates without installing |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit upgrade --check
agit upgrade
```

On user-facing CLI startup, agit also performs the same once-a-day best-effort check and prints a
new-version notice to stderr. It never installs automatically. JSON/quiet/CI invocations and the
internal `hooks`/`mcp` commands skip the check so machine-readable and integration output stays
unchanged.

Upgrading the CLI does not migrate or delete `~/.agit/repos`. Run `agit doctor` afterwards to verify runtime integrations.

After installing the CLI, `upgrade` runs the new executable's
`setup --skill --installed-only` to refresh existing Skills and remove versioned
legacy manuals, including `~/AGENTS.md`. This also runs when the CLI is already
current; `--check` never refreshes files. Native bundles are verified before
cleanup, and original instruction files are backed up beside the originals.
Hooks, MCP settings, project rules, and auto-push preferences are not refreshed.

An older CLI whose upgrade command only replaces the binary cannot run this
refresh automatically. After that upgrade, run
`agit setup --skill --installed-only` once with the new CLI. npm installation
already runs setup unless `AGIT_SKIP_SETUP=1` is set.
