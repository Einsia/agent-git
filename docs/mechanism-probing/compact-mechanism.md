# The compact mechanism in two runtimes (observed)

Written 2026-07-28. **The conclusions are drawn from real session files, not inferred from
documentation.**

## The conclusion in one sentence

**compact in both runtimes only appends; it never deletes the original text on disk.** What
compact changes is "the context window sent to the model"; the transcript file is strictly
append-only.

## Why this note exists

The earlier implementation rested on a wrong premise: "when a long task hits the context limit
the runtime rewrites the file, compresses the first half away, and the original text is gone for
good". The whole PreCompact rescue mechanism, the `<id>.rN.jsonl` revision copies, and three
user-facing warnings saying "Codex is not lossless" were all derived from that premise.

That premise came from an old design document (`docs/plans/2026-07-17-agent-identity-and-handoff-design.md`
says "agit should snapshot before compaction rather than let a session lose detail"), and
**I implemented it without verifying it**. Observation overturned it.

## Evidence

Sample: `~/.codex/sessions/2026/07/17/rollout-…-019f7132-….jsonl` — 786 MB, 9441 lines, ran for
37 hours, compacted 11 times.

Sampling widened to 9 sessions over 2 MB (one of them compacted 40 times):

| compactions | timestamps going backwards | user input from before the first compact |
|---|---|---|
| 19 | 0 | still there |
| 40 | 0 | still there |
| 12 | 0 | still there |
| ... all 9 samples | **all 0** | **all still there** |

Zero backward timestamps = the file is appended strictly in time order, with no stretch of it
rewritten.

## Codex compact: lossless filtering

One compact appends **five** records:

```
compacted                       ← the real payload
world_state                     ← environment snapshot (cwd / filesystem / agents_md)
turn_context                    ← a new turn
event_msg/token_count
event_msg/context_compacted     ← only a notification for the UI
```

### The trap: `compacted` is a top-level type

The `type` of `compacted` sits at the **top level**; its payload has **no** `type` field. The
first probe script filtered on `payload.type` and therefore saw only the last record, the UI
notification (`{"type":"context_compacted"}`, whose payload holds only type and no body), and
wrongly concluded that "compact carries no summary body".

### The structure of a `compacted` record

```json
{
  "type": "compacted",
  "timestamp": "2026-07-17T18:11:47Z",
  "payload": {
    "message": "",
    "replacement_history": [ /* structured message array, see below */ ],
    "window_number": 1,
    "window_id": "019f7156-5f95-7621-b4ae-4f891e3d338b",
    "previous_window_id": "019f7132-ab8e-7dc1-8031-76e32c862dbc",
    "first_window_id": "019f7132-ab8e-7dc1-8031-76e32c862dbc"
  }
}
```

### `replacement_history` is a filtered result, not a summary

**This is the most important finding.** It is a structured message array, and the user input
inside it is **kept verbatim** (12/12 exact matches, after normalizing whitespace). The user text
below is captured transcript evidence, quoted in its original language:

```json
[
  {"type":"message","id":"msg_…","role":"user",
   "content":[{"type":"input_text","text":"了解一下这个项目跟 Codex Session …"}],
   "internal_chat_message_metadata_passthrough":{"turn_id":"…"}},
  … 
  {"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>…"}]},
  {"type":"compaction"}                      ← the marker at the end of the array
]
```

Kept vs discarded (window 11 of the 786 MB session):

| Kept (17) | Discarded |
|---|---|
| every user message, verbatim | 434 assistant messages |
| 3 developer instructions | 1588 tool calls |
| one trailing `type:"compaction"` | 1500 reasoning records |

### The window chain is traceable

`window_number` climbs monotonically 1→11, `previous_window_id` points at the previous window,
and `first_window_id` always points at the first. The whole chain is traceable end to end.

Window contents grow with the session (the user says more and more):

```
windows 1-4:   9 (user 5 + developer 3 + compaction 1)
windows 5-8:   13 (user 9 + developer 3 + compaction 1)
window 9:      15
windows 10-11: 17 (user 13 + developer 3 + compaction 1)
```

## Claude Code compact: lossy summarization

Appends **one** synthetic user message:

```json
{
  "parentUuid": null,
  "isSidechain": false,
  "userType": "external",
  "cwd": "/mnt/…/ALE-Synthetic",
  "sessionId": "0653a7e1-a05f-4a45-bcbd-e5e6ba0d3b45",
  "version": "2.1.220",
  "type": "user",
  "message": {
    "role": "user",
    "content": "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.…"
  },
  "isCompactSummary": true,
  "uuid": "f6f7a17e-…"
}
```

Five points:

1. **`isCompactSummary: true` is the only reliable test.**
2. `type` and `message.role` are both `"user"` — it looks like a user prompt, but no person
   typed it.
3. `content` is a **plain string** (not an array of blocks), observed at 40405 characters.
4. `parentUuid: null` — the message chain is reset here. It **cannot** be used as the test: the
   first message of a session is `parentUuid: null` too.
5. **The body format is not stable**: of two samples, one uses `Summary:` and the other wraps the
   body in a `<summary>` tag. So only the field can be recognized; never decide from the body
   text.

Sample: `~/.claude/projects/-mnt-…-ALE-Synthetic/1480da41-…/subagents/agent-a6ec4cc063fc2ec9c.jsonl`
(95 lines, the summary at line 90, all 87 real messages before it still present, the earliest
still at line 1).

