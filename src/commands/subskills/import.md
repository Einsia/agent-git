---
name: agit-import
description: Adopt an existing runtime transcript into an Agent repo session branch.
---

# agit import

## Purpose

Import an existing Codex, Claude Code, or other runtime session into AgentGit. Import creates or binds a real `refs/heads/<branch>` and stores the transcript as context; a workspace binding does not select the correct repo automatically.

On a TTY, the zero-argument form opens the naming inbox for unmanaged sessions.
Select a session, choose its destination repo, and enter a branch name; the
screen leaves the alternate buffer before a separate lineage choice. Choose a
verified candidate, import independently, or cancel. Cancel is selected initially,
even when there is only one candidate. Explicit `--onto`, `--independent`, and
`--link-only` decisions use their existing claim and settlement checks.

## Synopsis

```bash
agit import [session] --into <owner/name@branch> [options]
agit import [session] --repo <owner/name> -b <branch> [options]
```

## Options

| Option | Meaning |
|---|---|
| `[session]` | Explicit runtime session ID or ID prefix; omitted targets require a choice in the interactive picker. `@` does not infer the current runtime session |
| `-n, --name <agent>` | Bare repo name for adoption; use `--into` to specify its owner and branch together |
| `--from <runtime>` | Source runtime, such as `codex` or `claude-code` |
| `--link-only` | Write the adoption link without importing/settling content |
| `--into <owner/name@branch>` | Explicit target Agent repo and session branch |
| `--repo <owner/name>` | Alias of `--into`; pair a repo-only value with `-b` |
| `-b, --branch <branch>` | Target session branch; recommended explicitly when creating/importing |
| `--propose-lineage` | Read-only local prefix report; requires a full native ID, `--from`, and qualified destination branch |
| `--independent` | Explicitly use the ordinary no-base import path; existing claim and settlement checks still apply |
| `--onto <ref>` | Attach a new branch at an existing ref; an existing branch must retain that ref in its first-parent history |
| `--privacy` | Adopt a privacy-redacted transcript copy (currently Claude Code only) |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface; machine-output flags still forbid it |

## Scenarios and examples

Use `--into <owner/repo>@<branch>`, or `--repo <owner/repo> -b <branch>`, to select the destination. Do not combine a branch in `--into` with `-b`. If the session ID is unknown, start with `agit status` or filter with `--from`. Use `--link-only` when adoption should be recorded but content should wait.

After `--link-only`, sign in and run `agit import <session-id> --from <runtime> --into <owner/repo>@<branch>`
to record the opening version. The offline link has no repository owner or branch claim;
`agit commit` cannot choose them. Session turns cannot be settled onto `main`.

```bash
agit import 132bf69f-22ab-4000-8000-000000000001 --from claude-code --repo szh/p1 -b fix-auth
agit import 132bf69f-22a --into szh/p1@imported --onto main
agit import 132bf69f-22a --link-only
agit import 132bf69f-22a --into szh/p1@pending --independent
```

After import, verify the real repository rather than trusting the summary:

```bash
git -C "$(agit repo path szh/p1)" show-ref --verify refs/heads/fix-auth
agit status
```

`--name` names a repo; it does not create a branch by itself. Prefer the qualified `--into` form when reusing a repository, especially in an organization namespace. Import does not change the parent shell's environment: subsequent commands need a full target or an explicitly set `AGIT_SESSION`.

## Inspect possible local lineage

Before adopting a session, inspect verified local prefixes:

```bash
agit import 132bf69f-22ab-4000-8000-000000000001 --from claude-code --into szh/p1@imported --propose-lineage --json
```

Preview requires the full native ID and an explicit destination branch. It does
not infer either identity from the cwd or `AGIT_SESSION`. It reads bounded native
bytes and existing local history without creating a repository, adoption link,
export cache, dictionary, or credential file, and without fetching objects.
OpenCode inspection uses a coherent read-only database transaction; SQLite can
maintain its ordinary WAL coordination sidecars without modifying application
rows. Unsupported or incomplete evidence is reported explicitly.

Inspecting an existing repository requires NUL-framed Git worktree output,
normally available in Git 2.36 or newer. Unsupported output reports
`git_worktree_format`; it is never parsed as ambiguous newline-delimited paths.
This capability requirement is local to discovery and selection. Explicit
`--onto`, `--independent`, and `--link-only` imports retain their ordinary checks.

A candidate command supplies `--onto` with the frozen commit. Choose a candidate
explicitly, or use the returned `--independent` command to import without choosing
a prior session base. Neither a singleton candidate nor an empty report selects
an action. Semantic comparison is unavailable, so no exact candidate does not
prove an unknown starting point.

Text actions preserve the selected directory, AgentGit home, and Hub. Unix output
uses POSIX shell syntax; Windows output starts a child PowerShell process and
requires PowerShell 7.3 or newer. The child preserves literal arguments and leaves
the parent shell's routing unchanged. Use `--json` when an argument contains a
terminal control character that cannot be displayed as a safe command.

`--propose-lineage` conflicts with `--onto`, `--independent`, `--link-only`,
`--privacy`, and `--name`. `--independent` conflicts with `--onto` and `--link-only`.
Versioned `--privacy` also requires `--onto` or `--independent`, so a preview cannot
create a redacted copy. Explicit `--link-only --privacy` keeps its offline copy-and-link
behavior.

## Choose before adoption

Without an explicit lineage decision, supply the full native ID, `--from`, and a
qualified destination branch. A call without an ID can list unmanaged native
identities and an explicit retry command; it does not adopt a discovered row. The command inspects only local evidence before
asking. Pipes, JSON, CI, agent sessions, quiet mode, and `--yes` cannot answer this
choice: they return exit code `8` with `operation: "choice_required"` and explicit
candidate and independent actions. The v2 envelope also carries these actions in
`fix`; v1 keeps its existing envelope. Running an action is an explicit new import,
subject to the ordinary current permission and lineage checks.

Interactive acceptance rereads the native source, original claim image, selected
repository, and destination ref. It repeats these checks under the branch and
claim locks before publishing. A changed observation refuses the choice and asks
for another inspection. A selected candidate remains its immutable commit even
if its displayed aliases move; its evidence must still match. External native
writers are not locked, so ordinary settlement checks still govern later changes.

Repeating an import into the same active, explicitly selected claim keeps the
ordinary idempotent settlement path. No hidden runtime or directory identity is
used to choose a destination.
