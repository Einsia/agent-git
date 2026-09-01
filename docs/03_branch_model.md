# One branch, one session

## 1. The model

```
agent    = repo
branch   = session
commit   = a snapshot of that session
```

Once a session occupies a branch, that branch never changes hands. To change direction, open a
new branch; the old one stays.

Precisely: a branch has **two forms**, fixed the moment it is born and never convertible (the
`line` field of `session/meta.json`). A **session line** is bound to one session for life and
takes turn / merge / view commits; a **file line** never claims a session, takes only commits of
the shared files (memory, skills, AGENTS.md), cannot be resumed, and serves only as the starting
point of a fork / new. The `main` that `agit init` creates is a file line, and sessions each sit
on their own session line — so "how many sessions an agent has" counts session branches, not
whether HEAD carries a transcript (§6).

This is not an invented structure — it already exists in the data, only without a branch to name
it. Committing two sessions alternately inside one agent grows two snapshot chains out of a
common ancestor anyway. All this does is let a git branch carry that, instead of interleaving two
unrelated histories on one `main`.

## 2. Session identity = the value this branch's root snapshot claims

**The runtime session id cannot be used.** `install::mint_id()` mints a new UUID on every
install, so the same agent cloned onto two machines necessarily has two different runtime ids.
Bind on that, and a commit from machine B onto the same branch is judged a change of session. A
runtime id identifies "this install on this machine", not a durable identity — so it does not
enter `session/meta.json` either.

**Fast-forward alone is not enough** either. FF only requires the new commit to be a descendant
of the old tip, and a descendant commit can replace every file. The binding has to be at the
content layer.

A snapshot id is the SHA of the git commit (with an `agit-` prefix) — but the commit SHA of the
root snapshot cannot be computed before the commit (the tree it covers contains
`session/meta.json` itself), so the root snapshot claims the branch with a **content hash**:

```text
session = "agit-" + hex(SHA-256(cwd ‖ 0x00 ‖ transcript bytes))[..40]
```

Computable before the commit, stable across machines, and it introduces no new id space. It
carries no parent (that is the git commit's business) and no session of its own (which would be
circular).

- Later snapshots on the same branch inherit `session` from the tip and never recompute it; the
  tip is whatever HEAD's blob says — the binding lives in the history, and the copy in the
  working tree is only a mirror.
- Only a place that "has no session identity yet" is claimed: the root snapshot of a new agent;
  or a cloned repo where `session/meta.json` cannot be read (pushed up by plain git), where the
  next commit treats it as the start of a new chain, claims by content, and says so rather than
  doing it silently.
- A branch cut off an existing tip with git is **born carrying the tip's identity** (the client
  only continues it), and it is claimed under that identity the first time it appears on the
  server (§4). What the gate locks is "an existing branch must not switch to another identity",
  not "one identity may appear on only one branch".

Lineage itself is the business of the git commit chain and is not written into
`session/meta.json`.

## 3. Repository layout

With one branch per session, a directory level split by runtime has no meaning. The main
checkout (`~/.agit/repos/<owner>/<name>`) stays on the `main` file line; each session branch
gets its own linked worktree the first time it needs a checkout
(`~/.agit/worktrees/<owner>/<name>/<branch>`, which `agit repo path <owner>/<name>@<branch>`
prints and creates on demand). In any checkout, the session content of that branch is these
three files under `session/`:

```
session/meta.json   session metadata (branch form, session identity, runtime, cwd)
session/log.jsonl   the full history, one envelope per line, append-only
session/VIEW        the resume VIEW, a derived file rewritten whole on every commit
```

All three names are fixed constants (`FILE` / `LOG_FILE` / `VIEW_FILE` in
`agit::domain::meta`, and a same-named set on the server side in `gitsync::provenance`), so no
path is recorded in meta — one less place that can disagree with the facts. They sit under
`session/` rather than spread over the repo root because the root also carries the shared files
(memory / skills / AGENTS.md): mixed together, "which bytes belong to this session" has no
path-level answer, and the gate, doctor and the read paths all judge by that boundary.

The envelope format, the write path and the read path are in `docs/02_session_store.md` §4;
`git log <branch>` is this session's version history, and `git show <tag>:session/log.jsonl`
gets that version (in envelope form — the envelope is unwrapped back to raw lines before
rendering or installing).

`session/meta.json`:

```json
{
  "line":     "session",
  "session":  "agit-7f3a…",
  "runtime":  "claude-code",
  "cwd":      "/Users/nana/Projects/OpenPad",
  "code":     "git@github.com:nana/OpenPad.git@1839e61",
  "kind":     "turn",
  "turn":     3
}
```

`line` is the branch form (§1), written down explicitly instead of inferred from "does the tree
carry session files": a session line just created, with its first turn not yet settled, equally
"has commits and no session files", and the inference would take it for a file line and refuse
it. `session` is what the binding rests on (the content hash claimed in §2); a file line never
has it, and a session line does not have it either until the first turn is settled. Once an
identity is claimed, `runtime` is required — the envelope's `_source` and the VIEW slicing both
parse with it, and nothing else can supply it when it is missing.

