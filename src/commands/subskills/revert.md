---
name: agit-revert
description: Drop selected events from the current VIEW while preserving the immutable evidence log.
---

# agit revert

## Synopsis

```bash
agit revert [REFS]... [options]
```

## Options

| Option | Meaning |
|---|---|
| `[REFS]...` | `<ref|@>#n[.k]`; repeatable |
| `--into <branch>` | Target branch; context branch by default |
| `-m, --message <message>` | Operation message |
| `--expected-head <SHA>` | Refuse if the target no longer has this full commit SHA; the final update also checks that head atomically |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit revert @#7
agit revert szh/p1@other#3.1 --into review -m "Drop the wrong assumption"
```

Revert changes only the VIEW and never deletes the evidence log. It is not a history rewrite or compression tool.
Sensitive-review remedies include `--expected-head` so a changed branch requires a fresh review before its event coordinates can be applied.
Guarded edits require inherited Git routing overrides, such as `GIT_DIR`, `GIT_WORK_TREE`, or environment-injected Git configuration, to be unset so inspection and publication address the same repository.
