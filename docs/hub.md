# AgentGitHub Hub

Hosts your team's Agent Stores (each one is a git repo holding raw sessions) and provides sync plus
web browsing. A single self-contained binary, `agit-hub`: the backend carries no heavyweight
dependencies (std TCP + shelling out to git), and the frontend is a React SPA embedded at build time
(hub-ui/, see [hub-ui/README](../hub-ui/README.md)).

**Hub = Registry + Sync + read-only rendering. It does not run agents and does not do semantic merges.**
The real merge happens **locally, on the consumer**, with `agit -a merge` — that is where the LLM is.

## Getting started

```sh
./build.sh ui                       # build the frontend first (only needed if you changed it; dist is committed)
./build.sh --release

agit-hub user add alice --admin     # step one: create a person. The password is prompted for, **never on argv**
agit-hub add payments --owner alice # host an agent (creates a bare repo); private by default
agit-hub token add ci --user alice --agent payments --write --ttl-days 90
agit-hub serve --port 8177          # start it; data dir defaults to ~/.agit-hub, **listens on 127.0.0.1 only**
```

Open `http://127.0.0.1:8177/`. The frontend uses the request's `Host` header to build a
copy-pasteable clone URL.

## Permission model

In one sentence: **every agent has its own** owner, visibility, and member table; **private by default**;
every entrance (JSON API, git smart-http, CLI) goes through the same decision,
[`agit::hub::acl::decide`](../src/hub/acl.rs)
— a pure `(caller, agent, action) -> Allow/Deny(reason)` function, exhaustively tested.

| | Read | Write (push) | Admin (visibility/members/rename/delete) |
|---|---|---|---|
| Anonymous | public only | ✗ | ✗ |
| Logged-in user (no grant) | public only | ✗ | ✗ |
| Member read / write / admin | ✓ | write and up | admin and up |
| owner | ✓ | ✓ | ✓ |
| Site admin (`user add --admin`) | ✓ | ✓ | ✓ |

**Two kinds of credential**:

- **cookie session** (for people): `POST /api/login` returns an `HttpOnly; SameSite=Lax` session cookie
  (plus `Secure` when TLS is on), 256 bits of randomness, stored server-side as a digest, expires after
  12 hours, dead the moment you log out.
  Passwords are stored in `<root>/users.json` (0600) with **argon2id + one salt per person**; the
  parameters are stored alongside the hash, so retuning them later will not lock existing users out.
- **token** (for git and scripts): `Authorization: Bearer <token>`, or type it in when git prompts for a
  password (any username works).
  A token can be **bound to a single agent**, given a TTL, and revoked; the server only stores its sha256
  digest, and the plaintext is shown once.

> **A token is an upper bound on permission, not a source of it.** Effective permission = the token's
> scope ∩ the owner's own permission.
> A read-only token in an admin's hands still only reads; a write token in a read-only member's hands is
> still read-only.
> Admin actions (delete a repo / change visibility / issue a token) **never accept a token** — they
> require the person's own login session.
> Delete the owner and their tokens die on the spot.

```sh
agit-hub token add ci --user alice --agent payments --write --ttl-days 90
agit-hub token list                # lists id/owner/binding/scope/expiry/last used; never echoes the secret
agit-hub token rm tok_abc123def456 # revoke
```

## Exposing it

Defaults to **listening on 127.0.0.1 only**: the Hub holds every transcript your team has, and
"install it and it's on the office network" cannot be the default.

```sh
agit-hub serve --host 0.0.0.0 --tls --trusted-proxy 10.0.0.1   # nginx/caddy in front terminates HTTPS
agit-hub serve --host 0.0.0.0 --insecure                        # plaintext on the wire (it will tell you the cost)
```

A non-loopback address + no TLS + no `--insecure` → **refuses to start**, and says exactly why (passwords
and tokens would cross the wire in the clear).
When you put a reverse proxy in front, always pass `--trusted-proxy <proxy IP>`: otherwise everyone behind
the proxy shares one per-IP rate-limit quota and knocks each other off;
and with no proxy declared the Hub **does not trust** `X-Forwarded-For` (anyone can forge it).

## Audit

