# Privacy benchmark

The CLI project runs an optional, offline comparison of two prebuilt CLIs against
reviewed synthetic conversations. The backend and GitOps have no benchmark workloads.
The parent pipeline's `privacy:bench` trigger is manual, optional and available only on
protected main outside scheduled pipelines. It has no downstream wait strategy. No
regular test, package, release, notification or merge gate depends on its result.

## Run in CI

Set these variables when creating the main pipeline, then play `privacy:bench`:

| Variable | Default | Accepted values |
| --- | --- | --- |
| `BENCH_BASELINE` | `release:latest` | `release:VERSION`, `release:latest`, `release:previous`, `dev:JOB_ID`, `dev:latest` |
| `BENCH_CANDIDATE` | `dev:latest` | Same selectors |
| `BENCH_PROFILE` | `default` | `default`, `entropy`, `all` |

The manual trigger button is not a parameter-entry form. Its child receives only the
selected benchmark parameters, not arbitrary parent pipeline variables. Release pairs,
dev pairs and mixed pairs all follow the same resolution and verification path. Choosing
the same selector on both sides explicitly requests a consistency comparison.

Provision `BENCH_GITLAB_READ_TOKEN` as a **masked, protected** project CI variable with
`read_api` access to CLI project 87. It is required even for a release pair so a missing
credential never silently changes the configured execution mode. It is used only for
build queries and artifact downloads. Never put its value in Git, command arguments,
fixtures, reports or ordinary runtime secrets. No Hub login, GitOps key, AWS permission,
report-repository credential or real session is needed.

The child uses the protected `sg-network-io` runner pool instead of the CLI's ordinary
Rust validation pool. `resource_group` permits one benchmark job at a time; the Python
runner pins itself and its descendants to one available CPU. The job has a timeout and
an earlier process timeout so normal termination can flush a report. Processes run in
separate groups; completion, errors and handled cancellation kill and reap the groups.
A forcibly lost runner may retain only the last uploaded/available phase files.

Download `privacy-bench-report/` from the child job artifacts and open `report.html`.
The report is standalone, has no external assets or service dependency, and escapes all
constructed input. `results.json`, `versions.lock.json`, `dataset-manifest.json` and
`diagnostics/` preserve machine-readable evidence. Artifacts use project access control.
The job prints the result and artifact path. A child failure stays visible without
blocking its parent. Artifacts expire after the configured retention period.

## Sources and reproducibility

Release resolution checks the public main npm package's stable versions and the matching
`@einsia/agent-git-linux-x64` package. A previous release must be a distinct available
platform version. It verifies SHA-512 npm integrity, the packed package name/version,
records archive and binary SHA-256, and checks the binary's `--version` before evaluation.
The platform release contract uses `x86_64-unknown-linux-musl`.

Dev resolution queries successful `dev:linux` jobs on main in project 87. The enclosing
pipeline need not be successful. It selects a job with artifacts, downloads by immutable
job ID, and checks channel, GNU target, version, full commit, pipeline SHA and SHA256SUMS
against the packaged `BUILD-INFO.txt`. A failed artifact download fails the run; it does
not select an older build or rebuild source. Requests refuse redirects to avoid forwarding
read credentials to another host. Binaries come only from the configured npm/GitLab origins.

The lock records selectors, actual package/job identity, target, hashes, runner commit,
runner script hashes, adapter version and dataset/annotation versions. Each run resolves
once, then evaluates sequentially. GNU/musl, build flags and platform differences must
remain visible in performance interpretation; elapsed times are not pure scanner speed.
Each case has a fresh home, dictionary and configuration. Initialization, evaluation,
report time and aggregate child CPU/max-RSS measurements are separate. Max-RSS uses the
host resource API and is a process-wide child high-water metric, not a per-case allocation.

## Corpus and scoring

