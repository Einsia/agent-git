---
name: agit-save
description: Upload now - this conversation, this folder's unsaved sessions, all of them, or the whole day's.
---

# agit save

## Purpose

Upload sessions now and get their links. It signs in, finds the conversation and its
destination by itself, and turns the folder on, so later turns upload after each turn without
any command. Use it for every "save / upload / back up this conversation" request from a Claude
Code or Codex conversation; do not build `import`, `commit` or `push` commands for it.

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
the user must approve a sign-in: send them the link, then run the same command again.

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
