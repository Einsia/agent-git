# agit architecture (session model)

> The current architecture. The earlier "two repos + hand-authored facts + deterministic evidence merge" design (architecture-v2) has been retired.

## In one sentence

**The versioned object is the agent's raw session.** Claude Code already dumps the whole
conversation to `~/.claude/projects/<slug>/`; agit versions that dump and syncs it with
push/pull. Merging is delegated to the agents themselves — they revive, read the sessions
and the code, and reconcile; only genuine conflicts are put to you. **No facts, no schema,
minimal footprint.**

## Two git repos + one pairing

| | What it is | Location |
|---|---|---|
| **Environment** | Your code repository, untouched | The project root's `.git` |
| **Agent Store** | A separate git repo holding the session dump | `$AGIT_HOME/agents/<aid>/` — found by **identity**, never by path |
| **Binding** | Which agents this repo works with | `.agit.toml` at the code-repo root — **committed** |
| **WorkspaceRevision** | The Agent↔Environment pairing | `.agit/workspace` (outside git, to avoid recursion) |

```
agit <git-args>     = transparent git acting on the Environment
agit a <git-args>   = isomorphic git acting on the RESOLVED agent's store
```
`a` is a subcommand, so it cannot be transposed: `agit a commit` (the store) vs `agit commit -a`
(your code, where `-a` is git's stage-all). The old `agit -a <args>` flag survives as a deprecated
alias.

## Identity — why the store is not a location

**An agent is a memory**, named for what it knows (`frontend`), not for a person or a repo. An agent
works in many repos; a repo hosts many agents. That many-to-many is why the store cannot live inside
the code repo: the old `<env>/.agit/agent` welded one memory to one environment, which is precisely
what made "continue this agent in the backend repo" impossible.

| Layer | Value | Travels? |
|---|---|---|
| **Identity** | `aid` = `agt_<uuid>`, minted once | committed **inside the store**, in `agent.toml` |
| **Local store** | `$AGIT_HOME/agents/<aid>/` | — |
| **Name** | a mutable label | a hint in `.agit.toml` |
| **Remote** | a git URL — a *locator* | in `.agit.toml` |
| **Registry** | `$AGIT_HOME/registry.json`: name→aid, a **rebuildable cache** | local |

Not the URL (you mint an agent before any remote exists; a hub migration rewrites every URL). Not the
name (names collide; renaming is labelling). **Keyed by the aid** ⇒ rename and publish are pure
metadata edits, no directory ever moves, and a running watcher is never orphaned.

**Resolution**, for every agent-scoped command: `--agent` → `$AGIT_AGENT` → the active pointer
(per-worktree, local) → `.agit.toml [defaults]` → an actionable error. **Never a silent fallback.**

> The rule that keeps a local file honest: *a gitignored local file is legitimate iff its absence is
> fully recoverable from committed state.* The active pointer passes (delete it, fall back to
> `[defaults]`). The old `.agit/store` pointer failed it — an absolute path that was the only
> resolver, so deleting it left nothing able to say where the store had gone. It is deleted, along
> with `$AGIT_AGENT_DIR` and `agit init --store`.

## What lives in the Agent Store

```
$AGIT_HOME/agents/<aid>/
  agent.toml                      Agent identity (committed — so the aid travels with the history)
  sessions/<runtime>/
    <id>.jsonl                    Full transcript
    <id>/subagents,tool-results   Sub-agents, large tool results
  sessions/sync/                  Dialogue transcripts from `agit a merge`
```
`agit snap` mirrors the runtime's session-dump directory **wholesale** into the store of the agent
that session belongs to. No `state/`, no facts.

**Attribution is by launch record, not by the active pointer.** Both runtimes dump per *project
directory*, so two agents working one repo write to the **same folder**; a pointer cannot tell their
sessions apart and would misfile them silently, into the wrong agent and then the wrong team.
`agit start` knows whose session it launched, and writes `session-id → {aid, env, runtime, started}`
before exec'ing the runtime. Sessions started by plain `claude` have no record and are attributed to
the repo's **default** agent — and reported as such.

## Data flow

```
runtime session ──dump──> ~/.claude/projects/<slug>/  ·  ~/.codex/sessions/
                          │  agit snap (mirror, routed by launch record)
                          ▼
        $AGIT_HOME/agents/<aid>/sessions/<rt>/    ──push/pull (git)──> team / Hub
                          │  agit a merge <agent|ref>
                          ▼
        both agents revive read-only, converse over the divergent tail
                          ▼
              resumable merged session  +  a list of real conflicts
```

The merged session is a real session, versioned like any other. You continue from it by
resuming it in your runtime — `agit a merge` prints the resume command (e.g.
`claude --resume <id>` / `codex exec resume <id>`). Nothing is distilled into a `CLAUDE.md`.

**The merge mode is decided by identity, not by git history.** `agent.toml` is committed inside the
store, so the aid is readable at any target. Same aid (another copy of my agent, e.g. a teammate's
push) → dialogue, then **fuse**: the histories merge and it is one memory again. Different aid (a
different agent) → dialogue only, and **both memories stay intact**. Deciding on the aid — rather than
on whether a merge-base happens to exist — is what removes the guess.

## Three layers — mind which is deterministic and which is not

| Layer | Deterministic? | Who does it |
|---|---|---|
| **Storage / sync** (`snap`, `commit`, `push`/`pull`) | ✅ deterministic | git |
| **File-level merge** (sessions with distinct uuids land side by side) | ✅ deterministic | git (no text conflicts) |
| **Semantic merge / conflict detection** (`a merge`) | ❌ **non-deterministic** | live agents in dialogue + `src/llm.rs` orchestration |

**Key design: the raw session is the single source of truth (git versions it
deterministically); a merge produces another resumable session, not a summary file.**
Isolating the non-determinism in the top layer — and making its output a resumable,
re-runnable session rather than a hand-maintained artifact — is the main lever for
controlling risk (see [risk analysis](风险分析.md)).

`agit a merge <target>` revives **both** agents (each side's latest session) read-only, each in
its own git worktree with its own diff since the common ancestor. The two agents converse
over the divergent tail, resolving what they can by reading the code, and surface only the
real conflicts — letting you decide each one interactively. The target is **another memory**: an
agent name or a ref, never a code branch. Both claude-code and codex are supported (`--from codex`
to revive the codex side; `--both` to write the merged session on both branches). Because the
dialogue is driven by live runtime sessions, `merge` needs the relevant runtime CLI available.

## Pluggable LLM backend

`src/llm.rs`: defaults to `claude -p`; `AGIT_LLM=codex` shells out to the local `codex exec`
(**implemented**, `-o` for the clean reply); `AGIT_LLM_CMD` accepts any stdin→stdout CLI.
The model-backed steps of `a merge` — the orchestrator/facilitator prompts and the synthesis of
the RESOLVED / OPEN conflict summary — go through this backend. (The dialogue turns
themselves are real resumed agent sessions, driven separately.)

## Secrets

Dumping the whole session means the transcript may contain secrets the agent has seen. The
commit/push hooks are installed **when the store is created** — whichever door it came through
(`a new`, `a track`, `a import`, `init`), because a store that skipped them scans nothing, silently.
They scan the session: for jsonl they use **high-precision rules only**
(AWS keys, connection strings, private keys, `password=`…) and **turn off generic entropy
detection** — otherwise the sea of UUIDs / requestIds in a transcript would flood you with
false positives. This does not catch general sensitive content (see risk analysis §8).

## Modules

| Module | Responsibility |
|---|---|
| `scope` | Dual-repo discovery, scope routing, `$AGIT_HOME` |
| `agent` | Identity (aid), the registry cache, the `.agit.toml` binding, resolution, and the agent verbs (`new`/`track`/`use`/`info`/`rename`/`import`) |
| `passthrough` | Transparent git passthrough (spawn, inherit stdio, propagate exit code, post-hook pairing) |
| `session` | `snap` — mirror the runtime's session dump into the store of the agent that owns it; `watch`; runtime resolution (no default) |
| `sync` | Dialogue-based agent merge (revive both sides read-only in per-branch worktrees, relay the conversation, surface + resolve real conflicts, leave a resumable merged session); claude-code + codex |
| `adapter` | Session parsing (`export` → `SessionIR`); Claude and Codex **both implemented** |
| `convo` / `register` | Lossless `ConversationIR` round-trip + install into a target runtime for resume (`agit convert`) |
| `llm` | Pluggable LLM CLI backend (claude / codex / any command); drives `sync`'s orchestration + synthesis |
| `scan` | Secret scanning (session mode) |
| `environment` / `workspace` | `EnvironmentState` capture / `WorkspaceRevision` pairing (real edges + `restore`) |
| `commands` | scan / workspace (incl. restore) / convert / adapter / the launch record |
| `init` | Make a repo ready: gitignore, ensure an agent resolves, install the secret hooks |
| `src/bin/agit-hub.rs` + `hub-ui/` | Hub: hosting + git smart-http + token auth + JSON API + embedded React SPA |

## Explicitly out of scope

- Hand-authored facts / an evidence schema / deterministic evidence merge (deleted).
- Running agents or merging on the Hub (merging happens only on the consumer's machine, to
  avoid cost + prompt injection).
- Exact replay / KV-cache reuse / process snapshots (that is Shepherd's territory, see
  competitive-analysis).

## Roadmap (design, not all shipped)

The design direction is a small primitive set — `snap` / `resume` / `merge` /
`push`-`pull`-`clone` / `graph` — operating on Workspace State (Agent State + Environment
State + Relations). `snap`, `merge`, `resume`, `start` and `watch` are shipped.

The identity model above is specified in
[`plans/2026-07-17-agent-identity-and-handoff-design.md`](plans/2026-07-17-agent-identity-and-handoff-design.md),
**the design of record**, which supersedes the branch model in
[`plans/2026-07-16-workspace-state-primitives-design.md`](plans/2026-07-16-workspace-state-primitives-design.md).
The full identity model ships today: `agit a publish` / `a rebind`, and the per-environment session
partition (`sessions/<env-slug>/<rt>/`) that `snap` now writes — flat `sessions/<rt>/` stores from before
the partition still resolve.
