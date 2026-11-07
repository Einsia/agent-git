---
name: agit-distill
description: Carry memory files from the current session branch into its own repository's main (alias of `agit memory distill`).
---

# agit distill

## Purpose

`agit distill` is `agit memory distill` spelled short: it carries the memory files that differ from `main` from this session branch into `main`, as one file commit, after scanning each for secrets and confirming each one. See the `memory` manual for the whole flow.

It reaches only the session's own repository's `main`: the repository of `--into`, or of this conversation. Each question and the result name it ("carry memory/decisions.md into alice/app's main?"). It doesn't carry memory into another repository, such as the one a team keeps its memory in. To put a memory file there, add it to that repository's main yourself, then push it:

```bash
agit file --into <team>/<repo>@main add <path-to-the-file> --to memory/<name>
agit file --into <team>/<repo>@main commit -m "Add <name>"
agit push <team>/<repo>@main
```

## Synopsis

```
agit distill [<file>…] [-y] [--into <owner/repo@branch>]
```

## Examples

```bash
agit distill                    # every file that differs from main, one confirmation each
agit distill refund-path.md -y
```
