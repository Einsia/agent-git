# AgentGit primitives — the Workspace State model

**Status:** proposal / design. Not implemented. Several decisions still open (see [Open decisions](#open-decisions)).
Supersedes the `reconcile`-centric model. Relates to
[`2026-07-15-claude-codex-conversion-design.md`](2026-07-15-claude-codex-conversion-design.md) (the resume/convert
machinery this design leans on).

---

## 1. Context

The PRD reframes what gets versioned. Git assumes the author lives *outside* the system and versions only the code
environment. An agent lives *inside* the system and accumulates state, so the versioned object must grow:

```
Git State       = Environment State
AgentGit State  = Workspace State
                = Agent State + Environment State + Relations
```

- **Agent State** — what the agent knows, why it decided that way, how far it got.
- **Environment State** — the repo + commit + stash the judgments rest on.
- **Relations** — the work edges: Agent↔Environment, Agent↔Agent.

The one-line frame that drives everything below: **AgentGit is git, but the versioned object is Workspace State, not
code.** Reuse git for the DAG and transport; add agent-state semantics on top.

## 2. The core realization: `reconcile` dissolves

Today's `reconcile` does two jobs, and both are artifacts of a pre-resume era:

1. **Distill** raw sessions → a `CLAUDE.md` the next session loads. This existed because there was no real *resume* —
   injecting a summary into `CLAUDE.md` was the only way "the next session sees the context."
2. **Merge** two branches' sessions.

We since built true resume (convert + register → `claude --resume` / `codex exec resume`). Once you can **resume the
actual (merged) state**, distilling it into a lossy `CLAUDE.md` is a step backward — and it contradicts the PRD's
headline principle, **无损 (lossless)**.

So the loadable output of a merge should be a **resumable state**, not a summary. `reconcile` splits cleanly and
disappears: its *merge* half becomes `sync`; its *load* half becomes `resume`; `CLAUDE.md`-injection is dropped
entirely (a "brief a newcomer" digest survives only as a read-only Hub *view*, not a primitive).

## 3. The primitives

Six verbs, git-shaped, operating on Workspace State.

| Verb | git analog | What it does | PRD req |
|---|---|---|---|
| `snap` | commit | Version the current Workspace State onto the timeline. Mostly **automatic** — agent state accrues as it works; rarely typed. | substrate |
| `resume <state>` | checkout | **The universal loader** — go live from any state. `--as codex\|claude` converts runtime; `--env <repo@commit>` rebinds to a new environment. Loading *someone else's* state = takeover. | 1, 3 |
| `sync <other>` | merge | Revive **both** agents and let them **converse** over the divergent tail → a **resumable merged state**. Stops only at real conflicts. | 2 |
| `push`/`pull`/`clone`/`fetch` | (same) | Move Workspace States between people/machines, lossless, permissioned. | 1 |
| `graph` / `timeline` | log | The Workspace-State DAG + the Relations edges (agent↔env, agent↔agent). | substrate |

`compare` (a structured "what does A know that B doesn't") was dropped — the `sync` dialogue *discovers* the
divergence by talking, so a separate diff primitive isn't needed. A human-facing "what does this agent know" is a
read-only Hub digest (rendered on demand), not a core primitive.

Two moves do most of the work:

- **`resume` is one primitive doing four jobs** — continue, takeover, cross-runtime, cross-environment — by changing
  *bindings*, not verbs. The whole "移动 agent / 切换端 / 接管别人 agent" surface collapses into checkout-with-parameters.
- **`sync` is the only smart-merge**, and its output is a state you `resume`, never a file you read.

### PRD coverage

| PRD requirement | Covered by |
|---|---|
| 1. session conversion + lossless push/pull + exposure/permissions + **takeover** | `resume --as`, `push`/`pull`/`clone`, Hub + permissions, `resume <other's state>` |
| 2. lightweight branch context sync (dialogue, divergence-only) | `sync` (two live agents converse over the divergent tail) |
| 3. cross-end task switching (env migration) | `resume --env <target>` |
| 4. harness (skills / MCP) | a facet of Agent State — the harness travels with `resume` (see §5) |
| 5. easy github migration (agit + github first) | substrate is plain git; timelines live in the git DAG |

## 4. Naming principle

Not a style choice — a constraint agit already imposes on itself. **agit is a transparent git wrapper: git verbs
already mean the git thing** (`agit -a merge`, `agit -a log`, `agit commit` pass straight through). So `merge`, `log`,
`diff`, `checkout`, `commit` are *taken* by their real git meaning.

> **Rule:** reuse a git term only when the op *is* that git op (± a little) and is deterministic. Invent a term when
> the op is categorically different — a model in the loop, or new-object semantics.

- `clone` = git clone + install hooks → deterministic, same intent → **reuse OK** (already done).
- `sync` ≠ git merge — it is non-deterministic, model-in-the-loop → **new word** (and `merge` is taken anyway).
- `resume` ≠ `checkout` — boots a live agent, not files → **new word**.
- `graph` (not `log`) because `log` is taken by passthrough.

Side effect, and a good one: the vocabulary becomes a **tell**. A git word behaves exactly like git (deterministic,
familiar); an agit word means an agent-semantic op that may involve a model. Users learn the boundary for free.

**Open naming call:** the smart-merge op. `sync` (implies mirroring — undersells it), `reconcile` (accurate English,
but baggage), `integrate`, `converge`. Leaning `reconcile` (keep the *word*, gut the *mechanism*) or `converge`.
Used `sync` as a placeholder throughout this doc.

## 5. Substrate

- **Workspace State is a DAG on git** (agit + github, req 5). Fork = branch, merge-base = common context, divergent
  tail = states after the split. The PRD's "two time trees, sync only after divergence" is literally git's merge-base
  on Workspace States. Environment State (repo + commit + stash) and the Relations edges are already captured today
  (`environment.rs`, `workspace.rs`).
- **Agent State is a single layer: the lossless transcript.** No structured "understanding," no derived schema. This
  was reconsidered and rejected — any schema is a lossy projection (drops ordering, tacit "why," dead-ends, taste), and
  a maintained structured artifact is exactly what got deleted on 2026-07-13. `sync` doesn't need it, because the two
  agents converse with their **full** context (see §6).
- **Exposure** ("谁能查看这个 agent 知道什么") is a **read-only digest rendered on demand** by the Hub (cache the render,
  invalidate on push) — a human convenience *view*, never a canonical layer the primitives depend on.
- **Harness (skills / MCP)** is part of "how the agent works" → a facet of Agent State that `resume` restores, so a
  resumed/taken-over agent comes back with its tools, not just its transcript.

## 6. `sync` = two live agents, in dialogue (RESOLVED)

**Decision:** no structured intermediate. `sync` revives **both** agents as live sessions (that's `resume`), each
holding its *complete* transcript, and they **converse** over the divergent tail until they've reconciled or hit a
conflict that needs a human. Nothing is compressed away, because each agent reasons from everything it actually knows.
This matches the PRD's "直接对话!" and keeps 无损.

Why not a structured "understanding": it's lossy (a projection of the transcript), it's a maintenance burden, and it's
a half-rebuild of the just-deleted fact model. `sync`-as-dialogue removes the need for it — the agents *discover* the
divergence by talking, not by diffing pre-digested JSON.

### Spike (2026-07-16) — the mechanism works

Two real `claude` sessions were seeded with a rigged **cross-cutting** scenario a summary-merge would miss
(A: login rate limiter keyed on `user_id`; B: renamed `user_id`→`uid` everywhere), then driven through a headless
resume loop (`claude --resume <id> -p "<other's message>"`, relayed back and forth). Result:

- **B caught the cross-cutting conflict immediately**: *"your limiter buckets on `user_id`, which won't exist
  post-merge → every bucket keys on undefined and collapses to one shared bucket."* Exactly the failure only dialogue
  catches.
- **They surfaced a subtler ambiguity themselves** — does B's rename land on the wire format or only internal code? —
  realized neither could settle it *from memory*, and escalated: *"someone needs to read the login body-parsing site…
  hold the limiter change until then."* The "stop only at a real conflict" behavior, emergent.
- Clean `DONE` termination, 5 turns.

Two learnings baked into the design:

1. **`DONE` ≠ resolved.** The agents converged on *what is undecided*, not the decision. The orchestrator must
   distinguish "converged clean" from "converged on an open conflict" — the latter is the signal to interrupt the user.
2. **Give the dialogue agents repo/tool access.** In the spike they *wanted* to `grep` the parser to settle the
   ambiguity but had no repo. Running `sync` with the agents pointed at the real code lets them resolve a whole class
   of conflicts by *reading*, not guessing — before ever bothering the user.

So §6's old "pure-transcript vs two-layer" question is closed: **pure transcript, dialogue-native.** That also keeps
the change *small* (§8) — no understanding layer to build.

## 7. The `sync` flow

Chosen interaction model: **watch, but interrupt the user only at real conflicts** (extends reconcile's "只把真正矛盾
的点拎出来问你"). Example — two agents about to merge feature A and feature B:

```
$ agit sync feature-b
◆ base: 3 shared states (skipped)
◆ diverged: A +2 states · B +3 states

A(feature-a)→B: "I keyed the login rate limiter on user_id. did you touch auth?"
B→A:            "yes — renamed user_id → uid, and added /login throttling."

⚠ conflict: B's rename breaks A's rate limiter (both at src/auth.js:20)
   [1] keep user_id   [2] adopt uid (update A's limiter)   [3] let them decide
 you> 2

✓ merged state → resume-able   (1 conflict resolved, 0 open)
  next:  agit resume  ·  then git merge feature-b (the code)
```

The payoff over a one-shot summary-merge: the *cross-cutting* conflict (B's rename silently breaking A's limiter) is
only visible when each side is backed by its **full** state and can question the other. A brief-based merge would miss
it. This is why `sync` is heavier than a routine pull, and reserved for deliberate pre-merge integration.

## 8. What changes in the code

**Remove** (clean, cohesive, ~320 lines + docs):

- `session.rs::reconcile` and its ~14 helpers (modes, `brief`/`brief_blob`, `synthesize`/`deterministic_context`,
  `merge_prompt`/`split_reply`, `persist_conflicts`, …).
- `commands.rs::write_claude_block` + `merge_managed` — the whole `CLAUDE.md` managed-block path.
- `main.rs` `reconcile` dispatch + `parse_reconcile`.
- `reconcile` / `CLAUDE.md` docs across README / 使用说明 / architecture / hub.md.

**Keep / repurpose** (most of the tree — much of it built in the 07-15/16 work):

- `push`/`pull`/`clone`/`fetch`/`log` — git passthrough → unchanged.
- `convert` + `register` → the core of `resume` (cross-runtime + install-loadable-session).
- `sync`-mirror + auto WorkspaceRevision → `snap`.
- `environment.rs` (Env State), `workspace.rs` (relations + restore) → `resume --env`, `graph`.
- `convo.rs` `ConversationIR` (raw + kinds) → Agent State transcript layer.
- `register.rs` headless-resume install → the substrate `sync`'s dialogue loop drives.
- `scan` (secret gate); `agit-hub` + `hub-ui` (exposure/view — retarget rendering, not rebuild).
- `llm.rs` incl. the codex backend → not the dialogue itself (that's real resumed agents), but still used for
  the facilitator/orchestrator prompts.

**Add** (net-new):

- The `sync` **dialogue orchestrator** — revive both agents, relay messages over the divergent tail, detect
  termination (converged-clean vs open-conflict, per the spike), surface real conflicts, capture the resolution into a
  resumable merged state. Run the agents **with repo/tool access** so they self-resolve by reading code.
- `resume` / `graph` verbs (mostly wiring over `convert`/`register` and `workspace`).

**Sizing:** *small and safe.* Pure-transcript (§6) means no understanding layer to build. The one genuinely new piece
is the dialogue orchestrator; everything else is removal + renaming + wiring over machinery that already exists.

## 9. Decided

- Drop `reconcile`-as-`CLAUDE.md`-distillation; the merge output is a **resumable state**.
- No compatibility burden — clean primitive set.
- `resume` is the universal loader (takeover / cross-runtime / cross-env are bindings).
- Naming principle: git terms stay bound to git; agent-semantic ops get new names.
- `sync` interaction model: watch, interrupt only at real conflicts. **Inline resolution shipped** — you decide each
  open conflict at the prompt and the decision is recorded into the merged session.
- **Agent State = single-layer transcript.** No structured "understanding" (rejected: lossy + maintenance burden). §6.
- **`sync` = two live agents in dialogue** (not a mediator model, not a structured diff). Spike-verified 2026-07-16. §6.
- Dialogue agents run **with repo/tool access** so they self-resolve by reading code; orchestrator distinguishes
  converged-clean vs open-conflict.
- **`reconcile` and the CLAUDE.md managed block are removed** (superseded by `sync`); the mirror is now `snap`.
- **`sync` supports claude-code and codex** (`--from codex`); both revived read-only in their own worktrees.
- The whole CLI + Hub is now **English**.

## Open decisions

1. **Integration-branch directionality** — `--both` ships; a dedicated integration-branch mode is still a maybe.

*Resolved:* **`snap` cadence → fully automatic** (`462ab3e`). `agit -a snap --watch` watches the runtime's session
dump and, whenever it changes and then settles (debounced), auto-mirrors + auto-commits into the Agent Store —
runtime-agnostic, secret-hook still enforced. One-shot `agit -a snap` remains for manual capture. Verified live:
create/modify a session → two debounced `auto-snap` commits.

*Resolved since:* **multi-session per side** (`df3318f`) — a branch's sessions collectively represent the agent;
`sync` resumes the richest on-branch session as the voice and briefs it on the rest (the diff stays the code
ground-truth). Verified end-to-end: a decision from a non-voice session shaped the merge. **`resume` and `graph`
are now first-class verbs** (`08142c2`): `agit resume <session> [--as rt] [--exec]` is the universal loader (verified
same-runtime and cross-runtime), and `agit graph` renders the Workspace-State timeline + relation edges. **Codex-side
`sync`** verified codex↔codex.

## Resolved: which code each agent sees (**two worktrees + diff**)

*(Was the biggest open item; resolved and verified 2026-07-16.)* Each agent runs in **its own branch's checked-out
tree** (a detached `git worktree` per side) and is handed **its own diff since the merge-base** as ground truth. The
first single-tree run had B (the rename branch) read A's tree, find no `uid`, and honestly report "no surface." With
two worktrees the second run was decisively better: each side reasoned from its real code, B caught the silent
clean-merge hazard, A rejected a wrong fix, B retracted an unsupported claim, and they escalated two clean items.
Robustness note learned along the way: don't derive the code branch from a single `session.gitBranch` (a session spans
branches, and project dirs accumulate) — A comes from the live checkout, B from a session branch that *differs* from A.
Safety verified: `--allowedTools Read Grep Glob` truly blocks writes and bash (the agents can read code, nothing else).

## Status of the build (2026-07-16)

MVP `agit -a sync <ref>` shipped and verified end-to-end with real `claude` (`d42123b`, then `06d716e`): revives both
sides as fresh read-only copies (via convert/register — `resume` really is built on convert), runs the dialogue with
**each agent in its own branch worktree + diff**, synthesizes RESOLVED / OPEN, archives the transcript to
`sessions/sync/`, and leaves a resumable merged session. User-facing messages are English. The mirror command was
renamed `sync`→`snap`. `reconcile` still exists (not yet removed).

## Appendix: history

The hand-authored **fact / evidence model was deleted 2026-07-13** in a five-commit sweep (`1fb7c92`…`4dab39d`). The
rip-out (`7e0611c`) removed eight modules, ~2,500 lines: `claim.rs` (403), `evidence.rs` (254), `summarize.rs` (239),
`facts.rs` (221), `merge.rs` (187), `validate.rs` (153), `extract.rs` (149), `align.rs` (98), + `tests/merge.rs` (102).
We considered reviving a lighter, LLM-derived version as an "understanding" layer, then **rejected it** (§6): the
`sync`-as-dialogue model removes the need, and any structured projection reintroduces the lossiness that motivated the
deletion in the first place.
