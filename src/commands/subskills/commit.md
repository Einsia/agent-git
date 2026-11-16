---
name: agit-commit
description: Settle the current turn or selected file changes as an Agent repo commit.
---

# agit commit

## Purpose

Record one turn on the current session branch. A normal commit writes to `~/.agit/repos/...`; only `--code` also commits the project code Git repo and creates a cross-link.

## Synopsis

```bash
agit commit [target] [options] [-- <path>...]
```

## Options

| Option | Meaning |
|---|---|
| `[target]` | Explicit owner/repo@branch or adopted native session ID; omitted targets and `@` require `AGIT_SESSION` |
| `[-- <path>...]` | Limit a code commit or a file-line commit to these paths |
| `--milestone <summary>` | Mark a completed phase with a short summary. Given while the turn is running, it goes on that turn when the turn is saved; with nothing new to save, it names the last saved turn |
| `--tag <name>` | Tag the milestone's turn, only when the user asks for a tag; `agit push` publishes it on the turn's public commit |
| `--code` | Also commit the code repo of the current directory (run it from the checkout you edited) and cross-link both commits; stages tracked changes and what is already staged, refuses while the checkout has untracked files (`git add` yours first); outside Git, warn and settle the session turn without the code side commit |
| `-m, --message <message>` | Message for a file-only commit; new turns usually derive their message from the transcript |
| `-n, --name <agent>` | Legacy native-ID compatibility: choose the repo name within the link's recorded owner; an ownerless link must be explicitly imported first |
| `-b, --branch <branch>` | Legacy native-ID compatibility with a recorded owner: choose the session branch; `main` is refused for turn settlement |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

Workspace bindings and runtime discovery do not select a branch. Use a complete
`<owner/repo>@<branch>` when `AGIT_SESSION` is absent, including for a file commit
on `main`. A native session ID must already have an explicit adoption claim.

## Examples

```bash
agit commit szh/p1@fix-auth --json
agit commit szh/p1@fix-auth --milestone "Phase one passed tests"
agit commit szh/p1@fix-auth --milestone "Phase one passed tests" --tag ms-auth   # the user asked for a tag
agit commit szh/p1@fix-auth --code -- src/auth.rs       # also commit the code repo, scoped to one path
agit commit <owner/repo>@main -m "docs: add README" -- README.md
agit commit <session-id> -n photo -b fix-auth           # requires a recorded repository owner
```

An offline `agit import <session-id> --link-only` record has no repository owner or branch claim.
After sign-in, use `agit import <session-id> --from <runtime> --into <owner/repo>@<branch>` to choose
lineage, establish that identity and record its opening version. Neither `-n` nor the signed-in
account supplies a missing owner.

Supervisors/hooks may settle each user turn, but the CLI does not promise an automatic push. Use `agit push` when the history must be shared.

## File commits on the file line

`main` is the file line (README.md, AGENTS.md, `memory/`, `skills/`); it never has a session link, and `-m` is the only kind of commit it takes. Edit the files in the Agent repo checkout (`agit repo path <owner/repo>`), then:

```bash
agit commit <owner/repo>@main -m "memory: refund decision" -- memory/refund.md
agit push <owner/repo> -b main
```

Staging is done for you: without `-- <path>...` every change in the checkout goes in (`git add -A`), and AgentGit storage paths are always excluded. `--milestone`, `--tag` and `--code` do not apply to a file commit. On a session branch, `-m` is refused while new turns are pending — settle them first.

## Turns are atomic

A turn is settled only once it has ended. A turn whose tool call has not returned yet — including the `agit commit` you are running right now from inside the turn — stays in the runtime and settles on the next `agit commit` (the Stop hook does this). So an `agit commit && agit push` issued mid-turn publishes history up to the previous turn; the current turn reaches the Hub with the next push.

## Shared files on the file line

`main` is the file line: it never carries a session, and it is where README.md, AGENTS.md, `memory/` and `skills/` live. Everything `agit new` inherits and everything teammates see when they `agit clone` comes from here. Updating it is a file commit — no `git add`, no session link:

```bash
cd "$(agit repo path <owner/repo>)"                  # the Agent repo checkout: a plain Git worktree
$EDITOR README.md memory/decisions.md                  # edit or add shared files
agit commit <owner/repo>@main -m "docs: describe the repo"
agit push <owner/repo> -b main --dry-run
agit push <owner/repo> -b main --yes                    # follow the dry-run decision above
```

- `-m` on the file line is always a pure file commit; `--milestone`, `--tag` and `--code` belong to turn commits.
- Without `-- <path>...` every change in the checkout is staged (`git add -A`); add `-- README.md` to limit the commit. AgentGit storage paths (`session/`, `LOG`, `VIEW`, `events/`) are excluded automatically.
- On a session branch `-m` is legal only while no new turns are pending; settle turns with `agit commit` first.
- Use `agit commit <owner/repo>@main -m "..."` to select the file line explicitly. The shorter `agit commit main -m "..."` requires `AGIT_SESSION` to supply the repo; a workspace binding does not select it.
- To write README.md for a repo the user names (for example "add a README to hachi/agit-dev"), use exactly this flow; do not `git commit` inside `~/.agit/repos` by hand.

## Milestones

- One user turn normally becomes one AgentGit commit. When a phase genuinely completes (working feature and passing tests), settle it:

  ```bash
  agit commit <owner/repo>@<branch> --milestone "short phase summary" --code
  ```

  Run it from inside the turn that completes the phase: the milestone waits on the session and goes on this turn when the turn is saved. With nothing new left to save, it names the last saved turn. Add `--tag <name>` only when the user asks for a tag. A tag on a session turn takes no `-m`; `agit push` publishes it on the turn's public commit, or names it and says why not.
