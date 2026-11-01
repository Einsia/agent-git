---
name: agit-remote
description: List the Hubs that hold an Agent repo, offline.
---

# agit remote

## Purpose

List the Hubs that hold a repo's publications, as this computer records them. Nothing is sent and nothing changes: the list comes from the checkout's pinned Hub identity and its `origin` URL, and the account from the credentials saved for that Hub. A checkout holds one pinned Hub, so the list is empty (never published) or `origin`, which is also the upstream that `agit push` and automatic upload use. The address is the repository's page on that Hub.

## Synopsis

```bash
agit remote [repo] [-v]
```

## Options

| Option | Meaning |
|---|---|
| `[repo]` | `<owner/name>`; default: the session's repo (`AGIT_SESSION`), else the repo this folder saves to |
| `-v, --verbose` | Also show the repo's address on that Hub, the account signed in there (or the `agit login --hub` command to sign in), and the upstream |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` returns `repository` and `remotes` (`name`, `hub`, `address`, `account`, `upstream`) |

## Examples

```bash
agit remote -v
agit remote alice/notes
```
