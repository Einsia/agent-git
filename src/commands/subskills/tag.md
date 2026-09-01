---
name: agit-tag
description: Create, move, or delete readable version tags for Agent repo refs.
---

# agit tag

## Synopsis

```bash
agit tag [NAME] [REF]
agit tag --delete <NAME>
```

## Options

| Option | Meaning |
|---|---|
| `[NAME]` | Tag name; omitted lists tags according to CLI behavior |
| `[REF]` | Branch/commit/turn ref for the tag; current context when omitted |
| `-m, --message <message>` | Tag message |
| `-f, --force` | Move an existing tag |
| `-d, --delete` | Delete a tag |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit tag v1 @
agit tag -m "Deliverable" release-1 szh/p1@feature-a
agit tag --delete scratch
```

Tags are read-only history anchors. To continue development, fork the tag with `agit fork <tag> -b <new-branch>`.
