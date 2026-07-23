---
sidebar_position: 13
title: Command reference
---

# Command reference

Every native `agit` verb, categorized, with a one-line description. Anything not listed is passed through
to git: `agit <git-args>` runs on the code repo, `agit a <git-args>` on the resolved agent's store. See
the [overview](./overview.md) for how the scope selector works.

## Setup and top-level

| Command | Does |
|---|---|
| `agit init [--agent <name>]` | Prepare this repo: clone the agents `.agit.toml` declares (or mint the first with `--agent`), and install the secret hooks on the store. |
| `agit clone <target> [--git] [--no-switch]` | git's clone, made smart about agit-hub agent stores: a positively-identified hub store URL, or a known agent name, is adopted as an agent. `--git` forces a raw git clone. |
| `agit --version` | Print the agit version. |
| `agit help` (also `-h`, `--help`) | Print top-level usage. |

## Capture

| Command | Does |
|---|---|
| `agit a snap [<runtime>] [--from <rt>] [--no-harness] [--watch] [--interval <n>]` | Mirror this project's session dump (and harness) into the store and commit, gated by the secret scan. Captures every runtime present unless one is named. |
| `agit watch [--daemon\|--background] [--stop] [--status] [--no-convert] [--no-harness] [--interval <n>]` | Hands-off: watch both runtimes, auto-snap and auto-convert both ways. `--daemon` runs it in the background. |

See [Capturing sessions](./capturing.md).

## Resume and convert

| Command | Does |
|---|---|
| `agit start [--agent <name>] [--as <runtime>]` | Launch a session already carrying the agent's latest context. |
| `agit resume [<session\|agent>] [--as <rt>] [--cwd <path>] [--env <path>] [--relocate] [--exec]` | Load a recorded session into a runtime and continue it. |
| `agit convert [<session\|agent>] --to <rt> [--from <rt>] [--cwd <path>] [--write]` | Rewrite a session into the other runtime's format. `--watch [--interval <n>]` runs the auto-convert worker. |
| `agit harness [show\|apply] [--from <rt>] [--from-env <path>] [--force]` | Show, or apply, the captured MCP servers, skills, and config. `apply` asks first. |
| `agit adapter` | List the runtimes agit knows. |

See [Resuming sessions](./resuming.md) and [Runtimes](./runtimes.md).

## Reconcile

| Command | Does |
|---|---|
| `agit a merge <target> [--from <rt>] [--both] [--quick] [--splice] [--dry-run]` | Reconcile two diverged memories by dialogue into a resumable merged session. `sync` is an alias. |
| `agit a log [--raw\|--git]` | The store's sessions as a divergence DAG, most recent first. `--raw` falls back to plain `git log`. |
| `agit a diff [<from>] [<to>] [--raw\|--git]` | The prompts and edits added between two refs, not a byte diff. `--raw` falls back to plain `git diff`. |

See [Merging sessions](./merging.md) and [Divergence](./divergence.md).

## Share (agent store)

| Command | Does |
|---|---|
| `agit a push [<remote>\|<url>] [git-push-args]` | Push the store's sessions and record the remote in `.agit.toml`. Scans first; on a hub auth rejection it points at the token page. |
| `agit a pull` | Fast-forward pull; divergence routes to `agit a merge`. |
| `agit a fetch` | Fetch, and report which sessions arrived. |
| `agit a clone [--init] [--no-switch] <name\|url>` | Clone an agent's store by identity; a bare name resolves through `.agit.toml`. `--init` mints a fresh agent into an empty store. |

See [Sharing an agent](../integration/sharing.md).

## Manage agents (`agit a`)

| Command | Does |
|---|---|
| `agit a init <name>` | Mint a new agent (a store with its own identity) and bind it to this repo. |
| `agit a list` | Agents you have locally, with session counts, watcher state, and which is active. |
| `agit a status` | A per-repo overview: agents, active one, session counts, last activity, and the active store's standing against its remote. |
| `agit a switch <name>` | Select this worktree's active agent. |
| `agit a info <name>` | Name, aid, store path, and remote for one agent. |
| `agit a rename <old> <new>` | Rename an agent (the aid is unchanged). |
| `agit a rebind [--remote <url>] [--new-id]` | Repair a binding's identity, or give a fork its own aid. |
| `agit a commit [git-commit-args]` | Commit into the store, scanning the staged index first. |

## Secrets

| Command | Does |
|---|---|
| `agit a scan [--staged] [<file>…]` | Scan session dumps for secrets by hand. |
| `agit a purge-history [--yes]` | Guard-railed history rewrite that scrubs plaintext (or re-seals sessions); prints the force-push command, never auto-pushes. |

See [Secrets](./secrets.md).

## Encryption

| Command | Does |
|---|---|
| `agit a encrypt [--readers a,b] [--public] [--team] [--org <org>] [--yes]` | Enable per-session keybox encryption to the named recipients. |
| `agit a encrypt --export <file>` / `--import <keyfile>` / `--rotate` | Manage the machine-global symmetric key (no-hub setups). |
| `agit a readers add\|rm\|ls <user>\|--public\|--team [--key HEX] [--repin]` | Manage a session's keybox recipients. |
| `agit a rekey` | Rotate the content key and re-seal it to the current recipients. |
| `agit crypt unlock` | Recover this machine's content keys from the committed keybox into the local keyring. |
| `agit a escrow enable` | Opt-in hub-assist key escrow (only under an org in hub-assist mode). |

See [Encryption](./encryption.md).

## Identity and provenance

| Command | Does |
|---|---|
| `agit identity register <you> [--label <name>]` | Print a paste block to enroll this machine's key under a hub account. |
| `agit identity show [<user>]` | This machine's keys and enrollment status, or another account's enrolled device keys. |
| `agit identity keys` | This machine's key details. |
| `agit identity revoke <fpr-or-label>` | Revoke an enrolled key. |
| `agit identity pin <user> [--repin] [--key HEX]` | Pin (or re-pin) an account's registered key set. |
| `agit provenance verify [<session\|agent>] [--repin]` | Check a captured session's signature against its recorded key. |
| `agit provenance key` | Show this machine's signing public key. |

See [Identity and signing keys](./identity.md) and [Provenance](../integration/provenance.md).

## Diagnostics

| Command | Does |
|---|---|
| `agit doctor` | Fast health check and environment summary to paste into a bug report. |
| `agit debug [--out <dir>] [--rerun "<subcmd>"]` | Write a full, redacted diagnostic bundle. Nothing is uploaded. |
| `agit relocate [<session>] [--to <path>] [--yes]` | Bring sessions started in the wrong directory into this repo. |

See [Diagnostics](./diagnostics.md) and [Relocating sessions](./relocating.md).

## Workspace and shell

| Command | Does |
|---|---|
| `agit workspace [log]` | Show the Agent-Environment pairing. |
| `agit workspace restore [N]` | Roll both repos back together to a pairing's joint state. |
| `agit graph` | Show the Workspace-State timeline and relation edges. |
| `agit shadow [install\|uninstall\|status]` | Route `git` through `agit` in your shell (bash/zsh/fish/PowerShell). |
| `agit hub team rekey\|sync <org>` | Rotate or re-seal an org's Team KEK to its members. |

## Git-invoked helpers

These are invoked by git, never typed by hand: `agit credential <get\|store\|erase>` (the hub credential
helper), `agit hook-scan` (the pre-commit/pre-push scan hook), and `agit crypt-clean` / `agit crypt-smudge`
/ `agit crypt-purge-index` (the encryption filter drivers).
