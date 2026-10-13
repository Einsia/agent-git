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
| `--format <jsonl\|ir\|markdown\|RUNTIME>` | Output format; default `jsonl` |
| `--view-only` | Export only the final VIEW, not the complete evidence log |
| `--redact` | Redact sensitive values before export |
| `--privacy` | Apply local policy, path aliases, omissions, replacements, and secret checks before rendering |
| `-o, --out <path>` | Output file; stdout when omitted |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` wraps the export in the unified CLI JSON envelope |

## Examples

```bash
agit export @ --format markdown -o /tmp/session.md
agit export szh/p1@handoff --format codex --view-only
agit export 132bf69f-22a --format jsonl --redact -o /tmp/redacted.jsonl
agit export szh/p1@handoff --privacy --format markdown -o /tmp/public-session.md
```

`--privacy` uses the shared session publication processor. It checks the complete LOG/VIEW pair
before selecting the requested VIEW or turn range, and saves stable aliases in device-local
repository state. Unsupported native records, uncertain tool origins, and non-text blocks receive
omission markers in public output. The stderr report explains omissions and recovery scope.
Independent repository files and standalone attachments are deferred.

Export uses local policy and device rules without uploading session content. Bound remote names do
not select an encryption key for an export. Use `privacy unlock` for account-authorized original
recovery and `privacy access` to manage readers of a published encrypted snapshot.

Ordinary exports retain their existing conversion behavior. `--redact` adds persona anonymization;
it does not enable repository scope policy. Privacy exports always run content checks, even when
`--redact` is omitted. Export does not change committed session history.
