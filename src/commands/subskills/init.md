---
name: agit-init
description: Create a local Agent repo and its main file line.
---

# agit init

## Purpose

Create a new local Agent repo:

```text
~/.agit/repos/<owner>/<name>
```

Initialize the `main` file line and the `AGENTS.md`, `memory/`, and `skills/` scaffold. The current directory is bound to the repo by default. `init` does not create a session or import the current conversation.

On a TTY, the zero-argument form opens a full-screen wizard for the explicit name, binding, and
item-by-item seed choices. It leaves the alternate screen before the ordinary init path writes or
prints anything. Explicit options and non-interactive calls retain the command-line path.

## Synopsis

```bash
agit init [<name>] [--seed] [--private] [--no-bind | --rebind]
```

## Options

| Option | Meaning |
|---|---|
| `<name>` | Agent repo name; omitted means interactive prompt, with the directory name only a suggestion |
| `--seed` | Confirm and copy project `AGENTS.md`, `CLAUDE.md`, `.claude/skills/`, and similar assets into `main` |
| `--private` | Record in the repo that the first `agit push` publishes private (`--public` at push time overrides) |
| `--no-bind` | Create the repo without binding the current directory |
| `--rebind` | Bind this directory even if it is already bound to another repo (refused otherwise) |
| `--json` | Emit the unified CLI JSON envelope |
| `-y, --yes` | Skip seed confirmations |
| `-q, --quiet` | Reduce output |
| `-C, --directory <dir>` | Run in the given directory |
| `--no-color` | Disable color |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface; machine-output flags still forbid it |
| `-h, --help`, `-V, --version` | Show help or version |

## Examples

```bash
cd /Users/me/Projects/p1
agit init p1
agit init p1 --seed
agit init p1 --no-bind
```

If `szh/p1` already exists, do not run `init` again. Use `new` for a new session in that repo and `import` for an existing runtime conversation.
