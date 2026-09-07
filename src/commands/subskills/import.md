---
name: agit-import
description: Adopt an existing runtime transcript into an Agent repo session branch.
---

# agit import

## Purpose

Import an existing Codex, Claude Code, or other runtime session into AgentGit. Import creates or binds a real `refs/heads/<branch>` and stores the transcript as context; a workspace binding does not select the correct repo automatically.

On a TTY, the zero-argument form opens the naming inbox for unmanaged sessions.
Select a session, choose its destination repo, and enter a branch name; the
screen then leaves the alternate buffer before the ordinary import path runs.
Explicit arguments and non-interactive calls retain the command-line path.

## Synopsis

```bash
agit import [session] --repo <owner/name> -b <branch> [options]
```

## Options

| Option | Meaning |
|---|---|
| `[session]` | Explicit runtime session ID, ID prefix, or transcript path; omitted targets require a choice in the interactive picker. `@` does not infer the current runtime session |
| `-n, --name <agent>` | Repo name only when a new Agent repo must be created |
| `--from <runtime>` | Source runtime, such as `codex` or `claude-code` |
| `--link-only` | Write the adoption link without importing/settling content |
| `--repo <owner/name>` | Target Agent repo; recommended explicitly |
| `-b, --branch <branch>` | Target session branch; recommended explicitly when creating/importing |
| `--onto <ref>` | Attach imported content to an existing ref |
| `--privacy` | Adopt a privacy-redacted transcript copy (currently Claude Code only) |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface; machine-output flags still forbid it |

## Scenarios and examples

Use `--repo` and `--branch` to import an external runtime into an existing repo. If the session ID is unknown, start with `agit status` or filter with `--from`. Use `--link-only` when adoption should be recorded but content should wait.

After `--link-only`, sign in and run `agit import <session-id> --into <owner/repo>@<branch>`
to record the opening version. The offline link has no repository owner or branch claim;
`agit commit` cannot choose them. Session turns cannot be settled onto `main`.

```bash
agit import 132bf69f-22a --from claude-code --repo szh/p1 -b fix-auth
agit import /tmp/transcript.jsonl --repo szh/p1 -b imported --onto main
agit import 132bf69f-22a --link-only
agit import 132bf69f-22a --into szh/p1@pending
```

After import, verify the real repository rather than trusting the summary:

```bash
git -C "$(agit repo path szh/p1)" show-ref --verify refs/heads/fix-auth
agit status
```

`--name` names a repo; it does not create a branch in the current repo. When the target repo exists, use `--repo`.
