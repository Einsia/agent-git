---
title: Merging
nav_order: 4
---

# Merging

`agit a merge <agent>` reconciles two sessions that have diverged. Unlike a textual git merge, it
doesn't combine files line by line: it revives both sides as read-only sessions, has them compare their
work against the code, and produces a new resumable session plus a list of conflicts that need you.

```
agit a merge frontend
```

The target is another memory — an agent name or a git ref in the store — not a code branch.

## What happens

Each side's most recent session is resumed read-only in its own worktree, with the diff it introduced
since the common ancestor. The two exchange summaries and reconcile what they can by reading the code.
Genuine conflicts are surfaced for you to resolve. The output is a session you resume like any other:

```
claude --resume <id>
codex exec resume <id>
```

## Merge mode

The mode is decided by the target's aid (see [How it works](concepts.html)):

- **Same aid** — a copy of the same agent, e.g. a teammate's pushed sessions. Reconciled by dialogue,
  and the git histories are merged. The two become one memory again.
- **Different aid** — a different agent. Reconciled by dialogue only; both histories stay intact.

## Options

- `--from <runtime>` picks which runtime revives the sessions when both are present.
- `--both` writes the merged session onto both branches instead of one.

Both sides are revived as real sessions, so the runtime CLI (`claude` or `codex`) must be installed.
The synthesis of the conflict lists uses the configured LLM backend (see
[Configuration](configuration.html)); with none available, open conflicts are listed rather than
resolved. Merging defers to a model, so it isn't deterministic — the raw sessions remain the git-versioned source of truth.
