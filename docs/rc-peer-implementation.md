# Daemon peer implementation

The standard CLI includes both controller and executor. The executable composes
`agit-controller` with the existing local session executor through
`src/rc/peers.rs` and the owner-authenticated local RPC listener. The controller
crate builds independently of the CLI and executor.

Desktop uses a local owner bridge for both local and remote machines. A remote
attachment calls `peer.connect`; subsequent RPCs use `peer.request`. Desktop
unwraps peer event envelopes into its existing machine event API. It does not
start the persistent SSH control process. SSH configuration discovery and the
installation/version probe remain Desktop setup operations.

## Consolidated RC changes

This branch includes the complete source history of CLI !182 (Desktop bridge and
native controls, `d420d869ad440825910a97b798694d01b28df24c`) and CLI !189 (detached
Hub startup readiness and reconnect backoff,
`3594052c975301a3ffadc2f700134a09bac90b9e`). CLI !188 is the combined review and
implementation branch.

The inherited Hub path waits for the exact child and Hub registration before
reporting detached startup readiness. Its reconnect history resets only after
sustained registration. These are retained implementation details, not the cloud
architecture selected by the RFC. Cloud RC will move to the shared `agitd`
controller; merging these branches preserves work and review history without
making the existing Hub control flow a design constraint.

The [cloud relay RFC extension](rfc-rc-cloud-relay.md) specifies Desktop access
without public inbound RC connections and the subsequent Web controller host.
`peer.connect_cloud` implements outbound rendezvous and authenticated ingress
into the existing executor. The deployed Desktop-to-chiikawa chain passed the
real-harness acceptance below. The separate Web controller host remains future work.

## Ownership and protocol boundaries

- The controller registry owns peer actors across UI disconnects. A peer actor
  owns its pending request map, generation, pinned fingerprint, and retry state.
- Tunnel workers run as separate processes and own provider I/O. SSH children
  belong to the worker process tree. The CLI supplies the remote argument vector;
  the tunnel crate contains no Agit session or command vocabulary.
- Local RPC authenticates the operating-system user before routing to either the
  controller or local executor. The remote owner bridge independently checks the
  remote operating-system identity. This path does not grant delegated cloud
  users machine-owner authority.
- Peer events carry a peer ID and connection generation around the unchanged
  executor frame. Each local client has an independent event cursor and bounded
  output queue. Lag closes that client so normal reconnect/replay can recover.
- An RPC timeout expires only that request. The writer and expiration handler
  atomically decide whether queued work can still be dispatched. An expired
  queued mutation is never written later; an already-started write reports an
  unknown outcome if no executor reply arrives.
- Cached attachments carry a route identity and generation. A late mutation
  cannot cross a reconnect or a replacement of the peer configuration. Read-only
  retries may advance the generation on the same route; replacing the route
  always requires explicit reattachment.
- Each wire attempt has a fresh ID prefixed with its logical operation ID. Read
  retries preserve the prefix and original deadline. Mutations are not replayed
  automatically. The executor's existing start and message receipts remain the
  authority for deduplicating accepted operations.

External watch replies do not advertise native inbox delivery. The legacy
`session.enqueue` method checks caller scope and role, then refuses input instead
of launching `codex queue`. Direct owner RPC, peer forwarding, and the inherited
Hub path share this refusal. Sending input to the application's own inbox still
changes its session and therefore cannot bypass the external read-only policy.
This closes that input path; it does not establish exclusive native ownership
for other session operations.

## Executor launch lifecycle

The executor reserves the logical session and any known native runtime/session
identity before handing a launch to a tracked worker. Native aliases conflict
across workspace boundaries. The worker initializes the harness outside the
daemon mutex, with a deadline, cancellation, and panic handling. Bootstrap
commands enter the private session queue before the live generation is exposed.
Only the matching reservation may register a completed launch.

Codex launch waits for the exact `initialize` and `thread/start` or
`thread/resume` replies before publishing the live session. An early
`thread/started` notification cannot substitute for that response. Native
identity must match a requested resume, and explicit refusal of direct input
prevents publication. Buffered native events are delivered after readiness.

An exact native active-writer refusal releases a resume reservation only after
the newly started child tree has been reaped. It does not claim that no process
was spawned. A lost response, malformed success, or uncertain child cleanup
keeps the reservation instead of launching another writer.

