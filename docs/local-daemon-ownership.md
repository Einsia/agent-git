# Local daemon ownership on Unix

Each selected daemon namespace (`rc` or `desktop-rc` under `AGIT_HOME`) has an
`agitd.lock` file. The daemon takes an exclusive, nonblocking filesystem lock
before inspecting or binding its control socket. The control listener retains
that descriptor throughout its lifetime and closes the socket before releasing
the lock. Executed child programs do not inherit the descriptor. Normal process
exit and crashes release the kernel lock without relying on a PID lookup.

The lock file is persistent: never delete, rename or replace it while any daemon
or starter can access this home. Replacing it would split contenders across
different kernel locks. Startup and stale cleanup use the same lock, so only one
participating starter can publish an instance. Shutdown does not unlink the
socket, PID file or ownership record; it cannot erase a replacement's state.

The lock contains a versioned record of the socket's filesystem device, inode
and change timestamp. After a refused connection, a released lock with a matching
record establishes that the participating socket owner has exited. Startup
rechecks that identity before removing the stale socket while holding the lock.
The record lives in the selected rc directory even when the socket path falls
back to a shorter path under `XDG_RUNTIME_DIR` or the temporary directory.

`agitd.pid` remains a numeric diagnostic file for compatibility. Its contents
never establish ownership or authorize a signal. A running PID comes from the
control socket's status reply. Process exit, PID reuse, reboot and container PID
namespaces therefore do not turn an unrelated process into the daemon owner.

## Evidence and limitations

- A valid status reply identifies a running daemon.
- A refused socket with a matching record and released lifetime lock is stale.
  A missing socket is absent only when ownership inspection succeeds and no
  instance holds the lock.
- Without a valid status reply, a held lock, connect timeout, unreadable ownership,
  incomplete publication or mismatched socket identity is unknown. Startup
  preserves the socket and reports the uncertainty. A full accept queue can refuse
  connections on macOS or time out on Linux; neither condition alone authorizes
  cleanup.

The lock covers the interval before the listener and PID file are published.
If a process crashes between binding and completing its ownership record, the
remaining socket is deliberately unknown. A partial record cannot prove which
socket instance it describes.

This relies on kernel filesystem locking shared by all processes using the home,
including containers on the same host sharing the same filesystem inode. Use a
local filesystem with working advisory locks. Independently copied homes, remote
filesystems without coherent locks, and processes that deliberately replace lock
files do not provide that guarantee. A changed device or socket identity fails
closed. Process visibility across PID namespaces is not used as evidence of exit.

## Legacy and incomplete state

A responding older daemon remains discoverable through the existing control
protocol. An unresponsive socket with only a numeric PID file, a missing ownership
record or a partial record cannot be recovered automatically: an older daemon may
still hold a live listener. Even a nonexistent PID in the current namespace does
not prove that such a daemon is gone.

For manual recovery, first stop or otherwise establish the exit of every daemon
and starter sharing this `AGIT_HOME`, including those in other containers. Remove
only the control socket named in the startup diagnostic, then retry startup. Keep
`agitd.lock` in place; startup will overwrite its record after binding. Do not kill
the numeric PID merely because it appears in `agitd.pid`, and do not remove another
namespace's state. If ownership cannot be established, preserve the state and
resolve that uncertainty before removing anything.

Windows continues to use its named-pipe ownership mechanism.
