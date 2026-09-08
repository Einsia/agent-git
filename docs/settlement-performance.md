# Native object construction and settlement performance

## Failure and storage boundary

Large imports can stop inside `git hash-object --stdin-paths`: the parent writes the
entire filename stream before reading the child's object IDs. Once both pipes fill,
neither process can advance. An interrupted import can retain its adoption link and
claim commit even though the settlement's final ref transaction never happened.

`tree_apply_owned` now uses gix's object database and tree editor. Payload construction
has no child-process input/output protocol, temporary index, or temporary file per
blob. The existing Git command boundary still resolves the immutable base tree, and
existing ref CAS, checkout journal, recovery and link-watermark publication remain
responsible for making the completed object chain visible.

The dependency is restricted to the CLI feature, with default gix features disabled.
The backend's `--no-default-features` build does not acquire it. Native writes preserve
binary content, modes, Git links, SHA-1/SHA-256 object formats and non-UTF-8 repository
paths. Object replacement is explicitly disabled on the object-store handle. No clean
or smudge filter is used to construct canonical transcript objects.

## Repeated work in settlement

The per-turn loop previously protected the complete prefix again, parsed it again to
find the current compact boundary, rewrapped it, encoded the full snapshot, listed
and removed the previous storage tree, and rehashed every accumulated event. Payload
work therefore grew with the sum of all turn-prefix sizes, not just transcript size.

Native settlement now prepares the closed transcript prefix once:

- Reuse the complete protected transcript and already-parsed runtime IR.
- Keep source-line coordinates separate from protected bytes; replacements may
  change string lengths and even obscure runtime labels in the protected copy.
- Validate the complete canonical snapshot, including cross-turn collisions and
  storage limits, before writing any object.
- Emit each immutable event once. Subsequent turns replace LOG, VIEW and metadata
  and add only newly reachable event objects.
- Stream canonical envelopes through a shared bounded LOG builder, retaining event
  bytes without retaining their parsed JSON trees or an aggregate intermediate JSONL.
- Reuse the encoded sequence when VIEW is identical to LOG, and observe the workspace
  code anchor once per settlement.

LOG and VIEW still contain complete ordered ID sequences, as required by the storage
format. Their comparatively small sequence buffers are rebuilt per turn; this is not
a claim that all work is strictly linear in the number of turns. Restored sessions
with a materialized baseline retain their existing provenance-aware LOG/VIEW extension
path. Both paths use the native tree writer.

The CLI reports preparation and turn-building progress. A built turn is not advertised
as published until the existing expected-old ref transaction succeeds. The adoption
link itself is not made transactional by this change: an interrupted claim can still
be resumed with `agit commit <owner/repo>@<branch>`.

## Measurements

Initial native-writer measurements on the same Linux machine, using release builds
at `de684c0`; each row is a single wall-clock observation.
The installed 0.1.1 baseline used an external input-spooling Git wrapper solely to let
it finish past the deadlock; its settlement algorithm was otherwise unchanged. The
fixed binary used the ordinary system Git without a wrapper.

| Workload | Baseline | Fixed |
| --- | ---: | ---: |
| Real long transcript, 29 turns | 384.34 s | 37.25 s |
| Synthetic, 10 turns / 50 assistant events per turn | 3.976 s | 1.364 s |
| Synthetic, 20 turns / 50 assistant events per turn | 9.689 s | 2.087 s |
| Synthetic, 40 turns / 50 assistant events per turn | 27.195 s | 4.006 s |

The real replay used a fresh isolated copy with the original claim refs and stable
repository-secret identities. Every turn's LOG blob, VIEW blob and complete events
tree had the same Git object ID as the baseline. The final replay used about 1.50 GiB
peak RSS; parsing and preparing the complete transcript still carries an in-memory
cost. These measurements are observations, not timing thresholds in CI.

The synthetic generator uses only fake credentials and synthetic conversation data;
it does not contact a Hub or modify the caller's AgentGit store:

```sh
cargo build --locked --release --bin agit
python3 scripts/bench-settlement.py --binary target/release/agit --turns 10,20,40
```

`--events-per-turn` and `--payload-bytes` vary the workload independently. For an
installed baseline with the deadlock, `--git-bin-dir` can name a diagnostic wrapper
directory that spools `hash-object --stdin-paths` input before executing system Git.
The wrapper is a baseline measurement aid, not part of the production implementation.

## Streaming snapshot follow-up

Snapshot encoding validates canonical envelope bytes once per LOG event and retains
those bytes directly, instead of keeping the full parsed LOG and repeatedly serializing
and parsing its envelopes. A shared `SnapshotLog` builder enforces the byte/event limits
and collision checks before accepting each event. Native preparation feeds it one wrapped
source line at a time, without accumulating a second complete envelope JSONL. An identical
VIEW reuses the encoded LOG sequence; distinct VIEWs still undergo strict validation and
must resolve to identical bytes in the validated LOG object map.

Sequential release replays on the same machine, each restored from the same pre-import
backup, compared this follow-up with `de684c0`:

| Real transcript, 29 turns | `de684c0` | Streaming snapshots |
| --- | ---: | ---: |
| Complete settlement | 35.47 s | 28.31 s |
| Snapshot preparation | 11.45 s | 5.03 s |
| Peak RSS | 1,565,784 KiB | 1,153,692 KiB |

Every turn's LOG, VIEW and events-tree IDs matched the recovered original history.
Preparation timing spans the CLI's preparation and first-turn progress messages.
These are single-run measurements: overall time decreased by about 20%, and peak RSS
by about 26%. Full source/protected transcripts, runtime IR and pending event bytes
still occupy memory; this is incremental snapshot construction, not a fully streaming
runtime parser. The optimized replay spent about 12.18 s before snapshot preparation
and 11.10 s after it, so snapshot encoding no longer dominates the entire operation.

Small synthetic transcripts were dominated by other costs and showed no consistent
speedup. The same 10/20/40-turn generator measured 1.171/2.138/3.825 s before and
1.084/2.098/4.040 s after. The benefit demonstrated here is for large payloads; these
observations do not establish a throughput improvement for every workload.

## Verification

Regression tests compare incremental and complete snapshot encodings at every source
boundary, including compact boundaries, protection-induced offset changes, duplicates,
malformed/blank lines and an unfinished tail. A bounded child-process regression
constructs 20,000 objects, checks the resulting tree, and verifies that HEAD and the
worktree remain unchanged. Additional tests preserve modes, external Git links,
object format, replacement-ref isolation and raw path bytes.

Validation commands:

```sh
cargo fmt --all --check
cargo build --locked --all-targets
cargo test --locked --tests
cargo clippy --locked --all-targets -- -D warnings
cargo check --locked --no-default-features
cargo build --locked --release --bin agit
```