`benchmarks/privacy/generate.py` produces checked-in cases, profiles and independent
annotations. It uses fixed synthetic values, no randomness, real sessions or repository
contents. The private historical audit attachments were not imported; provenance records
that the cases were reconstructed from the policy, detector and projector contracts.
A case is added for a distinct behavior boundary, not for every implementation branch.

Secret annotations partition every decoded string into protected and retained ranges,
using Unicode code-point offsets and JSON pointers. Repeated values have distinct ranges.
Nonstring types and values are preserved. Partial protection is FN; covering a neighboring
retained range is FP. A secret appearing in a protocol identifier can have a different
expectation from the same value in ordinary content.

The worker adapter executes `__privacy-worker-v1` with framed JSON and a synthetic local
Git fixture when repository Policy configuration is needed. The fixture has no remote;
its workspace and source paths are temporary. Public projection uses canonical envelope
order and hashes. `plain` exercises captured-envelope protection without public-view
Policy, while encrypted public views and privacy export share frozen publication
projection. These checks do not exercise encryption transport or repository publication.

The worker's output ranges are aligned through its own recovery dictionary. Exact
reconstruction is required; unexplained changes are indeterminate, not guessed from a
placeholder count. Recovery and repeated protection are checked independently. Raw
responses are retained. Random placeholder IDs are normalized only in the displayed diff.
Rule names absent from the protocol remain unavailable.

### Annotation and execution contracts

Dataset `1.5.0` uses annotation version `3`. The runner validates the entire corpus
before downloading or executing either CLI, including cases outside the selected
profile. Invalid pointers, unknown expectations, missing source values, overlapping
ranges and invalid stream cuts are dataset errors, with the case and annotation
identified in the diagnostic.

Secret annotations still partition decoded strings into `protect` and `keep` ranges.
The generator accepts a string to select every occurrence, or a selection object:

```python
{'value': 'synthetic-value', 'count': 2, 'occurrences': [1]}
{'value': 'synthetic-value', 'start': 10, 'end': 25}
```

Occurrence indices are zero-based. `count` checks the total number of occurrences;
`occurrences` selects which ones to protect. Explicit ranges must match `value`
exactly. Missing values and duplicate or overlapping selections fail generation.
The output structure is checked recursively, including empty objects and arrays,
container lengths, field sets and scalar types. Structural damage is a contract
failure and makes range scoring indeterminate rather than producing guessed counts.

Policy annotations identify both `input_pointer` and output `pointer`. The input
node must equal `value`, proving that the fixture actually contains the intended
target. Expectations have distinct meanings:

| Expectation | Required output |
| --- | --- |
| `keep` | The complete target node equals `value`, including types and neighboring text. |
| `rewrite` | The complete target node equals `replacement`, or its entire string matches `replacement_pattern`. |
| `hide` | The target node exists, the sensitive string is absent there, and the replacement contract passes. |
| `remove` | The target node is absent and its parent container still exists. A null value is not absence. |
| `allow`, `exclude` | The corresponding memory decision has the exact expected action. |

`hide` and `rewrite` require exactly one replacement contract. Patterns are reserved
for contract-defined variable components such as an allocated path alias; surrounding
content remains part of the full match. A hidden value with an incorrect replacement
still records a contract failure. Memory decisions must retain the input path order.
Tool checks preserve each call/result association at its record and block position,
allowing public ID aliases but rejecting duplicated or exchanged references. Exact
annotations on names, arguments and result bodies check their content independently.
The same association checks apply to plain capture.

Publication cases may declare `branch` and `workspace_files`. The branch is passed
to the production publication request. Fixture files are created before the worker
starts, inside its temporary workspace, so a captured read can refer to an actual
synthetic directory and file. Relative paths cannot escape that workspace or write
Git control files. A fixture's filesystem and captured contents must describe the
same scenario; missing parent directories must not accidentally stand in for an
existing source tree.

