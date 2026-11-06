# Session publication mode and original access

## Publication mode

Every session publishes `plain`: repository readers can read the published history. Local secret
protection and the ordinary publication checks still apply, and `--private` and `--public` set
the repository's visibility, not the session's.

Encrypted publication is retired. `agit privacy mode <owner/repo>@<branch> encrypted` says so,
changes nothing and exits non-zero. Omit the mode argument to see the local choice and the
registered one:

```sh
agit privacy mode alice/project@work
```

A session that was published encrypted continues as plain at its next push, and the push says so
once. Its new turns upload only the redacted view people with access already see, and the
originals stay on this computer. Turns that were already published go up as they were published,
no wider, and its earlier encrypted snapshots stay as they are. Files it changed before this, such
as memory or skills, are not uploaded. People given access to its
originals keep that access for the earlier snapshots (`agit privacy access show` lists them). The
Hub must accept a client that continues an encrypted session; one that doesn't yet refuses the
push, and nothing is uploaded.

The same holds for any session whose history includes an encrypted snapshot, a fork or merge of
one: it publishes what was already published, or is refused, and then nothing is uploaded.

Existing local snapshots retain their source commits. Publication generates plain metadata and
mapped parents when their birth metadata declares encryption; event bytes and file content stay
unchanged. Accepted history retains its exact commits. Custom tags targeting rewritten commits
must be removed before publication or recreated on the accepted public commits.

## Deliver a bound repository to another Hub

A repository first registered on one Hub remains bound to that Hub for ordinary pushes. During a
deliberate migration, use the explicit destination flag with credentials already saved for the
destination:

```sh
agit push alice/project@work --to-hub https://agent-git.example
```

The command reviews and confirms the destination before publishing. It then replaces the local
immutable Hub and agent binding and updates `origin`. For later ordinary pushes, select that Hub
with `AGIT_HUB_URL` (or `agit config --global hub.url`). No destination credential is inferred
from `AGIT_HUB_URL`, and the flag cannot be combined with the device-local `--to` publication
target. Sign in to the destination first when needed:

```sh
AGIT_HUB_URL=https://agent-git.example agit login
```

This is an explicit migration operation; automatic and supervised publication cannot invoke it.
Use `--yes` when authorizing the migration from an agent or script. The target receives independent
plain session identities and newly protected session snapshots; encrypted source sessions are
refused. The source history remains local and the source Hub is unchanged.

Delivery requires evidence that the session creator is your signed-in account on the source Hub.
Keep that Hub's saved sign-in with an immutable account ID available on this device. A different
creator, a missing source account, or an unresolved creator stops the operation before any
destination session is registered. The Hub registration API does not yet support creating a
session owned by the delivering account without claiming that account as its creator.

Only your source committer account is mapped to your target account. Other source accounts are
omitted, and their Git signatures use `AgentGit <unmapped@agentgit.local>`. A Git author different
from the committer also remains unmapped. Account identities already belonging to the target Hub
stay on that Hub. The saved delivery belongs to its original target account; switching accounts
cannot silently change attribution.

This migration keeps the destination-specific session IDs and refuses encrypted history. Stable
plain session IDs, creatorless registrations, named Hub remotes, and selectively publishing
encrypted sessions from local plaintext remain separate design and protocol decisions.

Session and account mappings are retained privately under the checkout's Git common directory,
scoped to the target Hub and immutable repository identity. Keep this local state: after recording
more turns, an ordinary push to the target Hub continues the delivered session. A failed delivery
keeps the source Hub binding, and retrying the explicit command reuses the prepared target identity.

## Tool and attachment publication

Privacy projections show tool calls and results by default, and retain recorded attachments in
`original` mode. Configure exclusions yourself when you want to hide content.

**These controls apply to encrypted public views, `export --privacy`, and `share`. Ordinary plain
push keeps its existing publication path and local secret protection; it does not apply these
projection controls.** Session encryption does not by itself hide tools or attachments in the
public view. Repository visibility determines who can read that view.

Inspect the effective configuration or locate the device-local JSON file:

```sh
agit privacy policy show --repo alice/project
agit privacy policy file --repo alice/project
```

Memory collection requires an entry in your own allowlist for that repository, kept on this computer. A teammate's entries don't cover your files. The default list is empty:

```bash
agit privacy policy allow-memory decisions.md --repo alice/project
```

Use a filename relative to the memory directory, or an intentionally chosen pattern. Branch
and mandatory exclusions can still refuse it. `agit memory status` explains policy exclusions;
`agit memory sync` retries eligible files after authorization. Content privacy checks also apply.

The relevant fields are:

