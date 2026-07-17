---
title: Configuration
nav_order: 7
---

# Configuration

## Environment variables

| Variable | Purpose |
|---|---|
| `AGIT_HOME` | Where agent stores and cross-repo state live. Default `$HOME/.agit`. Stores are at `$AGIT_HOME/agents/<aid>/`. |
| `AGIT_AGENT` | Selects an agent by name or aid for the shell — the second rung of resolution, after `--agent`. |
| `AGIT_LLM` | Model backend for merge synthesis: `claude` (default), `codex`, or a command name (e.g. `ollama run llama3`). |
| `AGIT_LLM_CMD` | A full command, run via `sh -c`, prompt on stdin and result on stdout. Overrides `AGIT_LLM`. |

The LLM backend is only used to synthesize merge conflict lists. If none is available, `agit a merge`
lists open conflicts instead of resolving them; everything else works without a model.

## Files

| Path | Committed to | What it is |
|---|---|---|
| `.agit.toml` | your code repo | The binding: which agents this repo uses, their remotes, and the default. Commit it. |
| `.agit/` | git-ignored | Local per-worktree state, including the active-agent pointer. Not shared. |
| `agent.toml` | the agent's store | Holds the aid. The client mints it; nothing else rewrites it. |

Credentials in a pushed remote or a rebind URL are stripped before they're written to `.agit.toml`; the full
URL with any token stays in the store's local git config only.
