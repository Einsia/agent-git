# Complete Web RC migration

Status: implementation in progress. The Desktop cloud path is verified. The
Web controller prototype is incomplete and is not part of the merged baseline.

This work implements the Web milestone in [the cloud relay RFC](rfc-rc-cloud-relay.md).
The final Web path is:

```text
Browser
  -> authenticated Web control adapter
  -> embedded Web controller using the shared agit-controller protocol
  -> independent tunnel worker
  -> opaque Cloud relay
  -> executor agitd admission and session policy
  -> agent harness session
```

Cloud connection admission and executor resource authorization remain separate.
The relay transports encrypted records and does not interpret session RPCs.
The Web control adapter authenticates browser sessions and supplies their immutable
principal to the controller. The executor decides which sessions that principal
may observe or control. Browser claims cannot grant executor owner authority.

## Merge and rollout boundary

Merge the completed CLI, Desktop, relay, and CI branches before changing the Web
control path. Preserve uncommitted prototypes on separate migration branches.
The partial discovery adapter that treats peer device IDs as legacy connection IDs
must not ship: those identifiers name different resources and protocols.

Use small commits for the shared controller host, Cloud control adapter, Web
transport, feature migration, and legacy removal. A prototype that replaces the
current application with a reduced chat page is not feature parity.

## Implementation order

1. Extract the cloud connector and controller host into `agit-controller`.
   Its standalone binary must not initialize an executor, scan native transcripts,
   open Git repositories, or launch an agent harness. Keep its tunnel workers as
   separate processes. Inspect its dependency graph and packaged binary.
2. Add the authenticated Web control adapter outside the relay module. Controller
   hosting must not depend on the data relay being enabled in that process.
   Identity-domain APIs own controller credentials, admission, and revocation;
   Web adapters must not modify identity tables directly. Bound channel count,
   pending work, initialization time, and output backpressure. Revoke expired
   browser authority and clean up abandoned controller identities after failures.
3. Define a durable mapping from existing Web workspace/project records to peer
   devices and executor project IDs. Keep workspace sharing and navigation intact.
   Convert legacy bindings explicitly and report devices that require enrollment.
   Do not forward legacy `connection_id` or Web workspace IDs as executor scope.
4. Replace the transport beneath `Workspaces`, `WorkspaceLive`, settings, and
   `useWorkspaceSocket`. Preserve structured error codes and `not_sent` versus
   unknown outcomes. Fence responses and events by controller attachment, peer
   route, generation, executor instance, and selected session. Reconnect by
   refreshing authority and reconciling receipts before replaying events.
5. Migrate each feature in the table below. Keep the executor as the authority for
   capability availability, operation receipts, session state, and control rights.
   Store only product metadata in Cloud; do not create another execution scheduler.
6. Package and deploy the controller with explicit configuration ownership and a
   portable pinned CLI revision. Verify dev with chiikawa before the cutover.
   Remove the old Web dispatch, pairing APIs, sockets, and obsolete background
   workers once their callers have migrated. The final Web path has no silent
   fallback to legacy RC. Git history preserves the old implementation.

## Web hosting choice

Web embeds the shared controller library in the backend. A standalone controller
host remains available, but is not required for browser connections. This avoids
per-user daemons and per-tab controller processes without duplicating the protocol.
Only independent tunnel workers run as child processes; no cloud executor scans
transcripts, opens local agent repositories, or launches harness sessions.

A backend replica pools identity leases by authenticated account and login session.
Workspace channels retain separate routes and bounded event queues; a tab can
neither list nor disconnect another channel's peer. Idle leases have a bounded
reconnect grace period, and identity-domain expiration handles crashed replicas.
Token rotation must not revoke other attachments that still use the active login.

Runtime routes and stream buffers stay in memory. Existing workspace/project/member
records and immutable private repository identities remain the durable product
model. Store operation receipts and lineage at lifecycle boundaries, never every
streaming delta. Request admission, aggregate bytes, initialization concurrency,
and per-login use are bounded. Slow clients reconnect and replay session history.
Authority is checked before requests; streamed output may use a short, explicitly
bounded authorization snapshot to keep storage load independent of token rate.

Sharing tunnels or moving the Web controller into a separate service later changes
this hosting module, not the browser protocol, executor protocol, or storage model.
Use this embedded implementation for the initial release instead of adding a
second scheduler or requiring a standalone controller process per attachment.

