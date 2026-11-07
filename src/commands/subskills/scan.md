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
| `[REFS]...` | Explicit refs, or the selected `AGIT_SESSION`; a secret scan of named branches checks what `agit push` of them would send, sensitive review uses the selected committed VIEW |
| `--secrets` | Scan credentials, tokens, keys, and other secrets |
| `--sensitive` | Ask a supported installed runtime to review selected VIEW events for disclosure risks |
| `--low-confidence` | With `--secrets`, also list entropy-only values in what push publishes, grouped by shape (not reported otherwise) |
| `--all` | With `--secrets`, scan the whole repository even when a branch is named |
| `--json` | Print structured results |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options |

## Examples

```bash
agit scan @ --secrets
agit scan @ --sensitive
agit scan szh/p1@feature-a --json
```

`--secrets` and `--sensitive` are mutually exclusive. `--json` uses the common CLI envelope (version 2 by default; `--json-version 1` selects version 1). The report is at `result.value` when `result.format` is `json`. Secret scanning is deterministic. With branches named (`<owner/repo>@<branch>`), it checks what `agit push` of them would send: their commits the Hub doesn't hold yet and the tags they reach, plus `main`'s when the push carries main (a session branch's push does when main fast-forwards the Hub's), and the working tree only when one of them is checked out in it. When the Hub can't be asked, nothing it holds is subtracted. With no ref, `<owner/repo>` alone, or `--all`, it checks the whole repository: every branch's commits the Hub doesn't hold and the working tree. The report's first line says which it scanned. Explicit scan results are advisory: `agit push` does not run this scan. A push replaces what the privacy rules recognize in the session records and commit messages it adds, and refuses to send a text file it adds that holds one; binary files, tag messages and low-confidence values go out as they are, and what the Hub already holds stays.

The secret report lists high-confidence findings: provider signatures, private keys, credential assignments and registered values. Each carries its location, a redacted preview and the privacy record id that `agit secrets allow <record-id> --repo <path>` accepts; an allowed value is no longer reported. The report states the total count, and says when only the first findings are listed or when the count is a lower bound. Entropy-only values (runtime ids, hashes, encrypted carriers) are not reported by default. `--low-confidence` lists those in what push publishes, within the same scope as the scan, grouped by shape (uuid, hex-40, hex-64, base64 run, other): each session's records as publication keeps them, the files as each branch holds them now (not their earlier versions; `files_scope` is `branch_tips`), staged files and changed working-tree files (a link is not followed), and commit and tag messages. A record publication leaves out is not read, and neither are the ids a published record is chained by. In JSON the report is `{check, advisory, scope, branches, hub_asked, working_tree, count, count_exact, findings[]}`, where `scope` is `branches` (with `branches` listing what the push would send) or `repository` (with `branches` null), plus `low_confidence: {count, count_exact, files_scope, groups: [{shape, count, findings[]}]}` only with `--low-confidence`; an absent `low_confidence` means not counted. Every finding has `rule`, `at`, `line`, `preview`, `source`, `confidence` and `record_id` (`null` when the local privacy store is unavailable, and for a low-confidence value).

A finding inside Agent history (blob, commit or tag objects) is never rewritten: `agit revert <owner/repo>@<branch>#n.k` removes an event from the VIEW while LOG and Git history keep it, so revoke and rotate a real credential. Missing or unreadable content is an incomplete scan, not a credential finding that can be classified away.

Sensitive review selects committed VIEW events, including `#n`, `#n.k`, or an ascending turn range. Historical selectors use the VIEW at that historical point. Unsettled runtime turns and working files are not included. Each finding has an immutable snapshot SHA, a validated `@#n.k` location, and locally generated command arguments. A branch-head removal remedy includes `--expected-head`, so a changed branch requires a fresh review. Historical points, sealed branches, and branch names that cannot be expressed unambiguously get inspection remedies only. Revert removes context from VIEW; the original evidence remains in LOG.
On Windows, paste printed remedies into PowerShell. For arguments containing ASCII double quotes, a local PowerShell scope preserves the native argument bytes without changing your session identity or the caller's argument-passing preference. The structured `argv` recipe retains the same explicit target.

The runtime comes from `runtime.default` (default `claude-code`). Supported Claude Code installations must expose safe mode, disabled model tools, strict MCP configuration, and session persistence controls. AgentGit uses an owned temporary working directory, removes parent session identity, isolates `AGIT_HOME`, and sends only the selected transcript through stdin. It never executes model-supplied commands or applies findings automatically. The installed native executable is trusted: its authentication housekeeping and administrator-managed policies remain active, and may perform their own I/O. This is not an OS sandbox or a guarantee that the native executable performs no writes. The native runtime's configured provider may be remote; "local runtime" does not mean offline inference.

Codex and other runtimes currently return unavailable because their model/tool configuration and authentication cannot yet be isolated through this review adapter. They are never silently replaced with another runtime. To choose a supported backend explicitly, use `agit config runtime.default claude-code`.

Review limits are 16 refs, 256 commits per history, 4,096 selected events, 1 MiB of selected envelope data, and 16 MiB of unique historical blob data. A runtime review has a two-minute deadline and bounded output. Unsupported runtimes, unavailable authentication, malformed or oversized input/output, missing event coverage, and unlocatable VIEW events return exit 4 with `complete: false`. A missing ref returns exit 3. A completed report with findings returns exit 7; a complete report with no findings returns exit 0. Model classifications are advisory and do not replace human review or the independent secret scan.
