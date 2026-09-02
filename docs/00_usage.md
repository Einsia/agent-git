# Using agit

A guide for users, organized by **what you want to do** rather than as a command table. Every
section can be typed as written.

Written against `agit 0.9.0` (`agit --version` tells you which one you have). The full flag list
is in `agit <command> --help`; this document covers only the part you reach for most.

## 1. The mental model first

```
agent   = a git repo       lives in ~/.agit/repos/<owner>/<name>
branch  = one session      once a session occupies a branch, that branch never changes hands
commit  = one user turn    you ask, the agent finishes answering: that is one commit
main    = the file line    AGENTS.md, memory/, skills/ — what sessions share
```

This is not a metaphor; underneath it is a real git repo. `agit log` is the history from the
session's point of view, `agit repo path` prints the repo path, and `git log` inside it shows the
same commits.

Two words recur below:

- **settle**: cut "the conversation added since the last record" into turn commits. The command is
  `agit commit`.
- **VIEW**: the context the agent really sees on the next resume. It is derived — when
  `agit revert` takes something out of it, the original record (the log) does not move a byte.

## 2. Getting started

### 2.1 Install

```sh
npm install -g @einsia/agent-git     # prebuilt binary, no Rust toolchain required
agit --version
```

People changing the code install from source:
`git clone https://github.com/Einsia/agent-git && cd agent-git && ./setup.sh`. Both paths install
the same binary.

You also need **git >= 2.28** (repo init uses `git init --initial-branch`). The rest of the
details (musl static linking on Linux, environment variables, platform sub-packages) are in
[`01_setup.md`](01_setup.md) and [`../npm/README.md`](../npm/README.md).

> Do not install `@einsia/agentgit` (no hyphen) — that is the pre-rewrite CLI; its protocol does
> not match.

### 2.2 Sign in

```sh
agit login
```

On a TTY it asks whether to authorize in the browser or use a device code. Where there is no TTY
but someone is watching the output — a container, say — `agit login --device` skips the menu and
goes straight to the device code (it prints a short code you confirm on another device). Fully
unattended CI and agent subprocesses have one path:

```sh
agit login --with-token < token.txt
```

Signing in is not only for push. **Settlement needs an account name**: the commit author and the
`<owner>/` of the repo path both come from the credentials, and neither can be filled in
afterwards. Without a sign-in, `agit commit` refuses outright.

To mark a session offline first, use the `--link-only` of 3.1 below.

### 2.3 Wire up the runtimes (once; after that, forget it)

```sh
agit setup
```

It does four things; the first three are idempotent (the AGENTS.md one has a known flaw, see the
note below):

| What        | Where                                              | Effect                                                       |
| ----------- | -------------------------------------------------- | ------------------------------------------------------------ |
| hooks       | `~/.claude/settings.json` (**claude-code only**)   | SessionStart registers the session, Stop settles a turn automatically |
| skill       | `~/.claude/skills/agit/`, `~/.codex/AGENTS.md`, and so on | teaches the agent the `agit commit --milestone` discipline |
| MCP         | each runtime's MCP config                          | the agent can `search` / `show` other people's sessions directly |
| AGENTS.md   | the current project                                | the project-level rules block                                |

Name the one runtime you use, so no config is written for tools you do not have:

```sh
agit setup --runtime claude-code
cd ~/Projects/payments && agit setup --agents-md     # install just the project-level block
```

With hooks installed, Claude Code **settles once every time a turn completes**, and day to day you
never type `agit commit`. Manual settlement stays useful — see the milestones in 3.2.

Hooks are a Claude Code mechanism. codex / opencode / cursor have no equivalent "turn finished"
callback; the skill above injects the settlement discipline into the agent, and the agent types
`agit commit` itself at the right moment.

> Running `agit setup --agents-md` repeatedly appends another `<!-- agit:end -->` marker to
> AGENTS.md every time. When you see a run of extra end markers, delete by hand until one pair is
> left.

