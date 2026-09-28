# Credential findings during publication

`agit push` scans outgoing history locally. Public pushes also receive a strict
server scan. A rejection from a supporting server identifies the rule, file or
Git object location, line number and redacted excerpt. Reports contain a bounded
sample, so the displayed locations need not include every occurrence.

Review those locations and classify each exact value for the publication audience. A local
non-secret declaration accepts an existing record ID or stdin:

```sh
agit secrets allow sec_record_id --repo /path/to/agent-repo --reason "Public identifier"
printf %s "$VALUE" | agit secrets allow --stdin --repo /path/to/agent-repo --reason "Test fixture"
agit secrets unallow sec_record_id --repo /path/to/agent-repo --json
```

Declarations retain reasons, pending operations and any retained immutable repository target.
The CLI synchronizes them immediately when possible and retains pending operations offline.
Commands report `local_applied` in JSON even when synchronization returns nonzero. `review --json`
distinguishes local policy from server acknowledgement. Ordinary push refreshes repository
policy and synchronizes before any LFS, branch or tag upload. First publication may create the
selected destination and bind its immutable identity before synchronization. Dry run checks and
reports the planned work without remote writes. A changed Hub or repository cannot inherit
another destination's declarations. Remote revocations and revision conflicts require a new
reviewed local decision before a conflicting write can be attempted.

Only repository declarations synchronize. Global allowlists and built-in exemptions remain
local. An older backend without the exact-policy endpoint blocks ordinary publication when
synchronization is required. An explicitly accepted push retains its existing route and reports
any declarations left pending. Deploy the backend policy support before releasing a CLI that
relies on it; verify ordinary public Git and LFS publication against that deployment.

Review all findings, including those omitted from bounded samples, before accepting a push.
To deliberately accept findings for the selected push, use:

```sh
agit push alice/notes@experiment --allow-secrets
```

Keep the same target and branch selection as the rejected command. The option
covers the branch requests and version tags sent by this command, including
credential findings in verified LFS payloads. Missing, corrupt or incompletely
scanned LFS payloads remain errors. It is not
saved in repository configuration and does not apply to later pushes. The CLI
prints a warning; it never retries a rejection with acceptance automatically.
`--dry-run` still sends no writes. `AGIT_ALLOW_SECRETS=1` affects only the local
check and does not authorize acceptance by the server.

The server still scans the requested history and records explicit acceptance
with the caller, repository, finding sample count and resulting ref digest.
Unreadable objects, exhausted scan budgets, invalid provenance, stale repository
identity, missing permissions, immutable-ref violations and quotas remain errors.
An older server can reject the request without supporting the acceptance option;
update the backend first, then deploy a CLI with this option.

For `agit repo visibility alice/notes public`, the CLI displays the server's
finding locations before the existing typed repository confirmation and separate
acceptance prompt. The confirmation remains bound to the scanned repository
snapshot and expires. Older servers without location details still provide their
rule counts.

Local secret projection protects supported new session content before Git
objects are formed. It does not rewrite existing history or automatically cover
arbitrary shared files, Git metadata, or writes made by other clients. These
surfaces explain why a strict server scan can find content after a local scan or
projection has reported no remaining session credentials.
