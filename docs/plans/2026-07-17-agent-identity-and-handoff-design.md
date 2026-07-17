# Agent identity, environment migration, and handoff — the design of record

Status: implemented and shipped. Supersedes the `.agit/store` pointer (removed) and the branch model in
`2026-07-16-workspace-state-primitives-design.md`. This document is kept as the record of the design.

---

## 1. What the PRD asks for, and what's missing

```
Git State      = Environment State
AgentGit State = Workspace State = Agent State + Environment State + Relations
```

| PRD requirement | Today |
|---|---|
| 1. Session conversion + lossless push/pull + 上下文暴露 + 权限控制 (takeover) | convert/push/pull work; **no agent identity, so no exposure model and no takeover** |
| 2. Lightweight context sync between **两个 agent**, by dialogue, only after their common context | `agit -a merge` exists, but is built around *code branches*, not agents |
| 3. 跨端任务自由切换 — one agent finishes frontend, continues on backend (环境的迁移) | **impossible**: the store lives at `<env>/.agit/agent`, welded to one repo |
| 4. Harness 层 (Skill, MCP) | capture shipped (project scope); restore shipped |
| 5. 从已有 github repo 方便迁移；初期 agit+github | `agit init` works in any repo; Hub hosts named agents |

The root problem is #3: **Agent State is welded to one Environment.** Fixing that forces an identity
model, which in turn is what makes #1's exposure/takeover and #2's agent-to-agent sync expressible.

---

## 2. The model, in plain terms

- **An agent is a memory.** A git repo full of the transcripts of what it did. It is *not* a person and
  *not* a repo-shaped thing — it is named for **what it knows** (`frontend`, `payments-api`).
- **An environment is a code repo.** An agent works in many; a repo hosts many agents. Many-to-many.
- **A session is one transcript.** It *notes* its cwd and git branch the way it notes the time.
- **Relations** pair an agent revision with the env commit it was looking at.

### Three axes — only one of them is a branch

| Axis | Modeled as |
|---|---|
| Agent | one store (one remote repo) |
| Environment | **data on each session** (cwd / repo identity) — never a branch, never a directory decision |
| Code branch | **data on each session** — a note, not a filing system |
| *Divergence between copies of an agent* | **an agent branch** (ordinary git) |

**Code branches and agent branches both exist and are independent.** You can sit on code branch
`feature-a` with agent branch `main`. `git checkout` moves one; `agit a checkout` moves the other.

### What was wrong before (and is wrong in the code today)

`sync.rs` decides "did these two diverge?" by **comparing code branch names**:

```rust
let branch_b = ... peer_branch(c, a);            // finds a branch != branch_a
if ba == bb { return Ok(false); }                // "same branch → nothing diverged to reconcile against"
```

Two teammates who both worked on `main` get `branch_b = None` → **no grounding, silent single-tree
fallback** — and the comment asserts nothing diverged, which is false. They diverged in their *sessions*.
Divergence is a property of **memories**, not of branch names.

**Fix:** ground each side on the **env commit it was actually paired with** (`WorkspaceRevision.env.head_commit`
— Relations, which the PRD already defines), not on a branch name. Commits are unambiguous across
same-branch divergence *and* across environments (`main` in web ≠ `main` in api).

---

## 3. Identity

| Layer | Value | Travels? |
|---|---|---|
| **Identity** | `aid` = `agt_<uuid>`, minted once, in the store's `agent.toml` | committed *in the store* |
| **Local store** | `~/.agit/agents/<aid>/` | — |
| **Name** | mutable label | hint in `.agit.toml` |
| **Remote** | AgitHub URL (or any git URL) — a *locator* | in `.agit.toml` |
| **Registry** | `~/.agit/registry.json`: name→aid — a **rebuildable cache** (`agent list --repair`) | local |

**Not the URL:** a URL is a locator. `git@`/`https://` are one agent; a hub migration rewrites every URL;
mirrors give one agent two URLs. Decisively: **you mint an agent before any remote exists** (work solo,
publish later) — under URL-identity a local agent has no identity, and publishing would *change* it,
moving the store and invalidating bindings teammates already committed.

**Not the name:** names collide; renaming is labelling.

**Store keyed by `aid`** ⇒ rename and publish are pure metadata edits; no directory ever moves, so a
running watcher is never orphaned.

