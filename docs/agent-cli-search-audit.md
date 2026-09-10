# Agent CLI and search experience audit

The local changes make search usable from one agent call: repeat `--query`, share
filters across queries, preserve structured evidence and pagination, and retain
successful entries when part of a batch fails. Hub search permissions and the backend dependency revision remain unchanged.
Saved-version filters narrow the candidates before the existing index and ranking.

## Implemented interface

```bash
agit status --json
agit search --query "cache failure" --query "build timeout" \
  --repo alice/service --runtime codex --in tool --json
```

`--repo`, `--owner`, `--runtime`, `--in`, `--tool`, and `--path` compose the existing
shared query grammar. Filters are applied by the Hub, not by removing rows from
an already counted page. `--owner` means the repo's namespace owner; it does not
mean the conversation author. The existing `repo:` alias has the same meaning
as `agent:`. A fully qualified repo selector matches an exact coordinate.

The batch transport is a bounded client fan-out over existing search endpoints,
not a new Hub batch endpoint. It reuses the HTTP connection pool, runs at most
four requests concurrently, accepts at most 16 queries, and preserves input
order. Each query retains its own Hub admission and scan budget. This saves
process and agent-tool overhead without allowing a batch to bypass backend
resource gates. Limits are checked before login or search requests.

Search stdout is raw JSON when redirected. Explicit `--json` uses the common
CLI envelope, with the search object under `result.value`. The result preserves
Hub hit fields, including fields introduced by a newer Hub, and includes
`page`, `per`, `has_more`, `incomplete`, and `unknown`. A partial batch fails the
process and sets MCP `isError`, while keeping successful query results.
Noninteractive commands skip incidental startup update requests.

## Agent output coverage

| Surface | Current contract | Remaining limitation |
|---|---|---|
| Global `--json` | One `cli-output` envelope with status, result, and diagnostics | Some commands still expose structured `result.lines`, rather than command-specific object fields |
| `status`, `config`, `log`, `branch` with `--json` | Typed values, complete identities, configuration provenance, and history/ref facts | Inspect the command reference for pagination and view-specific fields |
| `search --json` | Search fields in `result.value` | Search still needs Hub credentials, including for public-only queries |
| Piped search / `search --mcp` | Raw structured search object | Use global `--json` to also standardize local parse/login failures |
| MCP search | Batch and shared filters; explicit tool failure bit | Requests within one batch use existing per-query Hub endpoints |
| Other MCP tools | Common CLI envelope; VIEW value retains its established shape | Rich field schemas are not available for every command |
| Runtime-launching commands with `--json` | Require preparation-only options, such as `--no-launch` | A running terminal lifecycle cannot finish as one JSON document |
| Windows global `--json` | Native CRT/Win32 capture preserves arguments, stdin, exit codes, and JSON v2 recovery actions | Native Windows integration coverage runs in the Windows pipeline |

The injected Skill introduces the machine-output contract and explicit session
targets before its command catalog. Command-specific details remain in the
progressive-disclosure references. Existing sessions require an explicit target or `AGIT_SESSION`; runtime discovery
and workspace bindings never substitute for one. Terminal selections apply only
to the current operation.

## Author and time filters

```bash
agit search --query "cache failure" --query "build timeout" \
  --repo alice/service --author alice@example.org \
  --since 2026-09-01 --before 2026-10-01 --json
```

`--author` matches the selected saved version's Git author name or email exactly,
case-insensitively. This is recorded Git metadata, not proof of a Hub identity or
repository ownership. `--since` includes versions whose Git committer time is at
or after the boundary; `--before` excludes the boundary itself. Dates mean
midnight UTC and RFC3339 offsets normalize to UTC. Transcript event timestamps
remain independent of these saved-version filters.

The HTTP session endpoint accepts `author`, `since`, and `before` parameters,
including a filter-only request with an empty or omitted `q`. Its
`applied_filters` object acknowledges the canonical predicates. CLI and MCP
reject missing or different acknowledgement from older Hubs. Non-session
categories and the multi-category counts endpoint reject these session-specific
filters; the session response's `total` provides its filtered count.

Authorized selected commit coordinates are filtered before transcript blob
selection, indexing, grouping, ranking, counting, and pagination. Metadata is
read through a single bounded Git batch per repository cache miss, using the
existing process semaphore and read leases. Immutable successful projections are
cached by commit OID with a bounded process-wide cache. Failed reads never become
cached absences, and dropped candidates mark the response incomplete. The byte
budget is shared across repositories in each request. Normalized predicates are
part of result-cache, single-flight and qualifier-cursor keys.
FIFO eviction removes individual old entries instead of clearing the entire
projection when the cache fills.

No new database migration or CLI-parser dependency pin is needed: the filter wire
contract is additive to the typed HTTP session endpoint. The backend's existing
immutable shared dependency stays unchanged, so the local pairing can be reviewed
and tested without publishing a provider commit. Raw `author:` / `since:` /
`before:` query qualifiers are not introduced.

## Validation

The focused tests exercise request limits, literal MCP arguments, concurrent
request bounds, ordered partial failure, preserved evidence fields, global JSON
capture, redirected stdout, and a real MCP subprocess against a loopback Hub.
The integration fixture accepts only search requests, so an incidental startup
version request fails the test. Existing JSON-envelope and Skill adoption tests
cover the surrounding protocol and current-session guidance.

Saved-version tests cover date normalization and invalid boundaries, exact Git
author versus namespace/committer identity, filters before blob deduplication,
cold and indexed result equivalence, pagination totals, private repository
exclusion, HTTP validation, batch propagation and old-Hub acknowledgement errors.
The opt-in backend wire test invokes the integrated CLI against the real
authenticated HTTP router, including fractional UTC boundaries and shared batch
filters. See the backend's `docs/search-filter-validation.md` for its command.
