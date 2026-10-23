# Session publication mode and original access

## Choose an initial publication mode

New sessions default to encryption. Before the session's first remote registration, its creator
can explicitly choose plain publication:

```sh
agit privacy mode alice/project@work plain
agit privacy status alice/project@work
agit push alice/project@work --private
```

`plain` publishes the session without session encryption. Repository readers can read the
published history. Local secret protection and ordinary publication checks still apply.
`--private` and `--public` control repository visibility separately from session encryption.

The choice is local and follows the logical session across branch renames and multiple branches.
It covers all unpublished snapshots and future commits in that session. It does not create a
remote repository or register the session. Before registration, choose `encrypted` to restore
the encrypted default. Omit the mode argument to inspect the local choice and remote registration.
Changes require confirmation; use global `--yes` for an explicitly authorized unattended change.

The first push registers the selected mode. Once registered, it cannot change, even if that push
fails before uploading history. Original-access policy commands can also register an encrypted
session before its first push. A registered plain session uses repository read access instead
of original-content grants. `push --dry-run` still requires an existing registered session.

Existing local snapshots retain their source commits. Publication generates plain metadata and
mapped parents when their birth metadata declares encryption; event bytes and file content stay
unchanged. Accepted history retains its exact commits. Custom tags targeting rewritten commits
must be removed before publication or recreated on the accepted public commits.

## Original access

New sessions enable session-level explicit authorization by default. The immutable
session owner, while a current repository member, manages grants. Select a branch
or complete commit from a local checkout:

```sh
agit privacy access policy alice/project@work
agit privacy access show alice/project@work
agit privacy access grant alice/project@work
agit privacy access revoke alice/project@work --member bob
agit privacy access stop alice/project@work
agit privacy access disable-session alice/project@work
```

While the policy is `session`, `grant` confirms the current effective repository
members and gives that frozen recipient list access to complete originals of all
existing and future accepted commits in the logical session. Selecting a commit
still produces a session-wide confirmation. Later members are not added automatically.
Repository membership alone does not grant original access.

`disable-session` preserves current effective explicit grants on every already
accepted commit, across branches. Subsequent commits require their own explicit
grants. Local unpublished snapshots and prepared publications are excluded from
conversion. The Hub orders acceptance and disable atomically. Re-enabling session
authorization is unsupported.

Policy inspection and disable work before the first publication. The CLI registers
the immutable session configuration when needed, then reads or updates the remote
policy. The confirmed operation is reported as applied only after a successful
Hub response. Policy changes leave encryption configuration and published history
unchanged.

After disable, `grant`, `revoke`, `stop`, and `show` operate on the selected accepted
commit and its exact envelope. `show` reports the scope, policy and grant revisions,
explicit recipients, revocation and membership eligibility. Session inventories
report session grants; per-snapshot owner and committer defaults appear in commit
inventories and `privacy status`. Use an exact account ID when a username is ambiguous.

`revoke` suppresses the account's default and explicit access at the active scope.
`stop` removes explicit grants while preserving eligible defaults. Explicit regrant
can restore access, but grant-then-stop does not restore revoked defaults. Current
membership and the membership epoch still apply: departure invalidates grants and
rejoining does not restore them. Owner departure does not revoke other eligible
readers' access.

An independent fork has its own session policy and recipients. Resume, repeat
import, clone, branch rename, and repository copies preserve source authority.
Inherited originals remain governed by their source repository and session.

Changes require confirmation. Use the global `--yes` flag for an explicitly
authorized unattended operation and `--json` for structured results.

## Inspect storage and access

```sh
agit privacy status alice/project@work
```

Status resolves the selected local snapshot to its exact published commit. It reports original
storage separately from sanitized display processing: "Original saved encrypted; sanitized view
pending." It also reports your current original-read and grant-management capabilities, with
the grant policy, default and explicit access, session-grant provenance, revocation and
membership state when signed in. An unavailable
original does not mean that its ciphertext was lost. This command requests no content key.
Use `--json` for structured publication and capability fields.

## Retry an uncertain response

The CLI saves each confirmed request locally before submitting it. If the response
is unavailable, the error includes a request ID and a retry command:

```sh
agit privacy access retry alice/project REQUEST_ID
```

Retry uses the original account, immutable target, frozen recipients, policy and grant
revisions, and request ID. It does not resolve a moved branch or obtain a larger
member list. A policy change never silently converts a saved session request into
a commit request. A superseded request requires a fresh state read and confirmation.
A completed disable can be retried after later revocations without granting anyone
access again; its successful response may retain the existing revision. Retained requests are stored under the checkout's Git
directory and are not published with session history.

## Recover locally

An authorized reader can recover a published snapshot using their signed-in
account:

```sh
agit privacy unlock alice/project@work --workspace ./project
```

Recovery obtains the exact snapshot's content key and decrypts locally. Existing
recovered files remain local user data after cloud access is revoked.
