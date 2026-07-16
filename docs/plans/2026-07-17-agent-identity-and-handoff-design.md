# Agent identity, environment migration, and handoff — the design of record

Status: design. Supersedes the `.agit/store` pointer (a hack, to be removed) and revises the branch
model in `2026-07-16-workspace-state-primitives-design.md`. Nothing here is implemented yet.

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
`feature-a` with agent branch `main`. `git checkout` moves one; `agit -a checkout` moves the other.

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

**Resolution order for `-a` / any agent-scoped command:**

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

```
agit init [--agent <name>] [--import]   bind/mint an agent; --import adopts a legacy .agit/agent
agit agent list                          known agents, which are running, which is default
agit agent use <name>                    set MY default here (a default, NOT a lock)
agit agent add <name|url>                declare an agent in .agit.toml (clone if needed)
agit agent new <name>                    mint a new agent (works with no remote)
agit agent publish [--remote <url>]      push it to AgitHub; records remote in .agit.toml
agit agent rename <old> <new>            metadata only
agit agent show <name>                   name, aid, store, remote, sessions, environments
agit agent rebind <name> --remote <url>  override the integrity check, deliberately

agit start [--agent X] [--as claude-code|codex]   launch a session carrying that agent's context
agit -a <git…>                            git on the resolved agent's store
agit -a merge <ref>                       reconcile with another copy / another agent (§7)
```

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

`agit -a merge <target>` takes **another memory**. Never a code branch. (`feature-a` as an operand was
my error: one agent that worked feature-a then feature-b has *one* memory spanning both — nothing to
reconcile, and it correctly finds no divergent tail.)

**One command, one concept.** A target is an **agent name** or a **ref** — both name a memory, so the
UX must not split them (an earlier draft had `--agent frontend` vs `frontend/main`; that exposed our
plumbing, and `frontend/main` also required a hidden `remote add` + `fetch` that dragged another agent's
whole history into this store for no reason — the agent is already on disk at `~/.agit/agents/<aid>/`).

```
agit -a merge <X>
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
$ agit -a merge origin
origin is this agent (agt_01J…) — reconciling, then merging the histories.

$ agit -a merge frontend
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
$ agit -a log           # one memory, two environments
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

**codex names** (`codex resume <name>`) resolve via `~/.codex/state_5.sqlite`:
`SELECT rollout_path FROM threads WHERE title = ? AND cwd = ? AND archived = 0 ORDER BY updated_at_ms DESC`.
**No file fallback** — a dropped file is never name-resumable. Three traps:
1. **The name self-destructs**: every resume re-upserts `title = excluded.title`, derived from the
   rollout's **first user message**. A title set in SQL works *exactly once*.
2. **The fix (proven)**: make the rollout's **first user message literally be the name** → the upsert
   re-derives the same title forever, self-healing.
3. **Everything else fails silently** (fresh empty session, exit 0).

**Decision:** set the name via codex's own **app-server RPC `thread/name/set {threadId, name}`** (not raw
SQL — `state_5` is a generation counter with 40 checksummed migrations, several historically
destructive), prepend the synthetic first turn, **verify by re-resolving**, and **fall back to UUID** on
any mismatch. Names are opt-in; UUID always works.

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

---

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

Legacy detection belongs in **the resolver**, so every entry point (`-a`, snap, watch, start, resume,
merge) gives the same actionable error — not just `init`.

Honest sizing: the resolver+import+test-isolation slice is ~2–3 days. The **union** of what this doc
requires is **3–5 weeks**.

---

## 13. Acceptance criteria

1. **PRD #3** — frontend agent continues in backend: `agent add frontend && agent use frontend && start`
   carries its latest session; a later snap lands in the **same** store; both repos show one history.
2. **PRD #2** — two agents reconcile: `agit -a merge frontend/main` runs the dialogue, leaves **both**
   agents intact, emits a resumable merged session; **fails loudly** when no merge-base exists.
3. **PRD #1** — takeover: bob clones the code repo, `agit init` clones the declared agents from AgitHub,
   `agit start` continues alice's agent; both push to one agent and reconcile after diverging.
4. **Two agents at once** in one repo, each capturing to its own store, attributed by launch record.
5. `agit start --as codex` opens a session that **recalls a fact only its history knows** (not a fresh
   session) — the regression that motivated §9.
6. An old repo with `.agit/agent` gets one actionable error from **every** agent-scoped command.
