# Git remote identity

Ordinary `agit push`, `fetch`, `pull` and repeated `clone` operations use the selected
repository and its current remote. A missing or stale `agit.remoteIdentity` value does
not require deleting local branches or cloning into a fresh directory. If publication
created the remote before being interrupted, repeat `agit push <owner/repo> -b <branch>`.

A command may constrain its individual Git requests to the immutable ID returned by its
current metadata lookup. This prevents a repository replacement during the operation.
It does not require a matching persistent checkout pin. Secret scan exclusions are only
reused for the remote identity whose refs were checked; a changed target requires a full
scan. Authentication and credential URL scoping still apply.

A supervised RC process supplies `AGIT_EXPECTED_AGENT_ID`. Its checkout pin, configured
Hub and resolved remote must agree with that expected identity. Ordinary commands do not
rewrite an existing pin, so changing a remote cannot silently retarget a running RC task.
`agit clone --adopt-legacy-agent-id` remains available for explicitly recording a verified
identity on an existing unpinned checkout. Repository management and ownership promotion
keep their explicit identity constraints.
