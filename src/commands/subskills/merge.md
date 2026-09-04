---
name: agit-merge
description: Reconcile the intent of two session branches through the merge-agent protocol.
---

# agit merge

## Purpose

Merge is not text concatenation. It records a reconciliation around source and target VIEWs, selected turns/events, and shared-file intent. Nothing lands without a `summary`.

## Synopsis

```bash
agit merge <source> [options]
agit merge pick <source>#3..#5 <source>#8.2
agit merge drop <pick>
agit merge summary -m "conclusion"
agit merge --continue | --abort
```

## Options

| Option | Meaning |
|---|---|
| `[SOURCE]` | Source branch, tag, or `owner/repo@ref`; `@` means the session hosting this process and never falls back to the workspace pin or this directory |
| `--into <branch>` | Landing branch; current `@` by default |
| `--as <runtime>` | Runtime used to start the merge agent |
| `-m, --message <instruction>` | Extra constraints for the merge agent |
| `--manual` | Do not start a model; print fork point, new turns, and plumbing commands |
| `--dry-run` | Reconnaissance only; no lock, launch, or history change |
| `--status` | Show the open merge transaction |
| `--continue` | Validate the summary and commit the merge |
| `--abort` | Abandon the transaction; target ref does not move |
| `pick <refs>` | Select source turns/events |
| `drop <refs>` | Remove picks |
| `summary -m <text>` / `summary -F, --file <path>` | Write the required reconciliation conclusion from inline text or a file |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Standard flow

```bash
agit view szh/p1@other --json
agit show szh/p1@other#3.2
agit merge szh/p1@other --into main
agit merge pick szh/p1@other#3..#5
agit merge summary -m "Keep the new rate-limit policy and use uid on the target line"
agit merge --continue
```

If the intents cannot be reconciled, use `agit merge --abort`; do not rebase or force-push.

In a non-interactive environment (CI, pipes, or an agent harness), a normal merge refuses before
settling the target, materializing a merge session, or taking the branch lock. Use `--manual` to
open the transaction and follow the printed `pick`/`summary`/`--continue` protocol, or run the
command from a terminal.
