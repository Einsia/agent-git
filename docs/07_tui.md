# The side people use: the terminal interface

The earlier documents are all about the model: how a session is stored, how
branches split into lines, how a workspace is decoupled. This one is about **how
a person uses it** — what belongs on screen when a command is typed with no
arguments.

agit's main user is an agent: agents settle through hooks, learn who they are
from environment variables, and their output is read by programs. A person
sitting at the terminal needs something else: to **see what is there before
deciding what to do**. `agit resume` wants a branch name first and `agit new`
wants a repo name first — and remembering those two is exactly what opening the
terminal was for.

The interface solves that one thing. It adds no capability and changes no
existing output.

## 1. When it opens

Four tests, all required:

1. the command's **key argument is empty** (each command defines its own key
   argument);
2. stdin **and** stdout are both terminals;
3. **not inside an agent session**;
4. not explicitly turned off
   (`--no-tui` / `AGIT_TUI=0` / `--json` / `-q` / `-y`).

`--tui` overrides test 3, **but not test 4**. `--json` asks for machine-readable
output, `-q` asks for quiet, `-y` asks not to be prompted — none of the three is
compatible with "pop up a full-screen interface". Letting one flag silently
override another only manufactures "why was this run different" questions.

### Why test 3 speaks up and the other two do not

The three ways of not opening are nothing alike:

* **not a tty** (a pipe, CI): nobody is watching that side, and a line of
  explanation only pollutes stderr;
* **turned off by the user**: they just typed `--no-tui`; saying it back is
  noise;
* **inside an agent session**: they typed the bare command expecting a list and
  got a screen of plain text.

The third one has to be said, and it has to say which test blocked it and how to
get around it:

```text
note not opening the TUI: this looks like an agent session
     (AGIT_SESSION=nana/payments@refund-fix). use `agit --tui` to open it anyway.
```

Drop any one of those parts and the user is left guessing. The note goes to
stderr, so stdout is still consumable by a pipe.

### The tests are not a config key

There is no `tui.enabled` key. "When it opens" is a property of **this call** —
whether anyone is watching, who this output goes to — while configuration is
state that persists across calls. Make it configuration and, on one machine, the
behavior inside a pipe and the behavior in a terminal are decided by a file that
has nothing to do with either.

## 2. The handoff: the through-line

The interface does not exist for "a good-looking list"; it exists to **put the
user inside Claude Code's or Codex's own interface**.

Once a session is picked:

```text
agit's interface suspends  →  the terminal goes to the runtime  →  the runtime exits  →  agit takes it back  →  rescan, back to the list
```

**No nested interface.** The agent's TUI wants the same alt screen and the same
raw mode, and agit does not wrap another frame around it — that would wreck both
renderings at once. So terminal state is four actions (take over, suspend,
resume, give back), not two.

### The one invariant: entries and exits balance

The terminal is state that lives **outside the process**. One restore too few
leaves the user with a shell that has no echo and no cursor, and they do not
connect that to agit — they conclude the terminal is broken. One restore too
many is just as harmful: leaving the alt screen again when it has already been
left wipes out what is in their scrollback.

The invariant holds for **any** sequence of actions, including a panic partway
through, an early return, and a child process crashing while suspended. So the
state machine is independent of execution and can be tested exhaustively;
restoration goes through `Drop`, not through "remember to call it at the end of
the function".

### Results stay in the scrollback

Once the interface closes, the terminal shows what just happened, the same as
after an ordinary command. So everything that is read afterwards — which repo
was picked, which branch was created, which files were materialized — prints on
the **normal screen**, not in the alt screen. What is written in the alt screen
is gone the moment it exits, and that stretch turns blank.

`agit new` is therefore "pick the repo full-screen, type the name on the normal
screen": the name is this session's identity in version control, and it and its
result both leave a trace.

## 3. The screens

### 3.1 Bare `agit` / `agit resume` — which one to continue

