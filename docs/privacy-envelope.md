# Session privacy envelope and publication protocol

Sessions use `plain` or `session_owner_v2`. Registered configuration binds an immutable
repository ID and logical session ID to the scheme, owner, creator when known, and revision.
New sessions record their creator locally and default to session encryption. Existing plain
sessions retain their accepted history. Missing configuration, unavailable keys, and failed
requests cannot authorize plaintext publication for an encrypted session.

## Envelope and exact snapshot identity

`domain::privacy_envelope::PrivacyEnvelope` supports format version `2`. Each snapshot uses
a fresh random AES-256-GCM content key and nonce. The content key is wrapped with the session
owner's versioned X25519 public key using the libsodium sealed-box byte format. The committer
may be another account; that does not change the wrapper's owner.

| Field | Encoding and meaning |
| --- | --- |
| `format_version` | Integer `2` |
| `session_binding.repository_id` | Immutable source repository ID |
| `session_binding.session_id` | Source logical session ID |
| `session_binding.published_session_id` | Public logical session ID |
| `session_binding.owner_key` | Owner account ID and key version |
| `session_binding.committer` | Account attribution or an unmapped source-committer digest |
| `session_binding.source_commit` | Immutable original source commit ID |
| `policy_digest` | `sha256:` followed by 64 lowercase hexadecimal digits |
| `snapshot_digest` | Canonical JSON digest of `public_projection` |
| `public_projection` | Sanitized session, presentation metadata and processing report |
| `private_payload.algorithm` | `aes-256-gcm` |
| `private_payload.nonce` | 12 bytes in standard padded Base64 |
| `private_payload.ciphertext` | Ciphertext followed by the 16-byte GCM tag, standard Base64 |
| `private_payload.wrapped_keys` | Exactly one owner-key wrapper |
| `attachments` | Empty array; independent attachments are unsupported |

The wrapper contains `recipient`, `algorithm: "x25519-sealed-box"`, and `ciphertext`.
Its ciphertext encodes the 80-byte sealed content key in standard Base64. The recipient
must equal `OwnerKeyBinding::recipient_id()` for the authenticated account and key version.
The shared Rust type is the authority for this encoding.

Additional authenticated data is the UTF-8 encoding of
`agit-privacy-envelope-v2` followed by NUL and the canonical JSON SHA-256 digest of:

```text
{format_version, session_binding, policy_digest, snapshot_digest,
 public_projection, attachments, wrapped_keys, algorithm}
```

`wrapped_keys` and `algorithm` come from `private_payload`. Canonical objects sort keys by
UTF-8 bytes, preserve array order, and accept only browser-exact integers. JavaScript code
must not use ordinary object insertion order for numeric-looking keys. Changes to source,
owner, committer, policy, projection or wrappers invalidate authentication. Grants are stored
outside the envelope; granting or revoking readers never changes ciphertext or Git objects.

An `AcceptedSnapshot` adds the published commit ID and the digest of the exact serialized
envelope bytes to the binding. The outer commit ID cannot appear inside its own envelope.
Readers keep the expected accepted snapshot across the key request and compare every field.
Even JSON whitespace changes alter the exact envelope digest.

The obsolete repository-password envelope is unsupported. It cannot be opened or resealed by
the supported library API. Retaining an already accepted opaque object in Git ancestry does
not classify it as plain content or make it eligible for original recovery.

## Original delivery and recovery

Clients request an exact snapshot from the account-authenticated `original-key` endpoint.
The service checks current repository membership, membership epochs, revocations, and
eligible owner, committer, commit-grant or session-grant access. An active session
grant can authorize this exact snapshot without a per-commit explicit row. Repository
visibility alone does not grant originals.
Responses require authenticated TLS outside loopback, `private, no-store`, the complete
matching snapshot, and one content key. Personal private keys are not delivered to clients.

`ContentKeyResponse::open_layer` verifies the response, exact envelope bytes and session
binding before decrypting and parsing the original layer. The private metadata's owner,
logical session and original committer evidence must match the authenticated binding.
Keys remain request-scoped and are excluded from debug output and persistent recovery state.
The platform holds account keys and can decrypt; this is not a platform-blind design.

The private layer is UTF-8 JSON with `version: 1`, `session`, `metadata`, `path_aliases`,
`source_committer`, and optional `protected_values`. `session.log` and `session.view` contain
Base64 canonical AgentGit envelope JSONL, including original LOG events omitted from VIEW.
VIEW must reference events reachable from LOG. Raw Git committer evidence and optional account
attribution stay inside this layer; public Git headers use a generated privacy identity.

