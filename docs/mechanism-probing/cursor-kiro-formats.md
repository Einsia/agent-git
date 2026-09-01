# Cursor and Kiro transcript formats, and model attribution across the four runtimes

> Observed record + implementation spec. The forensic material is in `samples/`; the index is
> `samples/README.md`. Evidence grading follows `compact-mechanism.md`: **observed** /
> **documented** / **inferred** (with a confidence level).
>
> Sampled from this machine's macOS (Cursor IDE 3.13.25, Claude Code 2.1.197/2.1.219, Codex
> 0.146.0-alpha.3.1) and `nana-data@remote-host` (Claude Code 2.1.219/2.1.220, Codex 0.144.5,
> kiro-cli 2.14.1).

## 0. The conclusion in two sentences

**Cursor is readable, not writable.** `agent-transcripts/**.jsonl` is a projection, not the
source of truth; the truth sits in a 4.7 GB `state.vscdb` holding opaque protobuf and encryption
key fields, and this machine has no resumable CLI entry point. The adapter implements `parse`;
`install` errors out.

**Kiro is located, but the sample is too thin to finalize.** The paths and the resume command are
exact, and the full variant set of `LogEntryV1` was dug out of the binary's symbols, but what is
in hand is 2 sessions holding 4 records; `ToolResults` / `Compaction` / `ResetTo` have never been
seen. **Collect the data before writing the code.**

---

## 1. Cursor

### 1.1 Paths and reverse lookup

```text
main session   ~/.cursor/projects/<slug>/agent-transcripts/<uuid>/<uuid>.jsonl
subagent       ~/.cursor/projects/<slug>/agent-transcripts/<parent>/subagents/<sub>.jsonl
```

**One directory per session, with the jsonl named after the directory** — not
`agent-transcripts/<uuid>.jsonl`. This machine holds 60 main-session directories and 44 subagent
files.

The `slug` rule (**observed** over 10+ samples): `store::slug_for(cwd)` then
`trim_start_matches('-')`. It differs from Claude Code by one leading character:

```text
/Users/nana/Projects/AgentGit
  Cursor        Users-nana-Projects-AgentGit
  Claude Code  -Users-nana-Projects-AgentGit
```

Three kinds of slug **do not reverse into a cwd**, and `all_sessions()` must tolerate them
without pretending it can: `empty-window` (no folder opened), a bare-numeric workspace id, and
`var-folders-…` (a temp directory).

**The same session id appears under two slugs with diverging content** (**observed**: the same id
has 75 lines under `Users-nana-Projects-AgentGit` and 56 under `empty-window`, and neither is a
copy). So `resolve` goes straight at the slug for the cwd and falls back to a glob only on
failure, taking the newest mtime on that fallback. The ambiguity must be visible in `doctor`. The
cause is undetermined; see §4.

**Do not introduce an index database.** Cursor already splits by project directory (**observed**
on a warm cache: a direct stat at 0.009 ms, a one-level glob at 0.554 ms, listing all 105
transcripts at 5.018 ms), not the same order of magnitude as Codex scanning 18745 files.

### 1.2 Record schema

Only two top-level shapes exist (**observed** over 104 files and 5335 records,
`samples/cursor/record-histogram.txt`):

```text
5335  {role, message}          message has a single key, content, always an array
  48  {type:"turn_ended", status}
  10  {type:"turn_ended", status, error}
```

The array holds three kinds of block: `assistant`'s `{type:"text", text}` 3429 times and
`{type:"tool_use", name, input}` 5042 times, and `user`'s `{type:"text", text}` 475 times.

**Four absences decide what this adapter can do**: no `tool_result` (not one line of tool output
is persisted), no thinking block, no timestamp field, no `sessionId` / `cwd` / model.

Time comes only from the `<timestamp>Friday, Jul 31, 2026, 11:12 PM (UTC+8)</timestamp>` inside
the user body — a localized human-readable string, not ISO8601. An assistant record carries no
time information whatsoever.

`turn_ended` **appears only from 2026-07-30 on** (31 of the 105 transcripts have it), so it is an
optional field. A difference along the time dimension within one host is more dangerous than a
cross-host one, because it never surfaces when the test moves to another machine.

### 1.3 Mapping to the IR

Everything maps, **with no need to extend `EventKind`** — a Cursor transcript is itself a
projection meant "for the agent to read", and its shape is already the IR.

Two judgment calls:

