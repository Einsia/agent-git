---
name: agit
description: The English AgentGit guide: understand Agent repos, session branches, workspaces, and code repos; adopt the current session with import when AGIT_SESSION is absent; choose the right init/new/import/fork/resume/commit/push command and follow AgentGit history and collaboration rules.
---

# AgentGit overview

AgentGit (`agit`) is a version-control layer for agent conversations. It is not a replacement for the project code repository: it stores conversation context, VIEWs, events, shared memory, and skills in a real Git repository.

The core model:

```text
project workspace    = where the agent reads and edits code
code Git repo        = the project's own .git
Agent repo           = ~/.agit/repos/<owner>/<name>, storing conversation history
workspace binding    = a directory's persistent route to one Agent repo
session branch       = a branch in an Agent repo reserved by one session
main file line       = AGENTS.md / memory/ / skills/ shared across sessions
```

Do not confuse the project's `.git` with `~/.agit/repos/...`. Only `--code` also touches the project code repository; ordinary `agit commit` records the AgentGit conversation repository. Each directory has at most one persisted `bound repo`. Multiple session links may share the same cwd and belong to different repos, but they are not multiple workspace bindings; when there is more than one, select explicitly with a ref or `AGIT_SESSION`. `pinned` only identifies the branch currently preferred by that directory.

## Choose an Agent repo before starting a session

Before creating or importing a session, run this in the target workspace:

```bash
agit status
```

Follow these rules in order. Never guess from a directory name, the first repo in a list, or the repo used last time:

1. If and only if there is exactly one `bound repo`, default to creating the new session branch in that repo. Do not run `agit init` merely because this is a new session.
2. If there is no `bound repo` but exactly one adopted session link for this cwd, it may identify the reusable repo. If there are multiple session links, stop and ask the user to choose, or require `AGIT_SESSION`/an explicit `<owner/repo>@<branch>`; never take the first automatically.
3. If there is no reusable Agent repo or uniquely selected session, run `agit init` first, then create the session branch.
4. If the user has named an existing Agent repo, always reuse it; this takes precedence over directory state and session-link ambiguity. If it is not local, run `agit clone <owner/repo>` first, not `agit init`.
5. Organization repos (`<org>/<name>`) accept `import`, `commit` and `push` from whoever the Hub lets push to that repo (the org owner, and team members granted on it); an org owner may also import into a repo that does not exist yet — the first push creates it under the org. The CLI asks the Hub before importing or pushing, so a refusal names the real reason. The checkout lives under `~/.agit/repos/<org>/<name>`, versions are authored by the signed-in account, and org repos are always public on the Hub. Write owner names in lowercase. Do not `clone --mine` a copy just because the owner is not the user.

Reusing a repo normally means creating another session branch in that same Agent repo. A workspace binding only routes a directory to a repo; it does not create a branch. After creating or importing a session, verify the real Git ref:

```bash
git -C "$(agit repo path <owner/repo>)" \
  show-ref --verify "refs/heads/<branch>"
```

Name constraint: a **branch** name must not begin with `agit-`. That prefix is reserved for AgentGit version IDs (for example, `agit-<40-hex>`), because `owner/repo@<ref>` parsing uses it to distinguish a version ID from a branch name. Repo names are not subject to this rule (`hachi/agit-dev` is fine); `new` / `fork` / `import -b` reject such branch names locally before anything is created.

## Adopt the current session when it is not managed yet

If `AGIT_SESSION` is absent, do not assume the current transcript is already managed, and do not use `agit new` to upload it. First find the session:

```bash
agit status --check-missing
```

This reports the resolved identity, if any, and scans the runtime directories for sessions that no Agent repo has adopted yet; the transcript you are running in is one of them. When the user asks to upload, save, or adopt the current session, select its ID (or pass `@`, which means the current runtime session) and adopt it into the repo chosen by the rules above:

```bash
agit import <session-id> --repo <owner/repo> -b <branch>
```

`agit import` links that existing transcript to a real session branch and records its first version. `agit new` cannot take over the session that is already running: it launches a different session with an empty VIEW. Use `new` only when the user explicitly asks to start a fresh session. `-n <agent-name>` is only for naming a new Agent repo when none can be reused; it does not pick the branch.

## Pick the command

| Goal | Command | Result |
|---|---|---|
| Create the first Agent repo | `agit init <name>` | Creates the local repo and `main`, optionally binding the directory |
| Start an empty session in an existing repo | `agit new <owner/repo> -b <branch>` | Creates a real session branch and starts a runtime |
| Import an existing Codex/Claude conversation | `agit import <runtime-id> --repo <owner/repo> -b <branch>` | Adopts the transcript and settles it immediately |
| Open a line from an old point | `agit fork <source> -b <branch>` | Creates a branch; add `--resume` to start it |
| Continue an existing session | `agit resume <branch>` or `agit resume @` | Restores that session's VIEW and starts it |
| Run a frozen ref | `agit run <owner/repo>@<ref>` | Automatically chooses resume or fork |
| Save the current turn | `agit commit` | Records new content on the current session branch |
| Edit shared files on the file line (README.md, AGENTS.md, memory/, skills/) | `agit commit <owner/repo>@main -m "<msg>" [-- <path>...]` | Pure file commit on `main`; needs no session; publish with `agit push <owner/repo> -b main` |
| Publish local history | `agit push <owner/repo> -b <branch>` | Scans secrets, then publishes existing refs |

