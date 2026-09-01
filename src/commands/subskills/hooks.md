---
name: agit-hooks
description: Hidden runtime-hook entry: register the current session (SessionStart) and settle the turn that just ended (Stop).
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
| `ingest` | SessionStart | Register the current session according to the payload's `source`: `startup` with `AGIT_SESSION` in the environment claims that branch; `resume` and `clear` only register the session as a candidate for adoption; `compact` leaves the binding alone. It also writes the session's real binding back into the runtime's session environment. |
| `settle` | Stop | Settle the turn that just ended. The target branch is resolved from the payload's `session_id` through the store link — **never from the environment**. A session that was never adopted is not settled. |

## Options

| Option | Meaning |
|---|---|
| `--runtime <name>` | Which runtime is calling; inferred from the payload's `transcript_path` when omitted |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` does not change the hook stdin/log protocol |

## Notes

Both actions are **always silent and always exit 0** — a failing hook must not disturb the session. `agit config set commit.auto false` turns automatic settlement off.

The input schema is produced by the runtime hook; do not hand-forge it to claim another person's session.
