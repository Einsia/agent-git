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
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit revert @#7
agit revert szh/p1@other#3.1 --into review -m "Drop the wrong assumption"
```

Revert changes only the VIEW and never deletes the evidence log. It is not a history rewrite or compression tool.
