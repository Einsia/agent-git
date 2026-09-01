# Feasibility of Claude Desktop / ChatGPT Desktop as `agit` targets and sources

Machine surveyed: macOS darwin 24.6.0, 2026-08-01. Read-only throughout: nothing was written to or
deleted from either application's storage, and no message was sent and no session created through
their UI. Every SQLite database was `cp`-ed into `/tmp/agit-desktop/raw/` before being opened;
LevelDB was likewise copied, and its `LOCK` removed, before the scan.

Evidence labels: **[observed]** = a file / table / string actually inspected on this machine;
**[documented]** = official or third-party documentation; **[inferred]** = derived from the first
two, with a confidence level.

---

## 0. Conclusions at a glance

The assumption behind the task is "both desktop applications are server-authoritative". **Half of
that is right and half is wrong**, and the wrong half (ChatGPT Desktop) is the one that matters
more.

| Target surface | `parse` / `import` (read) | `install` / `clone --as` (write) |
|---|---|---|
| **ChatGPT Desktop** — the Codex surface | **Already works**. It is `~/.codex`; agit's existing `codex` adapter covers it unchanged | **Feasible**. Same as above; no new code, only a clear statement in `doctor` |
| **ChatGPT Desktop** — the ChatGPT chat surface | **Not feasible**. No local session storage on this machine | **Not feasible** |
| **Claude Desktop** — Code tab | **Already works (and is already happening quietly)**. The transcript is `~/.claude/projects/**.jsonl` | **Feasible, through a hand-off**. There is an official URL scheme `claude://resume?session=<uuid>` |
| **Claude Desktop** — Cowork / Chat (non-VM) | **Feasible, `parse` needs extending**. It is Claude Code jsonl with three extra record types | **Should not be done**. HMAC audit chain + migrating into a VM disk image |
| **Claude Desktop** — Cowork (VM sandbox) | **Not feasible**. The transcript is inside `sessiondata.img` | **Not feasible** |
| **Claude Desktop** — Chat / Projects (the claude.ai surface) | **Not feasible**. Zero local cache | **Not feasible** |

Two sentences:

- **ChatGPT Desktop needs no new adapter.** What it embeds is the Codex engine, reading and
  writing the same `CODEX_HOME=~/.codex`. `--as chatgpt-desktop` is an **alias** for `codex`, not
  a new runtime.
- **Claude Desktop needs a new runtime name, but its value is not "writing a file", it is the
  hand-off.** `--as claude-code` already delivers the transcript; what `--as claude-desktop` adds
  is that deep link, which lets the desktop application ingest the transcript **itself** and build
  its own index entry — agit writes not one byte into its private storage.

---

## 1. Observed storage layout

### 1.1 Installed versions **[observed]**

```
/Applications/Claude.app
  CFBundleIdentifier        com.anthropic.claudefordesktop
  CFBundleShortVersionString 1.24012.9
  ElectronAsarIntegrity     present  → Electron
  CFBundleURLSchemes        ["claude"], ["msauth.com.anthropic.claudefordesktop"]

/Applications/ChatGPT.app
  CFBundleIdentifier        com.openai.codex          ← the former Codex desktop
  CFBundleShortVersionString 26.727.40816  (CFBundleVersion 6067)
  ChromiumBaseVersion       150.0.7871.182            → Chromium/CEF, not Electron
  CFBundleURLSchemes        ["codex"]
```

