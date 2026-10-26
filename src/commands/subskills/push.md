---
name: agit-push
description: Publish saved history using registered session privacy settings and immutable destination authority.
---

# agit push

Session pushes may include local `main` when the Hub has no `main` or its current tip is an
ancestor of local `main`. If implicit `main` cannot fast-forward the Hub, push leaves it as on
the Hub, prints `agit pull <owner/repo>@main`, and publishes the selected session branches.
Explicit `@main`, `--branch main` and `--all` keep `main` selected and require synchronization
when it is behind or divergent.

If the destination branch has advanced beyond the local accepted history, push keeps local
unpublished commits and prints `agit pull <owner/repo>@<branch>` to synchronize first. A remote
advance is different from a missing or inconsistent local privacy mapping. Preserve the checkout
containing unpublished work when recovering a genuinely damaged mapping.

A collaborator with write access can publish a new fork after continuing a teammate's session,
including after switching runtimes. Published ancestors keep their existing commit IDs; the
collaborator does not need the original publisher's local privacy state. If a retained published
snapshot is unavailable locally, push names the missing commit and prints a fetch and retry
command. Keep the checkout containing unpublished work while recovering the public snapshot.

Destination-ref queries retry temporary connection failures and HTTP 408, 429, 500, 502, 503
and 504 with bounded backoff. Authentication keeps its separate refresh path; other failures
stop without a transient retry. Reads keep the prepared Hub, repository and account.
If publication stops, the diagnostic includes a command that retains the selected local
repository, branches and destination. It distinguishes no ref updates, acknowledged updates,
and unconfirmed effects. For unconfirmed effects, fetch and inspect status before rerunning.

Publish existing saved branches with their registered privacy settings. Plain sessions and file
lines retain their original Git objects unless initial plain selection requires publication metadata
to be normalized. Encrypted sessions publish a sanitized projection and
originals encrypted to the session owner's account key. Accepted ancestry remains unchanged, so
a repository can contain both modes. Use `new`, `fork` or `import` to create a session.

New sessions publish plain by default. To enable encryption, run `agit privacy mode owner/repo@branch encrypted`
before its first registration. This selects the logical session's mode across its branches;
`--public` and `--private` only select repository visibility. Registered modes cannot change.

## Synopsis

```bash
agit push [owner/repo@branch] [options]
```

| Option | Meaning |
|---|---|
| `[owner/repo@branch]` | Explicit saved branch; a bare repo with `-b` is also accepted. An omitted target requires `AGIT_SESSION` outside the terminal picker |
| `-b, --branch <branch>` | Branches to publish; repeatable |
| `--all` | Publish all settled local branches |
| `--private`, `--public` | Select visibility for a new repository; an existing audience is retained |
| `--to <owner/repo>` | Select the retained Hub publication target for a device-local RC repository |
| `--allow-secrets` | Compatibility flag for servers that require secret-findings acceptance |
| `--audit` | Review frozen outgoing content with an interactive agent, then confirm publication separately |
| `--dry-run` | Inspect registered sessions without creating a repository, registering sessions, or uploading history |
| `--show-preview` | Expand readable snapshots and captured plain file/LFS text; summarize binary bytes |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

A bare `agit push` in a human terminal selects a saved session in the TUI. The choice applies only
to this invocation. Agents and scripts use an explicit target or `AGIT_SESSION`. `--yes` confirms
publication but does not select a missing target or complete an audit review.

## Agent publication workflow

For a new repository's first push, explicitly pass `--public` or `--private` according to
the user's visibility instruction; if none is given, use `--private`. Keep that flag in both
the dry-run and publication commands below, which show the default private choice.

For an authorized ordinary publication, run a dry-run before uploading:

```bash
agit push <owner/repo>@<branch> --private --dry-run
```

Check the destination, visibility and snapshot summary. Keep the default summary output;
the complete preview remains in the JSON file at the reported path. After the dry-run
succeeds, or if it fails with either exact first-publication diagnostic below, publish with
confirmation supplied by `--yes`:

```bash
agit push <owner/repo>@<branch> --private --yes
```

Keep the same account, Hub, target, branch selection and publication options in both
invocations; replace `--dry-run` with `--yes`. If those inputs or the
saved history change, run the preview again before publishing.

With an explicit target and registered destination and sessions, ordinary dry-run
displays the summary without asking for publication confirmation or uploading history.
For a new destination, dry-run stops with:

```text
a new destination needs an immutable repository identity before a session publication preview; publish without --dry-run to confirm its creation
```

For an existing destination with a saved session that has not been registered, dry-run
stops with:

```text
session SESSION must be registered before a dry-run publication
```

`SESSION` is the saved session's AgentGit ID. This diagnostic is emitted only after the
Hub explicitly identifies that session as unregistered; a failed lookup is not equivalent.

Either diagnostic permits continuing the authorized publication with `push --yes` without
another confirmation. The ordinary push creates the destination or registers the session,
then prepares its preview before uploading. Match these diagnostics, not just a nonzero
exit code. Stop and report any other dry-run error, including permission denial, network
failure or content validation failure. If the subsequent push fails, report that failure
instead of retrying this exception.

