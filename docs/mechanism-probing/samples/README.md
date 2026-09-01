# agit format-probing sample index

The files indexed here preserve measured record shapes and counts. Examples that could correlate
private identities or infrastructure are redacted or paraphrased; surrounding analysis is English.

Probed 2026-08-01. Two hosts: this machine (macOS, `nana`) and `ssh nana-data@remote-host` (Ubuntu 22.04).
Read-only throughout; no code or session data was written under `/Users/nana/Projects/AgentGit` or on
the remote host.

## Versions

| Runtime | This machine | remote host |
|---|---|---|
| Cursor | IDE 3.13.25 (`cursor --version`) | the remote has only `~/.cursor-server`, no client state database |
| OpenCode | 1.18.13 (`opencode --version`, Linux) | not probed |
| Claude Code | transcript `version` 2.1.197 / 2.1.219 | 2.1.219 / 2.1.220 |
| Codex | `session_meta.payload.cli_version` 0.146.0-alpha.3.1 | 0.144.5 |
| Kiro | not installed | `kiro-cli 2.14.1` |

## Files

### `cursor/`
* `record-histogram.txt` — shape histogram over 104 transcripts and 11162 content blocks
* `record-shapes.jsonl` — one representative record per shape (body truncated at 240 characters, suspected secrets already `<REDACTED>`)
* `synthetic-user-text.txt` — tag counts over 475 `role=user` text blocks, and the injected texts enumerated
* `paths-and-resolve.txt` — path templates, project-slug rules, non-path slugs, same-id divergence, reverse-lookup timing

### `kiro/`
* `session-2293dba9.jsonl` / `session-c8e2f183.jsonl` — the complete event log of two sessions (2 records each)
* `session-2293dba9.state.json` — the sidecar state file in full (including `rts_model_state.model_info`)
* `session-2293dba9.history` — the readline history file
* `layout-and-index.txt` — directory layout, `--list-sessions` / `--list-models` output, the `data.sqlite3` schema
* `logentry-variants-from-binary.txt` — the full set of `LogEntryV1` variants extracted from the `kiro-cli-chat` binary

### `opencode/`
Probed 2026-08-05, this machine (Linux). Every sqlite3 read runs against a copy of the database under
`/tmp/opencode/opencode-probe/`; the write-side experiments (`opencode import`) all run in an isolated
scratch HOME+XDG.
* `part-type-histogram.txt` — histograms of part type / tool state machine / step-finish reason / synthetic
* `schema.sql` — the CREATE TABLE statements for `session` / `message` / `part` / `project`
* `message-and-part-samples.json` — real samples of user/assistant message rows and tool/reasoning parts (long output truncated)
* `compaction-anatomy.txt` — anatomy of the three-stage compaction (boundary part, `mode=compaction` summary message, continuation injection)
* `export-import-behavior.txt` — record of the export/import behavior experiments (idempotency, directory rewriting, id reuse and re-minting)
* `mutation-evidence.txt` — line-by-line diff of two copies: decisive evidence that an in-progress row is rewritten in place
* `synthetic-user-text.txt` — prefix histogram and shape summary over 105 synthetic user texts
* `export-sanitized-head.json` — the head of `opencode export --sanitize` output

### `model-attribution/`
* `cursor-state-vscdb.txt` — whole-database histogram of bubble-level `modelInfo`, fill rate, mid-session model switches, thinking persisted, `modelConfig`
* `cursor-ai-code-tracking.txt` — `ai_code_hashes.model` in `~/.cursor/ai-tracking/ai-code-tracking.db`
* `claude-code.txt` — `message.model` × top-level `effort` × `version` on both hosts, including `attributionAgent`
* `codex.txt` — `turn_context.payload.model/effort`, `model`/`reasoning_effort` in the `threads` table, record-type differences

### `work/`
Intermediate products (deletable): `cursor-local/` is a static snapshot of the 104 transcripts (raw
content, unredacted, for local review only); `codex_state5.sqlite` / `ai-track.db` are read-only copies
of the index databases (the originals cannot be opened by the sqlite3 CLI in URI read-only mode — they
report `SQLITE_CANTOPEN(14)` — and open normally once copied).

## One known side effect

Running `kiro-cli chat --list-sessions` / `--list-models` on the remote host produced
`~/.kiro/logs/20260731T163218598/{kiro,mcp,powers}.log`. That is the CLI's own log directory, not
session data; nothing else was written on the remote host.
