---
title: Command reference
nav_order: 8
---

# Command reference

Anything not listed here is passed through to git: `agit <git-args>` runs on the code repo,
`agit a <git-args>` on the resolved agent's store.

## Working with sessions

| Command | Does |
|---|---|
| `agit init [--agent <name>]` | Set up the agents this repo declares; `--agent` mints and binds a new one. |
| `agit start [--agent <name>] [--as <runtime>]` | Launch a session already carrying the agent's context. |
| `agit snap [--from <runtime>]` | Snapshot this project's sessions into the store by hand. |
| `agit watch [--daemon\|--stop\|--status] [--no-convert]` | Hands-off auto-snap and auto-convert; `--daemon` runs it in the background. |
| `agit convert <src> --to <runtime> [--write]` | Rewrite a session into the other runtime's format. |
| `agit resume <src> [--as <runtime>] [--exec]` | Load a session into a runtime and continue it. |
| `agit adapter` | List the runtimes agit knows. |
| `agit harness [apply]` | Show, or apply, an agent's captured MCP servers, skills, and config. |
| `agit shadow install\|uninstall\|status` | Route `git` through `agit` in your shell (bash/zsh/fish/PowerShell). |
| `agit scan` | Run the secret scan by hand. |

Set up capture once with `agit watch --daemon`: it snapshots new sessions as you work and converts
them between runtimes so either CLI can resume them. `agit snap` is the manual alternative.

## Managing agents (`agit a`)

| Command | Does |
|---|---|
| `agit a list` | Agents you have locally, with session counts and which is active. |
| `agit a init <name>` | Mint a new agent — a store with its own identity. |
| `agit a switch <name>` | Select this worktree's active agent. |
| `agit a clone <name\|url>` | Clone an agent's store by identity; a bare name resolves through `.agit.toml`. |
| `agit a info <name>` | Name, aid, store path, and remote for one agent. |
| `agit a rename <old> <new>` | Rename an agent. |
| `agit a push` | Push the store's sessions and record its remote in `.agit.toml` (credentials stripped). |
| `agit a pull` | Fast-forward pull; divergence routes to `agit a merge`. |
| `agit a fetch` | Fetch, and report which sessions arrived. |
| `agit a rebind [--remote <url>] [--new-id]` | Repair a binding's identity, or give a fork its own aid. See [Migration](migration.html). |
| `agit a merge <target> [--from <runtime>] [--both] [--quick]` | Reconcile two memories by dialogue into a resumable merged session. See [Merging](merging.html). |
