---
title: How it works
nav_order: 3
---

# How it works

Four ideas cover the rest of this guide. Read them in order.

**An agent is a git repo of session transcripts.** It lives at `~/.agit/agents/<aid>/` (under
`$AGIT_HOME`), separate from your code. It's named for what it knows — `frontend`, `payments-api` — not
for a person or a folder. `agit` on its own is plain git on your code repo — `agit commit`, `agit push`
and the rest pass straight through. Put `a` after it and the same git runs against the agent's store
instead: `agit a log`, `agit a diff`, `agit a push`, `agit a pull` all mean what you expect. A few
`agit a` verbs are agent-aware and do a bit more than plain git; those come up where they're relevant.

**An agent has a stable identity: the aid** (`agt_<uuid>`), minted once and committed in the store's
`agent.toml`. The name and the remote URL are mutable labels — a name can be changed or collide, a URL
is just a locator. The aid is the identity. Because `.agit.toml` records the aid, a remote recreated
under the same name can't silently bind you to a different agent.

**Your code repo is untouched.** The one thing agit adds is `.agit.toml`, a committed file that declares
which agents the repo uses and where to clone them. That's the binding, and a teammate's clone reads it
to pull the same agents. Local per-worktree state under `.agit/` is git-ignored.

**Agents and repos are many-to-many.** One agent can work across several repos — its store is keyed by
aid, not tied to a location — and one repo can host several agents.

## Selecting an agent

A command that acts on an agent resolves which one, in order:

1. `--agent <name>` on the command
2. `$AGIT_AGENT` in the environment
3. the worktree's active agent, set by `agit a switch <name>`
4. the binding's default in `.agit.toml`

If none of these resolves, the command errors instead of guessing.
