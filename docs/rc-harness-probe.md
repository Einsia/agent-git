# RC harness probe results

This document holds **measured results** only. Every design decision in `src/rc/harness/`
points back here, so it is not background material — it is the test itself. How to re-measure
sits at the end of each section.

Environment: Ubuntu 24.04 / claude-code **2.1.233** / codex-cli **0.147.0**.

## 0. The conclusion first: why not tmux / Zellij

The easiest thing to reach for in the survey is a terminal multiplexer (tmux, Zellij, and the
tmux-based products in the codelark class). Ruling it out is not a preference; it rests on three
hard facts:

1. **A byte stream has no structure.** Nothing separates a tool call from model prose, an
   approval prompt is a pile of ANSI escape sequences that cannot render as two buttons on a
   phone, and a change cannot be made into a diff.
2. **A byte stream cannot be committed losslessly.** All of agit's value rests on "structured
   events gather into a turn, a turn becomes a commit". A TUI recording can neither be split
   per turn nor drilled into by a merge agent.
3. **The problem it solves is one we do not have.** A multiplexer sells "the session outlives
   the client"; `agitd` holds the harness child process directly, so that holds by construction.

Facts 1 and 2 say "adopting it guts the core of the product", so this is not a trade-off; it is
an exclusion.

## 1. Slash commands work fully under stream-json (decisive)

This is the one hypothesis that could overturn the conclusion above: **if commands like `/goal`
do not work under `-p --input-format stream-json`, the web interface stays a chat box forever
and Zellij has a case**.

Measured: it does not hold. The commands **work as usual**.

### 1.1 The handshake returns a command directory

The `control_response` for `initialize` carries a `commands` array:

```
<< INIT returned 48 slash commands: ['deep-research', 'design-sync', 'dataviz',
   'update-config', 'verify', 'debug', 'code-review', 'simplify', 'batch', ...]
   has /goal? True   has /compact? True
```

The `system/init` frame carries a second copy as `slash_commands`. Both are reachable; we take
the former (`ClaudeCodeDriver::commands`) because it carries `description` and `argumentHint`,
which the web input box uses directly for completion.

### 1.2 A command is an ordinary user message

Send `/context` in as the body:

```
>> {"type":"user","message":{"role":"user","content":"/context"}}
<< TEXT: ## Context Usage  **Model:** claude-sonnet-4-5  **Tokens:** 21.7k / 200k (11%) …
<< RESULT subtype=success is_error=False num_turns=0
```

**`num_turns=0`** is the point: it runs locally and makes no model round trip.

### 1.3 `/goal` holds too

```
>> {"type":"user","message":{"role":"user","content":"/goal"}}
<< TEXT: No goal set. Usage: `/goal <condition>`
<< RESULT subtype=success is_error=False num_turns=0
```

Run it again through our own supervisor (`cargo run --example rc_smoke -- claude-code '/goal'`):

```
  TURN      completed outcome=ok cost=$0.0000
  item.completed 5 · item.delta 0
```

`cost=$0.0000` and not one `item.delta` — a local command, zero model calls.

> **Re-measure**: `python3 probe_slash.py` (see §6).

## 2. `-p` is not a one-shot mode

The `-p` flag is misnamed. With `--input-format stream-json` it is a **resident bidirectional
session**, the same channel the official Claude Agent SDK uses: multi-turn conversation,
mid-turn steering, interrupts, approvals and slash commands all hold inside one process (every
section of this file was produced inside one process).

The real one-shot modes are `claude -p "prompt"` (without stream-json) and `codex exec`, and we
use neither.

## 3. Approvals: `--permission-prompt-tool stdio` is the switch

Without this flag the CLI decides on its own from the local settings and **never asks** — a
shared workspace turns into "allow everything automatically", the most dangerous failure mode.
With it, every tool call sends a control request:

```json
{"type":"control_request","request_id":"55da65ec-…","request":{
  "subtype":"can_use_tool","tool_name":"Write","display_name":"Write",
  "input":{"file_path":"/…/probe-out.txt","content":"hello"},
  "permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],
  "tool_use_id":"toolu_01RDkNymnaJ3G4DJaLnLE9sg"}}
```

The response:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"55da65ec-…",
  "response":{"behavior":"deny","message":"denied by agit rc probe — do not write files"}}}
```

After a denial the model receives a tool_result with `is_error: true`, and then it **explains
why it did not act** instead of silently retrying.

The whole chain measured through the supervisor (`rc_smoke`):

```
  STATUS    awaiting_approval
  APPROVAL  FileChange  Write /tmp/agit-rc-smoke-147073/hello.txt
            → allowed
  STATUS    running
  ITEM      file_edit  line 8  hash 4a2b3dc4e429  Write
```

After approval the file really is persisted (`cat hello.txt` → `hello`).

The CLI judges commands like `echo` safe on its own and allows them without sending
`can_use_tool`. So **"no approval arrived" does not mean "approvals are broken"** — verify with
an action like `Write`.

### 3.1 The permission mode can change mid-session (measured on claude-code 2.1.237)

The legal values of `--permission-mode` are `acceptEdits` / `auto` / `bypassPermissions` /
`manual` / `dontAsk` / `plan`. `default` is **absent from the help listing and still
accepted** — the process starts normally and `system/init` reports `permissionMode: default`.

Switching mid-session goes through a control request:

```json
>> {"type":"control_request","request_id":"mode-1",
    "request":{"subtype":"set_permission_mode","mode":"auto"}}
<< {"type":"control_response","response":{"subtype":"success",
    "request_id":"mode-1","response":{"mode":"auto"}}}
```

**It takes effect on the spot**, and even the `system/init` that follows already reports the
new value. Controlled runs against the same prompt (create a file with Write):

| Starting mode | Switched mid-run to | `can_use_tool` count | File persisted |
|---|---|---|---|
| `default` | — | **1** | yes (after approval) |
| `default` | `acceptEdits` | **0** | yes |
| `default` | `auto` | **0** | yes |

The `permission_suggestions` entry in an approval request is the terminal's "don't ask again"
option:

```json
"permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}]
```

Putting it into the response's `updatedPermissions` **unchanged** is the same as choosing it.
So the driver stores the suggestions each request brings instead of assembling one itself —
that format is private, and a guessed one drifts sooner or later.

> Another trap: `system/init` appears only **after the first user message**; the handshake
> response comes first. Any script that waits for init before working hangs there (the probe
> script hung exactly this way), so the driver's tailer must not wait for it either — see
> "rebuild the tailer only when the path changes" in `supervisor`.

## 4. An interrupt really interrupts

```
>> {"type":"control_request","request_id":"int-1","request":{"subtype":"interrupt"}}
<< {"type":"control_response","response":{"subtype":"success","request_id":"int-1",
    "response":{"still_queued":[]}}}
<< {"type":"user","message":{"role":"user","content":[{"type":"text",
    "text":"[Request interrupted by user]"}]}}
