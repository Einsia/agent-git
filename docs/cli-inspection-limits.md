# CLI inspection limits

A limit on a complete in-memory snapshot need not limit a read-only preview. For Codex,
`import --propose-lineage` can switch from the ordinary snapshot reader to a record stream.
The stream reads the selected leaf and only the inherited ranges of its ancestors. It checks
the file identity, record boundaries and parent ordinals before reporting a candidate. It
hashes all reconstructed bytes, even after it has enough record hashes to compare every
admissible stored candidate. This does not change the limits on capture, upload or publication.

| Location | Budget | Decision and purpose |
| --- | --- | --- |
| `adapter/native_snapshot.rs`: `MAX_NATIVE_BYTES`, `MAX_WORKING_BYTES` | 64 MiB of transcript, 256 MiB of owned buffers | Remove as Codex preview rejection criteria. Keep on callers that materialize whole byte vectors; the preview can stream even when a small leaf inherits a large prefix. |
| `adapter/native_snapshot.rs`: `MAX_NATIVE_RECORDS`, `MAX_CAPTURE_BYTES` | 262,144 records, 512 MiB of capture | Remove from streamed Codex preview. Keep for bounded snapshot and export/capture operations that retain the data. |
| `adapter/native_snapshot.rs`: lookup and hashing | 262,144 directory entries; 512 MiB hashing allowance | Keep: ambiguous IDs must not silently resolve to the first file encountered, and background diff or continuation checks bound filesystem work. These are not limits on streamed transcript length. |
| `adapter/codex/lineage_stream.rs`, `adapter/codex.rs` | 8 MiB per streamed record, 64 segments | Keep: JSON parsing needs a complete record; every open segment holds a file descriptor. Repeated rollout IDs are rejected independently. |
| `commands/import/lineage/observed.rs`, `domain/import_lineage.rs` | 16 MiB retained streaming prefix; 16 MiB per stored candidate; 128 MiB total input and 256 MiB hydrated output | Keep: exact matching uses record hashes beyond the raw prefix; materialized baselines outside it cannot be certified. Repository objects and hydrated candidates still occupy memory together. |
| `commands/push/audit_workspace.rs` | 32 MiB of evidence, 16 MiB per object or manifest, 8 MiB of context | Keep: push constructs and verifies an immutable review workspace from reachable objects. Silently omitting excess evidence would misrepresent what was supplied. |
| `commands/scan/runtime.rs` | 3 MiB prompt, 1 MiB output, 128 KiB help | Keep: these bound input to a reviewer subprocess and data returned by it, not the native transcript. |
| `commands/merge/archive/file_agent.rs` | 512 KiB per seed or launch receipt | Keep: private structured metadata is read and parsed as a whole to verify which merge attempt may proceed. This is not the exploration transcript. |
| `commands/doctor/history.rs` | 10,000 commits, 200,000 reads, 512 MiB total | Keep: a diagnostic walks a potentially shared object graph, including objects fetched from elsewhere. The aggregate limits bound traversal and expansion rather than rejecting import of a single local session. |
| `adapter/opencode/status_snapshot.rs` | 16 MiB SQLite image, 2,000,000 VM steps | Keep: status copies a database image under read locks and runs a bounded query. The image is not a picture attachment. |
| `crates/agit-peer` (`lib.rs`, `client.rs`, `identity.rs`), `crates/agit-tunnel/src/protocol.rs` | 8 MiB peer frame or tunnel payload, 4 MiB peer response, 16 KiB certificate | Keep: network input comes from outside the process. Frames, decoded records, responses and certificates need admission before allocation or parsing. |

## First settlement is not lineage preview

`import::run_with_output` claims the branch, then `commit::record_at` records each finished
turn. In CLI 0.2.61, `settle_local_telemetry_inner` checks the physical transcript length
against `storage::MAX_MATERIALIZED_BYTES` **before reading it**. The early refusal protects
memory: ordinary settlement holds source bytes, the parsed session, the privacy projection
and snapshot-building buffers. It is not a Hub response. Without a native checkpoint, the
continuation reader cannot establish that any historical event has already been settled.
Simply removing the length check leaves these independent limits:

| Stage | Scope and consequence |
| --- | --- |
| Codex source reconstruction | `read_native_bytes_at` uses `Limits::capture()` (512 MiB input, 1 GiB working bytes); capture and read-only snapshot callers have separate record and lookup budgets. A small leaf may inherit a much larger prefix. |
| Privacy | The worker admits 8 MiB units. The protector can divide larger supported records, but a skipped, partial or omitted result cannot establish a lossless import. Encrypted recovery envelopes have their own limits; a new plain session does not use them. |
| Storage v1 | LOG and VIEW list event occurrences, not file fragments. Each sequence has at most 1,000,000 entries; an individual canonical event is at most 64 MiB. Materializing readers account for the expanded LOG and VIEW (including repetitions) and bounded distinct event bytes; deduplicating Git objects does not reduce the expanded size. |
| Checkout | Refresh snapshots pre-existing working-tree bytes for rollback (512 MiB total); this is distinct from a newly added event. A fresh import must not discard local edits to fit. |
| CLI publication | `SessionProjector` checks the distinct, LOG and VIEW byte budgets. `push` groups up to 48 branches with a 1 GiB estimated event budget, but an oversized branch goes alone. Grouping branches does not divide a single LOG. |
| Hub transport | Query `GET /api/hub/config`. Dev advertised 4,096 new commits and 2,147,483,648 request bytes per push. The latter counts the Git request, not the expanded JSONL. Private account quota, inspected uncompressed bytes and public repository quota apply separately. |
| Hub transcript | `gitsync/materialize.rs` on Hub main `980c85c45fd4` imports the CLI storage limits: at most 41 bytes of sequence per occurrence, 64 MiB per event and 512 MiB per expanded sequence. Its pair allowance is twice the sequence allowance, **not** 1 GiB for one LOG. A successful receive alone does not establish complete readback. |
| CLI readers | Ordinary materialization, including full in-memory renderers and resume, keeps its 512 MiB ceiling. Exporting a complete v1 sequence to a named JSONL file can verify and project batches of events before installing the finished file. This is not a streaming implementation of every renderer. |

For a fresh, independent Codex import above the materialization limit, the CLI reads one
bounded execution at a time. A completed execution without a new user prompt continues the
same turn; its records extend that turn's tree without adding another turn to branch history.
The branch tip and the local source checkpoint stop at the last verified completion. Later
bookkeeping or an unfinished execution stays in the native file, outside the saved prefix.
This path still requires explicit completion markers and applies the per-record, per-execution
and v1 sequence budgets. It does not change the limits of subsequent live settlement.
Repeating an import of a fully settled rollout is a no-op when the saved tip, reconstructed
byte count and SHA-256 still match. A changed rollout is not acknowledged as already saved.

`benchmarks/codex_first_import.py` exercises first settlement against an explicitly configured
test account and a fresh destination. It generates complete user turns with distinct records,
checks the logical and native session identities across commits, and compares the ordered
JSON values after export. Use `--tail bookkeeping` or `--tail incomplete` to check that the
export and checkpoint agree on the completed prefix. Local readback is not Hub publication. Pass
`--mib` to compare import RSS for different source sizes with the same record and turn shape.
On macOS and Linux the benchmark also samples the importing process and its descendants with
`ps`. The sampled sum includes shared resident pages in each process that maps them and can miss
peaks between samples. It excludes the generator and the subsequent export and comparison.
