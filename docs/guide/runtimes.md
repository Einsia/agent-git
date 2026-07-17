---
title: Runtimes
nav_order: 5
---

# Runtimes

agit supports two runtimes: Claude Code and Codex. A command that reads sessions uses the runtime you
name with `--from`; if you don't name one and only one has sessions, it uses that one; if both do, it
asks which.

```
agit adapter        # list the runtimes agit recognizes
```

Sessions are stored per runtime, so a session recorded under Claude Code and a session recorded under
Codex live side by side in the agent's store. `snap`, `merge`, and the other session commands take
`--from claude-code` or `--from codex` to pick one.

## Conversion

`agit convert` rewrites a session from one runtime's format into the other's, so a session recorded by
one runtime can be resumed by the other:

```
agit convert <source-session> --to codex --write
agit convert <source-session> --to claude-code --write
```

Without `--write` the command reports what the conversion would produce; `--write` installs the result
as a session the target runtime can resume.

- A **same-runtime** conversion is a byte-for-byte copy.
- A **cross-runtime** conversion is content-level: it carries the prompts, replies, and tool activity
  across and drops what the target has no equivalent for (encrypted reasoning, runtime-specific tool
  encodings).

The installed session is assigned a fresh id, which the target runtime requires to resume it.

You rarely run `convert` by hand. The daemon does it for you:

```
agit watch --daemon
```

`agit watch --daemon` auto-converts alongside capture, so a session recorded in one runtime becomes
resumable in the other with no explicit `convert`. Run it once and both formats stay in sync as you
work. See [Quickstart](quickstart.html) for capture and [Merging](merging.html) for how `--from`
selects the runtime that revives a session.
