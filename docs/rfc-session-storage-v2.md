# RFC: Lossless session storage with shared content objects

Status: Proposed. Implementation and migration are deferred pending review.

## Decision requested

Introduce an explicitly versioned `v2` session layout that stores repeated
message bodies, tool outputs, and embedded media once, while retaining every
event occurrence and its provenance. Use Git for event descriptors and ordered
references, and the existing Git LFS transport for external payload bytes.

Ship compatible readers before enabling writers. Validate the design against
the measured large Codex logs before migrating any history. Raising transcript
or HTTP request limits alone does not satisfy this RFC.

The acceptance boundary is stored and transferred data, bounded memory, and
correct reconstruction. A full native export can still exceed a gigabyte:
reproducing the native format necessarily reproduces its repeated content.
Ordinary browsing, settlement, and publication must not require that expansion.

## Evidence and current behavior

### Measurement scope

The following observations were collected locally on 2026-10-03. Sizes are
decimal MB, rounded for display. Labels identify private benchmark inputs;
transcript contents, native identifiers, and machine paths are not published.

| Input | Raw JSONL | Records | Embedded image data URI bytes | Distinct image data URI bytes | Image occurrences / distinct strings | `compacted` records / bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| A: remote-control architecture | 1,662.80 MB | 152,959 | 815.25 MB | 48.59 MB | 3,009 / 279 | 209 / 880.74 MB |
| B: workspace issue fixes, latest large segment | 1,421.05 MB | 75,013 | 1,008.11 MB | 16.78 MB | 3,888 / 70 | 115 / 1,062.93 MB |
| C: desktop architecture, original segment | 107.93 MB | 7,673 | 69.31 MB | 14.34 MB | 383 / 68 | 11 / 60.49 MB |

Method: parse each JSONL record, recursively visit its `payload`, and count
strings beginning with `data:` and containing `;base64,` within the first
100 characters. Deduplicate the UTF-8 bytes of the complete string with SHA-256.
All counted MIME types in these samples are PNG or JPEG. The byte counts include
the data URI prefix; they are not decoded image sizes or Git pack measurements.
Record sizes include their JSON wrappers. No parse errors were observed.

Most image repetition is inside compaction snapshots. Image strings in
`compacted` alone occupy 761.11 MB in A, 994.26 MB in B, and 54.97 MB in C.
These numbers overlap the compaction and media columns; they must not be added.
Non-image data also matters: `event_msg/item_completed` records account for
501.96 MB in A and 228.17 MB in B. Their exact shared-body savings have not been
measured. This RFC does not infer that all remaining bytes are redundant.

B is one physical segment of a longer native lineage, not the complete imported
session. C is also a segment. Full-lineage benchmarks must include all segments
and must distinguish inherited context from new evidence. The measured largest
records are approximately 5.20 MB, 12.31 MB, and 10.92 MB respectively.

Publication of the two large histories encountered HTTP 413. The inspected
backend default is a 128 MiB Git request limit; the deployed effective proxy and
application limits were not established. A source-file size is not a pack size,
and this RFC does not use the default as evidence of the deployed limit.

### Why existing deduplication does not solve this

The audited CLI checkout is at `103d0fa4`; the backend checkout is at
`248f9e3cbd2027cf63826bce88b118fc98966d7f`. Working-tree changes already exist in
the CLI. This is a source audit, not a statement about all deployed revisions.

- [NativeSnapshots](../src/commands/commit/native.rs) wraps native records as
  envelopes and adds them to `SnapshotLog`.
- [Storage v1](../src/domain/storage.rs) deduplicates complete canonical
  envelopes. `LOG` and `VIEW` hold event IDs; each `events/` file contains an
  entire envelope. A changed timestamp or compaction window produces a distinct
  event containing another copy of otherwise identical nested data.
- [The Codex adapter](../src/adapter/codex.rs) avoids expanding repeated
  `replacement_history` into the display IR when earlier messages exist. It
  does expand an opening compaction boundary for a VIEW-only resume. This is
  presentation behavior; it does not factor the persisted raw payload.
- [Export](../src/commands/export.rs) and the backend's
  `src/features/gitsync/materialize.rs` currently materialize complete sequences
  into strings. Compact storage alone would leave that memory bottleneck.
- [LFS history traversal](../src/domain/lfs/history.rs) already considers
  selected reachable histories, including objects absent from the tip. External
  payloads can use that transport, subject to integration and validation.

