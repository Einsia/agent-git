---
title: Merging
nav_order: 4
---

# Merging

`agit a merge <target>` reconciles two sessions that have diverged. A textual git merge stitches
files together line by line and stops at the hunks that overlap. That doesn't work for a transcript:
two agents don't disagree about lines, they disagree about what the code should do and why. So merge
revives both sides' latest sessions read-only, has them compare their work against the code, and
produces a resumable merged session plus the list of genuine conflicts.

You usually arrive here from `agit a pull`. When a teammate's sessions fast-forward yours it just
applies them; when the two histories have diverged it stops and sends you to `agit a merge`.

```
agit a merge frontend
```

The target is another memory — an agent name, or a git ref in the store — not a code branch. A bare
name resolves through the [binding](concepts.html); a ref lets you merge against an earlier point in
the same store.

## What happens

Each side's most recent session is revived read-only in its own worktree, carrying the diff it
introduced since the two histories' common ancestor. The two sessions exchange summaries and
reconcile what they can by reading the current code. What can't be reconciled — a decision one made
that the other's changes contradict — is surfaced as a conflict for you.

Because both sides are revived as real sessions, the runtime CLI (`claude` or `codex`) must be
installed. Which runtime revives them follows the usual rule: the one you name with `--from`, else
the only one present, else it asks (see [Runtimes](runtimes.html)).

The output is not a summary written to a file. It's a session you resume like any other:

```
claude --resume <id>
codex exec resume <id>
```

A successful merge captures that session into the store as the agent's latest, so a bare `agit start`
or `agit resume` (no id) picks it up like any freshly snapped session. You do not snap it by hand; if a
secret slips into the merged transcript the same gate holds it out of history and the merge exits
non-zero, exactly as a snap would.

## The conflict ledger

When merge surfaces conflicts, you decide each one: accept a way out the dialogue named, type your own
call, or leave it open for now. Those decisions are written to an auditable ledger beside the merge
transcript, at `<agent>/sessions/sync/<a>-<b>.decisions.md`, so what was settled (and what was
consciously deferred) lives in the store rather than only inside the resumed session. Every conflict is
on the record: an accepted option, a custom decision, and a deliberate defer alike. The settled
decisions are also folded into the merged session, so when you resume it the agent continues with them
already decided.

## Same agent vs. different agent

The target's aid decides how far the merge goes:

| Target | Reconciled | Git histories |
|---|---|---|
| **Same aid** — the same agent, e.g. a teammate's pushed copy | by dialogue | merged; the two become one memory again |
| **Different aid** — a distinct agent | by dialogue | left intact; each keeps its own history |

Merging by aid is why a teammate pulling your sessions and you pulling theirs converge on one
history, while merging two agents that happen to share a name does not fold them together. Identity
is the aid, not the label — see [How it works](concepts.html).

## Options

| Option | Effect |
|---|---|
| `--from <runtime>` | Pick which runtime revives the sessions when both have sessions present. |
| `--both` | Write the merged session onto both branches instead of one. |
| `--quick` | Skip the context handoff — reconcile from the diffs alone, without the summary exchange. Faster, less thorough. |
| `--splice` | The no-model merge. Combine both sides into one session for a fresh agent to read, instead of reconciling them. See below. |
| `--dry-run` (alias `--preview`) | Show what the merge would do without running it. See below. |

## It's model-backed

The conflict synthesis runs through a model, the backend set by `AGIT_LLM` (see
[Configuration](configuration.html)). If none is available, `agit a merge` lists the open conflicts
instead of resolving them; everything else still runs.

Deferring to a model is the trade for a real semantic merge with no schema, and it means the result
is non-deterministic — run it twice and the merged session may differ. That's fine, because the merge
is not the record. The raw sessions on both sides stay committed in the store, git-versioned, and
remain the source of truth; a merge you don't like is one you can drop and redo.

## Preview a merge: `--dry-run`

`--dry-run` (alias `--preview`) shows what a merge would do without doing it:

```
agit a merge frontend --dry-run
```

It runs only the inspection phases the real merge starts with (resolve the target, read each side's
sessions, pick the voice session per side) and prints the plan: the target, the mode (same agent, which
would reconcile then fuse the git histories, or a different agent, which would reconcile by dialogue
only), how many sessions each side has, and the voice session picked for each. It stops there. No model
runs, no session is revived, no transcript or ledger is written, and no worktree is left behind, so it
needs neither `AGIT_LLM` nor the runtime CLI. Rerun without `--dry-run` to carry it out.

## The stupid mode: `--splice`

`--splice` skips the model entirely. Instead of reconciling the two sides, it combines them: it takes
A's full transcript, appends B's tail from the point the two forked, normalizes the session id and cwd
onto one, and installs the result as a single new session.

```
agit a merge peer --splice
```

Nothing is reconciled and no conflicts are computed. You resume the combined session and the agent
reads both sides' context in one window, then decides for itself what to do with it — the reconciliation
happens live, in that session, rather than up front. Because it runs no model and revives nothing, it
needs neither `AGIT_LLM` nor the runtime CLI, and it is deterministic: the same two sessions always
splice to the same bytes.

Use it when you'd rather hand the whole picture to a fresh agent than have two revived ones negotiate,
or when no model is configured. Same-aid targets still fuse the git histories afterward, as with a
normal merge.
