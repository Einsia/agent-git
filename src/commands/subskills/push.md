---
name: agit-push
description: Publish saved history using the Hub repository's fixed encryption mode.
---

# agit push

## Purpose

Publish saved local history in the destination's authoritative mode. Ordinary
repositories receive source history, shared-file branches, tags and LFS with their
original commit IDs. Encrypted repositories receive a sanitized session projection
and encrypted originals. `push` publishes existing branches; use `new`, `fork` or
`import` to create a session.

## Synopsis

```bash
agit push [owner/repo@branch] [options]
```

## Options

| Option | Meaning |
|---|---|
| `[owner/repo@branch]` | Explicit saved branch; a bare repo with `-b` is also accepted. An omitted target requires `AGIT_SESSION` outside the terminal picker |
| `-b, --branch <branch>` | Branches to publish; repeatable |
| `--all` | Publish all settled branches; encrypted mode skips repository file lines |
| `--private`, `--public` | Select visibility for a new repository; an existing audience is retained |
| `--encryption=true\|false` | Select a new repository's fixed mode; omission uses local creation intent or `privacy.encryption` |
| `--to <owner/repo>` | Publish original history to another repository; for device-local RC, select its retained primary target |
| `--separate` | With `--to`, publish a separate copy from local RC while retaining its primary target |
| `--allow-secrets` | Explicitly accept deterministic findings in ordinary mode; encrypted publication still requires clean processed content |
| `--audit` | Open an interactive sensitivity reviewer for the frozen outgoing publication, then ask separately before publishing |
| `--dry-run` | Inspect the selected publication with Hub mode/identity lookups, without creating a repository or uploading history |
| `--show-preview` | Expand readable snapshot content and summarize binary payloads |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

An existing repository's mode is fixed, even when empty, and does not follow later
creation-preference changes. A conflicting flag is refused with guidance to create
another repository. Missing mode fields, malformed replies and failed lookups are
errors; they never authorize ordinary publication.

New repositories default to encryption enabled. Configure their viewing key with
`agit privacy init OWNER/REPO` before encrypted push or dry run. For agent-assisted setup, use
`agit privacy init OWNER/REPO --browser --json --yes`, show its setup link, wait for the user's reply,
and rerun against the same explicit Hub/repository until the API reports `ready`. See the privacy
subskill for the output and onboarding contract. An ordinary
destination requires no viewing password and can be created by its first confirmed
push with `--encryption=false`. Automatic publication requires an existing destination.

Visibility is settled once, at first publish. Without `--private` / `--public`, push takes the repo preference recorded by `agit init --private`, then the global `push.visibility` (`public` or `private`; `ask` means ask), and otherwise asks a person at a terminal (non-interactive runs and agent sessions default to private). `--dry-run` prints which of these applies.

A bare `agit push` in a human terminal selects a saved session in the TUI. The
choice applies only to this invocation. Agent and script callers use a complete
target or `AGIT_SESSION`; workspace bindings, native IDs, and the newest session
do not choose what to publish. `--yes` does not select a missing target.

## Review secret protection before pushing

Before an authorized push, complete this review using the selected Agent repo, not the source-code repository:

1. Inspect `agit secrets review --repo <agent-repo-path> --json` and run `agit scan <owner/repo>@<branch> --secrets --json`. Review existing protection candidates even when the scan is clean: already protected values appear as placeholders and may include false positives.
2. Check the actual values in authorized local source content and assess them for the requested audience. `review` returns opaque IDs and policy metadata, not plaintext; a label or placeholder alone is insufficient evidence for `allow`. Keep real credentials and uncertain candidates protected.
3. For each confirmed false positive, run `agit secrets allow <record-id> --repo <agent-repo-path> --reason <reason>`. When supplying the exact value directly, pipe it into `agit secrets allow --stdin --repo <agent-repo-path> --reason <reason>`. See [secrets](secrets.md) for input limits and structured local/synchronization results.
4. Rescan after each batch of allowance changes and inspect synchronization status, then proceed with the original push target and options. A truncated or sampled report is not the full finding set; finish reviewing remaining findings before publishing. Keep genuine credentials protected throughout this workflow.