Before sealing, publication hydrates local secret placeholders from a frozen dictionary,
rebuilds event hashes and evidence references, and validates the complete original. Missing
secret mappings refuse encryption. `protected_values` contains only literals used in this
snapshot, allowing later local settlement to protect recovered values without importing the
source dictionary's key, rule names or allowances.

Input budgets bound snapshot sequences and metadata before selection and encoding. The final
serialized original layer must fit within 4 MiB minus the authentication tag; the complete
envelope must fit within 8 MiB. Oversized input fails before upload. Referenced paths never
cause independent file or attachment bodies to be collected.

`agit privacy unlock OWNER/REPO@REF --workspace DIRECTORY` installs validated local recovery
under `<git-common-dir>/agit/privacy-recovery/<commit>` without changing branch refs. A caller
supplies the local workspace; historical paths never select write destinations. Restoration
clears source-device runtime claims, baselines and worktree observations. `resume` can request
originals automatically and uses the same validation and continuity checks. Locally recovered
files remain user data after server-side revocation.

Independent `fork` uses an authorized original when available, or sanitized context after an
explicit access denial or absent account credentials. Service or network errors remain errors.
The fork records its selected context and belongs to the forking account. Ordinary clone
preserves source session ownership and publication authority.

## Projection, export and share

`domain::privacy_publication::project_session` validates complete LOG/VIEW before projection.
It transforms each LOG event once and selects those same bytes for VIEW. Public event hashes
are computed after rewriting. The public value is
`{version: 1, session: {log, view}, metadata, report}` with canonical envelope JSONL strings.

Recognized Claude, Cursor and Codex records preserve safe text and authorized file-tool
content. Tool outputs require unambiguous call provenance and allowed paths. Shell commands,
unknown control records and unsupported attachment bodies become public omission markers;
the complete original remains private. Path aliases, replacements and the final semantic
secret scan apply to content and presentation metadata. Source paths are never public IDs.

Reports identify source LOG positions, omission reasons, and optional policy-bound decisions.
A decision's `rule` points into the effective policy, such as `repository.exclude[1]` or
`replacements[0]`. Its public path alias does not authorize execution or select a workspace.
`metadata.privacy` is sanitized display data validated by `privacy_metadata::validate_public`.
Git metadata and the envelope's public metadata reconstruct to the same canonical bytes.

`agit export OWNER/REPO@REF --privacy` emits sanitized selected content in the requested
format. `agit share` sends a structured sanitized value to `POST /api/shares/privacy` with
`format_version: 2`. Both public and encrypted links carry the same selected sanitized
records and metadata. Encrypted links use outer AES-256-GCM transport with unpadded Base64URL
nonce/ciphertext and a key in the link fragment. A link does not carry an original layer or
account content key. Share expiry, view limits and access passphrases remain link controls.
The CLI requires a versioned acknowledgement and does not fall back to a legacy endpoint.

## Mixed Git publication

`SessionPublicationPlan` resolves registered authority before `ProjectedHistory::prepare_sessions`
creates candidates. Plain commits and already accepted ancestry retain their IDs. Each new
encrypted snapshot has a minimal public tree containing sanitized session storage, metadata
and `privacy/envelope.json`. Raw originals and native archives cannot accompany that tree.
Generated commits preserve parent relationships while referencing accepted or generated objects.

Preparation uses an isolated object store under `<git-common-dir>/agit/privacy-publication`.
It has no object alternates into private source history. Authenticated mappings and immutable
candidates bind source/public commits, parents, source/public session IDs, policy, destination,
and owner key. Retries reuse prepared ciphertext. Accepted history is immutable across policy
changes, owner key rotation, session continuation and local candidate-cache loss.

A `SessionPublicationManifest` declares introduced plain and encrypted snapshots, registration
revisions, and preserved commit IDs. Its object set must equal the inspected publication.
Captured plain LFS payloads are verified and included in complete review. The preview reads
captured bytes, so later live-cache changes cannot alter confirmed content.

Explicit push displays the destination and complete outgoing public snapshots, metadata,
omissions and plain file content before confirmation. It also retains `publication-preview.json`
in private local storage. `--dry-run` prepares an existing registered destination without upload;
a new destination needs creation confirmation before its immutable identity can bind envelopes.
`--audit` adds separate model review and terminal confirmation. Identity, authority and captured
source changes after review stop publication. See [push](commands/push.md) for command behavior.

## Confirmed Git receive

Strategy registration uses `PUT /api/agents/{owner}/{name}/privacy/strategy` with
`expected_repository_id`, `policy_version: 1`, effective `policy_digest`,
`publication_format_version: 2`, and
`summary: {"session_modes": ["plain", "session_owner_v2"]}`.
The response must match the repository, policy and format. Its mandatory policy requires
`require_session_config: true` and `require_envelope: false`, because a repository may be mixed.

