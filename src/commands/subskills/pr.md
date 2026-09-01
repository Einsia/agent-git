---
name: agit-pr
description: Create, inspect, fetch, and merge pull requests between Agent repos.
---

# agit pr

## Subcommands

| Subcommand | Purpose | Arguments |
|---|---|---|
| `create <target>` | Open a PR from your forked branch | `-b/--branch <your-branch>` required, `-m/--message` |
| `list <repo>` | List a repo's PRs | `<repo>` |
| `show <id>` | Show a PR | `<id>` |
| `fetch <id>` | Fetch proposal commits into local `pr/<id>` for review | `<id>` |
| `merge <id>` | Approve and land a PR without starting a merge agent | `<id>`; `--adopt <new-branch>` adopts the contributor branch |

## Synopsis

```bash
agit pr create <owner/repo> -b <your-branch> -m "description"
agit pr list <owner/repo>
agit pr show <id>
agit pr fetch <id>
agit pr merge <id> [--adopt <new-branch>]
```

All `pr` commands support common `-y/--yes`, `-q/--quiet`, `-C/--directory`, and `--no-color` options. Global `--json` emits the unified CLI JSON envelope.

## Example

```bash
agit push szh/p1 -b feature-a
agit pr create alice/p1 -b feature-a -m "Implementation and tests"
agit pr fetch 42
agit diff pr/42
agit pr merge 42
```

`pr merge` is a remote approval/landing action. Use `agit merge` when turns must be selected by intent.
