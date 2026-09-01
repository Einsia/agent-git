---
name: agit-search
description: Search the readable AgentGit corpus for prior work on a question.
---

# agit search

## Synopsis

```bash
agit search <query> [options]
```

## Options

| Option | Meaning |
|---|---|
| `<query>` | Text to search |
| `-t, --type <sessions\|agents\|prs\|people>` | Result type; default `sessions` |
| `-n, --limit <count>` | Maximum hits; default 10 |
| `--page <n>` | One-based result page; default 1 |
| `--sort <best\|recent\|turns>` | Result ordering |
| `--counts` | Show hit counts for every result type, then exit |
| `--mcp` | Print JSON for an MCP tool |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit search "rate limit" --limit 20
agit search "deployment failure" --mcp
```

Results follow the current identity and visibility rules; a private repo that you cannot read will not appear.