### 2.4 Create an agent repo inside a project

```sh
cd ~/Projects/payments
agit init payments
```

```
✓ repo created: alice/payments (main is the file line; scaffolding in ~/.agit/repos/alice/payments)
  bound to this directory. Next:
    agit import          adopt a running session (settles on import)
    agit new -b <name>   start a fresh session (inherits memory/skills)
```

`--seed` finds the AGENTS.md / CLAUDE.md / `.claude/skills/` already in the project and collects
them into main **after asking you item by item**. With no TTY it collects nothing (personal memory
can hold private material, and it is never collected silently); `-y` takes everything.

**`agit init` writes no file into your code repo.** The binding between the directory and the
agent is recorded under `~/.agit/workspaces/`. (The only things that touch project files are the
AGENTS.md block from `agit setup`, and the `agit commit --code` you ask for explicitly in 3.2.)

## 3. The everyday path

A fictional project `payments` ties the rest together. The output fragments are real runs.

### 3.1 Put a running session under version control

The most common starting point: you have done a stretch of work in Claude Code and want that
conversation to have a history.

```sh
cd ~/Projects/payments
agit import                       # no argument: lists the unadopted sessions in this directory
```

Once you have picked one (or given the id prefix directly):

```sh
agit import 7f3a1c2e -n payments -b ratelimit
```

```
✓ adopted claude-code 7f3a1c2e-111
working dir  unknown (filled in when a version is recorded)
link         ~/.agit/store/claude-code/7f3a1c2e-1111-4a4a-8b8b-000000000001.json

#1 a37029959 add a per-user_id rate limit to payments
#2 21d8a51ff add a test for it

✓ settled 2 turns → alice/payments @ ratelimit
```

Adopting and recording the first version are **one command** — the in-between state ("linked, but
unversioned") means nothing to anyone.

Both arguments are required; agit does not guess:

- `-n <agent>` which agent it lands in. A wrong guess hangs this memory on the wrong lineage, and
  that kind of mistake is not noticed right away.
- `-b <branch>` which branch it lands on. A session may never land on `main` (that is the file
  line).

import **does not copy** the transcript; it writes a link. The original session keeps growing and
the link keeps pointing at it.

**Offline** (on a plane, not signed in yet):

```sh
agit import 7f3a1c2e --link-only    # mark it only, record no version
# back online: run import again — the link is still there, and this time the first version lands
agit import 7f3a1c2e -n payments -b ratelimit
```

(`agit commit` has no `-b`; fill the version in with the import above rather than with commit —
commit cannot say which branch to land on.)

**To show it to outsiders**, redact first: `--privacy` adopts a washed copy (secrets become
`[redacted:<rule>]`; home directory, user name and host name become stable pseudonyms), and not
one byte of the original enters history. claude-code only; other runtimes use
`agit export --redact`.

The copy is **frozen** and carries its own new session id: the original session keeps growing, the
copy does not follow. To publish later conversation, run
`agit import <id> --privacy -b <new-branch>` again — every run is a new frozen copy, and the old
branch, already taken by the old copy, is never refreshed.

### 3.2 Settle: get new conversation into history

With hooks installed this step is automatic. Three occasions call for typing it by hand.

**a. Land it every so often**

```sh
agit commit
```

```
  target: alice/payments @ ratelimit (cwd match (this directory’s only adopted session))
#3 6b2bb67d8 make the rate-limit threshold configurable

✓ settled 1 turns → alice/payments @ ratelimit
  next: agit push to publish · agit log to read history
```

One turn is one commit, and the message is that turn's user prompt (squashed to one line,
truncated when too long). A trailing turn that has not ended stays for next time — the question is
there but the agent has not answered, or a tool call the agent made has not returned yet (when the
agent runs `agit commit` from inside a turn, that call is the open one). It says so:
`the current turn is still in flight`. The turn lands on the next `agit commit` after the turn
ends (the Stop hook does this), and another `agit push` is what puts it on the Hub. With nothing
new it prints `nothing new since ...` and exits normally; that is not an error.

