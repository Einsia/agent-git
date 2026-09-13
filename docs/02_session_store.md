# Sessions, snapshots, versions

The mechanism told along one complete use: install the CLI → sign in → adopt a
session → record a version → publish → someone else picks it up.

## 1. `agit login`

The only prerequisite step, and a hard precondition for recording a
version — `agit import` and `agit commit` both count.

```bash
agit login
```

It puts a token pair in `~/.agit/credentials/<authority-key>.json` (0600): access lasts an hour,
refresh lasts thirty days. Once access expires the client swaps in a new one and
retries, so one sign-in lasts a month.

Recording a version requires signing in first, because a commit records **who
recorded it** (git's `user.name` / `user.email`, taken from the sign-in
credentials), and the agent repo path is `~/.agit/repos/<owner>/<name>/` — both
need the account name, and neither can be filled in afterwards.

`agit import` records the first version by default, so it needs a sign-in too.
To mark a session down where there is no network, use
`agit import <session> --link-only`: that path writes only the link, stays
usable offline. After sign-in, `agit import <session> --into <owner/repo>@<branch>`
records the opening version with explicit ownership. `log` / `show` /
`status` / `doctor` stay offline as always.

There is no other installation step. agit writes nothing into
`~/.claude/settings.json` or `~/.codex/config.toml`, installs no hooks and runs
nothing in the background — it works only while you are typing a command.

The cost is that **adoption is explicit**: a finished session does not enter
version control by itself; `agit import` has to name it.

## 2. Disk layout

The default storage root is `~/.agit`; `AGIT_HOME` can select another root.
Credential filenames are derived from the normalized Hub authority. They are
opaque keys, and each credential record is bound to that Hub. Use `agit login`
and `agit logout` to manage them rather than copying a token file between Hubs.

```text
~/.agit/
  credentials/<authority-key>.json     token pair and account identity, per Hub (0600)
  store/<runtime>/<session-id>.json    the local session link
  repos/<owner>/<name>/               the Agent repository
```

Two levels, not three. A middle level `drafts/<agent>/` — `agit commit`
materializing the content there, `agit push` copying it into the repo and then
**deleting the draft** — produces two bugs: the next commit loses its comparison
baseline, and the hint "rerun agit push if the tag did not get pushed" cannot be
carried out, because what has to be re-pushed is already deleted. `commit`
writes straight into the agent repo and makes one git commit, `push` is
`git push`, and both bugs are structurally impossible.

### store: links only

One session corresponds to one concrete rollout entry in the runtime:

| Runtime | Transcript file | Reverse lookup |
|---|---|---|
| Codex | `~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<uuid>.jsonl` | the `threads` table of `~/.codex/state_<N>.sqlite`, 0.40 ms |
| Claude Code | `~/.claude/projects/<cwd-slug>/<uuid>.jsonl` | one glob level, `projects/*/<id>.jsonl`, 2.2 ms |

`agit import <session-id> --from claude-code --into nana/opd@topic` adopts the
native session and records its initial version. Its local link lives at:

```text
~/.agit/store/claude-code/db57fdab-….json
```

```json
{
  "cwd": "/Users/nana/Projects/OpenPad",
  "agent": "opd",
  "owner": "nana",
  "branch": "topic"
}
```

This example shows the local route. The runtime and session id come from the
path and are not repeated in the JSON body; absent optional fields are omitted.

* `cwd` records the native session's project directory.
* `owner`, `agent` and `branch` identify its local repository claim. A session
  adopted with `--link-only` can remain unclaimed. Ordinary commands select an
  existing session through explicit arguments or `AGIT_SESSION`; a saved link
  does not authorize guessing a command target.
* Materialization and settlement can also persist `baseline_bytes`,
  `baseline_hash` and `materialized_from` to distinguish saved context from
  later native work. Supersession, merge-archive roles and naming state have
  separate optional fields; superseded or archive links are not active claims.

Import adopts an existing runtime session; resume/run and a resumed fork can
record a newly materialized instance, and commit updates its settlement state.
Clone only fetches repository history and optionally binds the workspace. It
does not create a runtime session link. These instance records remain local;
the repository's Git history carries the shared lineage.

The store keeps no copy of the transcript. Transcript files do not disappear
(observed over the 18779 `rollout_path` rows of the `threads` table, with the
most recent 400 and the oldest 200 sampled and none missing), while copies would
cost 11.2 GB (18858 sessions on this machine).

One file per session rather than a single `links.json`: writes can fire
concurrently (two sessions open at once), and one file per session removes the
read-modify-write race.

### An operation that scales with the session count must not open transcripts

Reading session state = opening the transcript + parsing the whole JSON. Parsing
one 3 MB session takes 6.8 ms and listing 18810 sessions takes 390 ms — **the
cost is in the multiplication by the session count, not in one parse**. The rule
is drawn there:

| Command | Reads transcripts | Count depends on |
|---|---|---|
| `agit log` / `status` | no | the number of adopted sessions |
| `agit import` finding candidates | no | the number of sessions under this directory |
| `agit commit` | **yes, once** | one |
| `agit import -n <agent>` recording the first version | **yes, once** | one (the same code as above) |
| `agit log <session>` | yes | the one the user named |
| `agit doctor` | yes | every agent repo on this machine + the adopted live transcripts |

The rule is not "session state is read in full only at `agit commit`, so
`import` never touches a transcript". The first half holds — recording one
version is one read; the second half attaches the reason to the wrong object:
what `import` parses after the selection is **that one** session, the same cost
as `commit` parsing it, and it does not change with 18858 sessions on disk or
with 3. So that cost is affordable and worth paying — the user wants "this
session is in version control", not "half of it is".

The one exception is `agit import`'s interactive list: Claude Code has no index
of the opening prompt, and showing it means reading the file. The scope is
pinned to "unadopted candidates under the current directory", and with a single
candidate there is nothing to ask and so nothing to read.

## 3. Snapshot = version

**Recording a version is an explicit action.** It covers everything in the
transcript at that moment. The first version is recorded by `agit import` along
the way, every later one by `agit commit`:

```bash
agit import db57fdab -n opd     # adopt + first version
agit commit opd                 # after more work, record another
```

```
✓ adopted claude-code db57fdab-2f3
  cwd      ~/Projects/OpenPad
  link     ~/.agit/store/claude-code/db57fdab-….json

✓ recorded db57fdab-2f3 → nana/pad
  version  agit-21fc4fdc111ed596a78771f54f45f7a6004d9d5d
  session  agit-a1b2c3d4… (this branch's first snapshot claimed it)
  turns    3
  code     git@github.com:nana/OpenPad.git@1839e61
  repo     ~/.agit/repos/nana/pad
  created the local repo for nana/pad; `agit push` creates it on the hub

  `agit push opd` publishes
```

Why the name has to be given: an automatically chosen agent name silently
decides which lineage this memory lands on, and that kind of mistake is not
noticed right away. A missing name is therefore an error, not "pick one at
random" — the error message carries a suggestion computed from the working
directory name, but typing it stays the user's own step.

### id

```text
snapshot id = "agit-" + <the git commit SHA-1 of this snapshot>
```

A git commit hash is already a content address: it covers the whole
parent → tree → blobs tree, any changed byte yields another id, and the id is
the git tag, which cannot be edited. No second hash layer is
needed — `SHA256(parent ‖ cwd ‖ transcript bytes)` only repeats what git already
does.

The `agit-` prefix has two uses: telling one from a bare git SHA at a glance;
and in `agit clone x/y@Z` a `Z` that starts with it is a version, otherwise a
branch name (so a branch name must not start with `agit-`; an agent name is not
in that position and is not restricted). A hyphen, not an underscore — `agit_`
+ 64 hex hits the backend secret rule `\bagit_[0-9a-f]{64}\b`.

The id exists only after the commit, so the commit message does not contain it;
the tag is applied after the commit, so its name is right by construction.

### Why a version is not taken from the last closed turn

Under the rule "a version = the hash of the last **closed** turn", a turn closes
only when the person speaks again. The last turn — often exactly the most
valuable stretch of work — then never reaches a version. Codex barely holds it
up with `event_msg/task_complete`; **Claude Code has no equivalent signal at
all** (its `system/turn_duration` does not line up with the human turn count:
22 turns against 48 records, 33 turns against 27, and it fires for interrupts
and system turns too).

The root cause is binding the version to the session structure. Bound to the
author instead, the open-turn / closed-turn distinction and everything built to
work around that asymmetry are gone together. `EventKind::TurnEnd` stays in the
IR (rendering skips it, and activity statistics must not count it as a reply),
but it decides nothing about a version.

### session/meta.json

Under `session/`, **one per git commit, overwritten in place** — git stores the
history, and accumulating would only produce two records that can disagree. The
only place that produces it is `meta::write`.

```json
{
  "line":    "session",
  "session": "agit-a1b2c3d4…",
  "runtime": "claude-code",
  "cwd":     "/Users/nana/Projects/OpenPad",
  "code":    "git@github.com:nana/OpenPad.git@1839e61",
  "kind":    "turn",
  "turn":    3
}
```

`line` is the **branch form**, fixed at birth and never converted: a `session`
line is bound to one session for life, a `file` line never claims a session (it
takes only shared-file commits). It is written down, not inferred from "does the
tree have session files" — the inference has a fatal overlap: a session line
just created, with its first turn not yet settled, equally "has commits and no
session files", and is rejected as a file line.

Every invariant is guarded by `meta::validate` (the persist path and the
tree-building path share it):