`/Applications/ChatGPT Classic.app` (the old `com.openai.chat`) is **not installed on this
machine**, and `~/Library/Application Support/com.openai.chat` does not exist either, so the path
named in the task cannot be verified here. **[documented]** Third-party reports say the old
classic application drops `conversation-*` / `project-g-*` directories under it (comments on
openai/codex#31878), but that is a different bundle and irrelevant to this report's targets.

### 1.2 ChatGPT Desktop: it is Codex **[observed]**

The decisive evidence is the engine binary the application ships inside itself:

```
/Applications/ChatGPT.app/Contents/Resources/codex        270,605,984 bytes
$ .../Resources/codex --version
codex-cli 0.146.0-alpha.9.2
```

Counting strings in that binary: `CODEX_HOME` 74 times, `rollout-` 31, `/sessions/` 3,
`archived_sessions` 6, `thread/resume` 22, `thread/fork` 16, `thread/inject_items` 6,
`useStateDbOnly` 1.

The matching runtime traces: in `~/.codex/logs_2.sqlite` (333 MB, last written 2026-07-31 17:18)
the newest rows of the `logs` table all carry `target`
`codex_app_server_transport::transport::remote_control::websocket` — the app-server process is
running, and writing its log into `~/.codex`. `~/.codex/ipc/ipc.sock` exists (a Unix socket,
`srw-------`).

All the desktop side maintains **on top of that** is one sidecar index layer, in
`~/.codex/sqlite/codex-dev.db`:

```sql
local_thread_catalog(host_id, thread_id, display_title, source_created_at,
                     source_updated_at, cwd, source_kind, source_detail,
                     model_provider, git_branch, observation_sequence,
                     missing_candidate, thread_source)
local_thread_catalog_sync_state(host_id, watermark_updated_at,
                     initial_build_complete, observation_sequence,
                     last_full_reconciled_at)
local_thread_catalog_hosts(host_id, host_kind CHECK IN
                     ('local','ssh','wsl','remote-control'))
local_thread_catalog_metadata(catalog_revision)
```

Observed: 42 rows, `catalog_revision = 107`, 4 hosts (1 `local` + 3 `remote-ssh-discovered:*`).
The `local` host has `initial_build_complete = 0` and `watermark_updated_at = NULL`, and only 4
rows reached the catalog while `threads` in `state_5.sqlite` holds 124 — **the catalog is a
lazily built derived cache, not the source of truth**. Those 4 rows' `thread_id` values hit 4/4
in `state_5.sqlite.threads`, and 4/4 have a corresponding rollout file under
`~/.codex/sessions/`.

The direction of derivation is stated by the protocol itself. The installed CLI
(`codex-cli 0.144.6`) emits its own protocol schema:

```
$ codex app-server generate-json-schema --out /tmp/agit-desktop/codex-app-server-schema
```

The description of `ThreadListParams.useStateDbOnly`, verbatim:

> "If true, return from the state DB without scanning JSONL rollouts to repair thread
> metadata. Omitted or false preserves scan-and-repair behavior."

**The default behavior scans the JSONL rollouts and repairs the state DB from them.** The rollout
on disk is authoritative; the `threads` table and `local_thread_catalog` are both rebuildable
indexes.

The doc comment on `ThreadResumeParams` is more direct:

> "There are three ways to resume a thread: 1. By thread_id: load the thread from disk by
> thread_id and resume it. 2. By history: instantiate the thread from memory and resume it.
> 3. By path: load the thread from disk by path and resume it."

The protocol also carries `thread/inject_items` ("Raw Responses API items to append to the
thread's model-visible history"), `thread/fork` (takes `lastTurnId`, so it can truncate to a given
turn) and `thread/rollback`. The full method table is in
`/tmp/agit-desktop/chatgpt-desktop/app-server-write-path-schemas.json`.

**The ChatGPT chat surface (not Codex)** holds no local conversation text at all: the
application's own Chromium profile `~/Library/Application Support/Codex/Default/` has **no
IndexedDB directory**, and `Local Storage` is 7.3 MB but carries a single storage key `app://-`
whose keys are all `statsig.cached.evaluations.*` / `statsig.session_id.*` / `statsig.d` (6 of
them) — pure feature flags. `~/Library/Preferences/com.openai.codex.plist` holds only Sparkle
update state and window positions.

### 1.3 Claude Desktop: two surfaces, opposite conclusions

**The claude.ai surface (Chat / Projects) keeps zero conversation text locally [observed].** This
is the cleanest piece of evidence:

The whole `~/Library/Application Support/Claude/IndexedDB/https_claude.ai_0.indexeddb.leveldb/`
directory is 16 KB, its only object store is `keyval-store`, and its entire contents are TanStack
Query persistence envelopes — snapshots from several points in time, verbatim:

```json
{"buster":"conversations_v2:anon","timestamp":1782875053883,"clientState":{"mutations":[],"queries":[]}}
{"buster":"conversations_v2:anon","timestamp":1785140381933,"clientState":{"mutations":[],"queries":[]}}
```

The `buster` is literally named `conversations_v2`, and `queries` is invariably an empty array.
This is not "the cache expired", it is **never cached**. `Local Storage` is 3 MB across 80 keys,
every one of them read (the list is in `/tmp/agit-desktop/claude-desktop/localstorage-keys.txt`),
and all of it is UI state:
`LSS-persisted.starred-local-code-sessions`, `banner_dismissed:*`, `epitaxy-tasks-store`,
`ccd-session-store`, `composer-draft:epitaxy-local_<uuid>` (an unsent composer draft), and so on.
The credentials are `oauth:tokenCache: "djEw..."` in `config.json` — a versioned ciphertext,
**[inferred, high confidence]** protected by Electron `safeStorage` → Keychain. **[documented]**
The official 3P documentation marks "no cloud conversation history" as specific to 3P mode, which
confirms from the other side that standard mode (with an Anthropic account) is
server-authoritative.

**The Code tab's transcript is standard Claude Code jsonl, under `~/.claude/projects/`
[observed].** Three lines of evidence, independent and mutually corroborating:

① The path construction inside app.asar (raw fragments extracted with `rg -a`):

```js
mwt="claude-code-sessions", gwt="local_"
function $v(){return process.env.CLAUDE_CONFIG_DIR}
var tn=Be(()=>($v()??X.join(Qr.homedir(),".claude")).normalize("NFC"),$v);
X.join(o,"projects",a); await ae.mkdir(l,{recursive:!0});
let u=X.join(l,`${t}.jsonl`); await Ih(u,s);          // ← the line that persists it
pr=()=>X.join(Zt.CLAUDE_CONFIG_DIR??X.join(Qr.homedir(),".claude"),"projects")
```

② The server-side copy is a **mirror**, not the authority — also from app.asar:

```
"sessionStore cannot be used with persistSession: false -- the storage adapter
 requires local writes to mirror from."
"ensure the subprocess CLAUDE_CONFIG_DIR matches the parent (same path, same
 separators) or transcript_mirror frames will be dropped."
```

③ Dynamic cross-corroboration. On this machine `~/.codex/external_agent_session_imports.json`
(Codex's ledger of external-agent imports) records these source paths verbatim:

```
/Users/nana/.claude/projects/-Users-nana-Projects-papers-notes/2880ba9c-504a-4be7-a44a-a2f6ff6d9304.jsonl
/Users/nana/.claude/projects/-Users-nana-Projects-papers-notes/631c45fa-9174-4057-a399-1c3c105d0cf1.jsonl
/Users/nana/.claude/projects/-Users-nana-Projects-papers-notes/ea6537b3-9381-4ab4-be0a-f5f48662eded.jsonl
```

Those three basenames are exactly the `cliSessionId` of three desktop sidecars (3/3 hit):

| Sidecar file | `cliSessionId` |
|---|---|
| `local_bd60b801-08eb-4edc-82bf-ca43be028375.json` | `2880ba9c-504a-4be7-a44a-a2f6ff6d9304` |
| `local_631c45fa-9174-4057-a399-1c3c105d0cf1.json` | `631c45fa-9174-4057-a399-1c3c105d0cf1` |
| `local_1ccc2de6-ebf0-4247-a3ad-27ccbf9a2166.json` | `ea6537b3-9381-4ab4-be0a-f5f48662eded` |

The sidecar itself carries **no conversation text**, only metadata (sample at
`/tmp/agit-desktop/claude-desktop/sidecar-local_XXXX.json`):

```
sessionId cliSessionId cwd originCwd lastFocusedAt createdAt lastActivityAt
model effort isArchived title titleSource permissionMode remoteMcpServersConfig
chromePermissionMode completedTurns bridgeSessionIds alwaysAllowedReasons
sessionPermissionUpdates classifierSummaryEnabled spawnSeed
```

The path is `~/Library/Application Support/Claude/claude-code-sessions/<accountId>/<orgId>/local_<uuid>.json`.
This machine has 7 sidecars, and their `cliSessionId` values hit **0/7** against the current
`~/.claude/projects/` (33 transcripts) — the transcripts were already cleared by Claude Code's own
retention policy (`~/.claude/.last-cleanup` updates daily). **That is an important observation in
itself: the sidecar index and the transcript have independent lifecycles, and the two drift
apart.**

**The Cowork / Chat surface [observed].** Under `local-agent-mode-sessions/<acct>/<org>/` each
session is a `local_<uuid>.json` plus a working directory of the same name, and that directory
holds `audit.jsonl` (462 KB / 32 lines on this machine, every line carrying `_audit_hmac` +
`_audit_timestamp`), `.audit-key` (51 bytes), `uploads/`, `outputs/`, and a private HOME
`. claude/projects/<a very long slug>/<uuid>.jsonl` (one 27-line sample on this machine).
**[documented]** The official 3P documentation says of `audit.jsonl`, verbatim: "Each entry is
HMAC-chained to the previous one so edits or deletions are detectable; the companion `.audit-key`
file holds the per-session signing key, encrypted via the OS keychain" — which matches what was
observed.

The VM layer: `vm_bundles/claudevm.bundle/` (6.7 GB) holds `rootfs.img`, `sessiondata.img` and
`efivars.fd`. For a Cowork session running inside the VM, the transcript lives inside
`sessiondata.img` and is **unreachable** on the host filesystem.

### 1.4 Sync metadata (a direct answer to task question 3)

| Application | Sync metadata | Direction of derivation |
|---|---|---|
| ChatGPT Desktop | `local_thread_catalog_sync_state(watermark_updated_at, initial_build_complete, observation_sequence, last_full_reconciled_at)` + `local_thread_catalog.missing_candidate` + `local_thread_catalog_metadata.catalog_revision` **[observed]** | **local file → index**. `thread/list` defaults to scan-and-repair **[documented: protocol schema]** |
| Claude Desktop Code tab | `bridge-state.json`: `{environmentId: "env_01XXXXXXXXXXXXXXXXXXXXXXXX", remoteSessionId: "session_01XXXXXXXXXXXXXXXXXXXXXX", localSessionId: "local_ditto_<org>", processedMessageUuids: [], pendingProcessedAcks: []}`; and `bridgeSessionIds: ["session_01YYYYYYYYYYYYYYYYYYYYYY"]` in the sidecar **[observed]** | **local write → server mirror** (`transcript_mirror` frames) **[observed: app.asar]** |
| Claude Desktop, the claude.ai surface | None (no conversation text stored locally, so there is no cursor to speak of) | Server-authoritative |

The key conclusion: **nowhere is there a dirty flag or a revision counter by which "the server
overwrites local".** ChatGPT Desktop's direction is local → index; the Claude Desktop Code tab's
direction is local → server mirror. That is why the worry that "anything written locally is
overwritten at the next sync" **does not hold** on these two surfaces.

---

## 2. Official entry points (task question 4)

### 2.1 `claude://resume?session=<uuid>` — Claude Desktop's official ingestion entry point **[observed]**

This is the most valuable finding of the survey. The `claude:` scheme is registered with
LaunchServices (`bindings: claude:`), and the route enum in app.asar is:

```js
e.ClaudeCodeDesktop="claude-code-desktop", e.Code="code", e.Design="design",
e.Resume="resume", e.Cowork="cowork", e.LocalSessions="local_sessions"
```

The `Resume` branch, verbatim:

```js
case Fr.Resume:{
  const l=i.searchParams.get("session");
  return l && C5.test(l)
    ? (g.info(`Resume deep link: importing CLI session ${l}`),
       Ed().then(u=>u.importCliSession(l), ...))
    : (g.warn("Resume deep link: missing or invalid session",{sessionId:l}),!1)
}
```

where `C5=/^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/i`.

The head of `importCliSession` itself:

```js
importCliSession(e){
  if(!n.isUuid(e)) throw new Error(`Invalid CLI session id: ${e}`);
  if((!this.currentAccountId||!this.currentOrgId) && await this.initializeWithAccount(), ...){
    const y=this.lastInitAuthFailed
      ? "Unable to import session: account information is unavailable because your
         sign-in has expired. Please sign in to the desktop app again."
      : "Unable to import session: account information is unavailable. Check your
         network connection and try again.";
    ... throw new Error(y)
  }
  const r=`${...`   // → LOCAL_SESSION_PREFIX + uuid, i.e. local_<uuid>
```

Next to it sits `importAllCliSessions` (bulk ingestion).

**What this means**: the desktop application **ships** an entry point that says "ingest one Claude
Code session by CLI session id", and while ingesting it resolves accountId/orgId itself and builds
the sidecar itself. So agit never has to forge
`claude-code-sessions/<accountId>/<orgId>/local_*.json` — it writes the transcript into
`~/.claude/projects/` (which agit already does), then runs
`open 'claude://resume?session=<uuid>'`.

The limits, stated honestly: this route is **publicly undocumented** and was read out of the
shipped bundle; success and failure show up only in the application's own UI and its own telemetry
(`desktop_code_deeplink_received`, `desktop_code_deeplink_resume_failed`), so **agit cannot
observe the outcome**.

The `Fr.Code` branch of the same enum carries the telemetry fields `has_prompt` / `has_folder` /
`has_file`, which **[inferred, medium confidence]** indicates an entry point of the form
`claude://code?prompt=...&folder=...&file=...` that "opens a new session with the prompt
pre-filled". Its exact parameter names are not established (the full `searchParams.get` list
offers `prompt`, `q`, `composer`, `start`, `mode` and `surface` as candidates), so **do not depend
on it before it is confirmed**.

### 2.2 `codex app-server` — ChatGPT Desktop's official protocol **[documented: self-described schema]**

```
codex app-server --listen <stdio://|unix://PATH|ws://IP:PORT>
codex app-server daemon {bootstrap,start,restart,stop,version,
                         enable-remote-control,disable-remote-control}
codex app-server proxy      # proxy stdio to the running control socket
codex remote-control {start,stop,pair}
codex app [PATH]            # open a workspace in the desktop application
```

The available methods relevant to agit: `thread/start`, `thread/resume`, `thread/fork`,
`thread/list`, `thread/read`, `thread/inject_items`, `thread/metadata/update`, `thread/name/set`,
`thread/archive`, `thread/rollback`, `turn/start`.

**agit should still not go through app-server.** The `thread/resume` doc says "Prefer using
thread_id whenever possible", and resuming by `thread_id` presupposes exactly that the rollout
file is already under `~/.codex/sessions/` — which is what agit's existing `codex::install` does.
Going through JSON-RPC drags in a `--listen`/daemon lifecycle, an `initialize` handshake and a
dependency on an experimental protocol version, and buys only "one fewer scan". Writing the file
is the smaller, steadier path, and the CLI and the desktop side share it.

### 2.3 An existing cross-vendor import precedent **[observed]**

`~/.codex/external_agent_session_imports.json` and `~/.codex/claude-cowork-import-history.json`
show that **Codex already imports Claude sessions officially**; the ledger fields are
`{source_path, content_sha256, imported_thread_id, imported_at, source_modified_at}`. The latter
specifically records imports out of Claude Desktop's Cowork directory:

```json
{"sourceId":"local_9f4b043d-bad7-4c57-b1fb-6c57e68dcbf1",
 "sourcePath":".../local-agent-mode-sessions/.../.claude/projects/.../97bf5cb4-....jsonl",
 "importedThreadId":"019f1bad-572c-7162-97ae-cd434872f63c",
 "contentSha256":"90ac967c...","importedAtMs":1782875773804}
```

Two things follow for agit. First, "an idempotent import ledger keyed by `content_sha256` +
`source_modified_at`" is a proven pattern, and it shares its reasoning with agit's chained hashes.
Second, agit's imports and Codex's **see each other's output**, so `agit doctor` is worth a line
saying "this session is a copy Codex already imported", otherwise the same session gets adopted
twice.

### 2.4 Entry points that definitively do not exist

- Claude Desktop has **no** session export/import feature. **[documented]**
  anthropics/claude-code#75185 is a request for exactly that feature, and the community's approach
  in the thread is **a script that reverse-engineers the `local_*.json` sidecar**. agit should not
  take that road (see §3.2).
- Neither application has a public REST API that **creates a session**. The Anthropic Messages API
  and the OpenAI Responses API are both stateless inference interfaces; what they create never
  appears in a desktop application's session list.
- Claude Desktop's MCP (`claude_desktop_config.json`, the `.dxt` / `.mcpb` extensions) **adds
  tools to a session**; it does not **create a session**. Wrong direction.

---

## 3. Feasibility verdicts

### 3.1 ChatGPT Desktop: read and write are both feasible, and no new adapter is needed

**Read: already works.** What agit's existing `codex` adapter reads —
`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` + `state_5.sqlite.threads` — is the desktop
application's Codex storage. This machine has 108 rollouts / 124 threads, with `source` taking the
values `vscode`(87) / `cli`(21) / subagent JSON(16). **`source` has no `desktop` value**, so the
`threads` table cannot tell "this one was opened from the desktop" from "this one was opened from
the CLI" — `local_thread_catalog` is the desktop's footprint. Which means `agit log` **cannot**
label the originating surface reliably, and **need not**: they are the same session.

**Write: feasible.** The mechanism is identical to `--as codex`. The grounds are the
scan-and-repair semantics in §1.2.

**Robustness: good, but the desktop side is strictly weaker than the CLI.** Two concrete risks:

1. If the application calls `thread/list` with `useStateDbOnly: true` (for list speed, the only
   reason that parameter exists), a rollout agit has just written **does not appear in the
   sidebar** until the next full scan. This is **[inferred, medium confidence]** — the value the
   application actually passes was not observed. The mitigation is not to write `state_5.sqlite`
   (that is a database the application has open; agit holding a write lock carries a real
   corruption risk, and the comments in `codex_index.rs` already lay down the "open read-only"
   rule) but to **also hand the user the last-resort command that always works**,
   `codex resume <id>`.
2. The `state_5.sqlite` filename carries a version number, which `codex_index.rs` already handles
   (take the highest number; fall back to scanning files when the schema is incomplete).
   `codex-dev.db` is the same, but agit never needs to touch it.

**Design conclusion: `chatgpt-desktop` is an alias for `codex`, not a new runtime.** Two or three
aliases in `normalize()`, one explanatory line in `doctor`, `RUNTIMES` unchanged, the set of
adapters unchanged. This is the one place in this report solved by "deleting a requirement" rather
than by adding code.

### 3.2 Claude Desktop: reading already happens, writing goes through a hand-off, not a forgery

**Read: the Code tab already works (and agit reads it today without knowing).** The Code tab's
transcript lands at `~/.claude/projects/<slug>/<cliSessionId>.jsonl`, and
`claude_code::all_sessions()` walks exactly `~/.claude/projects/*`. So **`agit import` already
adopts Claude Desktop Code tab sessions today** — agit merely mixes them in with CLI sessions and
draws no distinction.

That is an opportunity, not a problem: reading the sidecars (read-only, 7 files, a few hundred
bytes) gets `title` (named by the user, and more accurate than what `gist()` derives), `model`,
`permissionMode`, `completedTurns` and `bridgeSessionIds` for free. **Proposal**: while filling a
`SessionRef`, `claude_code::sessions_for` also looks in the sidecar directory once and uses the
sidecar's `title` on a hit. A missing sidecar is the normal case (0/7 hit in the other direction
on this machine), so this must be pure enhancement — missing means ignored.

**Read: Cowork / Chat (non-VM) is feasible, but `parse` needs extending.** Those transcripts are
Claude Code jsonl with three extra record types **[observed]**:

| `type` | Fields | Content | Must map to |
|---|---|---|---|
| `attachment` | `attachment.{type,addedNames,addedLines,...}` | Runtime-injected deltas to the tool / skill / agent listing (`deferred_tools_delta`, `agent_listing_delta`, `mcp_instructions_delta`, `skill_listing`) | `Other` |
| `last-prompt` | `lastPrompt`, `leafUuid` | **A copy of the user prompt** — one sentence appears 4 times on this machine | `Other` |
| `queue-operation` | `operation`(`enqueue`/`dequeue`/`remove`), `content`, `reason` | The `content` of an `enqueue` **is the user prompt verbatim**; a `remove` carrying `reason: absorbed_mid_turn` is what the user typed in while the agent was running a tool, absorbed into the current turn — it **never** appears again as a `user` record | `enqueue`/`dequeue` → `Other`; `remove`+`absorbed_mid_turn` → `UserInterjection` (enters the VIEW, opens no turn) |

**This is a ready-made bug shape, the same one the compact summary already produced.** If
`last-prompt` and `queue-operation/enqueue` are treated as `UserPrompt`, one sentence is counted 6
times (1 real + 4 `last-prompt` + 1 `enqueue`), and `gist()` and turn splitting break together.

There is a subtler problem: the `match` in `claude_code::parse` is currently
`{"user" => ..., "assistant" => ..., _ => {}}` — **an unknown type is dropped silently, without
even counting in `dropped`**. That violates the rule laid down at the top of `adapter/mod.rs`
("that loss is **explicit** (it gets reported), not silent"). Change `_ => {}` to emit an
`EventKind::Other`, so that `counts().dropped` is honest. That change has nothing to do with the
desktop applications; it is an independent correctness fix.

**Read: Cowork (VM) is not feasible.** The transcript is inside `sessiondata.img`. Changing that
verdict requires mounting the disk image — outside the "read-only inspection" boundary, and the
format can change at any time (the `vm_bundles` architecture is itself still evolving).

**Read: the claude.ai surface is not feasible.** The empty `queries` array in §1.3 is the whole of
the evidence. Reading it means calling the server API with the user's credentials, which is an
entirely different thing (and the credentials sit in the Keychain).

**Write: feasible, but it must be a "hand-off", not a "forged sidecar".**

There are two roads; the first is the recommended one:

> **Road A (recommended)**: agit writes the transcript into
> `~/.claude/projects/<slug>/<new_uuid>.jsonl` (the existing `claude-code::install`, a byte-level
> rewrite, lossless), then hands over `open 'claude://resume?session=<new_uuid>'`. The desktop
> application validates the UUID itself, resolves account/org itself, and builds the sidecar
> itself. **agit writes not one byte into the desktop application's private storage.**
>
> **Road B (not recommended)**: agit synthesizes
> `claude-code-sessions/<accountId>/<orgId>/local_<uuid>.json` directly.

Three reasons Road B is rejected:

1. **It has to guess an identity.** The path is segmented by `<accountId>/<orgId>`, so agit would
   have to dig the user's Anthropic account and organization UUIDs off the disk and then assert in
   their name that "this session belongs to this account". agit does not get to make that kind of
   assertion.
2. **The format is unstable, and it is migrating.** This directory has been renamed once already
   (`local-agent-mode-sessions` → `claude-code-sessions`, **[documented]** per community
   accounts), and is now moving into a VM disk image. Anything that reads or writes
   `local_*.json` stops working once that migration completes.
3. **It bypasses the application's initialization path.** `importCliSession` runs
   `initializeWithAccount()` and gives a specific error on failure. A hand-written sidecar skips
   all of that and lands in an untested state.

Road A's robustness: **medium, on the good side**. It depends on an undocumented URL route, but
(i) a URL scheme is a public interface registered with LaunchServices, far more stable than a
private directory format; (ii) the only parameter is a UUID, so there is no format coupling; (iii)
on failure the application gives the user a readable error rather than silently doing nothing;
(iv) even if the route disappears one day, the `claude --resume <uuid>` fallback still holds —
**the degradation goes in a direction known to work**.

`mint_id()` already produces UUIDv7, which satisfies the `C5` regex and `isUuid` natively; nothing
has to change.

**Write: Cowork / Chat should not be done.** The HMAC chain in `audit.jsonl` is **designed to make
modification detectable**. Writing into it manufactures a record that reads as "tampered with".
That is not a technical obstacle, it is design intent, and it deserves respect.

---

## 4. A capability model for `--as`

### 4.1 The problem

Without `--as`, `agit clone` defaults to `adapter::RUNTIMES.to_vec()` — **install into every
registered runtime**. The moment a non-resumable target gets into `RUNTIMES`, that fan-out
silently produces something that is installed but cannot be launched. So "capability" has to
become a first-class concept, and **the default target set must be derived from capability, not
from a hand-written list**.

`is_cross_vendor(from, to)` is `normalize(from) != normalize(to)` today, and decides between the
byte rewrite and the IR. That test is **wrong** for the new targets: `claude-desktop` and
`claude-code` share one jsonl format, `chatgpt-desktop` and `codex` share one rollout format.
Comparing by id misjudges them as cross-vendor and throws away encrypted reasoning for nothing —
and the module comment in `domain/install/mod.rs` exists precisely to prevent that.

### 4.2 What gets added to `Adapter` (two methods, one enum, one field retyped)

```rust
/// How far agit can go with a target runtime.
///
/// Deliberately one enum and not several bools: combinations of bools produce
/// meaningless states (can install but cannot parse), while `match` forces every
/// consumer to handle every level explicitly — the same reason `EventKind` splits
/// into two compact variants instead of carrying a `lossy: bool`.
///
/// The order is meaningful (`Ord`): `clone` compares directly when it picks "the best".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// Can only be read in. Nothing installs into it; `--as` rejects it.
    ///
    /// Example: a Claude Desktop Cowork session — the transcript is readable, but
    /// the HMAC chain in `audit.jsonl` is designed to make writes detectable.
    ImportOnly,

    /// Installs a real deliverable (a file plus one hand-off instruction), but the
    /// last step happens in a process agit cannot observe, so **agit must not claim
    /// success**.
    ///
    /// Example: Claude Desktop — the transcript goes into `~/.claude/projects/`, and
    /// ingestion is handed to the application itself through
    /// `claude://resume?session=<uuid>`.
    ExportOnly,

    /// Installs to disk and yields a resume command that is certain to run.
    Resumable,
}

