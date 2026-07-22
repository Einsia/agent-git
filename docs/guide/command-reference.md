---
title: Command reference
nav_order: 12
---

# Command reference

Anything not listed here is passed through to git: `agit <git-args>` runs on the code repo,
`agit a <git-args>` on the resolved agent's store.

## Working with sessions

| Command | Does |
|---|---|
| `agit init [--agent <name>]` | Set up this repo: clone the agents `.agit.toml` declares, or mint the first with `--agent`. |
| `agit start [--agent <name>] [--as <runtime>]` | Launch a session already carrying the agent's context. |
| `agit snap [--from <runtime>]` | Capture this project's sessions into the store by hand: mirror and commit them in one gated step (a suspected secret is mirrored to disk but held out of history). |
| `agit watch [--daemon\|--stop\|--status] [--no-convert]` | Hands-off auto-snap and auto-convert; `--daemon` runs it in the background. |
| `agit convert <src> --to <runtime> [--write]` | Rewrite a session into the other runtime's format. |
| `agit resume <src> [--as <runtime>] [--exec]` | Load a session into a runtime and continue it. |
| `agit adapter` | List the runtimes agit knows. |
| `agit harness [apply]` | Show, or apply, an agent's captured MCP servers, skills, and config. |
| `agit shadow install\|uninstall\|status` | Route `git` through `agit` in your shell (bash/zsh/fish/PowerShell). |
| `agit scan` | Run the secret scan by hand. |
| `agit provenance key` | Show this machine's signing identity (an ed25519 key, minted on first use). |
| `agit provenance verify <session>` | Self-verify a captured session's signature. See [Verify who produced a session](provenance.html). |
| `agit identity register <you>` | Print a paste-able block to enroll this machine's key under your hub account. |
| `agit identity show` | Show this machine's key fingerprints and enrollment status. |

Set up capture once with `agit watch --daemon`: it snapshots new sessions as you work and converts
them between runtimes so either CLI can resume them. `agit snap` is the manual alternative, and mirrors
and commits in one gated step just as the daemon does.

`agit a commit`, `agit a push`, and every snapshot scan the content for secrets in-process before
handing off to git, so the scan holds even when git's own hook is skipped. The visible override is
`AGIT_ALLOW_SECRETS=1`; see [Keep secrets out of shared history](secrets.html).

## Managing agents (`agit a`)

| Command | Does |
|---|---|
| `agit a list` | Agents you have locally, with session counts and which is active. |
| `agit a status` | A per-repo overview: which agents this repo works with, which is active, each one's session count, last activity, and live-watcher state, and where the active store stands against its remote (unpushed, behind, or diverged). |
| `agit a init <name>` | Add another agent to this repo (a store with its own identity). |
| `agit a switch <name>` | Select this worktree's active agent. |
| `agit a clone <name\|url>` | Clone an agent's store by identity; a bare name resolves through `.agit.toml`. |
| `agit a info <name>` | Name, aid, store path, and remote for one agent. |
| `agit a rename <old> <new>` | Rename an agent. |
| `agit a log [--raw\|--git]` | The store's sessions as a timeline, most recent first: runtime, when, where it ran, its opening prompt, and its tool activity. `--raw` (or `--git`) falls back to a plain `git log` on the store. |
| `agit a diff [<from>] [<to>] [--raw\|--git]` | The session-level change between two refs: the prompts and edits added, not a line-by-line diff of the transcript bytes. With no refs it uses this repo's unpushed range. `--raw` (or `--git`) falls back to a plain `git diff`. |
| `agit a push [<remote>\|<url>]` | Push the store's sessions and record the remote in `.agit.toml` (credentials stripped). With no refspec it pushes the current branch (a fresh store needs no `-u`); with no remote it fans out to every bound remote, naming each. Scans for secrets first, and on a hub push rejected for auth it points at the hub's token page. |
| `agit a pull` | Fast-forward pull; divergence routes to `agit a merge`. |
| `agit a fetch` | Fetch, and report which sessions arrived. |
| `agit a rebind [--remote <url>] [--new-id]` | Repair a binding's identity, or give a fork its own aid. See [Migration](migration.html). |
| `agit a merge <target> [--from <runtime>] [--both] [--quick] [--splice] [--dry-run]` | Reconcile two memories by dialogue into a resumable merged session. `--splice` skips the model and just combines both sides into one session; `--dry-run` (alias `--preview`) shows what a merge would do without running it. See [Reconcile diverged sessions](merging.html). |

`agit a log` and `agit a diff` render the SESSION view of the store by default, because a raw `git
log`/`git diff` there is a wall of transcript bytes. `--raw` (or `--git`) is the escape hatch back to
real git, so scripted `--format` output still works.
