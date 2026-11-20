---
name: agit-save
description: Upload now - this conversation, this folder's unsaved sessions, all of them, or the whole day's.
---

# agit save

## Purpose

Upload sessions now and get their links. It signs in, finds the conversation and its
destination by itself, and turns the folder on, so later turns upload after each turn without
any command. Use it for ordinary "save / upload / back up this conversation" requests from a
Claude Code or Codex conversation.

`--repo` selects the folder's future saving and upload destination, including when the folder
already has one. To publish an already saved ordinary session to another repository once, use
`agit push SOURCE_OWNER/SOURCE_REPO@SESSION_BRANCH --to DEST_OWNER/DEST_REPO` instead. Name the
actual saved source session; the destination must be writable on the same Hub. This publishes
saved history without moving the source claim or changing the folder's future destination.
See `push.md` for publication consent, privacy checks and the device-local RC restriction:
`--to` cannot replace an RC repository's already confirmed destination. It is not a persistent
per-session upstream setting, and `import --independent` is not a destination switch.

- Inside an agent, with no argument: the conversation the command runs in.
- In a terminal, with no argument: this folder's sessions the Hub does not have yet.
- `--all`: every session that ran in this folder, including those from before it was on.
- `--today`: every session active today, in every folder, each to its folder's destination.
- `SESSION`: one session of this folder by ID prefix or words from its title.

## Synopsis

```bash
agit save [SESSION] [--all | --today] [--repo OWNER/NAME [--public]]
```

## Result

With `--json`, `result.value.sessions[]` has `title`, `status` (`uploaded`, `pending` or
`failed`), `link` and `error`. `pending` means the conversation's first reply has not finished;
it uploads by itself when the reply ends. Exit code `8` with `result.value.sign_in_url` means
the user must approve a sign-in: send them the link, then run the same command again. When
that run succeeds, tell them which account they are signed in as and give them the session
links it returned.

`result.value.opencode_earlier` (`earlier`, `already_saved`), when not null, means this save looked
once at the OpenCode conversations of a folder turned on before agit recorded them. The ones it
counted stay out of plain saves, as the folder's other earlier sessions do. Tell the user how many,
and that naming one saves it. Don't save them yourself.

`result.value.folders_turned_on[]` (`folder`, `repository`) lists the folders this save turned
on. Tell the user that new sessions there now upload by themselves and ask once whether they
should keep doing so (`result.value.next.ask_user`). If the user says no, run
`agit off --json <folder>` (the folder's `if_no`): new sessions there stay on this computer, and
you do not ask again. Turn it back on (`agit on --json`) only when the user asks.

## Examples

```bash
agit save --json
agit save --today
agit save --all
agit save "login redirect"
```
