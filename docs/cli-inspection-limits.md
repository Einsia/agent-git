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
For a subsequent historical import, the physical leaf length is not the reconstructed
length. `Limits::capture()` allows 512 MiB of output and 1 GiB of working buffers.
`lineage_bytes_with_lookup_until` charges the leaf and inherited ranges; the bound-source
`capture_lineage_with_lookup_until` charges the full carriers it retains. `append_bounded`
admits output against both 512 MiB and the working budget minus those retained bytes.
The remaining allowance depends on the carriers, not free system RAM or Hub quota.

When capture hits a byte limit, an eligible historical Codex import streams the lineage.
For a saved v1 turn, its tip, runtime instance, source byte boundary and prefix SHA-256
must agree with the local checkpoint. The importer inherits the existing event objects
and LOG/VIEW IDs, then protects and records only completed new executions. Reading the
old source still costs I/O to verify the digest; it does not require a second full LOG.
A shorter or rewritten prefix cannot advance the ref. The first appended execution must
introduce a user prompt: an already published turn is not amended. Pull is fast-forward
only; a divergent history still requires explicit reconciliation. A different tip or a
missing checkpoint on a populated line is not proof of continuity. This is not an
unbounded archive merge or full-session restore.

On Hub versions with paged transcript support, receive and ordered record pages do not
require a complete inline rendering. Clients must follow the cursor at one immutable
commit through EOF. Event, sequence, Git request, commit-count and repository quota
limits still apply; exceeding them is not a successful partial upload. Publication
retains only records allowed by the existing publication policy.

`benchmarks/codex_first_import.py` exercises first settlement against an explicitly configured
test account and a fresh destination. It generates complete user turns with distinct records,
checks the logical and native session identities across commits, and compares the ordered
JSON values after export. Use `--tail bookkeeping` or `--tail incomplete` to check that the
export and checkpoint agree on the completed prefix. Local readback is not Hub publication. Pass
`--mib` to compare import RSS for different source sizes with the same record and turn shape.
On macOS and Linux the benchmark also samples the importing process and its descendants with
`ps`. The sampled sum includes shared resident pages in each process that maps them and can miss
peaks between samples. It excludes the generator and the subsequent export and comparison.

## Where growth still stops

For an independent historical Codex import, the three reported sizes measure the same
failure shape: a reader tries to admit a whole history as one allocation. They do not
describe three Hub quotas. The ordinary read-only snapshot allows 64 MiB; capture allows
512 MiB. For capture the effective output limit is
`min(512 MiB, 1024 MiB - retained carrier bytes)`. For example, retaining 601 MiB leaves
423 MiB for reconstruction, even on a host with abundant free memory. The eligible import
falls back to streaming when byte admission fails. Live settlement, encrypted evidence,
and a populated branch without a verified checkpoint are not interchangeable with this path.

| Resource | Source and unit | What happens as the session grows |
| --- | --- | --- |
| Cold carrier | `adapter/codex_rollout.rs`: header inspection up to 1 MiB; restoration decodes to a temporary file beside the carrier | The compressed file size is not a decoded transcript budget. Allow disk space for the plain copy; publication of the restored file must succeed before the cold copy is retired. |
| Historical import | `adapter/codex/lineage_stream.rs`, `commands/commit/large_codex.rs`: 8 MiB per record including newline, 64 MiB accumulated through one execution completion, 64 ancestor/leaf segments | Many ordinary turns stream. A single long line, one long execution, or deep pagination can still stop import. A partial final record is not a completed turn. Do not truncate the source to make it fit. |
| Privacy and storage | `privacy/projector.rs`: 8 MiB per worker unit including context; `storage.rs`: 64 MiB per wrapped event, 1,000,000 occurrences per LOG or VIEW | Raw record bytes are not wrapped event bytes. Transformation must preserve record boundaries. Repeated IDs still consume occurrences and the importer retains the sequence and per-ID metadata. No amount of Git compression extends the count limit; start another session before reaching it. |
| Repeat and append | `commands/commit/large_codex.rs`: checkpoint byte count, digest, runtime identity and branch tip; same per-execution and sequence limits | Repeat verifies the old prefix; a completed suffix is committed on top of it. A retry cannot treat an unverified prefix as saved. Runtime, disk usage and cumulative sequence copying can grow even when the event bodies stream. |
| Projection and transport | `hub/git/frozen.rs`: per-push advertised new commits and packed bytes; pack planner reserves 1/16 of the byte budget | Dev advertises 4,096 commits and 2 GiB per request. The planner can split a long history at successive fast-forwards; it cannot split an indivisible commit. Check the remote tip before retrying a multi-request push. |
| Hub admission | Hub `infra/config.rs`, `HistoryLimits` defaults: 512 MiB cumulative inspection per push, 16,384 first-parent binding steps, 50,000 whole-history commits | Inspection charges the bytes inspected, not the raw input length or the compressed pack size. The two advertised transport limits do not expose the other deployment overrides. |
| Plain publication VIEW verification | Hub `privacy/session/manifest.rs` calls `gitsync/routes.rs::check_view_consistency_with_budget`. It uses `DEFAULT_PAIR_BUDGET_BYTES` (1 GiB) for the request and overrides the materializer's expanded-size limit to that value | A private plain session can pass local settlement and export yet receive HTTP 422 when its published sequence expands past 1 GiB. Repacking, splitting transport, or retrying the same tip does not shrink that sequence. Keep the native source, local branch and verified file export; continue new work in a separate session until the receiver can verify the original. Do not publish an incomplete prefix as the whole session. |
| Repository capacity | Hub `Quota` defaults: private account 10 GiB total, organization 30 GiB total; public repository soft/hard 10/12 GiB | Usage across private repositories matters. Splitting transport does not increase quota. Check actual account usage before retrying; do not delete evidence to make a benchmark pass. |
| Hub readback | Hub `gitsync/materialize.rs`, `record_page`: at most 64 occurrences and a 4 MiB page target; first event may occupy up to the 64 MiB event cap | Page at an immutable commit and compare through EOF. Each call still reads sequence IDs (up to 41,000,000 bytes on Hub). Whole-sequence readers keep their 512 MiB expanded limit. Hub process admission is separately configurable: by default it uses the cgroup limit less cache and reserve (defaults 512 and 1,536 MiB). This is not the CLI reconstruction formula. |

To exercise growth on a fresh private `fleet-dev-test` destination, use the opt-in
benchmark with `--mib 600 --cold --growth`. It requires `zstd` and an isolated dev
login. It checks the local export and unchanged duplicate before publication, compares
each published snapshot at its own commit, and retries a rejected incomplete input.
Use a separate fresh destination with `--mib 1024 --cold --growth` to probe the
receiver's expanded-size boundary. If the first push is rejected, local checks have
passed but no remote readback or append has been verified by that run.
For an exact byte boundary, use `--bytes` instead of `--mib`; the boundary is the end
of a complete execution, not the size of a truncated file. The wrapped LOG and the Git
request differ in size from this native JSONL input.
This workload grows the transcript and turn count, not every individual protocol unit:
passing it does not establish that an oversized event, a full account or a million-entry
sequence can be published.