```text
┌ agit ── nana @ agent-git.com ── rc: online ── ⚠ 2 unnamed ───────────┐
│  sessions                          │  refund-fix                      │
│ ─────────────────────────────────  │ ──────────────────────────────── │
│ ▸ ● here    payments/refund-fix    │  repo     nana/payments          │
│   ○ same-repo  infra/deploy-v2     │  runtime  claude-code            │
│   ⚠ unnamed  claude-code a3f9c1…   │  active   6m ago                 │
└───────────────────────────────────────────────────────────────────────┘
 ↑↓ move  enter continue  / filter  q quit
```

Three sources:

| Badge | Meaning |
|---|---|
| `here` | a session adopted in this directory |
| `same-repo` | a branch in the same code repo |
| `unnamed` | a session that exists in the runtime but is not managed yet |

All three sources share one recency axis; on an exact tie an adopted session
comes first. The status bar keeps the number of `unnamed` sessions visible
without letting the badge split the list into a second ordering rule.

Every screen also carries one bounded snapshot of `agit rc status`. The probe
runs before the alternate screen is entered and is refreshed after a runtime
handoff, so a slow control socket cannot freeze an already captured terminal.
The bar says `rc: online` only when the daemon reports a live hub connection;
every other outcome is the conservative `rc: offline`.

Naming is itself an explicit action: the interface does not decide the repo or
branch. When unnamed sessions exist, the naming inbox opens before this list,
both on initial entry and after a runtime hands the terminal back. Enter on an
unnamed row opens the same inbox again.

```text
┌ agit name ── nana @ agent-git.com ── ⚠ 2 unnamed ────────────────────┐
│  sessions to name                  │  runtime  claude-code           │
│ ─────────────────────────────────  │  session  a3f9c1…               │
│ ▸ claude-code  a3f9c1…             │                                │
│     fix the flaky retry            │  repo     nana/payments         │
│   codex  7b21ee…                    │  branch   flaky-test_           │
└───────────────────────────────────────────────────────────────────────┘
 ↑↓ session   tab repo   enter name   s skip   x ignore   q quit
```

`Tab` changes the destination repo; branch editing has its own mode so command
keys remain typeable as branch-name characters. Enter adopts through the same
`agit import <id> --from <runtime> --into <owner/repo>@<branch>` path as the
CLI, with its output on the normal screen. `s` skips only this visit and leaves
the session in the inbox for next time. `x` records a persistent dismissal, for
sessions that should never enter version control. The runtime is part of every
identity, so equal ids from two runtime indexes are never conflated.

SessionStart hooks also make the state visible while the runtime owns the
terminal. Claude titles managed sessions `agit <owner/repo>@<branch>` and
unmanaged sessions `agit: unnamed`; both Claude and Codex give the agent the
explicit `agit import` form for an unmanaged session if the user asks to save
the conversation. Codex exposes no SessionStart title field, so its managed
sessions need no response. Compacting an existing conversation does not reapply
the Claude title, because that would replace a later name chosen by the user.

`●` / `○` is "the transcript file has grown within the last 90 seconds". A live
session must not be taken over by a second writer — once two streams of appends
interleave, both histories are destroyed (see
[`04_workspaces.md`](04_workspaces.md) §4). That is data corruption, not an
experience problem, so enter on a live session is blocked and says which
terminal to exit first.

With no candidate at all it **does not open an empty shell**: making the user
press q at an empty list wastes an interaction.

### 3.2 `agit new` — pick a repo, type a name

```text
┌ agit new ─────────────────────────────────────────────────────────────┐
│  pick a repo                       │  nana/payments                   │
│ ─────────────────────────────────  │ ──────────────────────────────── │
│ ▸ nana/payments                    │  from      main (file line)      │
│     12 sessions                    │  sessions  12                    │
│   einsia/infra                     │  memory/   4 files               │
│     2 sessions · read-only         │  AGENTS.md yes                   │
└───────────────────────────────────────────────────────────────────────┘
 ↑↓ move  enter pick  / filter  q cancel
```

