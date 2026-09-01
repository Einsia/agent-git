---
name: agit-distill
description: Carry memory files from the current session branch into main (alias of `agit memory distill`).
---

# agit distill

## Purpose

`agit distill` is `agit memory distill` spelled short: it carries the memory files that differ from `main` from this session branch into `main`, as one file commit, after scanning each for secrets and confirming each one. See the `memory` manual for the whole flow.

## Synopsis

```
agit distill [<file>…] [-y] [--into <owner/repo@branch>]
```

## Examples

```bash
agit distill                    # every file that differs from main, one confirmation each
agit distill refund-path.md -y
```