* A file line carrying `session` contradicts its own form; refused outright.
* A session line's `session` may be empty — that is the **birth state**: the
  identity takes `hash(cwd + transcript bytes)` to compute, and creating the
  branch precedes settling the first turn.
* Once the identity is claimed, `runtime` is mandatory: the envelope's `_source`
  and the VIEW slicing (§4) both resolve through it, and once it is empty
  nothing else can supply it.

`code` exists only when the cwd is a git repo, shaped `<origin>@<short-sha>`;
`cwd_state` is a bounded workspace summary collected at the same moment, for
example:

```json
{
  "origin": "git@github.com:nana/OpenPad.git",
  "head": "1839e61…",
  "branch": "main",
  "worktree": "dirty",
  "staged": 1,
  "unstaged": 2,
  "untracked": 1,
  "conflicted": 0,
  "status_digest": "…"
}
```

It contains no paths, no diffs and no file contents, so the size of the state
body does not grow with the number of files in the workspace; with no origin,
HEAD or branch the corresponding fields are empty, and `worktree` is `unknown`
when the status check cannot be run safely. `cwd_state` is an optional extension
field; a `code_state` written by an older version is read as the same field,
while new writes uniformly use `cwd_state`: an old commit without it is still
read under the original schema and no history migration is required. The backend
parses only the known fields the binding needs and ignores unknown ones, so
adding it changes neither the HTTP interface nor the push protocol.

When one settlement lands several turns at once, only the last turn carries the
`cwd_state` collected during that settlement; earlier turns cannot have the
state at the time they actually happened inferred back from the current
workspace, so they keep the default value rather than passing the current state
off as a historical fact.

`kind` is the kind of this commit (`turn` / `merge` / `view` / `file` / `archive`), and
`turn` is the turn ordinal. Empty fields are never serialized — making "no
identity" look like "the identity is the empty string" gives downstream one more
state to tell apart.

An `archive` snapshot retains additional native evidence in LOG without changing VIEW,
shared files, the logical session identity, or the completed-turn ordinal. It is a
session-line operation, never a file-line operation. Archive entries have no `#n`
turn label and remain selectable by their commit ID. Reader support alone does not
make a runtime produce these snapshots.

The `archive` kind extends a closed metadata enum. CLI releases whose readers do
not recognize it cannot read a history containing it and must be upgraded; adding
the reader does not rewrite any existing snapshot or change the storage layout.
The legacy `snapshot.json` format does not acquire this kind.

`session` is this branch's session identity (one session per branch, see
`docs/03_branch_model.md`): the root snapshot claims it with a content hash —

```text
session = "agit-" + hex(SHA-256(cwd ‖ 0x00 ‖ transcript bytes))[..40]
```

The root snapshot's own commit SHA cannot be computed before the commit (the
tree it covers contains `session/meta.json` itself), and the claim has to be
finished at the moment the file is written, so this value — stable over the same
content — stands in first. Later snapshots inherit from the tip and never
recompute; when the two disagree, HEAD's blob wins — the binding lives in the
history, and the workspace copy is only a mirror.

Fields deliberately **absent**:

| Dropped | Why |
|---|---|
| `parent` | a git commit records its parent itself; a snapshot does not repeat that layer |
| `signer` / `key` / `sig` | the signing mechanism is gone entirely — the id is the commit SHA, content integrity is git's guarantee; the author is recorded by git's `user.name` / `user.email` |
| `version` | that is the snapshot id (= the commit SHA), self-referential |
| `captured_at` / `signed_at` | an unverifiable self-report; for time, ask the git commit |
| the runtime's local session id | a local identifier for "this machine, this installation"; recording it only tempts someone to use it as a durable identity |

**The id is not written in either**: it is the SHA of the commit around it.
Validation = whether the tag name equals the SHA of the commit it points
at — this is how the server-side gate ④·layout judges it (see
`docs/03_branch_model.md` §4). The local half takes another shape: the commit
SHA is itself the content address, so doctor needs no machine tag to prove it
(§8, check 1).

## 4. Envelopes and the v1 LOG / VIEW sequences

Current snapshots declare `"layout": "v1"` in `session/meta.json`. The physical
paths are defined by `domain::meta`; `domain::storage` materializes their logical
envelope streams for readers:

```text
session/meta.json          session metadata and the storage layout declaration
LOG                        full ordered sequence of event ids, one id per line
VIEW                       ordered resume selection of event ids, one id per line
events/a/b/c/d/<event-id>   one canonical envelope and its trailing LF
```

An event id is a lowercase hex content address of the complete envelope,
including its source, session identity and trailing LF. Its first characters
select the shard directories; the complete id remains the filename. It is
distinct from `_object_hash`, which addresses only the envelope's `content`.
The managed `.gitattributes` rules preserve the bytes of `LOG`, `VIEW` and
`events/**`. Shared `AGENTS.md`, `memory/` and `skills/` remain separate content.

