---
name: agit-project
description: Bind an explicitly selected project directory, import its history, and opt into hook capture.
---

# agit project

Use only when the user explicitly requests project enrollment or synchronization.
Project enrollment is consent for the selected directory and repository, not permission
to collect every session on the machine. Ordinary session commands still require an
explicit session target; a project binding never selects a branch for them.

```bash
agit project bind /work/app --repo team/app --history all --auto-upload --dry-run
agit project bind /work/app --repo team/app --history all --auto-upload
agit project bind /work/new-app --repo team/new-app --history none --auto-upload
agit project bind /work/app --repo team/other-app --history none --rebind
agit project sync /work/app --dry-run
agit project sync /work/app
agit project status /work/app
agit project unbind /work/app
agit project drop                       # the conversation this command runs in
agit project drop team/app@project-claude-code-0123456789abcdef01234567
```

Run configuration from the device's terminal, outside an RC supervisor. The repository
must already exist and grant the current account write access. Binding clones it locally
without changing project code. Review the proposed source directory, destination and
publication policy before confirming; unattended use requires explicit `--yes`.

A directory routes to one repository. Binding it to another repository than its workspace
binding (from `init` or `clone`) or its project policy names is refused; `--rebind` moves it,
replacing both. The previous repository's history is left unchanged.

`--history all` imports and publishes native Codex and Claude Code sessions discovered
under the directory, including subdirectories without their own binding. Unclaimed
sessions start independent histories; existing claims in the same repository retain
their branch. A session claimed by another repository, including a desktop RC project,
keeps its source authority: `sync` reports a refusal before importing or publishing it elsewhere.
Publish those sessions from their source repository. Archived or superseded claims are reported
as failures. `--history none` excludes sessions already present at enrollment unless this
repository claims them; it does not silently publish their old content when they are
resumed. Rebind with `--history all` to include them.

`--auto-upload` installs available runtime hooks, confirms the current publication policy,
and enables the existing **repository-wide** `push.auto` preference. Other managed
sessions in that local repository inherit that preference. Global preferences are not
changed. New imports record the importing account as creator and default to plain publication.
Existing claims retain their session owner and registered settings. Codex hooks must be supported,
enabled and trusted in the installed runtime; a Codex without it does not block Claude Code capture,
and its sessions are published by `project sync`. Binding fails only when no runtime can
provide hooks.

The Stop hook hands the completed conversation to a background process and returns, so the
turn is not held by import or upload. That process imports/saves it through the ordinary
import path, then publishes with account, destination and registered-session authorization checks.
Dictionary synchronization is best effort; publication still validates source integrity and the
snapshot privacy contract. There is no resident retry queue. A failed push keeps
local history, and `status` shows the last result. Retry with `project sync`.
Changing the selected account or destination requires renewed project binding. RC-owned sessions
retain their supervisor lifecycle and source publication authority.

Hooks adopt only a conversation that started inside the bound folder. Every push, automatic
or manual, checks that rule again together with the project's exclusions, and refuses a
project branch that breaks it whatever its claim says; agit drops such conversations on its
own and says once which. `drop` stops uploading a conversation from this computer: the named
one, or without a target the conversation it runs in. It stops recording into its branch and
the project no longer captures it; its local history stays. A copy already on the Hub stays
there. Run `drop` only when the user asks for it.

`status` reads the local policy and last sync result. It does not claim Hub indexing is
complete. `unbind` pauses project hook capture and automatic publication for the selected
project without deleting history or changing repository preferences. Explicit manual
publication remains possible. Nested project bindings remain independent.
