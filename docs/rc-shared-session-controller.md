# Shared session controller protocol

Status: executor implementation in progress. The Hub session owner and shared Web
fan-out are separate work; ordinary peer/Cloud connections remain in use until
that path is implemented and verified.

An executor advertising `X-Agit-Session-Controller: session-v1` accepts a connection
grant with an optional `session_controller` object. A Hub must not issue this form
without the advertised capability. Personal connection grants omit the field,
including the original 0.2.1 wire format.

The object identifies the canonical native session and runtime, an ownership
generation, and an access ceiling. The granting account must own the target device.
The existing grant binds the source certificate, target device epoch, Hub identity,
and expiry. Renewal may extend expiry but cannot change the delegated scope.

The executor intersects the delegation with its current local owner policy. A
session delegation cannot read another session or obtain file, terminal, project
binding, or machine control authority. Catalog projection obeys the same scope.
Controller actor claims are accepted only on delegated connections; the backend
must authenticate them and enforce current workspace roles and membership before
sending a command. They preserve individual audit and retry identity while the
harness receives one shared control stream. Browser-supplied actor claims must
never pass through the backend unchanged.

Workspace membership is administered and checked by the cloud. Collaborators do
not need local user rules or personal device access when using a delegated backend
controller. The physical device owner can grant, narrow, or revoke the shared scope
in cloud settings; workspace administrators cannot expand that owner's ceiling.
The executor's local checks cover the authenticated capability and native execution
constraints, not a second copy of workspace membership. Admission, renewal and
revocation carry authority changes; ordinary frames use in-memory scope checks.

The tunnel remains an opaque transport. It may enforce connection tickets, but it
does not interpret workspace roles, session access, or controller ownership. Control
plane messages can use that transport without moving policy into the relay. Session
creation, project catalogs, files and terminals require separately scoped authority;
they are not implied by an existing-session grant. Resource scoping does not replace
the agent runtime's tool approvals or operating-system sandbox.

The Hub allocates monotonic ownership generations for each canonical session and
routes all Web collaborators to its current controller owner. The executor pins
the accepted generation and source identity in its private daemon state. A higher
generation replaces the owner; a different source cannot reuse the same generation.
Queued command admission and output projection reject the superseded owner. The
pin survives daemon restarts and is updated only when ownership advances, outside
the executor's main input loop. Existing native writer locks still apply.

The implementation does not turn the first browser's login into a shared service
credential. Hub issuance must use a scoped controller lease independent of any
browser login. Revoking a member removes that attachment; revoking the device or
delegation ends shared authority. Durable workspace destinations remain separately
authorized even when their live session history shares an upstream reader.

The controller library exposes `cloud::SessionRoute` for that service identity.
It obtains session admission with the service credential, verifies the returned
device, session and generation, and reuses the personal route's parallel transport
opening and peer TLS handshake. It does not renew the owner lease, decide workspace
membership, or carry a browser token. The hosting control plane owns those duties.

Project discovery and session creation use a separate `project_controller` grant
and `cloud::ProjectRoute`. The scope binds the executor project ID and absolute
local path, generation and access ceiling. The executor advertises
`X-Agit-Project-Controller: project-v1`; the Hub must check that capability before
issuing the scope through `/api/peer/project-connections`. A grant cannot combine
project and session scopes, and personal grants omit both optional fields.

A project controller can describe its filtered catalog, list runtime models for
the bound project and create a session with an authenticated actor. It cannot
subscribe to session events, read session history, send existing-session commands,
bind folders, browse files or open terminals. Existing-session operations move to
the canonical shared session controller. Rebinding the project path invalidates
the delegated authority. This keeps catalog discovery from creating another copy
of each session's event stream or inheriting machine authority.

The cloud lease and Web integration for project delegation remain separate work;
executor support alone does not enable workspace collaborators in production.

Validation must include cross-account fan-out, owner replacement, retry identity,
member revocation, a slow/disconnected browser, and external writer ownership using
actual CLI artifacts. Existing personal-grant compatibility and session isolation
are checked in the focused peer/executor tests; no new CI matrix is required.
The patch release must wait for active Windows/Linux collaboration acceptance,
including history rejoin and simultaneous commands, rather than idle-only checks.