**`FileEdit` switches to an allowlist of write tools.** The existing CC adapter's "a path means
an edit" heuristic inflates badly on Cursor: `Read` carries a `path` 2050 times, while real
writes number 1252 (`StrReplace` 709 + `Write` 338 + `ApplyPatch` 179 + `Delete` 26). Decide on
`{Write, StrReplace, Delete, ApplyPatch}`; everything else carrying a path records as `ToolUse`
but still fills `paths`. Cursor's tool names are a stable built-in set, not open the way CC's MCP
tools are, so an allowlist works here.

**A failed `turn_ended` still counts as `TurnEnd`.** `error` / `aborted` both mean "this turn
grows no further". The interruption itself has diagnostic value the IR cannot express; put it in
`Event.text` — it is not worth extending the IR for.

### 1.4 Recognizing injected messages

**This is where Cursor needs the most care: an injection and real human input share the same
`<user_query>` wrapper**, so the strategy the two existing runtimes use — match the whole body
against a known prefix — fails outright here. Two layers are required, and the order cannot be
reversed.

**Layer one strips the wrapper.** Take the content between `<user_query>…</user_query>` as the
candidate; lift the time out of `<timestamp>` as the event's `timestamp` (**this is Cursor's only
time source — do not drop it**); carry `<image_files>` / `<external_links>` / `[Image]` as
annotations. A record with no `<user_query>` at all (1 of 475) is the candidate whole.

**Layer two decides injection from the inner text.** Four kinds are observed
(`samples/cursor/synthetic-user-text.txt`):

| Count | Prefix | What it is |
|---|---|---|
| 12 | `Perform any necessary follow-up actions in response to the subagent completion above.` | wakes the parent agent after a subagent finishes |
| 9 | `Briefly inform the user about the task result and perform any follow-up actions` | wakes after a background task finishes |
| 2 | `You are the forked subagent; continue executing your task.` | the self-fork opening line |
| 1 | `Your previous response was interrupted. Continue from where you left off.` | picking up after an interruption (**no `<user_query>` wrapper**) |

**Repetition is not a signal of synthesis.** Write this into the comment: the natural fallback
idea is "the same sentence appearing more than once is an injection", and observation falsifies
it — real human input repeats heavily too (one sentence appears 4 times), because Cursor's fork /
multitasking **copies** the parent session's history into the child session's transcript.

`Start multitasking` (5 times) cannot be decided: it comes from a UI button the user clicked, it
reads like a command, and its origin counts as user intent. Leave it out of the injection list —
a miss only inflates a count, while a false positive erases real human input.

`<module>` appears twice, inside a Python traceback the user pasted — **not a tag**. That is
exactly why a generalized rule like "anything starting with `<` is an injection" cannot be used.

### 1.5 Compaction and thinking

**No compaction boundary marker is observed.** Across the whole `state.vscdb`,
`summarizedComposers` has 0 non-empty rows; the 4 sessions with the highest
`contextUsagePercent` (93.8% / 91.5% / 87.8% / 87.2%) each carry a transcript whose shape is
identical to an ordinary session's. At the field level there are traces of the mechanism
(`speculativeSummarizationEncryptionKey` is a base64 key). **Inferred (medium)**: the compaction
product is encrypted into `state.vscdb` and leaves no trace in the transcript. The consequence is
that `counts().compactions` is always 0 on the Cursor side, and that is not a bug.

**Thinking is entirely absent from the transcript.** Where it actually lands is the bubbles in
`state.vscdb`: 28373 of 140649 carry `thinking = {text, signature}`. `text` is a plaintext
summary, `signature` an opaque multi-line base64. **Keep it out of the IR uniformly** (not even
an `Other`, since the transcript cannot see it at all). `allThinkingBlocks` is an empty array
everywhere in the database; do not count on it.

### 1.6 Why it is not writable

Four independent pieces of evidence:

1. **The transcript is a projection.** One session's transcript has 75 lines against 154 bubbles
   in `state.vscdb`. Tool results, thinking, checkpoints, diffs and token counts are all lost.
2. **The truth is not writable.** `composerData.conversationState` is an opaque base64 protobuf
   under a `~` prefix, alongside `blobEncryptionKey` / `speculativeSummarizationEncryptionKey`.
   Building a session Cursor recognizes means writing, correctly and together, one
   `composerHeaders` row + `composerData:<id>` + N `bubbleId:…` entries, and forging
   `conversationState` on top. That is not on the order of "change a few ids".
3. **There is no resumable CLI.** `/usr/local/bin/cursor` is a VS Code-style launcher,
   `~/.cursor/cli/` holds only tunnel-related files, and `cursor-agent` is not installed.
4. **The IDE is holding that database** (`-wal` and `-shm` are present).

