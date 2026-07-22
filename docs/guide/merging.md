---
title: Reconcile diverged sessions
nav_order: 7
---

# Reconcile diverged sessions

When two people run the same agent and both push, their sessions diverge. `agit a merge` reconciles
them. It does not diff the transcripts line by line. Instead it revives both sides' latest sessions
read-only, has them compare their work against the current code, and produces a resumable merged session
plus a list of genuine conflicts.

You usually reach a merge from `agit a pull`: when a teammate's sessions fast-forward yours, pull applies
them; when the two histories have diverged, it stops and sends you here.

```
agit a merge frontend
```

The target is another agent (an agent name), or a git ref in the store. A bare name resolves through the
[binding](concepts.html); a ref merges against an earlier point in the same store. The target is never a
code branch.

## What a merge produces

Each side's most recent session is revived read-only in its own worktree, carrying the diff it introduced
since the two histories' common ancestor. The two sessions exchange summaries and reconcile what they can
by reading the current code. What they cannot reconcile (a decision one made that the other's changes
contradict) is surfaced as a conflict for you to decide.

Because both sides are revived as real sessions, the runtime CLI (`claude` or `codex`) must be installed.
Which runtime revives them follows the usual rule: the one you name with `--from`, else the only one
present, else agit asks (see [Move a session between runtimes](runtimes.html)).

The output is a session you resume like any other:

```
claude --resume <id>
codex exec resume <id>
```

A successful merge records that session into the store as the agent's latest, so a bare `agit start` or
`agit resume` picks it up. You do not snap it by hand. If a secret slips into the merged transcript, the
secret gate holds it out of history and the merge exits non-zero, exactly as a snap would (see
[Keep secrets out of shared history](secrets.html)).

## Same agent versus different agent

The target's aid decides how far the merge goes:

| Target | Reconciled | Git histories |
|---|---|---|
| Same aid (the same agent, such as a teammate's pushed copy) | by dialogue | merged into one history |
| Different aid (a distinct agent) | by dialogue | left intact; each keeps its own |

Merging by aid is why a teammate pulling your sessions and you pulling theirs converge on one history,
while merging two agents that happen to share a name does not fold them together. Identity is the aid,
not the name. See [Concepts](concepts.html).

## The conflict ledger

When a merge surfaces conflicts, you decide each one: accept an option the dialogue named, type your own
decision, or leave it open for now. Those decisions are written to a ledger beside the merge transcript,
at `<agent>/sessions/sync/<a>-<b>.decisions.md`, so what was settled and what was deliberately deferred
live in the store, not only inside the resumed session. The settled decisions are also folded into the
merged session, so when you resume it the agent continues with them already decided.

## Merge runs through a model

The conflict synthesis runs through a model, selected by `AGIT_LLM` (see
[Configuration](configuration.html)). If no model is available, `agit a merge` lists the open conflicts
instead of resolving them, and every other step still runs.

Because a model produces the result, the result is not deterministic: run the same merge twice and the
merged session may differ. The raw sessions on both sides stay committed and versioned in the store, so
they remain the source of truth. A merge you do not like is one you can drop and redo.

## Options

| Option | Result |
|---|---|
| `--from <runtime>` | Pick which runtime revives the sessions when both sides have sessions present. |
| `--both` | Write the merged session onto both branches instead of one. |
| `--quick` | Skip the summary exchange and reconcile from the diffs alone. Faster, less thorough. |
| `--splice` | Combine both sides into one session without a model. See below. |
| `--dry-run` (alias `--preview`) | Show what the merge would do without running it. See below. |

## Preview a merge with `--dry-run`

```
agit a merge frontend --dry-run
```

`--dry-run` (alias `--preview`) runs only the inspection phases the real merge starts with (resolve the
target, read each side's sessions, pick the voice session per side) and prints the plan: the target, the
mode (same agent, which reconciles then merges the git histories, or a different agent, which reconciles
by dialogue only), how many sessions each side has, and the voice session picked for each. It stops
there. No model runs, no session is revived, and no transcript or ledger is written, so it needs neither
`AGIT_LLM` nor the runtime CLI. Rerun without `--dry-run` to carry it out.

## Combine both sides with `--splice`

```
agit a merge peer --splice
```

`--splice` skips the model entirely. Instead of reconciling the two sides, it combines them: it takes A's
full transcript, appends B's tail from the point the two forked, normalizes the session id and working
directory onto one, and installs the result as a single new session. Nothing is reconciled and no
conflicts are computed. You resume the combined session, and the agent reads both sides' context in one
window and takes it from there.

Because it runs no model and revives nothing, `--splice` needs neither `AGIT_LLM` nor the runtime CLI,
and it is deterministic: the same two sessions always splice to the same bytes. A same-aid target still
merges the git histories afterward, as with a normal merge. Use it when you would rather hand the whole
picture to a fresh agent than have two revived ones negotiate, or when no model is configured.