pub trait Adapter {
    // ...existing methods unchanged...

    /// Which level this target reaches.
    ///
    /// **Deliberately no default implementation.** With a `Resumable` default, whoever
    /// adds an adapter and forgets to override it is swept into `clone`'s default
    /// fan-out and produces a session that is installed but cannot be launched — the
    /// very failure this capability model removes. One more line, and the compiler
    /// remembers it for you.
    fn capability(&self) -> Capability;

    /// The on-disk format family of the transcript. **Decides whether installing back
    /// into its own family can take the byte rewrite.**
    ///
    /// Separate from `id()` because several runtimes share one format: the Claude
    /// Desktop Code tab writes Claude Code's jsonl, and the ChatGPT Desktop Codex
    /// surface writes Codex's rollout. Comparing by `id()` judges them cross-vendor
    /// and throws away encrypted reasoning for nothing.
    fn format(&self) -> &'static str;
}
```

Retyping `Installed`'s `resume_cmd` is necessary because `clone` pushes it into `ui::accent()` and
prints it as a command, which for an `ExportOnly` target dresses an explanatory sentence up as an
executable one:

```rust
pub struct Installed {
    pub path: PathBuf,
    pub next: Next,
}

/// What to do once the install finishes.
pub enum Next {
    /// Run this command to resume.
    Resume(String),
    /// agit stops here. This holds **steps that can be followed**, not a sentence
    /// like "manual import may be required" that names no next step.
    HandOff { trigger: String, fallback: String },
}
```

`HandOff` splits into a `trigger` and a `fallback` field rather than one block of free text
because the two differ in kind: `trigger` can fail without agit observing it, `fallback` is known
to work. The UI must be able to **present them separately**, or the user cannot read off "which
one is the steady one".

### 4.3 The registry and the default targets

```rust
/// Every registered name. Used only by `normalize`'s error and by `doctor`'s listing.
pub const RUNTIMES: &[&str] = &["claude-code", "codex", "claude-desktop"];