## Shared files on the file line

`main` is the file line: it never carries a session, and it is where README.md, AGENTS.md, `memory/` and `skills/` live. Everything `agit new` inherits and everything teammates see when they `agit clone` comes from here. Updating it is a file commit — no `git add`, no session link:

```bash
cd "$(agit repo path <owner/repo>)"                  # the Agent repo checkout: a plain Git worktree
$EDITOR README.md memory/decisions.md                  # edit or add shared files
agit commit <owner/repo>@main -m "docs: describe the repo"
agit push <owner/repo> -b main                         # publish the file line
```

- `-m` on the file line is always a pure file commit; `--milestone`, `--tag` and `--code` belong to turn commits.
- Without `-- <path>...` every change in the checkout is staged (`git add -A`); add `-- README.md` to limit the commit. AgentGit storage paths (`session/`, `LOG`, `VIEW`, `events/`) are excluded automatically.
- On a session branch `-m` is legal only while no new turns are pending; settle turns with `agit commit` first.
- In a directory bound to the repo, `agit commit main -m "..."` resolves the repo from context.
- To write README.md for a repo the user names (for example "add a README to hachi/agit-dev"), use exactly this flow; do not `git commit` inside `~/.agit/repos` by hand.

## Command groups

### Authentication and configuration

| Command | Meaning |
|---|---|
| `login` | Sign in to the Hub (interactive, device flow, or stdin PAT) |
| `logout` | Sign out and remove local credentials; store/repos remain |
| `whoami` | Show the current Hub identity; `--check` verifies online |
| `config` | Read/set/unset `hub.url`, default runtime, push visibility, and related settings |

### Repositories and runtime entry points

| Command | Meaning |
|---|---|
| `init` | Create a local Agent repo, its `main` line, and shared-file scaffold |
| `clone` | Fetch an existing Agent repo; read-only by default, `--mine` makes a copy in your namespace |
| `repo` | Manage repo create/list/info/visibility/collaborators/rename/delete/path |
| `new` | Create an empty session branch in a selected repo |
| `run` | Resolve any frozen ref, choosing resume or fork, and start a runtime |
| `resume` | Strictly continue an existing writable session branch |

### Adoption, context, and sessions

| Command | Meaning |
|---|---|
| `import` | Adopt an existing runtime transcript into a repo/branch |
| `status` | Show identity, adopted sessions, bindings, and sync state |
| `switch` | Pin or unpin the workspace's default branch; it does not create a branch |
| `branch` | List, rename, remove, or seal existing branches; it does not create them |

### Recording and inspection

| Command | Meaning |
|---|---|
| `commit` | Record turn or shared-file changes in the Agent repo; `--code` also commits the code repo |
| `memory` | Memory between the runtime directory, this session branch and `main`: `status` / `diff` / `distill` / `sync` |
| `distill` | Promote selected memory files from a session branch into the shared `main` file line |
| `tag` | Name a ref with a version tag |
| `log` | Show turn/merge/view/file history |
| `show` | Render a VIEW; no argument uses the AgentGit repo bound to the current directory, while `@` means current runtime context |
| `diff` | Compare turns, VIEWs, or shared-file content |
| `view` | Print the structured VIEW used by merge agents and tools |

### Forking and reconciling history

| Command | Meaning |
|---|---|
| `fork` | Create a new session branch from any ref; does not start by default |
| `merge` | Reconcile source and target by VIEW and intent; a summary is required before continue |
| `cherry-pick` | Add selected turns/events from another line without starting a merge agent |
| `revert` | Drop events from a VIEW while leaving the evidence log unchanged |

### Remote synchronization and collaboration

| Command | Meaning |
|---|---|
| `push` | Publish existing local refs; it never creates a branch |
| `fetch` | Fetch objects and remote refs; local branches do not move |
| `pull` | Fetch and fast-forward only; warns and skips divergence |
| `pr` | Create, inspect, fetch, and merge Hub pull requests |
| `share` | Create or revoke read-only sharing links |
| `search` | Search the AgentGit corpus visible to the current identity |
| `rc` | Pair and manage the `agitd` remote-control daemon and web connection |

### Export, integration, and diagnostics

