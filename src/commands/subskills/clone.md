---
name: agit-clone
description: Fetch a local checkout of an Agent repo.
---

# agit clone

## Purpose

Fetch an Agent repo from the Hub into `~/.agit/repos/<owner>/<name>`, including local branches and remote-tracking refs. The checkout is read-only by default, does not start a runtime, and normally binds the current directory.

Without a target, the current directory must be a code Git repo with an `origin`; the CLI asks the Hub to reverse-map that code remote to an Agent repo.

## Synopsis

```bash
agit clone [<owner/repo[@version-or-branch]>]
```

## Options

| Option | Meaning |
|---|---|
| `<owner/repo[@ref]>` | Agent repo to clone; `ref` may be an `agit-<sha>` version or branch |
| `--mine` | Copy into your own namespace so it can be pushed |
| `--name <name>` | Name for a `--mine` copy; invalid without `--mine` |
| `--no-bind` | Do not bind the current directory |
| `--rebind` | Bind this directory even if it is already bound to another repo (refused otherwise) |
| `-y, --yes` | Skip confirmation |
| `-q, --quiet` | Reduce output |
| `-C, --directory <dir>` | Use the given directory |
| `--no-color` | Disable color |
| `--json` | Emit the unified CLI JSON envelope |
| `-h, --help`, `-V, --version` | Show help or version |

## Examples

```bash
agit clone alice/notes
agit clone alice/notes@refund-fix
agit clone alice/notes --mine --name notes
agit clone alice/notes --no-bind
```

`clone` fetches and optionally binds. Use `run` or `resume` to run a ref.

## Automatic publishing preference

An interactive repository creation asks whether to inherit the user preference, enable automatic pushing, or disable it for this repository. Use `--auto-push` or `--auto-push=false` to choose explicitly in scripts; omission inherits the user setting. Change or clear the override later with `agit config --repo <owner/repo> push.auto <true|false>` or `agit config --repo <owner/repo> --unset push.auto`.

Fetching an existing clone preserves its preference unless an explicit override is supplied. Making a local copy with `--mine` also offers the preference.
