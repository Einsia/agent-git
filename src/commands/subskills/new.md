---
name: agit-new
description: Create an empty session branch in an Agent repo and optionally start a runtime.
---

# agit new

## Purpose

Create a real session branch. The repo selects the context repository and the branch selects the session identity; workspace binding only supplies default routing and cannot replace an explicit repo.

## Synopsis

```bash
agit new [REPO] -b <branch> [options]
```

With no repo and no `-b` at a terminal, this opens a repo picker and then asks for the branch name on the normal screen. It stays non-interactive in pipes, in CI, and inside an agent session. It also does not open inside an unmanaged runtime session, where `new` is refused anyway unless `--fresh`.

## Options

| Option | Meaning |
|---|---|
| `[REPO]` | `<owner/name>`; when omitted, the session environment or this directory’s binding names the repo — a freshly `init`-ed directory qualifies |
| `-b, --branch <branch>` | New session branch name |
| `--from <ref>` | Start from an existing ref instead of an empty context |
| `--as <runtime>` | Runtime to start |
| `--cwd <dir>` | Runtime working directory |
| `--no-launch` | Create/materialize without starting |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface. `--tui` overrides the agent-session check but not `--json` / `-q` / `-y` |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit new szh/p1 -b feature-a
agit new szh/p1 -b review --from main --no-launch
agit new szh/p1 -b codex-fix --as codex --cwd ~/Projects/p1
```

Verify immediately:

```bash
git -C "$(agit repo path szh/p1)" show-ref --verify refs/heads/feature-a
```

If the repo does not exist, use `agit init` or `agit clone` first. Do not use `agit push` as a substitute for `new`.
