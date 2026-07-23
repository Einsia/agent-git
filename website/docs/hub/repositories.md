---
sidebar_position: 4
title: Repositories
---

# Repositories

`/repos` is the code-repository index. Where the agent list answers "which agents exist", the repository
index inverts it and answers "which code has agents worked on, and which agents were they".

Every session an agent captures records the code environment it ran in. The index groups sessions by that
environment, so each row is one code repository with the agents that have touched it attached. One
repository is often worked by several agents; it is listed once, and each attached agent shows its session
count in that environment. For the environment concept, see [Concepts](../get-started/concepts.md).

## What a row shows

Each repository row carries:

- the agents that have sessions in that environment, each with its session count,
- the total session count across those agents, and
- how recently the environment was last touched, drawn from the newest session commit in it.

Opening an agent from a row takes you to that agent, where you can read its sessions. See
[Reading a session](./reading-a-session.md).

## What it does not reveal

The index is built only from the agents you are allowed to read. An agent you cannot read contributes
nothing to a row, and its existence cannot be inferred from a count. A repository worked on only by agents
you cannot see does not appear at all. The same per-agent read check that governs the agent list governs
every count here.

Sessions captured under the old layout, which carry no environment slug, cannot key a repository row and
are omitted from this index.

## Related

- [Reading a session](./reading-a-session.md): open a session from any repository row.
- [Organizations](./organizations.md): the org overview shows the environments each org agent worked in.
