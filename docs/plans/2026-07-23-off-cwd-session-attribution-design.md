# Attributing off-cwd sessions to a repo

Date: 2026-07-23
Status: accepted

## Problem

agit captures agent sessions into a per-repo agent store. Today it owns a session only when the session's
recorded cwd equals the repo root: capture slugs the repo root and reads the runtime's session folder for
that slug. A very common workflow breaks this: people launch the agent from somewhere other than the repo
root, and consistently so, so their sessions are never owned.

- Launched in a subdirectory (`cd frontend && claude`).
- Launched in a parent / workspace dir (`~/dev`, with the repo at `~/dev/app`).

Both are currently dropped. `agit relocate` can pull them in, but it is a manual, per-run step, and a user
does not discover it until they notice their sessions are missing.

## Signals we have, and one we do not

The runtime transcript records the session `cwd` and the git BRANCH NAME (`gitBranch` for claude-code,
`payload.git.branch` for codex). Branch name does not identify a repo (every repo has `main`), and no commit
or repo root is recorded. So the only usable signals are:

1. the session cwd (a path), and
2. the file paths the session touched (`Edit`/`Write`/`MultiEdit` = edited; `Read`/`Grep`/`Glob` = read),
   resolved against the session cwd.

There is no reliable git-identity signal, and no fully-automatic rule resolves a read-only session launched
above the repo, because a read is noisy (an agent greps across a workspace, opens a shared config). That
case is irreducibly ambiguous and must involve a human.

## Design: ownership tiers

`snap`, `watch`, and the stranded/relocate paths classify a candidate session against a repo rooted at
`env`:

| cwd relation to `env` | activity | verdict |
|---|---|---|
| cwd == `env`, or cwd is UNDER `env` (git walk-up: same work-tree) | anything, incl. read-only | OWNED, auto-capture |
| cwd is a strict PARENT of `env`, or outside it | the session EDITED a file under `env` | OWNED, auto-capture (reported) |
| cwd is a strict PARENT of `env`, or outside it | read-only under `env`, or it also edited another repo | CANDIDATE, surfaced (not auto) |

- Tier 1 mirrors git: run from inside the repo, it is this repo, regardless of read vs edit. This alone
  fixes the subdir case, including read-only review sessions run from a subdir.
- Tier 2 keys off demonstrated work: an edit under `env` is strong evidence the session worked here, so it
  is owned even when launched from above. Reported (`captured N sessions that ran in <dir>, edited here`),
  never silent.
- Tier 3 is the ambiguous residue. Surfaced, never auto-claimed: interactive `snap`/`watch` asks a targeted
  one-key question (`1 session ran in ~/dev and read files here but edited none. Capture for this repo?
  [y/N]`); non-interactive runs list it and leave it for `agit relocate`.

A session that edited two repos is owned by each (it worked on both); it is captured into each repo's store
when you snap there, and the report says so.

## Candidate set (bounding the scan)

The existing `stranded_sessions(env)` already enumerates sessions across launch dirs and reports each
session's TRUE recorded cwd (read from the transcript, not decoded from the folder slug, which is lossy:
`/my/app`, `/my-app`, `/my_app` all slug the same). This wave reuses that enumeration and adds the
touched-files tier classification on top. Candidates are scoped to sessions whose recorded cwd is UNDER
`env` (subdir, git walk-up confirms same work-tree) or a strict PARENT/ancestor of `env`.

A session launched in an unrelated sibling that edited `env` via an absolute path is not in this set;
`agit relocate` remains the manual escape hatch for it. Scanning all folders to catch it is too costly and
not worth it.

## Implementation

- `convo`/adapter: extract the EDITED and READ file path sets from a transcript (parse the tool-use blocks;
  resolve relative paths against the session cwd). Reuse the existing transcript parse.
- A classifier `session_tier(session_cwd, touched, env) -> Owned | Candidate | Unrelated` implementing the
  table. `plausibly_here` already computes the parent/same-repo relation; extend with the touched-files
  check for tier 2 vs 3.
- `snap`/`sync`/`watch`: after the exact-cwd capture, walk the candidate set, auto-capture tier 1-2 (via the
  existing relocate install + gated commit path, so provenance and the secret gate are unchanged), and
  surface tier 3 (interactive prompt, or add to the stranded list for the non-interactive note).
- `relocate` keeps listing every plausibly-here session (all tiers) for explicit, batch, or cross-dir moves.

## Testing

- Tier 1: a read-only session in a subdir is captured; a session in the repo root still is.
- Tier 2: a parent-launched session that edited a file under `env` is captured; one that edited only a
  sibling repo is not captured for `env`.
- Tier 3: a parent-launched read-only session that read files under `env` is NOT auto-captured; it appears
  as a candidate. A session spanning two repos is owned by both.
- Path resolution: relative edited paths resolve against the session cwd before the under-`env` check.
- The secret gate and provenance still run on every auto-captured session (no bypass).

## Out of scope

- Machine-wide capture with repo-as-a-view (a larger rework; noted as a future direction).
- Attribution for a session launched in an unrelated dir that edited this repo by absolute path
  (`agit relocate` handles it).
