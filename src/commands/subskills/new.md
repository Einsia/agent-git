---
name: agit-new
description: Create an empty session branch in an Agent repo and optionally start a runtime.
---

# agit new

## Purpose

Create a real session branch with an empty VIEW, inheriting shared files from a file line. The repo selects the context repository and the branch selects the session identity. An omitted repo requires `AGIT_SESSION`; workspace binding cannot select it.

An explicitly named `owner/repo` that is missing locally is cloned read-only before the session is created. This preserves the remote repository identity and does not bind the working directory. Existing local repositories remain usable offline; omitted repos require `AGIT_SESSION` and remain local; bare repository names must uniquely match a local checkout.

## Synopsis

```bash
agit new [REPO] -b <branch> [options]
```

With no repo and no `-b` at a terminal, this opens a repo picker and then asks for the branch name on the normal screen. It stays non-interactive in pipes, in CI, and inside an agent session. It also does not open inside an unmanaged runtime session, where `new` is refused anyway unless `--fresh`.

## Options

| Option | Meaning |
|---|---|
| `[REPO]` | `<owner/name>` or `<owner/name>@<file-ref>`; when omitted, `AGIT_SESSION` must name the repo |
| `-b, --branch <branch>` | New session branch name |
| `--from <ref>` | Inherit shared files from a file-line ref, defaulting to `main`; the session context stays empty |
| `--as <runtime>` | Runtime to start |
| `--cwd <dir>` | Runtime working directory |
| `--no-launch` | Create/materialize without starting |
| `--fresh` | Explicitly start empty inside an unmanaged runtime session; otherwise import the current conversation first |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface. `--tui` overrides the agent-session check but not `--json` / `-q` / `-y` |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit new szh/p1 -b feature-a
agit new szh/p1 -b review --from main --no-launch
agit new szh/p1 -b codex-fix --as codex --cwd ~/Projects/p1
```

Verify immediately:

```bash
git -C "$(agit repo path szh/p1)" show-ref --verify refs/heads/feature-a
```

Use `agit init` to create a repository that does not exist on the Hub. A failed remote lookup or clone does not create a session or start a runtime. Do not use `agit push` as a substitute for `new`.

## Choose an Agent repo before starting a session

Before creating or importing a session, run this in the target workspace:

```bash
agit status
```

Follow these rules in order. Never guess from a directory name, the first repo in a list, or the repo used last time:

1. A persisted `bound repo` records the workspace’s intended repo for session creation. Name that repo explicitly in `new` or `import`; the binding never selects an existing session branch. Do not run `agit init` merely because this is a new session.
2. Adopted session links are discovery results, not default targets. If no destination repo has been selected, ask the user to choose or use an interactive picker; never infer it from the sole link or a native runtime ID.
3. If no suitable Agent repo exists, run `agit init` first, then create the session branch.
4. If the user has named an existing Agent repo, always reuse it; this takes precedence over directory state and session-link ambiguity. If it is not local, run `agit clone <owner/repo>` first, not `agit init`.
5. Organization repos (`<org>/<name>`) accept `import`, `commit` and `push` from whoever the Hub lets push to that repo (org owners, every member while the organization's base permission is write, which is the default, and people granted write on that repo); an org owner may also import into a repo that does not exist yet — the first push creates it under the org. The CLI asks the Hub before importing or pushing, so a refusal names the real reason. The checkout lives under `~/.agit/repos/<org>/<name>`, and versions are authored by the signed-in account. Organization repos support public and private visibility; a first publish without a visibility preference or interactive confirmation defaults to private. Write owner names in lowercase. Do not `clone --mine` a copy just because the owner is not the user.

Reusing a repo normally means creating another session branch in that same Agent repo. A workspace binding only routes a directory to a repo; it does not create a branch. After creating or importing a session, verify the real Git ref:

```bash
git -C "$(agit repo path <owner/repo>)" \
  show-ref --verify "refs/heads/<branch>"
```

Name constraint: a **branch** name must not begin with `agit-`. That prefix is reserved for AgentGit version IDs (for example, `agit-<40-hex>`), because `owner/repo@<ref>` parsing uses it to distinguish a version ID from a branch name. Repo names are not subject to this rule (`hachi/agit-dev` is fine); `new` / `fork` / `import -b` reject such branch names locally before anything is created.
