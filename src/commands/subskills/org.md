---
name: agit-org
description: Invite an existing account to an explicitly named organization using the selected Hub login.
---

# `agit org`

## Purpose

Invite an existing account as a member of an explicitly named organization.

## Synopsis

```bash
agit org invite <org> --user <username> [--json]
```

Use the organization owner's normal `agit login`. Personal access tokens and sessions exchanged
from them cannot invite. The organization and username must both be explicit; do not infer them
from a repository, current directory or email address. Sending an invitation requires the user's
authorization to invite that recipient.

The command invites the account as a member. It reports the selected Hub and account, pending
acceptance and the server expiry. It does not add membership immediately or send an email.
An identical pending invitation keeps its expiry; an existing member or a pending invitation
with another role is refused without changing it.

Use the printed Hub organization settings URL to review or revoke an invitation. The recipient
accepts through the existing Hub flow. An unsupported Hub requires an upgrade or its website;
never fall back to member-role changes. An unconfirmed result requires checking settings before
another attempt, not an automatic resend.

## Examples

```bash
agit org invite acme --user carol
```
