---
title: Move a session between runtimes
nav_order: 5
---

# Move a session between Claude Code and Codex

agit works with two runtimes, Claude Code and Codex. A session recorded in one can be converted to the
other's format and resumed there. The daemon does this for you; you can also convert a session by hand.

## How agit picks a runtime

A command that reads a session uses the runtime you name with `--from`. If you do not name one and only
one runtime has sessions here, agit uses that one. If both do, it asks which. List the runtimes agit
recognizes:

```
agit adapter
```

Sessions are stored per runtime, so a Claude Code session and a Codex session sit side by side in the
agent's store. `snap`, `merge`, and the other session commands take `--from claude-code` or `--from
codex` to pick one.

## Convert a session by hand

```
agit convert <session> --to codex --write
agit convert <session> --to claude-code --write
```

Without `--write`, the command reports what the conversion would produce. With `--write`, it installs
the result as a session the target runtime can resume. The installed session gets a fresh id, which the
target runtime requires to resume it.

- A same-runtime conversion is a byte-for-byte copy.
- A cross-runtime conversion carries the prompts, replies, and tool activity across. It drops what the
  target has no equivalent for, such as encrypted reasoning and runtime-specific tool encodings.

The argument can be a session id or path, or an agent name (which converts that agent's latest session).
With no argument, `agit convert --to <runtime>` converts the active agent's latest session.

## Convert automatically

The daemon converts sessions between runtimes as it records them, so you rarely run `convert` yourself:

```
agit watch --daemon
```

After this, a session recorded in one runtime is always available to resume in the other. See
[Capture agent sessions](capture.html) for the daemon, and [Reconcile diverged sessions](merging.html)
for how `--from` selects the runtime that revives a session during a merge.