Historical **v0** snapshots instead store complete envelope JSONL in
`session/log.jsonl` and `session/VIEW`. A missing `layout` field means v0;
readers use the metadata declaration rather than guessing from filenames.
Those paths remain compatibility inputs for history and migration, not the
physical layout written by current snapshots.

`VIEW` has no extension because it is an ordered selection, not a second
transcript. The capture discussion below describes the logical envelope
streams after materialization, independently of their v0 or v1 storage.

### Event objects materialize into envelope lines

```json
{"_source":"claude-code","_session_id":"agit-…","_object_hash":"…","content":{…the original line…}}
```

Four keys, and **the field declaration order is the wire byte order** (serde
serializes in declaration order; the test `envelope_wire_shape_is_stable` pins
it, and moving one breaks the wire format):

* `_source`: the normalized runtime id (`codex` / `claude-code`).
* `_session_id`: this branch's session claim, taken from `session` in
  `session/meta.json` and carried into the envelope unchanged, **never
  recomputed from content** — the `sessionId` inside content belongs to that
  recording, which is a different thing from the branch identity.
* `_object_hash`: the content address of this line's `content`; the algorithm's
  only implementation site is `transcript::object_hash`:

  ```text
  _object_hash = hex(SHA256(serde_json::to_string(&content)))[..40]
  ```

  Its canonicity (independent of key insertion order, `{"n":1e0}` normalized to
  `1.0`) rests entirely on serde_json's default features: objects go through
  BTreeMap (keys sorted), numbers through f64. A comment in Cargo.toml pins the
  ban on adding `preserve_order` / `arbitrary_precision`, and the
  `object_hash_*` tripwire tests fail loudly on the day someone adds one.

**The skip rule**: empty lines and lines that fail to parse (a truncated tail,
for one) never enter the repo; once the tail is complete it is accounted for by
the next commit. The envelope sequence therefore aligns with the **parseable
lines** of the live text, not with physical line numbers — a bad line occupies a
physical line number, and such a number must never index the packed envelope
sequence (coordinate discipline: cut the window on the raw live lines first,
then pack the slice).

**Unwrapping** has two settings: `unwrap_strict` requires every line to be a
valid envelope (its error message carries a 1-based line number);
`unwrap_lossy` skips bad lines and counts them, for reading a repo that "an
older tool may have written badly". The lines that come back are **canonically
serialized** — key order is rearranged by BTreeMap, the semantics are equal to
the letter, and the bytes do not follow the source file; downstream comparison
always goes through Value/hash, never raw bytes.

### VIEW = the resume selection

The full saved event sequence is `LOG`; the selected resume sequence is `VIEW`.
Repository-backed `agit show` reads `VIEW` by default and `LOG` with
`--log-only`. Resume materializes `VIEW` into envelopes and unwraps the native
records before installation. For ordinary capture, the selection runs from the
last compact boundary (inclusive) to the end; without compaction it contains
the whole captured transcript.

Both forms of compact boundary count (`EventKind::is_compact`): in Claude Code a
user line carrying `"isCompactSummary":true`, in Codex a
`{"type":"compacted",…}` line. The slice is cut on **raw live lines** by
physical line (`view_of_live`), and only then packed into envelopes by
`wrap_lines`.

The logical VIEW is rebuilt from live content when a turn is captured; v1
persists the resulting event-id sequence rather than a second envelope file.
Session enumeration does not count `VIEW` as another session: `session::list_in`
recognizes root `LOG` or the legacy v0 log together with claimed metadata.

**Current v1 self-consistency:** every event id in `VIEW` must resolve to its
canonical object and occur in `LOG`, including synthetic merge/cherry-pick
events. Paired operation markers must also be valid. Historical v0 handling
has compatibility rules for synthetic VIEW records; that exception must not be
used to admit an orphan event into a v1 sequence.

### The write path: commit / import -n

Both commands run the same code and produce byte-identical snapshots. Recording
one version:

1. **The continuity check**, against the committed HEAD blob and not the
   workspace file: the committed envelope hash sequence must be a **prefix** of
   the hash sequence of live's parseable lines (`transcript::continuity`). Three
   verdicts —
   * `Noop`: nothing grew, a no-op (printing "nothing new since agit-…").
     A last line written halfway (non-empty but not parseable as JSON) is not
     accounted for under the skip rule, and the output says it was skipped.
   * `Append`: pure append; carry on.
   * `Diverged`: something in the middle changed — this branch is already taken
     by another session, and it is refused before anything touches the disk:

     ```
     the current branch of alice/photo is already taken by another session.
       this branch's session is agit-…, and 019fb2… does not continue it
       → record it elsewhere: agit commit 019fb2… -n <another agent name>
     ```

     Native installation is a separate resume/run step (§7); clone itself
     does not create a native session. The storage layout does not replace the
     continuation checks for an installed instance.
