---
name: agit-rc
description: Start and manage owner-authorized remote agent sessions.
---

# Remote control

Run this on the machine where your code and agent runtime are installed:

```sh
agit rc start
```

On first use, follow the sign-in prompt. The command starts agitd, registers a Cloud device,
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
status --hub <origin>`. `cloud enroll` is an explicit registration tool; ordinary startup
performs registration automatically. SSH uses `agit rc local bridge --ensure`.

The peer executor currently supports Linux and macOS. On Windows, run it inside WSL; native Windows RC does not fall back to the removed pairing transport.