Every non-memory Policy case declares `runtime` and `record_layout`. The runner uses
the declared runtime for envelope `_source`, publication metadata and capability
probes. It sends the original native records to the worker, without converting their
schema. Conflicting metadata runtimes and unknown runtime names fail dataset validation.
Public projection produces the CLI's canonical message format; plain capture retains
the native format.

`record_layout` independently describes the expected roles and content-block positions.
Its `preserve` pointers identify native role and event-kind fields that plain capture
must retain exactly.
Each block declares its type and binds its structural fields to JSON pointers in the
corresponding native input record. For example, a Codex call binds `id` to
`/records/0/payload/call_id`, `name` to `/records/0/payload/name`, and `input` to
`/records/0/payload/arguments` with `encoding: "json"`. Bindings are resolved only for
structural scoring. They neither modify worker input nor decide which content Policy
should hide. Content expectations still use separate input and output pointers.
Plain output is checked through the same native-field bindings and must also preserve
the complete native JSON structure.
If a completed worker response turns an encoded argument into an unreadable carrier,
the report retains that output and its exact annotation verdicts. Encoded-field and
dependent structural checks fail explicitly instead of discarding the response as an
adapter JSON exception. Input fixtures still require valid encoded fields.

The `native-*` corpus covers protocol envelopes for Claude Code, Claude Desktop,
Cursor, Codex, OpenCode, Hermes, OpenClaw and WorkBuddy. These cases intentionally use
synthetic `Read`/`ReadPublic` tools with a common `file_path` argument to isolate source
routing, Policy overrides and associations. They do not establish coverage of the
agents' actual default toolsets or daily workflows.

The separate `workflow-*` cases use reference-backed tool names and argument shapes
for a read/edit/test task, including string-valued patch inputs. Each declared profile
has public and plain expectations. Claude Desktop and WorkBuddy use an explicitly
configured Desktop Commander MCP profile, not claimed built-in tools. The traces are
constructed from tool references and checked-in transcript conventions, not recordings
of live agent runs. See [tool profiles, sources and limits](privacy-benchmark-workflows.md).

Stream cuts are strictly increasing Unicode code-point offsets inside a text input.
The worker's consumed coordinates are UTF-8 bytes and must end on character boundaries.
The adapter retains the initial context and checks the final stream against whole-text
protection through recovered ranges, independently of randomized placeholder IDs.

Report schema `2` records protection and detection completion separately. Detector
failure cannot erase a completed protection result, and a failed protection run cannot
erase valid detector evidence. Each stage has its own common completed comparison set.
Unsupported and inapplicable detector observations are distinct from failed or partial
execution; an advertised detector that fails prevents a passing run. HTML case summaries
distinguish protection quality from execution completion. Raw detector responses and
errors are retained alongside worker diagnostics.

New binaries also expose `__privacy-bench-v1`: one JSON request on stdin and one JSON
response on stdout, dispatched before configuration, login, Git initialization or telemetry.
`schema_version: 1` with `operation: capabilities` lists operations. `scan_text` takes
`text`, `blocks`, `allows`, and `entropy`, and returns production detector findings with
UTF-8 byte offsets, rule/source and completion. `memory_policy` takes a serialized `policy`
and relative `paths`, returning the production memory decisions without filesystem reads.
Unknown schema/fields and oversized requests fail. The runner converts detector offsets
to annotation coordinates and scores detection separately from final protection.

The release worker supports the default Secret and Policy corpus. Detector telemetry is
optional evidence for text cases; structured worker cases do not invent raw detector
ranges. The `all` profile additionally requests `memory_policy`; binaries before this
entry was introduced report that case unsupported and cannot pass the full profile.
The entry's minimum source revision is `8a753585` (detector telemetry starts at
`3d01ea43`), not a claimed npm version: it must be packaged before `dev:latest` can expose the capability.