A proven pre-spawn failure releases its reservation. An ambiguous launch keeps
its reservation for the daemon lifetime; keyed starts also retain their durable
Pending receipt. Neither timeout nor a lost reply automatically creates another
writer. Connection authority and current confinement are checked again before
launch. These are executor admission reservations, not proof of exclusive native
ownership against external applications.

Cancelling a process owner terminates its process group as well as the direct
child. The daemon reconciles finished supervisor tasks even if their final note
was lost to a panic or cancellation. An outstanding session RPC retains its gate
until its result is projected; a slow observer queue delays retirement until the
terminal state can be queued.

Synchronous native discovery, local repository preparation, and durable roster
writes still run during admission. Moving harness initialization out of the
mutex does not complete isolation of those filesystem operations. External
ownership and recovery after daemon restart still require the native write
permit described in the RFC.

## Validation on 2026-09-15

The standalone tunnel tests cover framing cancellation, size limits, proxy
establishment, write acknowledgments, and independent worker process recovery.
Controller tests exercise out-of-order responses, stream namespaces, identity
pinning, reconnects, read retries, ambiguous mutations, and request isolation
within one peer and across peers.

`tests/desktop/peer_rpc.py` starts two isolated real daemons with separate Agit
homes. Its SSH executable shim connects the worker to the second daemon without
requiring a network service. It verifies distinct process ownership, UI detach,
concurrent local/remote requests, worker replacement, and unchanged executor
identity. This fixture is process integration coverage, not a real SSH test.

The Desktop Rust SSH integration test was also run over real OpenSSH from macOS
to chiikawa with a temporary remote Agit home and its existing compatible owner
bridge. Repeated attachments retained the remote daemon instance. Killing the
local SSH tunnel worker advanced the peer generation and replaced the worker;
both daemon instances remained unchanged and session discovery still worked.
No installed daemon or active user session was restarted for these tests.

The Desktop frontend suite passed (117 tests), its Rust unit suite passed, and
its frontend production build completed. These checks exercise the existing UI
recovery contract and native IPC path; they do not claim a manual GUI/harness
end-to-end test of every session operation.

The full workspace run finished the CLI library with 2565 passes, 6 ignored
cases, and 5 failures. The new tunnel command's missing telemetry registry entry
was corrected and its schema test passed. The bulk-tree deadline and secret-scan
performance tests passed when rerun individually. Both terminal failures involved
an interactive fish shell; all terminal tests passed with `SHELL=/bin/sh`.
Workspace/default and no-default-feature Clippy checks passed. The full workspace
run was not repeated, so this is not a claim of one clean full-suite run. The
live HTTPS proxy case and Windows process tests were not exercised locally.

The executor launch follow-up passed all daemon tests (146) and process-owner
tests (20) on macOS, including cancellation, ambiguous outcomes, native alias
reservations, and missing supervisor exit notes. Workspace/all-target Clippy,
no-default-feature Clippy, formatting, and the CLI build passed.

`python3 tests/desktop/launch_rpc.py target/debug/agit` exercises real owner RPC
with isolated Agit/Codex homes and a synthetic native harness. Concurrent clients
reuse one keyed start, competing resumes keep the same native identity without
creating another writer, and killing one harness preserves the other session
and daemon instance. The existing local and daemon-peer process fixtures also
passed against the updated binary. These synthetic-harness checks do not
establish exclusion against an external native application.

After consolidating !189, the combined branch passed the Hub Link unit suite
(11 tests), detached-startup unit suite (5 tests), and real mock-Hub startup
integration suite (3 tests). The local-owner and daemon-peer process fixtures
also passed, including tunnel replacement and unchanged daemon identities.
Formatting and diff checks passed. Native Windows execution was not repeated.

The native-opening follow-up passed the Codex driver suite (69 tests), the
daemon suite (146 tests), inbox validation, workspace/all-target Clippy, and the
synthetic launch process fixture. `tests/desktop/native_writer_rpc.py` also
passed with installed Codex 0.153.2, isolated native/Agit homes, seeded synthetic
history, and no model request. Direct and peer RPC both rejected an externally
locked roster session despite an old transcript timestamp. Repeated refusals
left the external process and transcript intact. After release, peer resume
succeeded and a competing native app-server was refused by Codex's writer lock.
The SSH shim in this fixture is process coverage, not a real network test.