**`id` in `.agit.toml` is an integrity check.** If `frontend.git` is recreated on the hub, or DNS moves,
name/URL identity silently binds you to a *different* agent wearing the same name. With the aid, agit refuses:

```
error: this repo is bound to agt_01J… (frontend), but https://hub/frontend.git is agt_02X…
       If intentional: agit agent rebind frontend --remote <url>
```

---

## 4. Binding and resolution

```toml
# .agit.toml — COMMITTED at the code-repo root. This is what makes collaboration work.
version = 1

[[agent]]
id     = "agt_0190f3a1-4c2b-7f1e-8a3d-9b2c1d4e5f60"
name   = "frontend"
remote = "https://hub.acme.com/frontend.git"

[[agent]]
id     = "agt_0190f4b7-9d81-7c02-b6aa-2f5e8c7d3a11"
name   = "api"
remote = "https://hub.acme.com/api.git"

[defaults]
agent = "api"        # what a FRESH clone activates — not what you have active
```

**Resolution order for `agit a …` / any agent-scoped command:**

1. `--agent <name|aid>` — per-invocation
2. `$AGIT_AGENT` — per-shell (this is how you run two agents at once)
3. **active pointer** — `git rev-parse --git-path agit/active` (per-worktree, local, gitignored)
4. `.agit.toml [defaults] agent`
5. actionable error — never a silent fallback

**The rule that stops this becoming `.agit/store` again:**

> A gitignored local file is legitimate **iff its absence is fully recoverable from committed state.**