**b. A phase is done: mark a milestone**

```sh
agit commit --milestone "rate limiting done, tests pass" --tag ms-ratelimit --code
```

- `--milestone` writes a one-line phase summary into the last turn commit; `agit log` shows it
  as ★.
- `--tag` tags it while you are there.
- `--code` also commits in the **code repo** (it really commits only when there are uncommitted
  changes; on a clean tree it records the current HEAD as the anchor) and writes the `origin@sha`
  cross anchor into this turn. The code repo must already have an `origin` remote; when cwd is not
  a Git repo, `--code` warns and skips the code-side commit, and the session turn still settles
  normally.

This one is for "whoever reads the log later and wants to know how far it got". Automatic
settlement does not write it for you.

One precondition: all three flags **attach only to the new turns this settlement produces**. With
no new turn pending, the whole command is a no-op (it prints `nothing new since ...`, exits
normally, and lands neither the ★ nor the tag nor the anchor). So with hooks installed, have the
agent type this inside the session — while the Stop hook has not settled that turn yet; typing it
in the terminal afterwards cannot catch up.

**c. Only shared files changed**

```sh
cd $(agit repo path)          # into the agent repo's main checkout (the main file line): edit memory/ skills/ AGENTS.md directly
                              # a session branch's copy lives in its own worktree: agit repo path <owner/repo>@<branch>
cd -                          # back to the project directory to settle: context resolves against the project directory, never inside the agent repo
agit commit -m "memory: conclusions on the refund path"
```

`-m` is a file-only commit, legal only when **no new conversation is pending settlement**. With
new turns it refuses; run `agit commit` first.

### 3.3 Look back

```sh
agit log
```

**Typed in front of a terminal, this opens the full-screen interface**, not the text below: a
timeline ordered by turn, `Tab` switches to the branch view, Enter reads that turn's conversation.
In a pipe, in CI and inside an agent session it stays text; to get text in a terminal too, add
`--no-tui`. The tests and the other screens are in [`docs/07_tui.md`](07_tui.md).

Here is the text form:

```
#  1 acdd8307e [file ] agit: init (main file line)
#  2 82eb408cb [file ] agit: claim session line ratelimit
#  1 a37029959 [turn ] add a per-user_id rate limit to payments
#  2 21d8a51ff [turn ] add a test for it
#  3 6b2bb67d8 [turn ] make the rate-limit threshold configurable
#  4 2f5f1a3a6 [turn ] make the error code configurable too  ⌂ ms-ratelimit
      code https://github.com/alice/payments.git@3d0fac3
      ★ rate limiting done, tests pass
```

That last turn is what the settlement in 3.2b produced: `⌂` is the tag, `★` is the milestone, and
`code` is the code anchor `--code` recorded.

Common combinations:

```sh
agit log --oneline -n 50            # one line per turn
agit log --kind turn                # conversation only, without file commits like repo creation and claims
agit log --grep rate --since 7d     # message substring + time
agit log --graph                    # the structure when there are several branches
agit log -- memory/notes.md         # only commits that touched this shared file
```

**Read the conversation itself**:

```sh
agit show                                   # the most recently touched adopted session (not the context chain in 8.2)
agit show 7f3a                              # by session id prefix — this one is exact
agit show alice/payments@ratelimit          # the VIEW of one branch of one repo (the world resume sees)
agit show 'ratelimit#5.1'                   # the 1st event the 5th commit added, raw JSON
agit show 'ratelimit:AGENTS.md'             # a shared file's contents at that point
agit show --agent alice/payments --tui      # full-screen browsing
```

Mind the quotes: `#` starts a comment in the shell, so always quote a reference carrying a `#`.

