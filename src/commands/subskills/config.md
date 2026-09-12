---
name: agit-config
description: Manage AgentGit user and repository configuration.
---

# agit config

## Purpose

Read, set, or remove user settings such as `hub.url`, `runtime.default`, `push.visibility`, `push.auto`, `commit.auto`, and `secrets.keystore`. `push.visibility` is the default for a first publish when neither a push flag nor a repo preference (`agit init --private`) says otherwise. `secrets.keystore` (`os | file`) says where the secret-filter key lives: the system credential store, or a private file under `AGIT_HOME/keystore/` for a machine with no desktop session (an SSH login, a CI runner; Unix only, and a backup of `AGIT_HOME` then carries the key); `AGIT_SECRETS_KEYSTORE` overrides it.

## Synopsis

```bash
agit config [<key> [<value>]]
agit config --list
agit config --unset <key>
```

## Options

| Option | Meaning |
|---|---|
| `<key>` | Configuration key; read it when no value is supplied |
| `<value>` | Value to write |
| `--unset` | Remove the key |
| `--list` | List all settings |
| `--json` | Return typed settings under `result.value`; no arguments lists every setting |
| `-y, --yes` | Skip confirmation |
| `-q, --quiet` | Reduce output |
| `-C, --directory <dir>` | Use the given directory |
| `--no-color` | Disable color |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface; machine-output flags still forbid it |
| `-h, --help`, `-V, --version` | Show help or version |

## Examples

```bash
agit config hub.url
agit config runtime.default codex
agit config commit.auto false
agit config secrets.keystore file   # on a machine with no desktop session
agit config --list
agit config --unset runtime.default
```

Configuration is not session metadata; changing it does not change an existing branch or runtime link.

On a TTY, the zero-argument form opens a full-screen editor. It labels each effective value as
coming from the environment, stored config, a built-in default, or no source, while showing the
stored value separately. Explicit arguments and non-interactive calls retain the command-line path.

JSON reads and successful writes distinguish `effective`, `stored`, and `source`
(`environment`, `stored`, `default`, or `unset`). When an environment override is
active, a successful write still reports that override as effective alongside
the newly stored value. `operation` identifies `list`, `get`, `set`, or `unset`.
Values retain the configuration's string representation, including booleans.
An unset `commit.auto` has the effective default `true`; displaying it never
writes that default to disk.

## Automatic publishing

`push.auto` defaults to `false`. Repository overrides live in the local Agent repo Git config and never travel in conversation history. An unset override follows the user preference, including later changes.

```bash
agit config --global push.auto true
agit config --repo alice/project push.auto false
agit config --repo alice/project push.auto
agit config --repo alice/project --unset push.auto
agit config --repo alice/project --list --json
```

Only `push.auto` currently supports `--repo`. Repository JSON results distinguish `repository` from `inherited` values. This changes the Agent repo preference, not the project code repository.

When enabled, a successful session settlement publishes its selected branch after releasing local locks. Installed Stop hooks use the same behavior. Empty settlements send nothing. Automatic pushes retain the normal identity, access, visibility and secret checks, and never accept a read-only clone promotion prompt. A failed upload leaves the local commit intact; run `agit push <owner/repo>@<branch>` to inspect and retry. The RC supervisor retains control of its own publication flow.
