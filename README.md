# agit

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust&logoColor=white)](https://www.rust-lang.org) [![Runtimes: Claude Code + Codex](https://img.shields.io/badge/runtimes-Claude%20Code%20%2B%20Codex-8A2BE2)](#move-a-session-between-runtimes)

agit saves each coding session your AI agent produces (what the agent read, ran, and changed) into a git
repository, so the session is versioned, shared, and reconciled the same way your code is. It works with
Claude Code and Codex.

## The two scopes

agit adds a layer on top of git; it does not replace it. `agit <git command>` runs git against your code
repository, unchanged. Put `a` after `agit` and the same command runs against the agent's store, a
separate git repository of session transcripts kept under `~/.agit`.

| You type | Runs git against |
|---|---|
| `agit <git command>` | your **code** repo (ordinary git, nothing changed) |
| `agit a <git command>` | the **agent's** store (a git repo of its sessions) |

So `agit log` shows your code history and `agit a log` shows the agent's sessions. Most `agit a` commands
are plain git on the store; a few carry a specific meaning for an agent (`agit a clone` clones by
identity, `agit a push` records the remote in `.agit.toml`, `agit a merge` reconciles two sessions).

## Install

```bash
npm install -g @einsia/agentgit
```

This installs the `agit` client. To route your existing `git` through agit as well, so ordinary git
commands version the agent's sessions too:

```bash
agit shadow install     # bash, zsh, fish, or PowerShell; undo with agit shadow uninstall
```

The `agit-hub` server is distributed separately. Teams host it with Docker or build it from source (see
[Self-host the hub](docs/deploying-the-hub.md)).

## Daily use

Set your git identity once (agit refuses to record a session without it), create an agent for the repo,
turn on the daemon, and work:

```bash
git config --global user.email you@example.com   # once, like any git repo
agit init --agent frontend                        # set up this repo and mint its first agent
agit watch --daemon                               # record and convert sessions in the background
agit start                                         # open a session carrying the agent's context
agit a log                                          # the session was recorded
```

`agit watch --daemon` records each new session into the agent's store and converts it between Claude Code
and Codex, so a session recorded in one is resumable in the other. Check it with `agit watch --status`,
stop it with `agit watch --stop`. Without the daemon, capture by hand with `agit snap`.

To share the agent, add a remote and push:

```bash
agit a remote add origin https://hub.example.com/frontend.git
agit a push -u origin HEAD                # records the remote in .agit.toml, credentials stripped
git add .agit.toml && git commit          # commit the binding so teammates get the agent
```

A teammate, on a fresh clone of the code repo, runs `agit init` to clone the agent, then `agit start`.

## What do you want to do?

| Goal | Guide |
|---|---|
| Install agit and record your first session | [Get started](docs/guide/quickstart.md) |
| Record sessions automatically as you work | [Capture agent sessions](docs/guide/capture.md) |
| Continue a session with its context loaded | [Resume a session](docs/guide/resume.md) |
| Move a session between Claude Code and Codex | [Move a session between runtimes](docs/guide/runtimes.md) |
| Give a teammate an agent and its history | [Share an agent with your team](docs/guide/sharing.md) |
| Combine two people's diverged sessions | [Reconcile diverged sessions](docs/guide/merging.md) |
| Stop a secret from reaching shared history | [Keep secrets out of shared history](docs/guide/secrets.md) |
| Confirm which person produced a session | [Verify who produced a session](docs/guide/provenance.md) |
| Browse agents and sessions in a web UI | [Browse agents on the hub](docs/hub.md) |
| Run a hub for your team | [Self-host the hub](docs/deploying-the-hub.md) |
| Point an agent at a recreated remote or fork | [Rebind an agent's identity](docs/guide/migration.md) |

## Documentation

- [Get started](docs/guide/quickstart.md): the five-minute first run.
- [Command reference](docs/guide/command-reference.md): every command, one line each.
- [Configuration](docs/guide/configuration.md): environment variables and files.
- [Concepts](docs/guide/concepts.md): the vocabulary (agent, aid, store, environment).
- [Architecture](docs/architecture.md): internals, for contributors.

## Build from source

```bash
git clone https://github.com/Einsia/agent-git
cd agent-git
./build.sh --release
```

The project is Rust (edition 2024 with a v4 `Cargo.lock`), so it needs `cargo >= 1.78`. `build.sh` handles
the toolchain check and the release build.

## License

MIT. See [LICENSE](LICENSE).
