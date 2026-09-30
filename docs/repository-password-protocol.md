# Repository viewing-key client contract

The CLI consumes the repository-password backend P0 contract through the selected Hub.
The repository name locates the route; the immutable `agent_id` authorizes identity matching.
Backend integration must pass against these routes before this feature is considered complete.

## Fixed repository mode

Hub creation accepts `encryption_enabled` and preserves an existing identity's
mode on retry. The CLI always sends the field explicitly, so a CLI-created
repository never depends on the Hub's choice for an omitted field. An explicit
conflicting selection returns HTTP 409 and directs the caller to create another
repository. Create and authoritative read responses return the immutable
`agent_id` and an explicit effective boolean. Legacy repositories without mode
configuration return `false`; existing encrypted publications retain enabled mode.

The CLI requires that explicit response field before using a mode. Missing,
malformed or failed responses never select ordinary publication. The user
`privacy.encryption` preference defaults to `false`, so new repositories use
ordinary publication unless encryption is chosen; it supplies only creation
intent, and `--encryption=true|false` overrides it when creating a new identity.
`privacy init` creates a missing repository encrypted unless a flag, local intent
or stored preference selects `false`.
Repository config reads are authoritative and mode writes are refused, including
for empty repositories. Visibility and `push.auto` are independent.

Ordinary publication uses native Git LFS. The CLI binds its batch request to the
repository identity; upload and verification use the action headers returned by
the Hub. Repository-scoped identity headers must not be appended again to those
actions, because the receiver rejects duplicate immutable identity headers.

The key protocol below applies to enabled repositories. An enabled repository
without a key requires explicit setup and cannot fall back to ordinary mode.
Key initialization, rewrapping, rotation and revocation never change mode.

## Repository key operations

Base path: `/api/agents/{owner}/{name}/privacy`.

| Operation | Request | Response |
| --- | --- | --- |
| Publishing public key | `GET /publishing-key` | `agent_id`, `config_version`, nullable `current` with `recipient`, `public_key_algorithm`, `public_key` |
| Administrator configuration | `GET /keys` | `agent_id`, `config_version`, nullable `current_recipient`, `keys` |
| Configuration mutation | `POST /keys` | Updated administrator configuration |
| Publication reader | `GET /keys/{recipient}?ref={full_commit}` | `agent_id`, `commit`, `session_id`, `key` |

Publishing requires write access; configuration requires management access. Reader lookup permits
anonymous public reads and preserves private read ACLs. The reader route must prove that a
COMMITTED publication in the named immutable repository references the requested recipient.
Neither Git-object presence nor a PREPARED admission authorizes a reader key response.

Mutations carry `expected_agent_id`, `expected_version`, and an explicit operation:
`initialize` and `rotate` carry `key`; `rewrap` also carries the current `recipient`;
`revoke` carries the exact `recipient`. Successful mutation returns the previous version plus one.
Configuration versions are JSON-safe nonnegative integers. Deleted keys do not reset the version.
A 409 is a terminal conflict for that invocation: refresh and reconfirm with new input.
Authentication failure uses the existing Hub auth category. Hidden/unreadable repositories,
unavailable historical keys and publication/recipient mismatches use 404. Publishing with an
empty configuration returns `current: null` and requires administrator initialization.

The public-key type excludes password wrapping fields. Key records contain `recipient`,
`current`, `updated_at` and the flattened key input:

- `public_key_algorithm: "x25519"` and a canonical standard Base64 public key.
- `encrypted_private_key`: `version: 2`, `algorithm: "xsalsa20-poly1305"`, Base64 nonce
  and ciphertext. The secretbox authentication tag precedes the encrypted private-key bytes.
- `kdf`: `algorithm: "argon2id13"`, Base64 salt, `opslimit` and `memlimit`.

