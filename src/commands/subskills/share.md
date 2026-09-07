---
name: agit-share
description: Create one-shot read-only session links and manage links you created.
---

# agit share

## Synopsis

```bash
agit share [ref-or-session] [options]
agit share list
agit share rm <slug>
```

## Options

| Option | Meaning |
|---|---|
| `[ref-or-session]` | An explicit saved ref or native session ID/prefix; omitted targets require the exact branch in `AGIT_SESSION` |
| `--full-log` | Share the saved point's full LOG instead of its VIEW; requires a saved ref |
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

Saved refs such as `owner/repo@branch`, tags, commit IDs, `~n`, and `#n` share the VIEW
at that exact point. Missing or invalid VIEW content is refused; it never falls back to LOG.
Use `--full-log` to include discarded history from the saved LOG. File, event, and range
selectors are not supported. A local `repo@ref` requires a unique matching local repository.

An explicitly named native session ID or prefix preserves live transcript sharing, including
unsettled content. The confirmation and result label this source as a live runtime transcript.
It is separate from a saved VIEW; use an AgentGit ref with `--full-log` for saved LOG content.
A name that matches both a ref and a native session is refused rather than choosing for you.
Shares remain readable presentations; use `agit export` when raw event data is required.
