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
| `sync <other>` | merge | Dialogue-reconcile two **divergent tails** → a **resumable merged state**. Stops only at real conflicts. | 2 |
| `push`/`pull`/`clone`/`fetch` | (same) | Move Workspace States between people/machines, lossless, permissioned. | 1 |
| `compare <a> <b>` | diff | What does A know/have that B doesn't — powers `sync`'s divergence and exposure. | 1, 2 |
| `graph` / `timeline` | log | The Workspace-State DAG + the Relations edges (agent↔env, agent↔agent). | substrate |

Two moves do most of the work:

- **`resume` is one primitive doing four jobs** — continue, takeover, cross-runtime, cross-environment — by changing
  *bindings*, not verbs. The whole "移动 agent / 切换端 / 接管别人 agent" surface collapses into checkout-with-parameters.
- **`sync` is the only smart-merge**, and its output is a state you `resume`, never a file you read.

### PRD coverage

| PRD requirement | Covered by |
|---|---|
| 1. session conversion + lossless push/pull + exposure/permissions + **takeover** | `resume --as`, `push`/`pull`/`clone`, Hub + permissions, `resume <other's state>` |
| 2. lightweight branch context sync (dialogue, divergence-only) | `sync` (+ `compare` for the divergence) |
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
- `compare`/`graph` because `diff`/`log` are taken by passthrough.

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
- **Agent State is two-layered** — the same raw+derived split as `ConversationIR`, promoted to the state level:
  - **lossless transcript** — the source of truth, what `resume` loads (无损).
  - **derived understanding** — a compact, structured "knows / why / where" with provenance, what `compare` / `sync` /
    exposure operate on (so you never diff two 100k-token transcripts).
- **Harness (skills / MCP)** is part of "how the agent works" → a facet of Agent State that `resume` restores, so a
  resumed/taken-over agent comes back with its tools, not just its transcript.

### What an "understanding" is

The structured, **derived** answer to the PRD's three questions, per state, with provenance:

```
understanding {
  goal:      "add rate limiting to login"
  facts:     [ { claim: "auth key stored under `user_id`", src: "src/auth.js:20" }, … ]
  decisions: [ { choice: "rate-limit keyed on user_id", why: "DB schema uses it", src: "turn 9" } ]
  progress:  { done: [...], next: [...], blocked: [...] }
  open_q:    [ "canonical field name — user_id or uid?" ]
}
```

It is **LLM-derived from the transcript-of-record**, regenerable, never canonical. That is the entire difference from
the hand-authored fact model that was deleted on 2026-07-13 (see [Appendix](#appendix-history)). The transcript stays
the truth; the understanding is a rebuildable cache of "what it means."

## 6. The open decision that sizes everything: pure-transcript vs two-layer

`sync` and `compare` need *something* to compare.

- **Pure transcript** — no derived layer; `sync`'s dialogue agents read each other's raw sessions and work out the
  divergence themselves. Simplest substrate, fully lossless, but every `sync`/`compare` is expensive and there is no
  cheap "what does this agent know" for exposure.
- **Transcript + understanding** — maintain a structured, cheap understanding per state; `compare`/`sync`/exposure run
  on it, `resume` still loads the transcript. Precise, cheap, greppable — but it is a derived artifact to keep fresh,
  and it is a slim rebuild of what was torn out three days ago (in LLM-derived, not hand-authored, form).

This single call decides whether the whole change is *small* or *medium-large* (see §8).

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
- `convo.rs` `ConversationIR` (raw + kinds) → Agent State transcript layer + the understanding substrate.
- `scan` (secret gate); `agit-hub` + `hub-ui` (exposure/view — retarget rendering, not rebuild).
- `llm.rs` incl. the codex backend → `sync`'s dialogue engine.

**Add** (net-new):

- The `sync` **dialogue engine** — the A↔B loop over divergent tails, conflict surfacing, → resumable merged state.
- `resume` / `compare` / `graph` verbs (mostly wiring over existing data).
- **If two-layer:** the understanding layer — schema + derivation prompt + storage + semantic diff.

**Sizing:** removing `reconcile` and renaming to the primitive set is *small and safe*. Whether the whole effort is
small or medium-large is decided entirely by §6 — pure-transcript keeps it small; the understanding layer is a slim,
derived rebuild of the ~2,500 lines removed on 07-13.

## 9. Decided

- Drop `reconcile`-as-`CLAUDE.md`-distillation; the merge output is a **resumable state**.
- No compatibility burden — clean primitive set.
- `resume` is the universal loader (takeover / cross-runtime / cross-env are bindings).
- Naming principle: git terms stay bound to git; agent-semantic ops get new names.
- `sync` interaction model: watch, interrupt only at real conflicts.

## Open decisions

1. **Agent State: pure-transcript vs transcript+understanding.** The cost driver (§6). *Biggest open item.*
2. **Name for the smart-merge op** — `sync` / `reconcile` / `converge` / `integrate` (§4).
3. **`sync` directionality** — update only the current branch, both branches, or a fresh integration branch. (Started,
   not decided.)
4. **The dialogue engine** — one mediator model role-playing both sides from transcripts, vs reviving both as live
   sessions that actually exchange messages, vs asymmetric (your live agent interviews the other from its transcript).
   Affects fidelity, cost, latency.
5. **`snap` cadence** — fully automatic (version every meaningful step) vs explicit checkpoints vs hybrid.

## Appendix: history

The hand-authored **fact / evidence model was deleted 2026-07-13** in a five-commit sweep (`1fb7c92`…`4dab39d`). The
rip-out (`7e0611c`) removed eight modules, ~2,500 lines: `claim.rs` (403), `evidence.rs` (254), `summarize.rs` (239),
`facts.rs` (221), `merge.rs` (187), `validate.rs` (153), `extract.rs` (149), `align.rs` (98), + `tests/merge.rs` (102).
The "understanding" layer in §5 is a lighter, **LLM-derived** (not hand-authored) descendant of exactly those modules —
bringing back the structured-state *view* without the structured-state *maintenance burden* that got it deleted.