Secret and Policy denominators remain separate. Reports show range precision/recall/FN
rate, normal-case FP rate, rewrite/structure failures, all completion states, per-version
coverage and the fixed common completed set. Zero denominators are null/N/A. Missing,
partial, unsupported and indeterminate cases are never TN. Candidate FP/FN or contract
failures fail the child, as does incomplete execution on either side. Baseline quality
failures remain valid comparison evidence and do not prevent a correct candidate passing.

## Local verification

Use Python 3.11 or later. The benchmark has no third-party Python dependencies:

```sh
python3 -m unittest discover -s scripts/privacy-bench -p 'test_*.py' -v
cargo test --locked --lib offline_ranges_preserve_byte_coordinates_and_explicit_rule_priority
cargo build --locked --bin agit
BENCH_TEST_BINARY="$PWD/target/debug/agit" python3 -m unittest discover -s scripts/privacy-bench -p 'test_*.py' -v
cargo fmt --all --check
cargo clippy --locked --workspace --lib --bins -- -D warnings
cargo clippy --locked --no-default-features --lib -- -D warnings
```

On Linux x64, with the protected variable supplied through the environment:

```sh
python3 scripts/privacy-bench/run.py --profile default --output /tmp/privacy-bench-report
```

For explicitly authorized local checks, `--use-glab-token` reads the configured host's
read-only glab credential internally, without printing it or passing it to the tested CLI.
This option is refused in CI. The output directory must be empty so stale artifacts cannot
masquerade as this run. On other platforms, downloads and checksums can still be verified;
execution then fails explicitly because the selected binaries are Linux x64.

When editing the corpus, review input and annotations together, advance `dataset_version`
for a changed released corpus, run `python3 benchmarks/privacy/generate.py`, and commit the
regenerated manifest with the inputs. An expected value must not be derived from either
CLI's output. A failing genuine expectation is a benchmark finding, not a reason to change
that expectation. Local checks expose a default-mode false positive on `YOUR_PASSWORD`
and an entropy-opt-in false positive on a typed SHA-256 digest. These are recorded findings;
selected profiles fail until the production detector satisfies the independent expectations.

### Corpus review

The annotation-version-3 review preserves the inputs and Secret protection decisions of
the original cases. Policy expectations now check complete target
nodes: file exclusion notices and tool names, complete replacement sentences, attachment
notices or retained metadata, precise metadata removal, and ordinary file content.
These expectations follow the production contracts and are not generated from CLI output.

Dataset `1.5.0` contains the original boundaries, 16 native protocol probes, 25 business
scenarios, 16 reference-backed read/edit/test traces, and 11 incident reconstructions.
There are 98 cases: 41 Secret and 57 Policy. The default profile contains 87 cases,
entropy contains 11, and all contains 98. Entropy includes opted-in native/media Policy
cases and its default-mode control. The [case catalog](privacy-benchmark-cases.md)
explains every case's scenario, expected behavior, basis and validity checks.

The workflow fixtures cover valid Python and JavaScript modules, shell commands,
INI credentials, HTTP Basic and signed JWT authentication, a PostgreSQL URI, decoded
JSON escapes, localized logs, stream boundaries, overlapping explicit rules, and
combined file/tool/attachment policies. The PEM pair is a valid offline-generated
test key, and the PNG is a valid image. The generator reads the fixed PEM files;
it never regenerates keys or derives expectations from scanner output.

Run the independent format review before accepting fixture changes:

```sh
python3 benchmarks/privacy/generate.py
python3 benchmarks/privacy/validate_fixtures.py --external
```

The external review uses Node.js syntax checking, a POSIX shell parser and OpenSSL
key inspection. It also replays the fixed calculator edit in a temporary project,
requiring its Python checks to fail before the edit and pass afterward. Recorded shell
commands remain data; the checker does not start agents or MCP servers. The other checks
use Python's standard library. Every declared
format check passed locally. Provider-shaped credentials remain synthetic and are
never sent to an issuer. The AWS profile distinguishes the non-secret access key
identifier from the secret access key using the cited AWS documentation. Public
path aliases are checked for their documented semantic form rather than a particular
allocated alias number.

