---
title: Configuration
nav_order: 7
---

# Configuration

agit reads four environment variables and keeps its per-repo state in three files. All of it has
working defaults, so nothing here needs setting before you start.

## Environment variables

| Variable | Purpose |
|---|---|
| `AGIT_HOME` | Where agent stores and cross-repo state live. Default `~/.agit`. Each store sits at `$AGIT_HOME/agents/<aid>/`. |
| `AGIT_AGENT` | Selects an agent for the shell, by name or aid. It ranks below `--agent` and above the worktree's active agent — [How it works](concepts.html) has the full resolution order. |
| `AGIT_LLM` | Backend for merge synthesis: `claude` (default), `codex`, or a command name (e.g. `ollama run llama3`). |
| `AGIT_LLM_CMD` | A full command, run via `sh -c` with the prompt on stdin and the result on stdout. Overrides `AGIT_LLM`. |

The LLM backend does one job: synthesizing the conflict list at the end of `agit a merge` (see
[Merging](merging.html)). With no backend available, `agit a merge` lists the open conflicts instead of
resolving them, and every other command runs without a model.

## Files

| Path | Location | What it is |
|---|---|---|
| `.agit.toml` | your code repo, committed | The binding: which agents this repo uses and where to clone them. Commit it so teammates get the agents. |
| `.agit/` | your code repo, git-ignored | Local per-worktree state, including the active-agent pointer that `agit a switch` sets. Not shared. |
| `agent.toml` | the agent's store | Holds the aid. The client mints it once; nothing else rewrites it. |

Credentials in a URL you push to or rebind against are stripped before they reach `.agit.toml`. The
full URL, token included, stays only in the store's local git config.
