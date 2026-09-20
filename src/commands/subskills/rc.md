---
name: agit-rc
description: Start and manage owner-authorized remote agent sessions.
---

# Remote control

Run this on the machine where your code and agent runtime are installed:

```sh
agit login
agit rc start --detach
```

Sign in to the same Hub account you use in Workspaces. Startup runs agitd, registers a Cloud device,
and enables inbound control for your own account. Open the printed Workspaces URL with the
same account, choose the device and a project folder, and start a conversation. No pairing
code, separate enrollment command, or configuration edit is required.

Other users need explicit device admission and executor access grants. Workspace membership
alone does not grant access to a device. Authentication and authorization remain independent
of tunnel transport.

Use `agit rc start --detach` to run in the background. Local and SSH access stay available
while Cloud registration or networking retries. Inspect `agit rc status` and the printed
private daemon log when a device is not online. `agit rc stop` stops that daemon and its sessions.

`agit rc local start --detach` is the outgoing controller startup used by Desktop. It does
not enable inbound Cloud access. `agit rc cloud inbound --hub <origin> --enabled false`
disables inbound access independently of outgoing control.

Advanced device management: `agit rc list`, `agit rc revoke <device-id>`, and `agit rc cloud
status --hub <origin>`. Registration is performed by `agit rc start`; there is no separate
enrollment command. SSH uses `agit rc local bridge --ensure`.

The peer executor supports native Windows, Linux and macOS. Windows uses a current-user local
named pipe for owner RPC; Unix systems use a current-user local socket. Cloud peers use the
same transport and session protocol on all three platforms.

After revoking a device, sign in as its owner and run `agit rc start --detach` on that
machine to reconnect intentionally. Startup replaces a revoked registration while keeping
the device identity used by its workspaces. Healthy registrations keep their credentials;
background connection retries cannot undo revocation.
