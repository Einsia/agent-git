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
agit project sync /work/app --dry-run
agit project sync /work/app
agit project status /work/app
agit project unbind /work/app
```

Run configuration from the device's terminal, outside an RC supervisor. The repository
must already exist and grant the current account write access. Binding clones it locally
without changing project code. Review the proposed source directory, destination and
publication policy before confirming; unattended use requires explicit `--yes`.

`--history all` imports and publishes native Codex and Claude Code sessions discovered
under the directory, including subdirectories without their own binding. Unclaimed
sessions start independent histories; existing claims in the same repository retain
their branch. Conflicting or protected claims are not reassigned. `--history none`
excludes unclaimed sessions already present at enrollment; it does not silently publish
their old content when they are resumed. Rebind with `--history all` to include them.

`--auto-upload` installs available runtime hooks, confirms the current publication policy,
and enables the existing **repository-wide** `push.auto` preference. Other managed
sessions in that local repository inherit that preference. Global preferences are not
changed. An encrypted repository must have its viewing key initialized by its owner;
never request the viewing password in chat. Codex hooks must be supported, enabled and
trusted in the installed runtime. Unsupported hook setup is an error, not success.

The Stop hook imports/saves the completed conversation through the ordinary import path,
then publishes through existing identity and privacy gates, plus consent for an encrypted
repository. Upload is synchronous;
there is no resident retry queue. A failed push keeps local history. Retry with `project sync`.
For an encrypted repository, policy, account, recipient or destination changes require review
and renewed binding consent; an ordinary repository's automatic uploads follow `push.auto` alone.
RC-owned sessions keep their supervisor lifecycle and are not taken over by this command.

`status` reads the local policy and last sync result. It does not claim Hub indexing is
complete. `unbind` pauses project hook capture and automatic publication for the selected
project without deleting history or changing repository preferences. Explicit manual
publication remains possible. Nested project bindings remain independent.