```

A turn running `sleep 30` is interrupted at once, the transcript keeps a
`[Request interrupted by user]` marker line, and the process exits non-zero. The driver reads
that line of text to judge the turn `Interrupted`.

## 5. Mid-turn steering: claude-code delivers at the tool boundary (this must reach the protocol)

Have it run `sleep 30`, and four seconds in write another user message to stdin:

| Time | What happens |
|---|---|
| 3.81 s | the first message echoes back; the model issues `Bash(sleep 30)` |
| 4.00 s | the second user message is written to stdin, **accepted**, no error, no new process |
| 35.06 s | `tool_result` returns; the second message **appears in the stream only at that moment** |
| 36.60 s | the model answers the new instruction, `num_turns=2` — inside the **same turn**, not a new one |

That is: **accepted immediately, delivered at the tool boundary, counted in the current turn**.
codex's `turn/steer` is a protocol-level verb and semantically more immediate.

This difference must not be hidden: the `turn.steer` response carries a `delivery` field
(`immediate` / `at_tool_boundary`), and the frontend shows "queued, delivered once the current
tool finishes" from it. Not showing it leaves a user on a weak network thinking the message was
lost, who then sends it again and again.

## 6. The codex app-server protocol table (exported from the binary, not read off the docs)

```bash
codex app-server generate-json-schema --out schema/
codex app-server generate-ts          --out ts/       # 642 .ts files
```

The key verbs it yields:

* client→server: `initialize`, `thread/start`, `thread/resume`, `thread/fork`,
  `turn/start`, `turn/steer`, `turn/interrupt`, `thread/compact/start`,
  **`thread/goal/set` / `thread/goal/get` / `thread/goal/clear`**, `fs/readDirectory` ...
* server→client (must be answered): `item/commandExecution/requestApproval`,
  `item/fileChange/requestApproval`, `item/permissions/requestApproval`
* notifications: `thread/started`, `turn/started`, `turn/completed`, `item/started`,
  `item/completed`, `item/agentMessage/delta`, `item/reasoning/textDelta`,
  `item/commandExecution/outputDelta` ...

On the codex side `/goal` is a **first-class RPC method**, not a slash command.

An approval decision is not a boolean:

```ts
type CommandExecutionApprovalDecision =
  "accept" | "acceptForSession"
  | { acceptWithExecpolicyAmendment: {…} }
  | { applyNetworkPolicyAmendment: {…} }
  | "decline" | "cancel";
```

### 6.1 Trap: codex does not send the `jsonrpc` field

The opening comment of `codex-rs/app-server-protocol/src/rpc.rs` is blunt about it:

> We do not do true JSON-RPC 2.0, as we neither send nor expect the
> `"jsonrpc": "2.0"` field.

The consequence is that **a strict JSON-RPC library cannot read codex**: the
`Request`/`Notification` of `jsonrpsee-types` carry a mandatory `TwoPointZero` and meet a codex
frame with a flat `missing field 'jsonrpc'`. So the codex driver hand-writes a lenient parse,
while the link between **us** and the hub still demands `"2.0"` strictly (both ends are ours, so
there is no reason to loosen it).

## 7. Two sources, one truth

The driver's stdout and the harness's own transcript file describe the same session, but they
are good at different things:

| | stdout | transcript file |
|---|---|---|
| token deltas | yes | no |
| approvals / interrupts / turn outcome | yes | no |
| **byte-identical to what `agit commit` stores** | **no** | **yes** |

So both are used, each with its own job, never competing:

* stdout drives the **ephemeral** events: `item.started` / `item.delta` / `turn.*` / `approval.request`, none of which is ever persisted.
* The file drives **`item.completed`** — the only event the hub persists and the web interface
  renders permanently. Every line is parsed by the **same** `adapter::parse` (the one `agit show`
  uses) and carries `transcript::object_hash`, the hash the envelope of the future commit will
  carry.

A test pins that last point (`supervisor::tests::live_object_hash_equals_the_committed_envelope_hash`):
the hash of that line in the live stream is byte-identical to the envelope hash
`transcript::wrap_lines` packs. This is why the hub can check its projection against pushed
history line by line, and why "talked about on the web but absent from `agit log`" is
structurally impossible.

## 8. How to re-run

```bash
# End to end (really starts a claude process and prints every protocol frame)
cargo run --example rc_smoke                                   # default: run one Bash
cargo run --example rc_smoke -- claude-code '/goal'            # slash command, zero model calls
cargo run --example rc_smoke -- claude-code 'Use the Write tool to create hello.txt …'  # approval round trip
cargo run --example rc_smoke -- codex 'say hi'                 # codex side (needs codex login first)

# Unit tests
cargo test --lib rc::
```

The probe scripts (`probe_slash.py` / `probe2.py`) stay in the scratchpad and are the raw source
for §1–§5 above; they do not depend on this repository and run standalone.
