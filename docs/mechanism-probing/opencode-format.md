# OpenCode's session storage format and adapter contract

> Probe record + adapter design spec. The forensic material is in `samples/opencode/`, indexed by
> `samples/README.md`.
> Evidence labels: **[observed]** = seen directly on this machine (with the command / sample file);
> **[documented]** = official source or CLI help; **[inferred]** = derived, with a confidence level.
> One for one with the observed / documented / inferred labels of the sibling documents.
>
> Sampled on this machine's Linux (`opencode --version` = 1.18.13); the database is a full copy of
> `~/.local/share/opencode/opencode.db` (139 sessions / 3419 messages / 14883 parts).
> Sandbox discipline: every sqlite3 read goes against a copy (`cp opencode.db{,-wal,-shm}` →
> `/tmp/opencode/opencode-probe/`), every write-side experiment (`opencode import`) runs in an
> isolated scratch `HOME`+`XDG_DATA_HOME`, and neither the real database nor the real HOME was
> touched by any probe.

## 0. Conclusion in two sentences

**OpenCode is readable and writable; the capability is `Resumable`.** Sessions live in a single
SQLite database (the jsonl-layout era is over), but the official byte path is complete:
`opencode export <id>` produces deterministic JSON, `opencode import <file>` writes idempotently to
the database and accepts a full id remint, `opencode --session <id>` resumes. The install recipe =
remint every id, re-render the export JSON → `opencode import` in the target directory.

**Rows are not append-only — the assistant message in flight, and its parts, are rewritten in
place** ([observed], line-by-line diff of two copies, see §3). agit's continuity check "live =
committed prefix + appended" has to become "draw the line at the last row past its terminal state",
leaving the tail for the next commit.

---

## 1. Storage layout

```text
~/.local/share/opencode/opencode.db        SQLite, WAL mode ([observed]: -wal/-shm companion files)
~/.local/share/opencode/opencode.db-wal    20 MB on this machine (at observation time)
~/.local/share/opencode/log/               runtime logs
~/.local/share/opencode/snapshot/<project_id>/  per-project git snapshots (for revert/undo)
~/.local/share/opencode/storage/           plugin directories only (agent-usage-reminder, oh-my-openagent);
                                           the classic storage/session/<project>/<id>.json layout is gone ([observed])
```

Only four tables carry a session ([observed], full CREATE TABLE statements in
`samples/opencode/schema.sql`):

- `session(id 'ses_…' PK, project_id → project.id, workspace_id, parent_id, slug, directory,
  title, version, summary_*, cost, tokens_*, revert, agent, model, time_created/time_updated,
  time_compacting, time_archived)`
- `message(id 'msg_…' PK, session_id, time_created, time_updated, data JSON)`
- `part(id 'prt_…' PK, message_id, session_id, time_created, time_updated, data JSON)`
- `project(id PK, worktree, …)`; `project.id` is 40 hex (inferred: a hash of the worktree path).

There are also the two event-sourcing tables `event`/`event_sequence` (`message.part.updated.1`
×33905, `message.updated.1` ×12561, `session.updated.1` ×3970, [observed]), plus `todo`,
`session_share`, `permission`, `session_context_epoch`, `session_message` and others.
**`session_message` has 0 rows in this database** ([observed]) — the schema is there, the data is
not; a listing query must not depend on it, and the `schema_ok` defensive check has to tolerate it.

`resolve(session_id)` is therefore a direct PK lookup [observed]: `SELECT … FROM session WHERE
id = ?`, O(log n).

## 2. Row model and part-type inventory

### 2.1 message.data

Two roles ([observed], `samples/opencode/message-and-part-samples.json`):

```json
user:      {"role":"user","time":{"created":…},"agent":"build",
            "model":{"providerID":"einsia","modelID":"kimi-k3"},"summary":{"diffs":[]},
            "tools":{…}, "system":"…", "variant":"…"}        // tools/system/variant optional
assistant: {"parentID":"msg_…","role":"assistant","mode":"build","agent":"build",
            "path":{"cwd":"…","root":"…"},"cost":0,"tokens":{…},"modelID":"kimi-k3",
            "providerID":"einsia","time":{"created":…,"completed":…},"finish":"stop"}
```

