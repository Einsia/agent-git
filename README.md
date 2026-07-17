# agit

**Git for what your AI coding agents know.**

Your code is shared with your team through Git. Your coding agent's *work* isn't. Everything it
figured out — the files it read, the dead ends it ruled out, the reasoning behind a change, what it
was about to do next — lives in a private session on your machine and dies there. A teammate who picks
up the same task starts from nothing.

agit versions that session like code. It snapshots the raw transcript your agent already writes to
disk, lets you push and pull it, and when two people's agents have diverged, it has the agents
themselves read each other's work, reconcile it, and surface only the genuine conflicts for you to
decide. No summaries to write, no schema to maintain — just the session, under version control.

Works with **Claude Code** and **Codex**.

---

## How it works

An **agent** is a memory: a small git repo of session transcripts, named for what it knows
(`frontend`, `payments-api`) rather than for a person or a folder. One agent can work across many code
repos, and one repo can host many agents.

```
~/.agit/agents/agt_0190…/         the agent — a git repo, identified by an aid that never moves
├── agent.toml                    its identity, committed so it travels with the history
└── sessions/claude-code/…        the raw session dumps

your-project/                     your code, left untouched
├── .agit.toml                    committed: which agents this repo uses (this is what a clone reads)
└── src/…
```

Three commands cover most of it:

- `agit <git-args>` runs git on your **code** repo, transparently.
- `agit a <git-args>` runs git on the resolved **agent's** store. It's a normal git repo, so
  `push`/`pull`/`clone`/`log` all work.
- `agit a merge <agent>` revives both agents' latest sessions read-only, lets them reconcile by
  reading the code, and produces a **resumable merged session** — not a summary in a file.

## Install

```bash
npm install -g @agentgit/agit
```

This puts the `agit` client on your PATH. (The `agit-hub` server is separate — teams host it with
Docker or build it from source; see [deploying the hub](docs/deploying-the-hub.md).) Or build the
client from source:

```bash
git clone https://github.com/Einsia/agent-git && cd agent-git
./build.sh --release
cp target/release/agit ~/.local/bin/
```

Optionally, route `git` through `agit` so every git command also versions your agent context:

```bash
agit shadow install     # bash, zsh, fish, or PowerShell; `agit shadow uninstall` to undo
```

## Quickstart

Set up an agent in your repo:

```bash
cd your-repo
agit init --agent frontend      # mint the agent and bind it to this repo
```

Work as usual, then capture and share what your agent learned:

```bash
agit start                      # launch a session already carrying this agent's latest context
agit snap                       # capture this project's sessions into the agent's store
agit a commit -m "auth flow"    # it's a git repo — commit the memory
agit a remote add origin https://hub.example.com/frontend.git
agit a push -u origin HEAD      # push it, and record the remote for your team in .agit.toml
```

A teammate, on a fresh clone of the code repo, gets the same memory in one command:

```bash
agit a clone frontend           # .agit.toml already says which agent and where; this clones it
agit start                      # continue where the agent left off
```

When two people's work has diverged, reconcile the agents:

```bash
agit a merge frontend
# both agents revive, read each other's sessions and the code, and resolve what they can.
# only the real conflicts stop to ask you. the result is a resumable session:
#   claude --resume <id>   or   codex exec resume <id>
```

Where a git verb means the same thing on the store, `agit a` uses the git name and adds the
agent-aware behavior: `agit a clone` (by identity), `agit a init` (mint one), `agit a switch` (pick
the active agent), `agit a push`/`pull`/`fetch`/`merge`. The rest are plain git on the store, so
`agit a log` and `agit a diff` do what you'd expect. agit never replaces `git`.

## A few things worth knowing

**Identity.** An agent is identified by an `aid` (`agt_<uuid>`), minted once and committed inside its
store. Not its name (names are mutable labels and can collide) and not its URL (that's just a
locator — you can create an agent before it has a remote). Because `.agit.toml` records the aid, a
recreated remote can't silently bind you to a different agent that happens to share the name.

**Two agents at once.** Selection is per-command, so you can run two side by side in the same repo:

```bash
agit start --agent frontend     # terminal 1
agit start --agent api          # terminal 2
```

agit attributes each captured session by the launch record it wrote at `agit start`, so the two never
get mixed up even though both runtimes dump to the same folder.

**Runtimes.** Works with Claude Code and Codex. Commands that read sessions use the one you name with
`--from`, otherwise the only one present, otherwise they ask.

**Hands-off capture.** `agit watch --daemon` auto-snaps and auto-converts in the background so you
never have to remember to run `snap`.

## Collaboration: the hub

`agit-hub` is a self-contained server that hosts agents as bare git repos with sync and a web UI (a
React app compiled into the binary). It gives every session a browsable event timeline, provenance,
permalinks, and revision diffs.

Permissions are per agent: each has an owner, a visibility (private by default), and members with
read / write / admin. People sign in with a password (argon2id + a cookie session); scripts and git
use tokens that can be scoped to a single agent, given an expiry, and revoked. Every request — git
smart-http included — goes through one authorization decision. Secrets are scanned server-side on
every push, so a leaked credential can't land in a shared repo.

See [docs/deploying-the-hub.md](docs/deploying-the-hub.md) to run one, or `deploy/` for a Docker /
Compose setup.

## Security

- **Sessions can contain secrets** — a `.env` the agent read, a token it printed. Every store is
  scanned before each commit and push, and again server-side on the hub, so secrets don't travel.
- **`.agit.toml` is attacker-controlled input** (a teammate wrote it). Remotes are checked against an
  allowlist of transports before agit will clone them, because `git clone 'ext::<cmd>'` executes
  `<cmd>` and `--` doesn't stop it.
- **`merge` uses a model**, so it's non-deterministic by design — the trade for a real semantic merge
  with no schema to maintain. It merges everything it safely can and stops only on genuine conflicts.

## Development

```bash
./build.sh test        # run the test suite
./build.sh ui          # rebuild the hub frontend (embedded into agit-hub)
./build.sh --release   # build both binaries
```

`build.sh` exists because the project uses edition 2024 and a v4 `Cargo.lock`, which need cargo ≥ 1.78;
it finds a suitable cargo for you. There's a demo of the two-agent merge under `demo/showcase/`.

## License

MIT
