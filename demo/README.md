# agit demo

One end-to-end showcase: **[`showcase/`](showcase/)** — version an agent's raw sessions directly, push/pull
them like any git repo, and when two agents diverge, revive both and let them reconcile by reading each
other's code. Only a real conflict stops to ask you.

```sh
./demo/showcase/setup.sh                 # stage: one repo, two diverged agent branches + both agents' sessions
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/showcase
# then follow demo/showcase/host-script.md act by act

./demo/showcase/rehearse.sh              # dry-run before you present (needs a local `claude`)
```

Three acts:
- **Raw session versioned + secret gate** — `agit -a snap` mirrors Claude's session dump; the pre-commit
  hook blocks a commit that carries a secret.
- **Two diverged branches** — `feature-a` added a rate limiter keyed on `user_id`; `feature-b` renamed
  `user_id → uid`. A cross-cutting conflict a text merge can't see.
- **sync** — `agit -a sync bob` revives both agents (read-only, each in its own branch's worktree), lets them
  reconcile by dialogue, and leaves a **resumable merged session**. Only the real conflict (`user_id` vs
  `uid`) surfaces.

**[`host-script.md`](showcase/host-script.md)** is the presenter's version: each step with its screen output,
the capability it shows, and a line you can say.

---

## The model

- **No facts, no schema.** Claude Code already dumps the entire session to
  `~/.claude/projects/<project>/` (transcript jsonl + subagents + tool results); `agit -a snap` mirrors that
  dump into the Agent Store.
- **Two repos:** your code repo (Environment, untouched) + `.agit/agent` (Agent Store — an independent git
  repo holding the sessions).
- **Collaboration is git:** the Agent Store is just a git repo, so push / pull / clone come for free.
- **The merge is agent-driven:** `agit -a sync <ref>` revives both agents and lets them reconcile by reading
  code; only real conflicts ask you. (Model-driven, so it's non-deterministic — by design.)
- **Secret gate:** dumping the whole session means a transcript can contain secrets, so commit and push scan
  for them (session scan uses high-precision rules, so it isn't drowned by UUID-style noise).

## Layout

| | Purpose |
|---|---|
| `showcase/` | the end-to-end demo (`setup.sh` · `host-script.md` · `rehearse.sh`) |
| `lib.sh` · `state.sh` | shared helper · `agit-state` (show the current state of both repos) |

For the full command reference, see [`docs/使用说明.md`](../docs/使用说明.md).
