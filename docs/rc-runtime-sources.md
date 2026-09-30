# Local Codex runtime sources

A Codex runtime home contains native history and configuration. It does not need
to be named `.codex`. Register each home used by a launcher:

```sh
agit rc sources add ~/.codex-control2 --name control2 --executable "$(command -v codex)"
agit rc sources list
```

Registration prints a `source_id`. It records local coordinates without copying
credentials, starting a task, or changing native configuration. Registration also
works without a Codex executable; history inspection remains available. Pass
`--executable /absolute/path/to/codex` when enabling control for that source.

An existing nonstandard server can be associated with the source using
`--socket /absolute/path/to/control.sock`. Omit this option to use the native
standard socket under the selected home.

Inspect conversations under a project, including nested working directories:

```sh
agit rc sources sessions SOURCE_ID --project /absolute/project/path --limit 100
agit rc sources sessions SOURCE_ID --project /absolute/project/path --after CURSOR
agit rc sources settings SOURCE_ID THREAD_ID --project /absolute/project/path
```

The list returns `rows` and `next_cursor`. Continue while a cursor is present,
including when a page has no matching rows. Native conversations outside the
selected project and archived conversations are excluded from discovery. An
unavailable or incompatible native index uses bounded transcript-header discovery;
inaccessible or incomplete history reports unreliable coverage.

Observed settings do not acquire a writer or change permissions. A setting that
cannot be determined remains unknown. A copied runtime home has a distinct
source identity even when it contains identical native thread IDs.

Print source-correct arguments for joining a shared server:

```sh
agit rc sources connect SOURCE_ID THREAD_ID
```

The result includes `environment.CODEX_HOME` and an `argv` array. The arguments
select the registered executable, explicit native socket and native thread ID.
The server must already be running. Merely running `codex` does not select that
conversation; joining it requires `resume THREAD_ID`.

Codex 0.153.0 can also discover a running default socket when launched without
configuration overrides: `codex resume THREAD_ID` joins the existing conversation,
and plain `codex` creates a conversation on that service. Configuration overrides
can select an embedded runtime instead. Use the explicit arguments returned by
`sources connect` for deterministic attachment, especially with a custom socket.
The native CLI does not guarantee starting a persistent service when none exists.

Stop discovering a source without deleting its files or stopping its tasks:

```sh
agit rc sources remove SOURCE_ID
```

Re-registering the same directory enables the source again. Symlink aliases
converge on its canonical identity. Replacing the directory creates a new identity;
existing references cannot silently attach to the replacement. Source settings
and registration state are stored privately under `AGIT_HOME/runtime-sources`.

## Executor catalog

Native parent relationships are represented by source-qualified `parent_session_ref`
values. An exact native internal-work marker sets `technical`; broad role labels
do not hide user conversations. Web and Desktop keep internal conversations in the
authorized catalog and reveal them with "Show internal conversations". A parent
name is shown only when that parent is in the authorized catalog.

Executors advertising `source-catalog-v1` maintain a private discovery index under
`AGIT_HOME/runtime-catalog`. A background worker reads enrolled sources in bounded
pages, independently of browser connections. It enrolls the configured default
Codex home when that directory exists. Automatic enrollment does not re-enable a
removed source or accept a replacement directory under the old identity.

The worker also checks native processes every 15 seconds on Linux and macOS.
Only processes owned by the executor's OS account are eligible. Inspection keeps
the executable path and `CODEX_HOME` (or `HOME/.codex`), rechecks process identity,
and discards other environment values and arguments. A discovered home remains
registered after its process exits while the directory exists. Native startup hooks also enroll their home,
including homes outside the profile and project directories.

Before catalog reconciliation, the worker retires an automatically enrolled home
under the operating system's temporary directory if filesystem metadata reports
that the home no longer exists. Its registration remains visible in `sources list`
with `enabled: false` and `disabled_reason: missing_temporary_home`; its source
generation advances so stale controls cannot attach to reused coordinates. No
native history is deleted and no running task is stopped. Registrations without
enrollment provenance are eligible only when their unnamed, unpinned coordinates
identify a recognizable `.tmp` allocation under a system temporary root.

Explicit registrations, persistent directories and permission failures do not
expire automatically. Unavailable persistent sources retain their registrations
and retry in the background. Healthy sources remain usable independently. A
manually removed or retired source stays disabled during automatic discovery;
explicit re-registration is required before it can become active again.

Profile discovery checks the OS user's home and bound project directories, plus
their immediate children. A candidate must contain `sessions` and a native
configuration or state database marker. This scan is bounded and does not search
the entire filesystem. Inactive homes elsewhere require `sources add` or prior
process/hook enrollment. Removing a source suppresses automatic re-enrollment;
an explicit executable choice is never replaced by a process observation.

The catalog and `sources list` expose a `discovery` report with independent
`processes` and `profiles` coverage and a check timestamp. OS restrictions or scan
limits produce partial coverage; platforms without process inspection report
unsupported coverage. Controllers retain known conversations while indicating
incomplete discovery. Manual registration remains available in either case.

