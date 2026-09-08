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
| `[SOURCE]` | Source branch, tag, or `owner/repo@ref`; `@` requires the session supplied through `AGIT_SESSION` |
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

The preview finds the common Git ancestor across the selected local repositories and counts
settled turns after it. File and branch-identity commits do not add turns. Unrelated histories
show an unavailable fork point and unknown added-turn counts; equal content does not prove
shared ancestry. Shallow, unreadable, or ambiguous ancestry refuses before a transaction is
opened. Cross-repository reconnaissance borrows the local object stores. Git may fetch missing
objects from a configured promisor remote while reading a repository.

Merge preflight settles complete local content and then verifies every known active target claim. An unfinished turn, malformed record, missing transcript, or rewritten baseline stops the merge before opening its transaction. A quiet hook exit does not prove settlement. With no local claim, manual merging requires no login.

If a runtime writes while its replacement is being installed, the original claim stays active and the merge transaction remains available for manual recovery. The prepared replacement remains unclaimed and can be found with `agit status --check-missing`; merge does not discard the original transcript or launch that replacement.

Cancellation and merge-agent launch share a transaction control guard. If cancellation completes before final publication, the prepared session stays unclaimed and no agent is spawned. Once spawning wins admission, the guard is released immediately; a later abort clears the transaction without waiting for or terminating that runtime. Transaction progress and landing hold the same guard from their state read through their update, so an aborted transaction cannot be revived by a delayed command.
