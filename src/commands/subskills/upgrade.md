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

On eligible user-facing CLI startup, production builds check for updates with a daily cache and
a short request timeout. A newer version prints a notice to stderr, including in pipes, CI, and
JSON mode. JSON stdout remains one complete envelope; the startup notice is separate from
`diagnostics.stderr` inside that envelope. The CLI never installs an incidental update without
an explicit terminal confirmation.

A human terminal waits at `Update agit now? [y/N]`. Enter or `n` skips installation; `y` runs
the existing upgrade and skill-refresh flow before restarting the requested command. Upgrade
output also goes to stderr. An unavailable update or failed installation does not prevent the
original command from running. After an accepted update attempt, the CLI restarts with the
original arguments through the installation path and skips the repeated startup check. This
uses the installed version even if installation succeeded but skill refresh failed.

JSON, CI, agent sessions, `--yes`, `--no-tui`, or redirected standard streams only receive the
notice and never wait for input. `--tui` can explicitly opt an agent terminal into the prompt;
JSON, CI, and redirected streams still prevent prompting. `--quiet` / `AGIT_QUIET` suppress both
the check and notice. Local inspection, search, scoped review, and internal `hooks`/`mcp` paths
retain their startup exclusions.

Upgrading the CLI does not migrate or delete `~/.agit/repos`. Run `agit doctor` afterwards to verify runtime integrations.

Prebuilt binaries carry Git and Git LFS inside the executable, so replacing that
single file upgrades the bundled runtime even through an older self-updater.
The runtime is extracted into `$AGIT_HOME/git-runtime` (default
`~/.agit/git-runtime`) on first use and reused by content hash. Different payloads
keep separate caches so existing processes can finish. Upgrades do not change
your global Git profile or shell PATH. `AGIT_USE_SYSTEM_GIT=1` selects your system
Git and Git LFS instead.

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
