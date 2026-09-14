# OpenClaw, Hermes, and WorkBuddy integration research

Research date: 2026-09-14. AgentGit revision: `5607774424ad019e87574de33fe37618f6a42069`.
Backend inspection revision: `90b6c7afa10cf1ac22d168d12e8b5f8a8f4b631d`.

Evidence labels: **Observed** means a local runtime experiment or installed artifact;
**Source** means inspected implementation or official documentation; **Proposed** means work
identified during the initial inspection. The delivered status below supersedes the baseline proposals.

## Delivered implementation

The shared middle layer is `Adapter + Session/Event IR`, backed by a single runtime registry.
Native records remain the archive source of truth. New integrations keep their storage,
completion, restoration, launch, and presentation behavior inside the adapter boundary;
application features consume the common interfaces.

| Capability | OpenClaw | Hermes | WorkBuddy |
| --- | --- | --- | --- |
| Discover and import native sessions | SQLite windows across agent roots | SQLite sessions and messages | Project JSONL, including sessions without GUI metadata |
| Bounded consistent snapshot | Read transaction, exact event JSON | Read transaction, deterministic row envelopes | Bounded native file reads |
| Parse, show, search, tool arguments/results | Supported | Supported | Supported |
| Same-runtime native history restoration | Native transaction writer | Native SessionDB writer | New native JSONL identity |
| Cross-runtime restoration and native continuation | Verified | Verified | Verified |
| Completed-turn automatic capture | Native lifecycle plugin | Native session hooks | SessionStart and Stop hooks |
| AgentGit tools | Skill and lifecycle context | Skill and stdio MCP | Skill and deferred stdio MCP tools |
| Hub transcript and runtime filters | Supported | Supported | Supported |
| AgentGit remote-control driver | Not implemented | Not implemented | Not implemented |

The registry is [adapter/registry.rs](../../src/adapter/registry.rs). The runtime adapters are
[openclaw.rs](../../src/adapter/openclaw.rs), [hermes.rs](../../src/adapter/hermes.rs), and
[workbuddy.rs](../../src/adapter/workbuddy.rs). Shared presentation details live in
[adapter/detail.rs](../../src/adapter/detail.rs), with native lifecycle installation in
[commands/setup/native.rs](../../src/commands/setup/native.rs). The Hub consumes the same library
API instead of maintaining another detail parser. Its dependency remains pinned to an immutable
CLI commit containing that shared API. Merge the CLI change first, then pin its resulting
squash commit in the backend before merging the dependent backend change.

`new`, `resume`, native `export`, setup choices, and local runtime labels use registered adapter
metadata. Native hooks still require a small integration module because the runtimes use
different lifecycle/configuration protocols. Adding another runtime therefore means its adapter,
registry entry, native integration where needed, and format/protocol fixtures. There is no new
plugin loader or alternate IR.

Saved history is dispatched by envelope source and native session identity through
[transcript/display.rs](../../src/domain/transcript/display.rs). This also carries tool arguments
and paired results into exports and restored sessions. `resume` and archive-merge exploration
use [install_saved](../../src/domain/install/mod.rs); they do not interpret an inherited mixed
VIEW using only the latest writer's runtime. Headerless Hermes and OpenClaw increments remain
valid archive records, and native bootstrap headers are restored only when installing a session.
Secret hydration preserves provenance and recomputes content hashes before that installation.

Hermes and WorkBuddy expose their per-record native session identity through `Adapter::record_group`.
CLI display and Hub projection use it to isolate reused tool IDs after repeated materialization.
The Hub's runtime labels and filter metadata still have a small frontend registration surface;
the shared Rust registry does not generate those frontend assets.

### Local use

The runtime executables are installed locally. The AgentGit implementation is built in this
worktree; it does not replace an existing system AgentGit binary. Select the integrations to
install into the desired native profile:

```sh
cargo build --bin agit
./target/debug/agit setup --runtime openclaw --hooks --skill
./target/debug/agit setup --runtime hermes --hooks --skill --mcp
./target/debug/agit setup --runtime workbuddy --hooks --skill --mcp

./target/debug/agit new owner/repo --as hermes -b investigation
./target/debug/agit resume owner/repo@investigation --as openclaw
./target/debug/agit export owner/repo@investigation --format workbuddy -o session.jsonl
```

Setup merges AgentGit entries into native configuration. Hermes asks for trust when it first
loads hook commands; the setup command does not pre-approve the user's hooks. Hermes requires
its optional MCP dependencies for MCP setup. OpenClaw uses a native plugin, with permission for
that plugin to inspect lifecycle context and prepend the AgentGit session annotation.

OpenClaw launches use the verified `2026.9.4` embedded CLI API through a bundled helper.
The helper initializes the native configuration and plugins, retains the native local state
lock and signal handling, finalizes native resources and output on success or failure, and
binds both the workspace and tool working directory to the selected project. Its version check
rejects unverified API changes. Relative `--cwd` paths are resolved before the launch shell changes directories; native default-workspace settings
remain intact. The smoke suite verifies new and resumed file reads plus automatic capture
with different native-default and selected project directories. Resident Node handles exercise
completion and error exit without trapping the prompt loop.

Model credentials remain the native runtime's configuration responsibility. The supplied Bailian
key was used only for the live connectivity probes described below, then its temporary file was
removed. It is not in fixtures, commits, or persistent runtime configuration. The smoke suite
uses a synthetic model on loopback and needs no API key.

### Reproducible verification

```sh
cargo build --bin agit --example native-runtime-probe
python3 scripts/native-runtime-smoke.py all
```

The [smoke suite](../../scripts/native-runtime-smoke.py) creates a temporary profile for each
installed runtime, installs synthetic archived history, resumes through its real CLI, checks
that native model input contains the original user prompt and paired tool result, and verifies
that the native completion hook saves the new reply to the selected AgentGit branch. It exercises
public `new --no-launch`, native `export`, and `resume --no-launch`, then continues the prepared
session using the installed CLI. Further exports and repeated restorations must retain both
inherited history and newly saved replies. It checks MCP discovery for Hermes and WorkBuddy.
OpenClaw uses its lifecycle plugin. Evidence directories are printed and retained for inspection.
The suite does not modify existing WorkBuddy tasks.

The runtime versions tested are OpenClaw `2026.9.4`, Hermes `0.21.2` at source
`5dea46d13deec9549bdc2ea703ae9201d733c28d`, and WorkBuddy AI `5.5.2`. Format fixtures additionally
cover branch selection, compacted/inactive messages, native row preservation, identity collisions,
read limits, tool pairing, and directory aliases. Backend session tests check native projection
and source coordinates; website tests check reasoning visibility and tool classification.
Archive-merge tests cover restoration of each selected native source and exclusion of LOG-only
context. Hydration tests verify preserved source identity and valid hashes after replacing local
secret placeholders.

### Boundaries

- OpenClaw native restoration uses the verified storage API for `2026.9.4`; another version must
  pass the writer and continuation probes before enabling that writer. Hermes restoration uses
  its installed native SessionDB API and rejects schema fields that it cannot preserve.
- Restoration of one native source into the same format preserves native fields while changing
  local identities. Cross-format or mixed-source restoration goes through the deliberately lossy
  IR; runtime-specific reasoning is not portable. Original native archive records remain intact.
- OpenClaw active branch selection is projected explicitly. Hermes in-place compaction changes
  the snapshot prefix and is subject to continuity checks; concurrent rewind/compaction under
  load has not been verified. Interrupted streams remain pending without a durable completion.
- WorkBuddy CLI sessions persist in native JSONL. Registration in the GUI task list is not
  implemented. Standalone WorkBuddy subagent sessions and AgentGit live remote-control drivers
  are outside this implementation.
- Model usage/cost normalization for these providers is not added. Transcript support does not
  imply provider billing support.
- OpenClaw setup requires JSON configuration and returns an error without rewriting a JSON5
  configuration that it cannot parse.

The initial research inventory below records the inspected baseline and the reasoning behind
these boundaries. Its proposed changes are superseded by the delivered status above.

