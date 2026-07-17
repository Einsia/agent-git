# agit

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust&logoColor=white)](https://www.rust-lang.org) [![Runtimes: Claude Code + Codex](https://img.shields.io/badge/runtimes-Claude%20Code%20%2B%20Codex-8A2BE2)](#runtimes)

**Version control for the sessions your AI coding agent produces.**

Your code is already under version control. The session that produced it isn't. When Claude Code or Codex reads the schema, rules out three approaches, finds the race in the retry path, and lands the fix, the diff goes into Git and everything that led to it stays in a transcript on your laptop. Pull the branch tomorrow and you get the fix with none of the reasoning behind it.

`agit` puts that session in a git repo. It snapshots sessions into a store, syncs them with push and pull, and when two people's sessions diverge it reconciles them by having the agents themselves compare their work against the code. It supports Claude Code and Codex.

## Contents

- [How it works](#how-it-works)
- [Install](#install)
- [Daily use](#daily-use)
- [Commands](#commands)
- [Runtimes](#runtimes)
- [Merge](#merge)
- [Sharing](#sharing)
- [The hub](#the-hub)
- [Security](#security)
- [Build from source](#build-from-source)
- [License](#license)

## How it works

agit is the git you already use, with a layer added on top. Every git command still runs through it:
`agit status`, `agit commit`, `agit push`, `agit log` act on your code repo exactly as plain git would.
What agit adds is a second thing to version alongside the code — the agent.

An **agent** is a git repo of session transcripts, stored at `~/.agit/agents/<aid>/` (under `$AGIT_HOME`) and separate from your code. You name it for what it knows, such as `frontend` or `payments-api`, and it carries a stable identity: the **aid** (`agt_<uuid>`), minted once and committed in its `agent.toml`. The name and the remote are mutable labels; the aid is the identity.

Your code repo is untouched except for one committed file, `.agit.toml`, which declares the agents the repo uses and where to clone them. A teammate's clone reads it. One agent can work across many repos, and one repo can host many.

You reach an agent's store by putting `a` after `agit` — the same git command then runs against the store instead of your code:

| You type | Runs git against |
|---|---|
| `agit <git command>` | your **code** repo — ordinary git, nothing changed |
| `agit a <git command>` | the **agent's** store — a normal git repo of its sessions |

So `agit log` is your code's history and `agit a log` is the agent's. Most `agit a` commands are plain git on the store. A few do a little more, because they're git verbs that carry a specific meaning for an agent:

- `agit a clone <name>` clones an agent by identity — the name resolves through `.agit.toml`.
- `agit a push` records the store's remote into `.agit.toml` as it pushes, credentials stripped.
- `agit a pull` fast-forwards, and sends a diverged history to `agit a merge`.
- `agit a merge <agent>` reconciles two sessions by dialogue, not line-by-line text.

## Install

```bash
npm install -g @einsia/agentgit
```

This installs the `agit` client. Optionally route your existing `git` through agit, so ordinary git commands also version the agent's sessions:

```bash
agit shadow install     # bash / zsh / fish / PowerShell; undo with agit shadow uninstall
```

The `agit-hub` server is distributed separately: teams host it with Docker or build it from source (see [deploying the hub](docs/deploying-the-hub.md)). Building the client from source is covered [below](#build-from-source).

## Daily use

Set up an agent for the repo, turn on the daemon, and then work the way you already do:

```bash
agit init --agent frontend   # set up this repo and mint its first agent
agit watch --daemon          # from now on, capture runs in the background
```

`agit watch --daemon` stays running in the background. It snapshots each new session into the agent's store, and it converts the session into the other runtime's format, so one recorded in Claude Code is resumable in Codex and the reverse. Check on it with `agit watch --status`, stop it with `agit watch --stop`.

`agit start` launches a session already carrying the agent's latest context, so the agent continues where it left off; whatever you run in it, the daemon captures. If you would rather not run the daemon, capture by hand with `agit snap`.

## Commands

Anything agit does not recognize as its own verb is passed straight through to git. These are the verbs it adds.

**Working in a repo**

| Command | What it does |
|---|---|
| `agit init [--agent <name>]` | Set up this repo: clone the agents `.agit.toml` declares, or mint the first one with `--agent`. |
| `agit start [--agent <name>] [--as <runtime>]` | Launch a session already carrying the agent's context. |
| `agit snap [--from <runtime>]` | Snapshot this project's sessions into the store by hand. |
| `agit watch [--daemon\|--stop\|--status] [--no-convert]` | Hands-off capture and runtime conversion; `--daemon` backgrounds it. |
| `agit convert <src> --to <runtime> [--write]` | Rewrite a session into the other runtime's format. |
| `agit resume <src> [--as <runtime>] [--exec]` | Load a session into a runtime and continue it. |
| `agit adapter` | List the runtimes agit recognizes. |
| `agit harness [apply]` | Show, or apply, an agent's captured MCP servers, skills, and config. |
| `agit shadow install\|uninstall\|status` | Route `git` through `agit` in your shell. |

**Managing agents** (`agit a`)

| Command | What it does |
|---|---|
| `agit a init <name>` | Add another agent to this repo, a store with its own identity. |
| `agit a clone <name\|url>` | Clone an agent's store by identity; a bare name resolves through `.agit.toml`. |
| `agit a switch <name>` | Choose which agent this worktree uses. |
| `agit a list` / `agit a info <name>` | List your agents, or inspect one. |
| `agit a rename <old> <new>` | Rename an agent; the aid is unchanged. |
| `agit a rebind [--remote <url>] [--new-id]` | Repair identity, or give a fork its own aid. |
| `agit a merge <target> [--from <rt>] [--both] [--quick]` | Reconcile two memories by dialogue (see [Merge](#merge)). |

## Runtimes

agit works with **Claude Code** and **Codex**. A command that reads a session uses the runtime you name with `--from`; if only one is installed, it uses that; if both are and you did not say, it asks. `agit adapter` lists what is installed.

```bash
agit start --as codex                        # start a Codex session carrying the agent's context
agit convert <src> --to claude-code --write  # rewrite a Codex session as a Claude Code session
agit resume <src> --as codex --exec          # load a session into Codex and continue it
```

The daemon's auto-convert keeps both formats in sync, so a session captured under one runtime is always available to resume under the other.

## Merge

Two people ran the same agent and both pushed. `agit a pull` fast-forwards when it can; when the sessions have diverged, it hands off to merge.

```bash
agit a merge frontend                 # reconcile against the frontend agent
agit a merge frontend --from claude-code   # name the runtime when both are installed
agit a merge frontend --both --quick  # revive both sides; shorter dialogue
```

`agit a merge <agent>` does not diff text. It revives both sides' latest sessions read-only, has them compare their work against the actual code, and produces a **resumable merged session**, which you pick up with `claude --resume <id>` or `codex exec resume <id>`, plus the list of genuine conflicts. Not a summary written to a file.

Merge runs through a model, so its result is not deterministic. The raw sessions on both sides stay committed and versioned in the store, so a merge you do not like is one you can drop and redo.

## Sharing

Sharing is git-native: a remote and a push.

```bash
agit a remote add origin https://hub.example.com/frontend.git
agit a push -u origin HEAD          # records the remote in .agit.toml, credentials stripped
git add .agit.toml && git commit    # commit the binding so teammates get the agent
```

A teammate, on a fresh clone of the **code** repo, already has the binding. One command sets them up:

```bash
agit init            # clone every agent .agit.toml declares
agit start           # launch a session already carrying the agent's context
```

To pull a single agent instead of all of them, use `agit a clone frontend`. Its harness (MCP servers, skills, config) comes over with `agit harness apply`.

## The hub

`agit-hub` is a separate, self-contained server your team hosts, via Docker or built from source (see the [`deploying-the-hub`](docs/deploying-the-hub.md) guide). It stores agents as bare git repos with sync, and serves a web UI (a React app compiled into the binary) that renders each session as a browsable event timeline with provenance and revision diffs.

- **Per-agent permissions:** an owner, a visibility (private by default), and members with read / write / admin.
- **Two ways in:** people sign in with a password (argon2id + cookie session); scripts and git use scoped, expiring, revocable tokens.
- **One decision:** every request, git smart-http included, goes through a single authorization check.
- **Secrets scanned server-side on every push,** so a leaked credential cannot land in a shared repo.

## Security

Sessions can carry secrets: a `.env` the agent read, a token it printed. Every store is scanned before each commit and push, and the hub scans again on the way in, so those do not get committed or shared by accident.

`.agit.toml` is attacker-controlled input, since a teammate wrote it. Before agit clones a remote it declares, that remote is checked against a transport allowlist, because `git clone 'ext::<cmd>'` executes `<cmd>`.

## Build from source

```bash
git clone https://github.com/Einsia/agent-git
cd agent-git
./build.sh --release
```

The project is Rust (edition 2024 with a v4 `Cargo.lock`), so it needs `cargo >= 1.78`. `build.sh` handles the toolchain check and the release build.

## License

MIT. See [LICENSE](LICENSE).
