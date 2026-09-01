---
name: agit-push
description: Publish existing local Agent repo refs to the Hub; it never creates a session branch.
---

# agit push

## Purpose

Publish refs that already exist locally. `push` is not a replacement for `new`, `fork`, or `import`, and it does not promise an automatic push for every turn.

## Synopsis

```bash
agit push [repo] [options]
```

## Options

| Option | Meaning |
|---|---|
| `[repo]` | `<owner/name>`; context resolution when omitted |
| `-b, --branch <branch>` | Branches to publish; repeatable |
| `--all` | Publish all local branches/refs |
| `--private` | Publish privately |
| `--public` | Publish publicly |
| `--dry-run` | Scan and show the plan without uploading |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

Visibility is settled once, at first publish. Without `--private` / `--public`, push takes the repo preference recorded by `agit init --private`, then the global `push.visibility` (`public` or `private`; `ask` means ask), and otherwise asks on a TTY (non-interactive runs default to private). `--dry-run` prints which of these applies.

## Examples

```bash
agit push szh/p1 -b feature-a
agit push --all
agit push szh/p1 -b feature-a --dry-run
```

A secret scan runs before publishing. If `refs/heads/<branch>` is missing, create it with `new`, `import`, or `fork` and verify it first. Push does not create a repo or branch from cwd.
