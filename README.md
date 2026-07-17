# agit

**Version an agent's raw sessions so teams can collaborate on Agent Context.**

Code is public to the team through Git, but an agent's work stays trapped in a private
session — what it read, the judgment calls it made, what it planned to do next: none of that
is visible or reusable to anyone else. agit manages the agent's **raw session** directly:
push/pull that blob, and at merge time let another agent read the other side, reconcile it,
and only surface the real conflicts to you.

**Minimal footprint**: no facts to design, no schema to design. Claude Code already dumps the
entire session to disk, so we just version that.

---

## Core: an agent is a memory

An agent is **not** a person and not a folder in your project. It is a memory: a git repo of the raw
transcripts of what it did, named for **what it knows** (`frontend`, `payments-api`).

```
~/.agit/agents/agt_0190f3a1-…/    ← the agent: a standalone git repo, keyed by its IDENTITY
├── agent.toml                    ←   the aid, committed — so it travels with the history
└── sessions/claude-code/         ←   the raw session dump (transcript + subagents + tool results)

your-project/                     ← an Environment: your code, left untouched
├── src/auth.js
├── .agit.toml                    ← COMMITTED: which agents this repo works with. This is the part
│                                     that makes collaboration work — a teammate's clone reads it.
└── .gitignore                    ← .agit/ (local state only) is added automatically
```

- **An agent works in many repos; a repo hosts many agents.** Many-to-many. The store lives in
  `$AGIT_HOME`, keyed by an identity that never moves, so one agent can carry its memory from the web
  repo into the api repo — and a rename or a publish moves no directory at all.
- **Identity is the `aid`** (`agt_<uuid>`), minted once, committed inside the store. Not the name
  (names are labels, and collide) and not the URL (a URL is a locator; you mint an agent before any
  remote exists). `.agit.toml` records the aid, so a recreated remote can't silently bind you to a
  different agent wearing the same name.
- `agit <git command>` acts on your code repo (transparently); `agit a <git command>` acts on the
  resolved agent's store. It's just a git repo, so push/pull/clone come for free.
- **Merging is done by an agent**: `agit a merge <agent>` revives both sides' latest sessions
  read-only, each in its own git worktree carrying its own diff, lets them converse and resolve
  conflicts by reading the code, and surfaces only the real conflicts for you to decide. The result is
  a **resumable merged session**, not a summary in a file. Works for both claude-code and codex.

**Runtimes are peers.** claude-code and codex, alphabetically, always. There is no default: commands
that read sessions use the one you name with `--from`, else the only one present, else they ask.

## Install

```bash
./build.sh --release          # see "Why not cargo build" below
cp target/release/agit ~/.local/bin/

cd your-repo
agit init --agent frontend    # mint that agent and bind it here (bare `agit init` asks)
```

## Usage

```bash
agit a new frontend                   # mint an agent — works with no remote; identity precedes any URL
agit a list                           # what this machine has
agit a use frontend                   # this worktree's default (a default, not a lock)

agit start                            # launch a session HERE already carrying this agent's latest
                                      #   context, from whatever repo it was last in
agit snap                             # capture this project's sessions + harness into the store
agit watch --daemon                   # hands-off: auto-snap + auto-convert, in the background
agit a add -A && agit a commit -m '...'
agit a push                           # publish to the team (secrets are scanned before push)

# teammate, on a fresh clone of the code repo:
agit a track frontend                 # .agit.toml already says which agents and where; this gets the memory

agit a merge frontend                 # reconcile two memories by dialogue; only real conflicts prompt you
#   same agent (another copy)  → the histories merge too
#   a different agent          → dialogue only, both stay intact
#   the merged state is a resumable session → `claude --resume <id>` / `codex exec resume <id>`

agit convert <src> --to codex|claude-code # convert a session so the other runtime can resume it
agit workspace                        # Agent↔Environment pairing; `workspace restore [N]` rolls both back
agit a scan                           # scan session dumps for secrets
agit adapter                          # list runtime adapters (claude-code, codex)
```

`agit a <verb>` is a **closed set** of management verbs — `list · use · new · track · info · rename ·
publish · rebind · import · merge`. Anything else after `a` is git, so `agit a log`, `agit a add -A`
and `agit a show` are all just git on the store. `agit` does not replace `git`.

**Running two agents at once**, same repo, same time — selection is per-invocation:

```bash
# terminal 1                     # terminal 2
agit start --agent frontend      agit start --agent api
```

`--agent` doesn't flip the default. Capture attributes each session by the **launch record** written
at `agit start`, not by the active pointer — two agents share one dump folder, and a pointer would
misfile them silently.