The backend pins the shared `agit` dependency in `Cargo.toml` and also owns
materialization logic. Updating only the CLI would produce unreadable sessions
or rejection at publication. The release base and dependency pin must be checked
again when implementation begins.

## Goals and non-goals

Required properties:

- Preserve every accepted native JSON value, including unknown fields and
  opaque reasoning or vendor payloads. Preserve sequence order, multiplicity,
  source, session identity, compaction boundaries, and turn selection.
- Share exact content across compactions, event wrappers, turns, and session
  branches within a repository. Do not conflate repeated user utterances or
  distinct tool invocations merely because their content matches.
- Resume from the selected VIEW without reading or expanding the complete LOG.
- Upload only missing payloads, validate their availability before publishing
  references, and report the expected transfer before starting it.
- Bound memory independently of reconstructed transcript length and retain
  separate limits on work, structure, and emitted bytes.

Out of scope: lossy summaries, perceptual image deduplication, dropping telemetry
or reasoning, semantic merging of messages, changing runtime log writers,
cross-repository existence probes, and rewriting published history. This RFC
does not redesign LOG prefix storage or Git's commit/tree representation.

"Lossless" means equality under the existing envelope canonicalization contract
in [transcript](../src/domain/transcript/mod.rs), after the configured privacy
transformation. Original whitespace, key order, and numeric spelling are not
preserved by that existing contract. Embedded string values and decoded media
bytes must round-trip exactly. Native files and their byte-frontier commitments
remain independent of the storage representation.

## Proposed storage contract

### Explicit layout and identity

`session/meta.json` declares `layout: "v2"` and a frozen encoding profile.
Readers never infer a layout from filenames. Unknown layouts or profiles fail
with an upgrade instruction before attempting to interpret their contents.

The proposed tree is:

```text
session/meta.json                 layout, profile, existing session metadata
LOG                               ordered logical event IDs
VIEW                              ordered logical event IDs
events/<existing fanout>/<id>      v2 event descriptors
objects/json/<fanout>/<sha256>     canonical structured-content documents
objects/data/<fanout>/<sha256>     canonical Git LFS pointers
```

New object IDs use full lowercase SHA-256. JSON and data namespaces distinguish
their interpretation. A data path's ID and size must agree with its LFS pointer,
and the pointer must verify against the payload bytes. JSON document IDs hash
the canonical encoded document bytes, including one final LF. The encoding
profile freezes serialization, factoring rules, and thresholds; changing them
requires a new profile rather than an implicit writer preference.

Retain the existing logical `_object_hash` and event-ID algorithms over the
fully reconstructed protected content and envelope. This keeps sequence
identities and continuity checks stable across representation changes. An event
descriptor carries source, session identity, content hash, profile, and a
structured-content root. Its path's logical event ID is verified by streaming
the reconstructed canonical envelope through the existing hash algorithm.

Logical hashes do not authenticate a descriptor merely by matching its claimed
fields. Validate the physical object closure and recompute logical hashes before
accepting a newly encountered event. Cache completed validation by immutable
object identity and profile, under repository authorization; do not trust an
unverified size or hash assertion. Git commit IDs change when trees change.

All reserved paths participate in storage classification, tree validation,
ordinary-file exclusion, and managed attributes. Agit writes LFS pointer blobs
explicitly and resolves payloads explicitly; generic clean/smudge filters or
line-ending conversion must not alter these protocol objects.

### Typed content references, without collisions with native JSON

Use an encoded node grammar outside the native JSON namespace. Proposed node
variants are:

| Node | Reconstructed value |
| --- | --- |
| `literal(value)` | An unchanged JSON subtree containing no protocol references |
| `object(entries)` | Object keys mapped to encoded child nodes |
| `array(items)` | Encoded child nodes in their original order |
| `json_ref(sha256)` | The JSON value reconstructed from another content document |
| `text_ref(sha256, bytes)` | A JSON string whose exact UTF-8 bytes are in a data object |
| `data_uri(prefix, sha256, bytes, encoding)` | The exact prefix plus a reversible Base64 encoding of binary bytes |

This is a proposed grammar, not wire-ready example JSON. An implementation must
freeze tagged serialization and golden vectors before enabling v2 writes.
Native objects with keys such as `$ref`, `kind`, or `sha256` remain ordinary data
inside `literal` or reconstructed objects; they cannot inject storage references.
Readers accept references only in protocol node positions.

