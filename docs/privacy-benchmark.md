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
