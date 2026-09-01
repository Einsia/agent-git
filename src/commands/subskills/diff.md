---
name: agit-diff
description: Compare current state, turn ranges, or VIEW differences.
---

# agit diff

## Synopsis

```bash
agit diff [RANGE] [options]
```

## Options

| Option | Meaning |
|---|---|
| `[RANGE]` | Agent ref/turn range; omitted compares the current AgentGit working state |
| `--turns` | Compare turn/event changes; default mode |
| `--view` | Compare the VIEW sequence of two refs |
| `--files` | Compare shared-file text; it is not a file-name-only listing |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit diff
agit diff @#3..@#8 --turns
agit diff szh/p1@feature-a..szh/p1@main --view
agit diff szh/p1@feature-a..szh/p1@main --files
```

Zero-argument diff describes AgentGit state, not `git diff` in the project code repository.