These results establish the tested Codex opening behavior, not a universal
native ownership contract. In an isolated Claude Code 2.1.222 probe using
only local synthetic model responses, competing processes resumed the same
native session and both continued accepting input. Agit-only reservations do
not establish exclusion against such external applications. Further ownership
work remains separate from controller/tunnel architecture acceptance.

Codex discovery and resume admission inspect its existing native writer lock
under a nonblocking shared coordination lock. An occupied writer remains
read-only even when its transcript is old. A released writer becomes resumable
immediately even when its transcript is fresh. Observation neither creates nor
removes native lock files. If native coordination is absent or cannot be read,
the existing transcript-recency estimate remains in effect; other runtimes keep
their own admission behavior. The observation does not reserve a session:
executor launch reservations and Codex's atomic `thread/resume` writer lock
still arbitrate competing launches before input is accepted. Native runtimes
that do not participate in Codex's writer-lock contract are not covered by it.

The native writer fixture refreshes the transcript timestamp after its external
owner exits and checks both the local catalog and immediate peer resume. It also
checks rejection while the external writer is live and reverse native exclusion.
This fixture requires an installed Codex and remains a targeted acceptance check,
not an additional required CI job.

## Utility connection event selection

An authenticated Cloud connection can include `session_events: false` in
`machine.describe` parameters. The executor then omits session notifications on
that connection while retaining RPC replies and authorized terminal events.
Other connections keep their own event selection and resource permissions.
Omitting the parameter leaves the selection unchanged; new connections default
to receiving authorized session events. The authenticated description includes
the current `session_events` value and advertises `session-events-v1`.

The Web utility route uses `Route::without_session_events()` to include this
preference in the existing handshake on every reconnect. Shared session routes
keep their event stream enabled. This removes duplicate transcript traffic before
it enters the tunnel, without adding a serial discovery request. Released 0.2.1
executors ignore the optional parameter and keep their personal peer/Cloud path;
their descriptions do not advertise delegated session services or event selection.

## Operational diagnostics

`cloud.presence_connected` and `cloud.endpoint_authenticated` include optional
`transport_timing` for the actual WebSocket worker connection. Its durations are
`dns_ms`, `tcp_ms`, `proxy_connect_ms`, `tls_websocket_ms`, and `total_ms`.
`proxy_connect_ms` is null for direct connections. `ipv6` describes the connected
TCP socket, including the proxy socket when a proxy is used. TLS and WebSocket
negotiation remain one combined measurement.

These worker durations exclude process startup and parent IPC. Compare their
`total_ms` with the endpoint's existing `transport_ms` to measure the remainder;
do not attribute that remainder exclusively to process startup. Correlate the
endpoint event with controller logs by `link_id`, since device wall clocks can
differ. No additional network requests are made to obtain these measurements.
The values stay in local diagnostics and contain no remote addresses, headers,
credentials, or conversation content.

The parent opts its child into the extended worker reply using
`AGIT_TUNNEL_CONNECT_TIMING=1`. Without that opt-in the reply retains its original
shape; a parent also accepts replies with no timings. This keeps worker startup
compatible when an executable is replaced while an older daemon remains alive.

Cloud `session.list` accepts an optional `resolve_session` logical or native ID.
Its `resolved_session` field contains executor-owned session, native, runtime,
workspace and project coordinates, or null when no readable native mapping is
available. Resolution uses the resource registry, including durable roster
entries, so the conversation need not appear in the active session catalog.
The connection's current read authority still applies when the response leaves
the executor. The field describes identity only; it does not start a harness or
claim that the session is active. Ordinary catalog requests remain unchanged.

Local owner daemons retain structured metadata at
`$AGIT_HOME/desktop-rc/diagnostics-<instance-id>.jsonl` (normally under `~/.agit`).
`machine.describe` returns the exact path as `diagnostic_log`; it is null if the
log cannot be opened, and startup stderr explains that failure. Detached startup
and Desktop's `bridge --ensure` also preserve stdout/stderr in private
`agitd-*.log` files in that directory. The existing Desktop diagnostic log remains
`~/.agit/desktop.log`.

Structured records include timestamps, daemon PID/instance, peer route and
generation, tunnel-worker PID, client/request IDs, executor dispatch correlation,
session status, error codes, and controller failure operation IDs/outcomes.
Request bodies, response bodies, and transcript content are excluded. Use the
daemon instance and request ID to follow a local call, then peer route/generation
and the failure operation ID to inspect connection failures and uncertain writes.

