---
name: agit-hooks
description: "Hidden runtime-hook entry: register the current session (SessionStart) and settle the turn that just ended (Stop)."
---

# agit hooks (hidden)

## Purpose

Called by the runtime hooks that `agit setup --hooks` installs; it is not the normal human entry point for session management.

It answers a question nothing else in agit can: **which session is running right now.** `AGIT_SESSION` only says which session this process was started for — once the user switches sessions inside the runtime's own TUI, it is stale (`/clear` mints a new session id, `/resume` moves to the resumed session's id), while the hook payload carries the new `session_id`.

## Synopsis

```bash
agit hooks ingest < hook.json    # SessionStart
agit hooks settle < hook.json    # Stop
```

## Subcommands

| Subcommand | Event | Meaning |
|---|---|---|
| `ingest` | SessionStart | Register the current session according to the payload's `source`: `startup` with `AGIT_SESSION` claims that branch; `resume`, `clear`, and Claude `fork` register unmanaged sessions for explicit adoption; `compact` leaves the binding alone. Existing bindings win over inherited environment values. Both runtimes receive the explicit destination or adoption guidance. Claude gets a default title only when its payload has no title, on events that support titles. |
| `settle` | Stop | Settle the turn that just ended. The target branch is resolved from the payload's `session_id` through the store link — **never from the environment**. A session that was never adopted is not settled. |

## Options

| Option | Meaning |
|---|---|
| `--runtime <name>` | Which runtime is calling; inferred from the payload's `transcript_path` when omitted |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` does not change the hook stdin/log protocol |

## Notes

Both actions **always exit 0** — a failing hook must not disturb the session. `settle` is silent. A successful SessionStart returns one structured hook response; Codex receives context without Claude's title field. A user's existing title survives resume. Claude ignores titles on `clear`, so that event only receives context. Compact and unknown entry events stay silent. To turn automatic settlement off:

```bash
agit config commit.auto false
```

Native startup or `/resume` into an adopted session also receives its last settled `cwd_state` summary when available, even outside a bound workspace. This includes Codex sessions materialized by `agit resume`; the hook adds context without overriding configured developer instructions. This is historical evidence, not current Git status or proof of the same machine. No worktree scan runs in this hook, and no code files are restored. New conversations stay unnamed until the user selects their repo and branch through `agit import` or the TUI naming inbox.

Hook contracts: [Claude Code](https://code.claude.com/docs/en/hooks) and [Codex](https://learn.chatgpt.com/docs/hooks). Installed Codex hooks must be enabled and trusted in the runtime before they can deliver context.

The input schema is produced by the runtime hook; do not hand-forge it to claim another person's session.
