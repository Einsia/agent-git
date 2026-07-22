# A coherent model for subagents, branches, and forks

Date: 2026-07-22
Status: design validated in brainstorm; not yet implemented

## The idea

Subagents, `/branch` (Claude Code's conversation branch), and forks are one operation seen
at three scopes: **agent memory diverges, then rejoins**. They differ by only two attributes.

| | scope of divergence | keeps the aid? | lifetime |
|---|---|---|---|
| **subagent** | within one session | yes | ephemeral (dies when it returns its result) |
| **branch** (`/branch`) | within one agent (a sibling session sharing a prefix) | yes | persistent |
| **fork** | across agents | no (new aid) | persistent |

`subagent -> branch -> fork` is one spectrum, ordered by how far the divergence travels and
whether it earns its own identity.

## The rejoin already exists

`agit a merge` is the universal rejoin and already keys on the aid:

- **Fuse** when the aids match (same agent; histories become one) -> the branch and subagent case.
- **DialogueOnly** when they differ (distinct agents) -> the fork case.

The shared-prefix reconciliation splice-merge uses (`src/sync.rs`, "the shared prefix is the
common ancestor both sessions forked from") is exactly the structure a `/branch` produces.

## Storage: lineage metadata, not git branches

Everything is a **session**; sessions form a **DAG through recorded branch-points**, held as
lineage metadata in each session's committed sidecar. There are **no git branches in the
store** and no `refs/heads` proliferation. The store stays single-line (`main`); the DAG is a
logical overlay. One representation covers all three:

- **subagent** = a branch-point that auto-rejoins. Keep the transcript where it already lives
  (`<parent-id>/subagents/*.jsonl`), but record the branch-point (parent turn -> subagent id ->
  the turn it returns into) as lineage on the parent, so it is addressable in the DAG instead
  of an opaque nested file.
- **branch** = a sibling session that shares a prefix with its parent and diverges; record the
  parent session id + the branch-point (the last shared turn).
- **fork** = a branch whose sibling session was installed under a new aid (clone + `rebind
  --new-id`); the existing `forked_from` / `forked_from_aid` hub lineage carries it.

## Capture first

The default path is **capture, not create**. When the user runs `/branch` in the runtime, the
runtime writes a new session that shares the prefix; agit's job is to **detect the shared
prefix and record the branch-point** so `agit a log` and the hub render it and `agit a merge`
reconciles it. agit already has the shared-prefix detection, so this is nearly free and works
with the runtime's own `/branch`. No new command is required for this path.

Open implementation question: confirm exactly how a runtime `/branch` manifests in the
transcript (Claude Code's `sessionId` / `parentUuid` linkage; codex's equivalent) so detection
is exact rather than only prefix-inferred. Verify against a real dumped `/branch` at build time.

## The one thin verb: `agit branch <session>`

A deliberate / cross-runtime branch, as **sugar over the existing `agit resume`-from-a-point
machinery**, not a new subsystem. Its value: gives codex (no native `/branch`) the same move,
records exact lineage deterministically, and is scriptable.

**UX rule: no turn indices, ever.**

- `agit branch <session>` with no selector = branch from the **tip** (the common case; parallel
  continuation). Launches or prints the resume command like `agit resume` does.
- Branch from an **earlier** point by human landmark, never a number:
  - interactive: print the session's **user prompts** as mile-markers; pick "branch after this";
  - scriptable: `--from "<prompt text>"` fuzzy-matches a prompt.
- `--as <runtime>` to branch into the other runtime; `--exec` to launch immediately, matching
  `agit resume`.

The branched session is captured like any other (the auto-snap path), so it becomes the agent's
latest and shows in the DAG with its branch-point recorded.

## The hub renders the DAG

The hub already renders a per-session spine. Add a **DAG view** of an agent's memory: the trunk
plus branch-points (subagents that fork-and-return, sibling `/branch` sessions, fork-out edges to
other agents) and the merge-back edges `agit a merge` records. Reuse the spine per node, arranged
as a graph. `GET /api/agent/<owner>/<name>` gains a lineage/DAG shape; a page renders it.

## Rejoin, end to end

- branch rejoin: `agit a merge <session-or-ref>` -> Fuse (same aid, shared prefix -> splice or
  dialogue), the merged session becomes latest (auto-snap, already built).
- fork rejoin: `agit a merge <agent>` -> DialogueOnly (already built).
- subagent rejoin: happens inside the runtime; agit only records it.

## Build order (each a gated, reviewed wave)

1. **Lineage capture + `a log` DAG** (client): detect runtime `/branch` via shared prefix,
   record branch-point lineage in the sidecar, surface it in `agit a log`. Record subagent
   branch-points too. No new user command.
2. **`agit branch <session>`** (client): the thin verb over resume-from-a-point, tip-default,
   landmark/prompt-match selection, no turn index.
3. **Hub DAG view**: lineage in the agent detail API + a graph page.
4. **`agit fork <name>`** (optional sugar): first-class verb over clone + `rebind --new-id`,
   already-tracked lineage.

## Non-goals

- No git branches in the agent store (the DAG is metadata, not `refs/heads`).
- agit does not reimplement the runtime's `/branch`; it captures it and adds one deliberate verb.
- No turn-index UX anywhere.
