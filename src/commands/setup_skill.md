---
name: agit
description: "Use AgentGit to adopt native conversations, select explicit session targets, save completed turns, inspect or resume saved context, and publish session history. Read command references only when needed."
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
session files       = ordinary files in a branch worktree, including artifacts/
```

Do not confuse the project's `.git` with `~/.agit/repos/...`. Only `--code` also touches the project code repository; ordinary `agit commit` records the AgentGit conversation repository. Each directory has at most one persisted `bound repo`. Multiple session links may share the same cwd and belong to different repos. Every existing-session operation selects its target through explicit arguments or `AGIT_SESSION`; directory state and native runtime IDs never select a session.

## Working from an agent or script

- Start with `agit status --json` in the intended project directory. Existing
  sessions require an explicit `<owner/repo>@<branch>` or `AGIT_SESSION`.
  Workspace bindings and discovered runtime sessions do not supply that target.
- `@` means the session supplied through `AGIT_SESSION`. A known conflicting
  native runtime claim makes that environment stale; use the explicit target
  reported by the runtime hook or correct the environment before continuing.
- Use `--json` for a stable envelope: check `ok` and `exit_code`, then inspect
  `result.format`. Structured data is in `result.value`; text-only commands
  expose `result.lines`. Errors and hints are in `diagnostics.stderr`.
- Machine output disables terminal pickers. Supply required targets and options;
  runtime-launching commands require `--no-launch` with `--json`. `--yes` only
  answers confirmation prompts and never selects a session identity.
- Search prior work with `agit search "question" --repo owner/name --json`.
  Repeat `--query` to batch related searches with shared filters. Read
  `references/commands/search.md` for pagination, limits, and uncertainty fields.
- Read only the command reference needed for the operation; use
  `agit <command> --help` to check this installed build's exact options.

## Choose an Agent repo before starting a session

Before creating or importing a session, run this in the target workspace:

```bash
agit status
```

Follow these rules in order. Never guess from a directory name, the first repo in a list, or the repo used last time:

1. A persisted `bound repo` records the workspace’s intended repo for session creation. Name that repo explicitly in `new` or `import`; the binding never selects an existing session branch. Do not run `agit init` merely because this is a new session.
2. Adopted session links are discovery results, not default targets. If no destination repo has been selected, ask the user to choose or use an interactive picker; never infer it from the sole link or a native runtime ID.
3. If no suitable Agent repo exists, run `agit init` first, then create the session branch.
4. If the user has named an existing Agent repo, always reuse it; this takes precedence over directory state and session-link ambiguity. If it is not local, run `agit clone <owner/repo>` first, not `agit init`.
5. Organization repos (`<org>/<name>`) accept `import`, `commit` and `push` from whoever the Hub lets push to that repo (the org owner, and team members granted on it); an org owner may also import into a repo that does not exist yet — the first push creates it under the org. The CLI asks the Hub before importing or pushing, so a refusal names the real reason. The checkout lives under `~/.agit/repos/<org>/<name>`, and versions are authored by the signed-in account. Organization repos support public and private visibility; a first publish without a visibility preference or interactive confirmation defaults to private. Write owner names in lowercase. Do not `clone --mine` a copy just because the owner is not the user.

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

This reports the resolved identity, if any, and scans the runtime directories for sessions that no Agent repo has adopted yet. The current transcript may already be adopted even when `AGIT_SESSION` is absent. Match its native session ID from the runtime or hook against the reported metadata; never choose by recency alone. When the user asks to upload, save, or adopt the current session, explicitly pass its native session ID (or choose it in the interactive import picker) and adopt it into the repo chosen by the rules above:

```bash
agit import <session-id> --from <runtime> --repo <owner/repo> -b <branch>
```

`agit import` links that existing transcript to a real session branch and records its first version after an explicit lineage choice. Use the full native ID and `--from` runtime. A terminal offers verified bases, independent import, or cancellation; pipes, JSON, CI and agent calls return choices without adopting. Pass `--onto <ref>` or `--independent` to make the decision explicitly in automation. `--propose-lineage` only inspects local evidence. `agit new` cannot take over the session that is already running: it launches a different session with an empty VIEW. Use `new` only when the user explicitly asks to start a fresh session. `-n <agent-name>` is only for naming a new Agent repo when none can be reused; it does not pick the branch.

Import cannot change the calling process's environment. Keep using the selected
`<owner/repo>@<branch>` explicitly for later commands, or supply that exact
`AGIT_SESSION` to each invocation. To inspect saved context without preparing a
runtime, use `show` or `view`. `resume --no-launch --json` prepares a runtime
session and records its claim; it is not a read-only preview.

## Pick the command

| Goal | Command | Result |
|---|---|---|
| Create the first Agent repo | `agit init <name>` | Creates the local repo and `main`, optionally binding the directory |
| Start an empty session in an existing repo | `agit new <owner/repo> -b <branch>` | Creates a real session branch and starts a runtime |
| Import an existing Codex/Claude conversation | `agit import <runtime-id> --from <runtime> --repo <owner/repo> -b <branch>` | Chooses lineage, then adopts and settles the transcript |
| Open a line from an old point | `agit fork <source> -b <branch>` | Creates a branch; add `--resume` to start it |
| Continue an existing session | `agit resume <owner/repo>@<branch>` | Restores that session's VIEW and starts it; never forks |
| Open a branch or saved point | `agit open <owner/repo>@<ref>` | Continues a writable branch head; forks other saved points (`run` is an alias) |
| Save completed turns | `agit commit <owner/repo>@<branch>` | Records completed pending turns; an in-progress turn waits for settlement after it ends |
| Edit shared files on the file line (README.md, AGENTS.md, memory/, skills/) | `agit commit <owner/repo>@main -m "<msg>" [-- <path>...]` | Pure file commit on `main`; needs no session; publish with `agit push <owner/repo> -b main` |
| Publish local history | `agit push <owner/repo>@<branch>` | Scans secrets, then publishes existing refs |

## Deliver files at completed milestones

When a milestone is ready, select the deliverables you intend to present to the
user: documents, PDFs, slides, images, videos, or HTML. Put them in `artifacts/`,
beside `memory/` and `skills/`, and explicitly stage and commit them:

```bash
agit file cwd
agit file add /absolute/path/report.pdf
agit file diff --staged
agit file commit -m "Deliver the report"
agit file link artifacts/report.pdf
```

`cwd` prints the selected branch's real file directory, where you can read and
edit files directly. External inputs default to `artifacts/<name>`; `--to` chooses
another relative path. Use `agit file --into owner/repo@branch` when `AGIT_SESSION`
is absent. Read `references/commands/file.md` for the complete command set.

Commit only material useful to the user; exclude scratch files, caches and build
dependencies. Stage the exact version to deliver. A file commit consumes only
that branch's index and never settles the conversation. Turn settlement and
automatic memory collection preserve manually staged files. Memory continues to
be collected automatically; other files require an explicit file commit.

Present the commit-and-path permalink returned by `agit file link`. Publish the
branch with `agit push` when the user has asked to share it or the workflow already
authorizes publication; a local file commit alone does not upload anything.
Keep large media local until the destination supports LFS. Do not commit an LFS
pointer without uploading its object to a compatible server. For slides, include
a PDF when browser preview is needed. HTML preview is a static document; use
inline styles and embedded images, without scripts or external assets.

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
- Use `agit commit <owner/repo>@main -m "..."` to select the file line explicitly. The shorter `agit commit main -m "..."` requires `AGIT_SESSION` to supply the repo; a workspace binding does not select it.
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
| `open` | Open a branch or saved point, continuing a writable head or forking; `run` is a compatibility alias |
| `resume` | Strictly continue an existing writable session branch |

### Adoption, context, and sessions

| Command | Meaning |
|---|---|
| `import` | Adopt an existing runtime transcript into a repo/branch |
| `status` | Show identity, adopted sessions, bindings, and sync state |
| `branch` | List, rename, remove, or seal existing branches; it does not create them |

### Recording and inspection

| Command | Meaning |
|---|---|
| `commit` | Record turn or shared-file changes in the Agent repo; `--code` also commits the code repo |
| `file` | Read, stage and explicitly commit ordinary branch files, and generate immutable Hub links |
| `memory` | Memory between the runtime directory, this session branch and `main`: `status` / `diff` / `distill` / `sync` |
| `distill` | Promote selected memory files from a session branch into the shared `main` file line |
| `tag` | Name a ref with a version tag |
| `log` | Show turn/merge/view/file history |
| `show` | Render a VIEW; an omitted target or `@` requires `AGIT_SESSION` and selects that exact branch |
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

Ordinary commands select an existing session from explicit arguments or `AGIT_SESSION`:

```text
1. Explicit command arguments
2. AGIT_SESSION=<owner>/<repo>@<branch>
3. Otherwise refuse and request an explicit target
```

`@` refers only to `AGIT_SESSION`. A registered native runtime claim may reveal that this environment value is stale after a runtime session switch; agit then refuses it and asks for an explicit target or a corrected environment value. It never chooses the runtime’s branch automatically. Hook payloads name their session explicitly and do not let an inherited environment value override that identity. A legacy link without a recorded owner cannot settle through hooks; run `agit import <session-id> --from <runtime> --into <owner>/<repo>@<branch>` to record the complete claim.

Workspace bindings remain descriptive repo routes for creation and status. Native session links, cwd matches, the current checkout and the newest transcript cannot fill in a missing session target. Human terminal pickers collect an explicit user choice for the current command. `agit`, without a subcommand, opens the resume picker; `log`, `push`, and `share` can also select a session without saving a directory-wide default.

`new` and `import` create/adopt identity and should name the destination repo explicitly. `branch --repo <owner/repo>` manages a repo without selecting a current session.

## Session rules

- `AGIT_SESSION=<owner/repo@branch>` explicitly selects the process’s session identity. A known conflicting native claim makes it stale and is refused. `@` uses this supplied identity: `agit log @`, `agit show @#3`, `agit commit @`.
- One user turn normally becomes one AgentGit commit. When a phase genuinely completes (working feature and passing tests), settle it:

  ```bash
  agit commit <owner/repo>@<branch> --milestone "short phase summary" --tag ms-short --code
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

Claude hooks may run `agit hooks settle` at Stop (older installs wrote `agit commit --from-hook`; `agit setup` retires it). In `AGIT_RC=1` supervisor mode, `agitd` may settle and push at turn boundaries; that is integration behavior, not a general CLI guarantee. When offline, the local Agent repo remains authoritative. Automatic publication is opt-in: `agit config --global push.auto true` enables it for repositories without overrides, while `agit config --repo <owner/repo> push.auto false` keeps one repository local. `init`, `clone`, and full `setup` offer this choice. Successful session settlement then pushes only that branch through the normal access and secret gates; upload failure never discards the local commit.

## Skill installation layout

`agit setup --skill` installs the same progressive-disclosure bundle for every
supported runtime. Each target is a real Skill directory containing one
entrypoint and a reference for every top-level command:

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
