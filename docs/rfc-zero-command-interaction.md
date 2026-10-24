# RFC: Sessions that save themselves

Status: Proposed. Implementation follows in separate merge requests after this document merges.

## Decision requested

Make automatic saving and uploading the default way AgentGit is used, and reduce the commands a
person has to know to a handful whose names say what they do: `agit on`, `agit save`,
`agit share`, `agit continue`, plus a bare `agit` that says what is happening and what to do next.
Every target these commands need — the destination repository, the session, its name — is
inferred from the current folder, its Git remote, the running agent and the signed-in account.

The existing commands, storage layout, Hub protocol and JSON contracts stay. The new commands are a
front door composed from them, so existing repositories, sessions, bindings and scripts keep
working unchanged.

## Evidence

### A fresh user, end to end

A new account on the Sim Hub, an isolated `HOME`/`AGIT_HOME`, the current `main` build, a Git
project with a GitHub-style `origin`, and a real Claude Code conversation:

| Step | What happened |
| --- | --- |
| `agit setup` | Configured hooks, skills and MCP across seven runtimes in well under a second. It also wrote `AGENTS.md` into the project's code repository, and registered MCP for Claude Code only in the current folder. |
| `agit login` (no terminal, as an agent sees it) | Prints a link and exits; finishing needs a second command, `agit login --hub HUB --complete STATE`, copied from the first one's output. |
| A Claude Code conversation in the project | Completed normally. Afterwards `agit status`: "no sessions adopted yet". **Nothing was saved, and nothing said so.** |
| `agit import` | "a session must be selected explicitly"; suggests `agit import <session-id> --from <runtime> --into <owner/repo>@<branch>`. |
| `agit import ID --from claude-code --into me/todo-app@readme` | Prints a lineage report ("No verified local prefix found; semantic comparison is unavailable") and a command to copy that adds `--independent`. Nothing is imported. |
| The same with `--independent` | Adopted, but "nothing new … the current turn is still in flight: `Bash` was called and has not returned yet" for a conversation that had ended. |
| `agit push` | "no explicit publish repository; provide owner/repo or set AGIT_SESSION". |
| `agit push me/todo-app@readme` | "first publication requires confirmation of repository creation; use --yes". |
| `agit push me/todo-app@readme --yes` | 47 seconds, then `HTTP 422 provenance_rejected: … session/meta.json unreadable (cwd is empty)` (a defect since fixed on `main`). |
| `agit share` | "`@` requires a valid AGIT_SESSION=<owner>/<repo>@<branch>". |

Ten commands in, the conversation was still not on the Hub. Every step required a fact the user
does not have (a native session ID, a runtime name, a branch name) or a concept the user should not
need (lineage, settlement, publication, `AGIT_SESSION`).

### The documented paths

- The agent quickstart on the website is eight ordered steps with about a dozen commands and ten
  recovery rules, to save **one** conversation. Users reported an agent taking 11 minutes and 40%
  of a five-hour quota for it, and half an hour inside another agent product.
- Automatic upload exists, but only through
  `agit project bind DIR --repo OWNER/REPO --history all|none --auto-upload`, against a repository
  that must already exist on the Hub; its sessions get branch names like `project-claude-code-3f2a…`.
- Every existing-session command requires `owner/repo@branch` or `AGIT_SESSION`; by design a
  folder's binding never selects a session.
- `agit --help` lists 42 commands; the first screen explains repositories, branches and commits.

### Simulated users (Sim Hub, first day)

- A Codex user set up by hand (`init`, `new … -b first-task --as codex`, `commit`, `push`), then
  found that turns were not saved automatically: Codex asks to trust hooks, they trusted only one of
  the two, and nothing told them the save hook was not running.
- The same user hit `the VIEW of … is empty — nothing to resume from` and asked what a VIEW is.
- A team lead using the agent quickstart could not finish because the final push failed, and the
  quickstart had no next step for that error. A later error, "the publication URL is outside the
  selected Hub", gave no hint whether the problem was theirs.
