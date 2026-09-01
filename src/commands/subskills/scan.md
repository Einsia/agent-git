---
name: agit-scan
description: Scan Agent history for secrets or sensitive content before publishing or sharing.
---

# agit scan

## Synopsis

```bash
agit scan [REFS]... [options]
```

## Options

| Option | Meaning |
|---|---|
| `[REFS]...` | Refs to resolve and validate; scanning still covers the entire repo publish surface |
| `--secrets` | Scan credentials, tokens, keys, and other secrets |
| `--sensitive` | Scan broader sensitive content |
| `--json` | Print structured results |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options |

## Examples

```bash
agit scan @ --secrets
agit scan @ --sensitive
agit scan szh/p1@feature-a --json
```

`--secrets` and `--sensitive` are mutually exclusive. `--json` uses the common CLI envelope; the existing scan report is at `result.value` when `result.format` is `json`. `--sensitive` requires a local review agent and fails with a precondition error when no model is available. `push`, public sharing, and private-to-public visibility changes may trigger a scan; a pass does not replace human review.