Small subtrees remain literals rather than becoming one file per scalar. A
bottom-up writer factors media and large strings first, then selected structured
subtrees. Codex `replacement_history` entries and ordinary message payloads are
eligible for the same content documents. Large tool output strings share exact
bytes even when their enclosing event types differ. Unknown payloads receive
the same generic size-based factoring without needing an adapter schema.

The initial profile must set deterministic thresholds using the offline
benchmark. Thresholds must include a maximum node/descriptor size, an upper bound
on fanout, and a policy for splitting oversized arrays or text payloads. There
is no adaptive "only externalize after the second occurrence" rule that makes
identical input encode differently depending on prior repository state.

References form an acyclic graph. Validation rejects cycles, unknown node types,
excessive depth/fanout, unresolved IDs, size mismatches, and noncanonical object
bytes. It never treats a failed reference lookup as a literal fallback.

### Media and compaction

Decode a Base64 data URI into a shared binary object only if decoding and the
specified re-encoding reproduce the entire original string exactly. Preserve
the exact URI prefix, including MIME parameters. For unsupported or noncanonical
encodings, store the original string through `text_ref`; do not silently
normalize padding, whitespace, alphabet, or MIME spelling. Validate lengths
before allocating. MIME metadata is not a reason to trust or execute content.

A compaction event keeps all of its fields. Its `replacement_history` becomes
an ordered array of references to message documents; newly introduced summaries
are new documents. Equal bodies can share an object, but each array slot and
each event occurrence remains present. The latest VIEW's closure includes all
retained opening context, even when its original event precedes the VIEW.

The invariant is that VIEW event IDs are reachable from LOG and every selected
event's content closure is available at that immutable snapshot. Reading a VIEW
may inspect LOG IDs for reachability; it must not hydrate unrelated LOG bodies.

## Capture, privacy, and bounded reads

The write pipeline is:

```text
bounded native capture and frontier validation
  -> existing privacy transformation
  -> canonical logical event identities
  -> content factoring and verified object staging
  -> turn snapshot construction
  -> atomic publication of the local branch update
```

Factoring must follow privacy processing. Extracting raw strings or images into
an object cache first would bypass that boundary. Existing encryption and
secret-mapping behavior remain authoritative; the codec does not introduce a
plaintext fallback. Content hashes and deduplication metadata remain scoped to
repository access. Do not add a cross-repository "does this hash exist" API.

Capture must stream closed records rather than load an entire native file and
then another protected copy. Keep the [native archive](../src/domain/native_archive.rs)
rules for malformed completed records, duplicate keys, incomplete trailing
records, byte-frontier verification, and mutable OpenCode observations. Use
bounded temporary storage for prepared objects and a disk-backed index if the
in-memory identity set would exceed its budget. Failed preparation cannot
advance the saved frontier or branch ref.

Provide a shared storage reader with distinct operations:

- Validate and enumerate an immutable snapshot's object closure.
- Project paginated events with lazy media and large-output references.
- Stream canonical envelopes or native JSONL to a writer.
- Materialize a bounded selected VIEW for a runtime that requires a file.

The CLI and backend use the same codec and validation rules, with different I/O
providers for Git objects and authenticated LFS payloads. Existing String-returning
helpers can remain bounded compatibility wrappers; they cannot be the path for
large export, import, publication validation, or indexing. Incremental settlement
must reuse validated prefixes and process appended evidence rather than hydrate
every earlier turn again.

Separate budgets for physical bytes, unique nodes, edges, depth, sequence
occurrences, maximum reconstructed event size, emitted bytes, CPU/work, and
resident memory. Count repeated references in expansion/work budgets even when
they cost only one stored object. Use checked arithmetic. A small DAG can encode
an enormous expansion; streaming alone does not make it safe.

Full export may allow a large streamed output under an explicit operation
budget. It must fail clearly on exhaustion, cancellation, missing objects, or
corruption, never return success with truncation. File exports stage and rename
on success; stdout exports report that output may be partial on failure. Resume
must not launch a runtime until its complete selected file is verified.

## Publication, backend, and viewing

Before changing remote refs, freeze the selected local commit roots and remote
identity, then compute their entire required closure. Upload and verify missing
LFS payloads first. Submit the Git update only after prerequisites are available.
The backend checks repository authorization and object availability under its
publication transaction before acknowledging the refs. A concurrent GC must not
delete staged payloads between verification and ref acknowledgement.

The existing LFS transport is reusable infrastructure, not evidence that these
v2 checks already exist. Backend quarantine validation, session projection,
statistics, search indexing, export, and media retrieval all need v2 support.
The backend's pinned shared library and its separate materializer must change
together. Desktop and web clients need a compatible projected-event API so they
do not receive embedded Base64 copies on every page load.

