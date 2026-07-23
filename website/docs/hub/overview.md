---
sidebar_position: 1
title: Hub overview
---

# Hub overview

:::tip Try the hosted hub
A hub runs at [agit.anggita.org](https://agit.anggita.org). Sign in to browse agents, or [run your own](../self-hosting/deploying.md).
:::

The hub is a server your team runs to share agents and read them in a browser. It does three things:

- **Hosts.** Each agent is a bare git repository of session transcripts. You push to it and pull from it
  over git smart-http, the same as any git remote.
- **Syncs.** It is the shared origin your teammates fetch from, so a session captured on one machine is
  readable on another.
- **Renders.** It serves a web UI that reads each session as a conversation, compares two revisions, and
  indexes the agents an organization or a code repository has produced.

The hub does not run agents and does not merge. Merging reads two diverged sessions and reasons about
them against your code, which runs locally on the machine that has both the code and the model. See
[Reconcile diverged sessions](../cli/merging.md).

There are no hub-specific client verbs. You reach the hub through the ordinary `agit` commands, pointing a
git remote at an agent's `.git` URL. To connect the CLI, see
[Connect the CLI to a hub](../integration/connect-cli-to-hub.md); to publish, see
[Share an agent](../integration/sharing.md). To stand a hub up, see
[Deploy the hub](../self-hosting/deploying.md).

## Sign in

Open the hub in a browser and sign in with your account. A password login returns a session cookie that
expires and is revoked when you log out. If your team enabled self-service registration you can create
your own account from the sign-in page; otherwise an administrator creates it for you. See
[Accounts](./accounts.md).

Git and scripts authenticate differently, with a token or an enrolled device key rather than a cookie.
See [Tokens](./tokens.md) and [Authentication](../integration/authentication.md).

## Browse agents you can read

The hub lists the agents you can read: your own, agents shared with you, and any public ones. An agent you
cannot read is not shown, and a private agent is indistinguishable from one that does not exist, so agent
names cannot be enumerated.

Each agent has an owner, a visibility (private by default), and members granted read, write, or admin.
What you can do follows your grant:

| | Read | Write (push) | Admin (visibility, members, rename, delete) |
|---|---|---|---|
| Anonymous | public only | no | no |
| Signed-in user, no grant | public only | no | no |
| Member (read / write / admin) | yes | write and up | admin and up |
| Owner | yes | yes | yes |

When the hub denies an action it says why. A refused push names the account it authenticated as and what
it lacks, so a wrong token, a missing grant, and a read-only scope are easy to tell apart.

## Where to go next

- [Reading a session](./reading-a-session.md): the conversation view, the provenance badge, files, and
  revisions.
- [Organizations](./organizations.md): shared ownership, invitation-only membership, and the org overview.
- [Repositories](./repositories.md): the index of code repositories your agents have worked in.
- [Signing keys](./signing-keys.md): enroll a device key so your sessions attribute to you.
- [Report a problem](./reporting-problems.md): every hub response carries a request id.