/// What `clone --as` installs into when given no argument.
///
/// The test is capability, not a list — adding a runtime later needs no edit here,
/// and no forgotten edit can sweep a non-resumable target into the default fan-out.
pub fn default_targets() -> Vec<&'static str> {
    let mut v: Vec<_> = all()
        .into_iter()
        .filter(|a| a.capability() == Capability::Resumable)
        .map(|a| a.id())
        .collect();
    v.sort();
    v
}
```

**`chatgpt-desktop` does not enter `RUNTIMES`.** It enters only `normalize`:

```rust
pub fn normalize(s: &str) -> Result<&'static str> {
    match s.trim().to_ascii_lowercase().as_str() {
        "claude-code" | "claude" | "cc" => Ok("claude-code"),
        "codex" | "cx" => Ok("codex"),
        // ChatGPT Desktop (com.openai.codex) embeds the Codex engine and reads and
        // writes the same CODEX_HOME. It is not another runtime, it is another
        // interface onto the same one, so it normalizes to codex instead of opening
        // a new adapter.
        "chatgpt-desktop" | "chatgpt" | "chatgpt-app" | "codex-app" => Ok("codex"),
        "claude-desktop" | "claude-app" => Ok("claude-desktop"),
        other => anyhow::bail!(
            "unknown runtime `{other}`, registered: {} (`agit doctor` shows what each reaches)",
            RUNTIMES.join(", ")
        ),
    }
}
```

`is_cross_vendor` is renamed and reimplemented on `format()`; the name now describes what it does:

```rust
/// Whether converting from `from` to `to` is lossy.
///
/// The test is the **transcript format family**, not the runtime name. Identical
/// formats take the byte rewrite (`domain::install::rewrite_identity`), and encrypted
/// reasoning and compact boundaries all pass through unchanged.
pub fn is_lossy_conversion(from: &str, to: &str) -> bool {
    match (get(from), get(to)) {
        (Ok(a), Ok(b)) => a.format() != b.format(),
        // An unrecognized name counts as lossy: better one loss reported too many
        // than one missed.
        _ => true,
    }
}
```

The `match runtime` inside `domain::install::rewrite_identity` also becomes `match format`.
Because `claude-desktop`'s `format()` returns `"claude-code"`, the two existing arms need **not
one line changed**.

What each adapter declares:

| `id()` | `format()` | `capability()` |
|---|---|---|
| `claude-code` | `"claude-code"` | `Resumable` |
| `codex` | `"codex"` | `Resumable` |
| `claude-desktop` | `"claude-code"` | `ExportOnly` |

### 4.4 What `clone --as` does with a non-`Resumable` target

Three rules:

1. **Without `--as`, never touch a non-`Resumable` target.** `default_targets()` already
   guarantees this. Mention at the end that they exist, or the user never learns the option is
   there.
2. **An explicit `--as <ImportOnly target>` must fail, with exit code `Usage`.** The user asked
   for something that cannot be done: a usage error, not a runtime failure.
3. **An explicit `--as <ExportOnly target>` installs, hands off, then forces `no_launch` true.**
   `clone` today calls `launch(&pick.1.resume_cmd)` — an `ExportOnly` target has nothing to
   launch, so the `launch` branch must be taken only on `Next::Resume`.

The output of `--as claude-desktop` (written in the register of the existing `ui::ok` /
`ui::warning` / `ui::hint`):

```
  ✓ transcript written  ~/.claude/projects/-Users-nana-Projects-payments/019f9a81-....jsonl
                        (native format, complete — encrypted reasoning and compact
                        boundaries are all there)

  ⚠ claude-desktop cannot be resumed by agit directly. The desktop application's session
    list is an index it maintains itself (claude-code-sessions/<account>/<org>/local_*.json),
    and agit does not forge that index: it has no public format, it has been renamed once,
    and it is moving into a VM disk image.

  let the desktop application ingest this one (it builds its own index entry):
      open 'claude://resume?session=019f9a81-...'
      the desktop application must be signed in. on failure it raises the error inside
      the application, where agit cannot see it, so a 0 exit from this command does not
      mean success.

  the path that is certain to work (bypassing the desktop application):
      (cd /Users/nana/Projects/payments && claude --resume 019f9a81-...)
