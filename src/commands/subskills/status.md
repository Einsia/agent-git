---
name: agit-status
description: Show current context, directory bindings, adopted sessions, and local Agent repo state.
---

# agit status

## Purpose

Answer "who am I, am I in a session, and is the repo synchronized?" It reports workspace bindings, adopted runtime sessions, local branches, and their relationship to fetched tracking refs.

Default status reads local state without checking for updates, creating storage, or migrating history.
Remote state means the refs already fetched locally. If interrupted storage recovery is pending,
status refuses. Complete recovery through the original AgentGit store before inspecting status
again; its normal `agit doctor` startup can recover supported local checkouts. Status does not
change the migration rules for repositories linked from another store.

## Synopsis

```bash
agit status
```

## Options

| Option | Meaning |
|---|---|
| `--check-missing` | Also inspect runtime indexes for unadopted sessions; SQLite may maintain WAL sidecar files |
| `--limit <n>` / `--offset <n>` | Page adopted sessions; limit 1–1000, default 8 for text and 100 for JSON |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Reading the output

The explicit `--check-missing` option uses the runtime index readers and may cause SQLite to
maintain its sidecar files. Use default status when inspection must leave local files unchanged.
Neither form performs a startup migration or an update check.
If the claim inventory is incomplete, `--check-missing` reports adoption as unavailable and skips
the runtime indexes. An omitted or unreadable claim is not evidence that a session is unadopted.

The adopted-session table names the complete runtime instance, recorded repository and branch,
and the last commit currently observed on that branch. A superseded instance does not borrow its
replacement's branch head. A retained merge exploration instance is labeled separately and does
not borrow ordinary pending activity or the branch head; a superseded exploration instance keeps
the superseded label. Displayed current claims are compared with bounded read-only native
snapshots using the same unsettled-event semantics as working-state `agit diff`. Counts distinguish
turns with pending activity from newly started turns; unclassified or incomplete native records
remain explicit lower bounds. Missing, busy, stale, conflicting or oversized evidence is unavailable,
never an inferred zero. Native text is not printed.

Default status never opens a source runtime database through SQLite: even a read-only SQLite
connection can create WAL coordination files. Codex inspection locates the full native identity
in its JSONL tree without consulting the index. OpenCode reads pinned database and sidecar handles
under cooperating locks, verifies the committed WAL frontier, and queries an owned in-memory image
in a supervised worker. It does not write the source database, WAL/SHM files, or temporary snapshots.
Missing, changing, unsafe, or over-budget evidence remains unavailable without recovery.

Each page inspects native details for at most its first eight session rows. Additional
rows preserve their identities and say `unavailable: per-page inspection limit`; use
`--offset` to inspect another page. Text and JSON share this bound. JSON session rows
include `last_commit` (null when unavailable), `pending_activity`, and `local_instance`.
`sessions.incomplete=true` means the total is only the number of claims observed, not a verified
current count or a guaranteed lower bound. A failed final inventory recheck also prevents
unadopted-session classification.

The session inventory and displayed-row inspections share command budgets. Status does not read
undisplayed native transcripts, and it warns when the local claim inventory is incomplete. Only a
missing Store is an empty inventory. An invalid, linked, or unreadable Store carrier remains
incomplete, and `--check-missing` does not inspect runtime indexes in that state. The
explicit `AGIT_SESSION` target uses this same snapshot for the runtime-identity veto. If the process
carries a runtime identity and its claims cannot be inspected completely, the target is unavailable;
runtime or directory discovery never supplies a replacement target. Without a runtime identity,
the explicitly supplied target can still be displayed independently of the claim inventory. The
"current claim" badge describes an ordinary stored identity that has not been superseded. "Busy claim
update" observes a held local claim lock; "stale baseline" compares the materialized tip with the
selected branch head. None of these badges proves a live native process or a remote instance.

With `--json`, `result.value` contains typed selection, workspace binding,
session identities, repository heads, and fetched-ref synchronization counts.
`sessions.next_offset` continues the session page; it is null when no rows remain.
Session IDs are complete and paired with their runtime, and superseded claims
remain explicitly marked. A bound repository does not populate `selection`.
`unadopted.checked=false` and `sessions=null` mean the optional index scan did
not run; they are not evidence that every runtime session is adopted.

Explicit discovery also works before the first adoption, without creating an
AgentGit store. Runtime and native ID together identify a session; an adopted
Claude ID does not hide an equal Codex ID. Known index failures are reported in
`unadopted.errors` and mark `unadopted.incomplete=true`. Discovery reflects the
runtime indexes available to this machine, rather than proving that no other
conversation exists.

- `no session target supplied through AGIT_SESSION`: no usable explicit process identity.
- `bound repo`: the cwd's Agent repo route; it does not prove that a branch exists.

The repository table lists branches even when no runtime session has been adopted. Each row
includes the last commit and its tracking ref. An explicit upstream takes precedence; otherwise
a fetched `origin` branch with the same name supplies the comparison. Remote-only branches from
any remote remain visible. Status does not contact those remotes.

- `in sync`: the local branch and its known tracking ref identify the same commit.
- `ahead`, `behind`, or `diverged`: complete local commit ancestry establishes the difference.
- `no known tracking ref`: no upstream or same-name fetched `origin` branch is known locally.
- `tracking ref unavailable locally`: the configured upstream is missing or cannot be mapped to a local ref; status does not substitute an origin branch.
- `comparison unavailable`: local ancestry is missing, malformed, or exceeds the inspection budget.
- `remote only`: a fetched remote branch has no corresponding local branch in the table.

Status bounds the displayed rows and ancestry inspection. An incomplete display includes a warning;
use `agit branch --repo <owner/repo> --all` to inspect another repository's refs. A missing tracking
ref does not establish whether the branch has ever been published. Shallow or grafted history
produces counts only when the immutable parent graph can be reconstructed completely.

## Examples

```bash
agit status
agit status --check-missing
agit -C ~/Projects/p1 status
```

A bound repo does not select a session target. Use a full target or set `AGIT_SESSION` to
an existing local branch. Use `new` to create a session or `import` to adopt one;
`resume` continues an existing session. Verify `refs/heads/<branch>` before publishing it.
