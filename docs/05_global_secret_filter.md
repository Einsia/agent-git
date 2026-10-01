# Local secret policy

The accepted architecture is [Local Privacy](rfc-local-privacy.md). Privacy runs
locally as an optional, bounded transformation. Failures preserve available
content and never gate recording, sharing, or publication.

The detector has no storage or network dependencies. Default heuristics and
explicit global/repository literal blocks share a compiled policy snapshot.
Explicit blocks override allows; user allows override default heuristics. A
recovery dictionary entry is not a block rule.

`agit secrets add NAME` reads a global literal through a hidden prompt.
Automation uses `--stdin`; values never belong in argv. `list` and `status`
show non-secret metadata. `remove ID --yes` disables a block while preserving
older recovery mappings. Repository rules use `secrets block add/remove` with
`--repo PATH`. `secrets allow/unallow ID` changes repository heuristic policy;
`--global` applies that decision to global user policy.

Policies reference opaque dictionary tokens in private local metadata. New
records use the user's automatic cloud key when cached, otherwise temporary
owner-private plaintext. Policy management needs no OS keyring configuration.
Legacy vaults are read for migration inside the disposable worker; unreadable
legacy state cannot stop business operations. The `secrets.keystore` setting
only selects how existing legacy keys are read.

`agit scan --secrets` is an explicitly requested diagnostic. Neither its result
nor successful initialization is a prerequisite for push. All LFS payloads are
excluded from privacy processing, regardless of whether they contain text.
