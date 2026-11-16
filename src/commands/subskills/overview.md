---
name: agit-overview
description: What bare agit shows - whether this folder is saved, its recent sessions, and the next step.
---

# agit (no command)

## Purpose

Run `agit` with no command to see, from local state only: the signed-in account, whether the
current folder is on and where it uploads, its recent sessions with their upload state (`not
saved`, `not uploaded yet`, `uploading`, `uploaded`), the last automatic upload's error, and
the next step. In a folder that is on it also hands sessions the Hub still lacks to the
background uploader. `agit --tui` opens the session picker instead.

## Examples

```bash
agit
agit --json
```

## The AgentGit model

The references cover explicit and advanced operations, and runtimes other than Claude
Code and Codex.

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

Do not confuse the project's `.git` with `~/.agit/repos/...`. Only `--code` also touches the project code repository; ordinary `agit commit` records the AgentGit conversation repository. Each directory has at most one persisted `bound repo`. Multiple session links may share the same cwd and belong to different repos. Apart from `agit save` and a bare `agit share` run inside a conversation, which take that conversation, and `agit memory status` and `agit memory diff`, which without a target read the folder's only conversation, every existing-session operation selects its target through explicit arguments or `AGIT_SESSION`; directory state and native runtime IDs never select a session.

## Working from an agent or script

- Use `--json` for a stable envelope: check `ok` and `exit_code`, then inspect
  `result.format`. Structured data is in `result.value`; text-only commands
  expose `result.lines`. Command errors and hints are in `diagnostics.stderr`.
  Startup update notices can also appear on process stderr, outside the JSON envelope.
  When a startup notice reports a newer version, run `agit upgrade` before continuing.
  `--quiet` suppresses these startup notices.
- Machine output disables terminal pickers. Supply required targets and options;
  runtime-launching commands require `--no-launch` with `--json`. `--yes` only
  answers confirmation prompts and never selects a session identity.
- agit never waits for an answer inside an agent session, even under a
  pseudo-terminal: a question it cannot ask takes its documented default and
  prints which, or fails with the flag that answers it. Create a repo from an
  agent with an explicit publishing choice: `agit init <name> --auto-push=false`
  (or `=true`). The `=` is required; `--auto-push false` is refused. Automatic
  pushing needs an account: signed out, `--auto-push=true` prints a login link
  for the human and stops before creating anything. A first `agit push` without
  `--public` or `--private` publishes private when nobody can be asked. New
  sessions record their creator and default to plain publication. Existing
  sessions retain their recorded settings. First publication confirms
  repository creation, then presents the prepared session preview; unattended
  explicit publication requires `--yes`.
- Never delete files under `~/.agit/store` or any `*.lock` file. AgentGit's locks
  are released when the process holding them exits, and deleting a lock file lets
  two writers in at once. `was already adopted` is informational; the import
  continues. If a lock error persists, look for a still-running `agit` or `git`
  process and report it to the user.
- When a command fails, follow the `hint` in `diagnostics.stderr`; it names the next
  command. Do not guess other commands or run diagnostics first. When a command says
  the person must sign in, run the login steps in [login](login.md), then rerun the same command.
- Read only the command reference needed for the operation; use
  `agit <command> --help` to check this installed build's exact options.

## Command groups

### Authentication and configuration

| Command | Meaning |
|---|---|
| `login` | Sign in to the Hub (interactive, device flow, or stdin PAT) |
| `logout` | Sign out and remove local credentials; store/repos remain |
| `whoami` | Show the current Hub identity; `--check` verifies online |
| `config` | Read/set/unset `hub.url`, default runtime, push visibility, and related settings |
| `telemetry` | Inspect usage statistics, preview their fields, or enable/disable collection |

### Repositories and runtime entry points

