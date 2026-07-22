---
title: The hub
nav_order: 10
---

# The agit hub

`agit-hub` is a separate, self-contained server a team hosts to share agents. It holds each agent as a
bare git repo of session transcripts and adds what a shared store needs: a database for its metadata,
access control (with organizations), self-service registration, sync over git smart-http, server-side
secret scanning on every push, content-addressed blob storage, a registry of members' device signing
keys (so a captured session can be verified as a person, not just a machine), an audit log, and a web UI
for browsing sessions. Run it with Docker or build it from source;
[Deploying the hub](deploying-the-hub.html) covers both.

The hub does not run agents and does not merge. Merging reads two sessions and reasons about them
against your code, which is a model's job that happens locally, on the machine that has both the code
and the model. The hub hosts, syncs, and renders; everything that needs a model stays on the client.

## Sharing an agent

Sharing goes through the ordinary client commands — there are no hub-specific verbs. Capture sessions
with the daemon, then push the store to the hub like any git remote:

```bash
agit watch --daemon                                    # snap new sessions into the store as you work
agit a remote add origin https://hub.example.com/frontend.git
agit a push -u origin HEAD                              # push, and record the remote in .agit.toml
git add .agit.toml && git commit                       # commit the binding so teammates get the agent
```

`agit a push` writes the store's origin into `.agit.toml` as it pushes. Any credential in the URL is
stripped before it lands in that file — a token never reaches the repo; the full URL stays in the
store's local git config. Commit `.agit.toml` and every teammate inherits the agent.

A teammate who has cloned the code repo already has the binding, so they clone the agent by name and
reconcile their own work against it:

```bash
agit a clone frontend                                  # .agit.toml already says which agent and where
agit a merge frontend                                  # reconcile locally, by dialogue
```

`agit init` in a fresh clone does the same clone step for every agent the binding declares. Merging is
a local, model-driven reconciliation, not a server operation; see [Merging](guide/merging.html).

## The database

The hub keeps its metadata (users, agents, tokens, and merge requests) in a relational database, chosen
by `AGIT_HUB_DB`:

- **Postgres**, for production, when `AGIT_HUB_DB` is a `postgres://` URL.
- **SQLite**, the zero-config default for a self-host, a `hub.db` file under the data root when
  `AGIT_HUB_DB` is unset.

Either way the bare git repos (the transcript history) and the audit log still live on disk under the
data root; only the metadata moves into the database. The hub creates and migrates its own tables at
boot, so there is no separate setup step, and a bad or unreachable `AGIT_HUB_DB` surfaces as a clear
error at startup rather than on the first request. `hub.db` and its write-ahead-log sidecars are
`0600`, since they hold credential digests and access-control facts.

## Permissions

Each agent has an **owner**, a **visibility** (private by default), and a **member table** granting
read, write, or admin. Every request — the JSON API, git smart-http, and the CLI alike — resolves
through one authorization decision, `acl::decide(caller, agent, action) -> Allow | Deny(reason)`:

| | Read | Write (push) | Admin (visibility / members / rename / delete) |
|---|---|---|---|
| Anonymous | public only | no | no |
| Signed-in user, no grant | public only | no | no |
| Member (read / write / admin) | yes | write and up | admin and up |
| Owner | yes | yes | yes |
| Site admin | yes | yes | yes |

When `decide` denies, the reason comes back named and actionable rather than as a bare rejection: a
refused push tells you which account it authenticated as and what it lacks (`denied: authenticated as
'bob', but that account has no write access to alice/frontend`), so a wrong token, a missing grant, and
a read-only scope are easy to tell apart. Existence is still not disclosed: an agent you cannot read is
answered identically whether it is private or absent.

There are two kinds of credential:

- **Cookie sessions, for people.** `POST /api/login` returns an `HttpOnly; SameSite=Lax` cookie (also
  `Secure` under TLS), stored server-side as a digest, expiring after 12 hours and dead the moment you
  log out. Passwords live in `users.json` (mode 0600), hashed with argon2id and a per-user salt.
- **Tokens, for git and scripts.** Sent as `Authorization: Bearer <token>`, or typed into git's
  password prompt (any username works). A token can be bound to a single agent, given a TTL, and
  revoked; the binding is checked against the token's own scope at creation, so a write token can only be
  bound to an agent you can already write, so you cannot mint one against an agent you only read and have
  it fail later at push. The server stores only its sha256 digest; the plaintext is shown once, at
  creation.

A token is an **upper bound** on permission, never a source of it: effective permission is the token's
scope intersected with the owner's own permission. A read-only token in an admin's hands still only
reads. Admin actions never accept a token at all — they require the person's own login session.

## Organizations

An owner can be a person or an **organization**. An org has a name and a member table of its own, with
two roles: an org **member** gets write (read and push) on every agent the org owns, and an org
**admin** gets admin (manage) on them and can add or remove members. Site admins can do everything.