2. Pass the logical LOG and VIEW envelope streams to `storage::write_snapshot`.
   It writes canonical event objects under `events/`, reuses identical existing
   objects, and writes the root `LOG` / `VIEW` event-id sequences. A conflicting
   body under an existing event id is refused.
3. Overwrite session/meta.json: `session` is inherited from the tip, and only a
   branch's first snapshot claims it by content.
4. git commit (message shaped `agit: 3 turns`; the author comes from the account
   name and email in the sign-in credentials), tag `agit-<40hex>` on the commit
   SHA, and the ownership written back to the store link — the next
   `agit commit <agent>` finds it through that.

### The read path: show / clone / resume

| Command | Which content it reads | How |
|---|---|---|
| Repository-backed `agit show` | selected `VIEW` | Materializes the selected point's sequence into envelopes for parsing/rendering; `--raw` strictly unwraps native records |
| Repository-backed `agit show --log-only` | full `LOG` | Uses the same layout-aware materializer for the complete saved sequence |
| `agit clone` | Git history and repository metadata | Fetches, creates the local checkout/branches and optionally binds the workspace; it does not install or launch a runtime |
| Repository-backed `agit resume` | selected branch head's `VIEW` | The slow path materializes and unwraps the VIEW, restores required native bootstrap metadata when available, then installs it; an eligible existing native instance can use the fast path |
| `agit show` selecting a native transcript or store link | the runtime's live transcript | Reads native content directly, with no repository envelope unwrapping |

A missing or unreadable required `VIEW` or event object is an incomplete
checkout. Materialization refuses instead of substituting the full `LOG`.
Recover the saved content with clone/fetch before retrying the selected session.
The selected branch/ref determines the saved VIEW, not an unrelated checked-out
branch.

### Historical layouts

| Generation | Physical shape | Current distinction |
|---|---|---|
| v0 | `session/meta.json`, `session/log.jsonl`, `session/VIEW` | Layout-aware readers retain historical compatibility; migration writes v1 without changing old commits |
| Earlier multi-session design | `sessions/<runtime>/<id>.jsonl` | Historical design, not the current v0/v1 session layout |
| Earlier root triplet | `snapshot.json`, `transcript.jsonl`, `view.jsonl` | Historical design, not the current root `LOG` / `VIEW` event-id format |

The earlier layouts must not be confused with v0 merely because their files
contain transcripts. Current v0/v1 interpretation is selected by
`session/meta.json`; valid v0 history remains readable and has a dedicated
storage migration path.

## 5. Turns

**A turn = from the person speaking to just before the person speaks again**,
including every tool round trip in between.

A turn boundary lands only at a real user prompt. Both runtimes stuff a great
deal into user messages, so those are filtered first:

| Runtime | Injected form | Observed |
|---|---|---|
| Claude Code | `<task-notification>` / `<command-name>` / `<local-command-stdout>` / `<local-command-caveat>` / `[Request interrupted by user]` / `A session-scoped Stop hook is now active` | of 549 plain-text user messages only 241 were typed by a person |
| Codex | `<codex_internal_context>` / `<environment_context>` / `# Files mentioned by the user:` | in one 786 MB session, of 219 `role=user` records only 16 were typed by a person |

The test is a **known-prefix match**, not "anything starting with `<`" — a user
may paste an HTML/XML fragment themselves.

Without the filter, one real 3 MB session cuts into 10 turns, 4 of them
notifications and interrupt markers. With it, 3 turns, all of them things a
person actually said.

### A turn hash is only for looking and comparing

```text
turn hash = hex(SHA256(previous turn hash ‖ 0x00 ‖ normalized content of this turn))[..40]
```

It carries **no** timestamp, no machine identity, no account name, and no
`agit-` prefix (a prefix would suggest it could be handed to
`agit clone x/y:<hash>`). It is truncated to 12 digits for display.

They are **never persisted**: the transcript is append-only, so reading the file
once at any moment recomputes the whole chain, identical to the letter with what
was computed turn by turn (observed as a session grew from 73 turns to 74: not
one of the first 73 hashes changed). `agit log <session>` computes on the spot,
so what it shows is the true state at this moment, including the turn in
progress.

Carrying no identity and no timestamp is the **point** of the design, not a
simplification. The question a turn hash answers is **"from which turn did your
copy of this session and mine diverge"**, and that is answerable only when the
same turn semantically gets the same hash for different people. Stuffing the
local ed25519 fingerprint and the turn's end time into the hash means two people
never compute the same turn hash, and fork-point detection exists in the code
while never taking effect in reality.

The cost is that "saying the same sentence twice gets the same hash". That is
not a problem: where uniqueness is needed, the snapshot id is what is used.