Media downloads are lazy and authorized against the repository and selected
snapshot; knowing a content hash alone is insufficient. Large tool output can
offer a preview plus explicit retrieval. A preview limit must not replace or
truncate the archived object. A disconnected client may display available text
and an unavailable-asset indicator; export and resume requiring that asset fail
explicitly instead of inventing a substitute.

Preflight reports the following separately, without disclosing transcript text:

| Quantity | Meaning |
| --- | --- |
| Captured source bytes | Selected native records before storage factoring |
| Reconstructed canonical bytes | Logical size of the selected history |
| Unique stored bytes | Git-side protocol objects plus unique external payloads |
| Missing payload bytes/count | Authorized remote batch result for selected roots |
| Git pack bytes | Exact measured outgoing pack, or an explicitly labeled estimate |
| Expected transfer | Pack plus missing payloads; protocol overhead labeled separately |

Report repeat-content savings independently of compression and remote cache
hits. A native file size or deduplicated image total must not be presented as
the expected upload size. If the required pack exceeds the effective advertised
transport limit, stop with an actionable breakdown. On HTTP 413, do not retry
the unchanged pack or silently increase limits. Unknown intermediary limits
remain an explicitly reported uncertainty.

GC roots include retained branches, tags, required historical snapshots,
in-flight publication leases, and local migration backups. Failed uploads may
leave unreferenced staged payloads subject to the existing lease/expiry policy;
successful refs must never outlive their required objects.

## Compatibility and rollout

1. Freeze the encoding profile and publish shared codec golden vectors. Build an
   offline encoder/decoder and benchmark without changing user histories.
2. Add v0/v1/v2 readers to the CLI and backend, including bounded projection,
   export, validation, and LFS resolution. Readers dispatch per immutable
   snapshot, including mixed-layout ancestry.
3. Deploy backend acceptance and read support, and compatible web/desktop
   projection. Advertise a proposed `session-storage-v2` capability and supported
   profiles for the authenticated repository endpoint. The capability's exact
   API is part of implementation review.
4. Enable opt-in writers only after the benchmark and end-to-end gates pass.
   Publication to an endpoint without positive capability evidence stops before
   uploading v2-only objects. It does not silently fall back to the giant v1
   representation. Offline v2 use remains an explicit opt-in.
5. Migrate selected unpublished histories, verify the complete result, and then
   consider making v2 the default for newly created sessions.

Old readers reject v2 rather than misreading descriptors as native envelopes.
Ordinary v1 branches remain usable. Appending a v2 commit to a published v1
history reduces future growth but does not remove old reachable blobs. Downgrades
retain read compatibility or stop v2 writes; they do not mutate v2 objects into
v1 under existing identities. Protocol thresholds and limits must not be relaxed
globally as a rollout shortcut.

## Migration of unpublished histories

Converting only the latest snapshot cannot solve the pending uploads: Git also
transfers all missing ancestors and their large event blobs. The migration unit
is the complete selected reachable history, not just its current VIEW.

For each candidate, require an authenticated remote check establishing that its
history is unpublished. If that cannot be established, stop the replacement
operation and keep the original refs. Do not infer absence from a failed fetch
or an empty local tracking ref.

Build a parallel converted commit graph under temporary local refs. Convert
every selected historical snapshot, preserve parent relationships within the
converted graph, turn ordinals, selected LOG/VIEW contents, logical session
identity, titles, ordinary files, and capture-frontier meaning. Remap parents
that would otherwise retain the large v1 closure. Unrelated shared-file ancestry
may be retained only after proving that it cannot bring the old payloads into
the outgoing history. Reject unsupported topology rather than flattening it.

Produce an old-to-new commit map and verify every corresponding snapshot.
References embedded in metadata must be classified: remap navigational commit
links through that map, and preserve immutable source-evidence identifiers with
an explicit migration record. Resolve tags and other aliases deliberately;
publishing an old alias would reintroduce the original blobs.

Only after verification, atomically redirect the unpublished local session ref
and its runtime link, using the frozen old head as a compare-and-swap condition.
Retain the original graph under excluded local backup refs. Publish only explicit
converted refs, never `--all`. No rebase, force-push, raw-log deletion, or remote
history replacement is part of this migration.

