---
sidebar_position: 11
title: Relocating sessions
---

# Relocating sessions

A session is captured for the repository it ran in. If a runtime recorded a session under the wrong
working directory (a parent or monorepo directory, or the same project checked out elsewhere), that
session is stranded: it belongs to this repo but was written under another path. `agit relocate` detects
stranded sessions and moves them into the current repo so capture stops stranding them.

## Wrong-cwd warnings

When agit notices a session that ran in a directory other than this repo's root, it warns rather than
capturing it silently under the wrong slug. The warning names the runtime, the directory the session was
recorded in, and points at `agit relocate` to bring it in. This is why a session you expected to see in
`agit a log` can be missing: it was recorded elsewhere and is waiting to be relocated.

## Relocate

```bash
agit relocate
```

The bare form lists every session that ran in another directory but belongs in this repo, then asks
before moving them:

```
Sessions that ran elsewhere but belong in /home/you/code/web:
  claude-code · /home/you/code · 2 hours ago
      "add a rate limiter"
bring these 1 session(s) into /home/you/code/web? [Y/n]
```

| Flag | Effect |
|---|---|
| `<session>` | Relocate one session, matched by id, transcript path, or a substring of its recorded directory. |
| `--to <path>` | Override the destination. It must be a git work tree; the session installs under a slug derived from it. |
| `--yes` (`-y`) | Skip the confirmation. |

The destination defaults to this repo's root. Relocate moves history into the store, so it never proceeds
unattended: a non-interactive shell without `--yes` is treated as "no". Running it in the right place with
nothing stranded is a valid, complete outcome, not an error.

## Related

`agit resume --relocate` handles the adjacent case: continuing one recorded session against the current
checkout when it is the same project moved, rewriting the session's recorded paths onto it. See
[Resuming sessions](./resuming.md).
