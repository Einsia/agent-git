---
sidebar_position: 4
title: Runtimes
---

# Runtimes

agit works with two runtimes: Claude Code and Codex. Each writes its live sessions to its own directory
in its own format; agit reads those dumps, mirrors them into the store, and writes them back so either
CLI can resume a session the other produced. List the runtimes agit recognizes:

```bash
agit adapter
```

## How agit picks a runtime

A command that reads a session uses the runtime you name with `--from`. If you do not name one and only
one runtime has sessions here, agit uses that one. If both do, it asks. Sessions are stored per runtime,
so a Claude Code session and a Codex session sit side by side in the agent's store, and `snap`, `merge`,
`convert`, and the other session commands take `--from claude-code` or `--from codex` to pick one.

## Claude Code

Claude Code writes each session as a single JSONL transcript under a per-project directory:

```
~/.claude/projects/<project-slug>/<uuid>.jsonl
```

The project slug is derived from the working directory, so sessions are split by project. agit reads that
transcript, and when it installs a session for Claude Code to resume it writes back to the same layout
under the current checkout's slug. Claude Code resolves a session by scanning that directory and matching
the id, so there is no index to maintain:

```bash
claude --resume <uuid>
```

Claude Code requires a UUID id and has no settable title field, so agit installs Claude Code sessions
under a fresh UUID (not the readable `<branch>-<hex>` name it can give Codex).

## Codex

Codex writes sessions as date-partitioned rollout files:

```
~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<uuid>.jsonl
```

agit reads these and, on install, writes a rollout file under a date directory; the exact date does not
matter because Codex resume scans `sessions/` recursively and resolves by id. Codex owns a session by the
`cwd` recorded in its `session_meta`, so a fork/resume rollout that embeds another project's parent
session is filtered out and never mistaken for this project's.

Resume a Codex session interactively:

```bash
codex resume <session-id>
```

`codex resume [SESSION_ID] [PROMPT]` (prompt optional) opens the TUI carrying the session, which is what
`agit start` and `agit resume` want. The non-interactive `codex exec resume <id>` requires a prompt, so a
promptless launch fails there; agit uses `codex resume`.

### model_provider on resume

`codex resume` reads the model provider from the session's `session_meta` to bootstrap the client. When
agit converts a session into Codex format, it writes `model_provider: "openai"` (Codex's default) so
Codex then picks that provider's default model. A same-vendor Codex-to-Codex replay preserves the source
provider rather than forcing openai. Override the provider at launch for a non-openai backend:

```bash
codex resume <session-id> -c model_provider=<x>
```

## Converting between runtimes

A session recorded in one runtime resumes in the other after conversion. The daemon does this
automatically; you can also convert by hand. See [Resuming sessions](./resuming.md) for `agit convert`,
`agit convert --watch`, and what a cross-runtime conversion carries and drops. For how `--from` selects
the runtime that revives a session during a reconcile, see [Merging sessions](./merging.md).
