# Claude ↔ Codex session interconversion — design

Status: **design approved, Step-0 spike pending** (go/no-go gate).
Date: 2026-07-15.

## Goal

`agit convert <src-session> --to codex|claude-code` writes a native session file the
**target** runtime can resume (`codex resume <id>` / `claude --resume <id>`). agit's job
ends at producing a file the native CLI accepts; the user runs the resume themselves.

## Constraints (from format research, verified on real data)

- **Encrypted reasoning is vendor-locked.** Claude `thinking.signature` and Codex Fernet
  `encrypted_content` are opaque, tied to the originating provider's key. The *other*
  model rejects them (400). Their plaintext is **not on disk** anyway (Claude `thinking`
  text is `""`; Codex reasoning `content` is `null`, only a 7% one-line `summary`).
- **Claude transcripts lack the request frame** — no system prompt, no tool schema, no
  CLAUDE.md. Codex stores `base_instructions` in `session_meta`.

Consequence — fidelity splits:

| direction | fidelity | mechanism |
|---|---|---|
| same-vendor (Claude→IR→Claude) | **byte-faithful** | re-emit raw records verbatim |
| cross-vendor (Claude→Codex) | **content-faithful** | rebuild visible turns; drop vendor token; synthesize system prompt; narrate tools |

Nothing human-readable is lost cross-vendor (there was no plaintext reasoning to begin
with). What is lost: the encrypted continuity token (useless cross-vendor) and the exact
request frame.

## Architecture (Approach B — neutral IR hub)

```
src/adapter/claude_code.rs   + reader jsonl→ConversationIR   + writer ConversationIR→jsonl
src/adapter/codex.rs         + reader rollout→ConversationIR + writer ConversationIR→rollout
src/convo.rs      NEW  ConversationIR types + convert() orchestration
src/register.rs   NEW  native-CLI on-disk contract (dirs, ids, index) — the fragile part, isolated
```

Two IRs, kept apart on purpose:
- `SessionIR` (exists) — lossy summary for reconcile's brief. **Untouched.**
- `ConversationIR` (new) — lossless full fidelity, for `convert` only.

Flow: `read source → ConversationIR → target writer (same-vendor raw passthrough |
cross-vendor synthesis) → register.rs install → print resume command`.

CLI: `agit convert <session-path-or-id> --to codex|claude-code [--cwd P] [--structured-tools] [--write]`.
Default is **dry-run** (emit to stdout); `--write` installs into the target's store.

## ConversationIR schema

```rust
struct ConversationIR {
    source_runtime: String,
    session_id: String,
    cwd: Option<String>,
    git_branch: Option<String>,
    system_prompt: Option<String>,   // codex base_instructions; None for claude
    events: Vec<Event>,              // file order — the fidelity anchor
}
struct Event {
    raw: serde_json::Value,          // ORIGINAL record verbatim (byte-faithful anchor)
    kinds: Vec<EventKind>,           // 0..n semantic items derived from raw
    id: Option<String>,              // record uuid / call_id
    parent_id: Option<String>,       // claude parentUuid — the tree
    timestamp: Option<String>,
}
enum EventKind {
    UserPrompt(String),
    AssistantText(String),
    ToolCall  { call_id: Option<String>, name: String, input: serde_json::Value },
    ToolResult{ call_id: Option<String>, output: String },
    Reasoning(Opaque),               // { vendor, blob } — kept, never re-emitted cross-vendor
    FileEdit  { paths: Vec<String> },
    Other,                           // meta/system/turn_context/token_count — lives only in raw
}
```

`kinds` is a Vec because one Claude `assistant` record holds multiple blocks
(thinking+text+tool_use). `Event` stays 1:1 with a source record so `raw` can round-trip.
Byte-faithfulness is mechanical: same-vendor writer = `events.map(|e| e.raw)` re-serialized
in order == source file. That equality is the round-trip test.

## Cross-vendor synthesis rules

1. **Reasoning → dropped.** No reasoning/thinking items → avoids the 400s. No plaintext lost.
2. **System prompt.** Codex→Claude: emit `base_instructions` as a leading tagged note.
   Claude→Codex: `session_meta.instructions = null` (Codex default at resume).
3. **Tool activity → narrated as text (default).** `ToolCall`/`ToolResult`/`FileEdit` become
   descriptive assistant text (`[ran Bash: cargo test]`, `[edited: main.rs]`) rather than
   structured tool items — the target's tool schema differs and a foreign structured call
   risks 400s / misleads the model. `--structured-tools` opts into real tool items (risky).
4. **Turn mapping** (narrate mode): rebuild the target's native records
   (Claude→Codex: response_item+event_msg pairs + synthesized session_meta;
   Codex→Claude: type=user/assistant with a fresh parentUuid chain).
5. **Fresh session id** — never reuse the source id.

## Native-CLI contract + Step-0 spike (GO/NO-GO)

To be *listed and resumed*, a hand-authored session must match the CLI's on-disk contract:

- **Codex**: `~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<uuid>.jsonl`, first line valid
  `session_meta` (id==filename uuid). Open: does `codex resume` scan the dir or read
  `~/.codex/session_index.jsonl`? `cli_version` gating?
- **Claude**: `~/.claude/projects/<slug>/<uuid>.jsonl`. Open: dir-scan vs index; first user
  prompt becomes the resume-list title.

Spike (pure reverse-engineering, no product code):
1. Copy a real session under a new uuid+filename into the right dir (+ index if present).
2. Run the resume path — does the copy appear and load?
3. Strip records until it stops loading → minimal contract.
4. Determine index dependency empirically.

Output: a "minimal loadable session" recipe per CLI, encapsulated in `register.rs`
(`install(runtime, bytes, meta) -> ResumeHandle`).

**Go/no-go**: if a CLI rejects hand-authored sessions (signed/checksummed index, hard
version gate), fall back to "content migration" — file readable/reconcilable, not natively
resumable. Note: `claude` is present locally; `codex` CLI may not be — the Codex contract
may only be derivable statically from the 838 real rollouts + docs.

## Error handling

- Unknown runtime → hard error. Malformed/empty source → refuse (`nothing to resume`).
- Same source==target → allowed (byte-faithful clone, fresh id).
- cwd portability: default source cwd, `--cwd` override, warn if not local.
- Huge tool outputs → truncate narration, mark `[output truncated]`.
- Secret leakage: run `scan.rs` over the output, warn before `--write`.
- Writes mutate real stores → `--dry-run` default, `--write` explicit, always print path + resume cmd.

## Testing (how "faithful" is proven, not asserted)

- **Round-trip byte tests (core)** — real session → reader → IR → same-vendor writer → bytes
  == original. Property test over many real sessions (838 codex + local claude).
- **Cross-vendor structural** — Claude→IR→Codex, read back → IR′, visible content equals source.
- **Synthesis units** — reasoning dropped, system prompt injected, narration, fresh id, truncation.
- **register.rs contract test** — write minimal session, assert CLI lists it (gated on CLI presence).
- **Resume smoke (acceptance)** — write file, run resume path, no error + history shown (gated).

## Explicitly out of scope

- Decrypting or re-encrypting reasoning (impossible — vendor keys).
- Cross-vendor structured-tool replay by default (schema mismatch; opt-in only).
- A live API proxy/shim.