```json
{
  "tools": {
    "omit": false,
    "overrides": {}
  },
  "attachments": "original"
}
```

These fields also supply the defaults when an existing policy file omits them. Keep the other
fields when editing the file. Policies remain below the local Git common directory; cloning a
repository does not install another device's policy.

### Hide or show tools

A tool setting controls both the call arguments and its associated results, including shell
stdout/stderr. Override names are exact and case-sensitive. `runtime:tool` takes precedence over
a bare tool name, which takes precedence over `tools.omit`. There are no wildcard rules.

```sh
agit privacy policy tool --omit true --repo alice/project
agit privacy policy tool Read --omit false --repo alice/project
agit privacy policy tool codex:exec_command --omit true --repo alice/project
agit privacy policy tool mcp__company__query --omit true --repo alice/project
agit privacy policy tool codex:exec_command --reset --repo alice/project
agit privacy policy tool --omit false --repo alice/project
```

Reset removes the named override; it then inherits a matching bare-name rule or the default.
Tool names come from the recorded runtime. Results with missing or ambiguous call identities
remain unavailable because a tool-specific choice cannot be established for them.

Captured file-tool content does not require a configured workspace or an include match.
`workspace`, `external_roots` and `include` still control file collection and path aliases.
Structured `path` and `file_path` inputs honor built-in sensitive-file exclusions, configured
exclusions, branch exclusions and mandatory sources. Exclusion patterns use the most specific
configured root, then the recorded cwd when available. Shell commands are not parsed to infer
which files their output contains; use the shell tool's omission setting to hide that output.
Readable tool text still undergoes path aliasing, configured replacements and secret protection.

### Choose attachment handling

```sh
agit privacy policy attachment omit --repo alice/project
agit privacy policy attachment metadata --repo alice/project
agit privacy policy attachment original --repo alice/project
```

| Mode | Published content |
| --- | --- |
| `omit` | A typed notice such as `[Image omitted by policy]` |
| `metadata` | Available descriptive type, name/title, MIME type and size, after text sanitization; no body, URL, path or resource ID |
| `original` | The recorded attachment structure and payload; readable text is sanitized and encoded binary payloads remain intact |

The mode applies to message attachments and nested tool attachments. Hiding a tool also hides
its attachments. Original mode retains data already captured in the transcript; it does not
read referenced files or fetch URLs. Binary payloads are not decoded for text scanning. Existing
input and publication size limits still apply, and this setting does not guarantee that every
viewer renders every attachment format.

### Omission notices and existing history

Notices distinguish tool policy, file exclusions, attachment handling, unsupported formats and
unverifiable sources. For example, `[Tool output omitted by policy]` is a configured choice,
while `[Content unavailable: unsupported message format]` is a format limitation. Reports retain
the source LOG position and reason. Hidden tools remain tool events rather than new user turns.

Changes apply when generating new projections. Accepted commits, ciphertext and previously
created shares remain unchanged. Older generic notices retain their wording; their cause is not
guessed from the placeholder. Activity summaries omit both old and new standalone notices.

## Original access

Encrypted sessions enable session-level explicit authorization by default. The immutable
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

A fork of an encrypted snapshot can be resumed and continued. Its push publishes the snapshot's
turns as they were already published, or is refused. Resume, repeat import, clone, branch rename, and repository copies preserve
source authority. Inherited originals remain governed by their source repository and session.

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

## Repair files an older agit filled with placeholders

Older agit versions replaced some values that are not credentials, such as checksums,
base64 data URIs, links and whole script lines, with `{{AGIT_SECRET_V1:…}}` or
`{{AGIT_SECRET_V2:…}}` placeholders in the files they published. Those files then broke
for everyone who downloaded or cloned them. `agit push` names such files on the branches it
publishes. To repair them:

```sh
agit repair placeholders alice/project            # every file line
agit repair placeholders alice/project@main --dry-run
```

The command reads each affected text file at the branch tip and puts back the values from
this device's dictionary. It then protects the file again under the current policy, so a
real credential stays a placeholder, written in the current short form
(`‹secret:kind:id›`); a file that held only credentials changes only in that spelling, and
`agit push` stops naming it. Changed files land as one ordinary commit,
"agit: restore values an earlier agit replaced". History is not rewritten: older
commits keep their placeholders, and the current files, which downloads and clones see,
are repaired. Nothing is published until you push: look through the listed files first,
since everything the current check doesn't treat as a credential is back in them. Push to
publish the commit.

Some placeholders may have originals that this device's dictionary does not hold. These
are listed by file and line, and the command exits with a precondition error. Sign in on
this device so its dictionary syncs, or run the repair on the device that published the
files.
