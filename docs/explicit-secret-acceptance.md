# Credential findings during publication

`agit push` scans outgoing history locally. Public pushes also receive a strict
server scan. A rejection from a supporting server identifies the rule, file or
Git object location, line number and redacted excerpt. Reports contain a bounded
sample, so the displayed locations need not include every occurrence.

Review those locations and remove unintended credentials before publishing.
To deliberately accept findings for the selected push, use:

```sh
agit push alice/notes@experiment --allow-secrets
```

Keep the same target and branch selection as the rejected command. The option
covers the branch requests and version tags sent by this command. It is not
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
