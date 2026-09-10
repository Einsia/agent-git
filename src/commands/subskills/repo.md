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
agit repo delete szh/p1 --local
```

`repo create`, `init`, and first `push` overlap only in remote creation. Use `new`, `import`, or `fork` when a session branch is needed.
