---
name: agit
description: "Use AgentGit when the user requests AgentGit operations, such as saving, uploading or sharing this conversation as a link, or the current session has an explicit AgentGit identity. Save, resume, inspect, share, or publish session history. Unrelated tasks do not need AgentGit checks."
---

# AgentGit overview

## When to use

Use this workflow when the user requests an AgentGit operation, or when
`AGIT_SESSION` or `AGIT_MERGE_TX` explicitly identifies the current managed session.
Saving, uploading or sharing this conversation, a read-only link to it included,
is an AgentGit operation.
An installed skill, a workspace binding, or a generic instruction in an ancestor
`AGENTS.md` does not by itself make an unrelated task an AgentGit operation.
For ordinary questions, device troubleshooting, browsing, or unmanaged project
work, continue the user's task without running agit. Do not run `agit status` or
scan transcripts merely to decide whether this skill applies. Reading this skill
for review or editing does not activate its session-management workflow.

## Save, upload and check a conversation (start here)

For Claude Code and Codex conversations these commands cover what users ask for. Each one finds
the conversation, its folder's destination and the account by itself, so do not build `import`,
`commit` or `push` commands for these requests.

| The user asks to… | Run |
|---|---|
| save, upload or back up this conversation, or get its link | `agit save --json` |
| upload all of today's sessions | `agit save --today --json` |
| upload the sessions that ran in this folder earlier | `agit save --all --json` |
| save every future session in this folder automatically | `agit on --json` |
| stop that for this folder | `agit off --json` |
| see what is saved here | `agit --json` |
| share a read-only copy of this conversation | `agit share --yes` |

- `agit save` run inside a conversation saves that conversation and turns its folder on, so later
  turns upload after each turn without any command. Send the user `result.value.sessions[].link`.
- When a push's output ends with a line that starts "For the agent:", ask the user once whether
  the folder should upload by itself as it describes, then wait for their next message. Run the
  `agit on` command it names only on a yes; anything else, or no answer, is no: run `agit off` in
  that folder, and don't ask again.
- When a save turned a folder on (`result.value.folders_turned_on[]`, each with `folder` and
  `repository`), tell the user, with the link, that new sessions in that folder now upload to
  that repository by themselves, and ask once whether they should keep doing so, naming each
  folder when there are several (`result.value.next.ask_user` has the question). If the user
  says no, run `agit off --json <folder>` for that folder (its `if_no`): new sessions there
  stay on this computer, and you do not ask again. Saving there again needs the folder turned
  back on (`agit on --json`), which makes its new sessions upload by themselves again, so run
  that only when the user asks for it.
- Exit code `8` with `result.value.sign_in_url` means the user must sign in: send them the link,
  wait until they say they approved it, then run the same command again. It finishes the sign-in
  by itself; do not run `agit login --complete`.
- `status` `pending` means this conversation's first reply has not finished; it uploads by itself
  when the reply ends, and `link` shows it then.
- A refusal says what happened and ends with the next step (`diagnostics.stderr`, level `hint`).
  Follow that step or report it to the user.
- Uploads go to a private destination. A destination is public only when the user names it with
  `--repo OWNER/NAME --public`.
- Create a session link, invite link or read-only copy only when the person asks for one; never
  create one on your own to finish a task.
- In a folder that is on, never run `agit commit` or `agit push` for its conversations.

Everything else is in `references/commands/<command>.md`; the table at the end says which to read.
To upload a running conversation, adopt it with `agit import` (`import.md`):
`agit new` cannot take over the session that is already running; it starts a new, empty one.

## Rules for every command

- Read `--json` output as an envelope: check `ok` and `exit_code`; data is in `result.value`,
  errors and the next step in `diagnostics.stderr`. When a command fails, follow its `hint`; do
  not guess other commands or run diagnostics first.
- An existing session is selected only by an explicit `<owner/repo>@<branch>` or `AGIT_SESSION`
  (`@`), except that `agit save` and a bare `agit share` take the running conversation. A
  workspace binding or native runtime ID never selects one. The project's `.git` is not the
  Agent repo under `~/.agit/repos`.
- agit never waits for an answer in an agent session: it takes the documented default and says
  which, or fails naming the flag that answers. `--yes` only answers confirmations.
- Push only when the user asked to upload. A new repository is private unless the user says
  public; pass `--yes` for a public destination only after the person confirmed it here.
- Never delete files under `~/.agit/store` or any `*.lock` file. Never rewrite AgentGit history
  with rebase, amend, force-push or `git checkout`.
- Read only the reference you need; `agit <command> --help` shows exact options.

## Pick the command

| Goal | Command | Read |
|---|---|---|
| Save, upload or share this Claude Code or Codex conversation | `agit save --json` | `save.md`, `on.md`, `share.md` |
| Upload another runtime's conversation, or to a named branch | `agit import … --independent`, then `agit push … --private --yes` | `import.md` |
| Create the first Agent repo | `agit init <name> --auto-push=<true\|false>` | `init.md` |
| Start an empty session (pick its repo first) | `agit new <owner/repo> -b <branch>` | `new.md` |
| Continue a session, open or branch from a saved point | `agit resume`, `agit run`, `agit fork` | `resume.md`, `run.md`, `fork.md` |
| Save completed turns, mark a milestone | `agit commit <owner/repo>@<branch>` | `commit.md` |
| Edit shared files on `main` (README, AGENTS.md, memory/, skills/) | `agit commit <owner/repo>@main -m "<msg>"` | `commit.md` |
| Deliver documents, slides, images, videos | `agit file add`, `commit`, `link` | `file.md` |
| Publish saved history | `agit push <owner/repo>@<branch> --yes` (`--dry-run` to preview) | `push.md` |
| Take an upload down | rotate exposed secrets, withdraw on its Hub page | `push.md` |
| Find how something was done before | `agit search "<words>" --scope org --json` | `search.md` |
| Sign in | `agit login --json` | `login.md` |
| Use another project folder in Hub Workspaces or Feishu RC | Start a new conversation and choose the folder | `rc.md` |
| Connect a device for remote control | `agit rc start --detach` | `rc.md` |
| Encryption, privacy policy, secrets | `agit privacy`, `agit secrets` | `privacy.md`, `secrets.md` |
| Targets, `AGIT_SESSION`, folder state | `agit status --json` | `status.md` |
| Memory between sessions | `agit memory status`, `agit distill` | `memory.md` |
| Act as the merge agent (`AGIT_MERGE_TX`) | `agit merge` | `merge.md` |
| Questions from other members' agents | MCP `agent_inbox`, `agent_reply` | `mcp.md` |
| Install skill, hooks or MCP | `agit setup` | `setup.md` |
| All commands by group; the model; conventions | | `overview.md` |
