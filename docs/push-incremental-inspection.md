# Incremental publication integrity

Push freezes the selected refs and verifies immutable source objects. A live authenticated
advertisement bound to the destination identity selects which LFS pointers need payload
availability and hash verification. Local tracking refs cannot establish remote presence.
Unavailable or corrupt required payloads remain integrity errors. All LFS payloads are
excluded from secret detection, regardless of their media type.

Publication does not run a secret gate. Local capture and explicit projection call the
independent best-effort privacy worker. Every push schedules encrypted dictionary synchronization
separately, including an up-to-date push. Optional scan, dictionary-key and synchronization failures
do not supply publication authority. Session registration, encryption and source-integrity checks
must succeed. Accepted committed prefixes and historical object IDs are retained.

Each session uses its registered plain or session-owner encryption scheme. Encrypted snapshots
build a public projection before transport. The immutable candidate cache binds source, session
owner, owner-key version, destination, effective local rules and recoverable dictionary contents.
A failed privacy dependency prevents reuse of an unverified dependency fingerprint. Retries retain
the same prepared ciphertext; they cannot fall back to plaintext after a key or registration failure.

`agit scan` is an explicit diagnostic. `--audit` requests a separate interactive review;
neither is an implicit prerequisite of the default upload path. See
[the local privacy RFC](rfc-local-privacy.md) for the accepted failure and recovery contracts.

## Publishing in parts

A push carries only what the destination does not hold yet: its plan stops at the commits the
destination's advertisement reaches and at the branch's accepted sources. When that new history
is more than one publication to this Hub can carry, the push cuts the branch's first-parent line
into parts. Each part fits the commits the Hub admits per request (`GET /api/hub/config`,
`limits.push.max_new_commits`) and the publication's own commit and metadata bounds, counting the
held commits it stands on. A merge brings its other parents' new history into the part that
reaches it. A step too large for any part is refused before anything is sent.

One confirmation covers every part of the reviewed heads. Each part is an ordinary publication of
its own: it is frozen at its cut, projected, inspected and previewed (a numbered preview file per
part), and sent with its own receipts. Before each later part the project policy and the
destination are checked again. Each part is planned from what the Hub holds after the one before,
so an interrupted run continues from there when it is rerun. A part is verified against its branch
by ancestry: the branch may have moved forward past the cut, but not been rewritten without it,
and no part goes past the reviewed head. Several branches that need parts are published one branch
at a time; a cross-Hub delivery is sent whole.