> **Scope**: `a` is a subcommand, so it cannot be transposed — `agit a commit` (the agent's store) vs
> `agit commit -a` (your code, where `-a` is git's stage-all). The old `agit -a <args>` flag still
> works as a deprecated alias.

## Coming from an older agit

Stores used to live at `<your-repo>/.agit/agent`, welded to one code repo. They don't resolve any
more — that weld is exactly what made "continue this agent in another repo" impossible. Adopt yours:

```bash
agit watch --stop     # if a watcher is running: moving the store out from under it would zombie it
agit a import         # mints an identity, moves the store, writes .agit.toml — nothing is lost
```

Every agent-scoped command tells you this, so you'll get the message from whatever you happened to type.

## Demo

```bash
./demo/showcase/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
export AGIT_HOME="/tmp/agit-demo/agit-home"   # the demo's agents, not your real ~/.agit
# follow demo/showcase/讲稿.md (the host script, with real screen output) act by act

./demo/showcase/rehearse.sh   # dry-run before you go on stage
```

One repo, two agents — `ratelimit` (bucketing on `user_id`) and `identity` (renaming it to `uid`) —
and a conflict neither can see alone. `agit a merge identity` reconciles them by dialogue and leaves
both memories intact. See [`demo/README.md`](demo/README.md).

## Switching LLM backends

The models `merge` uses for its dialogue and synthesis all go through `src/llm.rs`, and the backend
is pluggable:

```sh
export AGIT_LLM=claude               # default, local `claude -p`
export AGIT_LLM=codex                # local `codex exec` (wired up, no longer a stub)
export AGIT_LLM_CMD="<any command that reads the prompt from stdin>"   # overrides everything
```

Both sides of `merge`'s dialogue are real resumed sessions, so the runtime CLI (`claude` or `codex`)
must be present. When no LLM backend is available, the synthesis step is skipped: open conflicts are
listed for you rather than auto-resolved.

Codex is a peer, not an afterthought: its session dump (`~/.codex/sessions/`) is mirrored with
`agit snap --from codex`, reconciled with `agit a merge <agent> --from codex`, and converted with
`agit convert` (`agit adapter` marks both runtimes as implemented).

## Security

- **Dumping the whole session means the transcript may contain secrets** (a `.env` the agent
  `cat`'d, a connection string it printed). So every store is scanned before commit/push — the gate
  is installed when the store is created, whichever door it came through (`a new`, `a track`,
  `a import`, `init`). Session files (jsonl) use only high-precision rules, so real secrets aren't
  drowned out by the high-entropy noise of UUIDs and request IDs.
- **An agent's remote is its permission boundary.** An agent is one repo; who can read it is who can
  read that repo. Branches are not an ACL boundary — which is an independent reason one agent is one
  repo.
- **A committed `.agit.toml` is attacker-controlled input.** `agit a track` clones a URL chosen by
  whoever wrote the repo, so remotes are checked against an allowlist of transports and refused
  otherwise: `git clone 'ext::<cmd>'` runs `<cmd>`, and `--` does not stop it (it's a scheme, not a
  flag).
- **`merge` is non-deterministic** (it defers to a model) — a deliberate trade-off that buys the
  minimal footprint plus a real semantic merge: everything that can be merged is, and it stops to
  ask only on genuine conflicts.

## Why not `cargo build`

The dependency tree uses edition2024 and `Cargo.lock` is v4, which needs cargo ≥ 1.78 (Ubuntu 22.04's
apt ships 1.75). `./build.sh` finds a usable cargo on its own (preferring `~/.cargo/bin/cargo`), so
you don't have to touch `PATH`.

## AgentGitHub Hub

`agit-hub` hosts Agent Stores (bare git repos), git-smart-http sync, and web browsing. The frontend is a
React SPA compiled into the binary (hub-ui/): every session has an event spine, provenance, permalinks,
and a revision diff. See [`docs/hub.md`](docs/hub.md). After changing the frontend, rebuild it with
`./build.sh ui`.

**Permissions are per agent**: each has an owner, a visibility (private/public), and members
(read/write/admin). People sign in with a username and password (argon2id + a cookie session); git and
scripts use tokens, which can be bound to a single agent, given a TTL, and revoked. Every entry point —
including git smart-http — goes through one decision, `agit::hub::acl::decide`. Private by default, and
the server binds loopback unless you explicitly expose it.

## Roadmap

The broader direction frames Workspace State around a small primitive set — **snap / resume / merge /
push-pull-clone / graph**. `snap`, `merge`, `resume`, `start`, `watch` and the git passthrough ship
today. The identity model — an agent as a memory, keyed by an aid, shared across environments — is
specified in [the design of record](docs/plans/2026-07-17-agent-identity-and-handoff-design.md), which
supersedes the branch model in [the earlier note](docs/plans/2026-07-16-workspace-state-primitives-design.md).
Still design-stage there: `a publish` / `a rebind`, and the per-environment session partition.

## Development

```bash
./build.sh test               # suite is green: scope / pairing / secrets / passthrough / adapter / conversion / sync dialogue / hub auth
./build.sh ui                 # rebuild the Hub frontend (hub-ui → dist, embedded into agit-hub)
./build.sh --release
```

## License

MIT
