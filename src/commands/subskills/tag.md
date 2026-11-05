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
| `[REF]` | Branch/commit/turn ref for the tag; the branch supplied through `AGIT_SESSION` when omitted |
| `-m, --message <message>` | Tag message; refused on a session turn, whose tag is published without one |
| `-f, --force` | Move an existing tag |
| `-d, --delete` | Delete a tag |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit tag v1 @
agit tag release-1 szh/p1@feature-a
agit tag --delete scratch
```

A tag on a session turn is published by `agit push` on the commit the turn was published as; push names any tag it leaves out and why. The local tag and the Hub's then name different commits by design: the same turn, here as you saved it and on the Hub as it was published. Fetches leave the local tag as it is. Its name is held to the same check as a branch name, so a name holding something the privacy check would replace is refused.

Tags are read-only history anchors. To continue development, fork the tag with `agit fork <tag> -b <new-branch>`.