## Web application and reuse boundary

Keep the existing workspace shell, URLs, project navigation, membership, sharing,
and saved workspace identity. Desktop is the reference for conversation behavior
and the relevant transcript, composer, approval, question, model, goal, and command
presentation. Adapt its spacing and components inside the Web layout; do not copy
the Desktop application root or replace workspace navigation with a device picker.

Web offers Cloud devices only. It has no SSH setup, credentials, port controls,
local-machine discovery, or local executor startup. Cloud device admission remains
independent of whether the caller allows inbound control of another device.

Use a browser transport interface below conversation state and presentation.
Transport owns authentication, controller attachment, route generations, RPC errors,
and connection events. Conversation state owns selected project/session, history,
streamed events, pending intents, receipts, and drafts. Components receive data and
actions rather than importing Tauri or opening sockets. Port cohesive Desktop
modules with their source revision recorded in the commit; do not create a shared
UI package until both applications can consume its platform-independent interface.

Persist `peer_device_id` on each Web workspace and `executor_project_id` on each
bound Web project. The Web IDs continue to identify product metadata and sharing;
executor IDs identify authorized runtime resources. Resolve old bindings using a
verified owner and machine identity, or require explicit device selection when
that identity is unavailable. Never guess from a display name or reinterpret a
legacy connection ID. Retain existing project records during reassociation.

Each browser attachment receives an isolated controller authority derived from its
authenticated account. Workspace roles can narrow permitted operations but cannot
replace that account with the workspace owner's credentials. Membership requires
corresponding device admission and executor grants before live access is available.
Explicit read-only and denied states must survive both API and UI adaptation.

## Feature coverage

| Existing surface | Required behavior through the controller |
| --- | --- |
| Machine discovery and settings | Nested peer-device responses, pagination, independent inbound admission, key changes, revocation, and useful connection errors |
| Workspaces and folders | Existing routes and sharing, bind/unbind, empty executors, folder browsing, permitted filesystem operations |
| Session catalog | Supervised and native sessions, executor-filtered projects, names, saved history, and read-only external sessions |
| Conversation | Start/resume, streamed replies, history pagination, reconnect/replay, drafts, and idempotent operation receipts |
| Runtime controls | Models, permission modes, commands, goals, steering, and interruption where the harness supports them |
| Approvals | Tool decisions and structured questions with the executor's scope and expiry rules |
| Terminals and files | Existing terminal/input/resize/close and file UI through authorized executor RPCs |
| Failure handling | Expired auth, revoked devices, stalled workers, session failures, unknown outcomes, and traceable rejection reasons |

## Prototype review findings

The saved prototype demonstrates a standalone stdio controller host and a browser
transport, but still needs changes before delivery:

- Replace absolute developer-machine Cargo dependencies with a published commit.
- Move Web hosting out of `peer` relay lifecycle and remove direct identity SQL.
- Preserve the existing Web interface and its features instead of mounting the
  reduced `PeerWorkspaces` page as a replacement.
- Handle empty executor catalogs, native session discovery, project/session grants,
  structured RPC errors, event gaps, and browser-level reconnection.
- Reconcile pending operation intents with executor receipts; a failed request
  must not permanently prevent a user from editing the next message.
- Audit credential cleanup and resource bounds when either the browser or the
  controller host exits unexpectedly.

## Acceptance and diagnostics

Use focused tests for the boundaries above and one real dev acceptance session.
Do not add a broad concurrency matrix or duplicate the same fixture in each
repository. Capture request, principal, route, generation, executor instance,
session, turn, worker PID, and outcome metadata without tokens or message bodies.

The acceptance session must use the Web interface and chiikawa to discover an
inbound-enabled executor, create or reopen a conversation, receive a real model
reply, reconnect, and continue using the saved context. Exercise a representative
approval and terminal/file operation when permitted. Verify a denied principal
or externally controlled session remains read-only. Confirm Desktop still works
with its local inbound admission disabled.

Complete the migration only after active Web callers no longer depend on
`/api/rc/connections`, legacy pairing, the legacy machine/viewer sockets, or the
old RC dispatcher. Record the remaining references and their removal in review;
source searches alone are not a substitute for the real acceptance session.
