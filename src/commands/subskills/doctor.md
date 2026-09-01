---
name: agit-doctor
description: Diagnose AgentGit local storage, runtime integrations, and optional backend connectivity.
---

# agit doctor

## Synopsis

```bash
agit doctor [--check-backend]
```

## Options

| Option | Meaning |
|---|---|
| `--check-backend` | Also check Hub/backend reachability and configuration |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit doctor
agit doctor --check-backend
```

The report diagnoses problems; it does not automatically create branches or rewrite history. Follow its suggested `setup`, `login`, or explicit `new`/`import` actions.
