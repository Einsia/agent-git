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

## It's model-backed

The conflict synthesis runs through a model, the backend set by `AGIT_LLM` (see
[Configuration](configuration.html)). If none is available, `agit a merge` lists the open conflicts
instead of resolving them; everything else still runs.

Deferring to a model is the trade for a real semantic merge with no schema, and it means the result
is non-deterministic — run it twice and the merged session may differ. That's fine, because the merge
is not the record. The raw sessions on both sides stay committed in the store, git-versioned, and
remain the source of truth; a merge you don't like is one you can drop and redo.