`--audit` is a separate interactive workflow; even `--audit --dry-run` requires a person
at the terminal, and `--yes` cannot replace its review or final confirmation.

## Session authority and first publication

New sessions record their creator and default to plain publication. Existing plain sessions
retain their registered plain mode. Unknown local sessions require registration before a dry run;
only the Hub's explicit unregistered-session response permits registration. Missing permissions,
malformed responses and network failures never select a plaintext fallback.

A new destination requires confirmation before creation, because encryption binds its immutable
repository ID. `--yes` or authorized automatic publication can supply that confirmation. The CLI
then registers sessions, prepares immutable candidates and displays the publication preview before
upload. The owner remains the original session owner when a different account commits or pushes.
The snapshot's owner and mapped committer receive default original access subject to membership.
Use `privacy access` to manage additional readers of each exact snapshot.

Visibility is chosen at creation. Push uses an explicit flag, the preference recorded by
`agit init --private`, then `push.visibility`; an interactive caller can choose when configured to
ask. Noninteractive creation defaults to private. Existing repository visibility is preserved.

## Review and publication

The preview identifies the account, Hub, immutable destination, audience, registered modes and
selected refs. Stored preview JSON contains public snapshots and plain file content; each plain
snapshot contributes the files it introduces relative to its parents. A blob appears with its
content once; a later entry carrying it is marked `repeated`. A file that only appends to its first
parent's version shows the appended bytes under `appended_to`, and an object above the per-object
limit is listed with `omitted_bytes` after its payload is verified. Encrypted
snapshots exclude independent repository files. Captured attachments follow the local attachment
policy and default to retaining their recorded carriers. Complete original LOG and
VIEW records remain encrypted; reading them requires exact-snapshot account authorization.

Objects absent from the destination's authenticated live advertisement are captured for inspection.
Tracking refs and old local receipts cannot define that baseline. New commits, blobs, annotated
tags and LFS pointers are included even when a later snapshot deletes them. New pointer objects
require captured payload bytes with matching size and hash. Full preview displays captured LFS text;
binary content is summarized, and unchanged accepted pointers outside the upload are marked preserved.
A missing or unverifiable baseline requires full inspection. Unavailable payloads block publication.

Tool and attachment projection settings apply to encrypted snapshots, privacy exports and shares;
plain push retains its existing path and local secret protection. Tool content defaults to visible
and attachments default to original. Configure `privacy policy tool` or `privacy policy attachment`
to hide content before preparing new publications.

Local privacy processing applies path aliases, omissions, replacements and secret protection before
encrypted public bytes are prepared. Optional display processing can return partial results or skip
unavailable work. Session authority, source identity, ciphertext integrity and publication receipts
remain required independently of privacy findings. Dictionary synchronization runs independently.
Use `agit secrets review` or `agit scan` for explicit diagnostics.

Original recovery during encrypted publication must complete against a frozen local dictionary.
Missing mappings, hydration timeouts or worker failure stop preparation before upload; do not
work around these errors by dropping records or substituting a partially restored transcript.

Manual encrypted publication requires confirmation of the prepared preview; `--yes` supplies it
for an unattended invocation. Plain-only publication can proceed without a prompt when no terminal
is available. `--audit` requires its completed review and separate confirmation. Dry runs stop
after inspection. Changes to account, source, destination, audience or registered
session configuration invalidate the prepared publication before mutation.

The Hub's versioned receipt binds the manifest and exact ref transitions. Local receipts retain
source-to-public snapshot mappings and are saved only after verified Git acknowledgement. Retries
reuse prepared ciphertext and accepted mappings instead of creating a new identity for the same
snapshot. RC synchronization additionally requires its durable receiver acknowledgement.

## Interactive sensitivity review

`--audit` requires an interactive terminal and `runtime.default=claude-code` with an installed
Claude Code runtime that supports the required controls. The reviewer reads frozen content from
an isolated store and can send that content to its configured model provider. Exit the reviewer
normally when its report is ready; push validates the report and asks for a separate publication
decision. The report remains available during confirmation, and temporary review files are removed
when push ends. Global `--yes` and answers inside the reviewer do not replace final confirmation.

Incomplete review, unreadable evidence, unsupported decoding, exhausted budgets, unanswered required
questions and interruption stop an audited push. LFS payloads are excluded from model privacy review;
full terminal previews can still show captured LFS text. Model findings are advisory. The reviewer
does not edit content or publish it. `--audit --dry-run` runs review without publication and requires
an existing registered destination and sessions. Audited push cannot run with `--json`.

## Automatic publication and local RC

`push.auto` authorizes automatic publication. Enrollment binds the account, immutable repository,
URL and audience; delivery rechecks current authority and each session's configuration. Automatic
continuation retains the session owner and uses the same capture and receipt checks as manual push.

A device-local RC repository can select its initial Hub destination with `--to owner/repo`. Later
pushes retain that confirmed destination and the device's independent local identity. Cross-repository
push copies require source-publication authority and are currently refused before mutation.