```

When `--as` is given an `ImportOnly` target:

```
  ✗ cannot install into claude-desktop-cowork: it can only be read.

    The audit.jsonl in a Cowork session directory is HMAC-chained — the **purpose** of
    that chain is to make writes detectable. agit does not write into it.

    What works is the other direction:
      agit import --from claude-desktop-cowork   adopt it into version control
    To get some context into the desktop application, pick a target that can ingest it:
      agit clone <agent> --as claude-desktop
```

For `--as chatgpt-desktop` (normalized to `codex`, but say plainly what happened):

```
  ℹ chatgpt-desktop is codex: ChatGPT.app (com.openai.codex) embeds the codex-cli
    engine and shares ~/.codex with the CLI. installing as codex.

  ✓ written  ~/.codex/sessions/2026/08/01/rollout-2026-08-01T00-31-02-019f....jsonl
      codex resume 019f...                        ← always works in the terminal

  the desktop sidebar ingests this one on its next scan of ~/.codex/sessions.
  to open this workspace in the application now:  open -a ChatGPT /Users/nana/Projects/payments
```

### 4.5 The capability report in `agit doctor`

`doctor` today only runs `which(cli())` per runtime. Add a capability column, and probe the
desktop applications separately as **another interface onto the same runtime** — because "the CLI
is not installed but the desktop application is" is a real state, and `doctor` currently reports
it as "unavailable", which misleads the user:

```
agit doctor

  [✓] runtime claude-code     resumable     ~/.local/bin/claude
  [✓] runtime codex           resumable     ~/.nvm/.../bin/codex  (codex-cli 0.144.6)
  [✓]   └ ChatGPT Desktop                   /Applications/ChatGPT.app 26.727.40816
                                            (embeds codex-cli 0.146.0-alpha.9.2, the same
                                            ~/.codex)
  [!] runtime claude-desktop  export only   /Applications/Claude.app 1.24012.9
                                            transcript goes to ~/.claude/projects/,
                                            ingestion runs through
                                            claude://resume?session=<uuid>, whose result
                                            agit cannot observe
