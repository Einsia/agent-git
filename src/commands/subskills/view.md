---
name: agit-view
description: Print a materialized session/ref VIEW for a merge agent or tool.
---

# agit view

## Synopsis

```bash
agit view [TARGET] [--json]
```

## Options

| Option | Meaning |
|---|---|
| `[TARGET]` | Branch, tag, session, or `owner/repo@ref`; current context when omitted |
| `--json` | Print structured VIEW for scripts, MCP, or merge agents |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options |

## Scenario and examples

First run `agit view <source> --json` to see what the other session actually sees, then use `agit show <source>#n.k` to inspect one event. JSON mode uses the common CLI envelope; when `result.format` is `json`, the VIEW array is at `result.value`. VIEW is consumable context, not a complete replacement for the evidence log.

```bash
agit view @
agit view alice/notes@handoff --json > /tmp/view.json
```
