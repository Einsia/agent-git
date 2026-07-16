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

## Core: two repos + an agent-driven merge

```
your-project/
├── models/user.ts              ← Environment: your code, left untouched by agit
├── .gitignore                  ← .agit/ is added automatically
└── .agit/agent/                ← Agent Store: a standalone git repo
    └── sessions/claude-code/   ← Claude's raw session dump (transcript + subagents + tool results)
```

- `agit <git command>` acts on your code repo (transparently); `agit -a <git command>` acts on the Agent Store.
- The Agent Store is just a git repo, so push/pull/clone come for free.
- **Merging is done by an agent**: `agit -a sync <ref>` revives both branches' latest sessions
  read-only, each in its own branch's git worktree carrying its own diff, lets them converse and
  resolve conflicts by reading the code, and surfaces only the real conflicts for you to decide.
  The result is a **resumable merged session** (open it with your runtime's resume), not a summary
  written into a file. Works for both claude-code and codex.

## Install

```bash
./build.sh --release          # see "Why not cargo build" below
cp target/release/agit ~/.local/bin/

cd your-repo
agit init                     # create the Agent Store; re-run after cloning
```

## Usage

```bash
agit -a snap                          # mirror this project's Claude session dump in (use --from codex for Codex)
agit -a add -A && agit -a commit -m '...'
agit -a push                          # publish to the team (secrets are scanned before push)

agit clone <url>                      # teammate: pull the team Agent Store in one command
agit -a fetch origin
agit -a sync origin/main              # revive both agents, reconcile by dialogue; only real conflicts prompt you
agit -a sync origin/main --both       # also write the merged state back onto the peer's branch
#   the merged state is a resumable session → open it with `claude --resume <id>` (or `codex exec resume <id>`)

agit convert <src> --to codex|claude-code # convert a session so another runtime can resume it
agit workspace                        # Agent↔Environment pairing; `workspace restore [N]` rolls both repos back to a joint state
agit -a scan                          # scan session dumps for secrets
agit adapter                          # list runtime adapters (claude-code + codex)
```

Native verbs: `init` / `snap` / `sync` / `convert` / `clone` / `scan` / `workspace` / `adapter`.
Everything else passes through to git unchanged (in both scopes). `agit` does not replace `git`.

> **Scope ambiguity**: `agit -a commit` (agent store) vs `agit commit -a` (code repo, where `-a`
> is a git argument). Only the first token immediately after `agit` is read as the scope switch.

## Demo

```bash
./demo/showcase/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
# follow demo/showcase/讲稿.md (the host script, with real screen output) act by act

./demo/showcase/rehearse.sh   # dry-run before you go on stage
```

Three acts: Alice `snap` + `push` → Bob `clone` + `snap` + `fetch` → `sync` (the agents reconcile
by dialogue, and only real conflicts prompt a human). See [`demo/README.md`](demo/README.md).

## Switching LLM backends

The models `sync` uses for its dialogue and synthesis all go through `src/llm.rs`, and the backend
is pluggable:

```sh
export AGIT_LLM=claude               # default, local `claude -p`
export AGIT_LLM=codex                # local `codex exec` (wired up, no longer a stub)
export AGIT_LLM_CMD="<any command that reads the prompt from stdin>"   # overrides everything
```

Both sides of `sync`'s dialogue are real resumed sessions, so the runtime CLI (`claude` or `codex`)
must be present. When no LLM backend is available, the synthesis step is skipped: open conflicts are
listed for you rather than auto-resolved.

Codex is a first-class citizen: its session dump (`~/.codex/sessions/`) can be mirrored with
`agit -a snap --from codex`, reconciled with `agit -a sync <ref> --from codex`, and converted with
`agit convert` (`agit adapter` marks both runtimes as implemented).

## Security

- **Dumping the whole session means the transcript may contain secrets** (a `.env` the agent
  `cat`'d, a connection string it printed). So scan for secrets before commit/push. Session files
  (jsonl) use only high-precision rules, so real secrets aren't drowned out by the high-entropy
  noise of UUIDs and request IDs.
- **`sync` is non-deterministic** (it defers to a model) — a deliberate trade-off that buys the
  minimal footprint plus a real semantic merge: everything that can be merged is, and it stops to
  ask only on genuine conflicts.

## Why not `cargo build`

The dependency tree uses edition2024 and `Cargo.lock` is v4, which needs cargo ≥ 1.78 (Ubuntu 22.04's
apt ships 1.75). `./build.sh` finds a usable cargo on its own (preferring `~/.cargo/bin/cargo`), so
you don't have to touch `PATH`.

## AgentGitHub Hub

`agit-hub` hosts Agent Stores (bare git repos), git-smart-http sync, and web browsing. Push requires a
**write token** (`agit-hub token add … --write`); with `serve --private`, reads need a token too. The
frontend is a React SPA compiled into the binary (hub-ui/): every session has an event spine,
provenance, permalinks, and a revision diff. See [`docs/hub.md`](docs/hub.md). After changing the
frontend, rebuild it with `./build.sh ui`.

## Roadmap

The broader direction (design, not all shipped) frames Workspace State around a small primitive set —
**snap / resume / sync / push-pull-clone / graph**. `snap`, `sync`, and the git passthrough ship today;
resume today is the runtime's native resume (`claude --resume` / `codex exec resume`); a first-class
`resume` verb and a `graph` view are still design-stage. See
[the design note](docs/plans/2026-07-16-workspace-state-primitives-design.md).

## Development

```bash
./build.sh test               # suite is green: scope / pairing / secrets / passthrough / adapter / conversion / sync dialogue / hub auth
./build.sh ui                 # rebuild the Hub frontend (hub-ui → dist, embedded into agit-hub)
./build.sh --release
```

## License

MIT