`.agit/store` failed it (an absolute path that was the *only* resolver → didn't travel → nothing worked).
`.agit/active` passes: delete it and you fall back to `[defaults]`. It is a preference, not a pointer.

---

## 5. Commands

### `-a` is gone. One namespace: `agit agent …`, alias `agit a …`

`-a` was a flag *before* the verb, which produced the tool's worst footgun: `agit -a commit` (agent) vs
`agit commit -a` (code, `-a` is git's stage-all). A subcommand cannot be transposed, so the footgun dies.

```
agit a <git…>            git, on the resolved agent's store   (agit a log / commit / push / pull)
agit a <mgmt-verb> …     agent management (closed set, below)
agit agent …             the long form; `a` is an alias
agit <git…>              git, on your code repo (unchanged)
```

**Resolution:** the word after `a` is checked against a **closed, documented set** of management verbs;
anything else is handed to git. The set is deliberately chosen to **not collide with git's namespace**:

| Verb | Why not the obvious name |
|---|---|
| `agit a list` | — |
| `agit a use <name>` | sets MY default here — a default, **not a lock** (§5.2) |
| `agit a new <name>` | mint an agent; works with no remote |
| `agit a track <name\|url>` | **not `add`** — `git add` is far too common to shadow |
| `agit a info <name>` | **not `show`** — `git show` is a real verb |
| `agit a rename <old> <new>` | metadata only |
| `agit a publish [--remote <url>]` | push to AgitHub; records the remote in `.agit.toml` |
| `agit a rebind <name> --remote <url>` | deliberately override the integrity check |
| `agit a merge <target>` | the dialogue merge (§7) — shadows `git merge` **on purpose** |
| `agit a import` | adopt a legacy `.agit/agent` |

Everything else — `agit a log`, `agit a add -A`, `agit a commit`, `agit a push`, `agit a diff`, `agit a
branch`, `agit a checkout` — is plain git on the store.

Top level stays code-repo-shaped: `agit init`, `agit start`, `agit watch`, `agit graph`, `agit
workspace`, `agit resume`, `agit convert`, `agit scan`, `agit adapter`, and git passthrough.

### 5.1 `agit start` — the smooth path

1. resolve the agent (§4)
2. pick **its latest session from any environment** (from the store index, *not* from git-log topology:
   `git log --name-only` prints **nothing** on a merge commit, so a log-derived leaf-finder breaks
   exactly after a `merge`/`pull`)
3. resolve the **runtime** (§5.3) — never assume claude
4. rebind cwd to this repo; keep the paths it recorded elsewhere (real memory of that codebase)
5. materialize + install (id is **always a UUID** — §9)
6. **write the launch record** `session-id → agent` (§6)
7. ensure this repo's watcher is running (§6)
8. exec the runtime

`agit a use X` prints the equivalent manual command, so plain `claude`/`codex` always works too.

### 5.2 Two agents at once, same project

`use` sets a default; it is not a lock. Selection is per-invocation:

```console
# terminal 1                    # terminal 2, same repo, same time
$ agit start --agent frontend   $ agit start --agent api
```

`--agent` **does not** flip the default. (An earlier draft made it sticky so capture would file
correctly — unnecessary once the launch record owns attribution, and actively wrong here: it would make
two concurrent agents fight over one pointer.)

### 5.3 Runtime parity — claude-code and codex are peers

Today claude-code is hard-coded as the default in `snap`, `sync`, and `convert` (`let mut rt =
"claude-code".to_string()`), and codex reads as an afterthought. That is a bug of framing, and it leaks
into behaviour (`agit -a snap` silently means *claude*).

**Rule: there is no default runtime.** Resolve, in order:

1. `--as <rt>` / `--from <rt>` — explicit
2. **the session's own runtime** — a session knows what produced it; `start`/`resume` continue in it
3. the agent's sessions: exactly one runtime present → that one
4. both present → **ask** (or take the most recent, and say which); never silently pick
5. neither → error naming both

`snap` and `watch` capture **both** runtimes (watch already does; snap must stop defaulting).
Every user-facing list, error, and doc names them in the same breath, alphabetically: `claude-code, codex`.

### `agit start` — the smooth path

1. resolve the agent (§4)
2. pick **its latest session from any environment** (see §6 — from the store index, *not* from git-log
   topology: `git log --name-only` prints **nothing** on a merge commit, so a log-derived leaf-finder
   breaks exactly after a `merge`/`pull`)
3. rebind cwd to this repo; keep the paths it recorded elsewhere (they're its real memory of that codebase)
4. materialize + install (id is **always a UUID** — §9)
5. **write the launch record** `session-id → agent` (§6)
6. ensure this repo's watcher is running (§6)
7. exec the runtime

`agit agent use X` prints the equivalent manual command, so plain `claude`/`codex` always works too.

### Two agents at once, same project

`use` sets a default; it is not a lock. Selection is per-invocation:

```console
# terminal 1                    # terminal 2, same repo, same time
$ agit start --agent frontend   $ agit start --agent api
```

`--agent` **does not** flip the default. (An earlier draft made it sticky so capture would file
correctly — unnecessary once the launch record owns attribution, and actively wrong here: it would make
two concurrent agents fight over one pointer.)

---

## 6. Capture: watcher and attribution

**The problem:** the runtime dumps per *project directory*, not per agent
(`~/.claude/projects/<cwd-slug>/`). Two agents in one repo write to the **same folder**. The active
pointer cannot tell their sessions apart — attributing by it **misfiles silently**, into the wrong
agent, and pushes to the wrong team.

**The fix:** `agit start` launched the session, so agit knows whose it is.

- **Launch record**: `session-id → {agent aid, env, runtime, started}`, written at launch.
- **Capture reads the launch record**, never the active pointer.
- Sessions started by plain `claude` have no record → attributed to the repo's **default** agent, and
  reported as such (never silently).

**One watcher per environment, not per agent.** Two agents share one dump folder; per-agent watchers
would fight over it. The repo's watcher reads the folder once and routes each session to its owner.

- pidfile/log move to **`<env>/.agit/`** — they cannot live in `<store>/.git`, because a shared store
  means two repos collide on one pidfile.
- `agit start` ensures the watcher is up.
- Every store writer (snap, pairing record, merge, restore) takes **one lock owned by the store** — a
  shared store now has concurrent writers by design.

**Store layout** (one agent, many environments):

```
~/.agit/agents/<aid>/
  agent.toml                       # identity (committed)
  sessions/<env-slug>/<runtime>/<id>.jsonl
  sessions/<env-slug>/<runtime>/<id>.agit.json   # sidecar: agent, env, parent, last_activity
  harness/<env-slug>/<runtime>/…
  merges/…                         # dialogue transcripts (was sessions/sync/)
```

**Env identity is coarser than the dump partition — do not conflate them.** One environment can have
many checkouts: this machine has **231 worktrees of one repo**, sharing a root commit, each with its own
claude slug dir. Transcripts key on env (they're UUID-named and disjoint); anything *project-scoped*
(harness, memory) must key on the **checkout**. Conflating them is what makes memory ping-pong.

`EnvId` = dual key: **root commit + normalized origin URL**; record both, match on either, skip the
root-commit key when `git rev-parse --is-shallow-repository`.

---

## 7. Merge — always "reconcile my memory with another memory"

`agit a merge <target>` takes **another memory**. Never a code branch. (`feature-a` as an operand was
my error: one agent that worked feature-a then feature-b has *one* memory spanning both — nothing to
reconcile, and it correctly finds no divergent tail.)

**One command, one concept.** A target is an **agent name** or a **ref** — both name a memory, so the
UX must not split them (an earlier draft had `--agent frontend` vs `frontend/main`; that exposed our
plumbing, and `frontend/main` also required a hidden `remote add` + `fetch` that dragged another agent's
whole history into this store for no reason — the agent is already on disk at `~/.agit/agents/<aid>/`).

```
agit a merge <X>
  ├─ X is a known agent name?   → that agent's store
  ├─ X is a ref in my store?    → that ref
  ├─ BOTH                       → selector; ask (scripts: --agent X / --ref X)
  └─ neither                    → error + suggestions
```

### Mode is decided by **identity**, not by git history

`agent.toml` is committed **inside** the store, so the aid is readable at any target
(`git show <ref>:agent.toml`, or the named agent's store directly):

| | same aid (my agent, another copy) | different aid (a different agent) |
|---|---|---|
| example | `origin` — a teammate's push | `frontend` |
| outcome | dialogue → **fuse**: git merge; one memory again | dialogue only → **both stay intact** |
| PRD | #1 takeover / shared agent | #2 两个 agent → then 接着合并 the code |

Deciding on the aid — not on whether a merge-base happens to exist — removes the guess entirely. It is
also what fixes the silent no-op: today, no merge-base ⇒ `git diff A...B` exits 128 with **empty
stdout**, which `sync.rs` reads as "no divergent tail" ⇒ exit 0, does nothing. Cross-agent must
enumerate the peer's sessions two-dot instead, and never attempt a git merge.

agit states the mode it chose:
```console
$ agit a merge origin
origin is this agent (agt_01J…) — reconciling, then merging the histories.

$ agit a merge frontend
frontend is a different agent (agt_02X…) — reconciling by dialogue; histories stay separate.
```

Cross-agent output = a resumable merged session + an archived transcript; you then merge the **code**
yourself — exactly 「同步一下上下文，然后接着合并」.

**Today this path is a silent no-op** and must be fixed: with no merge-base, `git diff HEAD...other/main`
exits 128 and prints **nothing on stdout**, which `sync.rs` reads as "no divergent tail" → exit 0, does
nothing. Detect it (`git merge-base` rc=1) and enumerate the peer's sessions two-dot instead.

**Grounding** for both cases: each side's **paired env commit**, not `branch_tip(env, name)` (§2).

---

## 8. Environment migration, takeover, exposure

**跨端自由切换 (PRD #3)** — the frontend agent continues in the backend repo:
```console
$ cd ~/code/api
$ agit agent add frontend && agit agent use frontend
$ agit start            # carries its latest session (from ~/code/web) into api
$ agit a log           # one memory, two environments
```
Continuity comes from **session lineage** (the resumed session literally contains the prior
conversation), not from branches.

**Takeover (PRD #1) = shared.** We both push to **one** agent, like a shared git repo. No ownership
transfer, no locking — git already models this. When we diverge, §7(a) reconciles us.

**权限控制 / 上下文暴露** = the agent's **remote is the boundary**. An agent is one repo; who can read it
is who can read that repo. AgitHub already enforces this (sha256 tokens, write tokens for push, reads
gated by `--private`). Branches are *not* an ACL boundary — which is an independent reason
**one agent == one repo**.

---

## 9. Runtime facts (reverse-engineered; design against these, not against docs)

**Install id is ALWAYS a UUID, for both runtimes.** Verified against codex 0.144.4 with a fact only
history could know: a UUID-id rollout absent from codex's index **recalled** it; the same file under a
proper-name id answered from thin air with **exit 0**. A non-UUID thread id hard-errors — the only loud
failure in the entire path.

**codex names are NOT resumable — at all.** An earlier draft of this section proposed reaching
name-resumption via the app-server RPC plus a self-healing synthetic first turn, and called the latter
"proven". Both were re-tested against codex 0.144.4 and **both are false**. Four runs, each name-resume
paired with a UUID control on the *same thread*, each asked for a codeword only its own history holds:

| thread     | how the name was set                | `codex exec resume <name>` | `codex exec resume <uuid>` |
|------------|-------------------------------------|----------------------------|----------------------------|
| `019f6c84` | app-server `thread/name/set`        | `NONE` (exit 0)            | `PANGOLIN73VAULT`          |
| `019f6c83` | rollout's first user message *is* the name | `NONE` (exit 0)     | `NARWHAL31TUNDRA`          |

The UUID controls recall the codeword, so the history is there and the sessions are sound; name-resume
silently starts a **fresh, empty session and exits 0**. This is the failure mode agit exists to prevent,
so the rule is unconditional: **the install id is ALWAYS a UUID.** There is no opt-in name path to fall
back *from* — `install_id` (src/commands.rs) is the enforcement point.

Why the earlier draft misread it: codex's v2 protocol carries `Thread.name` ("Optional user-facing thread
title") and `Thread.preview` ("Usually the first user message") as **separate** fields. The first-user-
message trick only ever writes `preview`; it leaves `name` null, which is why it never self-healed a name.
`thread/name/set` does durably write `name` (it survives an app-server restart, and is distinct from
`preview`) — it just has no bearing on what `resume` will resolve.

**`thread/name/set` is still worth calling, for discoverability only.** Verified: invoking it on a rollout
agit dropped on disk — a file codex has never indexed — both succeeds *and forces the thread into the
index*, after which it appears in `codex resume`'s picker under agit's proper name. So a converted session
becomes findable by a human under the name it deserves, while the UUID does the actual resumption. The
app-server is flagged `[experimental]`, so this must **fail soft**: no app-server, no RPC, or an error →
skip the label. It can never regress resumption, because resumption never depended on it.

**claude names**: drop the file only — nothing to register. `~/.claude/projects/<cwd-slug>/<uuid>.jsonl`,
a `custom-title` record in the **tail 64KB** (re-appended on later writes), `isSidechain:false` on every
record, titles unique per project dir. **Misses are loud** — agit can trust the error.

**claude slug is many-to-one — a live bug.** `slug_for` maps every non-alphanumeric to `-`, so
`/home/user/my/app`, `/my-app`, `/my_app`, `/my.app` **all** produce `-home-user-my-app`. So `snap` in
repo A can capture repo B's sessions, label them env=A, and push them to A's team. **Fix:** give claude
the cwd-ownership filter codex already has — read each candidate's records and drop any whose `cwd` ≠
this env.

---

## 10. The handoff prompt — adopt what already works

Our `sync` handoff prompt is hand-rolled. Both vendors already solved "transfer a working context across
a boundary". Steal their structure.

**codex splits it in two** — and so must we:

*Summarizer side (425 bytes, verbatim):*
> You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will
> resume the task. Include: — Current progress and key decisions made — Important context, constraints, or
> user preferences — What remains to be done (clear next steps) — Any critical data, examples, or
> references needed to continue. Be concise, structured, and focused on helping the next LLM seamlessly
> continue the work.

*Receiver side (verbatim) — this is exactly our relay:*
> Another language model started to solve this problem and produced a summary of its thinking process. You
> also have access to the state of the tools that were used by that language model. Use this to build on
> the work that has already been done and avoid duplicating work.

**claude's compaction prompt is the richer model.** Use the **`Bkg` variant** — the "summarize an earlier
portion for a reader who will see newer messages the summarizer cannot" case — which is *semantically our
handoff*. The main variant assumes the summarizer **is** the resumer and drifts when another agent picks up.

Adopt, concretely:
1. **The three-layer no-tools clamp**, verbatim in spirit: a leading `CRITICAL: Respond with TEXT ONLY. Do
   NOT call any tools.` naming the real tools, the body restating the output shape, and a trailing
   reminder. A summarization turn that calls a tool burns the turn.
2. **`<analysis>` scratchpad, then throw it away**: force a chronological pre-pass in `<analysis>` tags,
   then strip `/<analysis>[\s\S]*?<\/analysis>/` and rewrite `<summary>…</summary>` → `Summary:\n…`.
3. **The 9 sections, in order**: Primary Request and Intent → Key Technical Concepts → Files and Code
   Sections → Errors and fixes → Problem Solving → **All user messages** → Pending Tasks → Work Completed
   → Context for Continuing Work.
4. **Preserve security constraints verbatim** — claude's prompt explicitly requires this so they survive
   the boundary. Ours must too.
5. **Add the half codex omits**: an explicit **drop list** (superseded approaches, resolved errors, verbose
   tool output already reflected on disk) and a fixed section schema. codex can be terse because it keeps
   tool state out of band; our handoff must say which substrate survives or the model will silently drop it.
6. **Use the override hooks** rather than fighting them: codex exposes `compact_prompt` /
   `experimental_compact_prompt_file` in config, plus `PreCompact`/`PostCompact` events. agit should
   snapshot before compaction rather than let a session lose detail it is supposed to version.

---

## 11. Bugs this design must fix (all evidence-backed)

| Bug | Evidence |
|---|---|
| codex proper-name installs are unresumable, fail open | verified: UUID recalled the fact; name-id hallucinated, exit 0 |
| claude slug collisions → snap captures another project's sessions | `slug_for` maps all non-alnum to `-`; 4 distinct paths → 1 slug |
| merge can't ground same-branch divergence, claims "nothing diverged" | `peer_branch` requires a *different* branch; `ba == bb → Ok(false)` |
| cross-agent merge silently no-ops | no merge-base → `git diff A...B` rc=128, **empty stdout** → read as "no tail" |
| `agit start` leaf-finder breaks after any merge/pull | `git log -1 --name-only` prints nothing on a merge commit |
| tests write into the developer's real `~/.agit` | `tests/cli.rs` + `tests/adapter.rs` have no `$AGIT_HOME` isolation |
| shared store has unlocked concurrent writers | restore/record/snap all write one index+HEAD |
| the no-tools clamp withheld nothing | `--allowedTools` is an additive GRANT; omitting it kept claude's default tools while the prompt claimed rejection |
| a committed `.agit.toml` remote is RCE on `agit a track` | verified, git 2.43: `git clone 'ext::<cmd>'` RAN `<cmd>` |

### The `.agit.toml` remote is attacker-controlled input

`.agit.toml` is **committed**, so `agit a track frontend` clones a URL chosen by whoever wrote the repo
— not by the machine running it. Clone a hostile repo, run one ordinary command, and it is code
execution. `track`'s bare-name path passed `entry.remote` straight to `git clone` with no check at all
(`looks_like_url` guards only the CLI path, and returns **false** for `ext::…` anyway).

Verified against git 2.43 — and the obvious fix does not work:

| attempt | result |
|---|---|
| `git clone 'ext::<cmd>'` | **`<cmd>` executed.** The clone then fails ("Could not read from remote repository") — *after* the payload has run |
| `git clone -- 'ext::<cmd>'` | **still executed.** `--` stops flag smuggling; `ext::` is a *scheme*, not a flag |
| `git clone '--upload-pack=<cmd>'` | not executed — but only because the destination does not exist yet, so clone dies first. An accident of argument order, not a control |

So the guard is an **allowlist of transports** (`check_remote`), refusing rather than sanitizing: a URL
agit cannot classify is one it cannot vouch for, and `track` has a safe answer — make the human paste it.
Behind it, `-c protocol.ext.allow=never` (a victim with `protocol.ext.allow=always` in their own
gitconfig is otherwise one step from RCE) and `--` for any future `-`-prefixed URL. Neither belt is
sufficient alone, which is the point: `--` does not stop `ext::`, and the config does not stop flags.

---

## 11b. UX issues to fix along the way

| Issue | Fix |
|---|---|
| `agit init` names the agent after the directory (`web`), so everyone renames immediately | **ask a human, refuse a script**: `--agent X`, else one prompt — `Agent name — what will this agent know? [web]:` — else an actionable error. The name is **never** derived silently; see below for why the dir-name fallback is banned |
| `agent track X` then `agent use X` — two commands, one intent | `track` **activates** by default (`--no-use` opts out) |
| `-a` transposition footgun (`agit commit -a` vs `agit -a commit`) | gone — `agit a commit` (§5) |
| `snap` silently means *claude* | no default runtime (§5.3) |
| `merge` printed nothing until the dialogue ended (~2 min of dead air) | stream turns live (§11c) |
| conflicts resolved via bare `print!`/`read_line`, no context | a real picker (§11c) |
| an unresolvable codex name silently starts a fresh session, exit 0 | verify by re-resolving; never trust exit 0 (§9) |
| `agit watch` output is invisible when piped (block-buffered, lost on SIGTERM) | flush per line |

### The directory-name fallback is banned, not merely discouraged

An earlier draft of the row above ended *"non-interactive falls back to the dir name"*. That fallback is
**deleted**, and the reason is mechanical rather than aesthetic: **it mints agents that can never be adopted.**

`validate_name` permitted a leading `.`, and `looks_like_url` reads any leading `.` as a **path**. So any repo
in a temp or dotted directory (`/tmp/.tmp9ndKZa`, `~/.config/foo`) minted an agent whose name `track` can never
resolve. Verified against the real binary:

```console
$ agit a new .tmp9ndKZa
minted .tmp9ndKZa (agt_85889e7b-…)          # succeeds

$ agit a track .tmp9ndKZa                    # from any other repo
error: refusing a remote agit cannot classify: `.tmp9ndKZa`
       Allowed: https://, http://, ssh://, git://, file://, git@host:path, or a local path.
```

The binding names it, `track` reads it as a path and refuses it as an unclassifiable remote, and **PRD #3 (§8)
and the fresh-clone path (§13.3) are dead for that agent**. `check_remote` — §11's fix for the `ext::` RCE —
and `validate_name` disagreed about what a name *is*, and the fallback is what walked users into the gap. The
test suite hit it first: every tempdir repo minted `.tmpXXXXXX`.

Two rules follow, and both are enforced:

1. **A name is always a human's decision** — `--agent`, or a prompt someone answered. A script that supplies
   neither gets an actionable error, never a name agit invented. This is §4's rule ("never a silent fallback")
   applied to naming: agit does not guess which memory you meant, and it does not guess what to call one either.
2. **`validate_name` refuses a leading `-`, `.` or `~`** — a name `track` cannot resolve is not a name. (A dot
   *inside* a name, `payments.api`, is fine.) Test it via the **`track` round-trip**, not the character class,
   so the test states the reason and survives someone tidying the charset later.

**The prompt is kept, and it does the teaching.** `Agent name — what will this agent know? [web]:` puts the
model in front of the user at the one moment they have to apply it — an agent is named for what it knows, and
it outlives this repo. The directory is offered as a *suggestion a human can see and reject*: a default only
once someone looked at it and pressed Enter. That is a decision. A directory used because nobody was watching
is a guess, and guessing is what this section exists to stop.

## 11c. Presentation — light TUI, not a TUI framework

Constraint: **no ratatui, no alt-screen, no full-screen takeover.** agit is a git-shaped CLI; it must
stay pipeable and scriptable. Everything below is ANSI + box-drawing + numbered pickers, degrading to
plain text when `!stdout().is_terminal()` or `NO_COLOR` is set.

**Ambiguity picker** (`merge bob` matching both an agent and a ref):
```
"bob" is ambiguous:
  1) agent  bob             agt_02X…   8 sessions · last 2h ago
  2) ref    refs/heads/bob  this agent · 3 sessions ahead
Pick [1/2]:
```

**`agit a list`** — a table, with what's running:
```
  AGENT      STATUS          SESSIONS  LAST
  frontend   ● running       11        2h ago  (here)
  api        ● running        8        5m ago
  infra      ·                2        5d ago
  default: frontend
```

**`agit start`** — a header, so you always know what you're carrying:
```
┌ frontend · ~/code/api · claude-code
└ carrying its latest session (from ~/code/web, 2h ago)
    "the login form posts user_id to /api/login"
```

**`merge`** — stream the dialogue live (it takes minutes; silence reads as a hang), then a conflict
picker with context rather than a bare prompt:
```
  A → I post user_id to /api/login …
  B → CONFLICT: I renamed that field to uid …

┌ conflict 1/2 ─────────────────────────────────
│ field name: user_id (frontend) vs uid (api)
│   1) keep uid, frontend updates its caller
│   2) keep user_id, api reverts the rename
│   3) leave open, decide later
└ your call [1/2/3]:
```

Rules: colour is emphasis only (never the sole carrier of meaning); every spinner/stream line is
`\r`-safe and flushed; anything interactive has a non-interactive counterpart that exits non-zero and
prints what needed deciding.

## 11d. AgitHub — what to fix

The PRD's 权限控制 ("who can see whose agent") is a **hub** requirement, and the hub as built cannot
express it: a token is all-or-nothing for the whole host, and reads are open by default. Ordered by
severity:

| # | Problem | Fix |
|---|---|---|
| 1 | **git-http bypasses auth per-repo**: the route is `path.contains(".git/")` + `GIT_HTTP_EXPORT_ALL=1`, so http-backend will serve **any** `*.git` under root once you're past the (open) read gate | authorize **the named agent** against the token's grants *before* handing off to http-backend; drop `EXPORT_ALL` |
| 2 | **No per-agent ACL** — the PRD's core ask | grants: token → {agent, read\|write}. `agit-hub token add bob --agent frontend --write` |
| 3 | **Reads open by default** (`--private` is opt-in) | **private by default**; `--public` is the explicit, loud opt-out. Sessions are transcripts — fail safe |
| 4 | **Binds 0.0.0.0 by default** | bind `127.0.0.1` unless `--host` is given explicitly |
| 5 | **No TLS** — tokens and full transcripts in cleartext | terminate TLS (or require a proxy and *refuse* to bind non-loopback without `--insecure`) |
| 6 | **Per-IP cap keys on the raw peer IP** → behind a proxy every user shares one IP and throttles together | trusted-proxy config + `X-Forwarded-For`; keep raw-IP behaviour when no proxy is declared |
| 7 | **Tokens never expire, can't be revoked** | TTL + `token revoke`; store `created/expires/last_used` |
| 8 | **No audit trail** — exposure control without accountability | append-only log: who read/pushed which agent, when |
| 9 | **Secrets gated only client-side** (a local hook, bypassable with `--no-verify`) | scan server-side on receive-pack; reject the push |

(1)+(2) are the load-bearing pair: without them "who can view whose agent" has no enforcement point,
and the answer to the PRD requirement is currently *"anyone who can reach the port."*

## 12. Cutover (hard, per decision)

Nested `<env>/.agit/agent` stops resolving; `scope::STORE_PTR` + `init --store` are deleted.

**Order matters — do not ship the resolver first.** The change that lets two repos share one store is
exactly the one that corrupts a flat, single-env store. Land the invariants first:

1. `$AGIT_HOME` + test isolation (`tests/cli.rs`, `tests/adapter.rs`) — *before* anything writes to `~/.agit`
2. env-partitioned layout (`sessions/<env>/<rt>/`), the store lock, pidfile → `<env>/.agit/`
3. `src/agent.rs`: identity, registry, resolution; delete `STORE_PTR`/`--store`/`AGIT_AGENT_DIR`
4. `agent` verbs + `.agit.toml` + `agit start` + launch record
5. one-shot `agit init --import` for a legacy nested store (pre-flight: refuse if a watcher is live —
   `mv` under a running daemon silently zombies it)
6. rewrite demo + all docs + tests

Legacy detection belongs in **the resolver**, so every entry point (`agit a …`, snap, watch, start, resume, merge) gives the same actionable error — not just `init`.

Honest sizing: the resolver+import+test-isolation slice is ~2–3 days. The **union** of what this doc
requires is **3–5 weeks**.

---

## 13. Acceptance criteria

1. **PRD #3** — frontend agent continues in backend: `agit a track frontend && agit start`
   carries its latest session; a later snap lands in the **same** store; both repos show one history.
2. **PRD #2** — two agents reconcile: `agit a merge frontend` runs the dialogue, leaves **both**
   agents intact, emits a resumable merged session; **fails loudly** when no merge-base exists.
3. **PRD #1** — takeover: bob clones the code repo, `agit init` clones the declared agents from AgitHub,
   `agit start` continues alice's agent; both push to one agent and reconcile after diverging.
4. **Two agents at once** in one repo, each capturing to its own store, attributed by launch record.
5. `agit start --as codex` opens a session that **recalls a fact only its history knows** (not a fresh
   session) — the regression that motivated §9.
6. An old repo with `.agit/agent` gets one actionable error from **every** agent-scoped command.
