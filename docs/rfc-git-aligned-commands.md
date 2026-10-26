# RFC: Commands that behave like Git

Status: Accepted as the direction (2026-10-26). The steps that change no existing command's name
or meaning (1, 2, 6, 7 and 8 under [Migration](#migration)) are scheduled after 0.2.28. The steps
that change what an existing command does or add command names (3, 4 and 5) wait until the command
surface has been reshaped, and may change with it. Follows
[Sessions that save themselves](rfc-zero-command-interaction.md), whose front door (`agit on`,
`save`, `share`, `continue`, `list`, `open` and a bare `agit`) is being implemented. This RFC covers
the rest of the command line.

## Decision

Make every agit command that shares a name with a Git command behave the way a Git user expects:
the same default target, the same split between local and network work, and nothing published or
deleted without the same kind of preview. Keep AgentGit's own ideas, conversations as branches and
a Hub that hosts them, but stop asking people for agit's internal names. No command is removed in
the 0.2 line, and old forms keep working.

## Why

What people say, across the Feishu threads of 10-20 to 10-25 and the Sim rounds:

- The commands surprise Git users: the same verb needs arguments Git never asks for, or does
  something else.
- `agit push` asks about things nobody has heard of, such as a device-local RC repository or a
  pinned repository identity (#92, #105).
- The secret scan runs twice, on this computer and again on the Hub, and the second run can
  disagree with the first.
- People are afraid to run commands by hand, and afraid to let their agent run them.

What the Sim shows. In A/B round 2 (10-25, 22:09-02:09), the person who typed commands followed the
Hub quickstart's classic steps, which mean five verbs with flags such as `--independent`,
`--private` and `--dry-run`. That cost 15 commands and 3 failures before the first upload. The
person whose agent used the front door typed none, and the first upload came after 3.3 minutes.
In #118, `agit memory status` and `agit scan` refuse in the folder where the user just worked and
ask for `owner/repo@project-claude-code-2aa6…`, a branch name the user has to look up on the Hub.

The common cause: Git resolves "which repository" from the working directory and "which branch"
from `HEAD`, while agit asks for both every time.

## The model, in Git's terms

| Git | agit | Who can see it |
| --- | --- | --- |
| working directory | the folder | everyone |
| repository | the folder's repository on the Hub (`owner/repo`) | everyone |
| `HEAD`, the current branch | the conversation: the one the command runs in, else the most recent one in this folder | everyone |
| remote and upstream | the Hub and the folder's repository | `agit status` |
| commit | a turn | `agit log` |
| `git push` after each commit | automatic upload, when the folder is on | `agit status`, the notice when a conversation starts |

agit's other internal names stay out of what people read: claims, links, the file line, the VIEW,
settlement, publication modes, the RC repository, pinned identity, accepted history and the
evidence log. They remain in `--verbose` output, in advanced commands and in the documentation of
the store.

## Rules

1. **Default target, as in Git.** Every command that acts on a conversation or a repository
   resolves its target in this order:
   1. an explicit `owner/repo@branch`;
   2. the conversation the command runs in (the runtime's session variable, `AGIT_RUNTIME_SESSION`);
   3. at a terminal, the folder's most recent conversation, named and confirmed with Enter;
   4. without a terminal, a refusal that prints the exact target to pass.

   This applies to commit, push, log, show, diff, scan, memory, branch and tag. It fixes #118.
   Agents keep explicit targets: rule 2 makes those correct, and rule 4 keeps a script from acting
   on a guess.

2. **Local verbs stay local.** `status`, `log`, `show`, `diff`, `branch`, `tag`, `commit` and `list`
   never contact the Hub unless asked (`--fetch`), never change remote state, and say so in `--help`
   ("changes nothing on the Hub"). Only `push`, `fetch`, `pull`, `clone`, `share`, `continue`,
   `save` and automatic upload use the network.

3. **One question before publishing, and only once.** The first upload to a new destination shows
   the repository, who can see it and how many conversations go there, then asks once, with Enter
   required. Today the forms that fork a branch and then ask a second question can leave the fork
   behind (#108). A cancelled command leaves nothing behind. Uploads after the first one are not
   asked about again, as with `git push`. Every publishing or deleting command takes `--dry-run`.
   Agents get the same preview in JSON, with `next.human_action` set, and must not pass `--yes`
   until the person has agreed.

4. **One secret check per upload.** The person sees one "checking for secrets" step and one
   verdict. The Hub's check is a safety net that agrees with the local rules (same detector
   version, sent with the push) and refuses only on a disagreement, naming the line. It never asks
   the person to decide twice. CLI !455 (privacy) already removes secret acceptance and scanner
   types from push; the rest needs the privacy and backend owners, and this RFC only fixes what
   people see.

5. **Push asks nothing that belongs elsewhere.** The device-local RC repository is configured by
   `agit rc` and never comes up in `agit push`. A Hub reached at a new address is the same Hub
   once it reports the same stable id (#105). Until then, an address mismatch names both
   addresses and gives one command to move.

6. **Errors end with a step in plain words.** A refusal names what it could not do and the exact
   command that does it. The front door already holds itself to this with a vocabulary test, which
   extends to every command's human output (rule 8 of the migration below).

## Command by command

| Git verb | agit today | Proposed |
| --- | --- | --- |
| `git init` | creates an agent repository with `memory/`, `skills/` and `AGENTS.md`, and binds the folder; new conversations are still not saved | the same as `agit on`: join the team's repository or create a private one, bind the folder, upload by itself. Scaffolding becomes opt-in (`--scaffold`), matching the decision that README and deliverables are not required on main |
| `git clone` | fetches; `run` or `resume` starts the conversation | unchanged; `agit clone owner/repo@branch` also prepares that conversation to continue, as `agit continue --no-launch` does |
| `git status` | identity, adopted sessions, sync state | the folder: on or off, its repository, recent conversations and whether each is uploaded (today's bare `agit` overview). `--verbose` adds today's identity and sync details |
| `git commit` | "settle" a named session | saves the current conversation's new turns locally (rule 1). Rarely needed when the folder is on |
| `git commit && git push` | (none) | `agit save` |
| `git push` | needs an explicit target, asks RC and visibility questions | pushes the current conversation to the folder's repository (rules 1, 3, 5) |
| `git fetch`, `git pull` | already Git-like | unchanged |
| `git log`, `git show`, `git diff` | need a target | default to the current conversation (rule 1) |
| `git branch` | list, rename, remove, seal | unchanged; "seal" is shown as "hide" |
| `git switch BRANCH` | `agit resume BRANCH` | adds `agit switch` as an alias of `resume` |
| `git switch -c NEW` | `agit fork` | adds `agit switch -c NEW [FROM]` as an alias of `fork` |
| `git switch --orphan` | `agit new` | unchanged |
| `git merge`, `cherry-pick`, `revert`, `tag` | already Git-like; merge is agent-driven | unchanged |
| `gh repo`, `gh pr` | `agit repo`, `agit pr` | unchanged |
| (none) | `import`, `project bind/sync/status/unbind/drop`, `run` | covered by `save`, `on`/`off`, `status` and `continue`; listed under `agit help advanced` |
| (none) | `file`, `memory`, `distill`, `view`, `export`, `privacy`, `secrets`, `rc`, `doctor` | unchanged; advanced |

## What the front door already does

The `agit on`/`save`/`share`/`continue` stack (CLI !446 and the save, share and help MRs after it):

- infers the repository from the folder and its Git remote, and joins the team's repository when
  one exists;
- names sessions from their first prompt;
- defaults the conversation to the one the command runs in;
- shows the advanced commands only under `agit help advanced`;
- holds its errors to plain words and one next step;
- gives agents one JSON document with `next`;
- asks before uploading a folder's earlier conversations, and says how many.

This RFC applies the same rules to the commands people already know from Git.

## Migration

Each step is one merge request, verified with real Claude Code and Codex sessions on the Sim Hub.
Steps 1, 2, 6, 7 and 8 come after 0.2.28; steps 3, 4 and 5 wait for the reshaped command surface.

1. **Default targets** (rule 1) for commit, push, log, show, diff, scan, memory, branch and tag.
   Explicit targets behave exactly as before.
2. **Enter-required confirmations, asked once** (rule 3, #108), with nothing left behind on cancel.
3. **`agit status` becomes the folder overview**; today's output moves to `--verbose`.
4. **`agit init` behaves as `agit on`**; the old scaffolding is behind `--scaffold`. A folder bound
   the old way says once that new conversations there are not saved by themselves, and that
   `agit on` makes them.
5. **Aliases** `agit switch` and `agit switch -c`.
6. **Push asks nothing that belongs elsewhere** (rule 5). This lands with #105's stable Hub id; until
   then, an address mismatch names both addresses.
7. **One secret check per upload** (rule 4), after !455, with the privacy and backend owners.
8. **Vocabulary test for every command**: human output must not contain claim, file line, VIEW,
   settlement, publication mode, RC repository or pinned identity, outside `--verbose` and the
   advanced commands.

Compatibility:

- The JSON contract (schema v2) does not change for existing commands; new fields are additive.
- Old forms keep working. For one release they print a one-line hint naming the new form.
- The skill, `AGENTS.md` section and Hub docs teach only the new forms from the release that ships
  them. The Hub quickstart switches to `agit save` and `agit share` in the same release.
- A command is removed only after telemetry shows it under 1% of use for two releases, and never
  within 0.2.

## How we will know

- **Sim friction per person**: typed commands, failed commands, help views, minutes and commands
  before the first upload and the first share. Run as an A/B round against the release before.
- **Autosave rate**: conversations on the Hub with no manual save, divided by all conversations.
- **Telemetry**: the share of commands that refuse for a missing target (expected to fall to near
  zero after step 1), and the use of advanced commands.
- **Fragmentation**: conversations of one team that land in more than one repository.