**Compare two points**:

```sh
agit diff main..ratelimit                   # fork point + which turns each side added (the default)
agit diff --view 'ratelimit#3..ratelimit'   # insertions / deletions in the VIEW sequence
agit diff --files v0.1..ratelimit           # text diff of the shared files
agit diff                                   # no range: what is still unsettled in the workspace
```

**See what the VIEW is made of** (the scouting command before a merge):

```sh
agit view ratelimit
```

```
  VIEW @ ratelimit (8 events)
     0    log#0 user           488B this branch            add a per-user_id rate limit to payments
     1    log#1 assistant      466B this branch
     ...
```

### 3.4 Continue yesterday's session

```sh
agit resume ratelimit
```

When the native session is still on this machine it is zero-copy (`claude --resume <id>`
directly); otherwise a new one is materialized from the VIEW and launched.

```sh
agit resume ratelimit --no-launch      # print the launch command only, paste it yourself
agit resume ratelimit --as codex       # switch runtime (it shows you the lossy list first)
agit resume ratelimit --cwd ../payments-2
```

The zero-copy path reuses the runtime's native transcript and bypasses the VIEW. So what
`agit revert` (see 4.3) just took out of the VIEW is **still visible** in a session resumed
zero-copy on this machine. For a revert to take effect in the resumed session, the materialized
path has to run — another machine, a merge, `--as` and `--cwd` all trigger it.

Every turn settlement also records a summary of the Git state of the cwd at that moment. On
resume, when the current cwd's origin, HEAD, branch or worktree summary has changed, resume lists
both states first and lets you continue, inject the difference into the runtime as
system/developer instructions, or cancel. When the current cwd is not a Git repo it only says the
comparison is impossible and continues; it never blocks the resume.

`resume` is a **strict entry point**: it takes a branch only. Tags, historical commits and `#n`
are all refused; those go through `fork` (next section). The reason is that history is not
rewritten — to get back to an old state, grow a new line instead of bending the old one back.

Start a session from scratch (no old context, only the team memory):

```sh
agit new -b onboarding                 # inherits AGENTS.md / memory/ / skills/ from main
```

Omitting the repo name relies on "the adopted session in the current directory" or the branch
`agit switch` pinned — **the directory binding does not count**. In a directory where you have
just run `agit init` and imported no session yet, this prints `can’t resolve the target`; write
the repo out in full: `agit new alice/payments -b onboarding`.

### 3.5 Off track: back to one turn and start over

```sh
agit log                               # find that point's short sha or tag first
agit fork 21d8a51 -b ratelimit-retry --resume
```

```
✓ forked 21d8a51 into ratelimit-retry (alice/payments @ e45a9f2e8 — new session in place)
```

fork is the only form of "checking out an old state" in agit. The source can be a branch head, a
historical commit, a tag, `<ref>#n`, someone else's `owner/repo@ref`, even a sealed branch. The
old line stays exactly as it was.

`--resume` means "launch it right after the fork"; without it you only get the branch and
`agit resume` it yourself later.

### 3.6 Publish and pick up

**Publish**:

```sh
agit push --dry-run     # rehearsal: runs the secret scan, lists what would be sent, no network
agit push
```

```
dry run — nothing left this machine
repo        alice/payments
branches    ratelimit, main
versions    1
remote      none yet (a real push would create it)
visibility  asked at first publish; unchanged after that
```

Three things worth knowing:

- **Visibility is decided at the first push only.** On a TTY it asks; non-interactive defaults to
  private. Change it afterwards with `agit repo visibility alice/payments public`; `agit push`
  never touches it.
- **Publishing is selective.** By default only the current session branch is pushed, `-b` can be
  repeated, and `--all` is what pushes everything. The main file line comes along — that is where
  the `ratelimit, main` in the fragment above comes from.
- **There is no `--force`.** History only grows.

**Pick up someone else's**:

