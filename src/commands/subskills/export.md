---
name: agit-export
description: Export an AgentGit session or ref as JSONL, IR, Markdown, or a runtime-compatible format.
---

# agit export

## Synopsis

```bash
agit export <target> [options]
```

## Options

| Option | Meaning |
|---|---|
| `<target>` | Session, branch, tag, commit, or repo ref |
| `--format <jsonl\|ir\|markdown\|claude-code\|codex>` | Output format; default `jsonl` |
| `--view-only` | Export only the final VIEW, not the complete evidence log |
| `--redact` | Redact sensitive values before export |
| `-o, --out <path>` | Output file; stdout when omitted |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` wraps the export in the unified CLI JSON envelope |

## Examples

```bash
agit export @ --format markdown -o /tmp/session.md
agit export szh/p1@handoff --format codex --view-only
agit export 132bf69f-22a --format jsonl --redact -o /tmp/redacted.jsonl
```

Use `--redact` before sending an export outside the system. Export does not modify the Agent repo.