The mandatory-policy digest hashes compact UTF-8 JSON in Rust struct field order:
`version`, `source`, `require_envelope`, `require_session_config`, `publication_format_version`,
`protected_paths`, and optional `content_policy_digest`. This carrier restriction is distinct
from local content exclusions. It is not the canonical sorted-object encoding used by envelopes.

Each bounded Git ref batch reads the complete authenticated `ls-remote --refs` advertisement,
retains unchanged refs, and applies its frozen target IDs. A ref digest hashes entries sorted
by full ref name, each encoded as `name + NUL + oid + LF`, with a `sha256:` prefix. Duplicate
or malformed refs fail; symbolic HEAD and peeled tag pseudo-refs are excluded.

`POST .../privacy/publication/preview` receives:

- `manifest`: the complete reviewed session manifest.
- `snapshot_digest`: SHA-256 of its compact serialized JSON.
- `policy_digest`: the effective local policy digest.
- `receiver_scope`: `sessions:{visibility}:{immutable_repository_id}`.
- `envelope_format_version`: `2`.
- `expected_refs_digest` and `target_refs_digest`: complete before/after maps.

The response echoes the bindings and adds `preview_id`, `expires_at` and
`mandatory_policy_digest`. Confirmation sends the same request plus `preview_id` to
`.../publication/confirm`. Every field and expiry is checked before receive. Git receives
`X-AgentGit-Privacy-Preview-Id` and `X-AgentGit-Privacy-Receiver-Scope` through URL-scoped
environment configuration, outside argv and saved Git config. Redirects are disabled.
Each receive is atomic; retries retain its receipt and subsequent batches use fresh receipts.

The receiver must consume the receipt with durable ref mutation, validate introduced objects
against registered modes, preserve accepted ancestry, and admit exact snapshot/default-reader
records atomically. CLI fixtures exercise these bindings with a simulated receiver; hosted
acceptance requires a backend implementing the same contract.

## Mandatory policy sources

The effective CLI policy loads system and device-managed exclusions before applying repository
allowlists and branch restrictions. These sources contain exclusions only; they cannot authorize
a workspace, add an include pattern or weaken another source. They apply to every repository and
unbound live share on this device. In this session-only phase, file exclusions control captured
tool text and path aliases, without collecting the referenced file.

The optional system policy is discovered at:

| Platform | Path |
| --- | --- |
| macOS | `/Library/Application Support/AgentGit/privacy-policy.json` |
| Other Unix | `/etc/agit/privacy-policy.json` |
| Windows | `%PROGRAMDATA%\AgentGit\privacy-policy.json` |

Additional organization/device sources are listed in `$AGIT_HOME/privacy-policy-sources.json`:

```json
{"version": 1, "sources": ["/managed/organization-privacy.json"]}
```

Each source uses the same schema as the system policy:

```json
{
  "version": 1,
  "id": "organization",
  "revision": "2026-09-23",
  "exclude": ["src/customer/**", "docs/internal/**"],
  "memory_exclude": ["personal/**"]
}
```

Patterns use the repository policy's relative-path syntax. File patterns apply relative to each
authorized root; memory patterns use memory-relative paths. The system source is evaluated first,
followed by configured sources in manifest order, then repository and branch exclusions. Built-in
sensitive-file exclusions always remain effective. Source IDs must be unique. A listed source
must exist and be a valid regular file; missing files, symlinks, malformed versions and unknown
fields make that optional local processing attempt unavailable. Each source or manifest is limited to 256 KiB, the manifest to 32 sources,
and each source to 1024 patterns of at most 1024 bytes each.

`privacy policy show` includes effective mandatory rules; editing the repository policy stores
only its local authorization. A `mandatory` field in repository JSON does not install a source.
Source IDs, revisions and rule contents participate in the effective digest, so rule changes
invalidate post-review verification even if the repository file is
unchanged. Source documents, paths and local workspace roots are not uploaded; envelopes retain
the digest. Device setup must install trusted sources separately after clone. This local loader runs without cloud rule discovery.

## Automatic publication and durable receipts

Automatic publication requires the repository's `push.auto` preference and authenticated
write authority. Project enrollment records its account, immutable destination and audience;
it does not choose a repository-wide encryption mode. Children use `AGIT_AUTO_PUSH=1` and
remove inherited `AGIT_YES`. Each attempt resolves registered session authority and prepares
and checks captured content again. Enrollment and publication settings remain device-local.

Local ordinary receipts use version `2` and preserve source/public identity. Encrypted receipts
use version `3`, explicit `session_owner_v2`, and a complete accepted snapshot. Retained object
lookup requires an authenticated ledger mapping and isolated storage; current branch tips do
not substitute for saved publication identity.

