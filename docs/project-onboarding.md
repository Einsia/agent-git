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

Developers publish using their own accounts. New sessions default to plain publication. To encrypt a session,
select `agit privacy mode MANAGER/project-name@SESSION_BRANCH encrypted` before first registration.
Repository membership permits published-content access; encrypted originals also require eligible
default or explicit access. Encrypted sessions use session-level grants: the
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

`--auto-upload` turns on automatic push for the whole repository: every managed session in
this local repository uploads after each turn, not only the folder's. The dry run says so in
`repository_auto_push` and `repository_auto_push_scope`. The command does not alter global
preferences.
Do not use `--yes` before reviewing these choices; it explicitly accepts the confirmations.

## Developer: enroll a new project

Use the same command with `--history none`:

```sh
agit project bind /work/new-project --repo MANAGER/project-name --history none --auto-upload
```

Existing sessions are excluded unless this repository already claims them. Newly created
sessions are adopted and uploaded
at the end of their first completed turn; later turns update that same branch. Only a
conversation that started inside the folder is adopted: one started elsewhere that later moves
into it is offered, never adopted or uploaded. Keep using
Claude Code or Codex normally. Rebind with `--history all` if you later want the old sessions.

Each new conversation in the folder opens with one line from agit, shown to you rather than
the agent, before anything is uploaded:

```text
agit: this conversation is uploaded to team/app (private to team: its owners and granted teams can read it) at the end of each turn. Not this one? Say so, or run `agit project drop`.
```

The line says who can read the repository: anyone for a public one; for a private one, the
organization's owners and granted teams, or only you and people you add when it is in your
own namespace. The visibility is the Hub's answer when the conversation starts, or its last
recorded answer when the Hub cannot be reached in time; when neither exists the line says so. A resumed
conversation that has not been uploaded yet shows the line again.

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
agit project drop [MANAGER/project-name@BRANCH]
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

Every push, automatic or manual, checks the project's capture rule again. A project branch whose
conversation started outside the folder, or whose session is excluded, is never pushed, whatever
its claim says. agit drops such conversations on its own and says once which ones it dropped. To
stop uploading a conversation yourself, run `agit project drop OWNER/REPO@BRANCH`, or
`agit project drop` inside the conversation: it stops recording into that branch and the project
no longer captures it; its local history stays, and it is never uploaded from this computer. A
copy already on the Hub stays there until you remove it on the Hub.

Unbind stops this project's hook capture and automatic pushing, without deleting existing
history. It puts the repository's `push.auto` back to what it was before the folder was bound;
if automatic push was already on then, it stays on and unbind says so. Explicit manual push is
still possible. Nested project bindings are independent. Already in-flight operations finish
before unbind obtains the project lock; the command reports a lock error if they do not finish
in time.

RC retains its own supervisor and publishing lifecycle. RC-owned sessions keep their claims and
source publication authority.