Declarations synchronize immediately when possible and again before publication. Offline decisions remain pending; check their status before relying on server policy. Ordinary push refreshes acknowledged policy and completes pending synchronization before LFS, branch or tag uploads. First publication may create the selected repository before synchronization. An older Hub without the protocol reports a missing capability. `--dry-run` checks and reports pending synchronization without remote writes. A clean local scan does not acknowledge server policy.

Use exact `allow` decisions for routine false positives. When publication is already authorized and you have determined that all remaining findings are suitable for that audience, you may use `--allow-secrets` yourself for ordinary publication. Explain the classification and report use of the flag. The review above is the agent's responsibility within the existing publication authorization; a scanner finding alone does not require another user confirmation. Unexamined findings in a bounded report still require review; incomplete scans, missing objects, integrity failures, authorization errors and other publication errors need their own diagnosis.

`--allow-secrets` applies only to this command's branch and version-tag requests, including verified LFS content. It emits a warning and reports any declarations whose synchronization did not complete. An ordinary retry does not inherit acceptance. Hiding an event from VIEW leaves LOG and Git history available for publication; it does not clear a historical credential finding.

## Ordinary publication

With encryption disabled, push freezes and inspects source commits, their ancestors,
tags and LFS bytes. Selected file-line branches are supported, and an existing local
`main` is included. Historical object IDs and original session identities are retained.
The CLI prints the destination, mode and selected refs, then requires confirmation.
`--yes` supplies that confirmation and retains inspection, identity and access checks.
Successful publication updates ordinary branch tracking and saves session receipts.
No viewing key, generated projection or privacy strategy is requested.

For pointers in the inspection scope, if an LFS payload is absent locally, push downloads it from the source
repository's pinned Hub identity into private inspection storage. Its size and
hash are verified and its content is scanned even when the current version has
deleted the file or the destination already stores it. Unavailable or corrupt
content blocks publication; restore the original payload or retry when the source
is available. This also applies to dry runs and separate destinations.

Ordinary and encrypted pushes inspect objects absent from the destination's live
advertised Git history. Encrypted pushes compute this scope after projection, using
published object IDs. Local tracking refs and previous push receipts do not define
this baseline. New commits, blobs and annotated tags are inspected, including files
added and deleted between published tips. New pointer objects require verified LFS
payloads even if the destination reports that it already has those payloads.

First publication, an empty destination, a failed advertisement or an unverifiable
baseline uses full inspection. Advertised objects unavailable locally cannot narrow
the scope. Manual push, automatic push and dry runs share these rules. Use
--audit for complete history and payload review. Incremental push does not recheck
remote history when scanner rules change. No scan-result cache is kept across pushes;
the Hub's receive policy remains authoritative.

An unchanged ordinary push still performs authenticated receive advertisement.
The Hub validates and registers existing native history that lacks committed
admission before responding, without moving refs. Failed reconciliation leaves
the push unsuccessful and does not replace local publication receipts or confirm
a supervisor result. This requires a Hub with ordinary-history reconciliation
support. RC becomes synchronized only after its receiver returns a durable ACK;
Git `UpToDate` and local publication receipts do not provide that acknowledgement.
Explicit secret-findings acceptance accompanies both ordinary push-access probes
and native publication; automatic pushes do not inherit that exception.

Ciphertext-only history cannot be published as unencrypted originals. If selected
ancestry contains encrypted snapshots, push requires the original historical data;
automatic bulk recovery and conversion of every historical version are not provided.

## A separate destination

Use an explicit target to publish available original history in another mode:

```bash
agit push alice/source@work --to alice/ordinary-copy --encryption=false
agit privacy init alice/encrypted-copy --encryption=true
agit push alice/source@work --to alice/encrypted-copy
```

The source keeps its refs, remotes, identity, automatic consent and primary receipts.
Source secret allowances do not authorize the separate destination; its own acknowledged
allowances govern inspection without rebinding or synchronizing source declarations.
The target gets its own immutable identity, fixed mode and publication state.
Encrypted copies use the target's current key and both source and target restrictions.
A missing target's creation mode uses `--encryption` first. An unpublished local
source then uses its recorded `init` intent; otherwise the user creation default applies.
An existing target always uses its authoritative mode. Reusing a confirmed target
name for another identity is refused.

