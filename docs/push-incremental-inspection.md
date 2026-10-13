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