There is no `parent`, and no `signer`/`key`/`sig` — the commit author comes from git's own
`user.name` / `user.email` (supplied by the sign-in credentials).

Snapshot id = the commit SHA (with an `agit-` prefix); no hash of its own is computed, because
git's commit hash already covers the whole parent → tree → blobs tree.

The agent name (`repo::valid_name`): non-empty, alphanumerics and `-` / `_` only — the name has
to serve as both a directory name (`~/.agit/repos/<owner>/<name>/`) and a URL path segment. It
**may** start with `agit-`: a repo name always sits in the `owner/<name>` position, never mixed
in with a ref, so there is no ambiguity to resolve; the hub allows it too, and derives one from
the directory name (binding `~/Code/agit-web` gives `alice/agit-web`).

The branch name (`repo::valid_branch_name`): non-empty, and must not start with `agit-` — that
prefix belongs to snapshot ids, otherwise `agit clone x/y@Z` has no way to tell whether Z is a
version or a branch. The character set is governed by git's own refname rules.

## 4. How the binding is guarded

Two layers — the client blocks first, the server is the backstop — and on both sides the test is
content, not the shape of a ref.

**Client: a continuity check before anything touches the disk.** `agit commit` requires the
committed envelope hash sequence to be a prefix of the live parsable-line hash sequence (see
`docs/02_session_store.md` §4). Anything but a prefix means another session wants to occupy this
branch — reject, history unchanged, not one byte moved:

```
the current branch of alice/photo is already occupied by another session.
  this branch's session is agit-…, and 019fb2… is not its continuation
  → record it elsewhere: agit commit 019fb2… -n <another agent name>
```

The copy `agit clone` installs into a runtime is minted with a new session id — content that is
nearly identical line by line is judged a divergence here as well. That is the cost of a
content-level test, and it is what the test is for.

**Server: one binding check inside the push gate.** A push passes **seven gates** (backend
`gitsync::routes::receive_pack`):

| # | Gate | What it tests |
|---|---|---|
| ① | Authentication | an anonymous push has no subject for the question "who pushed this" |
| ② | Authorization | whether there is write permission on this agent |
| ③ | ref movement rules | published history is immutable: non-fast-forward / deleting a published ref / repointing an `agit-*` tag |
| ④ | provenance | **two halves**: layout (`session/meta.json` is valid, the log is in the tree, tag name = commit sha) + VIEW self-consistency (every event the VIEW references is reachable, merge markers close in pairs) |
| ⑤ | session binding | the one this section describes |
| ⑥ | secret scanning | **public agents only**, see below |
| ⑦ | quota | only private agents have a hard cap |

Any gate that fails rolls back **every** ref (not only HEAD: a version ID is a tag, and a tag can
point at a commit that is not on the default branch).

The order is by **cost**: ① and ② decide before the request body arrives, and someone with no
write permission must not be able to make the server unpack a pack for them; ③ only compares
shas; ④ reads a few small blobs; ⑥ reads every newly reachable object in full. A push that is
certain to be rejected belongs at the cheapest gate that can stop it. The two halves of ④ are
split along the same line: the layout half sits right after ③, while the VIEW self-consistency
half, which reads the log and the VIEW of every session branch end to end, sits after ⑤.

**⑥ branches on visibility.** What it guards is "the hub is not a distribution channel for
leaked credentials", not "the hub keeps your secrets for you":

- push to a **private** agent → the server does not scan. The content goes into the author's own
  storage and nothing is distributed; and the server-side gate honors no inline annotation, so
  one false positive wedges the author's own work **with no way out**. The client-side scan
  before push still runs, and on the private path it is the only one.
- push to a **public** agent → scan. This push is itself the publication.
- private → **public** → scan the whole repo.
- a PR moving into a **public** target → scan.

All four routes pass through the same choke point, `gitsync::routes::expose()` — sharing one
choke point is what makes the next sentence an invariant rather than a coincidence: **every byte
is scanned in full by this rule set before it is readable by a third party.** History accumulated
while private is scanned end to end at the moment it turns public. The scan surface is
"everything outside the exclusion list" (`agit::domain::secrets::in_publish_surface`, plus
`session/meta.json`) — that is, everything except `.git/`.

The test at the session-binding gate (`check_session_binding`):

> The `session` in the pushed `session/meta.json` must equal the value on that branch's tip.

The four cases:

1. **The branch does not exist** → the first snapshot claims it. This is how a new branch is
   normally born.
2. **The tip carries no metadata** (an empty repo, a commit the hub bootstrapped itself), or
   **the tip is a legacy layout** (the new form does not parse, there is no session identity to
   guard) → allow. This is the grandfather clause: published history is not rewritten by the
   tooling, and the first new snapshot on a legacy branch claims it from then on.
3. **The tip has metadata but the server cannot read it** (over the read cap, read failure) →
   **reject**. Allowing this case too would turn case 2 into a two-step bypass: first inflate or
   corrupt the meta, then push under another identity. "The content itself says there is no
   identity" and "I cannot read the content" are two different things, and only the first
   deserves the grandfather clause.
