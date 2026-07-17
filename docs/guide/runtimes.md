---
title: Runtimes
nav_order: 5
---

# Runtimes

agit supports two agent runtimes, Claude Code and Codex, and treats them as peers. There is no default
runtime. A command that reads sessions uses the runtime named with `--from`, or the only runtime
present, or, if both are present and no choice is given, it asks. Where runtimes are listed, they are
listed alphabetically.

```
agit adapter        # list the runtimes agit recognizes
```

Sessions are stored per runtime under `sessions/<env>/<runtime>/`. `snap`, `merge`, and the other
session commands take `--from claude-code` or `--from codex` to select one.

## Conversion

`agit convert` rewrites a session from one runtime's format into the other's, so a session recorded by
one runtime can be resumed by the other:

```
agit convert <source-session> --to codex
agit convert <source-session> --to claude-code
```

A same-runtime conversion is a byte-level copy. A cross-runtime conversion is content-level: it carries
the prompts, replies, and tool activity across, and drops what has no equivalent (encrypted reasoning,
runtime-specific tool encodings). The installed session is always assigned a fresh UUID identifier,
which is required for the target runtime to resume it.

`agit watch` performs conversion automatically alongside capture, so a session recorded in one runtime
becomes resumable in the other without an explicit `convert`.