| Command | Meaning |
|---|---|
| `init` | Create a local Agent repo, its `main` line, and shared-file scaffold |
| `clone` | Fetch an existing Agent repo; read-only by default, `--mine` makes a copy in your namespace |
| `repo` | Manage repo create/list/info/visibility/collaborators/invite links/rename/delete/path |
| `new` | Create an empty session branch in a selected repo |
| `run` | Open a branch or saved point, continuing a writable head or forking |
| `resume` | Strictly continue an existing writable session branch |

### Adoption, context, and sessions

| Command | Meaning |
|---|---|
| `import` | Adopt an existing runtime transcript into a repo/branch |
| `project` | Explicitly enroll a project directory, sync its history, inspect status, or pause hook capture |
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
| `privacy` | Choose a session's initial publication mode, inspect storage, and manage original access |
| `fetch` | Fetch objects and remote refs; local branches do not move |
| `pull` | Fetch and fast-forward only; warns and skips divergence |
| `pr` | Create, inspect, fetch, and merge Hub pull requests |
| `share` | Create or revoke read-only sharing links |
| `activity` | Query submitted user and assistant messages by author and commit time; fetch locally when available, otherwise query the Hub |
| `month` | Draft your month in an organization from your own sessions, write it with your agent, and publish it as your own text |
| `search` | Search the AgentGit corpus visible to the current identity |
| `rc` | Start and manage the `agitd` remote-control daemon and peer/Cloud connections |

### Export, integration, and diagnostics

| Command | Meaning |
|---|---|
| `export` | Export as JSONL, IR, Markdown, Claude Code, or Codex format |
| `scan` | Scan secrets or sensitive content before publishing/sharing |
| `secrets` | Manage local secret policy and reversible synchronized mappings |
| `setup` | Install hooks, the skill, MCP, AGENTS.md integration, and shell completion |
| `upgrade` | Check for or install a newer CLI |
| `doctor` | Check local integrity and optionally the backend connection |

### Hidden integration commands

| Command | Meaning |
|---|---|
| `hooks` | Hidden runtime-hook stdin entry installed by `setup` |
| `mcp` | Hidden stdio MCP server for MCP clients |

These two commands are normally not used interactively.

## History

- Never use rebase, amend, force-push, or ordinary `git checkout` to rewrite AgentGit history.

## Commands and what they do

| Goal | Command | Result |
|---|---|---|
| Create the first Agent repo | `agit init <name> --auto-push=<true\|false>` | Creates the local repo and `main`, optionally binding the directory; omitting `--auto-push` inherits the user preference |
| Start an empty session in an existing repo | `agit new <owner/repo> -b <branch>` | Creates a real session branch and starts a runtime |
| Import an existing Codex/Claude conversation | `agit import <runtime-id> --from <runtime> --repo <owner/repo> -b <branch>` | Chooses lineage, then adopts and settles the transcript |
| Open a line from an old point | `agit fork <source> -b <branch>` | Creates a branch; add `--resume` to start it |
| Continue an existing session | `agit resume <owner/repo>@<branch>` | Restores that session's VIEW and starts it; never forks |
| Open a branch or saved point | `agit run <owner/repo>@<ref>` | Continues a writable branch head; forks other saved points |
| Save completed turns | `agit commit <owner/repo>@<branch>` | Records completed pending turns; an in-progress turn waits for settlement after it ends |
| Edit shared files on the file line (README.md, AGENTS.md, memory/, skills/) | `agit commit <owner/repo>@main -m "<msg>" [-- <path>...]` | Pure file commit on `main`; needs no session; publish with `agit push <owner/repo> -b main` |
| Invite someone to a repository | `agit repo invite <owner/repo>[@branch] --role read\|write --json` | Prints an invite link (owners only); `@branch` lands them on that session |
| Find how something was done before | `agit search "<words>" --scope org --json` | Cited session links from the person's organizations |
| Publish local history | `agit push <owner/repo>@<branch> --yes` | Prints the destination, visibility and snapshot summary, then uploads; add `--dry-run` only when the user wants a preview first |