Controller stderr also records `controller.cloud_connect` with the source,
target, and relay link IDs. Its phase durations separate admission, transport
opening, relay pairing, and peer TLS. Admission and transport opening overlap;
their durations must not be added to calculate the connection total. The controller
sends TLS ClientHello immediately after its relay join. It validates the expected
relay ready frame before delivering received bytes to TLS and authenticates the
executor certificate before any RPC. Pairing keeps its own deadline; the TLS
allowance starts when pairing completes. Executors use the existing join/ready
sequence, including released Linux 0.2.0 peers.
`pairing_tls_ms` measures this overlapping phase, `pairing_ms` ends at validated
relay readiness, and `peer_tls_after_ready_ms` measures the remaining TLS wait.
Do not add `pairing_ms` to `pairing_tls_ms`.
`controller.peer_handshake` associates the route and worker with transport and
`machine.describe` durations. Failed attempts include their completion state;
neither record includes credentials, certificates, or message contents.
These controller records use a bounded, nonblocking queue and a dedicated stderr
writer thread. A stalled consumer drops excess diagnostics without delaying
connection readiness or cancellation; process exit does not wait to flush them.

The writer runs off the request loop with a bounded queue. Its structured files
rotate at 4 MiB and retain three backups per daemon instance; each file is
owner-readable/writable only. Sequence gaps and a cumulative dropped counter
identify diagnostic loss under pressure. Files from prior instances and raw
startup logs remain available across restart. A full disk or crashed process can
lose diagnostic records; these logs are troubleshooting evidence, not execution
receipts, and logging failure must not authorize a retry or block request I/O.

## Architecture integration validation

After the logging change, isolated controller/tunnel suites passed (10 and 15
tests, plus the tunnel process test), along with local RPC/history tests (13),
the log rotation/content-boundary test, workspace/all-target Clippy, formatting,
and the CLI/bridge build. The controller dependency graph contains the tunnel
crate and shared infrastructure without the root executor crate.

The local-owner, daemon-peer, and synthetic launch process fixtures passed.
The peer fixture verified private persisted records for recovered worker PIDs,
connection generations, and failed operation IDs. Desktop passed its frontend
suite (117 tests), production build, and Rust suite (8 tests; 2 opt-in tests).
The real SSH opt-in test passed using the rebuilt local bridge and an isolated
remote owner daemon on chiikawa. A subsequent SSH worker kill advanced the peer
generation while preserving both daemon instance IDs, with the recovery present
in the local diagnostic file. No active user daemon was restarted.

This validates the current controller/tunnel/SSH architecture stage. Cloud RC
deployment and comprehensive external-writer arbitration remain later work;
the existing Hub implementation is retained compatibility code. Native Windows
execution and a second full CLI workspace run are not claimed here.

## Cloud peer ingress implementation

`agit rc cloud enroll --hub <origin>` persists a private endpoint identity and
device credential in the local daemon namespace. The enrolling account receives
an explicit machine-admin policy only when no machine rule already exists.
`devices`, `status`, `policy`, and `grant` inspect or update enrollment and
executor resource access. SSH continues to use host-owner authority.

The daemon supervises independent presence workers for enrolled origins. Each
offer obtains a fresh verified cloud grant, a separate data worker, and a mutual
TLS channel before entering the shared executor mux. Presence reconnection does
not close established data sockets. Controller `peer.connect_cloud` routes obtain
fresh discovery, credentials, grants, and tickets on each retry; a changed target
certificate or owner is rejected.

Cloud clients have a separate connection budget and bounded output queues.
Resource checks cover catalogs, history, commands, watch aliases, and event
fanout; output projection rechecks policy before sending queued replies.
Wire-supplied caller claims and peer management calls are refused. Duplicate
in-flight IDs close the cloud connection, including IDs of pending error replies.
Message receipts and durable launch IDs are scoped to the authenticated principal.
Native and logical session aliases retain session-specific denials through resume.
The executor carries a transport-independent admission lease into command tickets
and native launch preparation. Observed policy changes and closed cloud attachments
reject work that has not been accepted; accepted operations retain their normal
completion and uncertainty receipts. Local and SSH owner calls use their existing
authority. Policy read failures deny cloud access and emit a diagnostic event.

