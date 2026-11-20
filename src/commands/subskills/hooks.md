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
| `settle` | Stop | Save an eligible completed turn. The hook uses the payload's native session identity and validates its explicit claim or enrolled project capture; inherited `AGIT_SESSION` does not override that identity. Eligible project capture can handle sessions without a prior explicit import. A managed link can settle directly. Claude's missing-reply path can detach a settlement process when that launch succeeds. |
| `native-permission` | Claude on Unix: PermissionRequest | Relay a permission request to an authorized remote controller while Claude's native prompt remains available |
| `relay` | Claude on Unix: SessionStart | Receive remote-control inbox messages asynchronously, with output redirected away from model context |

## Options

| Option | Meaning |
|---|---|
| `--runtime <name>` | Which runtime is calling; inferred from the payload's `transcript_path` when omitted |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` does not change the hook stdin/log protocol |

## Notes

Ordinary hook errors are generally contained so they do not disturb the runtime;
Archive settlement can return a nonzero failure. Exit zero alone does not prove
that a turn was saved or uploaded. A failed automatic upload can produce a
person-facing `systemMessage` at Stop or the next SessionStart, with a fixed
reason. SessionStart also reports applicable capture destinations and held
history, and managed Codex startup can receive its team-memory index as context.

Stop can wait on synchronous local work or automatic upload. The detached
Claude reply wait bounds only that wait, not the whole hook. The relay's async
setting and the permission hook's timeout do not apply to Stop.

Hooks are normally installed by global npm/create-agit setup or an explicit
`agit setup --hooks`. The setup reference explains all installation routes and
the selected profile files. Open those files' hook entries to inspect them;
Claude's `agit doctor` check establishes configuration presence, not effective
execution or save results. Codex provides `/hooks` for trust and control.

To stop automatic turn saving for this AgentGit user configuration:

```bash
agit config --global commit.auto false
```

Restore it with `true`. This setting leaves hooks installed, does not disable
SessionStart or every operation before settlement, and does not undo queued
work. Explicit commits remain available. `agit off [DIR]` pauses that folder's
automatic capture; explicitly linked sessions can still save locally.

To stop an invocation, disable/remove only that AgentGit hook entry in the
runtime configuration, keeping other hooks and settings. Full setup can add it
again. `AGIT_SKIP_SETUP=1` and `--remove-skill` do not disable installed
Claude/Codex hooks; there is no `--remove-hooks` command. These hooks save
conversation history and do not restore code files or create a code commit.

Hook contracts: [Claude Code](https://code.claude.com/docs/en/hooks) and [Codex](https://learn.chatgpt.com/docs/hooks). Installed Codex hooks must be enabled and trusted in the runtime before they run.

The input schema is produced by the runtime hook; do not hand-forge it to claim another person's session.