Key points:

- Times are uniformly epoch milliseconds (13-digit integers). [observed]
- **`path.cwd` is the per-message launch directory, `path.root` is the project worktree** — `/` for
  the global project ([observed], two sessions carrying the two values). Model attribution is
  **per message** (`modelID`/`providerID`), and a user message also carries the `model` snapshot of
  the moment. It goes into `Event.model` rather than at session level, matching the conclusion in
  `cursor-kiro-formats.md` §3.2.
- On an assistant message in flight, `finish`/`tokens`/`time.completed` are filled in later (§3).
- `summary.diffs` hangs off the user message and references an on-disk diff [inferred, medium
  confidence]; it does not enter the IR.

### 2.2 The full part-type set

[observed] whole-database histogram (`samples/opencode/part-type-histogram.txt`):

```text
tool        3838   {type,tool,callID,state:{status,input,output?,title?,metadata?,time{start,end},error?}}
step-start  3011   {type, snapshot?}          ← the start of a step; snapshot is a 40-hex git tree hash
step-finish 2962   {reason,type,tokens,cost,snapshot?}   reason ∈ stop 297 / tool-calls 2663 / length 2
reasoning   2460   {type,text,time{start,end}}           ← plaintext reasoning, not encrypted
text        1922   {type,text,time?,metadata?,synthetic?}
patch        599   {type,hash,files[]}                   ← file-change receipt; hash points at the snapshot/ sidecar
file          86   {type,url(data:base64…),mime,filename}  ← attachment, base64 inline, largest 8.1 MB
compaction     5   {type,auto,overflow,tail_start_id?}   ← §7
```

The tool state machine `state.status`: `completed` 3798, `error` 37, **`running` 3** (residue of
interrupted sessions). Tool output is inlined **in full** in `state.output` (largest single part in
this database 8.1 MB), and `state.metadata` holds another copy of `output`+`exit`+`truncated`.
[observed]

[documented] (the sanitize switch in `cli/cmd/export.ts` of the `anomalyco/opencode` source, dev
branch) the type set also holds **`subtask` / `snapshot` / `agent`**, none of which occurs in this
database; on an unknown type the parser falls back to counting it in `dropped` (as
`unknown_record_types_are_counted` in `adapter/codex.rs` does).

### 2.3 Sort key

`(session_id, time_created)` **has no duplicate group on either part or message** ([observed]:
`GROUP BY session_id,time_created HAVING count(*)>1` returns 0 rows on both tables), and a part's
`time_created` is **strictly greater** than its host message's (0 rows smaller, 0 rows equal). The
canonical order is `(time_created, kind, id)`; the kind order (message before part) is only a fuse
for the same millisecond — a deterministic total order that does not depend on rowid. [observed]

## 3. Row mutability: the decisive experiment for the continuity check

The question: can rows already written for a "live session" in the DB be rewritten or reordered?
The strictly append-only premise of the jsonl runtimes has to be measured here.

[observed] method: while this session (ses_032345905ffefk87XdLu8d1wjW) was running, a full
db+wal+shm copy was taken at 01:22 and at 01:45, then every row already present in the first copy
was compared row by row. Results (full text in `samples/opencode/mutation-evidence.txt`):

- message, 3 rows: 1 unchanged; 1 (user) **changed in time_updated only**; 1 (the assistant in
  flight) changed in data — `finish: null→"tool-calls"`, `tokens` filled in from all zeros to the
  measured values, `time.completed` added.
- part, 14 rows: 12 unchanged; 2 (parts of that same message in flight) changed in data — text
  accumulating as it streams, tool `state.status running→completed` gaining output/metadata.
- **Zero deletions, zero reordering.**

Conclusions ([inferred], high confidence; every change point is corroborated by an `*.updated.1`
event-table row as the normal path):