Normalization takes only the part the IR models, leaving out uuid, parentUuid,
sessionId and file paths, which can differ when saved history is materialized
into another native instance. Clone itself preserves the saved history.
`EventKind::Other` (encrypted reasoning,
vendor-proprietary encodings) and `TurnEnd` do not take part, and how much was
dropped is recorded in `Turn::dropped`. The integrity of those excluded bytes is
carried by the envelopes in the repo (every line's full original text goes into
content, and the line-level hash is in §4), so keeping only the semantics here
is safe.

The modeled kind, optional tool name and text are length-framed before hashing, so
separator characters inside text cannot impersonate additional fields or events.
These hashes remain computed diagnostics rather than persisted identifiers.

```
agit log db57fdab

  49b46fec2d20  2026-07-28T03:54  what is qmt what is a low-latency trading desk
                  36 events · 19 not in the hash
  2a6b2c79a442  2026-07-28T04:28  keep helping with the big refactor…
                  60 events · 32 not in the hash
```

## 6. The agent repo

An Agent repo can hold multiple session branches alongside the shared `main` file
line. The following paths describe a session-branch checkout; `main` does not
require session LOG or VIEW files:

```text
~/.agit/repos/nana/pad/
  .git/
  session/
    meta.json                          metadata, including layout: v1
  LOG                                  full ordered event-id sequence (§4)
  VIEW                                 selected resume event-id sequence (§4)
  events/a/b/c/d/<event-id>              canonical envelope object
  .gitattributes                       byte-preservation rules for storage
  AGENTS.md / memory/ / skills/         shared content
```

```text
refs/heads/main              the default branch
refs/heads/<branch>          other branches
refs/tags/agit-<40hex>       one snapshot
```

The lineage is git's own commit chain, not something to be guessed at. Guessing
it means a "same root check": walk the chain for a common prefix and decide
whether this is a continuation, a fork from the middle (`--branch` required) or
a different root (refused unless `--force`). With the local fingerprint inside
the turn hash, that check answers "different root" for any two people, so it
never once succeeds. Committing again is necessarily a continuation — the new
commit's parent is the previous one, which cannot be misjudged as a fork — so
neither `--force` nor `--branch` exists. What guards the write is a
content-level test: the committed envelope sequence must be a prefix of live, or
the commit is refused before anything touches the disk (the
write path, §4). The branch model (one session per branch, and how the binding
is guarded) has its own document, `docs/03_branch_model.md`.

With unchanged content a commit is a no-op (as with `git commit`):

```
· db57fdab-2f3 has nothing new since agit-21fc4fdc.
  → go back into the session and keep working, then commit
```

Rerunning `agit import` on an already adopted session takes the same path: the
link stays unchanged and it lands on that same no-op. No extra root appears and
no duplicate snapshot with identical content — the test is the continuity of the
committed envelope blob with live, not byte equality of workspace files.

### On a detached HEAD, switching back depends on whether there is a choice

`agit clone x/y:<version>` checks out a tag, which detaches HEAD. A commit on a
detached HEAD lands on a commit no branch points at, and the branch that push
sends does not contain it — a silent loss.

But "detached" is not the same as "there is a choice": when what was fetched is
exactly the newest version, where it lands is `main` itself — exactly one branch
is there, nothing is left for a person to decide, and switching back changes not
one commit, so it switches straight away and prints a line saying so. What does
need the user to speak up is **no branch pointing here** (the intent to start a
new branch from some older version) or **several pointing here** — those two
cases are still refused, with the candidates listed (starting a new branch is
git's own job today: `git -C <repo> switch -c <branch>`).

## 7. Publishing and picking up

```bash
agit push opd
```

It is `git push`: confirm the remote exists (the first time creates the agent on
the hub), scan once for secrets, push the current branch and the tags. The local
repo is the authoritative copy and a failure clears nothing, so rerunning is
retrying.

When the remote has moved ahead the push is refused (not a fast-forward), and
nothing is reset automatically — that would lose local snapshots. The hint is to
pick it up with `agit clone` and carry on.

### git authentication

Git operations that need authentication (`clone` / `fetch` / `push`) use a
bearer token, injected through **environment variables** and visible only to
that one subprocess:

```text
GIT_CONFIG_COUNT=1
GIT_CONFIG_KEY_0=http.extraHeader
GIT_CONFIG_VALUE_0=Authorization: Bearer <access_token>
```

The other two routes are both unsafe: writing it into the remote URL
**persists** it into `.git/config` (while an access token is due for rotation an
hour later), and it also shows up in `git remote -v`, in push error messages and
in any log that gets pasted somewhere; `git -c http.extraHeader=…` puts the
token in argv, where any user on the machine sees it with `ps`.

Git's exit code cannot tell an authentication failure apart (they are all 128),
so the test is a keyword in stderr. stderr is forwarded to the user and kept at
the same time — plain inherit leaves nothing to judge on, while capturing all of
it makes push's progress bar disappear. On an authentication failure the token
is swapped once and the operation retried, exactly once.

### Secret scanning

At push, because that is the first time content leaves this machine. Not at
commit: a commit is a purely local action, a secret staying on this machine is
not a leak, and a commit that refuses to record a version because of a secret
makes people stop recording versions at all. The same division of labor as
git — commit freely, block at the push.

The allowlist is `~/.agit/.agit-allow-secrets` (not inside the agent repo: that
repo gets pushed).

The client-side gate can be bypassed, so the server scans too — but only at
**the moment content becomes readable by a third party**: a push to a public
agent, and a private-to-public flip. A push to a private agent is not stopped by
the server: that is the author's own storage, nothing is distributed, and one
false positive would wedge the author's own work with no way out (the
server-side gate does not honor inline annotations).