Passwords are exact UTF-8, including whitespace and combining characters. Argon2id uses version
0x13, one lane, and a 32-byte output. `memlimit` is bytes; the libsodium conversion truncates to
KiB. Supported records have operations in 1..=10, memory in 8 MiB..=1 GiB, a 16-byte salt,
24-byte nonce, 32-byte public key and 48-byte secretbox output. New records use the Web moderate
parameters (3 operations and 256 MiB). These parameters match
[libsodium's password API](https://doc.libsodium.org/password_hashing/default_phf) and
[byte-to-KiB conversion](https://github.com/jedisct1/libsodium/blob/master/src/libsodium/crypto_pwhash/argon2/pwhash_argon2id.c).
The CLI rejects the historical mislabeled wrapper.

Recipient identity remains `sha256-` followed by the SHA-256 hex digest of the canonical Base64
public-key string. A password rewrap changes configuration version, but does not change recipient,
content encryption, or publication AAD. Rotation requires retaining accepted public ancestry and
looking up historical reader records by the recipient in the selected local envelope.

## Interoperability fixtures

`tests/fixtures/privacy-web-key.json` is output from the existing Web `createPrivacyKey` function,
with an explicitly synthetic multilingual password. `privacy-cli-key.json` is Rust wrapping output
for the same private key and another synthetic password, opened with Web `unlockPrivacyPrivateKey`.
Rust tests reproduce the CLI bytes and open both records.
`privacy-password-key.json` is also shared byte-for-byte with the backend/Web fixture and is
opened by the CLI interoperability test. Fixture passwords/private keys are test
data only. Existing content-envelope fixtures remain unchanged.

## Integration boundaries

The accepted-commit reader route cannot unlock an independent share or a newly projected export
without trusted accepted-publication context. Repository-default encrypted shares/exports therefore require verified accepted-publication context.
Explicit-key export retains its caller-supplied key. Standalone encrypted sharing and encrypted
repository promotion must refuse before upload until a matching backend key-delivery contract
exists. No account fallback, guessed destination, or plaintext downgrade is allowed. Ordinary clone
retains the source repository identity.

## Accepted publication state

Local publication receipts and automatic consent distinguish ordinary and encrypted
publication. Encrypted records retain their version-1 encoding, including policy
digest and recipient. Ordinary records use version 2 with `mode: "ordinary"`,
omit encrypted-only bindings, and require the published commit to equal the source.
These records remain local and do not change the peer notification wire format.
Explicit publication to another repository stores its confirmed identity under
`agit/publication-targets/` and its receipts under `agit/publication-destinations/`
in the source Git common directory. These records are separate from primary receipt
and automatic-consent state. Identity bindings reject a deleted/recreated target
name, and target-scoped accepted-history mappings retain their existing protections.
An ordinary notification names the original session and accepted commit. Only a
matching durable backend acknowledgement clears the outbox; command completion
and local receipts alone do not acknowledge RC publication.

The CLI retains authenticated source/public/parent mappings under the local Git common directory's
`agit/privacy-accepted`, scoped to canonical Hub, immutable repository ID and branch. This state
is independent of recipient and build-specific preparation caches. Prepared candidates remain
separate until a complete successful transport result or authenticated remote ref reconciliation
proves acceptance. Interrupted batches do not promote uncertain entries.

Accepted public commits are reconstructed and checked before reuse. Recipient rotation or a new
preparation cache does not change accepted object IDs or public session IDs. Historical stores
remain addressable through accepted/pending mappings for delayed receipts. Missing or corrupt
mappings refuse publication with recovery instructions rather than generating guessed ancestry.

Fork publication may inherit an accepted mapping from another branch of the same immutable
repository. Source/public parents must agree, the accepted mapping must be unambiguous, and the
branch restrictions must match. Accepted source-equals-public ancestors retain their original
policy; private-source mappings must match the current repository policy. Conflicting mappings,
private-source policies or branch restrictions refuse publication; they do not authorize
regenerated ancestry. New fork nodes retain their own logical session identity.

Publication preparation does not fetch reader-key records for accepted ancestry. A fresh clone
can retain a historical envelope even after its password-wrapped private key is revoked. Such
source-equals-public ledger entries may omit a public-key fingerprint; their immutable commit
and verified graph still identify the accepted ciphertext. New encrypted mappings always retain
the active recipient fingerprint. An unchanged inherited head without a known fingerprint does
not acquire a fabricated local publication receipt. Write authorization, current publishing-key
checks and remote-ref verification still apply. Explicit unlock continues to require the matching
reader record and fails when that record is revoked.