RC settlement records publication intents in `<git-common-dir>/agit/rc-publications/` before
transport. Distinct candidates retain distinct notification IDs and outcomes; exact retries
reuse their own immutable candidate. Git success and a local receipt do not prove notification
delivery. Pending entries survive restart until a matching durable receiver acknowledgement.

The shared delivery contract is `agit_peer::publication`. An enrolled executor sends
`Delivery { grant_id, controller_generation, notification }` to
`POST /api/peer/publications/confirm`. The notification binds executor, immutable repository,
public commit, projected session and capture coordinates. Private source IDs and session text
stay local. The receiver checks current authority and admitted Git history. Callers recheck
their live execution guard after HTTP before recording acknowledgement.

Controllers keep local save, Git publication and receiver acknowledgement separate.
`commit.localSettled` and `session.publication.changed` trigger progress or rescan; neither is
proof of remote admission. `session.publication.deliver` pages durable records with bounded
work and returns retained acknowledgements after reconnect. Only matching live capture
coverage can advance a stream watermark.

Reclamation requires a newer durable acknowledgement for the same capture authority and
coverage, with both raw source ancestry and public ancestry containing the older commits.
Git replacements, grafts or shallow overlays cannot establish that relation. Pending records,
the source tip and the covering receipt remain; missing objects defer cleanup.

## Interoperability checks

The synthetic fixtures `tests/fixtures/privacy-session-owner-envelope.json` and
`tests/fixtures/privacy-session-owner-browser-envelope.json` pin both encryption directions,
canonical Unicode/numeric-looking key order, and exact content-key response bindings.
Their published test keys must never protect real data.

```sh
cargo test --locked --lib domain::privacy_envelope::tests
node scripts/privacy-envelope-interop.mjs tests/fixtures/privacy-session-owner-envelope.json \
  /absolute/path/to/website/node_modules/libsodium-wrappers-sumo/package.json
```

`examples/privacy-envelope-fixture.rs` emits fresh synthetic Rust data. The interop script's
optional final output path emits browser-encrypted fixture data. Focused projection, mixed
publication and recovery checks cover complete original retention, immutable retries, access
denial and native continuation; they do not establish compatibility with a deployed service.

## Original authorization protocol

Mutable policy lives outside `SessionPrivacy` and publication configuration revisions.
The shared Rust types are in `domain::session_grants`. The session path is
`/api/agents/{owner}/{name}/sessions/{logical_session_id}`; requests retain the immutable
repository ID in `X-AgentGit-Expected-Agent-Id`, even when a repository route changes.

`GET original-grant-policy` returns `{agent_id, session_id, grant_scope, policy_revision,
grant_revision}`. The policy's grant revision is the session grant counter; commit
targets have separate counters. `POST original-grant-policy` takes `{target, grant_scope,
expected_policy_revision, expected_grant_revision, idempotency_key}` with a session target.
The result is `{policy, converted_recipients, converted_snapshots}`. A repeated disable
returns authoritative state without repeating grant conversion. Reverse transitions
return `kind: "unsupported"` after authentication and owner authorization.

Grant targets have one of these strict shapes:

```json
{"scope":"session","agent_id":"REPOSITORY_UUID","session_id":"LOGICAL_SESSION_ID"}
{"scope":"commit","agent_id":"REPOSITORY_UUID","session_id":"LOGICAL_SESSION_ID","published_commit_id":"COMMIT_OID","envelope_digest":"sha256:DIGEST"}
```

`GET original-grants` uses `scope` and `expected_policy_revision` query parameters;
commit scope also requires `published_commit_id` and `envelope_digest`.
`POST original-grants/preview` takes `{target, expected_policy_revision}` and returns
`{id, target, policy_revision, grant_revision, members, expires_at}`. Members contain
immutable account IDs and display usernames; the server freezes their membership epochs.
`POST original-grants` takes `{target, expected_policy_revision, expected_grant_revision,
idempotency_key, action}`. Actions are `grant` with `preview_id`, `revoke` with `account_id`,
and `stop_explicit`. Results carry `{target, policy_revision, grant_revision}`; inventory
responses additionally contain `readers`.

Reader state distinguishes commit and session explicit grants, revocations and membership
invalidation. Either active revocation denies access. Each positive entitlement requires
its own valid membership epoch. Default suppression affects owner/committer defaults without
preventing a new explicit grant. Session fields are inactive under commit policy. Publication
status returns the matching `grant_policy` separately from capabilities, and requires policy
provenance when a reader is present. Original-key requests still bind one accepted snapshot
and exact envelope; authorization never introduces a session decryption key.
