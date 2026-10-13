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

Developers publish using their own accounts. New sessions default to encryption for the session
creator's account. Repository membership permits published-content access; original content also
requires eligible default or explicit access. New sessions use session-level grants: the
session owner can authorize current members
access with `agit privacy access grant MANAGER/project-name@SESSION_BRANCH` after reviewing the
session scope and frozen member list. This covers existing and future commits until
`agit privacy access disable-session` switches future explicit grants to individual commits.

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
repository, including a desktop RC project, keeps its source authority; sync reports a refusal
before importing or publishing it elsewhere. Publish that session from its source repository.
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
hooks cannot provide automatic capture; its sessions are published by `project sync`, and
binding with `--auto-upload` fails only when no runtime can provide hooks.

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

Uploads preserve identity and authorization checks. Dictionary synchronization is best effort;
publication still validates source integrity and the snapshot privacy contract. A failed upload
retains the local imported history. There is no resident retry service in this minimal
version: fix access or connectivity and rerun sync. The Stop hook starts the upload in a
background process and returns at once; `agit project status` shows its last result.
Automatic uploads recheck account, immutable destination, write access and registered session
settings. Changing the project's selected account or destination requires renewed binding.

Unbind stops this project's hook capture and automatic pushing, without deleting existing
history or resetting repository preferences. Explicit manual push is still possible. Nested
project bindings are independent. Already in-flight operations finish before unbind obtains
the project lock; the command reports a lock error if they do not finish in time.

RC retains its own supervisor and publishing lifecycle. RC-owned sessions keep their claims and
source publication authority.