Ownership by an org is expressed as the owner `org:<name>`, and org membership is folded into an
agent's effective permission at decision time: the single `acl::decide` check stays agent-only and
never learns that orgs exist, so an org just contributes members to the agents it owns. The routes live
under `/api/orgs`: `GET`/`POST /api/orgs` list and create orgs (the creator becomes the first org
admin), `GET /api/orgs/<name>` shows one, and `/api/orgs/<name>/members[/<username>]` manages the
roster. `GET /api/orgs/<name>/overview` backs the org page below: it returns the members plus every
agent the org owns and every personal agent its members own that you may read, each with its session
count and the code repos it has worked in. Org detail and membership follow the same
existence-non-disclosure as agents: you only see orgs you belong to, so org names cannot be enumerated,
and the overview lists only the agents you are allowed to read.

## Registration

New accounts are created by a site admin (`agit-hub user add`) by default: the hub is invite-only.
Turning on self-service registration with `--open-registration` or `AGIT_HUB_REGISTRATION` opens
`POST /api/register`, which lets anyone who can reach the hub create an account and be logged in with a
session cookie. A registered account is always a **normal, non-admin** user; registration can never
grant admin, which stays CLI-only. With registration off, `POST /api/register` is refused outright.

## Secret scanning on push

Sessions can carry secrets: a `.env` the agent read, a token it printed. The hub scans every push
server-side and rejects it if it finds one, so a leaked credential cannot land in a shared repo even
when someone cleared the local checks (git's `--no-verify`, or agit's own `AGIT_ALLOW_SECRETS`), neither
of which reaches the server. A false positive gets past this gate the way it gets past the local one:
an `agit:allow-secret` pragma travels in the commit, or the operator allowlists the string in the bare
repo's `.agit-allow-secrets`. The scan covers blob content, commit messages, author and committer
identity, and tag messages; a secret in any of those channels is refused. It fails closed: a push it
cannot fully scan (an oversized blob, an unreadable object) is rejected rather than waved through.

## Blob storage

Alongside the git transcript history, the hub offers content-addressed storage for large objects an
agent references. It sits behind a `BlobStore` with two backends, chosen by `AGIT_HUB_S3_ENDPOINT`
(independently of the `AGIT_HUB_DB` choice above):

- the local **filesystem** under the data root, the zero-config default, and
- **S3/Garage** when `AGIT_HUB_S3_ENDPOINT` is set (with `AGIT_HUB_S3_BUCKET` and the access/secret
  keys). A misconfigured S3 endpoint is an error at boot, never a silent fall back to local disk.

Uploads go to `PUT /api/agent/<name>/blob` and downloads to `GET /api/agent/<name>/blob/<digest>`, both
gated by the same per-agent ACL as everything else: write to upload, read to download. The server, not
the client, computes the `sha256` on upload, and that digest is the address; re-uploading identical
bytes is idempotent. Storage is namespaced per agent, so a blob uploaded under a private agent is
reachable only by someone who may read that agent, the same non-disclosure gate covers it. A blob is
opaque attacker-authored bytes, so it is deliberately not run through the secret scanner (which guards
the git transcript, the source of truth) and is served back with the same hardened, non-executable
download headers as a raw file.

## Audit log

`audit.log` in the data root is append-only JSONL. It records logins, agent and token lifecycle,
pushes and fetches, member and visibility changes, and rejected requests — who tried and did not get in
is often the more useful record. `GET /api/audit` returns one agent's log to an admin of that agent, or
the site-wide log to a site admin.

## Web UI and API

The web routes return a React SPA that is compiled into the binary and fetches its data from the JSON
API. Each session renders as a **spine**: a row of ticks whose height and color follow the event type —
prompt, reply, tool call, edit — so you can read the shape of a session at a glance, alongside its
provenance (runtime, model, branch, author, time) for judging whether it is worth merging.

Beyond that view, the API serves a **semantic** diff between two revisions — the instructions, files,
and conclusions added and removed, not raw jsonl line noise — and search across the sessions of every
agent the caller may read. Session transcripts are attacker-authored input, so raw blob and
cross-revision compare views serve their content with a content type the browser will not execute.

Two index pages cross the per-agent grain. **`/orgs/<name>`** is an organization overview: its members,
and the agents each can reach (the org's own plus members' personal ones you may read), with each
agent's session count and the code repos it has worked in. **`/repos`** inverts that: every code repo
the hub's agents have worked in, grouped by environment, with the agents attached to each. One repo is
often touched by several agents, and it is listed once with its per-agent session counts. Both are
built by fanning out over the agents you are allowed to read (backed by `/api/orgs/<name>/overview` and
`/api/repos`), so neither reveals an agent you cannot already see. `/repos` serves any caller the public
slice; `/orgs/<name>` is limited to members and site admins.

## Limits

- **Read-only rendering.** The hub never merges, judges conflicts, or recomputes anything; `agit a
  merge` does that on the client.
- **Known-format scanning.** Secret scanning is a strong backstop, not a guarantee against a novel
  secret shape.
- **No TLS of its own.** The hub refuses a non-loopback plaintext bind; run it on loopback or behind a
  TLS-terminating reverse proxy. See [Deploying the hub](deploying-the-hub.html).
- **Sessions live in memory.** Restarting logs everyone out — the upside is that revocation is
  immediate; tokens are unaffected.
- **Single node per data root.** The bare git repos and the audit log live on the data root's local
  disk, and on the default SQLite backend the metadata does too. Run one hub process per data root;
  concurrent writers within it are serialized (a database transaction, plus a per-backend write lock).
