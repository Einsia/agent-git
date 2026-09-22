---
name: agit-repo
description: Manage local and Hub Agent repos.
---

# agit repo

## Purpose

Low-frequency Agent repo administration. `init` creates a local repo. `repo create` creates a remote repo on the Hub and its empty local checkout with the remote identity and push URL; it does not create a session branch. The first `push` may also create a remote, so these operations are distinct.

## Synopsis

```bash
agit repo <subcommand>
```

## Subcommands

| Subcommand | Purpose | Main options |
|---|---|---|
| `create <name>` | Create a remote repo on the Hub | `--private` |
| `list` | List local repos | `--remote` lists visible Hub repos |
| `info [repo]` | Show repo details | An omitted repo requires `AGIT_SESSION` |
| `visibility <repo> <public|private>` | Change visibility | Making private public triggers a server scan |
| `collab add <repo> <user> [--role read|write]` | Add a collaborator | Default role `read` |
| `collab rm <repo> <user>` | Remove a collaborator | — |
| `collab list <repo>` | List collaborators | — |
| `invite <repo>[@<branch>] [-b <branch>] [--role read|write|owner]` | Print an invite link; with `@<branch>` (or `-b <branch>`) the invitee lands on that branch's session after accepting | Default role `read`; owners only; `@` selects the session in `AGIT_SESSION` |
| `rename <repo> <new-name>` | Rename a remote repo | — |
| `delete <repo>` | Delete a remote repo | `--local` deletes only the local copy |
| `path [repo]` | Print the main checkout for a bare repo, or `<owner/repo>@<branch>` for that session branch's worktree (created on demand); `@` selects the branch supplied through `AGIT_SESSION` | An omitted repo requires `AGIT_SESSION` and returns its main checkout |

## Examples

```bash
agit repo create notes --private
agit repo list --remote
agit repo info szh/p1
agit repo path szh/p1
agit repo path szh/p1@refund-fix   # that branch’s worktree; cd there to edit its memory/
agit repo visibility szh/p1 public
agit repo collab add szh/p1 alice --role write
agit repo invite szh/p1                        # anyone with the link joins as read
agit repo invite szh/p1@refund-fix --role write # lands on that branch's session after accepting
agit repo delete szh/p1 --local
```

## Invite links

`repo invite` needs the identity-pinned local checkout that `clone`, `repo create` or the first `push` leaves behind, and only an owner of the repository can create a link. The link does not expire and can be used more than once; whoever opens it and signs in joins with the chosen role. List and revoke links in the repository's settings page on the Hub (Invite by link). `--json` returns `url`, `role`, `repository`, `invitation_id` and `settings_url`.

With `@<branch>`, the session must already be on the Hub: the branch's last pushed or fetched head has to carry a settled session. A branch that was never pushed, the `main` file line, or a pushed head with no settled turn is refused before any link is created; publish it with `agit push <owner/repo>@<branch>` first. The link then also carries the session page, reported as `session_url` in `--json`.

`repo create`, `init`, and first `push` overlap only in remote creation. Use `new`, `import`, or `fork` when a session branch is needed.