```

`available()` has to be implemented directly for desktop targets (the PATH lookup behind `cli()`
does not apply to an app bundle). This is **inherently platform-specific** —
`/Applications/Claude.app` holds only on macOS. The convention: on an unsupported platform
`available()` returns `false` rather than guessing a path. `doctor` reports "unavailable" and
`--as` reports "this target is not supported on {os}"; both beat guessing a path that does not
exist.

### 4.6 What this design explicitly does not do

- **No second install path such as `install_desktop()`.** `claude-desktop::install` reuses
  `claude-code::install`'s disk-writing logic; only `Next` differs.
- **No writes to `state_5.sqlite` / `codex-dev.db` / `local_*.json`.** All three are databases or
  indexes someone else has open, and all three are derived data. `codex_index.rs` already lays
  down the "open read-only, fall back when the schema is incomplete" rule, and this design does
  not break it.
- **No app-server JSON-RPC client.** The reason is in §2.2: it buys one fewer scan, at the cost of
  a dependency on an experimental protocol plus daemon lifecycle management.

---

## 5. Fidelity analysis

### 5.1 `claude-desktop` (`format() == "claude-code"`)

| Source | Path | Result |
|---|---|---|
| `claude-code` / `claude-desktop` | `rewrite_identity` byte rewrite | **Lossless**. `thinking` / `redacted_thinking`, `isCompactSummary` records, the `uuid`/`parentUuid` chain and every unknown field all pass through unchanged |
| `codex` | `parse → render` through the IR | Lossy; the losses are listed below |

The concrete losses in `codex → claude-desktop` (derived from reading `claude_code::render`,
**[observed: source]**):

- `EventKind::Other` (Codex's encrypted `reasoning`) — `continue`, dropped. Counted in
  `counts().dropped`.
- `EventKind::CompactFiltered` — `continue`. This is **right**: it is the source runtime's
  context-management trace, not conversation content. The original is in the canonical layer.
- `EventKind::TurnEnd` (Codex's `task_complete`) — `continue`. Claude Code has no equivalent.
- `EventKind::ToolUse` — **degraded to one plain-text assistant message** whose content is only
  the tool name. `render` maps `ToolUse` to `("assistant","assistant")` with `content` as
  `[{type:"text",...}]`, **not a `tool_use` block**. So in the resumed session, "which tools the
  agent called" has become prose.
- `EventKind::FileEdit` — the same, and **`paths` is dropped entirely**. `render` takes only
  `e.text` (the tool name); `e.paths` is never written out. This is the heaviest single loss in
  cross-runtime conversion, because "which files this session changed" is one of agit's core
  signals.

**This loss is not specific to the desktop target** — `--as claude-code` behaves this way today.
The desktop target only inherits it.

### 5.2 `codex` / ChatGPT Desktop

| Source | Path | Result |
|---|---|---|
| `codex` | `rewrite_identity` | **Lossless**. `payload.encrypted_content` is preserved unchanged (this is exactly what the `install_touches_only_identity` test guards) |
| `claude-code` / `claude-desktop` | IR | `thinking` dropped (counted in `dropped`); `CompactSummary` dropped; `ToolUse → function_call` but with `arguments: "{}"` (every argument lost); `FileEdit → patch_apply_end` with `changes` set to an empty object (**the paths survive, the diff is lost**)|

The asymmetry is worth naming: **the Codex direction keeps `paths`, the Claude Code direction does
not.** It follows from `codex::render` having a `changes` map where `claude_code::render` has no
corresponding structure. To narrow the cross-vendor loss, changing `claude_code::render` so that
`FileEdit` emits a real `tool_use` block (carrying `input.file_path`) has the best return —
**unrelated to the desktop applications, an independent improvement**.

### 5.3 Where a desktop target is necessarily worse than a CLI target

These four are structural, not questions of implementation quality:

1. **agit cannot confirm success.** A CLI target delivers a command; the user runs it and success
   or failure is visible at once. A desktop target delivers a hand-off, and the outcome sits in
   another process's UI. `Next::HandOff` carries a `fallback` exactly so that the user has a path
   they can verify themselves when the hand-off fails.
2. **The sidebar index and the transcript have two lifecycles.** The 7 sidecars on this machine
   matching 0/7 against existing transcripts is the evidence of that drift. agit writes the
   transcript, the application writes the index, and each side is cleaned by its own retention
   policy.
3. **The Cowork/VM side refuses writes by design.** The purpose of the HMAC audit chain is to make
   modification detectable; `sessiondata.img` puts the content out of the host's reach entirely.
4. **The server mirror never holds this copy.** Claude Desktop `transcript_mirror`s the local
   transcript to the server; a transcript agit injected has no server counterpart, so anything
   rendered from the server (cross-device, full-text search) cannot see it. **[inferred, medium
   confidence]** — the application's concrete behavior for "a local session with no server
   counterpart" was not observed.

---

## 6. Open questions, and what would settle them

Ordered by impact on the design:

1. **What does the application pass for `useStateDbOnly` when it calls `thread/list`?** Impact: it
   decides whether a rollout agit has just written shows up in the ChatGPT Desktop sidebar at
   once. **What would settle it**: write a synthetic rollout under `~/.codex/sessions/` and watch
   the sidebar (needs write permission, out of scope here); or a deeper static analysis of
   `Contents/Resources/codex` and its TS/Swift call sites. **The current design does not depend on
   it** — the `codex resume <id>` fallback is unaffected either way.
2. **Does `claude://resume?session=<uuid>` really put an entry in the sidebar end to end?**
   Impact: whether `claude-desktop` is truly `ExportOnly` or close to `Resumable`. **What would
   settle it**: `open 'claude://resume?session=<some existing uuid>'` and then look at the sidebar
   (needs triggering application behavior, out of scope here). The static evidence (UUID
   validation → `importCliSession` → account/org resolution → `local_` prefix construction) is
   already complete, but has never been run through.
