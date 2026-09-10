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
| `[REFS]...` | Explicit refs, or the selected `AGIT_SESSION`; secrets scan the repo publish surface, sensitive review uses the selected committed VIEW |
| `--secrets` | Scan credentials, tokens, keys, and other secrets |
| `--sensitive` | Ask a supported installed runtime to review selected VIEW events for disclosure risks |
| `--json` | Print structured results |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options |

## Examples

```bash
agit scan @ --secrets
agit scan @ --sensitive
agit scan szh/p1@feature-a --json
```

`--secrets` and `--sensitive` are mutually exclusive. `--json` uses the common CLI envelope (version 2 by default; `--json-version 1` selects version 1). The report is at `result.value` when `result.format` is `json`. Secret scanning remains deterministic and repo-wide. Publishing gates use the secret scan, not a model's classifications.

Sensitive review selects committed VIEW events, including `#n`, `#n.k`, or an ascending turn range. Historical selectors use the VIEW at that historical point. Unsettled runtime turns and working files are not included. Each finding has an immutable snapshot SHA, a validated `@#n.k` location, and locally generated command arguments. A branch-head removal remedy includes `--expected-head`, so a changed branch requires a fresh review. Historical points, sealed branches, and branch names that cannot be expressed unambiguously get inspection remedies only. Revert removes context from VIEW; the original evidence remains in LOG.
On Windows, paste printed remedies into PowerShell. For arguments containing ASCII double quotes, a local PowerShell scope preserves the native argument bytes without changing your session identity or the caller's argument-passing preference. The structured `argv` recipe retains the same explicit target.

The runtime comes from `runtime.default` (default `claude-code`). Supported Claude Code installations must expose safe mode, disabled model tools, strict MCP configuration, and session persistence controls. AgentGit uses an owned temporary working directory, removes parent session identity, isolates `AGIT_HOME`, and sends only the selected transcript through stdin. It never executes model-supplied commands or applies findings automatically. The installed native executable is trusted: its authentication housekeeping and administrator-managed policies remain active, and may perform their own I/O. This is not an OS sandbox or a guarantee that the native executable performs no writes. The native runtime's configured provider may be remote; "local runtime" does not mean offline inference.

Codex and other runtimes currently return unavailable because their model/tool configuration and authentication cannot yet be isolated through this review adapter. They are never silently replaced with another runtime. To choose a supported backend explicitly, use `agit config runtime.default claude-code`.

Review limits are 16 refs, 256 commits per history, 4,096 selected events, 1 MiB of selected envelope data, and 16 MiB of unique historical blob data. A runtime review has a two-minute deadline and bounded output. Unsupported runtimes, unavailable authentication, malformed or oversized input/output, missing event coverage, and unlocatable VIEW events return exit 4 with `complete: false`. A missing ref returns exit 3. A completed report with findings returns exit 7; a complete report with no findings returns exit 0. Model classifications are advisory and do not replace human review or the independent secret scan.