| Command | Meaning |
|---|---|
| `export` | Export as JSONL, IR, Markdown, Claude Code, or Codex format |
| `scan` | Scan secrets or sensitive content before publishing/sharing |
| `secrets` | Register and review device-local secret protection rules |
| `setup` | Install hooks, the skill, MCP, AGENTS.md integration, and shell completion |
| `upgrade` | Check for or install a newer CLI |
| `doctor` | Check local integrity and optionally the backend connection |

### Hidden integration commands

| Command | Meaning |
|---|---|
| `hooks` | Hidden runtime-hook stdin entry installed by `setup` |
| `mcp` | Hidden stdio MCP server for MCP clients |

These two commands are normally not used interactively.

## Context resolution order

When a command must determine both repo and branch, use this order:

```text
1. Explicit command arguments
2. AGIT_SESSION
3. Runtime session link (~/.agit/store/<runtime>/<id>.json)
4. Workspace pin (agit switch)
5. The only adopted session in the current directory
6. If there is no unique answer, stop and require an explicit ref
```

Steps 2 and 3 arbitrate. `AGIT_SESSION` is injected once, when the runtime is launched, and it does not follow a session switch made inside the runtime's own interface (`/resume`, `/clear`). When the harness reports a session id whose link names a different branch, agit follows the link and prints a note saying which one it used. Treat step 2 as where the answer usually comes from, not as something that stays true after switching sessions.

The workspace lookup returns one persisted repo at most. A cwd can still have multiple adopted session links; those links are separate session metadata and require an explicit `<owner/repo>@<branch>` or `AGIT_SESSION` when they are ambiguous.

`new` and `import` create/adopt identity and should name the target repo explicitly. `commit`, `resume`, and `push` continue an existing identity and may use context resolution.

## Session rules

- `AGIT_SESSION=<owner/repo@branch>` is process identity and takes precedence over cwd — but not over the session that is actually running. Switching sessions inside the runtime leaves it stale; agit then follows the runtime's current session and says so. `@` means the current session branch: `agit log`, `agit show @#3`, `agit commit @`.
- One user turn normally becomes one AgentGit commit. When a phase genuinely completes (working feature and passing tests), settle it:

  ```bash
  agit commit --milestone "short phase summary" --tag ms-short --code
  ```

- Memory flows by itself between the runtime's memory directory and the session branch (materialized at `new`/`resume`, collected at every `agit commit`). `main` only moves when you distill: at a milestone run `agit memory status`, then `agit distill` (or `agit memory distill <file>…`) to carry the facts worth sharing into `main`; `commit --milestone` and `push` remind you when files are pending.
- Do not run two branches of the same Agent repo in parallel in one directory; identity follows the process, not directory guesses.
- With `AGIT_MERGE_TX=<owner/repo@target>`, act as the merge agent: inspect `agit view <source> --json`, drill into events with `agit show`, select with `merge pick/drop`, edit shared `memory/`, `skills/`, and `AGENTS.md`, write `agit merge summary -m "..."`, then `agit merge --continue`. Use `--abort` when irreconcilable.
- With `AGIT_RC=1`, a daemon supervises a shared workspace. Messages may come from other viewers/operators and approvals go to the workspace owner.
- Never use rebase, amend, force-push, or ordinary `git checkout` to rewrite AgentGit history.

## Recording and publishing

In ordinary CLI mode:

```text
agit commit = write to the local Agent repo
agit push   = separately publish existing local refs
```

Claude hooks may run `agit hooks settle` at Stop (older installs wrote `agit commit --from-hook`; `agit setup` retires it). In `AGIT_RC=1` supervisor mode, `agitd` may settle and push at turn boundaries; that is integration behavior, not a general CLI guarantee. When offline, the local Agent repo remains authoritative.

## Skill installation layout

`agit setup --skill` installs the same progressive-disclosure bundle for every
supported runtime. Each target is a real Skill directory containing one
entrypoint and one reference for every top-level command (41 references in the
current build):

```text
<runtime skill root>/agit/
├── SKILL.md
├── VERSION
└── references/commands/<command>.md
```

The global target directories are:

| Runtime | Directory |
|---|---|
| Claude Code | `~/.claude/skills/agit/` |
| Codex | `$CODEX_HOME/skills/agit/` (default `~/.codex/skills/agit/`) |
| OpenCode | `~/.config/opencode/skills/agit/` |
| Cursor | `~/.cursor/skills/agit/` |

The runtime should load `SKILL.md` first and read only the reference needed for
the current command or scenario. `--skill` no longer expands the full manual in
`AGENTS.md`; use the separate `agit setup --agents-md` option when a project
needs the short, marked session-integration block. Re-running setup replaces
only AgentGit-owned Skill files, removes stale `references/commands/*.md`, and
does not overwrite user content. It also removes a version-marked legacy inline
Skill block from older releases while preserving surrounding `AGENTS.md` text.

The English command references are embedded at build time from
`src/commands/subskills/*.md`; read the installed reference when exact
arguments, scenarios, or examples are needed.
