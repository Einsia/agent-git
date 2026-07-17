---
title: The hub
nav_order: 9
---

# The agit hub

`agit-hub` is a separate, self-contained server a team hosts to share agents. It holds each agent as a
bare git repo of session transcripts and adds what a shared store needs: access control, sync over git
smart-http, server-side secret scanning on every push, an audit log, and a web UI for browsing
sessions. Run it with Docker or build it from source — [Deploying the hub](deploying-the-hub.html)
covers both.

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

There are two kinds of credential:

- **Cookie sessions, for people.** `POST /api/login` returns an `HttpOnly; SameSite=Lax` cookie (also
  `Secure` under TLS), stored server-side as a digest, expiring after 12 hours and dead the moment you
  log out. Passwords live in `users.json` (mode 0600), hashed with argon2id and a per-user salt.
- **Tokens, for git and scripts.** Sent as `Authorization: Bearer <token>`, or typed into git's
  password prompt (any username works). A token can be bound to a single agent, given a TTL, and
  revoked. The server stores only its sha256 digest; the plaintext is shown once, at creation.

A token is an **upper bound** on permission, never a source of it: effective permission is the token's
scope intersected with the owner's own permission. A read-only token in an admin's hands still only
reads. Admin actions never accept a token at all — they require the person's own login session.

## Secret scanning on push

Sessions can carry secrets: a `.env` the agent read, a token it printed. The hub scans every push
server-side and rejects it if it finds one, so a leaked credential cannot land in a shared repo even
when someone bypasses the local hook with `--no-verify`. The scan covers blob content, commit messages,
author and committer identity, and tag messages — a secret in any of those channels is refused. It
fails closed: a push it cannot fully scan (an oversized blob, an unreadable object) is rejected rather
than waved through.

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

## Limits

- **Read-only rendering.** The hub never merges, judges conflicts, or recomputes anything; `agit a
  merge` does that on the client.
- **Known-format scanning.** Secret scanning is a strong backstop, not a guarantee against a novel
  secret shape.
- **No TLS of its own.** The hub refuses a non-loopback plaintext bind; run it on loopback or behind a
  TLS-terminating reverse proxy. See [Deploying the hub](deploying-the-hub.html).
- **Sessions live in memory.** Restarting logs everyone out — the upside is that revocation is
  immediate; tokens are unaffected.
- **Single process per data root.** State files are guarded by an in-process lock and atomic renames,
  so two servers pointed at the same root would clobber each other; that is unsupported.
