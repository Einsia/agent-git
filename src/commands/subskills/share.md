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

A bare `agit share` in a human terminal opens a wizard: choose a saved session,
then encrypted or public visibility and an expiry. The selection is temporary;
it does not bind another command to that session. Agents and scripts supply a
saved ref or `AGIT_SESSION`. `--yes` skips final confirmation, not target selection.

## Options

| Option | Meaning |
|---|---|
| `[ref-or-session]` | Explicit `owner/repo@branch`, another saved ref, `@`, or native session ID/prefix; omitted targets and `@` require `AGIT_SESSION` outside the terminal picker |
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
agit share
agit share owner/repo@branch --expire 24h --password
agit share 132bf69f-22a --public --views 10
agit share list
agit share rm abc123
```

A share is read-only and does not grant write access to the Agent repo. Before creating a link, agit asks for a final confirmation of the target, visibility, and expiry; `-y/--yes` confirms the generated preview without an interactive prompt. Saved refs and adopted live sessions use the same privacy processor, path aliases, content rewrites, and independent local secret processor as privacy export. Scanner and dictionary failures skip affected privacy work. The command prints the outgoing preview and report before upload.

Shares use local policy only. Policy loading, alias allocation, scanning, and dictionary failures
skip affected privacy work within the worker deadline. The Hub does not inspect or reject content
for privacy findings. The selected source and encryption recipient remain integrity boundaries.

Saved refs such as `owner/repo@branch`, tags, commit IDs, `~n`, and `#n` share the VIEW
at that exact point. Missing or invalid VIEW content is refused; it never falls back to LOG.
Use `--full-log` to include discarded history from the saved LOG. File, event, and range
selectors are not supported. A local `repo@ref` requires a unique matching local repository.

An explicitly named native session ID or prefix preserves live transcript sharing, including
unsettled content, with explicit `--public`. Encrypted sharing requires a saved, verified accepted
publication of an encrypted repository; standalone encrypted sharing refuses before upload.
Ordinary repositories, the default for new repositories, have no viewing key, so a share of
their history needs `--public` (anyone with the URL can fetch it) and is refused without it. The confirmation and result label this source as a live runtime transcript.
It is separate from a saved VIEW; use an AgentGit ref with `--full-log` for saved LOG content.
A name that matches both a ref and a native session is refused rather than choosing for you.
The command sends the versioned privacy share request to `/api/shares/privacy`. Public shares
carry a structured projected value. Encrypted shares wrap a standard privacy envelope inside the
browser-compatible AES-GCM payload; the fragment key opens that transport and the accepted
publication's repository viewing key opens the selected private records. Keep the source accepted
repository/commit locator when delivering originals: the standalone share page cannot infer it
from a recipient. `--password` remains a separate share access passphrase. A missing format acknowledgement or recipient-key drift
fails closed, with no legacy endpoint fallback. The backend stores the payload opaquely and the
website renders the structured public value; original-content unlock requires the accepted source
publication context.