```sh
agit clone einsia/payments                  # fetch only, nothing is launched; origin points at the source
agit show einsia/payments@refund-fix        # look at one of its session branches first
agit run einsia/payments@refund-fix -b my-take   # to actually run it: forks a branch you can write to and launches it
```

Both of those must point at a **session branch**. `@main` is the file line and carries no session,
so `show` and `run` both refuse it (to start a new session on its team memory, use `agit new`).
When you do not know the branches, run `agit log einsia/payments` first.

`clone` is **read-only by default**: nothing is created in your name, and local `agit commit` works
as usual. When you decide to take over:

```sh
agit clone einsia/payments --mine      # copies it under your name on the hub, repoints origin at yours, remembers the source as upstream
```

`agit run` is "one command to run any frozen ref": fetch → arbitrate (fork if needed) →
materialize → launch. The one-line reproduction command you hand people in a README usually looks
like this:

```sh
agit run lab/repro@v1 -b repro-1
```

**Sync between two machines**:

```sh
agit pull        # fast-forward only. On a real divergence it offers merge / fork and decides nothing on its own
agit fetch       # objects and remote refs only — local branches never move
```

### 3.7 Memory: the local directory, the session branch, main

Memory has three homes; the first two sync automatically, and the third moves only when you decide
it does:

```text
the runtime's own memory dir   Claude Code: ~/.claude/projects/<project>/memory/   ← live copy, agit does not take it over
the session branch's memory/   this session's versioned snapshot                   ← collected at every agit commit
main's memory/                 shared by the team, inherited by agit new           ← moves only through agit distill / merge
```

`new` / `resume` merge the branch's memory into the runtime directory before launching: into
per-branch subdirectories `agit/<owner>/<name>/<branch>/`, with a marked index block in
`MEMORY.md` pointing at them. A top-level local file with the same name and the same content is
not placed again, and no file of your own is touched. `resume` the same branch on another machine
and Claude reads that memory immediately. Both sides look only at first-level `*.md` (the shape of
Claude memory); subdirectories and other extensions do not enter the branch.

