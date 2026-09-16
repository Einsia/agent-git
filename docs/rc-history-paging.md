# Native history paging

The executor advertises `machine.describe.history` with `version: 2`,
`snapshot: true`, and the supported runtime names. Callers must negotiate this
capability before requesting consistent history. Paired Hub connections retain
bounded watch history; the Cloud executor endpoint provides pagination.

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