- They asked why a local save requires signing in.

### Field reports

- An experienced internal developer: the CLI is "so hard to use", "every command surprises someone
  who knows git", they "don't know what a command will do" when typing it, and are even less willing
  to let an agent run it.
- A team lead opens 20–40 sessions a day and needs one sentence that uploads all of today's.
- Install funnel: of 439 people who installed in a closed beta, 119 used it.

### What the evidence says

1. After installing and signing in, nothing is automatic.
2. Every command asks for identifiers the CLI could infer.
3. Messages use internal concepts and often stop without a next step.
4. Common goals (save this, send it to a colleague, pick up a colleague's, upload today's) each take
   several commands.

## Interaction model

### What a person needs to know

- **Session**: one conversation with an agent. It has a title (taken from its first request) and a
  link.
- **Folder**: where a session ran. A folder is either *on* (its sessions save and upload by
  themselves) or *off*.
- **Link**: the address of a session on the Hub. It works in a browser, in chat, and as the
  argument of `agit continue`.

Repositories, branches, commits, views, claims, lineage and publication remain the implementation
and the advanced command set; the front door never requires them and never prints them.

### Automatic by default

Once a folder is on:

- each completed turn of Claude Code or Codex is saved and uploaded in the background (the existing
  project capture worker, started by the Stop hook);
- a turn whose hook did not run (an untrusted Codex hook, a crash, being offline) is caught up the
  next time any session starts in an on folder, or any `agit` command runs there;
