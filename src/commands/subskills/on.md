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

Earlier sessions are not uploaded; the result says how many there are, and
`agit save --all` uploads them.

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
them the link, and run the same command again after they approve. Codex runs AgentGit's hooks
only after the user trusts them once in Codex (`/hooks`); the result says so.

## Examples

```bash
agit on
agit on --json
agit on --repo team/app-sessions
```