4. **Both sides have an identity** → they must be equal; unequal is a rejection.

Tags fall outside this gate: a tag is the name of a single snapshot, with no "previous tip" to
compare against; that an `agit-*` tag cannot be repointed is the job of the "published history is
immutable" gate. **Deleting the branch and pushing again is not a way out either** — the ref
movement rules stop the deletion of a published ref, so the "branch does not exist → claim" case
is never reached.

**There is no exit for rebinding.** "Published history is immutable" is one of the red lines, and
rebinding a branch to another session is exactly the silent substitution that line exists to
prevent — someone cites a version of `alice/photo:exif`, that branch becomes a different
conversation, and the one citing it cannot tell a substitution happened. To change direction,
open a new branch.

## 5. What branch operations look like today

There is no `agit branch` / `agit switch` yet — the "wait for `agit branch`" line in the
continuity check's rejection text says exactly that. Today's branch operations are git's own:

```bash
git -C ~/.agit/agents/alice/photo switch -c exif   # fork one off the current snapshot
git -C ~/.agit/agents/alice/photo switch main      # switch back
agit clone alice/photo:exif                        # pick up a remote branch (checkout origin/exif)
```

Every session branch has its own worktree: settlement, merge and
`agit repo path <repo>@<branch>` all address that branch's own checkout, the main checkout
always stays on `main`, two sessions settle concurrently without touching each other, and
deleting a branch never collides with "currently checked out". A worktree is only a cache of the
branch ref — delete it and it is rebuilt at the next use; it never holds unsettled state.

A repo whose main checkout still sits on a session branch migrates on demand: the first time
that branch is asked for a checkout, a clean main checkout moves back to `main` and the branch
gets a worktree; where it cannot move (uncommitted changes, an open merge transaction, no main)
the main checkout keeps serving as that branch's checkout, only without switching back and
forth.

`agit clone` installing into two runtimes produces two local sessions, but they are **two copies
of the same work**, not two directions: `agit commit <agent>` converges on one by asking "whose
transcript has moved since install" (comparing mtime, without opening the file); if both moved or
neither did, it lists the two session ids and lets the user say. Both copies carry a freshly
minted session id, so a commit onto the occupied original branch is rejected by the continuity
check — to keep the continuation, record it under another agent name
(`agit commit <session> -n <name>`), where that root snapshot claims a lineage of its own by
content.

## 6. The backend and the web interface

- The `refs` endpoint (`/api/agents/{owner}/{name}/refs`) lists branches and version tags, each
  branch carrying the `session` / `runtime` / `code` of its tip snapshot — one branch one
  session, so the branch list is the session list.
- The session endpoints (`/sessions`, `/sessions/{id}` transcript, raw records) all take
  `?ref=`: a branch name, an `agit-<sha>` version ID or a bare sha. Where `ref` lands on a tag
  or a sha the response is **content-addressed** — one version ID always points at one content,
  so it is cached `immutable` for a year; the default, which follows the branch HEAD, can only
  be `no-cache`.
- The session count counts **session branches** (`view::session_branches`), one branch one
  session. Not "does HEAD carry a transcript": `main` is a file line, sessions each sit on their
  own branch, and HEAD carries none of them — counted by HEAD, every card steadily reads "0
  sessions", and the more correct the push gate is (the less it lets a session land on a file
  line), the steadier that 0 gets. `session/VIEW` is a derived slice of `session/log.jsonl` and
  never counts as a second session.

## 7. Legacy layouts (deprecated)

Two layouts precede today's `session/` triple: the earlier one keeps the transcript at
`sessions/<runtime>/<id>.jsonl`, several sessions under one main, and snapshots with no
`runtime` field; the later one spreads the triple (`snapshot.json` / `transcript.jsonl` /
`view.jsonl`) directly over the repo root. Both are **deprecated, with no in-place migration**.
Old agents already online stay as they are (published history is immutable, the same reasoning
as §4), but the toolchain neither reads nor writes them:

- The server refuses a legacy-layout push — gate ④·layout tests `session/meta.json`: the file is
  absent, it does not parse, or `session` records something other than a branch identity (a
  legacy layout records the path of the transcript inside the repo); each of those is a
  rejection, and the rejection text gives the way out directly: "adopt it again with
  `agit import` on the latest agit and record the first version". The read path answers 400 for
  a legacy-layout repo as a whole rather than silently rendering an empty shell with "no
  sessions".
- The client does not recognize them either: `session::list_in` counts "there is session
  content" only where `session/log.jsonl` exists and the meta both reads out and has claimed an
  identity, so to `agit commit` / `agit resume` a legacy-layout repo has no session to choose.
  `agit doctor` folds them into a single warning (the test is a filesystem trait: the
  `sessions/` directory is still there, or the repo root carries `snapshot.json` /
  `transcript.jsonl`):

  ```
  ! found 2 legacy-layout repos (session files are not under session/): alice/one, bob/two
  → no in-place migration: clear these directories, then adopt again with `agit import` and record the first version
  ```
