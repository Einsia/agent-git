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