* **The session count counts session lines only.** The `main` that `agit init`
  creates is a file line and never claims a session
  ([`03_branch_model.md`](03_branch_model.md) §1). Take the branch total as the
  session count and a freshly initialized repo with no sessions at all reads
  `1 session` — that trades "cannot be counted" for "counted wrong", and the
  second is harder to spot. A branch with no declared form does not count
  either: guessing one is worse than admitting it is unknown.
* **A read-only checkout is marked.** `new` on someone else's checkout is legal,
  but the user has to know that publishing goes through `agit push --mine`.
* **The inheritance point is whatever `--from` gives**, defaulting to the `main`
  file line. What the screen shows and what downstream actually inherits must be
  the same string. If that point is a session line, say so on the spot — "that
  is a fork carrying context, not a new" — and give the `agit fork` form; do not
  change the semantics silently.
* **A branch name gets no "press enter for the default".** `agit init` already
  set the rule: the directory name is a suggestion, and only typing it out
  counts. A duplicate name is caught the moment typing ends, not reported as an
  error at the very end.

### 3.3 `agit log` — the timeline

```text
┌ agit log ── nana/payments @ refund-fix ───────────────────────────────┐
│ ▸ # 14 3f2a1bc turn   fix refund retry idempotency   6m ago   ⌂ v0.3  │
│   # 13 9c8e442 turn   add a regression test         18m ago           │
│   # 11 77dd310 merge  merge spike-idx                1h ago           │
│ ───────────────────────────────────────────────────────────────────── │
│  code git@…:nana/payments.git@1839e61                                 │
└───────────────────────────────────────────────────────────────────────┘
 ↑↓ move  enter read  tab branches  / filter  q quit
```

`Tab` switches between "turn-by-turn" and "branch-level". The branch level lists
the name, turn count, opening prompt, last activity and ahead/behind; enter
there opens that branch's turn-by-turn history.

Row width is budgeted in **columns**, not characters — a row of Chinese has half
as many characters as it has columns. When width runs short the yield order is
explicit: a turn row drops the message first, then the tag, and the time last; a
branch row drops the opening prompt first, then the time, then the line-form
marker, and only then narrows the name field. `#n`, the short sha, kind, turn
count and `↑↓` **never yield** — the first of those are what you locate by (they
are what lets `agit show` reach that turn), and `↑↓` is the divergence warning,
which the user will not act on without seeing it.

### 3.4 The transcript — reading that conversation

`agit show --tui`, and enter from the timeline, land on the same screen: the
list on the left, the conversation on the right.

* enter reads **that turn**, the same content as `agit show <ref>#n`;
* a transcript is parsed on demand and cached, keyed by the row's own identity
  rather than by its position — filtering changes positions;
* parsing a turn is not instantaneous, so the screen paints a frame before it
  reads. A cleared screen sitting still reads as a hang;
* text no runtime recognizes is handed over unchanged. Blank reads as "this turn
  has no content" when the fact is that it was not understood.

### 3.5 `agit import` — adopt an existing conversation

With no arguments, import lists the unmanaged runtime sessions under the current
directory. The selected row shows its opening prompt, destination repo and the
new session branch; `Tab` changes repo and `l` switches to `--link-only`, which
needs neither a sign-in nor a destination. A session that still looks active is
left for its own terminal to finish first.

The screen creates nothing. It leaves the alternate screen and fills in the
ordinary `agit import <id> --from <runtime> --into <repo>@<branch>` arguments, or
the equivalent `--link-only` form. Permission checks, branch creation, linking
and the opening settlement all stay on the command path.

### 3.6 `agit init` — create the file line

With no arguments, init opens a wizard for the repository name, directory
binding and optional project assets. The directory name is shown as a
suggestion but is never copied into the field: typing the name is the action
that chooses it. The owner is the signed-in account, or `local` while offline.

Seed selection is a separate checklist and begins empty. `AGENTS.md`,
`CLAUDE.md` and discovered skills may contain private memory, so each item must
be selected explicitly. The wizard then leaves the alternate screen and the
ordinary init path performs conflict checks, creates `main`, copies exactly the
selected assets and binds the directory.

