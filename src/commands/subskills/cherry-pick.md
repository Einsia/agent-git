---
name: agit-cherry-pick
description: Add selected turns or events from another branch to a target VIEW without starting a merge agent.
---

# agit cherry-pick

## Synopsis

```bash
agit cherry-pick [PICKS]... [options]
```

## Options

| Option | Meaning |
|---|---|
| `[PICKS]...` | `<ref>#n`, `<ref>#a..#b`, or `<ref>#n.k`; repeatable |
| `--into <branch>` | Target branch; defaults to the branch explicitly supplied through `AGIT_SESSION`. `@` in the source ref also requires `AGIT_SESSION` |
| `-m, --message <message>` | Message for the operation commit |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit cherry-pick szh/p1@experiment#4
agit cherry-pick other@fix#3..#6 --into review -m "Bring in the fix conclusion"
```

This selects AgentGit events, not code Git commits. Use `merge` when the two lines require full intent reconciliation.
