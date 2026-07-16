# Host script — versioning an agent's raw sessions, and merging two agents by dialogue

> For each step: **what you type · what appears on screen · what capability it shows · what you can say.**
> Screen output is taken from a real run (the dialogue is model-driven, so wording varies run to run).
>
> One-liner: **We don't design facts or a schema — Claude already dumps the whole session to disk, so we
> version that dump and push/pull it. When two agents diverge, we revive both and let them reconcile by
> reading each other's code; only a real conflict stops to ask you.**

## Stage

```sh
./demo/showcase/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/showcase
```

`setup.sh` builds one repo with two diverged branches, each the work of a different agent:

- **feature-a** — agent A added a login rate limiter, `loginKey()`, that buckets requests on `user_id`.
- **feature-b** — agent B renamed the identity field `user_id` → `uid`.

Their changes are a **cross-cutting conflict**: B renamed the field A's new code still buckets on. The two
agents' sessions are in the Agent Store — `main` = agent A's, `bob` = agent B's (as if you fetched a teammate's).

---

# Act 1 · The raw session is versioned, and secrets can't leak

### ① The session is already captured — with `agit -a snap`
```sh
agit -a log --oneline
find .agit/agent/sessions -name '*.jsonl'
```
```text
28f9c6a agent A: feature-a session
.agit/agent/sessions/claude-code/alice-session.jsonl
```
> Say: Claude Code already dumps the entire session — transcript, subagents, tool results — to
> `~/.claude/projects/<project>/`. `agit -a snap` mirrors that dump into a second git repo, `.agit/agent`,
> that sits next to your code. **No facts, no evidence, no schema — just the raw session.** `.agit/` is
> gitignored, so your code repo is untouched.

### ② Dumping the whole session means secrets could ride along — so commits are scanned
```sh
echo '{"type":"user","message":{"content":"AKIAIOSFODNN7EXAMPLE"}}' \
  > .agit/agent/sessions/claude-code/LEAK.jsonl
agit -a add -A && agit -a commit -m leak
```
```text
Found suspected secrets:
  sessions/claude-code/LEAK.jsonl:1  [aws-access-key-id]  AKIA…******

1 of them. Once the AgentState is pushed, a teammate who pulls carries them along.
Fix it. Or use --no-verify to bypass this hook and explicitly own the consequences.
```
> Say: because we dump the whole session, any `.env` or connection string the agent `cat`ed is in the
> transcript. A pre-commit hook scans it and **blocks the commit** before it can be pushed to the team.
> The scan uses high-precision rules, so high-entropy noise like UUIDs or requestIds doesn't trip it —
> only real secrets do.
```sh
rm .agit/agent/sessions/claude-code/LEAK.jsonl && agit -a add -A
```

---

# Act 2 · Two agents diverged on two branches

```sh
git log --oneline feature-a feature-b
git diff feature-a feature-b -- src/auth.js
```
```text
feature-a: login rate limiter on user_id
feature-b: rename user_id -> uid
payments: base
```
> Say: A shared starting point, then two branches. Agent A's `loginKey()` reads `req.body.user_id`; agent B
> renamed that field to `uid`. Git will merge the two lines textually **with no conflict marker** — and ship a
> silent break. Text merge can't see it. A semantic merge has to.

---

# Act 3 · `sync` — the two agents reconcile by dialogue

```sh
agit -a sync bob
```
```text
Two worktrees: A@feature-a · B@feature-b (each carries its own diff since the common ancestor).
Reviving both sessions (read-only): A=c3f691ab (1 local) … B=899a4919 (1 incoming) …

A → Since the fork I added one line to src/auth.js: loginKey(req) returning req.body.user_id,
    used as a rate-limit bucket key. What did you change?

B → My only change is that field rename: getUser now returns req.body.uid, no loginKey anywhere.
    CONFLICT: I renamed user_id -> uid, so your loginKey returns req.body.user_id = undefined once
    callers send the new payload — every request buckets under the same undefined key. DONE

A → Confirmed against my tree: grep finds only src/auth.js:1-2, no other callers. Your read is exact —
    git auto-merges the two lines textually and ships the break silently. DONE

── sync result ──
Agreed:
- Only conflict: B renamed user_id → uid (src/auth.js:1); A's loginKey (src/auth.js:2) still reads user_id.
- Post-merge loginKey returns undefined for every request → all callers collapse into one bucket.
- Keep B's rename; A's loginKey moves to req.body.uid, in the same commit as the rename.

Transcript archived → .agit/agent/sessions/sync/c3f691ab-899a4919.md
Merged state (both contexts + the reconciliation) — resume it to continue:
  (cd /tmp/agit-demo/showcase && claude --resume c3f691ab-2e17-7e91-82a6-a2236d52340b)

Nothing left for you to decide.
```
> Say: this is the whole point. `sync` doesn't summarize — it **revives both real sessions**, read-only, each
> in its own branch's worktree, and hands each one its own diff since the common ancestor as ground truth.
> They **talk it out by reading the code**. Because each side reasons from its full context, the cross-cutting
> conflict surfaces — a one-shot summary merge would miss it. The output is a **resumable merged session**
> (not a `CLAUDE.md`): resume it and you continue with both agents' context reconciled. When a real conflict
> can't be settled, `sync` stops and asks you, and bakes your decision into that merged session.

```sh
agit -a log --oneline           # the merge transcript is versioned too
sed -n '1,20p' .agit/agent/sessions/sync/*.md
```
> Say: the dialogue itself is archived under `sessions/sync/` — versioned provenance for *how* the two agents
> aligned, not just the result.

---

# What this run demonstrates

| Capability | Where |
|---|---|
| **Minimally invasive**: version Claude's own session dump, no facts or schema | ① |
| Two repos: context and code kept separate, neither pollutes the other | ① |
| The Agent Store is just git: push / pull / clone come for free | Act 1–2 |
| **Secret gate**: dumping the whole session never leaks a secret (and isn't drowned by UUID noise) | ① |
| **Agent-driven semantic merge**: revive both, reconcile by reading code | Act 3 |
| **Only real conflicts ask you**: everything reconcilable is reconciled | Act 3 |
| **Resumable merged state**: resume the session and continue with both contexts | Act 3 |
| Swap the LLM backend (Codex is first-class): `AGIT_LLM=codex`, or `AGIT_LLM_CMD=…` | — |

**Tear down:** `rm -rf /tmp/agit-demo`

---

> Rehearse before you present (runs non-interactively; needs `claude` on PATH):
> ```sh
> ./demo/showcase/rehearse.sh
> ```