### 3.7 `agit config` — effective and stored values

With no arguments, config opens all supported global keys in one editor. Each
row names the source of its effective value: environment, stored, default or
unset. The detail pane keeps the effective and stored values on separate lines.
For `hub.url`, it also shows the current `AGIT_HUB_URL` value and explains when
that environment variable masks the file.

`Enter` edits the stored value through the command's existing value-domain
checks, and `u` removes it. An environment override remains effective after
either operation, so the screen never implies that changing `config.json` can
change the current process environment. Explicit key/value and `--list` forms
keep their command-line behavior.

## 4. Two disciplines

### 4.1 The list does not parse transcripts

The cost of opening a screen must not grow linearly with the number of sessions,
branches or repos. List data therefore comes from metadata gathered without
parsing transcripts, and filtering never refetches it.

Import has one bounded exception: Claude has no indexed opening prompt, and a
column of ids does not identify conversations to a person. It parses only the
selected candidate on demand and caches the result; moving the cursor may parse
one more. Codex supplies that preview from its index without opening the
transcript. Candidates that were never inspected cost no transcript reads.

The fetching layer stands on its own and **asks git in batches**: the cost of a
per-item `git show` is almost entirely process startup — on this machine 12
repos and 39 branches measure 1.19 seconds, against 0.17 seconds for two batched
`cat-file` runs.

"Filtering" acts only on rows already fetched. A `/` that triggers a full
recompute is the easiest trap in an interface like this.

### 4.2 Non-tty output does not change by a byte

The interface is a layer **in front**, not a rewrite of the command. When the
tests do not hold, the original path runs unchanged; when they do, the interface
only **fills in the arguments** and hands off to that same path.

So a pipe, CI, a script and an agent session see exactly what they see with no
interface at all — including the exit code, what goes to stderr, and whether
help text takes the error channel or standard output. A screen lands only once
every command has been compared byte for byte against the same command with the
interface off.

## 5. Keys

The same on every screen, nothing to relearn:

```text
↑↓ / j k    move                  g / G     first / last
enter       main action           tab       switch view (when a second exists)
/           filter                q / esc   quit
f / b       page (conversation)   ctrl-c    quit
```

While a filter is being typed the footer keys change with it — there `q` goes
into the query, it does not quit. A footer still reading `q quit` is the screen
lying.

## 6. Turning it off

```bash
agit --no-tui log        # not this time
export AGIT_TUI=0        # not anywhere in this shell
agit --tui log           # yes inside an agent session (does not override --json / -q / -y)
```

`AGIT_TUI` is three-state: `1`/`true` on, `0`/`false` off, unset takes no
position. "Set to any value" does not count as on — `AGIT_TUI=0` is the most
natural way to write "turn it off", and reading it as "non-empty is true" turns
it into "on", the worst kind of counter-intuitive.

## 7. What is here and what is not

Here: session selection (bare `agit` / `agit resume`), repo selection
(`agit new`), the timeline (`agit log`), transcript reading (`agit show --tui`
and enter from the timeline), session adoption (`agit import`), and the terminal
handoff running through all of them.

All zero-argument interface forms in this document now use the shared shell.

## 8. Runtime validation

The Codex hook contract has an executable end-to-end probe at
[`../scripts/codex-hook-probe.py`](../scripts/codex-hook-probe.py). It creates
an isolated home, installs the hooks through `agit setup`, starts one real
managed Codex turn, captures the `SessionStart` and `Stop` payloads, and checks
that the Stop hook advances the selected AgentGit branch. It also runs Codex
with a non-default `CODEX_HOME`, which keeps session discovery, hook
installation, and transcript settlement on the same configured root.

Run it after building the debug binary:

```bash
cargo build
scripts/codex-hook-probe.py
```

The probe requires an installed Codex CLI with hooks enabled and an existing
Codex login. It copies only `auth.json` into a temporary directory and removes
that directory on exit. AgentGit uses a temporary local-only author identity;
the probe neither copies AgentGit credentials nor contacts an AgentGit Hub.