`session.catalog.list` accepts an optional bound `project_id`, an opaque `cursor`,
and a `limit` from 1 to 500. The executor obtains directory paths from its current
workspace bindings; clients cannot supply a home or executable. Results contain:

- `rows`: source-qualified conversation references, native IDs, project paths,
  native titles/previews and observed writer ownership.
- `next_cursor`: continue until absent, including after a page whose rows were
  filtered by current access policy.
- `revision`: the local discovery revision, independent of conversation output.
- `coverage`: source generation and `scanning`, `ready`, or `unavailable` status.

Pagination orders opaque conversation references. A concurrent discovery update
can change membership; restart pagination when reconciling a changed revision.
Nested project directories are included. Archived entries disappear after a
complete sweep. An interrupted sweep retains unseen records, and a temporarily
unreadable index retains previously observed metadata with unavailable coverage.
Cached writer ownership is an observation, not proof that control can be acquired.

`session.catalog.settings` reads the observed model and permission mode without
resuming or taking ownership. Local owner callers provide `session_id` (the row's
`session_ref`), `source_id`, `source_generation`, and `native_session_id`. Cloud
callers provide the conversation reference; admission supplies the source and
project coordinates from the executor catalog. A source-qualified reference never
inherits permissions granted to a bare native ID from another home. Source removal,
directory replacement, and workspace unbinding invalidate subsequent reads.

If the native index is missing or its schema is unsupported, discovery pages
through the source's `sessions` directories and reads bounded `session_meta`
headers. Its directory cursor survives worker restarts. Incomplete headers,
unreadable or changing directories, and exhausted scan budgets retain existing
catalog rows and report incomplete coverage. A complete sweep is required before
removing unseen rows. Explicit thread lookup can also recover a native rollout by
its filename and verified header without a SQLite index.

Devices advertising `source-catalog-delta-v1` return a durable `changes_cursor`
with each snapshot page. Retain the first page's change cursor while consuming
`next_cursor`, then pass it as `after_revision` to `session.catalog.list`.
Incremental responses contain upsert `rows`, `removed` conversation references,
and a replacement `changes_cursor`. Continue immediately while `has_more` is true,
even if a page has no visible changes. These requests cannot combine
`after_revision` with a snapshot cursor or an exact conversation lookup.

The device persists a bounded change journal across controller disconnects and
daemon restarts. A journal gap, cache replacement, source identity change, or
project scope change returns `reset: true`; discard the cached snapshot and fetch
it again before following changes. A quiet response advances coverage without
returning unchanged conversations. Current controllers poll this incremental
endpoint; it is not a server-push subscription. Cloud admission filters removals
by project and strips their device paths before returning them to the browser.

Discovery and observed settings do not imply shared control. Controllers must
negotiate shared-control capabilities before offering their controls.

Custom prompt catalogs and prompt expansion use the controlled source's runtime
home. Identically named prompts in different homes remain independent; a missing
prompt in one source does not fall back to the daemon's default home.

## Shared session settings

Attaching to an existing shared session preserves its native defaults. Resume
requests exclude full turn contents and request only the latest turn metadata to
recover whether work is running. Conversation history is read separately.

After attachment, native `thread/settings/updated` events refresh the model,
reasoning effort, and supported permission mode. The executor records observed
dangerous permissions durably before handling further commands. An unsupported
native policy remains unknown while output stays subscribed. A changed working
directory requires a new validated attachment; it does not authorize RC to stop
the shared native task.

`session.model` and `session.commands` are read-only. Workspace viewers can use
them even while the write-control gate is closed. A model or reasoning change
through `session.setModel` writes `thread/settings/update` on the shared server,
so other subscribers see the same defaults. During an active turn these defaults
apply to subsequent turns; the existing turn continues under its original
settings. Private runtimes retain their driver-specific update behavior.

`session.setPermissionMode` updates shared native defaults directly. It does not
wait for another RC-submitted turn: subsequent native CLI turns and queued inputs
use those defaults. An active turn retains the policy captured when it started.
The response and `session.permissionMode` event carry `native_default: true`;
`applied: next_turn` describes the active turn boundary, not an unsent RC override.
Permission selectors show the shared defaults, and a null event mode means the
policy is unconfirmed. Sequenced snapshots and replay must preserve that unknown
observation until a newer authoritative observation supersedes it.

An unconfirmed shared update keeps the task and output subscription alive. RC
retains conservative authorization and does not fabricate a Plan restart policy.
Buffered notifications cannot clear this uncertainty. A confirmed explicit policy
update or a fresh validated attachment can establish known defaults again.

## Shared approvals

The native service sends pending approval requests to subscribers, including
clients attaching while a request is pending. Its `serverRequest/resolved`
notification removes the matching request from RC and publishes
`approval.resolved` with the logical `session_id` and `approval_id`. Subscribers
remove only that card; other pending approvals remain actionable. The hub retains
this event for replay so reconnecting viewers do not restore a resolved card.
Resolution means the request is closed, without identifying which client's
decision won. Request IDs are matched within the bound native thread.

