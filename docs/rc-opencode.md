# OpenCode remote control

A current AgentGit daemon can drive OpenCode through its native ACP stdio
interface. The machine must have OpenCode on PATH and a working provider
configuration. Run `opencode --version` in the terminal that starts `agit rc
start`, then restart the daemon and reload the web workspace after installing
or upgrading the runtime. Executable detection does not verify authentication.

## Supported operations

The driver supports new sessions, native session resume in the same project
directory, text prompts, streamed assistant replies, interruption, and
single-request allow or deny decisions. An optional model override is passed
through ACP. The web renders these operations from the machine's capability
report.

Remote OpenCode uses the default permission mode and a private launch-specific
agent that requests tool approvals. The driver never chooses ACP's
`allow_always` response. Existing native sessions with explicit permission
overrides cannot be resumed through this policy; start a new RC session.
Provider configuration and authentication remain on the machine.

Native slash commands, mid-turn steering, permission-mode switching, nested
agents, and interactive question tools are not exposed by this driver. Tool
execution stays in OpenCode; the ACP client does not implement filesystem or
terminal requests that could execute an operation outside the native approval
path. The embedded native server binds to loopback with an ephemeral port and
a generated process-local password.

## Record consistency

ACP provides control events and transient text, while completed records come
from OpenCode's SQLite database. The supervisor reads a bounded native snapshot
at readiness, turn completion, and after proving that the native process tree
has stopped. The exit snapshot includes persisted records from incomplete
turns; transient text that OpenCode has not written to its database is not
recoverable from that snapshot. Snapshot failures surface a recovery message
without invalidating process termination. Native row identities survive in-place
updates; message context determines each part's role. A resumed supervisor
seeds existing records without publishing them as new messages.

Read-only watches reread the database, retain context outside the visible
history window, and wait for terminal assistant records. The projection uses
the adapter's native event mapping and raw object hashes, with the same secret
redaction rules as other RC runtimes. The database remains the source used by
normal AgentGit settlement.

## Validation evidence

The native driver was exercised locally with OpenCode 1.18.30 and an isolated
OpenAI-compatible synthetic model service. The tests created and resumed the
same native session, read its database records, denied a file-writing shell
request, allowed that request once, and cancelled it while permission was
pending. Denial and cancellation left the marker absent; a single allow
created it. A cancelled approval was followed by another approval in the same
native process. Exit probes retained stored prompts and completed tool records
when the next model response was interrupted or failed. This establishes local protocol and tool-execution behavior, not
access to a real model provider or a deployed Hub result.

The native smoke test is opt-in because CI does not install OpenCode or a
provider. Normal unit tests cover scoped acceptance, duplicate inputs,
one-shot approval translation, stale native request identities, cancellation,
permission overrides, database row changes, workspace relocation, native hashes,
redaction, and history-window context.

Protocol references: [OpenCode ACP](https://opencode.ai/docs/acp/) and
[OpenCode server](https://opencode.ai/docs/server/).