Every `agit commit` (the Stop hook's automatic settlement included) collects the changes
**relative to the baseline taken at launch** into the session branch, as one file commit: what was
newly written, modified or deleted at the top level, plus the agent's edits and deletions inside
the mirrored subdirectories. Personal memory that was already there at launch and was not touched
this time does not enter the branch. A file the secret scan hits (including values registered
with `agit secrets`) is not collected, and is named. A session not launched through agit has no
baseline: settlement establishes the baseline and collects nothing, and an explicit
`agit memory sync` is what pulls in everything currently at the top level.
`agit config memory.track off` turns local collection off.

`main` does not move on its own — it gets pushed and inherited by a colleague's `agit new`, while
the runtime's memory directory naturally holds personal feedback and private material. When a
phase is done:

```sh
agit memory status          # one line per file: branch vs main, local vs branch
agit memory diff notes.md
agit distill                # files that differ from main enter main after item-by-item confirmation (each passes the secret scan first)
agit push -b main
```

A file deleted on the branch that main had passed down is carried into main as a deletion by
distillation too. `commit --milestone` and `push` report how many items are not distilled yet.
`agit memory sync` does one bidirectional sync right away. Only Claude Code has a per-project
memory directory on disk; on other runtimes `sync` is a no-op, while `status` / `distill` work on
the branch as usual.

## 4. Several people, several lines

### 4.1 Share with someone who does not have agit

```sh
agit share 7f3a --expire 24h --views 3
```

Mints a read-only sharing link; the other side needs no account. **End-to-end encrypted by
default**, with the key in the URL's `#k=` fragment, printed once at that moment and never again —
so send the whole link; `agit share list` cannot give the key back.

```sh
agit share list           # which links are still alive
agit share rm <slug>      # revoke one
```

`--public` is an unencrypted, crawlable link; `--password` adds a passphrase. A session with a
secret finding is refused outright.

### 4.2 Search for precedent

```sh
agit search "monorepo build cache" -n 5
```

Searches the corpus you can read for "has anyone done this before". Every hit carries an outcome
(success / failed / unknown) — a failed one is a warning about the pitfall, not a recipe.

The main entry point is MCP: after `agit setup` the agent calls `search` itself and looks for
precedent whenever it is stuck.

### 4.3 Merge two sessions

You forked off the main line to try something, the conclusion is worth bringing back, but the
process must not pollute the main line's context:

```sh
agit merge try-ratelimit --dry-run      # the fork point and how much each side added, first
agit merge try-ratelimit -m "the conclusion only, not the process"
```

The second opens a transaction, locks the target branch, and then **launches a merge agent** to do
the reconciliation. Text conflicts are not the point; **intent conflicts** are (one side buckets by
`user_id`, the other renames it to `uid`; both compile, and together they are wrong).

The merge agent runs the whole process on its own side — picking material, reconciling shared
files, writing the conclusion, landing it — and you only read the result. To learn where it got
to, or to call it off:

```sh
agit merge --status      # while the transaction is open and unlanded, where it is stuck
agit merge --abort       # give up; the target branch never moved
```

That `-m` sentence goes to the merge agent as the opening prompt and bounds the reconciliation
("the conclusion only", "keep the reproduction steps", and so on). Without a summary nothing
lands; agit enforces that and the agent cannot get around it.

**To pick a few turns only**, not worth a transaction:

```sh
agit cherry-pick try-ratelimit#3..#4 -m "take the uid rename over"
```

**Take a bad conclusion out of the context**:

```sh
agit revert @#12.4 -m "the conclusion is wrong"
```

It removes from the VIEW only; not a line of evidence leaves the log. This is the one correct way
to undo — no rebase, no amend, no force push.

### 4.4 PR

Propose a change to someone else's agent:

```sh
agit clone einsia/payments --mine                        # a pushable copy under your own name first
# do the work, agit commit
agit push -b my-branch
agit pr create einsia/payments@refund-fix -b my-branch -m "what it does"
```

`-b` is the source branch in your fork; the positional argument is their destination
(`<owner/repo>[@<branch>]`). The author's `agit pr merge` **does not launch a merge agent** — it
only lands what you proposed, so when the two sides have really diverged, reconcile it in your own
fork (4.3) before proposing.

On the author's side:

```sh
agit pr list alice/payments      # owner/repo must be written out here
agit pr show 12
agit pr fetch 12                 # lands at local refs/agit-pr/12
agit pr merge 12
```

## 5. Scan for secrets before publishing

```sh
agit scan
```

```
✓ clean scan (1 refs)
```

`agit push` runs the same scan internally; `agit scan` runs it separately, up front (CI uses it
too). The exit code is 7 when it finds something.

- **False positive**: add that string to `$AGIT_HOME/.agit-allow-secrets`, or put an
  `agit:allow-secret` note on that line of the original.
- **A real secret**: `agit revert @#n.k` takes it out of the VIEW.

`--sensitive` is designed to start a separate local review agent that looks at sensitive content
rather than structure, but 0.9.0 ships no usable local review agent: running it reports an unmet
precondition (exit code 4). What works today is the default `--secrets` structured scan.

## 6. Detach a session from the terminal (remote control)

Closing the laptop must not stop the session.

```sh
agit login
agit rc start --detach --name my-laptop
agit rc status
```

The daemon connects to the hub over outbound WSS; no inbound port is opened. Then **Bind a
folder** in the web interface: at the moment of binding, the private agent repo for that folder is
created. Every message sent from the web interface still goes through the same hooks settlement
path, lands as a turn commit, and shows up in `agit log`.

```sh
agit rc list             # every machine under your name (offline ones included)
agit rc revoke <id>      # revoke one: disconnects immediately, frees the slot
agit rc stop
```

The quota is 5 machines per person; an offline machine still holds its slot, and only a revoke
frees it. The full design is in [`04_workspaces.md`](04_workspaces.md).

## 7. When something goes wrong

```sh
agit doctor
```

```
  [✓] runtime claude-code resumable · format claude-code · /Users/alice/.local/bin/claude
  [✓] git              git version 2.47.1
  [✓] local store      1 adopted sessions, 1 versioned
  [!] agent repos      1 of them, 1 with unpublished commits: payments
  [✓] sign-in          alice @ https://agent-git.com

=== session metadata integrity ===
  ✓ all 1 repos’ session metadata is consistent
  ✓ checked 1 live transcripts: all continue committed content
  ✓ the VIEW of 1 repos all reference reachable events
```

An offline check-up: whether the runtimes are on PATH, the git version, the store against the
remote, whether the VIEW is self-consistent, whether live transcripts are still being appended to.
Add `--check-backend` when you suspect the network.

```sh
agit upgrade --check      # report whether a new release exists, nothing else
agit upgrade              # atomically replaces the current binary; on failure the installed one is untouched
```

## 8. Quick reference

### 8.1 Reference syntax

```
owner/repo              a repo on the hub (the repo name alone when it is unique locally)
owner/repo@<ref>        a ref in someone else's / a remote repo
<branch> <tag> <sha>    a ref inside the current repo (sha prefix ≥ 4; an ambiguous match is reported, never resolved for you)
@                       the current session's branch (resolved through AGIT_SESSION, valid only inside a session)
<ref>~n                 n commits back
<ref>#n                 the commit the n-th turn settled (#-1 is the last turn)
<ref>#n.k               the k-th event in the n-th turn
<ref>#a..#b             a turn range
<ref>:<path>            the file contents at that point
```

Two easy traps:

1. **The n in `#n` is the turn ordinal in the left column of `agit log`**. Repo creation, claims,
   a fork's identity commit, a `-m` file commit and merge commits take no turn ordinal and leave
   that column blank in the log; point at them with a short sha, a tag or `<ref>~n`. Only when the
   whole history never declared a turn ordinal (an ordinary git branch pushed in from outside)
   does `#n` fall back to "the n-th commit from the root".
2. The shell treats a leading `#` as a comment. Always quote a reference containing `#`:
   `agit show 'ratelimit#5.1'`.

### 8.2 "What is the current branch"

A command with the target omitted looks for one in this order. To see which one matched, run
`agit status` — it echoes the route (`via: ...`); `commit` / `fetch` / `merge` put the route in
parentheses on the `target: ...` line; `log` / `show` and friends print nothing.

```
1. what you wrote explicitly (positional argument / --repo / -C)
2. the AGIT_SESSION environment variable          ← injected when agit launches a session; the proper route for an agent calling agit inside its own session
3. the session id environment variable the runtime exposes itself
4. the branch agit switch pinned
5. the only adopted session in this directory     ← with several, they are listed for you to pick; no guessing
6. none of them: an error that tells you what to type next
```

`@` uses steps 2 and 3 only. The cwd match and the pin do not apply to `@` — a merge agent and
parallel sessions can share one directory, and a guess there names the wrong session.

When several sessions run in one directory and commands start refusing to guess:

```sh
agit status              # what the via line resolves through right now
agit switch ratelimit    # pin it
agit switch --unbind     # unpin
```

### 8.3 Command overview

| Goal                   | Command                                                   |
| ---------------------- | --------------------------------------------------------- |
| Sign in / identity     | `login` `logout` `whoami` `config`                        |
| Create / fetch repos   | `init` `clone` `repo` (create/list/info/visibility/collab/rename/delete/path) |
| Adopt / status         | `import` `status` `switch` `branch` (rename/rm/seal)      |
| Record                 | `commit` `tag` `memory` (status/diff/distill/sync) `distill` |
| Inspect                | `log` `show` `diff` `view`                                |
| New line / continue    | `fork` `new` `resume` `run`                               |
| Merge / undo           | `merge` (pick/drop/summary) `cherry-pick` `revert`        |
| Remote                 | `push` `pull` `fetch`                                     |
| Discover / share       | `search` `share` (list/rm) `pr` (create/list/show/fetch/merge) |
| Export / diagnostics   | `export` `scan` `setup` `upgrade` `doctor`                |
| Remote control         | `rc` (start/stop/status/list/pair/revoke)                 |

Global options: `--no-color` `--json` `-y/--yes` `-q/--quiet` `-C <dir>`.
Only some commands really emit JSON for `--json` today (`view` and `scan` certainly do); do not
rely on it indiscriminately in scripts.

### 8.4 Where things live

```
~/.agit/repos/<owner>/<name>/    agent repos (real git repos)
~/.agit/store/                   session links (pointing at the original in the runtime directory, not a copy)
~/.agit/workspaces/              directory ↔ agent bindings, the branch switch pinned
~/.agit/credentials/<hub>.json   credentials, one file per hub (0600)
~/.agit/config.json              global config
~/.agit/secret-filter/           the encrypted vault of registered secrets (its key is elsewhere)
~/.agit/keystore/                the vault key, only with `secrets.keystore = file` (0600)
```

`AGIT_HOME` moves the whole thing elsewhere (default `~/.agit`). `AGIT_HUB_URL` switches hub, and
**switching hub is switching identity**: credentials are stored per host, so switching back and
forth needs no new sign-in.

Config has six keys, and the same command reads and writes:

```sh
agit config --list
agit config runtime.default codex     # write
agit config runtime.default           # read
```

`hub.url` · `runtime.default` · `push.visibility` · `commit.auto` · `memory.track` (`session | off`,
whether the runtime's project memory is collected into the session branch, see 3.7) ·
`secrets.keystore` (`os | file`, where the secret-filter key lives: the system credential store,
or a private file under `~/.agit/keystore/` for a machine with no desktop session such as an SSH
login or a CI runner — Unix only, and a backup of `~/.agit` then carries the key along with the
global vault, while a repository's dictionary in its `.git` takes both backups to decrypt;
`AGIT_SECRETS_KEYSTORE` overrides it, and `agit doctor` reports whether the chosen store
answers).
`push.visibility` governs the first publish only: push's `--private`/`--public` overrides it, and so
does the preference `agit init --private` records in the repo; set to `ask` (the default) it asks
once at the first publish, and a non-interactive environment gets private.
Setting `commit.auto` to `false` turns off the hooks' automatic settlement and makes everything
manual. When that key has never been set, `--list` shows `commit.auto = false (default)` while the
real default behavior is that automatic settlement is **on** — only an explicit `false` turns it
off, so do not read that default in `--list` backwards.

Common exit codes: `0` Ok · `2` Usage · `3` Ref / context does not resolve · `4` Precondition ·
`5` Auth (not signed in) · `6` Network · `7` Policy (a secret is blocked) · `8` Interactive.
The same failure is not necessarily the same code across commands; in scripts, read stderr too.

## 9. Read on

| To learn about                              | Where                                                  |
| ------------------------------------------- | ------------------------------------------------------ |
| Build, run the backend, debug               | [`01_setup.md`](01_setup.md)                           |
| How sessions are stored locally             | [`02_session_store.md`](02_session_store.md)           |
| The one-branch-one-session model in detail  | [`03_branch_model.md`](03_branch_model.md)             |
| The design of workspaces and remote control | [`04_workspaces.md`](04_workspaces.md)                 |
| Login / token mechanics                     | [`commands/auth.md`](commands/auth.md)                 |
| Probed storage formats of each runtime      | [`mechanism-probing/`](mechanism-probing/)             |
