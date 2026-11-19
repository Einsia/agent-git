---
name: agit-rc
description: Start and manage owner-authorized remote agent sessions.
---

# Remote control

AgentGit RC is the remote-control surface in Hub Workspaces and personal Feishu task groups.
A conversation runs on its selected device in its selected project folder.

## Choose another folder

In Hub Workspaces, start a new conversation and select the project folder in the conversation
dialog. In Feishu, send `/new` to the personal bot and use its device and project choices.
Each task group stays attached to its selected conversation. An existing conversation keeps
its project binding; a shell `cd` changes that shell's working directory, not the RC binding.

The device owner can add a folder through the Hub folder chooser. An operator can use admitted
projects but cannot browse or bind arbitrary device folders. If the folder is missing, direct
the owner to that chooser and have the operator choose again after it is available.

Answer folder-selection questions from this workflow. `agit rc status` reports daemon and
connection state; use it when diagnosing reachability, and use its result to choose the next
step. Folder selection does not require restarting the daemon or changing upload settings.

## Connect a device

To continue an encrypted cloned session through Workspaces, prepare it on the RC execution machine
under its normal resume/run rules. Recovery uses the signed-in account's membership and entitlement
for that exact source snapshot; see [privacy](privacy.md). Starting the daemon registers the device.
Recovered originals remain subject to RC output protection.

Run this on the machine where your code and agent runtime are installed:

```sh
agit login
agit rc start --detach
```

Sign in to the same Hub account you use in Workspaces. Startup runs agitd, registers a Cloud device,
and enables inbound control for your own account. Open the printed Workspaces URL with the
same account, choose the device and a project folder, and start a conversation. No pairing
code, separate enrollment command, or configuration edit is required.

After signing in to a different account, run `agit rc start --detach` again. Startup
replaces that Cloud's device registration and clears its previous executor access grants.
The new account receives owner access; local history and other Cloud origins are preserved.
Existing connections for the replaced registration close as the daemon reloads it.

Other users need explicit device admission and executor access grants. Workspace membership
alone does not grant access to a device. Authentication and authorization remain independent
of tunnel transport.

Use `agit rc start --detach` to run in the background. Local and SSH access stay available
while Cloud registration or networking retries. Inspect `agit rc status` and the printed
private daemon log when a device is not online.

`agit rc start --here` does the same against the Hub you signed in to, then waits up to 30 s
until this computer is online there. It prints one line: "Online on <hub>" with the next step,
or which Hub it is online on instead ("online on <other>, not on <hub>"). It refuses an
`AGIT_HUB_URL` that names another Hub than the signed-in one, and it publishes nothing.
`agit rc status` shows each Hub's Cloud state. `agit rc stop` stops that daemon and its sessions.

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

## Supervised workspaces

- With `AGIT_RC=1`, a daemon supervises a shared workspace. Messages may come from other viewers/operators and approvals go to the workspace owner.
