---
name: agit-on
description: Save and upload a folder's Claude Code and Codex sessions automatically from now on.
---

# agit on

## Purpose

Turn a folder on: every finished turn of a Claude Code or Codex session that runs in it, or
below it, is saved and uploaded in the background. Use it when the user asks for this folder's
sessions to be saved automatically. `agit save` turns the folder on too, so a request to save
the current conversation does not need it first.

The folder is the Git top level of the directory, or the directory itself. The destination is
chosen in this order and printed: the folder's existing binding; `--repo`; a private Hub
repository linked to the folder's Git origin that the account can write to; otherwise the
account's own private repository named after the origin or the folder (created if missing).
A public destination is used only when named with `--repo` and `--public`.

Earlier sessions are not uploaded, and the result says how many there are. Uploading them is
the user's choice: ask whether to upload one (`agit save <id>`) or all of them
(`agit save --all`), and don't run either unasked. `result.value.next.ask_user` has the question.

A second project folder is turned on the same way: `agit on` in that folder, with `--repo` to
choose its repository when it shouldn't be the one chosen for it. A conversation already run
there before it was on is saved by naming it: `agit save <id>`.

A folder inside one that is on belongs to it: its conversations upload to that folder's
repository. Inside a folder that is off, `agit on` turns on only the folder it runs in (its own
Git checkout, or that directory) and leaves the one around it off; `agit on` in the outer folder
turns that one back on.

In a folder nothing saves, a conversation's start tells the person once that agit isn't saving
it; `agit config notice.unsaved false` stops that everywhere.

A folder that is a Git main checkout also covers its linked worktrees (`git worktree add`) that
have no setting of their own, and the result names each one (`also covers … (git worktree)`).
Sessions that ran in a worktree before the folder covered it count as earlier sessions there. A
separate clone or a submodule is not covered. In a worktree, `agit on` says which folder covers
it, and `agit off` turns off only the worktree.

## Synopsis

```bash
agit on [DIR] [--repo OWNER/NAME [--public]]
```

## Options

| Option | Meaning |
|---|---|
| `DIR` | The folder (default: the current directory) |
| `--repo OWNER/NAME` | Save to this repository instead of the chosen one |
| `--public` | With `--repo`: allow a public repository; everything uploaded becomes readable by anyone |
| `--json` | Emit the unified CLI JSON envelope |

Exit code `8` with `result.value.sign_in_url` means the user has to approve a sign-in: give
them the link, and run the same command again after they approve. When that run succeeds, tell
them which account they are signed in as and what is now on; don't stop at "signed in". Codex runs AgentGit's hooks
only after the user trusts them once in Codex (`/hooks`); the result says so.

## Examples

```bash
agit on
agit on --json
agit on --repo team/app-sessions
```
