# Native history paging

The executor advertises `machine.describe.history` with `version: 2`,
`snapshot: true`, and the supported runtime names. Callers must negotiate this
capability before requesting consistent history. The peer/Cloud executor endpoint
provides pagination.

`session.history` accepts `session_id`, and authorized native lookups also supply
`runtime` and `cwd`. The initial request omits `before` and `snapshot`. Its response
contains `items`, `before`, `has_more`, an opaque `snapshot` token and
`status: "complete"`. Every subsequent page sends both the previous `before` and
the same token. `before` is an exclusive cursor, not a portable line number;
OpenCode uses projected-item positions. `has_more` must agree with `before > 0`.
An empty display page may still have earlier records. A token-only request reads
the tail of that same snapshot, allowing a collector to verify its final read.

File sources and Codex ancestor prefixes are copied into private, unlinked
scratch files with source metadata checked across capture. OpenCode uses the
existing bounded read-only SQLite snapshot and native event projection, retaining
stable part identities when rows are revised. Pagination never reopens a different
native version. New writes remain available through watch and a fresh initial
request; snapshot reads neither launch nor resume a harness. Copies have bounded
size, cache capacity and idle lifetime; the constants live in
`src/rc/local_history/snapshot.rs`. Eviction or daemon restart expires the token.

File-backed history has a separate logical disk budget from OpenCode's in-memory
projection: up to 8 GiB per snapshot and 32 GiB across retained file snapshots.
macOS and Linux attempt a filesystem copy-on-write clone of each open source;
unsupported filesystems and other platforms stream into private scratch files
with a fixed buffer. Neither method loads the whole transcript into RAM. Page
and RPC response limits still apply. Clones stay isolated from subsequent source
appends, truncation and replacement. A streaming capture can take longer and
retries if the source changes during copying.

If history shows a local privacy-protection failure, inspect the executor log.
On macOS an existing dictionary may need Keychain authorization before its key
can migrate to repository-local storage. Run `agit secrets review --repo
<Agent-repository-path>` in an interactive terminal using the same installed CLI
and approve the system dialog. This prints record summaries, never secret values.
Keep the existing vault and keystore; replacing either cannot recover its key.

`source_id` identifies the same native event part in pages, watch and supervised
projections. File identities combine carrier, byte position and the projected
record hash; separate repeated records retain separate identities. OpenCode uses
session and native part identities so revisions replace earlier presentation
items. Clients should prefer `source_id` over transport item IDs. A watch
`session.history.status` event reports `complete`, `failed` with an error kind, or
`reset` when file coordinates expire. A reset invalidates the displayed watch
projection and requires fresh pagination. This status concerns reading, not the
execution state of the harness.

History RPC failures retain the existing JSON-RPC error code and add `data.kind`,
`data.retryable`, and `data.restart`. Kinds include `source_missing`,
`source_unreadable`, `invalid_record`, `incomplete_record`, `cursor_expired`,
`source_changed`, `resource_limit`, `unsupported`, `busy`, and `read_failed`.
Errors do not include native paths or parser excerpts. Missing or damaged sources
are not successful empty histories. A caller retains already displayed messages
on failure and offers an explicit reload for expired snapshots.

## Local verification

Build the CLI, then run:

```sh
cargo build --bin agit
python3 tests/desktop/history_rpc.py target/debug/agit /tmp/history-watch.json
```

The test starts an isolated real daemon with synthetic Codex/Claude JSONL and an
OpenCode SQLite store. Runtime discovery shims refuse harness execution. It checks
pagination/watch overlap, repeated text, live appends, replacement and revisions,
read errors, and snapshot scope. It does not contact an LLM or a hosted workspace.

For the backend collector integration, keep the fixture daemon running:

```sh
python3 tests/desktop/history_rpc.py target/debug/agit --serve /tmp/history-fixture.json
```

The backend repository documents the collector and component commands in
`docs/web-history-checkpoints.md`. Stop the fixture process after those checks;
its temporary sources and daemon are owned by the test process.

## Projection measurement (2026-09-23)

A release-profile offline reader on einsia-h1 isolates history processing from
the Cloud route. In the baseline at `20d7aaf8`, persona projection recompiles the
username and hostname regexes for every JSON key and string. Compiling them once
per `Redactor`'s fixed persona removes that repeated work. Identity evidence and secret
policy still run on every read; protected pages and authorization are not cached.

| Saved fixture | Warm read before | Warm read after | Projected items |
| --- | --- | --- | --- |
| Staging collaboration | 254–264 ms | 55–65 ms | 28 |
| Production collaboration | 343–347 ms | 123–129 ms | 30 |

Each range covers three warm reads after an initial read, using the same build
profile and toolchain. Item digests and byte lengths agree across baseline,
candidate, and an independent read through the published 0.2.4 daemon. Existing
redaction tests also pass, including explicit-policy reload and failure isolation.
These bounded local measurements do not establish end-to-end Cloud latency or
fleet percentiles. Remaining work includes cold initialization, native evidence
loading, session creation, and the separately tracked browser reset.

The same candidate also passes a staging Cloud trial on chiikawa: two accounts
open four connections and perform twelve simultaneous reads of an existing tool
conversation. All return identical items without reconnecting. Warm requests take
1,196–1,206 ms, compared with 1,616–1,627 ms in a passing published-0.2.4 trial;
first-page requests take 1,916–1,923 ms and 2,077–2,086 ms, respectively. The
executor performs one upstream projection per concurrent batch. This still exceeds
the latency target and compares different builds, so the isolated same-toolchain
measurement above is the evidence for the specific code change.

An earlier official-package trial fails before reading history: the executor
logs completion of `machine.describe`, but the backend's shared-session handshake
times out and reconnects. The later official-package trial passes. The failed
trial and correlated AWS logs remain part of the evidence; this optimization does
not establish the cause or repair of that intermittent handshake failure.