A shared `approval.decide` waits for native request resolution. Its successful
response includes `resolved: true` and `decision_confirmed: false`; a competing
subscriber may have supplied the winning decision. An unconfirmed response keeps
the request pending and reports `SessionBusy`. Retrying observes resolution
without resending the answer. Stream notifications continue to close the card
when confirmation arrives, and this wait does not stop the shared task.

## Shared message delivery

After attaching with `session.resume`, address `session.enqueue` with the returned
logical `session_id`, its `workspace_id`, a fresh UUID `client_msg_id`, and the
message. The executor selects the native source and thread from the live binding;
caller-supplied native coordinates cannot redirect the message. Read-only grants
do not permit queue submission or model changes.

Keep the same client message ID when retrying the same operation. The receipt is
bound to the authenticated account, workspace, source and native thread. Changing
the text under that ID is rejected. A source generation refresh does not discard
the receipt or authorize replay. The response includes the native correlation ID
for matching the transcript echo to the submitted message.

`queued` means that the native service acknowledged the queue item. The service
can immediately execute an idle thread, or start the item after current work
finishes. Continue reading the conversation stream for output and completion;
queue acknowledgement is not task completion.

An uncertain write retains an `unknown` receipt. Retrying first looks for the
native queue item, then scans the source-confined native transcript incrementally
for the exact correlation ID. `delivered` confirms that native history contains
the input; it does not assert that model execution succeeded. Large records and
unfinished trailing lines do not require loading the entire conversation.
Absence from a bounded scan never authorizes resubmission. An explicit native
refusal is recorded as `rejected`.

Shared attachment preserves unsupported native policies as unknown when the
caller is authorized for unknown permissions. It does not substitute a local
permission mode or prevent model reads. A confirmed explicit permission update
restores a known mode. Native policy drift during attachment cannot grant an
operator the authority reserved for the owner.

Shared interruption reads bounded native current-turn metadata before addressing
an exact turn. RC reports success only after the native server acknowledges the
interrupt, or after a native read confirms no active turn. Refusal and uncertain
responses remain errors; they do not clear pending approval cards or terminate
the shared service.

Source-qualified catalog references support read-only `session.watch` and
`session.history` before managed attachment. Watches retain the native ID and
source identity separately, read observed model and permission settings from the
enrolled home, and apply repository protection through the qualified link. Each
poll revalidates the source, native header and current workspace binding; source
removal or project unbinding ends the read stream without affecting native work.
History snapshot cursors remain confined to their source and generation.

Source-qualified `session.goal.read` uses the same enrolled source for catalog and
managed references. It connects to the existing native service, requests the
thread goal without loading or resuming the conversation, and rechecks source
identity and workspace confinement before returning protected output. An offline
service reports unavailability; read-only inspection never starts its replacement.
Native goal updates are visible on the next read without acquiring control.

A live watch publishes an explicit null permission when an observed native
policy cannot be mapped to an Agit mode. Consumers must clear the previous mode
instead of leaving a stale known value beside the newly observed model.

Executors advertising `source-catalog-resolve-v1` accept `resolve_session` on
`session.catalog.list` with an opaque catalog
reference and no cursor. It returns at most that source-qualified row, after
checking its current native index, transcript header and bound directory. This
allows a controller to recover an evicted discovery entry without loading every
catalog page or accepting caller-supplied runtime paths. Empty scoped results do
not authorize an operation.

Cloud session-controller admission resolves source-qualified references against
the current native source and workspace before accepting its grant. The admitted
controller retains its resource independently of discovery page eviction. Cloned
execution authority retains that pin until admitted work finishes; disconnected
clients cannot keep unused catalog pins indefinitely. A source removal or
registration-generation change revokes the pin when resource state refreshes,
while another source with the same native thread ID retains its own authority.

Catalog pages expose `complete` independently of their visible rows and cursors.
It is false while any enrolled source is still scanning or unavailable. Scoped
readers retain this aggregate signal when unrelated source health identifiers are
removed, so an empty filtered page cannot claim discovery is complete.

An uncertain shared turn-start receipt keeps its subscriber alive and is marked
non-retryable with outcome unknown. Native completion can be observed even if
the matching acceptance notification was lost; that completion does not turn an
unconfirmed prompt receipt into an acknowledged submission. Private executor
fail-closed handling remains separate.

If a shared model update loses its acknowledgement, model and effort become
unknown while output and queued work continue. Buffered settings notifications
cannot confirm that write. An explicit confirmed model update or a fresh native
attachment can restore known settings. Explicit native refusals preserve the
previous observed settings.

Executor `turn.start` and `turn.steer` requests with a client message ID reserve a
private durable receipt before dispatch. A daemon restart or memory-cache eviction
does not authorize the same operation again. Confirmed results replay with the
new request ID; a claim without a confirmed result remains unknown. A proven
not-sent refusal releases its claim so the same operation can be retried. Receipt
files contain the request digest and result, not the prompt. Cloud identities are
scoped to the authenticated principal. Storage work runs outside the routing loop
so pending writes do not stall other clients' reads or model output.
