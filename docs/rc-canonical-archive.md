# Canonical RC archives and live history

Status: implementation in progress. The legacy web checkpoint writer remains
active until the canonical capture and recovery path passes acceptance.

The device pending-capture journal and orphaned local-owner recovery worker are
implemented on this branch. They preserve completed Codex and Claude native
prefixes before completion delivery and retry standard settlement without
starting a model. Verified protected publication transfers notification ownership
to the existing durable privacy outbox. The project destination handshake,
external-watch capture, canonical web reads, and legacy migration remain required
before the complete design is deployed.

## Data and identity

Each admitted device project directory maps to one immutable private Hub repository.
Workspace membership controls access to that mapping; a second workspace binding
the same device directory does not create another conversation identity.

The native recording, standard Agit session, and live RC stream have distinct
coordinates. A persisted capture receipt binds them explicitly:

- Device identity and credential epoch.
- Runtime source identity and source generation, native session identity, and
  canonical project directory.
- Logical RC session identity and executor incarnation.
- Local repository identity, capture kind, and branch.
- Hub origin, immutable repository identity, projected Agit session identity,
  and remote branch.

Display names and directory aliases cannot replace these coordinates. A receipt
is an expectation, not continuing authority: admission, confinement, capture
claims, repository identity, and publication policy are checked before execution
and again before publishing a result.

Archive storage uses the existing standard layout: `session/meta.json`, `LOG`,
`VIEW`, and content-addressed `events/` objects. Raw native records are protected
before standard envelope construction. Display items, truncated tool previews,
and conversation-only history cannot reconstruct a resumable archive.

## Native history privacy projection

Reading a Codex or Claude native history page does not acquire the dictionary's
writer lock or register new values. The reader authenticates a bounded dictionary
snapshot and rechecks current key access and complete ciphertext before returning
the page. Concurrent dictionary changes require a bounded retry; unavailable
policy withholds content rather than repeatedly inspecting individual records.

Existing reversible mappings retain their standard Agit placeholders. A newly
discovered sensitive value receives a display-only "[redacted:secret]" marker.
These markers are not capture input, repository records, or a separate cache.
Standard settlement still registers durable reversible mappings before archive
publication. Native identities remain occurrence-scoped, and current explicit
protection rules override native identity evidence.

Compiled matchers reuse only identical ordered pattern prefixes after the current
policy has selected active values. Overlaps across matcher segments retain current
pattern indices and the ordering required by connected-region projection.

## Durable turn boundaries

The executor records a pending capture before acknowledging a completed turn.
The record contains routing expectations, the exact native completion identity,
and its closed transcript boundary; it contains no copied transcript or login
token. Replayed completion events select the same pending capture.

Capture and publication are separate states of the same task:

1. Pending native capture.
2. Standard session commit confirmed locally.
3. Publication intent durably transferred into the existing privacy outbox.
4. Destination commit confirmed by the remote receiver.

An archive task is retained until another durable owner has accepted its work.
Neither a subprocess exit nor enqueueing a notification is a remote receipt.
Later turns may be captured without waiting for an earlier network retry, but
their archived commits preserve every completed turn boundary.

The recovery worker starts with the daemon, independently of browsers and native
runtime ownership. It reads a completed prefix and runs standard settlement; it
does not resume, interrupt, or launch the model. A live supervisor retains its
writer slot. Recovery checks the current native claim before selecting an idle
or orphaned capture, and standard repository transactions fence concurrent
writers.

Retries retain their original source and destination. Missing credentials,
changed bindings, unreadable transcripts, and publication-policy failures remain
visible pending work. A retry cannot silently select another project or grant
itself broader runtime permissions.

## Publication binding

Device-local repositories are not silently renamed into Hub repositories. The
cloud project receipt confirms an explicit destination against the device owner,
project directory, immutable repository identity, and current Hub grant.
Existing capture or publication bindings are reconciled rather than overwritten.

Automatic publication uses the standard protected push pipeline and durable
publication receipts. The receiver maps the logical RC alias to the projected
standard Agit session. It must not report a private source commit as the public
destination commit.

## Web reads and live reconciliation

Opening a conversation starts an authenticated latest-page read from its standard
Agit session and a fresh device history request concurrently. Loading feedback is
immediate. The archive page carries its immutable commit and pagination cursor;
device history carries its native snapshot and replay boundary.

The frontend merges by confirmed source/event identity, not by message text,
arrival order, tool name, or command output. A delayed archive response cannot
replace newer device content. Tool calls and results retain native correlation
identities. Pagination stays on one immutable archive commit until an explicit
latest-page refresh adopts a newer commit.

After fresh history finishes, replay starts at the corresponding confirmed
boundary. Events arriving during synchronization remain buffered and are applied
once. A replay gap triggers bounded history reconciliation. Device failure keeps
the archive visible and displays a truthful offline or synchronization state.

The server reads this same standard session for repository history and workspace
history. There is no separately written web-history file. Any client sliding
window is an eviction cache of authenticated standard pages, keyed by account,
repository, session, and immutable commit; it cannot authorize reads or supersede
a newer session generation. Persistent client caching remains optional until
its revocation, refresh, and live reconciliation contract is tested.

## Migration and rollout

1. Add durable executor capture intents and recovery without changing native
   control ownership. Preserve the existing privacy publication outbox.
2. Bind cloud project receipts to standard capture destinations. Validate
   owner, invited-member, and multiple-workspace access boundaries.
3. Reconcile each legacy checkpoint branch against the complete protected native
   recording. Preserve the branch identity and prior Git history. Stop if the
   native recording is unavailable or conflicts with that history; do not invent
   native records from display projections.
4. Switch the workspace latest-page endpoint to standard Agit session reads and
   reconcile archive and live identities. Keep old checkpoints readable during
   migration, without writing new independent checkpoints after activation.
5. Verify every migrated branch with both standard Agit readers and web readers,
   including tool calls, results, compactions, interrupted turns, and unknown
   native record types.
6. Roll out compatible CLI and backend candidates through staging, then deploy
   production and verify the installed daemon build rather than only the CLI
   version on disk.

## Acceptance evidence

Measure navigation, archive request, controller admission, route connection,
catalog resolution, snapshot capture, protection, transfer, parsing, rendering,
and the first visible latest reply on the same trace. Report cold opens and
repeat refreshes separately without removing either from acceptance.

Use real multi-turn conversations with tool output and realistic privacy
dictionaries. Verify the full returned item digest and exact latest reply so a
faster incomplete result cannot pass. Exercise desktop and mobile viewports,
multiple workspaces, active owners, offline devices, task retry, daemon restart,
and revoked memberships.

User actions must receive visible feedback within one second. Workspace opening
must reach the latest running conversation within three seconds. A local
projection benchmark, an archived reply, or an HTTP success does not establish
the browser latency requirement. Retain failed measurements alongside accepted
ones and correlate device diagnostics with staging CloudWatch traces.
