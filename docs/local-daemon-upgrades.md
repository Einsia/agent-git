# Local daemon upgrades

The local owner daemon lives in the selected `AGIT_HOME/desktop-rc` namespace.
Replacing an executable does not replace the image already running in that
daemon. CLI upgrades and bridge attachment use the same local reconciliation
policy; neither searches other `AGIT_HOME` directories.

## Identity and compatibility

`agit rc local status` includes an optional `identity` object with `instance_id`,
`build_id`, `executable`, and `rpc_features`. An absent identity denotes a daemon
that cannot negotiate this contract. `machine.describe` exposes the same
`instance_id`, `build_id`, and `rpc_features`, without the installation path.

The build ID is embedded at compilation from source inputs, workspace dependency
manifests, the dependency lock, compiler identity, target/features, and relevant
build settings. It does not read the executable's installation path at query
time. The instance ID changes whenever a daemon starts.

`bridge` requires `peer-control-v1` and `history-v2`. Compatible builds can share
the daemon. A client can add requirements without changing the raw RPC stream:

```sh
agit rc local bridge --ensure --require-feature safe-restart-v1
agit rc local bridge --ensure --require-current-build
```

The second command requires the bridge's exact build, including builds with the
same package version. An unsupported requirement fails before starting or
stopping a daemon. Without `--ensure`, attachment never starts or replaces one.
Diagnostics stay on stderr; stdout contains only the client's RPC traffic.

## Safe replacement

Only a daemon advertising `safe-restart-v1` accepts the local control request
`stop_if_idle`, fenced by its expected instance and build IDs. Admission closes
atomically with the check for accepted RPC work. Requests remain accounted for
through reply delivery, including requests whose viewer disconnects.

Automatic replacement is deferred while supervised sessions, launch reservations,
unresolved durable start receipts, terminals, session RPC workers, or controller
peer connections remain active. A session waiting for input or approval still
owns its runtime and blocks replacement. Read-only subscriptions can reconnect;
historical session records alone do not prevent replacement.

A busy result reopens admission. An accepted stop closes admission, replies to
the control client, and uses normal shutdown. A namespace-local file lock
serializes cooperating reconcilers. A replacement starts only after the old
daemon is confirmed absent, and must pass the required build/capability checks.
Timeouts and uncertain process state do not authorize force-kill or socket removal.

To request a safe restart explicitly:

```sh
AGIT_HOME=/path/to/state /path/to/agit rc local restart --if-idle
```

The result is JSON: `absent`, `unchanged`, `restarted`, or `deferred`. A deferred
explicit restart returns a precondition exit code and includes the recovery
command. If no daemon exists, it remains stopped. Ordinary `rc local stop` keeps
its explicit behavior of stopping supervised work.

## Upgrade and legacy behavior

Before executable replacement, `agit upgrade` captures the running local
instance associated with that installation. After replacement, the installed
CLI performs reconciliation. A daemon owned by another executable is left
alone. A changed instance is rechecked rather than stopped using an obsolete
observation. Skill refresh runs independently of the daemon outcome.

A successful installation can report that daemon restart was deferred. This is
not an installation rollback. After the blocking work ends, retry the displayed
safe restart command or the attachment that requires the installed build.

A daemon without safe restart negotiation is never stopped based on a separate
status snapshot. The error identifies the local daemon and asks the owner to
finish user work before explicitly stopping and starting that namespace. This
legacy transition can require manual recovery. On Unix, stopping a daemon that
predates lifetime ownership can leave control sockets without an ownership record.
Automatic startup and `restart --if-idle` preserve these sockets because they
cannot distinguish a stopped daemon from an unresponsive legacy listener.

After independently confirming that every daemon and starter using the home's
local namespace has exited, including in containers, use the scoped command in
the diagnostic:

```sh
AGIT_HOME=/path/to/state /path/to/agit rc local recover --confirm-stopped
AGIT_HOME=/path/to/state /path/to/agit rc local start --detach
```

Recovery does not stop or start a daemon. It removes only the control socket and
preserves the lifetime lock and durable state. Keep legacy starters disabled until
replacement startup finishes. See [ownership recovery](local-daemon-ownership.md#legacy-and-incomplete-state)
for the confirmation requirements and filesystem checks. A local controller error
does not imply that an SSH executor needs an upgrade.

Reattachment restores views and subscriptions using existing identities and
receipts. Clients must not replay writes whose outcome is unknown. Daemon
replacement neither recreates sessions nor starts another turn automatically.

## Focused verification

`tests/desktop/upgrade_rpc.py` exercises the real local control boundary. To also
exercise actual executable replacement with a synthetic release server, pass a
different build supporting safe restart and a build without that feature:

```sh
python3 tests/desktop/upgrade_rpc.py /path/to/new-agit /path/to/old-safe-agit /path/to/legacy-agit
```

The new and old-safe fixtures must report the same package version. The test
uses isolated homes and installation paths, checks retained project state,
verifies busy deferral and concurrent bridge attachment, and leaves an unrelated
namespace running until fixture cleanup. It does not download a production
release or connect to a model provider.