1. **A row past its terminal state never changes** (terminal state = the assistant message's
   `finish` is non-empty, its tool parts' `status ∈ {completed,error}`).
2. **The trailing assistant message still being written, and its parts, can be rewritten in place.**
3. `time_updated` is a noise column (a user message is touched even with no data change), so
   **every hash and every comparison must exclude it**.

→ agit's snapshot line-drawing rule: **the boundary is the last terminal-state assistant message**;
the incomplete tail after it is not committed and the next read overwrites it. The continuity
check = committed and live are byte-for-byte equal before that boundary.

## 4. Canonical text format (agit's hashing and storage shape, the final spec)

There is no native jsonl, so the following is an agit-defined spec ([inferred] design + [observed]
verification; the verification script and its results are in
`samples/opencode/mutation-evidence.txt`):

```jsonl
{"id":"ses_…","kind":"opencode.meta","project_id":"…","parent_id":null,"directory":"…","time_created":…,"version":"…"}
{"id":"msg_…","kind":"message","session_id":"ses_…","time_created":…,"data":{…verbatim…}}
{"id":"prt_…","kind":"part","message_id":"msg_…","session_id":"ses_…","time_created":…,"data":{…verbatim…}}
```

Rules:

1. **The leading meta line carries immutable columns only**: `id, project_id, parent_id, directory,
   time_created, version`. It **leaves out** `title` (generated asynchronously, so it changes) and
   `cost/tokens/summary/share_url/revert/time_updated/time_compacting` (all of which change).
2. Every line after it is one message or one part, ordered per §2.3. `data` embeds the TEXT read out
   of sqlite **verbatim** (no re-serialization) — once a row is at rest, rereading it yields
   identical bytes ([observed] cmp passes across two reads).
3. **Exclude the `time_updated` column** (a row-level column, not inside the data JSON; §3
   conclusion 3 says why it moves).
4. Serialization: compact JSON (no spaces), key order fixed as above, `ensure_ascii=False`.
5. Append-friendly: the bytes before the boundary are stable (§3); a file part's base64 is inlined
   in data, large but stable — keep it (8 MB scale is still acceptable; gzip at the storage layer if
   that is genuinely too large, which is independent of the format).

Verification: a session at rest, 39 lines, two copies 23 minutes apart, is **byte-identical**; an
active session has a stable prefix of 12 lines out of 18, and the first divergence is exactly the
line of the assistant message in flight — precisely what the §3 rule says. [observed]

## 5. Listing, lookup, and the read convention

`sessions_for(repo)` ([observed] a full scan of 139 rows finishes in 2-5 ms against the 379 MB copy,
`time sqlite3 …` measured at 0.003 s; at this scale an index is not even needed):

```sql
-- exact match on the launch directory
SELECT id, title, parent_id, agent, time_created, time_updated
FROM session WHERE directory = ? ORDER BY time_created;
-- or by project (equivalent when directory == the git worktree; defends against a launch
-- from a subdirectory, of which this database has no instance)
SELECT s.id, … FROM session s JOIN project p ON p.id = s.project_id
WHERE p.worktree = ? ORDER BY s.time_created;
```

[observed] semantics: `session.directory` = the launch cwd (139 of the 139 rows here either equal
the worktree or fall into the global project); `project.worktree` = the git root, and every
non-repository directory is filed under the special project `project.id='global'` (worktree=`/`).
26 of the 139 sessions here belong to the global project — **repo attribution for those sessions
can only come from `directory`**, because the join yields `/`. `all_sessions()` is the same full
scan (139 rows, milliseconds).

**Subsessions**: a non-empty `parent_id` marks a subagent session started by the task tool (73
children / 66 roots here). `opencode session list` **lists root sessions only** ([observed]); agit's
`sessions_for` returns the full set and keeps `parent_id` (no visibility filtering).

The read convention (following the precedent of `adapter/codex_index.rs`, [documented] in that
file's comments): `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI` (`file:…?mode=ro`, which already implies
query_only semantics; `PRAGMA query_only` can be added as a second belt). **Never `immutable=1`** —
under WAL, immutable reads torn or stale pages. A read-only connection does not disturb a running
opencode ([observed] this session stayed live throughout, with no lock conflict during the probe).

## 6. Inventory of injected messages (user-role, non-human)

The test is the field, not the prefix: the text part carries **`"synthetic":true`** ([observed] 105
of them; the 1426 assistant-side texts never carry it). Five shapes
(`samples/opencode/synthetic-user-text.txt`):

| Count | Prefix | Source |
|---|---|---|
| 86 | `Called the Read tool with the following input: {"filePath":…}` | a replay channel that rewrites a tool call as plain text [inferred, medium confidence] |
| 11 | `<system-reminder>Note: The user opened/selected …` | IDE / editor context reminder |
| 4 | `Continue if you have next steps, or stop and ask…` | continuation after a compaction or an interruption (accompanied by `metadata.compaction_continue`) |
| a few | `[SYSTEM DIRECTIVE: OH-MY-OPENCODE - TODO CONTINUATION] …` | todo continuation from the omo plugin |
| 1 | `The previous request exceeded the provider's size limit…` | notice of a retry after media went over the size limit and was compressed |

Mapping rules (the same philosophy as the claude_code/codex adapters — a miss only makes the count
too high, a false positive erases real human input):

- `synthetic == true` → **produces no `UserPrompt`**; the injected prompt counts as
  `EventKind::Other` (the line number is kept so it can be looked up), and `counts().prompts`
  excludes it.
- text with role=user and `synthetic` absent → `UserPrompt` (391 of them here) [observed].
- The backstop: a future injection form carrying no flag → lands in `UserPrompt` (count too high),
  which errs in the safe direction.

## 7. The compaction mechanism: a lossy summary, in three parts

[observed] (`samples/opencode/compaction-anatomy.txt`, dissecting ses_038237e2…) one compaction
produces three new rows:

1. **The boundary marker** — a user message whose only part is `{"type":"compaction","auto":true,
   "overflow":false,"tail_start_id":"msg_…"}`. `tail_start_id` points at the first (assistant)
   message of the new context window.
2. **The summary body** — an assistant message with `"mode":"compaction","agent":"compaction",
   "summary":true`, whose text part is the lossy summary the model wrote (`## Objective\n- …`).
3. **The continuation injection** — a user text, `synthetic`, with `metadata.compaction_continue`
   set (the Continue row in §6).

`session.time_compacting` is NULL on all 139 sessions ([observed]) — a transient flag set only while
a compaction is in flight, so it **cannot** be the test for one. The test: the message holds a
compaction part (the boundary), or the message has `mode=="compaction"` (the summary). The user's
original input is kept verbatim in the rows before the compaction (nothing on disk is deleted), but
what is fed to the model is the summary — so in IR semantics this **maps to
`EventKind::CompactSummary`** (lossy, `is_lossy_summary()=true`, not an input to merge). The summary
body can serve as that event's `text` (for display), while the boundary parameters
(`auto/overflow/tail_start_id`) stay out of the IR and are read back from the raw line via
`Event.line`. `gist()` must skip both, as the rule in the other two runtimes does.

## 8. export / import and the capability verdict

### 8.1 export ([observed] + [documented] `cli/cmd/export.ts`)

- `opencode export <id>` → one JSON on stdout: `{"info":{id,slug,projectID,directory,path,
  parentID,title,agent,model,version,summary,cost,tokens,permission,time{created,updated}},
  "messages":[{"info":{…message fields…},"parts":[{…part fields…}]}]}`, indented by 2 spaces.
- **Two exports of a session at rest are byte-identical** ([observed] cmp passes, 185575 bytes).
- With no id it picks interactively; `--sanitize` replaces body text / paths / URLs with
  `[redacted:<kind>:<id>]` (sample `export-sanitized-head.json`).
- A part **carries no `time_created`** — export is lossless in semantics but not in bytes (below).

### 8.2 import ([documented] `cli/cmd/import.ts` + [observed] the full experiment set, `export-import-behavior.txt`)

The statements that write to the database: the session row uses `INSERT … ON CONFLICT DO UPDATE
{project_id, directory, path}`; message/part rows use `INSERT … ON CONFLICT DO NOTHING`. What the
experiments show:

- **The original session id is reused, never reminted**; stdout prints `Imported session: ses_…`,
  exit 0.
- **directory/path/project are rewritten into the cwd context of the import** — that is a feature:
  run import from the target repo root during install and ownership comes out right on its own.
- **Re-importing under the same id does not overwrite content**: a changed title is discarded, a
  changed part body is not written (both measured). → Idempotent, and it also **satisfies "agit
  never modifies a user's existing session" for free** — but only if agit mints new ids itself:
  import under the original id still UPDATEs the session row's directory (moving it out from under
  the user), so the id has to change.