What this gate guards is "the hub is not a distribution channel for leaked
credentials", not "the hub keeps your secrets for you". The three taken together
do not weaken the invariant: **every byte is scanned once in full by this rule
set before it is readable by a third party** — history accumulated while private
is scanned in its entirety at the moment it goes public.

When it does scan, the server scans by **ref difference** and rolls back **every
ref**; scanning only HEAD, or rolling back only `refs/heads/main`, lets a tag
carrying a secret bypass the gate (a version id is a tag, and a tag can point at
a commit that is not on the default branch).

### Picking up

```bash
agit clone alice/photo                     # a read-only pickup, the newest main
agit clone alice/photo:exif                # a branch
agit clone alice/photo:agit-21fc4fdc…      # one snapshot
agit clone alice/photo --mine              # make a copy under your name
```

**Read-only by default**: nothing is created under your name and `origin`
points at alice's copy. Clone fetches the repository and optionally binds the
workspace; it stops before native installation or launch. Use an explicit
`agit run` / `agit resume` selection to continue afterward. Materialization and
session identity belong to that separate operation, not to fetching a copy.

The two remotes are configured the way git itself means them:

| How it came to be | `origin` | `upstream` |
|---|---|---|
| created by your own `agit import` | yours | none |
| `agit clone alice/photo` | **alice's** | none |
| `agit clone alice/photo --mine` | your copy | alice's |