- uploads go to a private destination unless the person named a public one explicitly (see
  [Visibility](#visibility)); a link readable without an account exists only after
  `agit share --anyone` or `agit share --public`;
- secrets are redacted locally, as today;
- the session's name comes from its first request (`fix-login-redirect-7c1d`; a request without
  Latin letters falls back to the runtime and date, `claude-1025-7c1d`). Existing sessions keep
  their names.

### Inferring the destination

`agit on`, and `agit save` in a folder that is not on yet, choose the destination in this order and
print the choice:

1. the folder's existing binding (from `agit init`, `agit project bind` or an earlier `agit on`);
2. `--repo OWNER/NAME`, when given;
3. private repositories on the Hub already linked to this folder's Git `origin` that the account
   can write to (`GET /api/agents/for-repo`). One match is used. With several, a terminal offers a
   choice, and without one the command lists the matches with the exact command for each. This is
   how a team shares one destination: the lead runs `agit on --repo team/project-sessions` once in
   their clone, which links that origin; teammates run `agit on` in theirs;
4. otherwise `<account>/<name>`, where `<name>` is the origin's repository name, else the folder's
   name. It is created privately if missing and linked to the origin. If the account already has a
   public repository of that name, `<name>-sessions` is used instead (and so on with a number).

### Visibility

Automatic upload sends every future turn of every session in the folder, so a destination
readable by anyone must never be chosen for a person:

- Steps 1, 3 and 4 accept only private destinations. A public one there stops `agit on` before
  anything is enabled, with the next step: `agit on --repo OWNER/NEW-NAME` for a private one, or
  `agit on --repo OWNER/NAME --public` to upload to that public one knowingly.
- A public destination named with `--repo` also needs `--public`; at a terminal the confirmation
  says the uploads will be public.
- Turning a folder on records the destination's visibility in the existing automatic publication
  enrollment, and every automatic upload checks it again. When the destination's visibility no
  longer matches, uploads pause, `agit` says why, and `agit on` (with `--public` when it became
  public) resumes them.
- The front door never changes a repository's visibility.

The folder is the Git top level when there is one, else the directory itself. The home directory
and the filesystem root are refused with an explanation and a suggestion.

### Selecting a session

Commands that act on one session take an optional `SESSION`:

| Given | Meaning |
| --- | --- |
| nothing, inside an agent | this conversation (from the runtime's session variable or the ID the SessionStart hook supplied) |
| nothing, in a terminal | a picker of this folder's sessions, newest first |
| nothing, neither | `save` saves this folder's unsaved sessions; the others list candidates with the exact command for each and exit `8` — `share` never guesses |
| a Hub link | that session |
| `owner/repo@branch` | that session (the existing form) |
| words | sessions in this folder whose title contains them; several matches are listed |
| a runtime session ID or a prefix of it | that session |

### Commands

```text
agit                    what is saved here, what isn't, and the next step
agit on [DIR]           save and upload this folder's sessions from now on
agit off [DIR]          stop; nothing is deleted
agit save [SESSION]     save and upload now  (--today, --all)
agit share [SESSION]    a link to send  (--to USER, --anyone, --public)
agit continue [SESSION] pick up a session here, yours or a teammate's
agit list               sessions here  (--today, --everywhere)
agit open [SESSION]     open in the browser
agit login | logout     sign in, sign out
agit search QUERY       find earlier sessions
```

`agit help --all` lists the advanced commands; all of them keep working.

#### `agit`

```text
$ agit
Signed in as alice on agent-git.com

~/code/todo-app is on: sessions upload to alice/todo-app (private)
  Add a usage section to the README        2 min ago   uploaded
  Fix the login redirect                   yesterday   uploaded
  Try a Redis cache                        yesterday   uploading…

Send one: agit share · Pick one up: agit continue · Everything: agit help
```

When the folder is off it says so and offers `agit on`, with the destination it would use and how
many earlier sessions it would leave for `agit save --all`. Signed out, the first line is the one
command to sign in. Codex hooks that are installed but not trusted are reported here, with what to
open in Codex. Waiting and failed uploads are shown with the reason in plain words.

#### `agit on`

Signs in if needed, infers the destination, creates it if missing (privately), installs the hooks
for the runtimes present, and turns on automatic upload. Earlier sessions are not uploaded; the result
says how many there are and that `agit save --all` uploads them. In a terminal it shows the
destination and asks once (Enter accepts); elsewhere it proceeds and prints what it did.

#### `agit save`

Uploads now. With no argument inside an agent, it saves this conversation and turns the folder on,
saying so and how to undo it. `--today` saves every session that was active today, in every folder,
each to its folder's inferred destination. `--all` saves every session in this folder.
A conversation whose first reply has not finished yet is reported with its link and "appears when
this reply finishes": the Stop hook uploads it.

The agent quickstart becomes: install, run `agit save --json`, send the user the links it returns.

#### `agit share`

Saves the session if needed and prints its link. When the recipient may lack access, and the
account can invite, it also prints an invite link that grants read access to the destination and
opens this session — the output says that access covers the destination's sessions, not only this
one. `--to USER` adds that account as a reader instead. Two flags make a link that needs no
account, both through the existing share command: `--anyone` is the end-to-end encrypted share
link (the key travels in the link; `--expire`, `--views`, `--password` and `--full-log` also
select it), and `--public` is the existing unencrypted public projection. `agit share list` and
`agit share rm` are unchanged.

#### `agit continue`

Picks up the session in the current folder. One of your own sessions on this machine resumes in
place; one from another machine is fetched first. A teammate's session is fetched and continued as
a new session of yours, saved to this folder's destination when it is writable and otherwise to the
source's or your own; the original is never changed. It starts the session's runtime, or the
installed one when that runtime is missing (`--with codex|claude`). In an agent or with `--json`,
it prints the launch command instead of starting it.

#### `agit login`

In a terminal it opens the browser and waits for approval. Elsewhere it prints the link and exits
`8`; the pending sign-in is remembered, so the next `agit` command after approval finishes it, with
no second command to copy. Any front-door command that needs an account starts this itself.

### Messages

Every refusal says what happened in plain words and ends with one next step: a command that can be
run as printed, or one sentence naming what the person has to do. The front door never prints
internal terms (view, settle, claim, file line, lineage, provenance, publication, pin, `AGIT_SESSION`).
A test over the front door's message catalog enforces the vocabulary.

### Agents and scripts

- No prompts outside a terminal: the default is taken and reported, or the command exits `8` with
  the choices and the exact command for each.
- Every front-door command supports `--json` in the existing v2 envelope. Results name sessions by
  title and link and carry `next` as `{command, why}` or `{human_action, url}`.
- The skill shrinks to: everything saves itself; use `agit save`, `agit share`, `agit continue`;
  pass `--json`. The SessionStart context stops instructing agents to run `import` and `commit`.
- MCP tools for `save` and `share` follow the same rules (coordinated with the MCP owners).

## Command comparison

| Goal | Today | Proposed |
| --- | --- | --- |
| Start saving a project | `agit repo create NAME --private`, then `agit project bind DIR --repo OWNER/NAME --history none --auto-upload` | `agit on` |
| Save this conversation (agent) | eight quickstart steps: `whoami`, `login --json`, `login --complete`, `status --json`, `repo info`, `init --auto-push=false`, `import ID --from RT --into O/R@B --independent`, `push --dry-run`, `push --private --yes`, `repo invite --json` | `agit save` |
| Upload today's sessions | per session: `import` + `push`; or per folder: `project sync DIR` | `agit save --today` |
| Upload a folder's earlier sessions | `project bind DIR --repo O/R --history all --auto-upload` | `agit save --all` |
| Send a session to a colleague | `agit repo invite O/R@B` or `agit share O/R@B` | `agit share` |
| Pick up a colleague's session | `agit clone O/R`, `agit run O/R@B -b NEW` | `agit continue LINK` |
| Continue my own session | `agit resume BRANCH` (`AGIT_SESSION` or a full target) | `agit continue` |
| See what is saved | `agit status`, `agit project status DIR` (raw JSON) | `agit` |
| Stop saving | `agit project unbind DIR` | `agit off` |
| Sign in from an agent | `agit login --json`, then `agit login --hub H --complete STATE` | `agit login` once |

## Compatibility and migration

- **Storage**: unchanged. The front door reads and writes the same repositories, links, workspace
  bindings and project policies.
- **Existing folders**: a folder bound by `agit init` or `agit project bind` counts as on. If its
  automatic upload is off, `agit` says so and `agit on` turns it on after one confirmation; nothing
  starts uploading without that.
- **Existing sessions**: keep their repositories and branch names. `agit save --all` uploads ones
  never uploaded.
- **Commands**: all existing commands and flags keep their behavior and JSON output and move to
  `agit help --all`. The exception is `agit share` without any of `--anyone`, `--public`,
  `--expire`, `--views`, `--password` or `--full-log`: it now returns the session link and an
  invite instead of an encrypted share link. `--anyone` selects the previous default, and the
  other flags keep their behavior.
- **Bare `agit` in a terminal** shows the overview instead of the continue picker, which moves to
  `agit continue`.
- **Hub**: no changes are required; the front door uses repository creation with code origins,
  `for-repo`, push, invitations and collaborators as they are deployed. Optional Hub follow-ups: a
  "Continue in your terminal" button that copies `agit continue LINK`, and the shorter quickstart.
- **Skills and hooks** are refreshed by `agit setup`, which the installer and upgrades already run.

## Delivery

1. This document.
2. `agit on`/`off`, destination inference, readable session names, catch-up, bare `agit`.
3. `agit save` (`--today`, `--all`), sign-in that finishes itself, the new skill and SessionStart
   context, and the shorter quickstart on the website.
4. `agit share`, `agit continue`, `agit list`, `agit open`.
5. Help and message pass over the remaining front-door paths; `docs/00_usage.md` rewritten around
   the front door, with the advanced commands as reference.

Each is verified on the Sim Hub with real Claude Code and Codex sessions, then measured by a
simulation round against the friction metrics (commands typed per person, actions that should have
been automatic) before and after.

## Not in scope

- Installation without Node.js, and the installer itself.
- Hub address identity across renamed or re-addressed Hubs (tracked separately).
- Changing what is encrypted or redacted.
- Per-session read access on the Hub; invites grant read access to a destination.
