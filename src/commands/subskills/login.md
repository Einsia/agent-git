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
agit login [--hub <url>] [--with-token | --device | --complete <state>]
```

## Options

| Option | Meaning |
|---|---|
| `--hub <url>` | Hub URL; precedence is option, `AGIT_HUB_URL`, `config hub.url`, then the built-in public Hub |
| `--with-token` | Read an explicitly supplied PAT from stdin, suitable for CI |
| `--device` | Use device-code flow directly |
| `--complete <state>` | Check the browser request once and save credentials after human approval |
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
the argument array `complete_command` in `result.value`.

1. Show the login link to the human and ask them to sign in and approve CLI access.
   The human completes authentication in their browser; do not ask them to paste
   passwords, access tokens, or refresh tokens into the conversation.
2. After the human approves, execute the returned `complete_command` argument
   array on the same machine with the same `AGIT_HOME`. Keep its explicit `--hub`.
   Add `--json` if machine output is needed.
3. Completion checks once and exits. Exit `0` means credentials were saved;
   exit `8` means approval is still needed. Retry the same completion command
   after the human approves. If the link expires, run `agit login` again and show
   the new link. Do not continuously create new requests while the human is signing in.
4. Continue the original task only after completion succeeds. `agit whoami --check`
   can verify the saved identity.

Use `--with-token` only when a PAT has already been explicitly provided for that
purpose. `--device` remains a waiting flow for a human terminal and cannot be
combined with `--json`.
