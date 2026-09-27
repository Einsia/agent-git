# RFC: Discover and control conversations across native runtime owners

Status: Implemented in CLI 0.2.8 with deployed backend support. Final production
browser interaction acceptance remains open; this RFC is not fully accepted yet.

## Delivery status

[CLI 0.2.8](https://github.com/Einsia/agent-git/releases/tag/agit-v0.2.8)
ships runtime-source registration, source-qualified discovery and control, shared
Codex attachment, and fallback-only intent expiry. The production backend source
is `faa838190bd8a732801f74849a0a5d834646b117`; deployment uses the same verified
image in all three regions.

Actual Codex 0.155.1 acceptance covers both launch orders on the standard socket,
co-control, queue execution, reconnect/replay, and shared streams beyond the
fallback deadline. The server and active native task survive Agit exit. A separate
real fallback run verifies background release without stopping execution or losing
subsequent output. The default remains fifteen minutes.

Additional actual-native acceptance verifies concurrent startup, overlapping
execution of distinct conversations on one server, a registered nonstandard
endpoint, and explicit handshake rejection without embedded fallback. A config
override disables implicit reuse in the tested launcher; selecting the endpoint
explicitly preserves the model override and attaches to the shared service.

Read-only production acceptance on the affected device verifies its existing
custom-home conversations, history, models, and permissions without acquiring
control. The production in-app browser displays a disposable conversation's
complete replies, model, and permission mode after refresh. Actual browser sending
revealed that CLI 0.2.8 rejects `turn.start` and `turn.steer` for attached native
sources while accepting `session.enqueue`. CLI 0.2.9 prepares the admission
correction; its release and production-page acceptance remain required. Browser
queue and device-removal acceptance remain open. Protocol-level acceptance does
not substitute for those interactions.

Switching the disposable executor from a musl build to the GNU validation build
also exposed a source identity compatibility error: creation time metadata became
available while the directory device and inode remained unchanged. The prepared
correction compares creation times when both builds report them, while preserving
device/inode and principal checks. Source identity and generation must survive the
build transition; actual execution and the formal release remain acceptance gates.

Cold Cloud resume also requires the executor to retain the enrolled source in
its saved-identity resolution receipt. Omitting it turns a source-qualified
conversation into a raw native ID when the shared controller is selected. The
prepared correction keeps that source and generation, with current read authority
still checked; production cold resume remains required.

The partial-discovery notice follow-up in backend MR !593 is verified in every
production region at GitOps commit `b81efa6b20b443f73a6ee4855437df070ae4d595`.
Issue #63 is closed; issues #64 and #65 remain open until
the remaining production acceptance and evidence updates are complete.

## Decision

Make the authorized project catalog independent of who starts a conversation.
Agit must discover native conversations across enrolled runtime environments,
observe them without acquiring their writer, and choose the strongest available
control channel whenever an authorized operator opens or sends to a conversation.

For Codex, the primary architecture is a shared app-server per enrolled runtime
source. Agit and native clients attach to that server regardless of who launched
it. A loaded conversation has one executor/writer and multiple authorized clients;
joining it does not require the other client to surrender a native writer lock.
Agit starts a discoverable server only when no compatible endpoint exists.

Opening an inactive conversation for control resumes it through the selected
shared server. A writer outside that server requires its own verified endpoint or
inbox; the shared server cannot steal an opaque embedded runtime's writer.
Writer contention is an ordinary mode transition, not a failed conversation.

The runtime writer, the local OS principal, the workspace role, and the browser
controller are different identities. "Any owner" means any runtime writer within
the device's authorized execution scope. Workspace membership does not grant
access to another OS account or to unshared project roots.

## Baseline audit and design evidence

Audited CLI source: `88e13b675d07e2c3cd6b71d43ed94965eacc4483`.
Audited web source: `1ebeb7bce77fed3b5821b3ef2c14274cbf062be4`.

The affected device has ordinary terminal conversations in a custom Codex home,
with their native index recording the same project cwd as RC. The daemon has no
`CODEX_HOME` override. The device also runs project-child conversations with
individual Codex homes. This is not a requirement to launch conversations through
Agit: existing discovery already supports external sessions in the default store.

| Path | Baseline limitation | Required change |
| --- | --- | --- |
| `src/adapter/codex.rs`, `codex_index.rs` | Home is resolved from daemon-wide environment; project query uses exact cwd equality | Explicit runtime context and canonical project containment |
| `src/rc/daemon/sessions.rs` | Scans one environment per adapter, skips unavailable executables, truncates project results, silently skips source errors | Durable multi-source catalog, pagination, partial-source diagnostics |
| `src/rc/daemon/mod.rs`, `codex_ownership.rs` | Writer observation uses the default home; uncertainty becomes an activity heuristic | Source-aware ownership evidence with explicit uncertainty |
| `src/rc/native_inbox.rs` and inbox preparation | Executable comes from daemon PATH; child inherits daemon environment | Deliver using the selected native environment and native identity |
| `src/rc/native_settings.rs`, `watch_rpc.rs` | Snapshot and stream use native IDs without a durable source namespace | Source-qualified settings, replay, and stream identity |
| Backend `web_control/scope/resources.rs` | Native aliases are keyed by bare native ID | Source-qualified admission and shared controller ownership |
| `website/src/routes/WorkspaceLive.tsx` | Catalog reload follows connection/session-status events; managed and local sessions have separate presentation | One project list and independent catalog-change subscription |

The shared-server design is grounded in the public Codex `rust-v0.155.1` source,
commit `be2951ea34f0d295ed0becf97079f92fa5f6950e`. This is an audited revision,
not a claim that every deployed Codex version supports the same behavior.

- [TUI discovery and launch selection](https://github.com/openai/codex/blob/be2951ea34f0d295ed0becf97079f92fa5f6950e/codex-rs/tui/src/startup_orchestration.rs#L153)
  probes the default daemon socket when launch options permit reuse.
- [Implicit versus explicit connection failure](https://github.com/openai/codex/blob/be2951ea34f0d295ed0becf97079f92fa5f6950e/codex-rs/tui/src/lib.rs#L496)
  allows implicit discovery to fall back to an embedded runtime; an explicit endpoint
  fails without starting an embedded replacement.
- [Running-thread resume](https://github.com/openai/codex/blob/be2951ea34f0d295ed0becf97079f92fa5f6950e/codex-rs/app-server/src/request_processors/thread_processor.rs#L4259)
  reuses the existing execution and returns its live settings.
- [Thread subscribers](https://github.com/openai/codex/blob/be2951ea34f0d295ed0becf97079f92fa5f6950e/codex-rs/app-server/src/thread_state.rs#L320)
  are a set of connection IDs, not one exclusive desktop controller.
- [Server transport selection](https://github.com/openai/codex/blob/be2951ea34f0d295ed0becf97079f92fa5f6950e/codex-rs/app-server/src/lib.rs#L754)
  distinguishes single-client stdio from a Unix socket acceptor.

The installed desktop's SSH glue also starts `app-server --listen unix://` and
connects with `app-server proxy`. The inspected desktop is 26.917.71314 (10954),
with bundled CLI 0.155.0-alpha.16.4; it is not the exact public revision above.
Source inspection establishes feasibility. The acceptance gates below are required
before advertising support for a deployed client/server version pair.

## Bidirectional shared-server contract

### Conditions and user-visible guarantee

Within the same device, authorized OS principal, enrolled canonical `CODEX_HOME`,
and compatible server endpoint, later clients can attach to the same server and
conversation regardless of its launcher. Server launcher identity is diagnostic
metadata, not a condition for admitting another client.

| Launch order | Required behavior |
| --- | --- |
| Agit starts the server, then native CLI joins | Agit exposes the standard source socket; the native CLI connects and resumes the same thread, while RC remains subscribed and usable. |
| Native desktop, native daemon, or another tool starts the server, then Agit joins | Agit discovers and validates the existing endpoint, joins it, and resumes the same thread without launching a competing server or replacing its settings. |
| Agit and another launcher race to start | The native startup lock/socket bind determines the listener. The loser re-probes and joins the winner; it never removes a live socket. |
| Existing server uses an explicit nonstandard endpoint | Register and validate that endpoint. Agit and the native CLI both use its explicit address. |
| Existing runtime is embedded or stdio-only | There is no independently attachable listener. Preserve discovery/observation and use only verified cooperative operations; explain that shared-server attachment is unavailable. |

The standard Unix rendezvous is:

```text
$CODEX_HOME/app-server-control/app-server-control.sock
```

With an unset `CODEX_HOME`, Codex uses its default home. A custom home must be
selected by both clients; matching project directories alone does not select the
same runtime source. Custom source enrollment in #64 stores the endpoint alongside
the source and exposes a source-correct local connection command.

The plain `codex` command can automatically join an existing default socket in
compatible builds with eligible launch options. It remains a new-conversation
entry point: it does not guess which existing thread the user intends to control.
Use the native picker or an explicit native ID to join the intended conversation:

```sh
CODEX_HOME=/absolute/runtime-home codex resume THREAD_ID
```

The deterministic shared-server path specifies both endpoint and thread:

```sh
CODEX_HOME=/absolute/runtime-home codex --remote unix:// resume THREAD_ID
# For a registered nonstandard socket:
CODEX_HOME=/absolute/runtime-home codex --remote unix:///absolute/server.sock resume THREAD_ID
```

These are native CLI commands, not a requirement to launch the TUI through Agit.
Agit must provide the correct command for the selected source and thread. Do not
use `--last` or infer a conversation from cwd when several conversations can match.

Automatic discovery is opportunistic. In the audited revision, arbitrary `-c`
overrides (including feature flags), non-default loader overrides, strict config,
worktree/OSS/workload-identity modes, hook-trust bypass, or executor selection can
prevent implicit reuse. Connection failure can also fall back to embedded mode.
The support contract must therefore test the user's real launcher, not merely the
binary version. Existing `codex-control2`-style wrappers must be checked for both
home selection and incompatible launch overrides. An explicit endpoint avoids
silent embedded fallback, but unsupported launch options must still be rejected
or configured on the server; never silently discard a requested option.

Starting plain `codex` when no shared listener exists can create an embedded
runtime. A subsequent Agit process cannot retroactively turn that live execution
into a shared server. Guaranteeing arbitrary cold-start ordering requires prior
shared-daemon setup or upstream support for a discoverable listener; no claim of
unconditional compatibility with every process named `app-server` is made.

### Endpoint discovery, launch, and attachment

1. Resolve the enrolled source and exact local principal before probing. Discover
   registered endpoints and the source's standard socket; validate local ownership,
   source coordinates, protocol handshake, and required operation support. A PID
   match or successful socket connect alone is insufficient. Keep remote endpoint
   credentials local and preserve workspace scope filtering on all returned data.
2. Reuse a compatible listener even if Agit did not start it. If a listener exists
   but is unreachable, unauthorized, or incompatible, return that specific state;
   do not silently start a different executor in the same source.
3. If absent, use supported daemon management where its behavior is compatible,
   or a supervised `codex app-server --listen unix://` with the resolved source.
   Keep its lifetime independent of browser sockets and individual conversations.
   Prefer the native startup reservation. On an address-in-use race, re-probe the
   winner. Do not unlink a live socket, replace credentials, or restart another
   client's server. Agit shutdown disconnects its clients without stopping a shared
   server or a task belonging to another client.
4. Connect over the native WebSocket protocol on the Unix socket (directly or using
   `app-server proxy` as a byte transport). Perform normal initialization, then
   `thread/resume` for the selected native ID. A proxy stream carries WebSocket
   framing; it is not the existing newline-JSON stdio transport.
5. Join loaded threads without launch-setting overrides, hydrate the native live
   model/permission snapshot, subscribe to events, and reconcile pending approvals
   and message receipts. Cold resume of a dormant thread occurs on this same
   server. Writer conflict with a different executor triggers endpoint negotiation,
   not creation of another thread or a forced ownership takeover.
6. On disconnect, reconnect to the same source/endpoint and revalidate its instance
   before resuming. Persisted identity does not prove that a replacement server has
   the same active turn. Reconcile ambiguous commands rather than replaying them.

### Co-control and background behavior

Native clients already share one execution actor. Agit must not add a global
exclusive controller lease that makes other native subscribers wait for expiry.
An Agit controller lease, when needed, tracks RC interaction and authorization; it
is separate from the native writer and from the set of viewing subscribers.

All clients observe the same live turn and settings. Use native turn/approval IDs
and operation-specific capabilities for mutations. Input from another client can
start or steer the shared turn; the native server arbitrates it. Serialize Agit's
own conflicting requests, but do not claim that an Agit lock serializes native
clients. Duplicate approval responses must resolve cleanly through the native
protocol, and one client's settings change must refresh the other clients.

For #63, automatic release applies only to fallback control when shared-server
attachment is unavailable. Successfully starting a persistent server at the standard
source socket or joining a compatible existing server disables inactivity release.
Keep shared control and output subscriptions regardless of how long the user is
silent; other clients can co-control without requiring RC to release anything.
Explicit release, authorization revocation, and transport recovery remain separate.

Only an acquired fallback interactive controller starts the inactivity timer. Use
15 minutes without user instructions by default; allow five minutes only after
that fallback path passes deployed read-continuity acceptance. Observation-only or
inbox-only modes that hold no interactive control have nothing to release. Release
only RC's fallback controller through a verified non-disruptive background path;
never stop the task, steal an external writer, or claim unsupported handoff succeeds.

Bind the timer to the current control mode and generation. Successful promotion
from fallback to shared attachment cancels it atomically; a stale callback cannot
release shared control. Temporary loss of a shared connection is reconnecting,
not fallback acquisition, and must not arm a timer. Arm a new timer only after a
real transition acquires fallback control. Its initial baseline is fallback
acquisition (or a later user instruction), not a historical shared-mode deadline;
passive reconnect, output, and retries do not renew it. Preserve read subscriptions
and the existing task throughout mode changes and any fallback release.

## Coverage contract

Within enrolled local runtime sources and authorized bound project roots, every
non-deleted user conversation remains discoverable regardless of activity, writer,
launch mechanism, or age. Idle, running, awaiting approval, interrupted, failed,
and disconnected are states on the same row, not reasons to omit it.

Initial enrollment includes the daemon environment, standard runtime homes,
previously registered homes, same-principal running processes, and bounded scans
for recognized runtime stores under authorized projects and standard user profile
locations. Process inspection retains only allowlisted coordinates, never an
entire environment or command line. It deduplicates helper children and records
new sources durably, so sessions remain visible after their process exits.

Setup and native launch hooks register future environments where supported;
registration is also available through an explicit local source command. Existing
unmanaged launchers remain supported through process discovery. On platforms that
cannot inspect another process environment, the local registration adapter supplies
that information and reports coverage gaps.

An arbitrary inactive store at an unknown path cannot be discovered from its
project cwd alone. The UI must expose incomplete source coverage and permit local
enrollment, rather than claiming a complete inventory. An inaccessible OS account
requires that account's local agent and explicit delegation. It is not solved by
making the daemon root or merging credential directories.

## Runtime sources and identity

Introduce immutable request-local `RuntimeContext` values backed by a local source
registry. Remove process-global environment mutation from discovery and control.

A source records an opaque `source_id`, runtime type, OS principal, canonical
storage root and filesystem identity, executable identity/version, approved launch
profile, optional authenticated endpoint reference, discovery provenance, health,
and generation. Credential references stay local. Home paths, tokens, and full
configuration are not sent to the cloud catalog.

A conversation uses this canonical key:

```text
(device_id, os_principal_id, runtime, source_id, native_session_id)
```

Agit assigns an opaque stable `session_ref` for wire use. A workspace binding
relates that resource to its authorized project; it does not rename or re-own the
native conversation. Symlink aliases of one store converge locally. A copied store
with the same native UUID is a different source until an explicit verified
migration establishes otherwise. Two workspaces may have authorized views of one
conversation, but writer reservation and shared control use the canonical resource.

Every adapter operation takes the resolved context: enumerate, locate, history,
writer probe, settings, enqueue, attach, resume, approvals, and stop. The selected
context supplies `CODEX_HOME`, executable, provider/auth configuration references,
and necessary runtime options. A browser cannot supply an arbitrary executable,
home, or OS principal. Agit does not copy authentication files, rewrite provider
IDs, or borrow daemon credentials to make a resume succeed.

## Continuous project catalog

A local catalog worker reconciles all enrolled sources independently of browser
connections. SQLite native indexes are read in read-only mode with bounded work;
header-based discovery handles incompatible or absent indexes without loading
complete transcripts. Native index/WAL and directory events invalidate cached
results. Debounced polling reconciles missed events, including on network disks.

Canonical containment includes the project root and descendants, with path-component
boundaries and device/OS path semantics. `/project-other` is not under `/project`.
Resolve symlinks before admission. Nested bindings use the most specific authorized
binding. A session moving outside its authorized roots loses control authorization;
an old catalog row never proves current authority.

Use an Agit-owned indexed catalog for prefix queries and stable pagination, rather
than repeatedly scanning every transcript or silently dropping older entries.
Catalog rows include source health and freshness. Errors in one source do not erase
other rows or turn the complete list into an empty success.

Separate discovery from executability: a readable transcript stays visible when
its executable or credentials are unavailable. Explain which control capability is
missing. Deleted native records remain deleted; technical child conversations are
associated with their parent and available in a scoped expanded view. Broad labels
such as "subagent" must not hide real user conversations.

Proposed wire contract:

```text
session.catalog.list(project, cursor, filters) -> rows, next_cursor, revision, coverage
session.catalog.subscribe(after_revision) -> upsert/remove/source_health deltas
session.attach(session_ref, desired=control|observe, client_operation_id)
  -> session, ownership, capabilities, settings, stream, control_generation
```

The cloud projects executor-established coordinates into workspace scope and
subscribes once per shared resource. A durable revision cursor supports reconnect;
a cursor gap requires a fresh snapshot. No synthetic browser session or copied
device-owner credential is used.

## Ownership and control negotiation

Keep separate observations for native ownership, execution state, transport health,
and available operations. A transcript with no recent writes can still have an
active writer. A running process can have an idle conversation. Neither implies
that Agit may take its writer.

Native ownership is `free`, `external`, `agit`, or `unknown`, with evidence and a
source generation. On attach or mutation, revalidate authorization and coordinates,
reserve the canonical resource in the broker, then negotiate with the runtime.

| Native situation | Result for an authorized control request |
| --- | --- |
| Thread is loaded on a compatible shared server, regardless of launcher | Join that server and existing thread; preserve all other clients |
| No writer | Find or start the source shared server, then resume there; report full control only after native acknowledgment |
| External owner has an authenticated endpoint | Attach as another client to that same owner; expose only verified operations |
| External owner supports an inbox only | Follow history and settings; send through the inbox |
| External owner has no usable command channel | Keep history/settings visible; explain unavailable operations |
| Ownership uncertain | Probe/attach with a deadline; attempt resume only through a runtime contract that atomically enforces exclusive writing |
| Device/source unavailable | Preserve the row and last verified information; reconnect and renegotiate |

The native writer lock is the final authority. A free probe is not an ownership
claim. If an external process wins the race with `thread/resume`, transition to
external control instead of repeatedly launching a second writer. Never unlink a
native lock or kill its process to make this transition succeed.

Opening the catalog does not resume every dormant conversation. Opening a
conversation with control intent, or submitting input, triggers negotiation.
View-only workspace roles observe without acquiring a writer. A currently
attached control-intent client can automatically upgrade to full control when the
external writer exits, after fresh native acquisition. Ownership loss downgrades
capabilities immediately without replacing the row or discarding history.

## Cooperative control and capability model

For Codex, prefer the validated shared app-server endpoint before considering an
Agit-private harness or a native inbox.
Verify it belongs to the selected source, principal, native session, and owner
instance before sending anything. Connecting to a newly started app-server is not
attaching to an existing terminal writer.

Return capabilities per operation, including a reason, transport, and generation:
read history, read settings, enqueue, start, steer, interrupt, answer approval,
change model/effort/permissions, and release control. Observed settings are distinct
from editable settings. Model and permission visibility never depends on acquiring
the writer.

A native endpoint may allow interruption but not answering an approval owned by
another client. Probe and test each operation, including turn-ID and approval-ID
binding. Permission escalation still requires the caller's workspace authority and
the device's delegated ceiling. The broker serializes conflicting control requests;
stale generations cannot cancel or reconfigure a newer turn.

For the deployed Codex queue adapter, the initial guaranteed external-control set
is observation, observed settings, and queue submission when the capability probe
passes in that exact environment. Do not infer steering or approval support from
the presence of `codex queue`. Stronger operations are enabled after endpoint
acceptance. Other runtimes use the same interface with their own capabilities.

If a runtime exposes no cooperative control API, full control of its active writer
requires upstream support or integration at launch. The product cannot promise
arbitrary live interruption of an opaque process. It can promise persistent
visibility, every supported control operation, and automatic acquisition when the
writer becomes available.

## Durable message and handoff semantics

Persist a command ledger keyed by canonical resource, caller, and
`client_operation_id`. Distinguish `accepted`, `queued`, `delivered`, `running`,
`completed`, `failed`, `cancelled`, and `unknown`. Queue acceptance is not execution.
Link a command to a native queue item or turn ID when the native protocol provides
that evidence. Without correlation, show delivery uncertainty rather than treating
any subsequent turn as proof that this command executed.

On a timeout, query the existing operation before attempting another submission.
The shared Codex adapter submits through `thread/queue/add`. A successful add can
start execution immediately when the thread is idle; it is not a promise that the
item remains in `thread/queue/list`. `clientUserMessageId` is correlation data,
not an idempotency guarantee: submitting it again can create another item and
execute the prompt again. Persist the source-qualified operation claim before
submission and the native queue ID after acknowledgement. Do not unconditionally
call `thread/queue/start` after adding: an already-consumed item is absent, not a
failed add. Reconcile ambiguous delivery with queue and turn evidence before
allowing any retry to dispatch.

Use native idempotency if available. Otherwise preserve an ambiguous receipt and
never replay it automatically; application-level persistence alone cannot promise
exactly-once execution across an opaque native queue.

When an external owner exits with pending messages, resume the same native identity
and store after acquiring its writer. Prefer consumption of the existing native
queue. Do not copy queued text into `turn/start` unless a native atomic transfer or
unambiguous cancellation proves the old queue item cannot execute. If reconciliation
is unavailable, retain the pending/unknown item visibly and require an explicit
resolution. Daemon and browser reconnects never create another message silently.

## Recovery and UI

Show one conversation list under each project, irrespective of launcher. Runtime
source and owner are secondary details. A conversation row can display "Controlled
here", "Shared control", "Following; messages queued", "Control unavailable", or
"Reconnecting". Current model, effort, and permission mode include freshness and
remain visible when read-only.

Controls follow returned capabilities and reasons. Writer contention must not
produce a device-offline toast. Source discovery failure, permission denial, lack
of an executable, and unsupported native operations are separate failures. An
operator never needs to import, fork, move a transcript, or restart a native task
just to see or attach to it.

Persist source registrations, aliases, command receipts, and control intent needed
for recovery. Reconnect renews authorization and owner generation before replaying
notifications or enabling mutations. Historical configuration events cannot
supersede a newer settings snapshot. Expired workspace grants revoke subscriptions
and commands independently of whether the runtime continues locally.

## Delivery sequence

1. **Runtime-context plumbing and identity.** Add source registry and context-aware
   adapter interfaces. Carry `session_ref` through roster, history, stream IDs,
   protection/settings caches, inbox receipts, controller grants, and cloud aliases.
   No concurrent global `CODEX_HOME` switching.
2. **Complete discovery.** Enroll existing custom homes; add project-descendant
   matching, durable pagination, catalog deltas, and partial-source diagnostics.
   Ship unified UI listing and read-only settings before expanding control.
3. **Shared Codex server transport.** Implement source-scoped get-or-start, native
   WebSocket attachment, launcher-independent lifecycle, explicit local resume
   commands, and both launch-order acceptance gates. Replace per-conversation
   private stdio servers on supported shared paths.
4. **Automatic control negotiation and other runtimes.** Build atomic attach/resume,
   source-aware inbox fallback, capability-driven UI, fallback-only intent expiry,
   and reconnection on that transport. Preserve message receipts across transitions.
   Add optional endpoint registration and verified adapters for other runtimes.
5. **Rollout and acceptance.** Deploy cloud support for the new capability contract,
   then release CLI source-context support, then enable richer UI behavior per
   advertised device capability. Old devices keep their existing limited catalog
   with an explicit upgrade/coverage notice; bare IDs resolve only when unambiguous.

These changes belong to CLI 0.2.8 and its backend/web deployment, not the 0.2.7
release notes. The design preserves the existing project delegation ceiling and
the agreed device-owner folder-add restriction.

## Acceptance gates

- Agit-first: start a shared server from Agit, start a thread through RC, then use
  the native CLI to resume that ID. Verify the same server instance, native thread,
  active turn, and writer remain; both clients can issue supported operations.
- Native-server-first: start the listener independently, open a native CLI thread,
  then attach RC. Check the same identity and ensure Agit creates no second server.
- Verify plain `codex` discovers the server without claiming it selects an existing
  thread. Verify `codex resume ID` joins that thread. Test explicit `--remote` mode
  separately, including failed handshakes and configurations that disable implicit
  discovery; never report an embedded fallback as successful shared attachment.
- Exercise default and custom homes, a nonstandard registered socket, simultaneous
  starts, and an incompatible listener. Verify no live socket replacement and no
  cross-source thread or credential leakage. Record actual client/server versions
  and the launcher flags for each supported path.
- Two clients exchange input and settings, race an approval response, disconnect,
  and rejoin while a turn streams. Both see authoritative state; no duplicate task
  or application-generated duplicate command appears. Test with an isolated native
  conversation and then the deployed web/controller path.
- In both shared-server launch orders, keep RC silent past the configured fallback
  timeout while output streams. Verify there is no inactivity release, unsubscribe,
  unload, or interruption and native CLI co-control remains available. Restart or
  exit Agit while a native client is attached; the shared server and task continue.
- Acquire fallback control after shared attachment is unavailable, then verify
  non-disruptive release at the no-user-instruction deadline with live observation
  preserved. Keep the default at 15 minutes until that fallback path passes the
  five-minute continuity gate. Observation-only modes never acquire release timers.
- Race a fallback timer with promotion to shared attachment. Cancel or fence the
  stale timer so shared control survives. A transient shared transport failure
  does not arm it; actual fallback acquisition starts a fresh generation and
  deadline without renewing on passive traffic.
- Discover an external TUI before and after daemon startup, in the default home and
  a custom home, without restarting or importing it. Keep it visible after exit.
- Discover project-child sessions and older paginated sessions; exclude adjacent
  directories and unauthorized roots. Report unavailable sources without hiding
  healthy sources.
- Keep identical native UUIDs in different homes separate. Converge aliases of one
  physical store. Exercise two authorized workspaces without duplicate writers.
- Read model/effort/permissions and enqueue into the correct custom home. Assert the
  default home's queue and configuration are untouched.
- Race an external start with an RC resume. Exactly one native writer wins, and RC
  transitions to the appropriate control mode with the same history identity.
- Follow an active idle/running/approval-waiting owner. Enable only demonstrated
  endpoint operations; never infer writer exit from transcript age.
- Exit an owner with pending input; reconnect the browser or restart Agit during
  delivery. Verify no silent duplicate, no invisible pending command, and no false
  execution acknowledgment.
- Revoke membership, rebind a project, replace a source path, or reuse a PID while a
  request is pending. Stale resource generations cannot mutate the replacement.
- Validate schema fallback, network-filesystem notification loss, long histories,
  and source-budget exhaustion with bounded discovery and usable partial results.
- On the affected device, use the existing custom-home conversations as acceptance
  targets for read-only discovery/settings; use an isolated equivalent conversation
  for interrupt, approval, ownership-race, and queue-handoff tests.

Completion requires the web path through the actual cloud controller and device,
not just direct adapter tests or a newly created Agit conversation.
