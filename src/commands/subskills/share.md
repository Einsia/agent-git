---
name: agit-share
description: Create one-shot read-only session links and manage links you created.
---

# agit share

## Synopsis

```bash
agit share [session] [options]
agit share list
agit share rm <slug>
```

## Options

| Option | Meaning |
|---|---|
| `[session]` | Adopted session ID or prefix; when omitted, use the latest settled session in the AgentGit repo bound to the current directory. If no repo is bound, refuse instead of falling back to the global newest session |
| `--public` | Create an unencrypted link that can be fetched directly |
| `--expire <24h\|7d\|30d\|never>` | Expiration; default `7d` |
| `--views <count>` | Maximum views |
| `--password` | Add password protection; the server stores only a password hash |
| `list` | List links you created |
| `rm <slug>` | Revoke a link |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit share --expire 24h --password
agit share 132bf69f-22a --public --views 10
agit share list
agit share rm abc123
```

A share is read-only and does not grant write access to the Agent repo. Before creating a link, agit asks for a final confirmation of the target, visibility, and expiry; `-y/--yes` is the explicit opt-out. Apply the publishing security scan before making a link public.
