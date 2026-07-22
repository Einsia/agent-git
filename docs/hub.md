---
title: Browse agents on the hub
nav_order: 10
---

# Browse agents on the hub

The hub is a server your team hosts to share agents and browse them in a web UI. It hosts each agent as a
git repository you push to and pull from, and it serves web pages that render each session, compare two
revisions, and index the agents an organization or a code repository has produced. To run one, see
[Self-host the hub](deploying-the-hub.html).

You reach the hub through the ordinary client commands. There are no hub-specific verbs: you push and pull
an agent the same way you would with any git remote. See [Share an agent with your team](guide/sharing.html).

## Sign in and browse agents

Open the hub in a browser and sign in with your account. The hub lists the agents you can read: your own,
agents shared with you, and any public ones. An agent you cannot read is hidden, and a private agent is
indistinguishable from one that does not exist, so agent names cannot be enumerated.

Each agent has an owner, a visibility (private by default), and members granted read, write, or admin.
What you can do with an agent follows your grant:

| | Read | Write (push) | Admin (visibility, members, rename, delete) |
|---|---|---|---|
| Anonymous | public only | no | no |
| Signed-in user, no grant | public only | no | no |
| Member (read / write / admin) | yes | write and up | admin and up |
| Owner | yes | yes | yes |

When the hub denies an action, it says why: a refused push names the account it authenticated as and what
it lacks, so a wrong token, a missing grant, and a read-only scope are easy to tell apart.

## View a session

An agent page lists its sessions. Open one to see it rendered as a timeline of events: prompts, replies,
tool calls, and edits, each event drawn as a tick whose height and color follow its type, so you can read
the shape of a session at a glance. Alongside it, the page shows the session's provenance (runtime, model,
branch, author, time) and its verification badge, so you can judge who produced it and whether it is worth
merging. See [Verify who produced a session](guide/provenance.html).

## Compare two revisions

The hub serves a semantic diff between two points in an agent's history: the instructions, files, and
conclusions added and removed, not the raw transcript bytes. It also serves search across the sessions of
every agent you can read.

## The organization overview

`/orgs/<name>` is an organization's overview page. It lists the org's members and every agent they can
reach: the agents the org owns, plus members' personal agents you are allowed to read. Each agent shows
its session count and the code repositories it has worked in. The page is limited to org members and site
admins, and it lists only agents you may read, so it never reveals an agent you cannot already see.

## The code-repository index

`/repos` inverts that view. It lists every code repository the hub's agents have worked in, grouped by
environment, with the agents attached to each. One repository is often touched by several agents; it is
listed once, with each agent's session count. The index is built from the agents you are allowed to read,
so it never reveals one you cannot see.

## How you sign in and push

The hub accepts two kinds of credential:

- A cookie session, for people. Signing in with your password returns a session cookie that expires and
  can be revoked by logging out.
- A token, for git and scripts. Create a token in the web UI, then send it as a bearer token or type it
  into git's password prompt. A token can be scoped to a single agent, given a time limit, and revoked. A
  token is a ceiling on permission, never a source of it: a read-only token still only reads, and admin
  actions require your own login, never a token.

If your team has enabled self-service registration, you can create your own account from the hub. Otherwise
an administrator creates it for you.

## What the hub does not do

The hub hosts, syncs, and renders. It does not run agents and does not merge. Merging reads two sessions
and reasons about them against your code, which runs locally on the machine that has both the code and the
model. See [Reconcile diverged sessions](guide/merging.html).

For the storage backends, permission engine, and API routes behind these pages, see
[Self-host the hub](deploying-the-hub.html).
