# Reversible secret dictionaries

The accepted architecture and cloud contract are specified in
[Local Privacy](rfc-local-privacy.md).

Session metadata observations, including runtime titles, use the same local protection
and hydration path as conversation text. Structural identifiers retain their original values.

Every successful replacement has an exact durable token-to-original record.
New tokens have the form `{{AGIT_SECRET_V2:DICTIONARY_UUID:RECORD_UUID}}`.
They contain random identifiers, not hashes of secret values. Existing V1 aliases
remain usable when the legacy dictionary can be read. Multiple devices may create
different aliases for the same original; synchronization preserves each alias.

The private SQLite journal under `AGIT_HOME/privacy/` stores immutable mapping
packages, an encrypted outbox, acknowledgements, download cursors and retry
identities. Dictionary membership and scoped policy decisions are separate.
Allowing a false positive or disabling a block never deletes the original needed
to restore an older token.

A dedicated cloud key is provisioned automatically for the signed-in account.
The client caches keys by Hub, immutable account and key version. AES-256-GCM
packages bind those identities and package/dictionary IDs as associated data.
Long originals are fragmented; hydration exposes them only after every fragment
has been authenticated. A new token is returned only after a durable transaction.

Without a usable key, mappings remain temporarily in private local plaintext.
The sync transport accepts only authenticated-encryption envelopes. It never
uploads that plaintext journal. Every push, including a no-op, schedules a bounded
sync attempt; login and fetch also offer sync opportunities. Failures retain the
outbox for later attempts. The server stores opaque packages and keys without
scanning, decrypting, or redacting conversation content.

Restoration takes place only in a local runtime materialization. Unknown tokens
remain visible until their mappings arrive. Git objects, worktrees and published
history are not hydrated or rewritten. A repository reader does not inherit
access to the author's private dictionary.

Local native-prefix checkpoints and committed content preserve settled history
across rule changes and privacy outages. An unavailable checkpoint falls back to
content comparison; unknown opaque spans retain their committed representation.
