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
| `ingest` | SessionStart | Register the current session according to the payload's `source`: `startup` with `AGIT_SESSION` claims that branch; `resume`, `clear`, and Claude `fork` register unmanaged sessions for explicit adoption; `compact` leaves the binding alone. Existing bindings win over inherited environment values. Claude Code, Codex and WorkBuddy receive the native session ID with the explicit destination and its `agit commit`/`agit push` commands, or the adoption command. Claude gets a default title only when its payload has no title, on events that support titles. |
| `settle` | Stop | Settle the turn that just ended. The target branch is resolved from the payload's `session_id` through the store link — **never from the environment**. A session that was never adopted is not settled. When Claude Code has not yet written the stopped turn's reply, the hook returns at once and a detached `agit hooks settle` settles the turn after the reply is written, so the last turn before `/exit` is saved too. |

## Options

| Option | Meaning |
|---|---|
| `--runtime <name>` | Which runtime is calling; inferred from the payload's `transcript_path` when omitted |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` does not change the hook stdin/log protocol |

## Notes

Both actions **always exit 0** — a failing hook must not disturb the session. `settle` is silent. SessionStart tells the model nothing: an agent that needs the conversation's state runs `agit`, which names it from the runtime's own session variable. In a folder that uploads by itself, every start (resume and `agit continue` included) returns one `systemMessage` for the person, naming the destination, who can read it there, and `agit project drop`. Otherwise SessionStart prints nothing, except to give a Claude Code conversation that an earlier agit renamed its own title back. To turn automatic settlement off:

```bash
agit config commit.auto false
```

No worktree scan runs in this hook, and no code files are restored. New conversations stay unnamed until the user selects their repo and branch through `agit import` or the TUI naming inbox.

Hook contracts: [Claude Code](https://code.claude.com/docs/en/hooks) and [Codex](https://learn.chatgpt.com/docs/hooks). Installed Codex hooks must be enabled and trusted in the runtime before they run.

The input schema is produced by the runtime hook; do not hand-forge it to claim another person's session.
