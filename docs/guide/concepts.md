---
title: How it works
nav_order: 3
---

# How it works

Four ideas cover everything else in this guide.

**An agent is a git repo of session transcripts.** It lives under `$AGIT_HOME/agents/<aid>/` (default
`~/.agit`), separate from your code. `agit a <git-command>` runs git against it — `agit a log`,
`agit a diff`, `agit a commit` behave exactly as you'd expect. The `a` is a target selector (the store
instead of your code), not a namespace: most verbs pass straight through to git, and a small closed set
are agit's own (`list`, `use`, `track`, `publish`, `merge`) or git verbs it augments where plain git
would be wrong for transcripts (`push` records the remote for your team, `pull` refuses to textually
merge diverged sessions, `fetch` reports what arrived). `agit a info` exists instead of letting
`agit a show` fall through to `git show`, so the two never collide.

**An agent has a stable identity, the aid** (`agt_<uuid>`), committed in its `agent.toml`. The name and
the remote URL are labels that can change; the aid does not. This is why a remote recreated under the
same name can't silently swap one agent for another, and why tracking an agent gives you the same
agent, not a copy.

**An environment is your code repo**, left untouched. The only thing agit adds to it is `.agit.toml`, a
committed file that records which agents the repo uses and where to get them. A teammate's clone reads
it. `.agit/` (local state) is git-ignored automatically.

**An agent and a code repo are many-to-many.** One agent can work across several repos (its store is
keyed by aid, not tied to a location), and one repo can host several agents.

## Selecting an agent

Commands that act on an agent use, in order: `--agent <name>`, then `$AGIT_AGENT`, then the worktree's
active agent (set by `agit a use`), then the binding's default. If none resolves, the command errors
rather than guessing.
