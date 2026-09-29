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
agit init [<name>] [--seed] [--private] [--encryption=true|false] [--no-bind | --rebind] [--auto-push[=<true|false>]]
```

## Options

| Option | Meaning |
|---|---|
| `<name>` | Agent repo name; omitted means a prompt when agit may ask (see below), with the directory name only a suggestion |
| `--seed` | Confirm and copy project `AGENTS.md`, `CLAUDE.md`, `.claude/skills/`, and similar assets into `main` |
| `--private` | Record in the repo that the first `agit push` publishes private (`--public` at push time overrides) |
| `--encryption=true\|false` | Record local creation intent for a new Hub identity; an existing Hub mode is fixed |
| `--no-bind` | Create the repo without binding the current directory |
| `--rebind` | Bind this directory even if it is already bound to another repo (refused otherwise) |
| `--auto-push[=<true\|false>]` | Set this repository's automatic publishing; the value needs `=`. Omitted: inherit the user preference |
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
agit init p1 --auto-push=false
```

If `szh/p1` already exists, do not run `init` again. Use `new` for a new session in that repo and `import` for an existing runtime conversation.

## Encryption creation intent

Encryption intent stays in local Git config and is separate from the Hub's mode.
Omitting `--encryption` uses the user creation default when a new remote identity
is created. An empty clone already has a fixed Hub mode; a conflicting selection
is refused before writing the scaffold. The init wizard offers an explicit choice
or inheritance from `privacy.encryption`. This choice does not enable uploads.

## Automatic publishing preference

An interactive repository creation asks whether to inherit the user preference, enable automatic pushing, or disable it for this repository. Use `--auto-push` or `--auto-push=false` to choose explicitly in scripts and agents; omission inherits the user setting. The value must be attached with `=`: `agit init --auto-push false` would read `false` as the repository name, so a boolean word right after a bare `--auto-push` is refused with exit code `2`.

Inside an agent session (`AGIT_SESSION` or a runtime session variable such as `CODEBUDDY_SESSION_ID`), with `--json`, or unless stdin, stdout and stderr are all terminals, agit asks nothing: not this question, the repository name, or the sign-in menu. In CI it does not ask this question or the repository name either. An unasked question inherits the user preference and, once the repository is created, prints one line naming it and the `agit config --repo` command that overrides it; a missing name is an error with exit code `8`. The question is asked only for a repository this command creates. Change or clear the override later with `agit config --repo <owner/repo> push.auto <true|false>` or `agit config --repo <owner/repo> --unset push.auto`.

Enabling automatic pushing while signed out enters the login flow before creating the repository. Where nobody can be asked, that flow prints a login link for the human and the `agit login --complete` command, and init stops without creating anything; run it again once signed in, or pass `--auto-push=false`. Creating the repo itself does not upload a session; automatic publication follows successful session settlement.

## Browser password onboarding

For initial agent-assisted setup, use `--auto-push=false` while login, import, and viewing
password setup are in progress. Follow `agit privacy init OWNER/REPO --browser --json --yes`
in the privacy subskill: show the setup link, wait for the user, rerun with the same explicit
Hub and repository, and push only after API-confirmed readiness. Enable repository `push.auto`
afterward only if the user selected it. Browser setup does not change the upload preference.
