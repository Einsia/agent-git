# agit CLI JSON output

`agit --json <command>` (and the compatible `agit <command> --json` spelling)
emits exactly one JSON document on stdout. The normative machine-readable
default contract is [`cli-json-schema-v2.json`](cli-json-schema-v2.json), checked into the
source tree as the single source of truth. A future packaging step can install
that file next to the binary for local validation, but the output itself does
not depend on an installation path. It deliberately carries the stable schema
name `cli-output` and `schema_version: 2` instead of pointing at an assumed
online URL.

## Envelope

Every supported command uses the same top-level fields:

- `schema`: the stable schema name (`cli-output`). It is intentionally not a
  network URL; installations may live in different prefixes or be offline.
- `schema_version`: currently `2`; consumers should reject or explicitly
  negotiate versions they do not understand.
- `command`: the canonical top-level command name.
- `ok`: `true` exactly when `exit_code` is zero.
- `exit_code`: the normal [CLI exit code](00_usage.md#exit-codes), including the supported
  generic failure code when a more precise cause is not established.
- `result`: command output, represented as `json`, `json_lines`, `text`, or
  `empty`.
- `diagnostics`: captured stderr diagnostics as `{level, message}` objects.
  Stdout is not duplicated: it belongs exactly once in `result`.
- `fix`: alternative typed recovery commands. The CLI reports them without execution;
  see [recovery actions](cli-json-fixes.md).

Use `--json --json-version 1` to retain the legacy envelope and validate it with
[`cli-json-schema.json`](cli-json-schema.json). That version has no `fix` field.
The original version 1 schema is preserved unchanged.

`result.format=json` preserves an existing structured command value under
`result.value`; it is not JSON encoded as a string. JSONL output uses
`result.format=json_lines` and decoded `result.values`. Human-oriented commands
temporarily use `result.format=text` with newline-free, non-empty `result.lines`;
blank presentation lines are omitted. Future iterations can add command-specific
`kind` values and structured fields while
keeping this envelope stable.

The `whoami` command is the first command-level structured result. Its
`result.value` contains `hub`, `account`, optional `email`, non-secret access
and refresh token states with expiry timestamps, and `check` fields including
whether an online check was requested and whether the server was reachable.
Token values themselves are never included.

## Platforms and execution

The envelope is available on macOS, Linux, and native Windows MSVC builds.
Capture runs in process and preserves arguments, stdin, diagnostics, and the
command's exit code without dispatching the command again. Windows redirects
both CRT descriptors and Win32 standard handles. JSON v2 retains typed recovery
actions; `--json-version 1` selects the compatible legacy envelope.

Capture setup failures reject the command before it runs. Incomplete capture
returns a failure with an explicit diagnostic; the command may already have run,
so inspect its state before retrying a mutation.

Interactive and long-running commands reject JSON before preparing storage or
starting work. Use the corresponding finite form, such as `login --with-token`,
`open --no-launch`, `new --no-launch`, `resume --no-launch`, `merge --manual`,
or `rc start --detach`, when requesting a JSON document.

The hidden `hooks` and `mcp` commands are excluded: they own stdin/stdout as
line-oriented protocols, so wrapping their stream would make the protocol
invalid. Their existing protocol formats remain unchanged.

## Local import lineage reports

`agit import <full-native-id> --from <runtime> --into <owner/repo@branch>
--propose-lineage --json` returns an `import-lineage` object inside
`result.value`. Its separate contract is
[`import-lineage-schema.json`](import-lineage-schema.json). Register the existing
[`cli-json-schema-v2.json`](cli-json-schema-v2.json) schema under its
`cli-output-v2.json` ID when validating the report's typed commands offline.
Both outer envelope versions preserve the same nested report; successful previews
have an empty v2 `fix` array, and v1 has no top-level `fix` field.

A report identifies the explicitly supplied native session and destination. Each
candidate carries a frozen commit, references reaching it, completed prefix turns,
native records, and an evidence class. Exact native record equality and verified
materialization evidence support an explicit base choice; they do not assert an
observed Git parent of the external native transcript. No transcript content,
secret values, or transcript digests appear in the report.

`scan_state: complete` means the bounded local inspection had no unavailable
component. It never claims that semantic discovery is available. Missing local
repositories, unreadable claims, partial native data, missing Git objects, and
inspection limits produce `incomplete` with typed `unavailable` reasons. A report
can retain valid candidates alongside unavailable evidence. A missing or
ambiguous native identity is a command error instead of an empty report.

The report's `apply` and `independent` fields contain advisory `FixCommand` data;
no action runs during reporting. They are `null` when safe command routing cannot
be represented. Candidate actions use the full frozen OID and explicit native
and destination arguments. The independent action supplies `--independent` and
keeps the ordinary import's claim, permission, and settlement checks. Neither
option authorizes rerouting an existing claim without its normal confirmation.
An import without `--onto`, `--independent`, or `--link-only` requires an explicit
choice unless it repeats the same active destination claim. Non-interactive
invocations return exit code `8` with `operation: "choice_required"`; v2 also puts
the available actions in `fix`, while v1 keeps its existing envelope. `--yes` does
not choose a candidate. The interactive menu defaults to cancellation and
revalidates an accepted observation before acquiring write authority.

## Local branch synchronization in status

The status report lists each repository's bounded `branches` page. Its `items`
retain full branch tips and tracking refs, a human-readable `state`, and numeric
`ahead`/`behind` values when the locally available immutable history proves them.
Unknown or unavailable comparisons use null counts. A failed inspection uses
`items: null` and a non-null `error`; it does not claim an empty repository.
`omitted` counts undisplayed branches, while top-level `repositories_omitted`
counts repositories outside the shared display budget. Status neither fetches
missing objects nor treats the primary checkout's HEAD as every branch's sync state.

## Pending native activity in status

Default status inspects bounded native evidence for displayed, uniquely claimed
session rows. It performs no network requests and does not create or modify
native transcripts, databases, WAL/SHM files, export caches, or temporary
snapshots. This is a bounded observation, not an unconditional instant lookup.
`--check-missing` separately requests unadopted-session index discovery.

For OpenCode, status reads pinned database and sidecar handles under SQLite's
cooperating read locks, validates the committed WAL frontier, and reconstructs
an owned in-memory database. SQLite receives only that memory image. A supervised
worker limits acquisition and query time; it returns the pending summary without
returning native message contents. Missing, malformed, changing, unsafe, or
over-budget evidence produces `unavailable`, never an inferred zero count.
A hot rollback journal or an orphaned WAL index is refused without recovery.

OpenCode counts use canonical native message/part identities. In-place revisions
can change an existing turn without starting another turn; tool updates do not
create another call merely because their state changed. Missing or unclassified
records make semantic counts lower bounds. Compaction evidence is reported
separately. A materialized session must still match its recorded byte prefix and
branch-tip baseline. Status rechecks the local claim inventory and branch head
before publishing a successful observation.

The current session-detail page shares a 30-second deadline and a 256-MiB work
allowance, inspecting at most eight displayed claims. Each supervised operation
has at most five seconds plus a separate two-second process cleanup allowance.
An OpenCode observation reserves 128 MiB from the page allowance, including
failures. It accepts at most 16 MiB of combined database/WAL bytes, uses a
32-MiB SQLite heap cap, and bounds native output to 2 MiB and 16,384 records.
SQLite VM work, JSON structure, and secret hydration have additional limits.
Rows outside the remaining allowance stay explicitly unavailable. These limits
apply to session-detail inspection, not to every other status section.

Both JSON envelope versions retain the same status fields:
`pending_activity` contains the verified summary or refusal, `last_commit`
identifies the selected local branch evidence, and `local_instance` describes
local claim evidence. A current local claim does not prove that a native process
is alive. Status does not choose a session identity from the first row; process
identity still requires an explicit command target or `AGIT_SESSION`.