The integration fixture runs mutual TLS into the shared executor mux and checks
filtered catalog/events, trusted caller stamps, duplicate-ID rejection, and an
owner socket that remains usable after the cloud connection closes. Its executor
responses are controlled fixtures; it does not prove a deployed relay or a live
harness conversation. Desktop includes cloud discovery and selection through owner
IPC; the deployed chain also passed the real-harness acceptance below.

Cloud diagnostics retain policy revisions, presence state, device/grant/client
IDs, worker PIDs, retry delays, and connection close reasons. The cloud description
omits the local diagnostic file path. Credentials and transcript bodies remain
outside these logs.

### Process-level cloud validation

`tests/desktop/cloud_rpc.py` runs against the backend's opt-in
`cloud_peer_process_chain` fixture. It uses public login/enrollment APIs, real
controller and executor processes, independent OS tunnel workers, and a synthetic
Codex app-server. The test passes native launch, duplicate receipts, events,
history, worker replacement, unchanged daemon identity, event replay, and an
executor session denial. It retains private diagnostics and removes disposable
credential stores during cleanup.

The authenticated cloud account is loaded from the selected Hub's saved login.
Executor project responses publish trusted coordinates before exposing success,
so a newly bound project can immediately launch a session. TLS adapters close
both byte pumps when either side stops; an idle endpoint observes tunnel failure
and lets the controller reconnect.

Set `AGIT_PEER_CLI` to the built CLI and `AGIT_PEER_PROCESS_TEST` to the absolute
script path when invoking the backend test. `AGIT_PEER_DESKTOP_TEST` optionally
adds the Desktop `cloud_bridge_ipc` Rust test executable to the same fixture.
These are test-only inputs. A synthetic harness does not establish a successful
real-model conversation or a deployed cloud service.

### Deployed cloud acceptance on 2026-09-15

`tests/desktop/cloud_live.py` passed against `https://dev.agent-git.com`, an
isolated chiikawa executor, and a real Codex harness using `gpt-5.6-sol`.
The Desktop native IPC adapter submitted a turn through its local controller,
independent tunnel workers, and the cloud relay, then observed the matching
successful completion and assistant reply.

The run verified duplicate start/turn receipts, history and event replay,
worker replacement from generation 1 to 2 without changing the executor instance,
conversation context after reconnection, stale-route rejection, and executor
session denial with Forbidden plus catalog removal. The owner RPC remained
available. Cleanup stopped the isolated daemons, revoked the disposable devices,
and removed their saved credentials. The installed Desktop daemon was untouched.

Backend deployment pipeline 18086 and dev runtime configuration pipeline 18093
passed. Private evidence is retained locally at
`/private/tmp/agd-cloud-live-gx9miu7b` and on chiikawa at
`/tmp/agit-cloud-chain-01a0a312/live-18093-a`. The event journal contains test
conversation evidence; daemon diagnostic logs contain metadata only.

## Remaining acceptance work

The full [RFC](rfc-rc-daemon-peer-transports.md) remains open. In particular:

- Replace transcript-recency takeover heuristics with executor-owned exclusive
  native session control. External or unknown ownership must be read-only
  through every entry point. Current read-only watch behavior and per-session
  RPC gates do not establish that stronger guarantee.
- Move the remaining synchronous start/resume discovery, repository preparation,
  and durable writes out of the executor state lock with revalidation.
- Add durable controller recovery/operation receipts where mutations must be
  replayable across controller restarts, and delegated peer authorization.
- Host the standalone controller for future cloud Web RC. Existing Hub
  registration and replay remain a compatibility protocol adapter.
- Verify mutual control with active harnesses and the complete acceptance suite
  over every supported platform/provider. Windows process-tree behavior needs
  its Windows CI environment.

The controller and tunnel split is an implementation milestone, not completion
of these remaining executor and cloud requirements.

## Grants on the presence channel

Executors request `X-Agit-Peer-Offer: grant-v1` on their authenticated presence
WebSocket. A supporting Hub attaches the verified connection grant to the offer,
avoiding a separate executor HTTP verification round trip. The executor validates
the issuer, expiry, exact target credential and source identity before endpoint
TLS. The relay still rechecks live authority before `DataReady`, and the executor
still applies its resource policy to every admitted operation.

The offer's optional `grant` field is sent only to an executor that requests it.
Linux 0.2.0 presence messages retain their existing shape. A new executor can use
the existing peer verification endpoint when a Hub does not attach a grant; this
is peer protocol negotiation and does not restore paired RC. Diagnostics record
`verification_source` as `presence` or `http` for actual rollout verification.
