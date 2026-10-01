# Incremental publication integrity

Push freezes the selected refs and verifies immutable source objects. A live authenticated
advertisement bound to the destination identity selects which LFS pointers need payload
availability and hash verification. Local tracking refs cannot establish remote presence.
Unavailable or corrupt required payloads remain integrity errors. All LFS payloads are
excluded from secret detection, regardless of their media type.

Publication does not run a secret gate. Local capture and explicit projection call the
independent best-effort privacy worker. Every push schedules encrypted dictionary synchronization
separately, including an up-to-date push. Scan, dictionary, key and synchronization failures
cannot reject publication. Existing committed prefixes and historical object IDs are retained.

An explicitly encrypted repository builds its selected public projection before transport.
The optional projection cache binds source, path permissions, recipient, effective local
rules and recoverable dictionary contents. A failed privacy dependency prevents reuse of a
previous process's cache entry; it does not authorize an unchanged dependency fingerprint.

`agit scan` is an explicit diagnostic. `--audit` requests a separate interactive review;
neither is an implicit prerequisite of the default upload path. See
[the local privacy RFC](rfc-local-privacy.md) for the accepted failure and recovery contracts.
