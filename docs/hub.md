# AgentGitHub Hub

Hosts your team's Agent Stores (each one is a git repo holding raw sessions) and provides
sync plus web browsing. A single self-contained binary, `agit-hub`: the backend carries no
heavyweight dependencies (std TCP + shelling out to git), and the frontend is a React SPA
embedded at build time (hub-ui/, see [hub-ui/README](../hub-ui/README.md)).

**Hub = Registry + Sync + read-only rendering. It does not run agents and does not do semantic merges.**
The real merge happens locally, on the consumer, with `agit -a sync` — that is where the code and the LLM live.

## Getting started

```sh
./build.sh ui                  # build the frontend first (only needed if you changed it; dist is committed)
./build.sh --release

agit-hub add payments          # host an agent (creates a bare repo)
agit-hub token add ci --write  # issue a write token — push requires one (see "Auth")
agit-hub serve --port 8177     # start it; data dir defaults to ~/.agit-hub. With --private, reads need a token too
```

Open `http://<host>:8177/`. The frontend uses the request's `Host` header to build a
copy-pasteable clone URL.

## Auth (token)

Push (`git-receive-pack`) **must** carry a write token, or it is rejected with 401 — this
closes the "anyone can push, anyone can overwrite or poison someone else's sessions" hole.
When no write token is configured, nobody can push (a safe default).

```sh
agit-hub token add alice --write   # issue a write token (can push); the token is shown only once
agit-hub token add bob --read      # a read-only token (used for reads in --private mode)
agit-hub token list                # lists names and permissions only, never the secret
```

Tokens live in `<root>/auth.json`. When git prompts for a username/password, put the token in
the password field (any username works); or use `Authorization: Bearer <token>`. Reads are open
by default; with `serve --private`, reads also require a valid token.

## Publishing (Alice)

```sh
cd your-repo
agit -a snap                                   # mirror this project's Claude session into the store (snap --from codex for Codex)
agit -a remote add origin http://alice:<token>@<host>:8177/payments.git
agit -a push -u origin main                    # pre-push scans for secrets first, then git smart-http (with the token)
```

## Consuming (a teammate)

```sh
agit clone http://<host>:8177/payments.git     # one command to pull the team's Agent Store
agit -a fetch origin
agit -a sync origin/main                        # local: revive both agents, they reconcile by reading code, and only real conflicts prompt you — leaving a resumable merged session
```

`agit -a sync` works for both claude-code and codex sessions. The dialogue and synthesis it
runs use a local LLM backend, selected with `AGIT_LLM=claude` (default) / `AGIT_LLM=codex` /
`AGIT_LLM_CMD="<cmd>"`.

## Endpoints

The web routes (`/`, `/agent/<name>`, `/session/<id>`, `.../diff`) all return the same SPA shell,
which the frontend renders by URL; the data comes from the JSON API below.

| Path | Content |
|---|---|
| `GET /` and any web route | React SPA shell (`/assets/app.js` + `app.css`, embedded at build time) |
| `GET /api/agents` | agent roster: name, session count, last activity, `host` |
| `GET /api/agent/<name>?page=&q=` | paged session summaries (spine, provenance, instructions, conclusions, changed files) + commit history |
| `GET /api/agent/<name>/session/<id>?at=` | full view of one session + revision list (`at=` pins to a historical commit) |
| `GET /api/agent/<name>/session/<id>/diff?from=&to=` | the **semantic** diff between two revisions (added/removed instructions/files/conclusions, not raw jsonl line noise) |
| `/<name>.git/...` | git smart-http (push/pull/clone; push requires a write token) |

**Session spine (signature):** each session renders as a row of ticks, with height and color set
by event type (prompt / reply / tool / edit) — so you can read the rhythm of a session at a glance
(tool-heavy? lots of back-and-forth? a burst of edits at the end?). The data comes from ConversationIR.

**Provenance:** each session shows its runtime / model / branch / author / time (the last commit
that touched it). A consumer uses this to judge trust, freshness, and relevance before deciding
whether to `sync` it against their own agent branch.

## Why the Hub does not merge

Merging means reading the sessions and reasoning about their meaning — that is an LLM's job, and it
belongs where the code and the model are: locally, on the consumer. `agit -a sync` revives both
agents (each read-only in its own branch's git worktree, with its own diff), lets them talk it out
and resolve conflicts by reading the code, and surfaces only the real conflicts for you to decide.
The Hub has neither your code nor, by design, a running agent, so it does only three things: hosting,
sync, and read-only rendering. This also avoids the expensive and dangerous (prompt-injection)
design of "run one LLM on the Hub to process everyone's uploaded sessions".

## Limits

- **Read-only rendering**: the Hub does not merge, does not judge conflicts, does not recompute
  anything. Merging still happens locally on the consumer via `agit -a sync`.
- **No secrets stored**: enforced by the publisher-side pre-push hook; but it only catches known
  formats (see [risk analysis](风险分析.md) §8).
- **Auth granularity**: tokens are global (a write token can push to any repo); there is no
  per-agent ACL or subscription yet.
- **Search cap**: with `?q=`, it scans at most the most recent N sessions (anything beyond that is
  flagged in the response, not silently truncated).
- `git http-backend` behaves differently on Windows; untested.