The worst failure mode is "the command returns success and the user opens Cursor to nothing", so
the refusal must land **before any work starts**, not after the files are written.

---

## 2. Kiro

Kiro CLI is a renamed Amazon Q Developer CLI (the same host has `~/.amazon-q.dotfiles.bak`, and
the crate name inside the binary is `chat_cli`).

```text
~/.kiro/sessions/cli/<uuid>.jsonl     event log (the transcript itself)
~/.kiro/sessions/cli/<uuid>.json      sidecar state snapshot (model, cwd, per-turn metadata)
~/.kiro/sessions/cli/<uuid>.history   readline history
~/.local/share/kiro-cli/data.sqlite3  both session tables are empty; not an index source
```

**Reverse lookup is the simplest of the four**: `resolve(id)` stats directly, O(1);
`sessions_for(repo)` readdirs and then reads the top-level `cwd` out of each `<id>.json` — the
`.json` is only 5.5 KB and a separate file, so reading it never touches a `.jsonl` that may run
to tens of MB, which is cheaper than Codex's `read_head(8192)`.

Do not use `kiro-cli chat --list-sessions`: it filters by the current cwd, and it forks a 695 MB
process (**observed** at about 5 s).

### 2.1 Record schema

A serde adjacently-tagged enum:

```json
{"version":"v1","kind":"Prompt","data":{
  "message_id":"f3dacb75-…","content":[{"kind":"text","data":"hi"}],
  "meta":{"timestamp":1785003130}}}
```

The block is `{kind, data}`, not `{type, text}`. `meta.timestamp` is Unix seconds and **exists
only on `Prompt`**.

**The observed sample is 4 records** (2 sessions, both greetings, zero tool calls). The full
variant set was dug out of the serde literals in the `kiro-cli-chat` binary (unstripped, carrying
debug_info) (**inferred, high confidence**, `samples/kiro/logentry-variants-from-binary.txt`):

`Prompt` / `AssistantMessage` (with two sub-variants, `Response` and `ToolUse`) / `ToolResults`
/ `Compaction` / `ResetTo` / `Cancelled` / `PromptClear`.

### 2.2 `ResetTo` is the only one that does not map cleanly

It turns the log into an event stream that **needs an apply pass before it yields the current
state** (the binary has `LogEntry::apply`), rather than a conversation record that reads in
order. Some records ahead of a `ResetTo` are semantically withdrawn by the user while their bytes
stay in the file.

**This is a substantive problem for agit**: a naive sequential parse counts history the user
explicitly rejected into the snapshot.

Run a pre-pass apply inside `parse` — scan out every `ResetTo` first, mark the skips from
`effectiveFromMessageId` against `message_id`, then run the normal mapping. The IR stays clean
and semantic correctness is settled inside the adapter, the same move Codex makes by keeping
`replacement_history` bodies out of the IR. **But no sample of the exact `ResetTo` payload
exists, so this is design intent, not an implementable spec. It is the largest gap in this
document.**

### 2.3 The turn-termination signal is the most complete of the four

But it sits in the `.json`, not the log (**observed**, `samples/kiro/session-2293dba9.state.json`):

```json
"user_turn_metadatas": [{
  "message_ids": ["f3dacb75-… (Prompt)", "fa147fee-… (AssistantMessage)"],
  "end_reason": "UserTurnEnd",
  "end_timestamp": "2026-07-25T18:12:13.424416722Z",
  "turn_duration": {"secs": 2, "nanos": 762278439},
  "context_usage_percentage": 1.7728001, "user_prompt_length": 2
}]
```

Structured, carrying in-turn statistics, and `message_ids` points back at log records — stronger
than Codex's `task_complete`.

**It settles injection recognition as a side effect**: `user_turn_metadatas` counts human turns
and nothing else, so "a `Prompt` whose `message_id` appears in some `message_ids` is human input"
is far more reliable than guessing prefixes. (**Inferred, medium confidence**, verified on one
turn only.) Kiro does inject — the binary carries `SessionTool::InjectContext`, `SteerMessage`
and `<goal>` symbols — the shape is simply unknown.

### 2.4 The compaction direction is unknown; treat it as lossy for now

`LogEntryV1::Compaction` exists and `CompactStrategy` is referenced 29 times, but the sample
sessions reach only 1.77% `context_usage_percentage`, nowhere near compaction.

