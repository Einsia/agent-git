# Project onboarding for managers and developers

This workflow requires a build containing `agit project`. Check `agit project --help`;
the feature branch is not a published CLI release. Use a device terminal, not an RC
supervised session. Keep the binary at a stable path after installing its hooks.

## Manager: prepare the shared destination

Sign in as yourself. Create one private AgentGit repository per project, or use an
existing repository. This is a conversation repository, not your source-code Git repo.

```sh
agit login
agit repo create project-name --private
agit repo collab add MANAGER/project-name DEVELOPER --role write
```

Replace the uppercase names with actual account names. Repeat the grant for each developer.
The current `repo create` creates under the signed-in account; select an existing organization
repository through the website instead of passing an organization-qualified creation name.

For an encrypted repository, initialize its viewing key before developers publish:

```sh
agit privacy init MANAGER/project-name --browser --json
```

Open the returned setup URL, set the viewing password yourself, then repeat the command
to confirm readiness. Do not send the password or your login credentials to an agent.
Developers publish using their own accounts and the repository's public publishing key;
reading original private context may separately require the viewing password.

## Developer: enroll an existing project

Run on the machine where your native sessions are stored, using your own account:

```sh
agit login
agit project bind /work/project --repo MANAGER/project-name --history all --auto-upload --dry-run
agit project bind /work/project --repo MANAGER/project-name --history all --auto-upload
agit project status /work/project
```

Review the directory and candidate list first. Binding includes subdirectories except
directories with another explicit binding. Paths belong to native session metadata; session
files need not be stored inside the project. Each developer/machine can use a different path.

Confirm the destination, independent import of unclaimed histories, and publication policy.
Existing claims in the same repository keep their branch. A session claimed by another
repository, including a desktop RC project, keeps that claim; sync publishes a separate copy
of its branch to this repository (status `copied`), and later syncs refresh the copy.
Archived or superseded instances are not published; inspect reported failures.

`--auto-upload` uses the existing repository-wide `push.auto` preference. Other managed
sessions in this local repository inherit it. The command does not alter global preferences.
Do not use `--yes` before reviewing these choices; it explicitly accepts the confirmations.

## Developer: enroll a new project

Use the same command with `--history none`:

```sh
agit project bind /work/new-project --repo MANAGER/project-name --history none --auto-upload
```

Existing sessions are excluded unless this repository already claims them. Newly created
sessions are adopted and uploaded
at the end of their first completed turn; later turns update that same branch. Keep using
Claude Code or Codex normally. Rebind with `--history all` if you later want the old sessions.

The command installs hooks for available supported runtimes. Codex must report an enabled
hooks feature and may ask you to trust the installed commands. A runtime without working
hooks cannot provide automatic capture; omit `--auto-upload` and use manual sync instead.

## Verify, retry and pause

```sh
agit project status /work/project
agit project sync /work/project --dry-run
agit project sync /work/project
agit project unbind /work/project
```

Check that the Manager can open each developer's sessions in the shared repository.
Repeat sync to verify that no duplicate branches appear. The last result distinguishes
successful pushes and failures; a push does not prove that Hub search indexing has finished.

Uploads use existing privacy and identity gates. A failed upload retains the local imported
history. There is no resident retry service in this minimal version: fix access, connectivity
or privacy setup and rerun sync. Hooks can wait for scanning/upload; this is not token streaming.
For an encrypted repository, an account, repository identity, policy or recipient change
requires renewed explicit consent; an ordinary repository's automatic uploads follow `push.auto`
alone.

Unbind stops this project's hook capture and automatic pushing, without deleting existing
history or resetting repository preferences. Explicit manual push is still possible. Nested
project bindings are independent. Already in-flight operations finish before unbind obtains
the project lock; the command reports a lock error if they do not finish in time.

RC retains its own supervisor and publishing lifecycle. This command does not migrate or
take over RC-owned sessions; it publishes copies of them, refreshed by each sync. It also does not add project question-answering, a dashboard,
or a new web interface.
