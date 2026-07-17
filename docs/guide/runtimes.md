---
title: Runtimes
nav_order: 5
---

# Runtimes

agit supports Claude Code and Codex. A command that reads sessions uses the runtime you name with
`--from`; if only one has sessions it uses that, and if both do it asks which.

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