Rollback before publication restores the recorded local refs and links. After
publication, correction uses forward commits or a separately named history;
backup refs are not automatically pushed. Published sessions are outside this
migration's replacement scope. Selecting failed sessions or the currently
excluded conversation is not a side effect of format migration.

## Acceptance and benchmark plan

### Maintained correctness contracts

Use a focused end-to-end fixture containing compaction, repeated image and tool
content, equal messages occurring at distinct positions, unknown native fields,
and a VIEW beginning at a compaction boundary. Check every recorded turn:

- Streaming decode reproduces the canonical protected envelopes, logical event
  IDs, source/session attribution, sequence multiplicity, and selected VIEW.
- Original data URI strings and decoded media bytes match exactly; noncanonical
  Base64 exercises the lossless text fallback.
- VIEW-only resume includes retained opening context without hydrating unrelated
  LOG bodies. Full export preserves every accepted record.
- Repeated content adds references, not duplicate payloads. A no-op settlement
  and retry add no objects; a new turn uploads only its missing closure.
- Privacy transformation precedes extraction, and protocol-looking native keys
  remain data.

Add focused failure-boundary cases for corrupt/missing objects, conflicting
sizes, cycles or expansion-budget exhaustion, interrupted publication, and
unsupported reader/profile versions. An asset upload failure must leave refs
unchanged. Failed resume must leave the runtime unlaunched. Do not add redundant
runtime matrices that exercise the same storage codec path.

### Offline corpus gate

Keep private logs out of fixtures and CI. Record a local manifest of immutable
input fingerprints, byte lengths, lineage coverage, tool revision, profile, and
benchmark configuration. Capture machine-readable results for A, B, C, and the
complete relevant lineages. Verify logical round trips by streaming rather than
holding input and output in memory simultaneously.

Measure capture, factoring, all-turn conversion, validation, VIEW resume,
paginated browsing, full export, initial publication, and one incremental turn.
Report peak RSS, elapsed time, Git object/tree counts, unique JSON/data bytes,
serialized sequence bytes across commits, actual pack bytes, LFS bytes, and
verified remote cache hits. Include cold and warm object-cache behavior.

Proposed release gates, to validate rather than claim as achieved:

- Each measured image string in A/B/C reconstructs exactly, and each eligible
  binary object is stored once per repository closure.
- For A and B individually, total v2 unique protocol and payload bytes are at
  most 25% of the corresponding canonical v1 event-body bytes. Count all unique
  objects needed by the converted history, not just its tip. Report Git tree
  overhead separately so a path explosion cannot hide behind this ratio.
- Each initial outgoing Git pack fits the target's confirmed effective transport
  budget. LFS transfer size is reported separately, not counted as zero.
- Peak RSS for streaming capture, export, and validation stays within a proposed
  256 MiB benchmark budget, excluding the separately launched runtime. Record
  per-process and concurrent aggregate peaks when helpers are involved.
- Increasing repeated compaction content grows reference/index bytes without
  replicating payload storage. Append settlement avoids full-prefix decoding;
  cumulative all-turn conversion does not rematerialize every earlier prefix.
- Full-lineage results disclose their own sizes and pass the same boundedness
  checks; the segment table cannot stand in for a full migration measurement.

The size and memory targets are proposed acceptance thresholds, not existing
defaults or measured outcomes. Failure triggers a design review of factoring,
object granularity, or indexing; it does not justify deleting evidence, hiding
LFS bytes, or merely raising caps. The measured image deduplication alone cannot
establish that these gates will pass.

## Alternatives and remaining decisions

| Alternative | Assessment |
| --- | --- |
| Raise read/request limits | Leaves repeated storage, bandwidth, and memory growth intact |
| Rely on Git compression or repack | Useful orthogonally, but does not establish bounded logical reads or shared payload transport |
| Externalize images only | Necessary savings, but leaves repeated message and tool bodies |
| Store only IR or latest VIEW | Loses native evidence, unknown fields, or earlier turns |
| Compress the entire log into one archive | Retains whole-history upload/read coupling and poor incrementality |
| Reference whole compaction records | Their changing wrappers still prevent reuse of nested content |

Before implementation approval, settle the exact node serialization, profile
thresholds/chunking, resource-budget defaults, capability API, and migration
command/report interface. The corpus benchmark must determine whether JSON
objects need additional grouping to avoid excessive tiny Git objects or LFS
requests. Packing/grouping must preserve random access and object verification.

This RFC changes documentation only. It does not enable a writer, alter a read
limit, migrate a session, or deploy either repository.
