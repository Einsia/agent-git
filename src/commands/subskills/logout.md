---
name: agit-logout
description: Sign out of the AgentGit Hub.
---

# agit logout

## Purpose

Revoke the server session and remove local credentials. It does not delete `~/.agit/repos`, `~/.agit/store`, or workspace records.

Credentials are deleted only after the Hub confirms the session is over. When it doesn't (unreachable, a server error, a refusal that doesn't show the session ended), they are kept for another try and the command exits non-zero (`6` network, `5` auth, `4` unreadable credentials or no Hub address, `1` for `--all` with mixed reasons). Tell the person the session may still be valid. Retry later, or run `--local-only` only when the person accepts that the session stays valid on the Hub until it expires.

## Synopsis

```bash
agit logout [--all] [--local-only]
```

## Options

| Option | Meaning |
|---|---|
| `--all` | Sign out of every Hub a credential names, one at a time; one Hub failing does not stop the others, and its credentials are kept |
| `--local-only` | Delete this computer's credentials without contacting the Hub; the session is not revoked and may stay valid until it expires |
| `--json` | Emit the unified CLI JSON envelope |
| `-y, --yes` | Skip confirmation |
| `-q, --quiet` | Reduce output |
| `-C, --directory <dir>` | Use the given working directory |
| `--no-color` | Disable color |
| `-h, --help`, `-V, --version` | Show help or version |

## Examples

```bash
agit logout
agit logout --all
agit logout --local-only
```

With `--json`, `result.value.hubs` holds one entry per Hub: `hub`, `revocation` (`confirmed`, `already_invalid`, `unconfirmed`, `not_attempted`) and `credentials_removed`.
