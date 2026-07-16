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
| **Agent Store** | A separate git repo holding the session dump | `.agit/agent` (gitignored) |
| **WorkspaceRevision** | The Agent↔Environment pairing | `.agit/workspace` (outside git, to avoid recursion) |

```
agit <git-args>     = transparent git acting on the Environment
agit -a <git-args>  = isomorphic git acting on the Agent Store
```
The scope switch only recognizes the first token immediately after `agit`
(`agit -a commit` vs `agit commit -a`).

## What lives in the Agent Store

```
.agit/agent/
  agent.toml                      Agent identity
  sessions/<runtime>/
    <id>.jsonl                    Full transcript
    <id>/subagents,tool-results   Sub-agents, large tool results
```
`agit -a snap` mirrors the runtime's session-dump directory **wholesale** into the store.
No `state/`, no facts. (This mirror command was formerly named `sync`; it is now `snap`.)

## Data flow

```
Claude session ──dump──> ~/.claude/projects/<slug>/
                          │  agit -a snap (mirror)
                          ▼
                   .agit/agent/sessions/<rt>/     ──push/pull (git)──> team / Hub
                          │  agit -a sync <ref>
                          ▼
        both agents revive read-only, converse over the divergent tail
                          ▼
              resumable merged session  +  a list of real conflicts
```

The merged session is a real session, versioned like any other. You continue from it by
resuming it in your runtime — `agit -a sync` prints the resume command (e.g.
`claude --resume <id>` / `codex exec resume <id>`). Nothing is distilled into a `CLAUDE.md`.

## Three layers — mind which is deterministic and which is not

| Layer | Deterministic? | Who does it |
|---|---|---|
| **Storage / sync** (`snap`, `commit`, `push`/`pull`) | ✅ deterministic | git |
| **File-level merge** (sessions with distinct uuids land side by side) | ✅ deterministic | git (no text conflicts) |
| **Semantic merge / conflict detection** (`sync`) | ❌ **non-deterministic** | live agents in dialogue + `src/llm.rs` orchestration |

**Key design: the raw session is the single source of truth (git versions it
deterministically); a merge produces another resumable session, not a summary file.**
Isolating the non-determinism in the top layer — and making its output a resumable,
re-runnable session rather than a hand-maintained artifact — is the main lever for
controlling risk (see [risk analysis](风险分析.md)).

`agit -a sync <ref>` revives **both** agents (each side's latest session) read-only, each in
its own branch's git worktree with its own diff since the merge-base. The two agents converse
over the divergent tail, resolving what they can by reading the code, and surface only the
real conflicts — letting you decide each one interactively. Both claude-code and codex are
supported (`--from codex` to revive the codex side; `--both` to write the merged session on
both branches). Because the dialogue is driven by live runtime sessions, `sync` needs the
relevant runtime CLI available.

## Pluggable LLM backend

`src/llm.rs`: defaults to `claude -p`; `AGIT_LLM=codex` shells out to the local `codex exec`
(**implemented**, `-o` for the clean reply); `AGIT_LLM_CMD` accepts any stdin→stdout CLI.
The model-backed steps of `sync` — the orchestrator/facilitator prompts and the synthesis of
the RESOLVED / OPEN conflict summary — go through this backend. (The dialogue turns
themselves are real resumed agent sessions, driven separately.)

## Secrets

Dumping the whole session means the transcript may contain secrets the agent has seen. The
commit/push hooks scan the session: for jsonl they use **high-precision rules only**
(AWS keys, connection strings, private keys, `password=`…) and **turn off generic entropy
detection** — otherwise the sea of UUIDs / requestIds in a transcript would flood you with
false positives. This does not catch general sensitive content (see risk analysis §8).

## Modules

| Module | Responsibility |
|---|---|
| `scope` | Dual-repo discovery, scope routing |
| `passthrough` | Transparent git passthrough (spawn, inherit stdio, propagate exit code, post-hook pairing) |
| `gitx` | Small git plumbing helpers shared across modules |
| `session` | `snap` — mirror the runtime's session dump into the Agent Store |
| `sync` | Dialogue-based agent merge (revive both sides read-only in per-branch worktrees, relay the conversation, surface + resolve real conflicts, leave a resumable merged session); claude-code + codex |
| `adapter` | Session parsing (`export` → `SessionIR`); Claude and Codex **both implemented** |
| `convo` / `register` | Lossless `ConversationIR` round-trip + install into a target runtime for resume (`agit convert`) |
| `llm` | Pluggable LLM CLI backend (claude / codex / any command); drives `sync`'s orchestration + synthesis |
| `scan` | Secret scanning (session mode) |
| `environment` / `workspace` | `EnvironmentState` capture / `WorkspaceRevision` pairing (real edges + `restore`) |
| `commands` | scan / workspace (incl. restore) / clone / convert / adapter |
| `init` | Create the Agent Store + hooks |
| `src/bin/agit-hub.rs` + `hub-ui/` | Hub: hosting + git smart-http + token auth + JSON API + embedded React SPA |

## Explicitly out of scope

- Hand-authored facts / an evidence schema / deterministic evidence merge (deleted).
- Running agents or merging on the Hub (merging happens only on the consumer's machine, to
  avoid cost + prompt injection).
- Exact replay / KV-cache reuse / process snapshots (that is Shepherd's territory, see
  competitive-analysis).

## Roadmap (design, not all shipped)

The design direction is a small primitive set — `snap` / `resume` / `sync` /
`push`-`pull`-`clone` / `graph` — operating on Workspace State (Agent State + Environment
State + Relations). `snap` and `sync` are shipped; `resume` (a universal loader over
`convert`/`register`) and `graph` (the Workspace-State DAG) are still being designed. See
[`plans/2026-07-16-workspace-state-primitives-design.md`](plans/2026-07-16-workspace-state-primitives-design.md).
