# Workspaces: decoupling a session from the terminal

The first three documents are about "a session as an asset" — the session runs on your machine,
and agit versions it losslessly, publishes it, fetches it back. This one is about the other half:
**which machine a session runs on is decoupled from where you watch and direct it**.

Closing the laptop must not stop the session. The session itself keeps running on the machine
that has RC turned on; the terminal, the web interface and a phone are only views of it.

## 1. Three layers, landing exactly on the existing model

```text
workspace              one machine + a set of folders + a set of members
  └─ project           one folder          ⇄  one **private agent repo**
       └─ session      one conversation    ⇄  one **branch** in that repo
            └─ one settlement              ⇄  one **commit**
```

The right-hand column is not newly invented: it is the `agent = repo, branch = session,
commit = snapshot` of `docs/03_branch_model.md`. A workspace only adds one layer of orchestration
on top, answering "which folder belongs to which repo, which machine is running it, who may come
in".

So every message sent from the web interface still lands as a turn commit, through the existing
automatic hooks settlement — **no new commit path**. Otherwise a gap opens between "talked to it
on the web" and "not in `agit log`", and that is the one gap this product cannot have.

## 2. Start an executor

```bash
agit login
agit rc start --detach
```

Startup enables inbound Cloud control for the signed-in owner, registers a peer device,
and starts independent presence and tunnel workers. The daemon supervises harness sessions
locally. Controllers on Web and Desktop use the same peer session RPC; the Cloud relay
forwards opaque endpoint traffic. SSH authenticates through the machine's SSH service.

Cloud registration retries in the background without holding local daemon startup or session
execution. A ready local RPC endpoint does not imply Cloud presence is online. Use
`agit rc status`, `agit rc cloud status --hub <origin>`, and the private daemon logs to
inspect the local process and enrollment, then verify Online in the Cloud device list.

The device's owner can create a workspace without another enrollment command. Other accounts
need explicit device admission and executor permissions. Reconnect preserves the existing
peer identity; it does not consume another device slot. Revocation removes remote access.

Desktop's `agit rc local start --detach` starts outgoing control independently of inbound
access. `agit rc cloud inbound --hub <origin> --enabled false` disables incoming Cloud control.
The standard peer startup works on native Windows, Linux, and macOS. Linux 0.2.0's existing
peer enrollment remains supported; its default paired startup requires upgrading.

## 3. Binding a project = creating a private repo

**Bind a folder** in the web interface, or let someone else see it inside your workspace:

* At the moment of binding, the **private agent repo** for that folder is created (its name comes
  from the folder name, `/home/me/projects/Agent Git` → `agent-git`). The user is not asked for a
  name again — they just pointed at a folder, and that is this project's identity.
* From then on every session in this project is a branch of that repo and every turn's settlement
  is a commit, pushed the moment it settles.

**The bound folders are the allowlist.** agitd refuses every fs and exec request outside them,
and this check runs **on the machine**, not in the hub. The reason is the trust boundary: the hub
relays instructions and projects state, it is not the source of permission; even with the hub
broken into, the ceiling on the damage is pinned by local policy on the machine.

Binding refuses only system roots (`/`, `/etc`, `/usr`, ...). A project may sit in `/srv`,
`/opt`, `/workspace` — confining it to `$HOME` puts half the real repos out of reach.

## 4. Take over a session already running on this machine

The most common way in is not "new", it is "I have been talking in the terminal for a while and
want to keep going from the web".

The left side of the page lists the local sessions under this workspace's folders that **can be
taken over**, ordered by most recently talked to, carrying the opening prompt as a summary. That
list **does not scan the whole store**:

* codex goes through its own `threads` table's `(archived, cwd, updated_at DESC)` index (observed
  0.8 ms), and `first_user_message` hands over a summary for free, with no transcript to parse.
* Claude Code is one readdir of `~/.claude/projects/<cwd-slug>/`.

So the cost tracks "how many sessions this project has", not the twenty thousand rollouts on
disk. Only the first dozen or so are parsed once each for their summary, and that is a constant.

**A session that is still alive cannot be taken over.** A transcript file that was still growing
a moment ago (within 90 seconds) is most likely open in someone else's terminal; `--resume` puts
a second writer on the same transcript file, and once the two streams of appends interleave, both
histories are destroyed. That is data corruption, not an experience problem, so the machine side
refuses outright and the UI blocks it too. Quit in that terminal first, then take over.

