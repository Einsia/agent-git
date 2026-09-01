---
name: agit-repo
description: Manage local and Hub Agent repos.
---

# agit repo

## Purpose

Low-frequency Agent repo administration. `init` creates a local repo. `repo create` currently creates a remote repo on the Hub and prints follow-up `init`/`push` guidance; it does not create a session branch. The first `push` may also create a remote, so these operations are distinct.

## Synopsis

```bash
agit repo <subcommand>
```

## Subcommands

| Subcommand | Purpose | Main options |
|---|---|---|
| `create <name>` | Create a remote repo on the Hub | `--private` |
| `list` | List local repos | `--remote` lists visible Hub repos |
| `info [repo]` | Show repo details | Context when omitted |
| `visibility <repo> <public|private>` | Change visibility | Making private public triggers a server scan |
| `collab add <repo> <user> [--role read|write]` | Add a collaborator | Default role `read` |
| `collab rm <repo> <user>` | Remove a collaborator | — |
| `collab list <repo>` | List collaborators | — |
| `rename <repo> <new-name>` | Rename a remote repo | — |
| `delete <repo>` | Delete a remote repo | `--local` deletes only the local copy |
| `path [repo]` | Print the local repo path: the main checkout, or `<owner/repo>@<branch>` for that session branch’s worktree (created on demand; `@` = the current session) | Context when omitted |

## Examples

```bash
agit repo create notes --private
agit repo list --remote
agit repo info szh/p1
agit repo path szh/p1
agit repo path szh/p1@refund-fix   # that branch’s worktree; cd there to edit its memory/
agit repo visibility szh/p1 public
agit repo collab add szh/p1 alice --role write
agit repo delete szh/p1 --local
```

`repo create`, `init`, and first `push` overlap only in remote creation. Use `new`, `import`, or `fork` when a session branch is needed.
