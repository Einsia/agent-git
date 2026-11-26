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
