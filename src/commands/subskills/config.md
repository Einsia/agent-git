---
name: agit-config
description: Manage AgentGit user and repository configuration.
---

# agit config

## Purpose

Read, set, or remove user settings such as `hub.url`, `runtime.default`, `push.visibility`, `push.auto`, `commit.auto`, and `secrets.keystore`. `push.visibility` is the default for a first publish when neither a push flag nor a repo preference (`agit init --private`) says otherwise. `secrets.keystore` (`os | file`) says where the global registration key lives: the system credential store, or a private file under `AGIT_HOME/keystore/` for a machine with no desktop session (an SSH login, a CI runner; Unix only, and a backup of `AGIT_HOME` then carries the key); `AGIT_SECRETS_KEYSTORE` overrides it.

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
Existing settings retain their string representation, including booleans.
`privacy.encryption` returns JSON booleans for `effective` and `stored` (or null
when unset), with `scope: "creation_default"`.
An unset `commit.auto` has the effective default `true`; displaying it never
writes that default to disk.

## Encryption creation default

`privacy.encryption` accepts `true` or `false` and defaults to `false`: new Hub
repositories use ordinary publication, which needs no viewing password. Set it to
`true` to opt into encryption for every repository created afterward, or choose per
repository with `--encryption=true`. An existing repository's mode is fixed at
creation and does not follow later preference changes. Visibility and automatic
uploading are independent settings.

```bash
agit config privacy.encryption true
agit config privacy.encryption --json
agit config --unset privacy.encryption
```

Unsetting the preference restores the default of `false`. The global config
editor shows the effective value, stored value and source separately.

## Automatic publishing

`push.auto` defaults to `false`. Repository overrides live in the local Agent repo Git config and never travel in conversation history. An unset override follows the user preference, including later changes.

```bash
agit config --global push.auto true
agit config --repo alice/project push.auto false
agit config --repo alice/project push.auto
agit config --repo alice/project --unset push.auto
agit config --repo alice/project --list --json
```

`push.auto` supports a repository override. `privacy.encryption` is read-only at
repository scope: an established repository returns the authoritative Hub mode,
`source: "hub"`, `scope: "repository_mode"`, `fixed: true` and its immutable
`agent_id`. A purely local repository reports `scope: "creation_intent"` and
`fixed: false`; that intent applies only when creating a new destination.
A device-local RC repository reads the mode of its confirmed Hub destination;
`publication_repository` names that destination separately from the local repository.
Repository set/unset of `privacy.encryption` is refused. Mode lookup errors or
an unsupported Hub response are errors, never a disabled-mode default.

Repository `push.auto` JSON results distinguish `repository` from `inherited`
values. This changes the Agent repo preference, not the project code repository.

Once enabled, a successful settlement publishes its selected branch after releasing
local locks. Installed Stop hooks use the same behavior; empty settlements send nothing.
For an ordinary repository nothing else is required: automatic push needs no earlier
explicit push, passes the same inspection, identity and write-access checks as a manual
push, never accepts secret findings, and creates a missing destination with the
non-interactive visibility default. For an encrypted repository, first run
`agit push <owner/repo>@<branch>` to inspect and confirm the destination and mode; that
successful publication stores local authorization bound to the account, immutable
repository, URL, visibility, mode, privacy policy and current recipient. Changed consent
inputs require another explicit push, `--yes` cannot substitute for saved consent, and
automatic encrypted publication cannot create a destination. No automatic push promotes
a read-only checkout. A failed upload leaves the local commit intact; run an explicit push
to inspect and retry. RC uses the same authoritative mode and requires a durable backend
acknowledgement.