`.git/config` persists both of these anyway, so there is no new state to invent.
The read-only tier deliberately sets no `upstream` (its `origin` is the source),
so "is there an `upstream`" is exactly equivalent to "is this one mine" — the
test `agit push` uses. `@{upstream}` (git's tracking branch) and a remote named
`upstream` are two different things; the shared name is git's own baggage.

A read-only checkout's `origin` is the source, so **running
`agit clone alice/photo` again picks up the original author's later work**
(fetch + fast-forward; when both sides have moved it says "local and remote have
diverged, local left unchanged" and merges nothing). The recorded source is what
makes that path exist at all: a copy diverges the moment it is cloned, and with
nothing locally recording where the source is, there is nowhere to pick up from.

`--mine` has the server `clone --bare` alice's bare repo into **your**
namespace. That step brings the complete git history, so every early snapshot
alice recorded holds unchanged in your repo, and your continuation lands **on
top of** her history instead of starting another root. `clone --bare` goes
through a local path and git hard-links the objects, so copying a very large
agent costs almost no extra space. A copy's visibility **follows the
source** — copied from a public one it is public, copied from a private one
(which only an authorized person can copy at all) it is private.

Running `--mine` on an existing read-only checkout is an **in-place promotion**:
make the copy, change `origin`, record `upstream`, move the directory from
`repos/alice/photo` to `repos/<you>/photo`. Nothing already committed locally
is lost. The directory has to move because `repos/<owner>/<name>` records
"which agent on the hub this is a checkout of", and after the promotion that
agent is yours.

When the source is already under your own name both steps are skipped: there is
nothing to copy, and that path is "carry on from another machine".

The local fetch prepares repository branches and reads `session/meta.json`.
By default clone binds the current workspace to the repository (unless
`--no-bind` is requested); that routing does not create or select a native
session. It prints next-step guidance and stops.

When a later resume needs installation, it materializes the selected branch's
root `VIEW` sequence through the event objects and unwraps the native records.
Historical v0 points use `session/VIEW` through the same layout-aware boundary.
It does not install full `LOG` history in place of a missing VIEW. Required
bootstrap records may be restored separately from LOG; this does not restore
compacted-away context wholesale.

```text
agit clone alice/photo
  fetch repository history and prepare local branches
  bind the current workspace unless --no-bind is set
  print next-step guidance; no runtime is launched
```

A cloned history is the complete git history — every snapshot the original
author recorded and every commit's author field hold unchanged in your checkout.

When the copy already exists under your name and is a copy of exactly this
source, repeating `clone --mine` is idempotent (a retry of the same copy).
Colliding with an **unrelated** agent of the same name returns 409, and no new
name is guessed — your next step is `agit push`, and what it pushes must be the
agent you think it is. Use `--name` for another name in that case (`--name`
means something only under `--mine`: a read-only pickup produces no copy, so
there is nothing to name).

When one name matches **two** local checkouts (your own `<you>/photo` and the
`alice/photo` left behind by a read-only pickup), `agit commit` / `agit push`
error out and list both instead of picking one: the two readings point at
different lineages, and picking the wrong one records a stretch of work into the
other chain without being noticed right away.

### `agit push` in a read-only checkout

`origin` points at someone else's copy, and you cannot push into it. `push` can
then mean only one thing: make it yours and publish. So it **asks once** and
does it, instead of raising a "go run another command first" error — that
command holds no decision for a person to make.

```
alice/photo belongs to alice — you can't push to it.
create bob/photo under your name and publish it? [Y/n]
```

Answering no does nothing, and every local version is still there. In a
non-interactive environment (CI, scripts) it **errors** and offers
`agit clone alice/photo --mine`: a namespace write nobody nodded at does not
belong in automation. The promotion comes **after** every local judgment (are
there snapshots, a detached HEAD, the secret scan) — a repo that cannot be
pushed must not leave an agent behind on the server.

## 8. Integrity

`agit doctor` runs three checks over every agent repo on this machine, plus one
aggregated warning about legacy layouts (repos left from before the upgrade are
gathered into one line instead of flooding the screen one by one):

```
=== session metadata integrity ===
  ✓ session metadata is self-consistent for 2 agents
  ✓ checked against 2 live transcripts: both continue the committed content, 1 of them has 12 new lines since its latest version — `agit commit` records the next one
  ✓ VIEW is self-consistent in 2 repos
```

1. **Metadata self-consistency**: `session/meta.json` reads out and this branch
   has commits. Requiring instead that HEAD carry an `agit-<sha>` tag named
   after the version id makes the check self-referential once the version id is
   the commit SHA itself — the commit SHA covers the whole
   parent → tree → log → VIEW tree, touched content necessarily changes the SHA,
   and no machine tag has to prove it. The slot that frees up goes to check 3.
2. **The transcript is still append-only**: the committed envelope hash sequence
   must be a prefix of the hash sequence of live's parseable lines. Both sides
   use the same ruler (`object_hash`), so "how many lines are new since the
   latest version" is the length difference between the two; a half-written line
   at the end of live is left out of the comparison by the skip rule and is
   never misjudged as a divergence. What is compared is the content hash of each
   line's parse result, not the raw bytes — changed content necessarily changes
   the hash, while touching only key order or whitespace is not a change.
3. **VIEW self-consistency**: the current v1 `VIEW` event ids must be
   reachable in `LOG`, and merge / cherry-pick markers must close in
   pairs (the self-consistency invariant, §4); a log present with no VIEW is an
   error too (the VIEW is a required file). doctor only reports — the next
   version (`agit commit`) rebuilds the VIEW whole.

Check 2 depends on the committed content staying: a staging area deleted after a
successful push leaves no local copy, so only turn hashes can be compared, and a
turn hash excludes exactly the thinking blocks and encrypted reasoning, whose
bytes can then be changed undetectably. Committed content stays in the agent
repo permanently, with every line's full original text inside the envelope's
`content`.

A reported problem looks like this:

```
  ! 1 self-consistent, 1 problem
    the repo of nana/smoke has no commits
  → what was committed is still in the agent repo, nothing is lost
```

```
  ! checked against 3 live transcripts: 2 diverge from the latest version
    nana/smoke (claude-code db57fdab): the local session diverges from the latest version — handle this before resume / clone
  → what was committed is still in the agent repo, nothing is lost
  → to keep the new work after the divergence, record it into another lineage: agit commit <session id> -n <another agent name>
```

```
  ! found 2 legacy-layout repos (session files are not under session/): alice/one, bob/two
  → the layout is not migrated in place: clear those directories, then adopt again with `agit import` and record the first version
```

## Command table

```bash
agit login                                sign in (a hard precondition for recording a version)
agit import -n <agent>                    list this repo's sessions, adopt one and record the first version
agit import <session-id> -n <agent>       adopt the named one
agit import <session-id> --link-only      adopt without recording a version (works offline)
agit commit <agent>                       record another (continuity check, append envelopes, rebuild the VIEW)
agit commit <session>                     the same, named by session (when one agent has several sessions)
agit push <agent>                         publish (it is git push)
agit log                                  the adopted sessions
agit log <session>                        each of its turns (computed on the spot)
agit log <owner>/<agent>                  the snapshots of one agent
agit show <session>                       read a session (saved VIEW by default; --log-only selects LOG)
agit resume <session>                     continue a session (materialize saved VIEW when installation is needed)
agit clone <owner>/<agent>                fetch a read-only checkout; no runtime installation or launch
agit clone <owner>/<agent> --mine         make a copy under your name (origin yours, upstream the source)
agit doctor                               session metadata integrity, live transcript check, VIEW check, runtimes, backend
```