`<root>/audit.log`, JSONL, append-only (rotation is logrotate's job). It records login/create repo/push/
fetch/member changes/visibility changes/delete repo/issue token/revoke, **and rejected requests** —
"who tried and did not get in" is often more useful than "who got in".
`GET /api/audit?agent=&limit=`: one agent's audit needs admin on that agent; site-wide audit is for site
admins only.

## Migrating from the old version

Old Hub tokens have no owner (one token = a pass for the entire host), which does not map onto the new ACL,
so they are **all invalidated**; `agit-hub token list` flags them, and reissuing is enough. Old repos with
no record in `agents.json` → treated as **ownerless private** (only site admins can see them); claim one
with `agit-hub add <name> --owner <user>`.
Both are printed as a reminder when `serve` starts.

## Publishing (Alice)

```sh
cd your-repo
agit -a snap                                   # first, mirror this project's Claude session in
agit -a remote add origin http://alice:<token>@<host>:8177/payments.git
agit -a push -u origin main                    # pre-push scans for secrets first, then git smart-http (with the token)
```

## Consuming (a teammate)

```sh
agit clone http://<host>:8177/payments.git     # one command to pull the team's Agent Store
agit -a fetch origin
agit -a merge origin/main                  # local: the agent reads the sessions, synthesizes CLAUDE.md, only real conflicts prompt you
```

## Endpoints

The web routes (`/`, `/agent/<name>`, `/session/<id>`, `.../diff`) all return the same SPA shell,
which the frontend renders by URL; the data comes from the JSON API below.

| Path | Content | Needs |
|---|---|---|
| `GET /` and any web route | React SPA shell (`/assets/app.js` + `app.css`, embedded at build time) | — |
| `POST /api/login` · `POST /api/logout` · `GET /api/me` | session | — |
| `GET /api/agents` | agent roster: name, `aid`, session count, last activity, visibility, your role | only lists what you can see |
| `POST /api/agents` | create an agent (`{name, visibility}`, private by default) → `201 {name, aid, clone_url}` | login |
| `GET /api/agent/<name>?page=&q=` | session summaries (spine, provenance…) + history + `aid`/env/branch/size/runtime/members | read |
| `PATCH /api/agent/<name>` · `DELETE /api/agent/<name>` | rename/change visibility · delete repo | admin |
| `GET·POST /api/agent/<name>/members` · `DELETE .../members/<user>` | member table | read · admin |
| `GET /api/agent/<name>/session/<id>?at=` | full view of one session + revision list (`at=` pins to a historical commit) | read |
| `GET /api/agent/<name>/session/<id>/diff?from=&to=` | the **semantic** diff between two revisions (added/removed instructions/files/conclusions, not raw jsonl line noise) | read |
| `GET·POST /api/tokens` · `DELETE /api/tokens/<id>` | token self-service (the plaintext comes back once) | login session |
| `GET /api/audit?agent=&limit=` | audit | admin / site admin |
| `/<name>.git/...` | git smart-http (push/pull/clone) | read / push needs write |

**Where `aid` comes from:** `agt_<uuid>` is minted by the **client** and committed to `agent.toml` in the
store; the Hub only reads it with `git show <ref>:agent.toml` and never mints one. An empty repo (just
created, nobody has pushed yet) honestly reports `aid: null`
(`aid_source` says whether that is `none` or `unidentified`). A rename does not change identity: the name
is just a mutable label.

**Session layout:** both `sessions/<env>/<runtime>/<id>.jsonl` and the old `sessions/<runtime>/<id>.jsonl`
**are accepted** (the old layout reports `env` as `null`). claude-code and codex are peer runtimes; the
list is alphabetical.

**Session spine (signature):** each session renders as a row of ticks, with height and color set by event
type (prompt / reply / tool / edit) — so you can read the rhythm of a session at a glance (tool-heavy?
lots of back-and-forth? a burst of edits at the end?). The data comes from ConversationIR.

**Provenance:** each session shows its runtime / model / branch / author / time (the last commit that
touched it), and a consumer uses that to judge trust, freshness, and relevance before deciding whether to
reconcile it into their own CLAUDE.md.

## Why the Hub does not merge

Merging means reading the sessions and reasoning about their meaning — that is an LLM's job, and it has to
happen **where the code and the model are**: locally, on the consumer.
The Hub has neither your code nor, by design, a running agent, so it does only this: hosting, sync,
read-only rendering.
This also avoids the expensive and dangerous (prompt injection) design of "run one LLM on the Hub to
process everyone's uploaded sessions".

## Limits

- **Read-only rendering**: the Hub does not merge, does not judge conflicts, does not recompute anything.
  Merging still happens locally on the consumer, in reconcile.
- **No secrets stored**: enforced by the publisher-side pre-push hook; but it only catches known formats
  (see [risk analysis](风险分析.md) §8).
- **No TLS**: the Hub does not terminate HTTPS itself; either listen on loopback only, or put a reverse
  proxy in front (`--tls`).
- **Sessions live in memory**: restarting the process = everyone logs in again (what you get for it:
  revocation takes effect immediately). Tokens are unaffected.
- **Single process**: read-modify-write of `users.json`/`agents.json`/`auth.json` relies on an in-process
  lock + atomic rename; multiple `agit-hub serve` pointed at the **same root** will clobber each other,
  and that is not supported.
- **Search cap**: with `?q=`, it scans at most the most recent N sessions (anything beyond that is flagged
  in the response, not silently truncated).
- `git http-backend` behaves differently on Windows; untested.