Taking over uses the same semantics as `agit resume`: the fast path is a native resume (the
harness's own `--resume`), content untouched and id unchanged.

## 4.5 Drive a session from the web

The web interface is not a chat box bolted onto an agent, it is **another terminal for that
session**. So everything you would do in the terminal has to be here:

| Action | How it is sent | Semantics |
|---|---|---|
| Talk | `turn.start` (when idle) / `turn.steer` (mid-turn) | see below |
| Interrupt | `turn.interrupt` | claude-code leaves `[Request interrupted by user]` in the transcript; that turn is recorded as `interrupted` |
| Answer an approval | `approval.decide` | `session_id` required, `allow` / `deny`, plus a `scope` |
| Switch permission mode | `session.setPermissionMode` | see the table below |

**A mid-turn steer does not arrive at the same moment on both runtimes, and that has to be
shown.** codex's `turn/steer` is a protocol-level verb and arrives at once; claude-code writes
one more user message to stdin and **waits for the current tool call to return** (observed:
written at 4.00 s, in the stream only at 35.06 s). The `delivery` field of the response says
exactly this, and the page renders "queued, delivered once the current tool finishes" from it —
without that, a user on a weak network believes the message was lost and sends it three times
over.

### Permission modes

A web session, like a terminal session, can tighten or loosen the guard at any time:

| Mode | Means | claude-code | codex |
|---|---|---|---|
| `default` | the machine's own configuration decides; everything else is asked every time | `--permission-mode default` | `on-request` + `workspace-write` |
| `accept_edits` | file edits land without asking, commands still ask | `acceptEdits` | **unsupported** (it does not separate editing files from running commands) |
| `auto` | everything the sandbox permits proceeds | `auto` | `never` + `workspace-write` |
| `plan` | read-only, look but do not touch | `plan` | `never` + `read-only` |
| `bypass` | no checks at all | `bypassPermissions` | `never` + `danger-full-access` |

What runs on the wire is this **neutral vocabulary**, not either side's native values — the two
shapes are fundamentally different (claude-code has one scalar, codex has two axes,
approvalPolicy × sandbox), and passing native values makes the web interface speak one harness's
dialect and then mistranslate the other. Every runtime reports during machine discovery which modes it
can **actually express** (`capabilities[].permission_modes`), and the page renders only the ones
reported. Offering codex an `accept_edits` gets the user asked anyway after they pick it — better
not to offer it.

**When a switch takes effect is reported too** (`permission_switch`): claude-code is `immediate`
(observed: the `set_permission_mode` control request answers `{"mode":"auto"}` and takes effect
on the spot); codex is `next_turn` — its policy rides along with `turn/start` as a sticky
override, and cannot be changed halfway through a turn. The page says "takes effect next turn" as
it is, and does not pretend the change already happened.

**Loosening needs authorization, tightening does not.** Switching to `bypass` is owner-only (it
hands out an approval-free shell on a real machine, the same class of act as binding a
directory); switching to `plan` / `default` is open to any operator at any time — needing to ask
before you may hit the brakes is absurd.

### Approvals

The approval card has three buttons: **Allow**, **Deny** (a reason can ride back to the model
with it), and **Allow, don't ask again** (`scope: "session"`). The third one appears only where
this call really supports it (`can_allow_for_session`) — claude-code gives its suggestion per
call, codex supports it uniformly.

It lands as two different things on the two sides: claude-code sends the `permission_suggestions`
the CLI attached to the request straight back unchanged (observed shape
`[{"type":"setMode","mode":"acceptEdits","destination":"session"}]`), so **the mode changes along
with it**, and the machine broadcasts a `session.permissionMode` to bring every viewer up to
date; codex answers `acceptForSession` outright and remembers it itself.

**A `permission_escalation` approval always goes back to the owner.** Stepping outside the
allowlist, reaching the network, escalating privilege — only the machine knows the test for these
three, so the hub records the `kind` when it relays `approval.request` and authorizes the answer
by it; **a kind it cannot determine is treated as the strictest** (as owner-only). The cost of
allowing is irreversible; asking once more is merely slow.

## 5. Sharing

Three roles; the test is "can this change state on that machine":

| Role | Can do |
|---|---|
| `viewer` | read the event stream and the history, see who is driving, **no approval buttons** |
| `operator` | + send messages, steer mid-turn, interrupt, answer ordinary approvals, preview files |
| `owner` | + add and remove members, bind project paths, delete the workspace, **open a terminal**, browse directories, answer dangerous-class approvals |

`owner` **is not an assignable role**: it is expressed by `workspaces.owner_id` and travels with
the workspace.

An invitation is **per workspace** and never per connection — a connection stands for a whole
machine, and sharing a whole machine must have no entry point at all.

### Sharing a workspace ≈ handing out a shell account

This is not compliance talk, it is this feature's largest risk surface. What gets shared is the
real filesystem and the real shell of a **real machine**. Hence four hard constraints:

1. **The path allowlist is enforced in agitd**, not in the hub.
2. **A dangerous configuration is refused by default**: a session started with
   `--dangerously-skip-permissions` is marked, and nobody but the owner may drive it — that would
   be handing out an approval-free root shell.
3. **Approvals go back to the owner by default**, not to "whoever is driving".
4. **Full audit**: every instruction sent down and every approval decision enters `audit_log`.

The terminal and directory browsing are owner-only: a shell goes around approvals, goes around
the tool allowlist, and enters no session's history.

## 6. Driving rights

The executor serializes session mutations and enforces native writer ownership. A session
controlled by another runtime remains read-only until that owner releases it. Local and
remote controllers share the same ownership checks. Background catalog reads do not acquire
or clear the writer gate. Conflicting writes must not start a second native turn.

## 7. Command table

```text
agit rc start [--detach] [--name <machine>]   start the daemon and enable owner Cloud access
agit rc status                                connection state, uptime, active sessions
agit rc list                                  every machine under your name
agit rc revoke <device-id>                    revoke one Cloud device
agit rc stop                                  stop the daemon
```

Creating, listing, editing and deleting the workspaces themselves happen in the web interface
(`/workspaces`) — they are account-level orchestration operations, belong to no local directory,
and putting them in the CLI would only make people think they have something to do with the
current one.

## 8. Troubleshooting

| Symptom | Cause | What to do |
|---|---|---|
| `path must be shorter than SUN_LEN` | `$AGIT_HOME` is too deep; a unix socket path caps at 108 bytes | it already falls back to `$XDG_RUNTIME_DIR`; if that still fails, move `AGIT_HOME` somewhere shallower |
| `rc start` says it is already running, but `rc status` says it is not | a stale socket file / pidfile | the test is **whether the socket connects**; inspect the private daemon log and use `agit rc stop` before restarting |
| the page says the machine is offline, but local RPC is ready | Cloud enrollment or presence has not completed | check the selected Hub, device enrollment, and timestamped Cloud logs |
| taking over a session is refused with still open in a terminal | that session is open right now, and taking it over destroys both histories | quit in that terminal, then take over |
| binding a folder is refused | the path does not exist, or it is a system root | use a real project directory |