3. **The exact parameter names of `claude://code?...`.** Impact: whether a second export path
   exists that "opens a new session with the prompt pre-filled" (useful when there is no
   transcript and only an intent needs handing over). The telemetry fields on the `Fr.Code` branch
   are `has_prompt` / `has_folder` / `has_file`, and the full `searchParams.get` candidate list
   holds `prompt`, `q`, `composer`, `start`, `mode`. **What would settle it**: fully deobfuscate
   the minified `Fr.Code` segment. That was not carried to certainty here, so **do not depend on
   it before it is confirmed**.
4. **Is the Claude Desktop Code tab transcript always under `~/.claude/projects/`?** In app.asar
   `Zt.CLAUDE_CONFIG_DIR=n` is assignable (Cowork/VM takes that path), so "a host-cwd session is
   redirected too" is possible. The 3/3 cross-corroboration on this machine supports "not
   redirected", but the sample dates from 2026-06-30. **What would settle it**: open a Code tab
   session in the desktop application and see where the file appears (needs creating a session
   through the UI, out of scope here).
5. **Does `claude-code-sessions/` in standard mode (with an Anthropic account) also carry a
   per-session working directory?** The official 3P documentation says "in the same per-session
   layout", but this machine has only `local_*.json` and no working directory. **[inferred, low
   confidence]** The difference may come from 3P versus standard mode, or from the version. The
   impact is small: agit does not intend to read or write that directory.

