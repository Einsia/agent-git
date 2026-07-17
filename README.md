# agit

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust&logoColor=white)](https://www.rust-lang.org) [![Runtimes: Claude Code + Codex](https://img.shields.io/badge/runtimes-Claude%20Code%20%2B%20Codex-8A2BE2)](#runtimes)

**Version control for the sessions your AI coding agent produces.**

Your code is already under version control. The session that produced it isn't. When Claude Code or Codex reads the schema, rules out three approaches, finds the race in the retry path, and lands the fix, the diff goes into Git — and everything that led to it stays in a transcript on your laptop. Pull the branch tomorrow and you get the fix with none of the reasoning behind it.

`agit` puts that session in a git repo: it snapshots sessions into a store, syncs them with push and pull, and when two people's sessions diverge it reconciles them by having the agents themselves compare their work against the code.

## Contents

- [The daily loop](#the-daily-loop)
- [The model](#the-model)
- [Commands](#commands)
- [Runtimes](#runtimes)
- [Merge](#merge)
- [Sharing](#sharing)
- [The hub](#the-hub)
- [Security](#security)
- [Install](#install)
- [Build from source](#build-from-source)
- [License](#license)

## The daily loop

Set up an agent for the repo once, turn on the daemon, and then work normally.

```bash
agit init --agent frontend   # mint an agent for this repo and bind it
agit watch --daemon          # start the daemon and forget it
```

`agit watch --daemon` runs in the background and does two things without you:

- **Auto-snapshot** — every new session lands in the agent's store the moment it exists.
- **Auto-convert** — each session is rewritten into the other runtime's format, so one recorded in Claude Code is resumable in Codex, and the reverse.

From here you just run Claude Code or Codex the way you already do. Manage the daemon like any other:

```bash
agit watch --status     # running? what has it captured?
agit watch --stop       # stop it
```

Run it in the foreground to watch it work, or drop the conversion step:

```bash
agit watch              # foreground
agit watch --no-convert # snapshot only, no runtime conversion
```

Without the daemon, capture by hand — `agit snap` is the daemon's snapshot step run once, on demand:

```bash
agit snap                # snapshot this project's sessions into the store
agit snap --from codex   # ...from a specific runtime
```

## The model

An **agent** is a git repo of session transcripts, stored at `~/.agit/agents/<aid>/` (i.e. `$AGIT_HOME`). You name it for what it knows — `frontend`, `payments-api` — not for a person or a folder. It carries a stable identity, the **aid** (`agt_<uuid>`), minted once and committed in its `agent.toml`.

Your code repo is untouched except for a single committed file, `.agit.toml`, which declares which agents the repo uses and where to clone them. A teammate's clone reads it and knows exactly which stores to pull.

The relationship is many-to-many: one agent works across many repos, and one repo hosts many agents.

> **Identity is the aid, not the name and not the URL.** The name is a mutable label that can collide; the URL is just a locator. Because `.agit.toml` records the aid, a remote someone recreates under the same name can't silently bind you to a different agent.

## Commands

`agit` has two scopes. Anything it doesn't recognize as its own verb is passed straight through to git:

| Command | Runs git against |
|---|---|
| `agit <git-args>` | your **code** repo, transparently |
| `agit a <git-args>` | the resolved **agent's** store (a normal git repo) |

So `agit log` and `agit diff` act on your code; `agit a log` and `agit a diff` act on the agent's history.

### Git, mapped

The agent-store verbs keep git's names where the meaning carries over — the agent version does a little more:

| Git | agit | What it adds |
| --- | --- | --- |
| `git init` | `agit a init <name>` | mints a store with its own identity (`aid`) |
| `git clone <url>` | `agit a clone <name\|url>` | a bare name resolves via `.agit.toml` |
| `git push` | `agit a push` | records the store's origin into `.agit.toml` (credentials stripped) |
| `git pull` | `agit a pull` | fast-forward only; divergence routes to `agit a merge` |
| `git fetch` | `agit a fetch` | reports which sessions arrived |
| `git merge` | `agit a merge <agent>` | reconciles two sessions by dialogue, not by text |

### Native verbs

| Command | What it does |
|---|---|
| `agit init [--agent <name>]` | Set up the agents this repo declares; `--agent` mints and binds a new one. |
| `agit start [--agent <name>] [--as <runtime>]` | Launch a session already carrying the agent's context. |
| `agit snap [--from <runtime>]` | Snapshot this project's sessions into the store (manual capture). |
| `agit watch [--daemon\|--stop\|--status] [--no-convert]` | Hands-off auto-snap and auto-convert. |
| `agit convert <src> --to <runtime> [--write]` | Rewrite a session into the other runtime's format. |
| `agit resume <src> [--as <runtime>] [--exec]` | Load a session into a runtime and continue it. |
| `agit adapter` | List the runtimes agit knows. |
| `agit harness [apply]` | Show, or apply, an agent's captured MCP servers, skills, and config. |
| `agit shadow install\|uninstall\|status` | Route `git` through `agit` in your shell (bash/zsh/fish/PowerShell). |

### Managing agents

The verbs above cover the git-shaped operations; these are the agit-specific ones:

| Command | What it does |
|---|---|
| `agit a switch <name>` | Select this worktree's active agent. |
| `agit a list` | List the agents this repo uses. |
| `agit a info <name>` | Inspect one agent. |
| `agit a rename <old> <new>` | Relabel an agent (the aid is unchanged). |
| `agit a rebind [--remote <url>] [--new-id]` | Repair identity, or give a fork its own aid. |
| `agit a merge <target> [--from <rt>] [--both] [--quick]` | Reconcile two memories by dialogue (see [Merge](#merge)). |

## Runtimes

agit works with **Claude Code** and **Codex**. A command that reads a session uses the runtime you name with `--from`; if only one is installed, it uses that; if both are and you didn't say, it asks. `agit adapter` lists what's installed.

```bash
agit start --as codex                   # start a Codex session carrying the agent's context
agit convert <src> --to claude --write  # rewrite a Codex session as a Claude Code session
agit resume <src> --as codex --exec     # load a session into Codex and continue it
```

The daemon's auto-convert means a session captured under one runtime is always available to resume under the other.

## Merge

Two people ran the same agent and both pushed. `agit a pull` fast-forwards when it can; when the sessions have diverged, it hands off to merge.

```bash
agit a merge frontend                 # reconcile against the frontend agent
agit a merge frontend --from claude   # name the runtime when both are installed
agit a merge frontend --both --quick  # revive both sides; shorter dialogue
```

`agit a merge <agent>` doesn't diff text. It revives both sides' latest sessions read-only, has them compare their work against the actual code, and produces a **resumable merged session** — pick it up with `claude --resume <id>` or `codex exec resume <id>` — plus the list of genuine conflicts. Not a summary written to a file.

> Merge uses a model, so it's non-deterministic by design. That's the trade for a real semantic reconciliation with no schema to force the sessions into.

## Sharing

Sharing is git-native — a remote and a push.

```bash
agit a remote add origin https://hub.example.com/frontend.git
agit a push -u origin HEAD          # records the remote in .agit.toml, credentials stripped
git add .agit.toml && git commit    # commit the binding so teammates get the agent
```

A teammate, on a fresh clone of the **code** repo, already has the binding:

```bash
agit a clone frontend               # .agit.toml already says which agent and where
agit harness apply                  # bring over its MCP servers, skills, and config
agit start                          # launch a session already carrying the agent's context
```

Or just run `agit init` in the fresh clone — it clones every agent the binding declares.

## The hub

`agit-hub` is a separate, self-contained server your team hosts — Docker, or build from source (see the [`deploying-the-hub`](deploying-the-hub.md) guide). It stores agents as bare git repos with sync, and serves a web UI — a React app compiled into the binary — that renders each session as a browsable event timeline with provenance and revision diffs.

- **Per-agent permissions:** an owner, a visibility (private by default), and members with read / write / admin.
- **Two ways in:** people sign in with a password (argon2id + cookie session); scripts and git use scoped, expiring, revocable tokens.
- **One decision:** every request — git smart-http included — goes through a single authorization check.
- **Secrets scanned server-side on every push,** so a leaked credential can't land in a shared repo.

## Security

Sessions can carry secrets — a `.env` the agent read, a token it printed. Every store is scanned before each commit and push, and the hub scans again on the way in, so those don't get committed or shared by accident.

`.agit.toml` is attacker-controlled input — a teammate wrote it. Before agit will clone a remote it declares, that remote is checked against a transport allowlist, because `git clone 'ext::<cmd>'` executes `<cmd>`.

## Install

```bash
npm install -g @einsia/agentgit
```

That installs the **client** only. The `agit-hub` server ships separately — Docker, or build from source.

Optionally, route your existing `git` through agit so capture keeps working without changing your muscle memory:

```bash
agit shadow install     # bash / zsh / fish / PowerShell
agit shadow status      # is it active?
agit shadow uninstall   # undo it
```

## Build from source

```bash
git clone https://github.com/Einsia/agent-git
cd agent-git
./build.sh --release
```

The project is Rust — edition 2024 with a v4 `Cargo.lock` — so it needs `cargo >= 1.78`. `build.sh` handles the toolchain check and the release build.

## License

MIT. See [LICENSE](LICENSE).