The marker names `compact-boundary` and `preCompact` **do not exist** (searched globally, 0
hits).

## The two side by side

| | Codex | Claude Code |
|---|---|---|
| where the payload sits | `compacted.payload.replacement_history` | a user message carrying `isCompactSummary` |
| shape | structured message array | one block of text |
| user input | **kept verbatim** | summarized away |
| traceable | window chain (`previous_window_id`) | `parentUuid: null` breaks the chain |
| original text on disk | kept | kept |
| nature | **lossless filtering** | **lossy summarization** |

The difference has a practical consequence: the user's full chain of intent can be rebuilt from
Codex's `replacement_history`, and **cannot** be rebuilt from Claude Code's summary.

## How agit models it

`EventKind` splits into two variants (`src/adapter/mod.rs`):

```rust
CompactFiltered   // Codex: lossless filtering
CompactSummary    // Claude Code: lossy summarization
```

Two variants rather than one `lossy: bool`, so that `match` forces every consumer to handle both
cases explicitly — the compiler makes you think through whether to deduplicate, how to render, and
whether it can be fed to merge.

Three rules:

1. **`gist()` skips both.** Otherwise a resumed session shows "This session is being
   continued..." in `agit log` instead of the first thing the user actually typed — which
   destroys exactly what `agit log` is for (recognizing which stretch of work this is from its
   opening prompt).
2. **The body of `replacement_history` does not enter the IR.** Those messages already appear
   verbatim in the transcript before this record; putting them into the IR again multiplies
   duplicate hits in `counts()` and in search (a sentence compacted 11 times is counted 12
   times). The IR carries one boundary event plus the window number, nothing more.
3. **A compact boundary is not re-emitted into the target runtime.** Writing it out as a user
   message during a cross-runtime conversion makes the restored agent believe it is continuing a
   session that does not exist.

`is_lossy_summary()` is for `merge`: Codex's filtered result can go straight in as input, Claude
Code's summary cannot (it would take the summarization for the user's original intent).

## Another bug found along the way: Codex's synthetic user messages

The same class of problem. Codex packs a large volume of runtime injection into `role=user` and
sends it to the model. In the 786 MB session, of 219 `role=user` messages **only 16 were typed
by a person**:

| count | content |
|---|---|
| 199 | `<codex_internal_context source="goal">` goal reminder |
| 3 | `<environment_context>` environment block |
| 1 | `# Files mentioned by the user:` attachment list |
| **16** | **real user input** |

The consequence of not telling them apart: `gist()` picks up
`<environment_context> <cwd>/mnt/…`, and `counts().prompts` is inflated 13-fold.

The test matches on a **known prefix** (`is_synthetic_user_text`), not on "anything starting
with `<`" — a user may perfectly well paste an HTML/XML fragment. Better to miss a new injection
shape (the count runs a little high) than to misjudge real user input as synthetic (that would
leave `agit log` unable to recognize this stretch of work).

Separately, `event_msg/user_message` is exactly those 17 human inputs and can serve as a
cross-check source (Codex writes both sides: `event_msg` for the UI, `response_item` for the
model). The current implementation does not use it, because whether it exists under `codex exec`
non-interactive mode is unverified.

## Where the correction landed

> This table records the change sites **at the time**. The store has since become link-only and
> `agit init` and `hook/` were deleted outright, so `hook/codex.rs`, `commands/init.rs` and
> `RewriteDetected` no longer exist. The observed conclusions above are unaffected.

| site | what changed |
|---|---|
| `hook/codex.rs::has_precompact_gap()` | always true → false; deleted the test that pinned the wrong conclusion |
| `commands/{init,status,doctor}.rs` | three misleading warnings become conditional (the switch stays) |
| `adapter/mod.rs::EventKind` | added `CompactFiltered` / `CompactSummary` |
| `adapter/mod.rs::gist()` | skips compact boundaries |
| `adapter/mod.rs::Counts` | added a `compactions` field, not counted into `prompts` |
| `adapter/claude_code.rs::is_compact_summary()` | recognizes `isCompactSummary` |
| `adapter/codex.rs` | recognizes top-level `compacted` records + `is_synthetic_user_text()` |
| `ui/transcript.rs` | compact boundaries collapse to a one-line separator |
| `domain/capture/mod.rs` | `RewriteDetected` now means an integrity anomaly |

## Not yet verified

* **Whether Codex's `replacement_history` is what actually goes to the model** (rather than only
  a persisted record). Only packet capture proves that — it means pointing the codex config at a
  proxy, which would record real API requests, so it was not done. The persisted record already
  carries every conclusion above.
* Whether `event_msg/user_message` exists under `codex exec` non-interactive mode.
* The compact shape of a Claude Code main session (not a subagent). `isCompactSummary` was found
  only under `subagents/`, never in a main-session sample — either those sessions never reached
  the context limit, or the main-session shape differs.

## Scripts used to reproduce this

All under `/tmp` (they get cleaned out; copy them into `notes/` to keep them):

* `probe-compact.py` — 6 records either side of a compact marker
* `probe-compacted-record.py` — dissects the `compacted` record and `replacement_history`
* `probe-replacement.py` — what was kept, what was discarded
* `verify-compact.py` — multi-file sampling plus a strict check of verbatim retention
* `probe-synthetic.py` — composition breakdown of the synthetic user messages
* `/tmp/real-check/` — runs agit's adapter over real files, checking gist / counts / no leakage
