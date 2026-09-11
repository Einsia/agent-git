---
name: agit-file
description: Manage deliverables and ordinary files in an explicitly selected branch worktree.
---

# agit file

Use `AGIT_SESSION=owner/repo@branch`, or the global `--into owner/repo@branch`
option. Each session branch has its own worktree and index. This command does not
touch the project code repository.

| Command | Behavior |
|---|---|
| `cwd` | Print the file worktree's absolute directory |
| `add <source>... [--to <path>]` | Snapshot files into staging; external sources are copied to `artifacts/<name>` |
| `status` | Show staged and unstaged changes |
| `diff [--staged] [path...]` | Review working changes or the staged snapshot |
| `commit -m <message>` | Commit the staged snapshot without settling conversation turns |
| `list [--ref <ref>]` | List ordinary files at a committed version |
| `get <path> --output <local-path> [--ref <ref>]` | Copy committed bytes to a local file |
| `rm [--cached] <path>...` | Stage deletion; `--cached` keeps the working copy |
| `mv <source> <destination>` | Move within the worktree and stage the change |
| `restore --staged <path>...` | Unstage while preserving the working copy |
| `link <path> [--ref <ref>]` | Print a Hub permalink pinned to a commit hash |

`add` resolves source paths from the invoking directory. Paths already inside the
file worktree retain their relative locations; `add .` from that worktree stages
ordinary tracked and unignored files, including deletions. `--to` accepts one
source and a destination relative to the file worktree. Directories are copied
recursively. Storage paths, path traversal and symlinks are refused. Other
commands take paths relative to the file worktree.

```bash
agit file --into alice/research@report cwd
agit file --into alice/research@report add /absolute/path/report.pdf
agit file --into alice/research@report commit -m "Deliver the research report"
agit file --into alice/research@report link artifacts/report.pdf
```

Editing a file after `add` does not change the staged version. Call `add` again to
replace that snapshot. Turn settlement and automatic memory collection do not
consume manually staged files. The legacy `agit commit -m` file mode stages
working changes itself; use `agit file commit` when explicit staging matters.

An empty staged snapshot creates no commit. A file commit preserves the VIEW,
transcript and turn number. It remains local until the branch is pushed. A link
exists on the Hub only after that commit has been published.
Permalinks use this repository's current `origin`, including its Hub mount path,
even when the globally selected Hub differs. A missing origin or an origin that
does not match the selected repository is refused; publish or correct that
repository's remote first.