For a device-local RC source, plain `--to` retains its existing primary-binding
semantics. Add `--separate` for an explicit copy without replacing that binding:

```bash
agit push desktop-machine/project@work --to alice/copy --separate --encryption=false
```

Separate publication is an explicit unsupervised operation. It does not authorize
automatic uploads to the additional target. A ciphertext-only clone requires the
complete original data for the selected historical scope; unlocking one snapshot
does not supply every version. Automatic bulk decryption and source-history rewriting
are outside this workflow.

## Encrypted publication

Encrypted push fetches the destination repository's current viewing public key, generates isolated
public Git history with encrypted originals, and scans the complete result. Writers need write
access and the public key; they do not enter the repository password. The CLI prints the actual destination and a local JSON preview with public snapshots and
omission reports. Confirm interactively or supply `--yes`; `--yes` still performs every check.
Source refs remain private local history, while remote refs identify the generated public history.
Only selected session branches are published; `main` is not implicitly added. Explicit file-line
targets are refused while independent repository-file publication is deferred.
When `push.auto` is enabled, confirmation also authorizes automatic incremental publication under
the displayed repository policy, recipient and destination. The local receipt is saved only after
a successful push. Changes to those conditions require another explicit confirmation.

Add `--audit` to review disclosure
risks that depend on meaning and context, such as private conversations or internal
customer information. This review includes the generated public historical LOG, public
metadata and encrypted envelopes in the selected publication. LOG content hidden from VIEW
still undergoes the same privacy processing. Repository files and standalone attachments are excluded from session publication.

```bash
agit config runtime.default claude-code
agit push alice/payments@session-1 --audit
```

The reviewer opens in the same terminal with visible progress and interactive
questions. It uses immutable `agit show audit/source@<full-commit> --log-only --raw
--no-tui` reads in an isolated inspection store and bounded exports of the complete
published carriers. The native runtime uses its configured model provider; the
review can send the supplied content to that provider. It requires an installed
Claude Code runtime with the required controls, and `runtime.default` must select
`claude-code`. Other configured runtimes are refused explicitly.

When the report is ready, exit the reviewer normally to return to push. Push checks
the report and displays findings before asking for a separate publication decision.
Answers inside the reviewer and global `--yes` do not replace that final decision.
The complete JSON report remains available during this confirmation. Temporary
inspection files are removed when the push command ends.

Incomplete review, unreadable evidence, unsupported decoding, exhausted budgets,
unanswered required questions, or an interrupted reviewer stop the audited push.
Verified binary content is listed as excluded from text review. Model findings are
advisory; the deterministic secret gate remains mandatory. The reviewer does not edit, redact, create repositories, or publish content.

Encrypted repository creation and password initialization precede publication review.
Implicit promotion of a read-only checkout remains refused. Explicit separate publication
uses available original data; it does not copy source ciphertext or its admissions.
Ordinary clone retains the source repository identity.

Password changes rewrap the same key and leave published Git objects unchanged. Key rotation
retains accepted ancestors, tags and public session identities; only new snapshots use the new
key. A repeat push after rotation preserves its original receipt binding. Automatic publication
requires renewed consent for the new recipient. Missing or damaged accepted mappings require
restoring local privacy state or continuing from a fresh clone; they never authorize rewriting
remote ancestry. Changed policies still require review and may require a fresh clone before
continuation. Retained old ciphertext stores remain available for pending receipt delivery.
Fresh clones can publish new snapshots after a historical reading key is revoked: accepted
ciphertext remains unchanged, and only new nodes need the current publishing key. This does not
restore permission to unlock the revoked originals.

`--audit --dry-run` performs the interactive review and prints its report without
creating, promoting, or publishing a repository. Audited push requires terminal
input and output and cannot run with `--json` or from an unattended script.

## Examples

```bash
agit push szh/p1@feature-a
agit push szh/p1 --all
agit push szh/p1 -b feature-a --dry-run
agit push szh/ordinary --all --encryption=false
```

A secret scan runs before publishing. If `refs/heads/<branch>` is missing, create it with `new`, `import`, or `fork` and verify it first. A directory binding does not select a publication target. Share/export encryption selections are independent of this fixed repository setting.