**Map it to `CompactSummary` for now** (`is_lossy_summary()` returns true, so `merge` will not
take it as the user's original intent), with a comment saying so as the conservative assumption.
The two ways of being wrong do not cost the same: treating lossless as lossy only forgoes one
usable input, while treating lossy as lossless poisons what `merge` concludes.

### 2.5 install is marked experimental

The resume command is exact (**documented**): `kiro-cli chat --resume-id <ID>`. But the amount of
rewriting sits between Claude Code and Codex and is fussier — every `message_id` in the `.jsonl`
must stay consistent with `user_turn_metadatas[].message_ids` in the `.json`, which is the one
cross-file reference.

Three unverified questions all need write access to settle: whether the `.json` is required or
can be rebuilt from the event stream, whether `session_state`'s serde is strict, and whether
`--resume-id` validates the cwd.

**Treating it as usable before that verification produces the worst failure mode: "it looks like
it succeeded, and the resume is an empty session".**

---

## 3. Model and reasoning-effort attribution

### 3.1 Where it lands

| Runtime | Field path | Granularity | Effort |
|---|---|---|---|
| Codex | `turn_context.payload.{model,effort}` | per turn | a separate field |
| Claude Code | `message.model` + a **top-level** `effort` | per assistant record | a separate field (newer versions only) |
| Cursor | absent from the transcript; the `state.vscdb` bubble's `modelInfo.modelName` | per bubble | **baked into the slug** |
| Kiro | `rts_model_state.model_info.model_id` in `<id>.json` | per session | **missing** |

Real values (**observed**, `samples/model-attribution/`):

```text
Codex         gpt-5.6-sol·ultra   gpt-5.5·xhigh   codex-auto-review·low
Claude Code   claude-opus-5·xhigh   claude-opus-5-thinking (2.1.219 bakes the effort into the slug)
Cursor        claude-opus-4-8-thinking-max   claude-4.6-opus-high-thinking
              gpt-5.5-extra-high-fast   default
Kiro          claude-opus-5   auto (the default; the real model is unknowable)
```

**Do not enumerate the efforts and do not normalize the slug.** `effort=ultra` is outside the
common low/medium/high/xhigh set; Cursor has three different spellings (`-thinking-<effort>` /
`-<effort>-thinking` / `-<effort>`); Claude Code moved `-thinking` out of the slug into a
separate field between 2.1.219 and 2.1.220. Any parsing rule guesses wrong on the next new slug.
**Store it unchanged and let the display layer print it directly.**

Three traps:

- **Codex's `session_meta.payload` has no `model`**, only `model_provider`. "Read the file header
  and you know the model" does not hold on Codex. But the index database's `threads` table **has
  both a `model` and a `reasoning_effort` column** (79% / 75% filled); use it on the listing
  path — 0.4 ms against opening a file that may be 786 MB.
- **Cursor's own two databases disagree on slug granularity**: `ai_code_hashes` records
  `claude-opus-5-thinking-high` while the bubble `modelInfo` for the same sessions records a bare
  `claude-opus-5`. A slug taken from `modelInfo` may already have lost its effort suffix.
- **Remote Cursor attribution is missing entirely**: the remote host has transcripts but no `.vscdb`
  anywhere under `~/.cursor-server`. **The transcript is on machine A and the model information
  is on machine B**, and no design gets around that.

### 3.2 IR changes

```rust
pub struct Model {
    /// The raw slug the runtime wrote to disk, with no normalization applied.
    pub slug: String,
    /// Reasoning effort, **filled only when the runtime records it in a separate field**. One
    /// already baked into the slug is not repeated here — otherwise the render carries the
    /// redundancy `claude-opus-4-8-thinking-max · max`.
    pub effort: Option<String>,
}

// Event gains model: Option<Model>, Turn gains models: Vec<Model>, deduplicated in first-seen order
```

**It must sit on `Event`, not `Session`**: 4 sessions are observed switching among 3 models
inside one composer (`claude-4.6-opus-high-thinking` → `gpt-5.3-codex` → `claude-4-sonnet`).
Putting it at session level is a lie. Nor can it sit on `Turn` alone: one turn can carry several
models (Codex's `codex-auto-review` runs at `effort=low` inside the same session, and CC's
subagent records interleave with the main agent's in one file), and forcing it down to turn level
makes an irreversible choice inside `parse`.

**`normalize()` must not touch `model`, and a regression test must pin that.** The turn hash buys
comparability across people and across runtimes exactly by stripping machine identity and
timestamps; folding the model back in throws half of that away — the same conversation run on a
different model no longer hashes the same, and `fork_point` fails immediately. Integrity is the
snapshot id's job, since it hashes the raw bytes.

### 3.3 Rendering in `agit log`

What is rendered is **the union of models over the newly added turns**, not the whole session:

```text
agit-3f9c8a…  +2 turns  claude-opus-5·xhigh          fix the photo thumbnail rotation bug
agit-5b02d1…  +3 turns  claude-opus-5·xhigh +1       rework the payment module error handling
agit-c14a77…  +2 turns  —                            (Cursor, no attribution in the transcript)
```

With several models, print the one with the most events plus `+N` and leave the full set to
`agit show`. **With no attribution, print `—` rather than leaving it blank** — blank reads as a
rendering bug. `--no-model` turns it off: the core value of `agit log` is recognizing a stretch
of work by its opening prompt, and the model must not crowd out the gist.

---

## 4. Trait changes required (the minimal set)

**`parse_at(&self, path: &Path)`**, whose default implementation reads the file and then calls
`parse`. Kiro needs it to read the sidecar `.json`; Cursor needs it too — the transcript has
neither `sessionId` nor `cwd`, so both come from the path. The other two runtimes change nothing.

**`installable() -> bool`**, defaulting to `true`, overridden to `false` by Cursor. It lets the
layer above refuse before any work starts.

**`infer_runtime` learns two new formats.** Kiro is `{version, kind, data}`; Cursor is
`{role, message}` with **no** `sessionId`/`parentUuid` — so its test must be ordered after Claude
Code's. It also scans the first few non-empty lines instead of only the first (Cursor's first
line may be a `turn_ended`).

**`is_cross_vendor` is renamed `needs_ir_roundtrip`.** What it answers is "is this the same
runtime"; "vendor" never fit, and across four runtimes it actively misleads: CC / Kiro / Cursor
can all run `claude-opus-5` (same vendor, different runtime, still needs an IR round trip), and
one Cursor session can switch from `claude-4.6-opus-high-thinking` to `gpt-5.3-codex` (different
vendor, same runtime, still a byte rewrite). With an orthogonal `installable(to)` beside it, the
decision for all 12 combinations is two independent checks.

## 5. Cross-runtime matrix

| from ↓ / to → | claude-code | codex | cursor | kiro |
|---|---|---|---|---|
| claude-code | — | works | **refused** | experimental |
| codex | works | — | **refused** | experimental |
| cursor | works | works | — | experimental |
| kiro | source side unverified | source side unverified | **refused** | — |

Counterintuitively, **Cursor is a cleaner source than Claude Code**. Its transcript has already
stripped reasoning, tool results and vendor encodings, what remains is exactly the shape of the
IR, and the tool `input` even carries absolute paths — more complete than Codex's
`patch_apply_end`. Everything lost was never on disk to begin with (tool results, thinking,
assistant timestamps, model attribution, and pasted images that become dead links off-machine).

## 6. What could not be settled

| Question | Impact | How to settle it |
|---|---|---|
| Kiro's `ToolResults` / `Compaction` / `ResetTo` / `Cancelled` / `PromptClear` payloads | half the mapping rules rest on inference; getting `ResetTo` wrong emits history the user rejected | run a session on the remote host that does real work (tool calls, `/clear`, an interruption), then push it to compaction with `--model deepseek-3.2` (164 K context) |
| whether Kiro's `.json` is required for install | decides how hard reinstalling is | build a session holding only a `.jsonl` and try `--resume-id` |
| whether an explicit Kiro `--effort` persists | decides whether effort attribution exists at all | run `--effort xhigh` once and look at `additional_fields` |
| Cursor's compaction boundary | whether `compactions` always being 0 means "it never happened" or "it is never marked" | push one session to 100% context; the highest of this machine's 562 is 93.8% |
| the cause of one Cursor id diverging across two slugs | `resolve` taking the newest mtime is a guess | move an agent inside a clean profile and diff |
| Codex's `inter_agent_communication_metadata` | the existing adapter silently skips it | dissect the two on the remote host |

## 7. Landing order

1. the trait changes `parse_at` + `installable()` + `infer_runtime` (pure addition for the existing two)
2. the `Model` type + `Event.model` + `Turn::models` + a regression test pinning the hash unchanged, CC and Codex only
3. attribution rendering in `agit log`
4. the Cursor adapter (read-only), with attribution left as None throughout
5. `codex_index::Thread` gains the `model` and `reasoning_effort` columns
6. **the Kiro adapter — collect the data before writing the code**
7. the `needs_ir_roundtrip` rename + the refuse/warn matrix over the 12 combinations

Step 6 sits there deliberately. The self-criticism opening `compact-mechanism.md` — "that premise
came from an old design document, and I implemented it without verifying it" — describes exactly
where Kiro stands now: 4 records, and everything else inferred from binary symbols.
