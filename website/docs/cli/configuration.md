---
sidebar_position: 12
title: Configuration
---

# Configuration

agit reads a handful of environment variables and keeps its per-repo state in a few well-known files. All
of it has working defaults, so nothing here needs setting before you start. The `agit-hub` server reads
its own separate set; see [Self-hosting configuration](../self-hosting/configuration.md).

## Environment variables

| Variable | Purpose |
|---|---|
| `AGIT_HOME` | Where agent stores and cross-repo state live. Default `~/.agit`. Each store sits under `$AGIT_HOME/agents/<aid>/`, and this machine's signing key under `$AGIT_HOME/identity/`. |
| `AGIT_AGENT` | Selects an agent for the shell, by name or aid. It ranks below `--agent` and above the worktree's active agent (see [Concepts](../get-started/concepts.md) for the full order). |
| `AGIT_HUB_URL` | Names the hub for API calls (identity, provenance lookups) and marks its host as "a hub" for the git credential helper. When unset, the active agent's primary remote is used instead. |
| `AGIT_HUB_TOKEN` | A bearer token for the hub API, overriding any credential parsed from the remote URL. |
| `AGIT_HUB_USER` | The hub account the credential helper authenticates as, overriding the account remembered at `agit identity register` time. See [Authentication](../integration/authentication.md). |
| `AGIT_ALLOW_SECRETS` | The visible override for the secret scan. Set to `1` (or `true`/`yes`) to let a commit, push, or snapshot through despite suspected secrets. Disclosed on every use, unlike git's `--no-verify`. See [Secrets](./secrets.md). |
| `AGIT_LLM` | Backend for merge synthesis: `claude` (default), `codex`, or a command name (e.g. `ollama run llama3`). |
| `AGIT_LLM_CMD` | A full command, run via `sh -c` with the prompt on stdin and the result on stdout. Overrides `AGIT_LLM`. |

The LLM backend does one job: synthesizing the conflict list at the end of `agit a merge` (see
[Merging sessions](./merging.md)). With no backend available, `agit a merge` lists the open conflicts
instead of resolving them, and every other command runs without a model.

`AGIT_HUB_PUBLIC_URL` is a server-side variable read by `agit-hub`, not the CLI. See
[Self-hosting configuration](../self-hosting/configuration.md).

## Files

| Path | Location | What it is |
|---|---|---|
| `.agit.toml` | your code repo, committed | The binding: which agents this repo uses and where to clone them. Commit it so teammates get the agents. |
| `.agit/` | your code repo, git-ignored | Local per-worktree state, including the active-agent pointer that `agit a switch` sets. Not shared. |
| `agent.toml` | the agent's store | Holds the aid. The client mints it once; nothing else rewrites it. |
| `.agit-allow-secrets` | the agent's store | The secret-scan allowlist: known-safe strings the scan should not flag. See [Secrets](./secrets.md). |
| `.agit/keybox.jsonl` | the agent's store | The keybox: one sealed envelope of each session content key per recipient. See [Encryption](./encryption.md). |
| `$AGIT_HOME/identity/ed25519` | your machine | The per-machine ed25519 signing key (private key `0600`) that provenance signs sessions with. Minted on first use. See [Identity and signing keys](./identity.md). |
| `$AGIT_HOME/identity/hub-account` | your machine | The hub account the credential helper authenticates as, written by `agit identity register`. `AGIT_HUB_USER` overrides it. |
| `sessions/sync/*.decisions.md` | the agent's store | The merge conflict ledger: the accept, custom, and defer decisions from each `agit a merge`, beside the merge transcript. See [Merging sessions](./merging.md). |

Credentials in a URL you push to or rebind against are stripped before they reach `.agit.toml`. The full
URL, token included, stays only in the store's local git config.

An in-file `agit:allow-secret` pragma marks a single line as a false positive, the line-level counterpart
to the `.agit-allow-secrets` allowlist. Both are covered under [Secrets](./secrets.md).
