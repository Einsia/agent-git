# agit CLI JSON output

`agit --json <command>` (and the compatible `agit <command> --json` spelling)
emits exactly one JSON document on stdout. The normative machine-readable
default contract is [`cli-json-schema-v2.json`](cli-json-schema-v2.json), checked into the
source tree as the single source of truth. A future packaging step can install
that file next to the binary for local validation, but the output itself does
not depend on an installation path. It deliberately carries the stable schema
name `cli-output` and `schema_version: 2` instead of pointing at an assumed
online URL.

Startup update notices can appear on process stderr before the JSON envelope is captured.
They are separate from command diagnostics in `diagnostics.stderr`, never alter stdout or
the command exit status, and never prompt or install. `--quiet` suppresses the startup
update check and notice.

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

`remote` returns `repository` and `remotes` in `result.value`, read offline from
the checkout. Each remote has `name`, `hub`, `address` (the repository's page on
that Hub, or `null` when the checkout records none there), `account` (the
account saved for that Hub, or `null`), and `upstream`. A checkout holds one
pinned Hub, so the list is empty or holds `origin` as the upstream.

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
option authorizes rerouting an existing claim without its normal confirmation:
when the session is claimed on another branch, `claim_move` names that claim and
the destination, the typed actions carry `--yes` only when the inspecting invocation
supplied it, and the human report prints its commands with `-y` after a line that
names the move.
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

## Session order in status

`sessions.items` lists the sessions that ran in the current folder first, marked
`in_current_folder: true`, then the others; within each part, active claims come
ahead of superseded ones. `offset`, `limit` and `next_offset` page that order. The
text view's page holds every session of the current folder whatever its limit, so
the budget applies only to the others, and it names the command for the next page.

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

The appended `project` field is `here` for an exact recorded cwd match, `same-repo`
for a recorded code-origin match, or `other` when no match is recorded. It refers
to the code repository, not the Agent repository. Missing, malformed, changed,
or over-budget evidence is `unavailable: project evidence`. An absent configured
origin is ordinary absence. These labels do not select a session or prove a live
process. Project inspection shares the page deadline, reads at most eight branch
metadata blobs capped at 1 MiB each, and caps origin output at 16 KiB. Final claim
inventory changes invalidate project labels alongside the other observed details.

The appended `upload` field is `uploaded`, `waiting`, `not_uploaded` or null, with the meanings
`agit project list --json` gives them. It is read from the recorded repository's local and
remote-tracking refs, and from its accepted publication record, for every displayed row,
including rows past the eighth, whose transcripts the page doesn't inspect. The ref read names
objects without reading a commit and shares the page deadline. It is null when the claim names
no repository or branch, when this computer has no such branch, or when the refs can't be read
in time. Final claim inventory changes reset it to null with the other details.
Superseded and merge-exploration rows may report their recorded cwd match, but
their code-origin relationship remains unavailable.

## Runtime session in status

`runtime_session` describes the conversation that runtime session variables name,
such as `CODEBUDDY_SESSION_ID` or `CLAUDE_CODE_SESSION_ID`. `state` is `none`,
`detected` or `ambiguous`. `session` is non-null only for `detected` and holds
`runtime`, `session_id`, `variable`, boolean `managed` and nullable `target`
(`<owner>/<repo>@<branch>` of an active complete claim). `managed` is true when the
store holds ownership evidence for the conversation (an agent, branch, baseline or
archive role); a hook placeholder or a `--link-only` link is not managed.
`candidates` lists every named conversation; nested runtimes inherit their parent's
variables, so more than one candidate is reported as `ambiguous` rather than
resolved. A file-backed runtime
counts only when its transcript exists; a database-backed runtime is not opened.
The field is display only: `selection` and every command target still come from
explicit arguments or `AGIT_SESSION`. Both envelope versions carry it.

## Shared files and merge transactions in status

`shared_files` contains `items` and `incomplete`. Each item separates `repo`, nullable `branch`,
nullable `checkout`, nullable relative `path`, `staged` and `local_bytes`. A null branch with a
checkout denotes detached HEAD; a null checkout denotes unavailable repository inspection.
An unavailable file path is null, rather than an empty-string file. State strings describe
observed staged changes and raw local-byte comparisons without Git filters or line conversion.

`merge_transactions` contains `items`, `incomplete` and `repositories_omitted`. Successful items
include `repo`, `target`, `source`, full `target_head` and `source_head`, numeric `picked_count`,
boolean `summary_ready`, and null `error`. Unavailable items retain only the repository and a
fixed error; all transaction facts are null. Status reads retained intent without completing,
repairing or removing it. Summary text and picked-event lists are never returned.

Both output versions preserve exact identity and path strings in these fields instead of
escaped or truncated terminal cells. Empty arrays with `incomplete: true` do not prove absence.

## Enrolled folders: project list

`agit project list --json` describes every folder this machine saves conversations for, offline
and without changing local state: it takes no lock, so a listing never delays or turns away an
automatic upload. `result.kind` is `project` and `result.value` holds:

- `schema_version` (`1`), `hub` (the selected Hub), and `incomplete`: true when a project policy,
  a directory binding or a runtime's session index could not be read, so a short list is told
  apart from a partial one. `unreadable` lists each of those as `what` (`folder upload policy`,
  `folder binding`, `Claude Code conversations` or `Codex conversations`) and `path` (the file,
  or null for a runtime's conversations).
- `folders`: one item per existing folder that has a project policy (`agit project bind`) or a
  directory binding (`agit init`, `agit clone`); a folder with both is listed once, by its policy.

Each folder has:

- `folder`: the canonical path. `state` says what actually happens to a new conversation there:
  `on` (saved and uploaded automatically), `blocked` (a policy that uploads automatically, but
  the repository's automatic publication is off on this machine, so conversations are saved and
  not uploaded until `agit config --repo OWNER/NAME push.auto true`), `off` (a policy that is
  paused or uploads only on `agit project sync`) or `local_only` (saved on this machine only, by
  a binding without a policy).
- `held`: null, or why a folder that should upload isn't, as `code` and `text` (the reason in plain
  words). Codes are an open set: a reader shows `text` for a code it doesn't know and counts the
  folder as needing attention. Today's codes are `push_auto_off` (the `blocked` state),
  `public_needs_consent` (the repository is public and automatic upload into it was never
  switched on), `signed_out` (agit isn't signed in as the folder's account, or an upload failed
  for sign-in and no sign-in was written since), `other_account` (agit is signed in as another
  account or to another Hub; it also carries the `hub` and `account` the folder uploads as) and
  `no_write_access` (the Hub last said the account can't push to the repository). A held reason
  is judged from local state, so it clears once its cause is answered here.
- `repository` (`owner/name`) and `repository_url`, its page on the folder's Hub.
- `visibility`: `public` or `private` as last seen for this folder (the Hub's answer, or an upload
  refused because the repository is public), or null when agit never learned it. The listing
  doesn't ask the Hub, so it can be out of date.
- `writable`: whether the Hub last said the signed-in account may push to the repository the
  checkout is pinned to (recorded by `agit clone` and `agit project bind`), or null when it said
  nothing about that account and repository.
- `auto_push`: the repository's automatic publication preference on this machine
  (`agit config --repo OWNER/NAME push.auto`), or null when it has no local checkout. While it is
  false, the uploads a policy attempts are refused by that choice, not failing.
- `queued`: conversations waiting for the folder's background upload.
- `last_upload`: null, or the last automatic or `project sync` attempt as `at`, `ok`, `error` (the
  failure's own words, without the diagnostic envelope), `reason` (the failure's code:
  `signed_out`, `hub`, `privacy_incomplete`, `too_large`, `builds_on_withdrawn`,
  `public_needs_consent`, `refused` or `other`; null when it succeeded or predates codes) and
  `session_id` (the conversation it failed on, or the one it uploaded). A `project sync` result
  also has `uploaded`, `failed`, `skipped` and `pending` counts; a session left out or still in
  its first turn is not a failure. A refusal recorded while `push.auto` was off is left out once
  the folder is `on` again, since it no longer describes the folder.
- `sessions_here`: Claude Code and Codex conversations whose directory is this folder or below
  it. A conversation belongs to the deepest listed folder holding its directory, the way a nested
  binding owns its directory.
- `awaiting_upload`: how many of those are saved and not on the Hub at their saved head (status
  `not_uploaded` or `waiting`), counted over all of them, not only `recent`.
- `capture`: one item per runtime installed here, as `runtime`, `ready`, `fix` and `used_here`. `ready` is
  whether that runtime's new conversations here are saved by themselves: for Claude Code, its
  settings carry AgentGit's capture hooks; for Codex, its hooks feature is on and AgentGit's hooks
  are trusted, which shows as a hook seen for the folder's newest Codex conversation. `fix` is what to do when it isn't, else null. Both are null when the
  runtime couldn't be asked in time. Codex is asked from the folder, and its answer is kept for at
  most an hour, and only until Codex, its system or personal configuration, or a project
  configuration above the folder changes. `agit project list` without `--json` uses a kept answer
  and doesn't start Codex. `used_here` is whether any of the folder's conversations, not only
  those in `recent`, is one of that runtime's.
- `recent`: the newest of those conversations, newest first. Each has `title` (the runtime's own
  name for it, else its opening request; null when neither is known), `runtime`, `session_id`,
  `last_active` (when its transcript last changed), `status`, `repository` (where it is saved,
  or null), `saved_at` (its saved branch head's commit time, or null) and `url` (its Hub page,
  only when this checkout has seen it published).

`agit project sync` prints `{project, repository, result}`. `result` has `at`, the counts above,
and `sessions`: one row per candidate session, as `runtime`, `session_id` and `outcome`.
`outcome.status` is `pushed` (with `branch` and `head`), `failed` (with `error` and `reason`),
`pending` (claimed, but no turn of it has finished yet) or `skipped` (with `reason`: `dropped`,
`before_on`, `codex_hooks_not_seen` or `delegated`). Only a failed row fails the command, and the
first failure and the count are also said in plain words.

`status` is `not_saved` (no repository holds the conversation), `not_uploaded` (saved here, and
this checkout knows of no upload), `waiting` (the Hub has an older head than the saved one) or
`uploaded` (the folder's last upload published this head, or the head this checkout last saw on
the Hub is the saved one). Titles are read from bounded ends of each transcript, so listing costs
the same however long a conversation grows.
