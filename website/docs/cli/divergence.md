---
sidebar_position: 6
title: Divergence
---

# Divergence

Agent memory diverges, then rejoins. A subagent, a `/branch`, and a fork are the same operation seen at
three scopes, differing only in how far the divergence travels and whether it earns its own identity.
`agit a log` renders the result as a DAG, and `agit a merge` is the universal rejoin.

## The model

| | Scope of divergence | Keeps the aid? | Lifetime |
|---|---|---|---|
| **Subagent** | Within one session | Yes | Ephemeral (dies when it returns its result) |
| **Branch** (`/branch`) | Within one agent: a sibling session sharing a prefix | Yes | Persistent |
| **Fork** | Across agents | No (a new aid) | Persistent |

- A **subagent** is a branch-point that auto-rejoins. The transcript lives where the runtime writes it,
  under the parent session; agit records the branch-point (which parent turn spawned it, and the turn it
  returns into) so it is addressable in the DAG.
- A **branch** is a sibling session that shares a prefix with its parent and then diverges. This is what
  Claude Code's `/branch` produces. agit records the parent session id and the last shared turn.
- A **fork** is a branch whose session was installed under a new aid (a clone plus `agit a rebind
  --new-id`). It keeps its own identity and its own history.

## Capture, not create

The default path is capture. When you run `/branch` in the runtime, the runtime writes a new session that
shares the prefix; agit detects the shared prefix and records the branch-point in the session's committed
sidecar, so `agit a log` renders it and `agit a merge` reconciles it. There are no git branches in the
store and no `refs/heads` proliferation: the store stays single-line, and the DAG is a logical overlay
held as lineage metadata on each session.

## The DAG in `agit a log`

```bash
agit a log
```

`agit a log` renders the store's sessions as a terminal tree. Roots (sessions with no shown parent) print
first in recency order, and each `/branch` child hangs indented beneath the parent it shares a prefix
with. A child shows where it forked from its parent (`branched at <id>`) and its divergent tip prompt
rather than the opening it shares with the parent, so siblings that begin identically are still told
apart. A session that spawned subagents is annotated (for example `+2 subagents`). Every node carries a
short session id.

Pass `--raw` (or `--git`) for the byte-level `git log` on the store instead of the session view.

## Rejoin

`agit a merge` is the rejoin, and it keys on the aid:

- A **branch** rejoins with `agit a merge <session-or-ref>`. The aids match, so the histories fuse and
  the merged session becomes the agent's latest.
- A **fork** rejoins with `agit a merge <agent>`. The aids differ, so the two reconcile by dialogue only
  and each keeps its own history.
- A **subagent** rejoins inside the runtime when it returns its result; agit only records it.

The shared-prefix structure a `/branch` produces is exactly what the splice merge reconciles from. See
[Merging sessions](./merging.md) for how a reconcile runs, and [Concepts](../get-started/concepts.md) for
the aid.
