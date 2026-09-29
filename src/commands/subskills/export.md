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
| `--format <jsonl\|ir\|markdown\|privacy-envelope\|RUNTIME>` | Output format; default `jsonl` |
| `--view-only` | Export only the final VIEW, not the complete evidence log |
| `--redact` | Redact sensitive values before export |
| `--privacy` | Apply local and bound Hub rules, path aliases, omissions, replacements, and secret checks before rendering |
| `--viewing-public-key <BASE64>` | X25519 recipient key for `privacy-envelope`; otherwise select the repository key through this snapshot's accepted publication |
| `-o, --out <path>` | Output file; stdout when omitted |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` wraps the export in the unified CLI JSON envelope |

## Examples

```bash
agit export @ --format markdown -o /tmp/session.md
agit export szh/p1@handoff --format codex --view-only
agit export 132bf69f-22a --format jsonl --redact -o /tmp/redacted.jsonl
agit export szh/p1@handoff --privacy --format markdown -o /tmp/public-session.md
agit export szh/p1@handoff --format privacy-envelope -o /tmp/session.encrypted.json
```

`--privacy` uses the shared session publication processor. It checks the complete LOG/VIEW pair
before selecting the requested VIEW or turn range, and saves stable aliases in device-local
repository state. Unsupported native records, uncertain tool origins, and non-text blocks receive
omission markers in public output. The stderr report explains omissions and recovery scope.
Independent repository files and standalone attachments are deferred.

`privacy-envelope` automatically enables that processor and includes a recoverable encrypted
private layer. It accepts a complete branch, tag, or commit snapshot; combining it with `--view-only`
or a turn selector is refused so a narrower selection cannot silently export a larger private layer.
Without `--viewing-public-key`, it requires a verified accepted publication for the selected snapshot
and fetches that publication's repository key. Unpublished snapshots refuse before output. The CLI
does not need a viewing password. Keep the accepted repository/commit locator with the artifact
when delivering it; the envelope does not embed a key descriptor. See `docs/privacy-envelope.md` for the wire contract.

Repositories with a remote identity, origin or upstream require fresh authenticated source rules,
including when `--viewing-public-key` is explicit. Default-key exports also apply account rules.
The CLI rechecks source rules, local policy, repository bindings and any fetched recipient key
before writing output; a changed binding leaves an existing output file untouched. A repository
without remote bindings can export with local/device rules offline, including encrypted export
with an explicit key. Export does not send session content to these rule endpoints.

Ordinary exports retain their existing conversion behavior. `--redact` adds persona anonymization;
it does not enable repository scope policy. Privacy exports always run content checks, even when
`--redact` is omitted. Export does not change committed session history.
