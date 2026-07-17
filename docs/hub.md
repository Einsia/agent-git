---
title: The hub
nav_order: 9
---

# The agit hub

`agit-hub` is a self-contained server that hosts agents for a team. Each agent is a bare git repo of
session transcripts; the hub adds access control, sync over git smart-http, secret scanning on push,
an audit log, and a web UI for browsing sessions. It does **not** run agents or merge anything — that
happens locally on each person's machine, where the code and the model are.

This document explains what the hub is and how its permissions and API work. To actually run one, see
[deploying-the-hub.md](deploying-the-hub.md).

## Publishing and consuming an agent

Publishing and consuming go through the normal client commands, not hub-specific ones.

```bash
# alice points her agent's store at the hub and pushes
agit a remote add origin https://hub.example.com/frontend.git
agit a push -u origin main                             # push, and record the remote in .agit.toml
agit a push                                            # later: push new sessions (scanned first)

# a teammate, after cloning the code repo
agit a clone frontend                                 # .agit.toml says which agent and where; clones it
agit a merge frontend                                 # reconcile locally, by dialogue
```

`agit a push` records the store's origin in the committed `.agit.toml` as it pushes. Any credential in
the URL is stripped before it's written to that file, so a token never reaches the repo; the full URL
stays in the store's local git config.

## Permissions

Every agent has its own **owner**, a **visibility** (private by default), and a **member table**.
Every request — the JSON API, git smart-http, and the CLI alike — goes through one authorization
decision, `agit::hub::acl::decide(caller, agent, action) -> Allow | Deny(reason)`, a pure function
with an exhaustive test suite.

| | Read | Write (push) | Admin (visibility / members / rename / delete) |
|---|---|---|---|
| Anonymous | public only | no | no |
| Signed-in user, no grant | public only | no | no |
| Member (read / write / admin) | yes | write and up | admin and up |
| Owner | yes | yes | yes |
| Site admin (`user add --admin`) | yes | yes | yes |

There are two kinds of credential:

- **Cookie sessions**, for people. `POST /api/login` returns an `HttpOnly; SameSite=Lax` cookie (also
  `Secure` under TLS), 256 bits of randomness stored server-side as a digest, expiring after 12 hours
  and dead the moment you log out. Passwords live in `users.json` (mode 0600), hashed with argon2id
  and a per-user salt; the parameters are stored with the hash, so retuning them later never locks
  anyone out.
- **Tokens**, for git and scripts. Sent as `Authorization: Bearer <token>`, or typed into git's
  password prompt (any username works). A token can be bound to a single agent, given a TTL, and
  revoked. The server stores only its sha256 digest; the plaintext is shown once, at creation.

A token is an **upper bound** on permission, never a source of it: effective permission is the token's
scope intersected with the owner's own permission. A read-only token in an admin's hands still only
reads. Admin actions — deleting a repo, changing visibility, issuing a token — never accept a token at
all; they require the person's own login session. Delete a user and their tokens die with them.

## Secret scanning on push

The hub scans every push server-side and rejects it if it finds a secret, so a leaked credential
cannot land in a shared repo even if someone bypasses the local hook with `--no-verify`. The scan
covers blob content **and** commit messages, author and committer identity, and tag messages — a
secret in any of those channels is refused. It fails closed: a push it cannot fully scan (an
oversized blob, an unreadable object) is rejected rather than waved through.

## Audit log

`audit.log` in the data root is append-only JSONL. It records logins, agent and token lifecycle,
pushes and fetches, member and visibility changes, and — importantly — **rejected** requests, since
"who tried and did not get in" is often the more useful record. `GET /api/audit` returns one agent's
log to an admin of that agent, or the site-wide log to a site admin.

## The API and web UI

The web routes (`/`, `/agent/<name>`, `/session/<id>`, and their diff/compare views) all return the
same React SPA shell, which is compiled into the binary at build time; it fetches its data from the
JSON API. The API is organized by capability rather than listed exhaustively here:

- **Agents** — list (only what you can see), create, read (session summaries, history, members,
  identity), rename, change visibility, soft-delete and restore, archive and unarchive, fork,
  transfer ownership, and star. Listing is cursor-paginated.
- **Sessions** — a full view of one session pinned to any historical revision, and a **semantic** diff
  between two revisions (added and removed instructions, files, and conclusions rather than raw jsonl
  line noise). Raw blob and cross-revision compare views serve attacker-authored content with a
  content type that the browser will not execute.
- **Members and tokens** — the member table, and token self-service (the plaintext returns once).
- **Search** — across the sessions of every agent the caller may read, with an honest cap flag when a
  scan is truncated rather than a silent cut.
- **Identity** — `GET /api/agent/by-aid/<aid>` resolves an aid to the agent's current name. The aid is
  minted by the client and committed to `agent.toml` in the store; the hub only ever reads it and
  never mints one. A freshly created, never-pushed repo honestly reports `aid: null`.

Both session layouts are accepted: `sessions/<env>/<runtime>/<id>.jsonl` and the older
`sessions/<runtime>/<id>.jsonl` (the old one reports its env as null). Claude Code and Codex are peer
runtimes, listed alphabetically.

Each session renders as a **spine** — a row of ticks whose height and color follow the event type
(prompt, reply, tool call, edit) — so you can read the shape of a session at a glance, alongside its
provenance (runtime, model, branch, author, time) for judging whether it is worth merging.

## Why the hub does not merge

Merging means reading two sessions and reasoning about what they mean, which is a model's job and has
to happen where the code and the model both are: locally, on the consumer. The hub has neither your
code nor, by design, a running agent. Keeping it to hosting, sync, and read-only rendering also avoids
the cost and the prompt-injection risk of running one model on the server over everyone's uploaded
sessions.

## Limits

- **Read-only rendering.** The hub never merges, judges conflicts, or recomputes anything; `agit a
  merge` does that on the consumer.
- **Secret scanning catches known formats.** It is a strong backstop, not a guarantee against a
  novel secret shape.
- **No TLS of its own.** It refuses a non-loopback plaintext bind; run it on loopback or behind a
  TLS-terminating reverse proxy (`--tls`, and `--trusted-proxy` so the rate limit sees real client
  IPs). See [deploying-the-hub.md](deploying-the-hub.md).
- **Sessions live in memory.** Restarting logs everyone out (the upside: revocation is immediate);
  tokens are unaffected.
- **Single process per data root.** State files are guarded by an in-process lock and atomic renames,
  so two servers pointed at the same root would clobber each other; that is unsupported.