- **A full id remint works**: replacing session/message/part ids with random strings (rewriting the
  `parentID` and `tail_start_id` references to match) imports successfully and shows up in `session
  list`. [observed] Two constraints: every id must be a **string** (`parentID:null` gives
  `Expected string, got null` — a message with no parent must have the key **deleted** rather than
  set to null); and a non-empty internal reference on a message/part must point at an id that
  exists.
- **A partial export imports**: a truncated export of 2 of the 8 messages lands complete in a fresh
  database ([observed]; within the same scratch database it is silently skipped on an id collision —
  that is idempotency, not failure).
- Fidelity: after an export and re-import, part/message `data` is **100% equal in JSON semantic
  value** (30/30, 8/8, [observed]), and the byte differences are only reordered keys; `time_created`
  on message/part always takes the moment of import, while the session's `time.created` keeps its
  original value.

### 8.3 resume semantics and the capability verdict

[documented] (`--help` / `run --help`): `-s/--session <id>` resumes the named session, `-c` resumes
the most recent one, and `--fork` pairs with either to resume into a new copy. `opencode run
--session <id>` works the same way headless. **Resuming continues writing in place** (into the same
session row; the `session.updated` event stream corroborates this) → **install must import a copy
under a new id**, never target the user's original session.

**Capability verdict: `Resumable`.** The grounds (against the definition in `desktop-apps.md` §4.2):
it installs to disk (import writes to the database, and the byte path is officially maintained); it
yields a resume command that is certain to run (`opencode --session <id>`, the same CLI already on
this machine); and its last step goes through no private index agit cannot observe (where it lands
is the source-of-truth database itself, unlike Claude Desktop, where a sidecar has to be forged and
the application has to pick it up). It is not ExportOnly (there is no "hand off to a process agit
cannot observe" hop), and still less ImportOnly.

## 9. Adapter method design (master table)

| Method | Design |
|---|---|
| `sessions_for(repo)` | the two SQL statements of §5: an exact `directory = repo` match + `project.worktree = repo` as the backstop; includes subsessions (keeps `parent_id`); reads the `session` table only and never touches part content. [observed] milliseconds |
| `all_sessions()` | `SELECT id,title,parent_id,directory,agent,time_created,time_updated FROM session ORDER BY time_created` |
| `resolve(id)` | direct PK lookup; returns a `(db_path, id)` tuple — the "backing store" is not a file but a set of database rows, and §5 gives the opening convention |
| raw bytes | the §4 canonical jsonl (meta line + message/part lines, `time_updated` excluded), **materialized from the database on demand**, byte-stable (verified in §4) |
| `parse` | read the canonical stream → IR; §9.1 has the mapping table |
| `render` | IR → the export JSON shape (§8.1), key order following export's own `info`/`message.info`/part order |
| `mint_id()` | `ses_` + a 24-character random string already passes import validation [observed]; still prefer an opencode-style time-ordered id (`ses_<time prefix><random>`) to keep list ordering readable [inferred, medium confidence]; `msg_`/`prt_` on message/part likewise |
| `install` | (1) IR→export JSON, with every id reminted, internal references rewritten to match, and the key deleted where there is no parent; (2) `cd <target repo> && opencode import <file>`; (3) resume_cmd=`(cd <repo> && opencode --session <new_id>)`. [observed] verified end to end |
| `capability()` | `Capability::Resumable` |
| native resume command | `opencode --session <id>` (equivalent to `opencode -s <id>`; `opencode run --session <id>` for headless) |

### 9.1 part/message → EventKind mapping table

| Source shape (the test) | EventKind | Notes |
|---|---|---|
| text part with role=user, `synthetic` absent | `UserPrompt` | 391 of them [observed] |
| text part with role=user, `synthetic==true` | `Other` (not counted in prompts) | §6; the text goes into `Event.text` verbatim, for diagnostics |
| user message holding a compaction part | `CompactSummary` (the boundary) | merging it with the next row into one event is cleaner [inferred]; `auto/overflow/tail_start_id` are read back via line |
| text part of an assistant message with `mode=="compaction"` | folded into the text of the preceding `CompactSummary` | the summary body is for display; not an input to merge |
| assistant text part (the synthetic flag does not exist on this side at all) | `AssistantReply` | `time{start,end}` can supply `timestamp` |
| tool part | `ToolUse`; tool ∈ {`edit`,`write`} is promoted to `FileEdit` | the same allowlist as the cursor adapter; `state.input.filePath` goes into `paths`; `state.output` stays out of the IR and is read back via line (8 MB-scale output must be kept outside the IR) |
| reasoning part | `Other` | plaintext, but not rendered (matching how the other two treat thinking); the count is kept |
| patch part | `Other` (`files[]` goes into `paths`) | hash points at the snapshot/ sidecar; avoids double-counting FileEdit against the edit/write events |
| file part | attached to the `paths` of its owning `UserPrompt`/`ToolUse` (recording `filename`) | the data: base64 body stays out of the IR [inferred, high confidence: otherwise one 8 MB attachment ruins the transcript page] |
| step-start / step-finish | no event (known structural parts, not counted as dropped) | `tokens/cost` on `step-finish` stay in raw; turns are bounded by user prompts |
| unknown part type (`subtask`/`snapshot`/`agent`/future types) | `Other`, counted in `dropped` | matches the codex adapter |

`TurnEnd` as well: **not produced** (opencode has no explicit turn-termination record;
`finish:"stop"` is per-message only, and a tool loop puts several assistant messages in one turn —
the same handling as Claude Code). [inferred, medium confidence]

## 10. What could not be determined

| Question | Impact | How to decide it |
|---|---|---|
| whether `revert` (/undo) deletes or hides rows already written | the worst case for the §3 continuity boundary | revert is NULL throughout this database; run `/undo` in a real session, then diff two copies |
| whether the `snapshot` on `step-start`/`step-finish` and the `snapshot/<project>/` sidecar become dead links after install | whether render carries the snapshot field of patch/step | import a session holding a patch part, run `--session`, trigger one more edit, and see whether revert errors |
| whether `opencode --session` after import really carries the full tool history into inference, end to end | the last mile of `Resumable` (the storage layer is proven, the inference layer has not been run) | send one `run --session` at the imported copy from a scratch HOME with credentials configured |
| whether an injection form carrying no `synthetic` flag exists | the completeness of the §6 rule | hand-sift all 391 "non-synthetic user text" rows in the database (only a screenful of the sample was sifted) |
| whether listing switches data source once the `session_message` table is enabled | the shelf life of the §5 SQL | follow anomalyco/opencode's migration record (`migration` already holds `session_message_cursor`) |
| what happens to a user message's `tools`/`system`/`variant` across a cross-version export/import round trip | render fidelity | export the same session from both 1.18.11 and 1.18.13 and three-way diff them |
| the real payload of the `subtask`/`agent`/`snapshot` parts | three rows of the §9.1 table rest on [documented] alone | build a session that uses the subagent/queue commands and sample it |
| import.ts is cited from the dev branch source | the documented claim may differ slightly from the 1.18.13 binary | every load-bearing behavior (id reuse, directory rewrite, idempotency, the string schema) was re-checked against the binary on this machine — residual risk is low |

---

**Reproduction material**: the probe scripts and the database-copying procedure are in
`/tmp/opencode/opencode-probe/` (volatile); the conclusive evidence is frozen into the eight files
under `samples/opencode/`. Write-side experiments ran only under
`HOME=/tmp/opencode/opencode-probe/home{,2,3} XDG_DATA_HOME=…/xdg{,2,3}`, and the real database
`~/.local/share/opencode/opencode.db` was read exactly once, by `cp`.
