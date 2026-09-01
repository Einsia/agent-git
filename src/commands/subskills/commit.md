---
name: agit-commit
description: Settle the current turn or selected file changes as an Agent repo commit.
---

# agit commit

## Purpose

Record one turn on the current session branch. A normal commit writes to `~/.agit/repos/...`; only `--code` also commits the project code Git repo and creates a cross-link.

## Synopsis

```bash
agit commit [branch|@] [options] [-- <path>...]
```

## Options

| Option | Meaning |
|---|---|
| `[branch|@]` | Target branch; context resolution when omitted, `@` means the current branch |
| `[-- <path>...]` | Include only these paths in the code commit |
| `--milestone <summary>` | Mark a completed phase with a short summary |
| `--tag <name>` | Tag the commit |
| `--code` | Also commit the code repo and cross-link both commits; outside Git, warn and settle the session turn without the code side commit |
| `-m, --message <message>` | Message for a file-only commit; new turns usually derive their message from the transcript |
| `-n, --name <agent>` | Agent repo name when repo context is unavailable |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit commit                                             # settle the pending turns of the current session
agit commit @ --milestone "Phase one passed tests" --tag ms-auth
agit commit --code -- src/auth.rs                       # also commit the code repo, scoped to one path
agit commit <owner/repo>@main -m "docs: add README" -- README.md
```

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