The [incident review](privacy-benchmark-incidents.md) maps CLI issues and six readable
frozen Hub sessions to the new cases. It distinguishes direct reader-visible anomalies,
workflow corroboration and report-only facts. Samples use newly constructed IDs,
credentials, paths and image bytes. Publication retry, account synchronization, locked
key recovery, file-commit integration and reports without a concrete reproducer are
listed separately rather than represented by misleading scanner-only cases.

Native verification uses CLI `0.2.42` built from main revision `987ecc31`. Two isolated
full-corpus runs completed all 98 cases with identical verdicts. Secret output scoring
reported 14 FP ranges and 3 FN ranges. Policy reported 4 FP checks and 10 contract
failures: the caption rewrite, omitted Codex freeform call/result checks, and opaque
encoded-argument checks. These are failed checks, not a count of independent root causes.
These are quality findings, not execution failures:

| Finding | Cases |
| --- | --- |
| Human-readable password literals remain visible | `scenario-python-settings-review`, `scenario-ini-database-passphrase` |
| Basic Authorization credentials remain visible | `scenario-http-basic-authentication` |
| A valid Python class reference is replaced | `scenario-python-provider-references` |
| Credential replacement also consumes surrounding quotation marks | `scenario-javascript-client-options`, `scenario-shell-password-value`, `scenario-unicode-authentication-log`, `scenario-stream-password-at-eof`, `scenario-stream-multiple-credentials` |
| A bearer replacement consumes the caption's following full stop | `scenario-image-metadata-and-caption-secret` |
| Public projection omits a valid freeform patch call and result | `workflow-codex-calculator-encrypted-public` |
| A valid image companion/raw MCP payload is replaced under entropy opt-in while its equivalent structured/data-URL copy survives | `incident-claude-duplicated-image`, `incident-mcp-image-carrier-equivalence` |
| A blocked content occurrence makes the whole encoded argument opaque; native ID annotations still pass, but argument rewrite/structure checks do not | `incident-codex-call-linkage` |
| Document placeholder and typed digest false positives | `normal-placeholders`, `entropy-source-identifiers` |

The full profile remains failed on these independent expectations. Secret recovery,
idempotence and stream-equivalence checks passed. Every `workflow-*-plain` case and the
other public workflow profiles passed. The remaining incident cases passed their local
contracts, without claiming that historical integration defects are resolved. This is a native same-binary check,
not a Linux CI or release-versus-dev acceptance result.

Before rollout, configure the protected CI variable, verify runner availability, merge the
CLI changes and allow its normal dev packaging to finish. Then the reviewer can run the
manual benchmark. No backend/GitOps rollout or runtime environment variable change applies.

## Implementation verification

Local verification used npm release `0.2.40` and main dev
`0.2.40-dev+4c4265688ba7` (commit `4c4265688ba7b97e85ba7ea9d8971e82815e3fbe`).
The Linux source path verified npm integrity and dev job `155457` build metadata/checksums.
This macOS workstation cannot execute the Linux artifacts; native compatibility execution
used the corresponding macOS release and verified dev job `155458`. Both completed the
default corpus with the documented placeholder FP and no Policy errors. The entropy corpus
also exposes the documented digest FP. The locally built
feature binary also completed the optional detector and memory checks.

The scoring/distribution/report tests, real-worker check, Rust protocol test, rustfmt and
both CI Clippy configurations passed locally. Parent/child YAML parsed and local invariants
confirmed the manual trigger, no wait strategy, independent runner, no normal job dependency
and always-upload artifacts. Service-side REST CI lint returned HTTP 403 because the supplied
read-only token lacks that endpoint's permission; the read-only GraphQL alternative was not
usable on this host. No pipeline was triggered, and no remote CI pass is claimed. Service-side
lint and the protected-main manual run remain acceptance checks for an appropriately authorized
operator. The CLI feature branch has not been pushed or merged by this implementation task.
