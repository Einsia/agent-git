---
title: Command reference
nav_order: 8
---

# Command reference

Anything not listed here is passed through to git: `agit <git-args>` runs against the code repo,
`agit a <git-args>` against the resolved agent's store.

## Working with sessions

| Command | Does |
|---|---|
| `agit init [--agent <name>]` | Set up the agents the binding declares; `--agent` mints a new one and binds it here. |
| `agit start [--agent <name>] [--as <runtime>]` | Launch a session already carrying the agent's context. |
| `agit snap [--from <runtime>] [--watch] [--no-harness]` | Copy the runtime's session files into the store. `--watch` runs it continuously. |
| `agit watch [--daemon\|--stop\|--status] [--no-convert]` | Hands-off capture: auto-snap and auto-convert both runtimes. `--daemon` backgrounds it. |
| `agit convert <src> --to <runtime> [--from <runtime>] [--write]` | Rewrite a session into the other runtime's format. `--watch` auto-converts both ways. |
| `agit resume <src> [--as <runtime>] [--env <path>] [--exec]` | Load a session into a runtime and continue it. |
| `agit adapter` | List the runtimes agit recognizes. |
| `agit harness [apply] [--from <runtime>] [--force]` | Show, or apply, an agent's captured MCP servers, skills, and config. |
| `agit scan [--staged] [paths]` | Run the secret scan by hand. |

## Managing agents (`agit a`)

| Command | Does |
|---|---|
| `agit a list` | Agents you have locally, with session counts and which is active. |
| `agit a new <name>` | Mint a new agent. |
| `agit a use <name>` | Set this worktree's default agent. |
| `agit a track <name\|url>` | Clone an agent's store (from the binding, or a URL). |
| `agit a info <name\|aid>` | Name, aid, store path, and remote for one agent. |
| `agit a rename <old> <new>` | Rename an agent. |
| `agit a publish [<url>] [--no-push]` | One step: set the remote, record it, and push. Same binding effect as `push` below. |
| `agit a rebind [<name>] [--remote <url>] [--new-id]` | Fix a binding's identity, or give a fork its own aid. See [Migration](migration.html). |
| `agit a import [<name>]` | Adopt a legacy in-repo store. See [Migration](migration.html). |
| `agit a merge <target> [--from <runtime>] [--both] [--quick]` | Reconcile with another memory. `--quick` skips the context handoff. See [Merging](merging.html). |

## Git on the store, augmented

A handful of git verbs do the session-aware thing under `agit a`, because plain git would be wrong or
incomplete for transcripts. Everything else (`log`, `diff`, `status`, `commit`, `branch`, …) is plain
git on the store.

| Command | Does |
|---|---|
| `agit a push [<git args>]` | Git push, and record the store's `origin` in the binding (credential-stripped) so teammates can clone the agent. |
| `agit a pull [<git args>]` | Fast-forward only. On divergence it refuses rather than textually merge transcripts, and routes you to `agit a merge`. |
| `agit a fetch [<git args>]` | Git fetch, then report which sessions arrived (and from which runtime) and how to integrate them. |
| `agit a clone …` | Refused — a store is cloned by identity. Redirects to `agit a track`. |
| `agit a init` | Refused — a store needs an identity. Redirects to `agit a new`. |
