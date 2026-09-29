---
name: agit-login
description: Sign in to the AgentGit Hub.
---

# agit login

## Purpose

Create Hub credentials for recording, publishing, and account operations.
Public read-only cloning does not require login; `clone --mine` does.

## Synopsis

```bash
agit login [--hub <url>] [--with-token | --device | --complete [<state>] [--wait <seconds>]]
```

## Options

| Option | Meaning |
|---|---|
| `--hub <url>` | Hub URL; precedence is option, `AGIT_HUB_URL`, `config hub.url`, then the built-in public Hub |
| `--with-token` | Read an explicitly supplied PAT from stdin, suitable for CI |
| `--device` | Use device-code flow directly |
| `--complete [<state>]` | Finish a sign-in after human approval and save credentials; without a value it finishes the request `agit login` recorded for this Hub, including an interrupted `--device` login |
| `--wait <seconds>` | With `--complete`: how long to wait for the approval (default `90`; `0` checks once) |
| `--json` | Emit the unified CLI JSON envelope |
| `-y, --yes` | Skip confirmation |
| `-q, --quiet` | Reduce output |
| `-C, --directory <dir>` | Use the given working directory |
| `--no-color` | Disable color |
| `-h, --help`, `-V, --version` | Show help or version |

## Examples

```bash
agit login
printf '%s\n' "$AGIT_PAT" | agit login --with-token
agit login --hub https://staging.agent-git.com --device
```

Login does not create an Agent repo or bind a workspace.

## Agent and non-interactive login

Run `agit login --json` when the agent needs the human to sign in. With redirected
input or output, plain `agit login` also returns immediately with a login link
instead of opening a browser or waiting for terminal input. Quiet output keeps
the link and the instructions.

The initial response exits with `8` (human interaction required), not successful
authentication. JSON exposes `authorization_url`, `expires_in`, `message`, and
the argument array `complete_command` in `result.value`. The login request is
also recorded under `AGIT_HOME`, so any later process on the same machine and
`AGIT_HOME` can finish it.

1. Show the login link to the human and ask them to sign in and approve CLI access.
   The human completes authentication in their browser; do not ask them to paste
   passwords, access tokens, or refresh tokens into the conversation.
2. Run `agit login --complete` (add `--hub <url>` if the login used one, and
   `--json` if machine output is needed). It finishes the recorded request and
   waits up to 90 seconds for the approval, so it can run right after showing the
   link. The returned `complete_command` array does the same for that request.
3. Exit `0` means credentials were saved, including when an earlier run already
   finished the request. Exit `8` while the request still waits means the
   approval is still missing: rerun `agit login --complete` after the human
   approves. Exit `8` naming another Hub's request means the login used `--hub`:
   run the command it names. Exit `8` saying the request is no longer valid, or
   that no request is waiting, means only a new login can sign in: run
   `agit login --json` again and show the new link. Exit `8` saying `agit logout`
   cancelled the sign-in means someone signed out on purpose: sign in again only
   if the human still wants to. Any other failure, such as a network error, keeps
   the request: rerun `agit login --complete`.
4. Never start a new login while one is waiting for approval: a new request
   replaces the recorded one, and the human has to approve again. Start over only
   when `--complete` says so, or when the human never received the link.
5. Continue the original task only after completion succeeds. `agit whoami --check`
   can verify the saved identity.

A login that is interrupted while it waits (`agit login --device`, or the browser
flow at a terminal) leaves its request recorded; `agit login --complete` finishes
it from any later process without a new code, and while it waits it repeats the
device-code link and code for the human. A new `agit login` finishes a recorded
request the human already approved instead of replacing it. `agit whoami --check`,
`agit commit` and Hub commands that stop with "not logged in" first make one
quick attempt to claim an approved recorded request, and otherwise name
`agit login --complete` as the next step while the request waits.

Use `--with-token` only when a PAT has already been explicitly provided for that
purpose. `--device` is a waiting flow for a human terminal and cannot be combined
with `--json`; if the agent runtime stops it, finish it with `agit login --complete`.