---

## 7. Recommended landing order

Ordered so that "every step is worth landing on its own, and none depends on the next":

1. **Add the `chatgpt-desktop` aliases to `normalize` + add desktop-application probing to
   `doctor`.** Zero risk, purely explanatory, and it settles the "what about ChatGPT Desktop"
   question immediately.
2. **Change `_ => {}` in `claude_code::parse` to emit `EventKind::Other`, and handle `attachment`
   / `last-prompt` / `queue-operation` explicitly.** An independent correctness fix; without it,
   adopting a Cowork session counts every prompt 6 times over.
3. **`Capability` + `format()` + `default_targets()` + `is_lossy_conversion`.** Pure refactoring,
   due even with only two CLI runtimes, because `cursor` / `kiro` need it just as much when they
   land.
4. **`Installed.next: Next`.** Lands together with 3; `clone`'s printing and its `launch` branch
   change at the same time.
5. **The `claude-desktop` adapter.** Last, because it is the only part that depends on an
   undocumented interface, and its value (that deep link) can only be presented correctly once
   1-4 are all in place.

---

## Appendix: forensic material in `/tmp/agit-desktop/`

```
claude-desktop/
  EVIDENCE-no-local-conversation-cache.txt        the empty queries array, verbatim
  EVIDENCE-code-tab-writes-claude-projects.txt    three evidence chains, raw asar fragments
  localstorage-keys.txt                           the full list of 80 keys
  sidecar-local_XXXX.json                         a sidecar sample (title / mcp config redacted)
chatgpt-desktop/
  EVIDENCE-local-authoritative.txt                embedded engine version, scan-and-repair verbatim
  app-server-write-path-schemas.json              ThreadResume/Fork/InjectItems/List/Start
  codex_app_server_protocol.v2.schemas.json       the full protocol schema (471 KB)
  codex-dev.db.schema.sql                         the desktop sidecar index schema
  local_thread_catalog-sample.csv                 a catalog sample (titles redacted)
  local_thread_catalog_sync_state.csv             the sync watermark, unchanged
codex-app-server-schema/                          full `generate-json-schema` output
raw/                                              read-only copies of the SQLite / LevelDB
```