## Installed tools and observed behavior

| Runtime | Inspected version | Local entry point | Native conversation carrier |
| --- | --- | --- | --- |
| OpenClaw | `2026.9.4` | `~/.local/bin/openclaw` | Per-agent `openclaw-agent.sqlite` |
| Hermes | `0.21.2`, source `5dea46d13deec9549bdc2ea703ae9201d733c28d` | `~/.local/bin/hermes` | `state.db` |
| WorkBuddy AI | Existing macOS app `5.5.2` | Added `~/.local/bin/workbuddy-cli`, using the app's bundled CLI | Project JSONL; desktop session metadata in `workbuddy.db` |

OpenClaw has a dedicated Node `26.8.2` installation under `~/.local/share/openclaw-runtime`.
Hermes is installed under `~/.hermes/hermes-agent` with its own Python 3.13 environment. The
existing Node default was retained. No OpenClaw gateway daemon or messaging channels were installed.

All probes used separate roots under `/private/tmp/agentgit-{runtime}-probe`. Model requests used
Bailian's `https://dashscope.aliyuncs.com/compatible-mode/v1` endpoint and `deepseek-v4-flash`,
which is the model identifier verified by an actual successful request. The suggested
`deepseek-v4.1-flash` was not used. See the [official DeepSeek API reference](https://help.aliyun.com/zh/model-studio/deepseek-api).

| Experiment | OpenClaw | Hermes | WorkBuddy |
| --- | --- | --- | --- |
| Create a session and return a specified marker | Passed | Passed | Passed |
| Exit, resume by the same native ID, recall that marker | Passed | Passed | Passed |
| Read a synthetic local file and return its contents | Passed | Passed | Passed |
| Find the native tool call and its matching stored result | Passed | Passed | Passed |

Validation checked output content and native call/result identities, not just exit codes. These
are continuity and tool-use smoke probes, not a comparative coding benchmark. Custom-provider
cost fields reported by a runtime are not billing evidence.

The API key was injected through process environment variables. Probe configurations retain only
environment references; the temporary credential file was removed after checking generated state
for the literal key. No existing WorkBuddy conversation was used as a model prompt or test input.

Configuration details relevant to reproduction:

- OpenClaw: provider `bailian`, `api: "openai-completions"`, `apiKey: "${DASHSCOPE_API_KEY}"`;
  model `bailian/deepseek-v4-flash`. The file probe enabled only the `read` tool.
- Hermes: a named `providers.bailian` entry with `base_url`, `key_env: "DASHSCOPE_API_KEY"`,
  `default_model`, and `transport: "chat_completions"`. Merely supplying `OPENAI_API_KEY` with
  a custom URL did not select the intended credential in this version. `--reasoning low` worked;
  `--reasoning none` was rejected by the provider.
- WorkBuddy: isolated `models.json` with a custom model URL ending in `/chat/completions`,
  `apiKey: "${DASHSCOPE_API_KEY}"`, and tool-call support. The successful file probe used
  `--tools Read --allowedTools Read`; it did not enable general permission bypass. A previous
  permission-denial response was persisted as ordinary conversation text, so the successful
  permission test used a fresh session.

## OpenClaw

**Observed:** the active carrier is
`$OPENCLAW_STATE_DIR/agents/main/agent/openclaw-agent.sqlite`. With the default state root, this is
`~/.openclaw/agents/<agentId>/agent/openclaw-agent.sqlite`. The probe contained:

- `session_nodes`: routing `session_key`, `current_session_id`, lifecycle and ownership metadata.
- `session_windows`: `session_id`, `session_key`, `previous_session_id`, and rollover reason.
- `transcript_events`: `session_id`, ordered `seq`, native `event_json`, and `created_at`.

An event starts with `type: "session"` and a header containing `id`, `cwd`, and version. Message
events contain `message.role` and content blocks; other event types include model changes,
thinking-level changes, and custom events. The observed tool pairing was
`message.content[].{type: "toolCall", id, name, arguments}` to a `role: "toolResult"` message
with `toolCallId`. Assistant `stopReason: "toolUse"` preceded the result; the final reply had
`stopReason: "stop"`.

**Source:** the [session documentation](https://docs.openclaw.ai/concepts/session) distinguishes
active SQLite data from archived JSONL and legacy migration inputs. The
[schema reference](https://docs.openclaw.ai/reference/session-management-compaction/schema)
describes routing/window identities and event ancestry. An adapter needs to preserve `parentId`
and compaction/reset relationships rather than flattening all windows into one transcript.

**Observed:** this command family created and continued the same explicit native session:

```sh
openclaw agent --local --session-id <session-id> --message "<prompt>" --json
```

The command result's `sessionFile` was a logical identifier such as
`agent:main:explicit:<session-id>`, not a filesystem path. `openclaw transcripts` concerns the
meeting-transcript store in this installation; it is not an agent-session export interface.

**Proposed:** discover all configured agent roots and read bounded, consistent SQLite snapshots.
Keep the agent/store identity as well as the routing key and window ID; a cwd identifies the
recorded agent workspace and is not evidence that every channel conversation belongs to a Git
repository. Verify reset, fork, compaction, and history installation against the installed
version. Gateway/hook integration is a separate follow-up to local `agent --local` execution.

## Hermes

**Observed:** `$HERMES_HOME/state.db` contains `sessions` and `messages`. Session rows include
`id`, `source`, `cwd`, `model_config`, and `parent_session_id`. Message rows include `id`, `role`,
`content`, `tool_calls`, `tool_call_id`, `tool_name`, `finish_reason`, reasoning fields,
`active`, `compacted`, `api_content`, and display metadata. The file probe paired an assistant's
`tool_calls[].id` with a `role: "tool"` row's `tool_call_id`; the final assistant row had
`finish_reason: "stop"`.

**Observed:** one-shot CLI creation and native resumption both worked:

```sh
hermes chat -Q --oneshot --provider bailian -m deepseek-v4-flash --reasoning low -q "<prompt>"
hermes chat -Q --oneshot --resume <session-id> --provider bailian -m deepseek-v4-flash --reasoning low -q "<follow-up>"
```

**Source:** at the inspected Hermes revision, `hermes_state_messages.py` distinguishes active
model context from display history (`active = 1 OR compacted = 1`).
`hermes_state_sessions.py` distinguishes compression continuations, delegated children,
branches, and resets even though they can all have `parent_session_id`. Archive must preserve
native fields; display and model-context projections need their own explicit selection rules.

`hermes sessions export` supports JSONL, but the ordinary JSONL renderer in
`hermes_cli/session_export.py` emits one complete session object per line. Feeding that directly
into AgentGit's line-addressed store would collapse message-level coordinates. The inspected
`hermes sessions import` command is specifically a Claude Code / Codex importer, not proof that
arbitrary native Hermes exports can be round-tripped. See the
[official sessions guide](https://hermes-agent.nousresearch.com/docs/user-guide/sessions).

**Proposed:** normalize a consistent SQLite snapshot into deterministic native records, retaining
the original row identities and fields. Reuse the existing database-adapter approach, but model
rewind/compaction updates explicitly. Verify a supported native writer or import bridge before
advertising AgentGit history installation. Hook integration should distinguish a successful
completed turn from shutdown, failure, or interruption; the
[Hermes hook guide](https://hermes-agent.nousresearch.com/docs/user-guide/features/hooks)
describes lifecycle callbacks, but callback names alone are insufficient settlement evidence.

## WorkBuddy: storage and CLI

**Observed from the installed app:** `/Applications/WorkBuddy AI.app` contains a runnable CLI at
`Contents/Resources/app.asar.unpacked/cli/dist/codebuddy.js`. This is the CLI bundled with the
installed WorkBuddy app; it does not require installing a separate CodeBuddy package.

The app's product metadata selects `.workbuddy-ai`. Its desktop state root on this machine is:

```text
~/.workbuddy-ai/
  workbuddy.db                         Desktop session/task metadata
  projects/<cwd-slug>/<session-id>.jsonl
  projects/<cwd-slug>/<session-id>/subagents/agent-*.jsonl
```

`workbuddy.db` has a `sessions` table containing ID, cwd, title, status, model, project, and
timestamps. It is not the message-body store. Transcript discovery must include JSONL even when
a desktop metadata row is absent. The project slug resolves the cwd and replaces path separators
and colons with hyphens, trims leading/trailing hyphens, and collapses repeated hyphens; it is
not Claude Code's project-slug contract.

**Observed/Source:** invoking the bundled CLI directly defaults to `~/.codebuddy`; it honors
`CODEBUDDY_CONFIG_DIR`. The added `workbuddy-cli` wrapper explicitly selects the WorkBuddy root:

```sh
#!/bin/sh
WORKBUDDY_CONFIG_DIR="${WORKBUDDY_CONFIG_DIR:-${CODEBUDDY_CONFIG_DIR:-$HOME/.workbuddy-ai}}"
CODEBUDDY_CONFIG_DIR="$WORKBUDDY_CONFIG_DIR"
export WORKBUDDY_CONFIG_DIR CODEBUDDY_CONFIG_DIR
exec node "/Applications/WorkBuddy AI.app/Contents/Resources/app.asar.unpacked/cli/dist/codebuddy.js" "$@"
```

With the selected root's model/auth configuration available, launch and continue without the GUI:

```sh
workbuddy-cli -p "<prompt>" --session-id my-session --output-format json
workbuddy-cli -p "<follow-up>" --resume my-session --output-format json
```

The probes used these flags with `--model deepseek-v4-flash`, isolated configuration roots, bounded
turns, and explicit tool settings. The installed CLI also advertises `stream-json` input/output,
`--fork-session`, `--serve`, and `--acp`; those control modes were not exercised. They are useful
candidates for a future RC driver, not verified RC capabilities.

**Observed:** native records differ from Claude Code:

- Text: `type: "message"`, top-level `role`, `content` blocks with `input_text` or `output_text`,
  `sessionId`, `cwd`, `timestamp`, and optional `parentId` / `providerData`.
- Calls: `type: "function_call"`, `callId`, `name`, and stringified `arguments`.
- Results: `type: "function_call_result"`, matching `callId`, `output`, and `status`.
- Other records include `file-history-snapshot`.

An assistant text record and its function call shared the same `id`. Deduplication by message ID
alone would discard valid data. An assistant text record marked `status: "completed"` appeared
before its tool call and result, so that status cannot independently prove a completed user turn.
The CLI result envelope and future hooks need to be mapped to durable native evidence.

**Observed limitation:** CLI probes created persistent JSONL without creating `workbuddy.db` in
the isolated root. Native CLI creation/resumption is verified; automatic registration in the
desktop task list is not. If GUI visibility is required, investigate the app's task-creation API
instead of assuming a transcript file is sufficient.

## Initial code change inventory

The links below refer to the inspected AgentGit checkout. Backend paths are listed separately
because that repository consumes AgentGit as a pinned library dependency.

| Boundary | Current code | Required change |
| --- | --- | --- |
| Registration and detection | [adapter/mod.rs](../../src/adapter/mod.rs): `RUNTIMES`, `normalize`, `get`, `all`, `infer_runtime` | Register runtime descriptors once; add precise format probes. The current `sessionId` detector would classify WorkBuddy records as Claude Code. |
| Native discovery and capture | [adapter/native_snapshot.rs](../../src/adapter/native_snapshot.rs), [adapter/opencode.rs](../../src/adapter/opencode.rs) | Add OpenClaw/Hermes SQLite snapshots and WorkBuddy project/subagent discovery. Preserve explicit identity, bounds, consistency, and raw records. |
| Pending history and import identity | [commands/diff/pending.rs](../../src/commands/diff/pending.rs), [import lineage acceptance](../../src/commands/import/lineage/acceptance.rs) | Replace OpenCode-specific database branches and format identity switches with adapter-owned source semantics. Do not assume monotonically growing files. |
| Enrichment and settlement | [adapter/enrich.rs](../../src/adapter/enrich.rs), [domain/turn/mod.rs](../../src/domain/turn/mod.rs) | Add per-format tool/reasoning details and explicit completion evidence; move format-specific dispatch behind the adapter boundary. |
| Native history installation | [domain/install/mod.rs](../../src/domain/install/mod.rs) | Delegate same-format identity rewriting, bootstrap, and dangling-call handling. Verify render/install before declaring a target `Resumable`. |
| Launch and native identity | [commands/new.rs](../../src/commands/new.rs), [commands/resume.rs](../../src/commands/resume.rs), [infra/runtime_session.rs](../../src/infra/runtime_session.rs) | Centralize executable/arguments/env and explicit session identity. `new` currently falls back to `claude` outside its known launch cases. |
| Setup and hooks | [commands/setup.rs](../../src/commands/setup.rs), [commands/hooks.rs](../../src/commands/hooks.rs), [infra/runtime_memory.rs](../../src/infra/runtime_memory.rs) | Add runtime integration descriptors for skill/MCP/hook locations and native payload mapping. Register only verified lifecycle behavior. |
| Local UI | [ui/session.rs](../../src/ui/session.rs), [TUI selector](../../src/tui/screens/selector.rs), [TUI sessions](../../src/tui/screens/sessions.rs) | Derive runtime choices and labels from registered metadata where possible. |
| Remote control | [rc/harness/mod.rs](../../src/rc/harness/mod.rs) | Extend `AnyDriver` and its capability registration only when the runtime's live protocol is implemented and tested. |

Backend changes required for complete Hub behavior:

- `Cargo.toml` and `Cargo.lock`: advance the `agit` library pin (currently
  `91fb92c2f5700ed110b96d90b807fc649a1499e4`) to a revision containing the adapters.
- `src/features/sessions/detail.rs`: `of_line` independently handles Claude/Cursor, Codex, and
  OpenCode. New IR parsing alone would leave tool arguments, reasoning, and compaction details
  incomplete. Share native detail extraction from the library, while keeping Hub response limits
  and presentation policy in the backend.
- `website/src/lib/agents-runtimes.ts` and
  `website/src/routes/session-transcript-utils.ts`: extend runtime filters and normalization;
  prefer registry metadata exposed by the API where practical. Update support lists with the
  actual capability delivered.

## Initial implementation plan

1. Consolidate registration and the leaking boundaries while preserving existing runtime behavior:
   descriptors, launch integration, native snapshot semantics, format detail extraction, native
   localization, and turn-completion policy. Extend the current abstractions incrementally; a
   dynamic plugin loader or a replacement IR is unnecessary for these integrations.
2. Add each native adapter and targeted format fixtures. Verify discovery, explicit-ID selection,
   import, incremental settlement, display, search, and raw export. WorkBuddy is a useful first
   file-format pilot; OpenClaw and Hermes exercise the shared database-source boundary. Do not
   advertise any runtime as an install target while only reading has been implemented.
3. Verify native-history installation into a fresh ID, then same-runtime and cross-runtime resume.
   Test an exported AgentGit VIEW, including a cut at a compact boundary and unmatched tool calls.
   Native `--resume` of an existing session, proven here, does not establish this capability.
4. Add verified setup/hooks and optional live drivers. Test stream reconstruction, interruption,
   approvals, permission changes, and restart/resume before reporting their RC capabilities.
   Update the backend dependency and Hub presentation alongside the supported data features.

The main format tests should cover tool pairing, record identity collisions, explicit session
identity, database mutations, active versus compacted history, branch/reset lineage, incomplete
trailing writes, and unknown records. Database tests should verify consistent bounded snapshots
without modifying the native store. Hub validation should include tool arguments/results and
reasoning, not only the conversation's visible text.

The baseline investigation did not establish native history installation or hook execution.
The implementation and smoke suite above now verify those paths. Concurrent compaction/rewind
under load, remote control, and WorkBuddy GUI task-list registration remain unverified.
