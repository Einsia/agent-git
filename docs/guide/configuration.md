---
title: Configuration
nav_order: 13
---

# Configuration

agit reads a handful of environment variables and keeps its per-repo state in a few well-known files.
All of it has working defaults, so nothing here needs setting before you start. The `agit-hub` server
reads its own set of variables, listed separately below.

## Environment variables

| Variable | Purpose |
|---|---|
| `AGIT_HOME` | Where agent stores and cross-repo state live. Default `~/.agit`. Each store sits at `$AGIT_HOME/agents/<aid>/`. |
| `AGIT_AGENT` | Selects an agent for the shell, by name or aid. It ranks below `--agent` and above the worktree's active agent ([Concepts](concepts.html) has the full resolution order). |
| `AGIT_LLM` | Backend for merge synthesis: `claude` (default), `codex`, or a command name (e.g. `ollama run llama3`). |
| `AGIT_LLM_CMD` | A full command, run via `sh -c` with the prompt on stdin and the result on stdout. Overrides `AGIT_LLM`. |
| `AGIT_ALLOW_SECRETS` | The visible override for the secret scan. Set to `1` (or `true`/`yes`) to let a commit, push, or snapshot through despite suspected secrets. Disclosed on every use, unlike git's `--no-verify`. See [Keep secrets out of shared history](secrets.html). |

The LLM backend does one job: synthesizing the conflict list at the end of `agit a merge` (see
[Reconcile diverged sessions](merging.html)). With no backend available, `agit a merge` lists the open conflicts instead of
resolving them, and every other command runs without a model.

## Hub environment variables

The `agit-hub` server (see [Self-host the hub](../deploying-the-hub.html)) reads these. Leave them all
unset for a zero-config self-host: SQLite for metadata, the local filesystem for blobs, invite-only
signup.

| Variable | Purpose |
|---|---|
| `AGIT_HUB_DB` | A `postgres://` URL selects the Postgres backend (production). Unset (or any non-URL value) selects the default SQLite `hub.db` under the data root. |
| `AGIT_HUB_REGISTRATION` | Enables self-service registration (`POST /api/register`) when set to `1`/`true`/`open`/`yes`. Off by default (invite-only). The `--open-registration` flag does the same. |
| `AGIT_HUB_S3_ENDPOINT` | Set (non-empty) to store blobs in S3/Garage instead of the local filesystem. Selects the blob backend independently of `AGIT_HUB_DB`. |
| `AGIT_HUB_S3_BUCKET` | The bucket blobs are stored in. Required when `AGIT_HUB_S3_ENDPOINT` is set. |
| `AGIT_HUB_S3_ACCESS_KEY`, `AGIT_HUB_S3_SECRET_KEY` | The S3 credentials. Required when `AGIT_HUB_S3_ENDPOINT` is set. |
| `AGIT_HUB_S3_REGION` | The S3 region name. Defaults to `garage`. |

An `AGIT_HUB_S3_ENDPOINT` set with any of the bucket or key values missing is an error at boot, not a
silent fall back to local disk.

## Files

| Path | Location | What it is |
|---|---|---|
| `.agit.toml` | your code repo, committed | The binding: which agents this repo uses and where to clone them. Commit it so teammates get the agents. |
| `.agit/` | your code repo, git-ignored | Local per-worktree state, including the active-agent pointer that `agit a switch` sets. Not shared. |
| `agent.toml` | the agent's store | Holds the aid. The client mints it once; nothing else rewrites it. |
| `.agit-allow-secrets` | the agent's store | The secret-scan allowlist: known-safe strings the scan should not flag. See [Keep secrets out of shared history](secrets.html). |
| `$AGIT_HOME/identity/ed25519` | your machine | The per-machine ed25519 signing key (private key `0600`) that provenance signs sessions with. Minted on first use. See [Verify who produced a session](provenance.html). |
| `sessions/sync/*.decisions.md` | the agent's store | The merge conflict ledger: the accept, custom, and defer decisions from each `agit a merge`, beside the merge transcript. See [Reconcile diverged sessions](merging.html). |

Credentials in a URL you push to or rebind against are stripped before they reach `.agit.toml`. The
full URL, token included, stays only in the store's local git config.

An in-file `agit:allow-secret` pragma marks a single line as a false positive, the line-level
counterpart to the `.agit-allow-secrets` allowlist. Both are covered under
[Keep secrets out of shared history](secrets.html).
