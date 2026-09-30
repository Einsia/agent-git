# Privacy envelope wire format

This format applies to encrypted repository publication and explicitly encrypted
artifacts. A Hub repository's immutable `encryption_enabled` setting selects its
publication pipeline. Ordinary repositories retain native history and do not use
this envelope. See [the fixed-mode contract](repository-password-protocol.md#fixed-repository-mode).

`domain::privacy_envelope::PrivacyEnvelope` encrypts private bytes with a fresh random AES-256-GCM
object key. It wraps that key using the recipient's X25519 viewing public key and libsodium's
`crypto_box_seal` format, implemented by [RustCrypto crypto_box](https://docs.rs/crypto_box/0.9.1/crypto_box/).
The viewing password and private key are unnecessary for publication. Readers unwrap the object key
with the viewing private key and authenticate the envelope before using any decrypted bytes.

The JSON object has these fields:

| Field | Encoding |
| --- | --- |
| `format_version` | Integer `1` |
| `policy_digest` | `sha256:` followed by 64 lowercase hex digits |
| `snapshot_digest` | Same digest encoding, identifying the frozen input snapshot |
| `public_projection` | Policy-processed public JSON |
| `private_payload.algorithm` | `aes-256-gcm` |
| `private_payload.nonce` | 12 bytes, standard padded Base64 |
| `private_payload.ciphertext` | AES-GCM ciphertext followed by its 16-byte tag, standard Base64 |
| `private_payload.wrapped_keys` | Nonempty array of recipient wrappers |
| `attachments` | Array of `{id, digest, size, kind}` descriptors |

Each wrapper is `{recipient, algorithm: "x25519-sealed-box", ciphertext}`. The recipient is a unique
safe token, and its ciphertext is the standard Base64 encoding of the 80-byte sealed object key.
Safe tokens contain ASCII letters, digits, dots, hyphens or underscores and are at most 128 bytes.
Attachment IDs are unique safe tokens. A descriptor's digest and size identify the exact stored
attachment bytes; `kind` is `public` or `private_encrypted`. A private descriptor must describe
ciphertext, never a digest of plaintext. Object transport must validate those bytes against the
descriptor. Public JSON and each ciphertext have a 4 MiB limit; the complete envelope has an 8 MiB
limit. An attachment has a 64 MiB limit. Unknown protocol fields are rejected.

AES-GCM additional authenticated data (AAD) is UTF-8:

```text
agit-privacy-envelope-v1<NUL>sha256:<lowercase hex digest>
```

The digest hashes canonical JSON containing exactly the following fields:

```json
{
  "algorithm": "<private_payload.algorithm>",
  "attachments": [],
  "format_version": 1,
  "policy_digest": "<policy_digest>",
  "public_projection": {},
  "snapshot_digest": "<snapshot_digest>",
  "wrapped_keys": []
}
```

Use the actual values from the envelope, including the wrappers inside `private_payload`.
Canonical JSON contains no insignificant whitespace. Object keys are sorted lexicographically by
UTF-8 bytes at every depth; arrays retain their order. String encoding uses JSON escaping with
literal UTF-8 for non-control characters. Only integers in `[-9007199254740991, 9007199254740991]`
are supported. Floating-point values are rejected. In JavaScript, generate sorted object members
directly: `JSON.stringify` on a newly sorted object still reorders integer-like keys. The executable
reference is `scripts/privacy-envelope-interop.mjs`.

Changing the public projection, policy, snapshot, recipient wrappers or attachment manifest fails
authentication. A browser supplies this AAD as WebCrypto's `additionalData`. Decryption without it
fails. Envelope validation covers the cryptographic structure; callers must independently enforce
the repository policy, scan the final public projection and validate referenced object bytes.

Generate a synthetic fixture from the CLI library and verify it using the website's libsodium
installation, without starting a server:

```sh
cargo run --quiet --no-default-features --features secret-vault \
  --example privacy-envelope-fixture > /tmp/cli-privacy-envelope.json
node scripts/privacy-envelope-interop.mjs /tmp/cli-privacy-envelope.json \
  /absolute/path/to/website/node_modules/libsodium-wrappers-sumo/package.json
cargo test --lib domain::privacy_envelope::tests --no-default-features --features secret-vault
```

The fixture's deliberately public key protects synthetic test data only. The optional third script
argument writes a browser-produced fixture; the checked-in `tests/fixtures/privacy-browser-envelope.json`
pins browser-to-CLI decryption, Unicode key ordering and numeric-looking JSON key ordering.

The recoverable private layer is UTF-8 JSON with `version: 1`, `session`, `metadata`
and `path_aliases`, with optional `protected_values`. `session.log` and `session.view` are standard Base64 encodings of canonical
AgentGit envelope JSONL; both are retained, including LOG events omitted from VIEW. Metadata holds
the original session metadata object. `path_aliases` maps logical aliases to historical source paths.
This entire object is encrypted; source paths and original metadata never belong in public descriptors.

Before publication, local secret placeholders are hydrated from an immutable dictionary snapshot.
Session event hashes and recovered-evidence references are rebuilt together, while selections retain
their saved-event coordinates. Unresolved placeholders refuse new encryption with an instruction
to restore the dictionary or unlock the source publication; they are not claimed as recovered text.
`protected_values` is a sorted, unique array of secret literals present in the original session or
metadata. It contains only values used by this publication, not the repository's complete dictionary.
Omitting the field means an empty set. A selected share retains only its selected records' and
metadata's values. These values let a receiving CLI protect the recovered plaintext on future
settlement and publication, including low-entropy registered secrets.

`PrivacyEnvelope::seal_layer` validates and encrypts the layer. `open_layer` authenticates before
parsing it, checks canonical session envelopes and requires VIEW events to be reachable from LOG.
The serialized layer must fit inside the private-payload budget, including the AEAD tag. Invalid
content fails as a whole.

Publication admits saved LOG and VIEW sequences under a 32 MiB limit per sequence and a
32 MiB limit on their unique event bodies. Saved metadata is limited to 1 MiB. Privacy export
applies these checks before selecting records; saved shares use the same limits, with explicit
`--full-log` reading only LOG. Live shares use bounded native snapshots and limit both native
bytes and their wrapped session sequence to 32 MiB. Oversized or incomplete input fails before
upload, and a refused export preserves an existing output file.

Hydration retains its bounded output, and private-session size is checked while rebuilding
envelopes and before allocating Base64 copies. These admission checks do not increase the
private-layer limit: its final JSON, including metadata, path mappings and protected values,
must still fit within 4 MiB minus the authentication tag. Receivers enforce their own limits.

`PrivateLayer::restore` creates a fresh private staging directory with standard v1 session files,
metadata and the reverse alias map. The caller supplies the new workspace;
historical source paths never select write destinations. Restored metadata uses that workspace and
clears source-device runtime claims, baseline counters and worktree observations. The temporary
directory is removed when its owner is dropped. A caller must deliberately install it before
resuming; the stored transcript remains historical evidence rather than current execution approval.
Unlock imports `protected_values` into the receiving repository's encrypted local dictionary before
installing the recovery cache. It creates local handles and adds protection; source rule names,
allowances and dictionary keys are not imported. Viewing private keys remain in memory
by default.

`agit privacy unlock OWNER/REPO@REF --workspace DIRECTORY` reads the selected envelope's
recipient and requests `/api/agents/{owner}/{name}/privacy/keys/{recipient}?ref={commit}` from
the locally pinned source Hub. The response must match immutable repository ID, commit, session
and recipient. Public reads may be anonymous; private reads retain repository ACLs.

The CLI reads the repository password locally without echo and unlocks the password-wrapped
X25519 key. The password protocol and configuration APIs are documented in
[repository-password-protocol.md](repository-password-protocol.md). Passwords, derived keys and
plaintext private keys are not sent to the Hub or written into recovery files.

After decryption, validated recovery data is installed under
`<git-common-dir>/agit/privacy-recovery/<commit>`, with the publication blob digest and a manifest.
The operation leaves branch refs unchanged. An omitted workspace uses the configured local privacy
workspace, never a source metadata path.

`--remember-for HOURS` (1-720) explicitly retains the key in the OS credential store after the
publication and staged recovery are validated. There is no file fallback. `--use-saved-key`
checks live read access and the exact publication/key record before using the saved key. Matching
uses canonical Hub, immutable repository ID, recipient and public key; account identity and slug
are not reading-key scope. Expiry is enforced on access and does not extend during reuse.

`agit privacy forget-unlock OWNER/REPO` removes the repository slot using its local immutable
identity without network access, including after logout. Deletion retains recovered session files.
Password rewrapping preserves key identity; another recipient cannot reuse that saved key.

Run `agit resume OWNER/REPO@BRANCH` after unlocking its tip. Resume validates a local `recovery.json`
manifest against the selected envelope and every recovered file, then installs a new runtime in
the selected workspace using the same installation rules as ordinary resume. The original private
VIEW supplies context; LOG-only conversation stays outside that context. A required bootstrap
record can come from the same verified private LOG. Same-runtime installation preserves native
roles, models, tool calls and results, thinking and other native fields, subject to ordinary session
identity, working-directory and unfinished-call adaptation. Cross-runtime conversion retains its
ordinary loss warnings. Branch memory, skills and independent artifacts are not recovered or
installed during this path.

The runtime link pins the recovery commit and manifest digest and records
`privacy_recovery_format: "native-v1"`. Missing or changed recovery data blocks continuation and
settlement instead of silently substituting public history. File-only commits may advance the
branch while that binding continues to reference the original snapshot. The next turn settlement
retains the original private LOG and VIEW as their respective prefixes and appends only new runtime
events after the verified baseline. It then clears both recovery fields. The new local snapshot
drops the parent's envelope, which authenticates only the published parent. Publication applies
its privacy policy and generates a new envelope for the continued snapshot.

A bound link without a format marker represents legacy quoted history. Resume replaces it from
the original cache only when the baseline is intact and no conversation has been added; runtime
bookkeeping such as titles does not prevent replacement. The old transcript remains available and
its link records the successor. A legacy runtime with unsettled conversation must be committed
before ordinary replacement. That commit retains the quoted context; already committed quoted
history is not automatically migrated.

Legacy quotes carry `agit_recovered_evidence`, a versioned reference to their original runtime,
session and content hash in the private LOG. Publication renders each quote from that source
record's checked public projection. Excluded tool output and bootstrap data remain excluded inside
quoted history. Missing or malformed references become omission markers. Legacy textual evidence
and quotes converted into native message blocks follow the same rule; a live transcript without
the original source evidence cannot publish their private bodies.

## Session publication projection

`domain::privacy_publication::project_session` validates the complete canonical LOG/VIEW pair
before generating public events. It transforms each LOG event once and selects those same
bytes for the VIEW. Public object hashes are computed after rewriting. The public session uses
the Claude message schema with a synthetic session identity; original runtime records, identities,
metadata and reverse aliases remain in the authenticated private layer.

The public value is `{version: 1, session: {log, view}, metadata, report}`. LOG and VIEW are
canonical envelope JSONL strings. `snapshot_digest` is the canonical JSON digest of that public
value. Reports identify source LOG positions and omission reasons without copying source paths.

Claude/Cursor messages and Codex response items support text and file tool calls. A recognized
file tool must identify one allowed file; relative paths require an explicit historical working
directory. Call IDs are scoped to the source runtime/session, and duplicate IDs cannot authorize
outputs. Excluded file calls and their results become omission markers. Shell commands, unknown
native records, and unsupported attachment bodies are omitted publicly and retained privately.
This conservative coverage is explicit in the report. Referenced files are never opened implicitly.

Path aliases and configured replacements are applied before the final semantic JSON secret scan.
Replacement output receives another path pass, so a replacement cannot introduce an unchecked
absolute path. Stable aliases are stored outside the tracked tree through `PathAliasStore`.

`agit export OWNER/REPO@REF --privacy` renders this public session in the chosen export format.
`--format privacy-envelope` emits the complete envelope and selects a repository key through the selected snapshot's verified accepted publication. Supply `--viewing-public-key BASE64` for an offline export to a known X25519 recipient.
Encrypted export requires a complete snapshot and refuses VIEW-only or turn-range selection.
Public export preserves those selections by their original event coordinates before rendering.
Frozen snapshots are read by complete commit ID, including when a branch has the same spelling;
named refs cannot redirect the exported public or encrypted content.
The focused `cargo test --test privacy_export` check invokes the CLI, compares both public outputs,
opens the resulting private layer and verifies the selected source history remains unchanged.
`cargo test --test privacy_push` also covers the CLI publication, fresh-device clone, authorized
unlock, safe resume, settlement and re-publication sequence against an HTTP Git receiver. The
authorization service is simulated; the test validates the CLI flow without claiming deployed
website or backend interoperability.

## Structured shares

`agit share` sends `POST /api/shares/privacy` with `format_version: 2`. A public share carries a
JSON share value containing the checked `presentation`, selected public session LOG/VIEW and
projected metadata. An encrypted share wraps the standard `PrivacyEnvelope` in the existing
browser-compatible AES-256-GCM transport; the outer nonce and ciphertext use unpadded Base64URL,
and the outer key remains only in the link fragment. The inner envelope's private layer contains
the selected original records and is sealed to the accepted source publication's repository viewing public key. Unselected LOG
events and unrelated reverse mappings are absent from that layer. Independent repository files
and attachments are outside the share payload.

Before projection, sharing resolves the current account's mandatory Hub sources and the source
repository's sources through the authenticated resolver below. Native sessions without a repository
retain the account sources. After confirmation, the CLI refreshes every resolved scope and checks
the local policy and repository identity again before uploading.

The server stores the payload as an opaque string and returns `format_version: 2`. The CLI refuses
an acknowledgement without that version and refuses recipient-key drift after preparation. A
legacy share endpoint is never used as a fallback. The website should parse the structured public
share value before rendering; for encrypted shares it first decrypts the outer transport, then
uses a repository viewing key only when its accepted source publication context is supplied.
Standalone share pages cannot infer that context from a recipient. The public projection remains
readable after outer link decryption. Encrypted live/standalone creation requires an accepted
publication first; explicit-key export remains available. The share access passphrase stays separate
from the repository viewing password.

## Projected Git history

Local candidate previews include a `rule` reference, for example `repository.exclude[1]`,
`branch.exclude[0]` or `mandatory[0].exclude[2]`. Indices are zero-based positions in the effective
policy shown by `privacy policy show` and are interpreted together with that preview's policy
digest. Rule references identify the winning decision without copying its private pattern, root
path or source ID. References without a pattern index, such as `repository.include` or
`path.authorized_roots`, identify a failed eligibility check.

Session processing reports optionally include `details` with `policy_version`, `policy_digest`
and `decisions`. Each decision has `record` (zero-based source LOG position, or null for metadata),
`path` (rewritten public alias or null), `rule`, `action` and a positive `matches` count.
Actions are `allow_path`, `exclude_source`, `rewrite_text` and `mask_secret`. Path decisions are
deduplicated per record/path/rule/action; rewrite and secret counts aggregate matching content.
An `allow_path` decision approves a path candidate, not the entire tool record; unsupported or
ambiguous tool provenance can still produce an omission for that record.
Custom rewrites use `replacements[N]`; secret checks use `secret_scan` without exposing matched
literals. Report aliases pass the same rewriting and scanning as session content.
References describe the effective policy used for that snapshot. A detached export/share folds
branch exclusions into the repository exclusion list in branch-name order; its rule positions
and policy digest describe that narrowed policy, rather than an arbitrary current branch.

`ProjectionReport::validate` checks record positions and binds detail policy version/digest to
the enclosing envelope. Content inspection omits only the verified generated policy digest;
report paths and other decisions remain scanned. Absent details remain valid for older published snapshots; readers must
preserve that absence when reconstructing existing objects. Details participate in the public
projection digest and authenticated data. Full encrypted snapshots retain original LOG/VIEW and
metadata for all decisions; public-only outputs include no original data, and selected shares
retain only their selection. The CLI states that recovery scope alongside its source report.

Cursor messages use their native top-level role. Their text passes the same path rewriting,
replacement and secret checks as other recognized messages. A Cursor tool call without an ID
can publish its own input only when its file path and tool schema are allowed; it cannot authorize
a later tool result. Empty call IDs and unsupported control records never establish provenance.
The encrypted private session retains the original records, including omitted public content.

`domain::privacy_git::ProjectedHistory` rebuilds the complete captured ancestry in a separate
Git object store below `<git-common-dir>/agit/privacy-publication`. Selected session branches
retain their required ancestors, but `main` is not added as an independent publication root.
Explicit file-line targets are refused while repository-file publication is deferred. That store has no alternates
pointing at the original history. Generated commits carry fixed public authorship and messages;
their parents reference generated commits only. Each snapshot contains public session storage,
public metadata and `privacy/envelope.json`. Public session identities and version tags belong
to the projected history. Original branch commits and annotated tag objects are never copied.

The Git `session/meta.json` and the envelope's public `metadata` are the same JSON object.
Its storage fields retain synthetic runtime/session identities and no local workspace authority.
The optional `privacy` field contains the same processed presentation metadata used by export
and share, including its `schema_version: 1`. Minimal mode preserves logical workspace aliases
and session facts; explicit `metadata: "project"` additionally retains sanitized origin and
worktree counts. Source branch/head/status digests, native identities and raw working directories
remain private. All presentation strings pass path rewriting, replacements and secret scanning.

Readers validate `metadata.privacy` with `privacy_metadata::validate_public` and reconstruct the
exact metadata bytes with `privacy_metadata::git_metadata`. Unknown presentation fields are
rejected. An absent `privacy` field remains accepted for existing minimal synthetic snapshots.
Presentation metadata is display data; it never selects a workspace, runtime ownership or policy.

Publication carries session records and metadata. Repository files such as `AGENTS.md`, memory,
skills, documents and artifacts are excluded even when their paths match the local allowlist.
Their bodies are not read, rewritten, encrypted or restored by the session publication processor.
The public projection has no `files` map and the private layer has no attachment collection.
Generated session envelopes have an empty external `attachments` list; session recovery rejects
nonempty lists. File content already present in LOG/VIEW remains session evidence and follows the
same public text restrictions and private encryption as other records. Paths mentioned in records
never cause the referenced filesystem contents to be collected.

The local cache binds source snapshots, generated public bytes, private-layer fingerprints,
policy, recipient, destination and projected parents. Repeated preparation reuses randomized
ciphertext rather than changing remote object identities. Reverse aliases include only those
used in each snapshot, so unrelated later paths cannot invalidate earlier ciphertext. A lock
protects the generated refs during review and publication; source refs and policy are rechecked.
Policy or recipient changes may produce a different history and require a new publication branch,
destination or explicit migration. Before strategy registration, the CLI checks the current remote
refs against the captured graph: existing branches must fast-forward and existing selected tags
must retain their object IDs. Incompatible history is refused without registering the new strategy,
requesting receive receipts or uploading Git objects. Each actual batch still obtains a fresh
ref-transition receipt, so this preflight does not replace the receive-time checks. This mechanism
never force-pushes over published history or removes content already published under an older policy.

Preparation also keeps an authenticated encrypted index under the local publication store's
`.git/privacy-preparation/`. Its separate repository-local key is unrelated to the repository viewing
key. The index skips source materialization, hydration and projection for an unchanged snapshot
only when build, policy, branch, recipient/destination, parents, persona, registered rule contents,
authenticated dictionary and allowance state still match. It rechecks recorded path decisions
and alias bindings without reading referenced file bodies. Unrelated new path aliases do not
invalidate existing bindings. Cached commits still undergo complete generated-tree reconstruction
and publication inspection; the full selected history remains available for preview.

New source snapshots are processed as complete LOG/VIEW pairs because later records can change
earlier tool-call provenance. A dependency change triggers reprocessing. A snapshot whose own
projection changes the dictionary becomes eligible for fast reuse only after a stable pass.
An upgrade discards the acceleration index while preserving the ciphertext cache for checked
regeneration. Invalid cache authentication or mismatched object mappings stop preparation.


## CLI publication

Push resolves the Hub identity and explicit mode before selecting a pipeline. A
missing mode field is an unsupported backend, never an ordinary-mode fallback.
An existing mode ignores creation defaults and cannot be changed by push flags.

For an ordinary repository, push captures raw source ancestry, selected
branches, `main`, reachable tags and LFS. Commit IDs and session identities remain
unchanged. The command confirms the destination, sends the captured objects using
ordinary Git/LFS receive, and retains ordinary receipts. It requests no viewing key
or privacy strategy. An explicit first push can create this destination using
`--encryption=false`; automatic publication requires an already confirmed destination.

Default inspection excludes Git objects reachable from the actual destination's live
advertised refs, using only roots available in the isolated local object store. It
checks new commit and tag bodies, new blobs throughout unpublished history, and the
verified bytes behind new LFS pointer objects. Remote LFS availability alone cannot
waive payload inspection. The complete frozen plan still controls source verification
and upload refs. The baseline is bound to the destination URL and immutable repository
identity; a changed destination or source invalidates the inspection.

First publication, empty or unavailable advertisements, and unverifiable baselines
fall back to full inspection. Encrypted scope is computed after projection from the
public object graph, so changed policy or key material that creates new objects is
included. Manual, automatic and dry-run push share this behavior. --audit always
reviews complete selected history and payloads. Default incremental push does not
retroactively apply updated scanner rules to remote content, and retains no scan
results across pushes. Server-side scanning policy is unchanged. See
[incremental inspection](push-incremental-inspection.md) for scope details and local measurements.

The following projection and key requirements apply to encrypted repositories.

`agit commit` settles original session records locally. When automatic publication is enabled,
its post-settlement child invokes `push` for the explicit session branch after releasing
settlement locks. RC settlement uses the same unattended publication configuration. These paths
share the same authoritative mode and destination as explicit push. Encrypted consent
additionally binds the policy and recipient; ordinary consent needs neither.

Encrypted `agit push OWNER/REPO@BRANCH` and `push --audit` prepare this projected history and
fetch the destination repository's current viewing public key. They inspect the selected generated objects using
the source repository's registered-secret rules. Ciphertext and generated identities are excluded
from heuristic inspection only through generator-owned object IDs; public messages and metadata remain scanned. Existing cache entries must reproduce the generated tree,
parents, authorship and message before reuse.

Push writes `publication-preview.json` in its private publication store. It contains the actual
public snapshots, omission reports, frozen refs and destination. Explicit pushes also display the
complete public LOG and any differing VIEW for every selected historical snapshot, alongside public
metadata, rewrite counts and omission reasons. Tool inputs and results remain complete; terminal
controls are displayed as escaped characters. Automatic pushes retain the generated preview file
and summary without printing the full session again. Explicit publication requires an
interactive confirmation or `--yes` after preparation. `--dry-run` performs authenticated read-only
Hub lookups and preparation without creating a destination or uploading objects. `--audit` adds
model review and always requires its separate terminal confirmation. `--allow-secrets` cannot
bypass privacy publication checks. Changes to source refs, policy, account, audience, remote
identity or viewing key after review block publication.

The source session branches remain local originals; remote branches and version tags point to
the generated public history. Push reports remote ref results without assigning those public tips
as tracking baselines for the private source branches. Existing plaintext repositories remain
ordinary; a different mode requires a new destination. Encrypted publication requires the strategy and receive-receipt
APIs below; unavailable or incompatible endpoints stop the upload.

After clone, publication can retain known ancestry fetched from the same destination. Each retained
snapshot must match the supported public schema, exact generated metadata, LOG/VIEW event files,
envelope and synthetic commit bytes. Its parents must already have passed
the same reconstruction. Extra files, native archives, altered modes or mismatched parents block
reuse. Public contents still pass secret inspection; the original ciphertext and historical policy
binding remain unchanged. New local snapshots use the current device's policy and viewing recipient.
This preserves fast-forward ancestry without granting source-device directories local authority.

Run `cargo test --test privacy_push` for the real CLI/HTTP Git receiver check. It covers confirmation,
dry-run, complete remote object inspection, exact private LOG recovery, repeat identity and
incremental fast-forward publication. The fixture supplies viewing-key, repository and receive-receipt APIs;
it does not establish acceptance by a deployed backend.

## Confirmed Git receive

After local confirmation and destination checks, the CLI registers a public strategy with
`PUT /api/agents/{owner}/{name}/privacy/strategy`. The body contains `policy_version: 1`, the
effective `policy_digest`, `publication_format_version: 1` and `summary: {"session_only": true}`.
The response must match the immutable repository ID, policy and envelope versions. Its mandatory
publication policy is a server-owned carrier restriction, separate from device content exclusions.
The CLI validates its version, envelope requirement and digest. The digest uses SHA-256 over the
compact UTF-8 JSON struct in this field order: `version`, `source`, `require_envelope`,
`publication_format_version`, `protected_paths`, followed by `content_policy_digest` when present.
Protected pushes require this final digest to match the resolved destination content rules below.
This encoding matches the strategy API; it is
not the canonical sorted-object encoding used by public session digests.

For each bounded Git ref batch, the CLI reads the complete authenticated `ls-remote --refs`
advertisement. It retains unchanged refs, then applies the batch's frozen object IDs to derive
the target map. A ref-map digest is `sha256:` plus the lowercase SHA-256 of entries sorted by full
ref name, each encoded as `name + NUL + oid + LF`. Include all ref namespaces, omit symbolic HEAD
and peeled tag pseudo-refs, and reject duplicate or malformed entries.

`POST /api/agents/{owner}/{name}/privacy/publication/preview` receives:

- `snapshot_digest`: the target ref-map digest, binding the reachable Git objects.
- `policy_digest`: the effective CLI policy digest.
- `receiver_scope`: `repository:{visibility}:{recipient_fingerprint}`, where visibility is
  `public` or `private` and the fingerprint includes its `sha256:` prefix.
- `envelope_format_version`: `1`.
- `expected_refs_digest` and `target_refs_digest`: the complete before/after ref-map digests.

The recipient fingerprint is the canonical JSON digest of `{"id": recipient_id, "key": public_key}`.
`public_key` is the standard-Base64 X25519 key. For account publication, `recipient_id` is
`digest_bytes(public_key.as_bytes())` with its colon replaced by a hyphen. Backend derivation
must retain this ID binding instead of hashing only the raw key bytes.

The response echoes these fields and adds `preview_id`, `expires_at` and
`mandatory_policy_digest`. The CLI requires an unexpired receipt and exact binding matches,
then sends the same request plus `preview_id` to `/privacy/publication/confirm`. The confirmation
must echo the same ID, bindings and mandatory-policy digest. Publication review stays in the CLI;
these requests validate the already confirmed content.

The Git subprocess receives `X-AgentGit-Privacy-Preview-Id` and
`X-AgentGit-Privacy-Receiver-Scope` in URL-scoped configuration environment entries. They are
absent from argv and stored Git configuration. Redirects remain disabled. Each receive uses
`--atomic`, and a credential retry retains the same receipt. Each subsequent batch acquires a
fresh receipt against the currently advertised refs. A fully up-to-date batch may need no receive
request; its unused receipt expires. Earlier acknowledged batches remain published if a later
batch fails, and the publication report retains those results.

The backend must reserve and consume receipts with its durable ref mutation, validate the complete
received graph, and independently check repository identity, policy, audience and viewing recipient.
Matching a caller-provided scope header alone does not establish the current audience or key.
The CLI does not fall back to receipt-free publication when these APIs fail.


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
fields block processing. Each source or manifest is limited to 256 KiB, the manifest to 32 sources,
and each source to 1024 patterns of at most 1024 bytes each.

`privacy policy show` includes effective mandatory rules; editing the repository policy stores
only its local authorization. A `mandatory` field in repository JSON does not install a source.
Source IDs, revisions and rule contents participate in the effective digest, so rule changes
invalidate publication consent and post-review verification even if the repository file is
unchanged. Source documents, paths and local workspace roots are not uploaded; envelopes retain
the digest. Device setup must install trusted sources separately after clone. This local loader
remains independent of authenticated Hub discovery.

## Authenticated Hub policy sources

Push, share and remote-bound privacy export resolve mandatory Hub rules before preparing session
projections.
The authenticated `POST /api/privacy/policy-sources/resolve` request is:

```json
{"version":1,"repository":"owner/repo","agent_id":"immutable-agent-id","request_id":"random-request-uuid"}
```

For a non-null repository, `agent_id: null` requires that the destination is absent and resolves
its namespace rules before creation. An existing repository must match its immutable ID.
`repository: null` and `agent_id: null` resolve the authenticated account's applicable namespace
rules, including account-scoped sharing of an unbound native session. A null repository with a
non-null agent ID is invalid. The server determines the account's namespace; a client cannot supply
another owner ID. The response echoes the nullable scope fields and contains exactly:

```json
{
  "version": 1,
  "hub": "https://hub.example",
  "repository": "owner/repo",
  "agent_id": "immutable-agent-id",
  "account_id": "authenticated-account-id",
  "request_id": "random-request-uuid",
  "owner_id": "immutable-namespace-id",
  "revision": "organization-rule-revision",
  "issued_at": "2026-09-23T00:00:00Z",
  "expires_at": "2026-09-23T00:05:00Z",
  "sources": [
    {"version":1,"id":"organization","revision":"rules-1","exclude":["src/internal/**"],"memory_exclude":[]}
  ]
}
```

The CLI verifies the selected account through `/api/auth/me` and compares all request bindings.
The response Hub must be canonical. Issuance may be at most 30 seconds ahead of the device clock;
the lifetime is positive, at most five minutes, and not expired. The response is bounded to
256 KiB and 32 uniquely identified sources. Source validation uses the exclusion-only device
schema above. Unknown fields, duplicate IDs, invalid globs and unavailable endpoints refuse
publication. Personal namespaces explicitly return a valid empty `sources` array when no rules
apply; missing policy data is not treated as an empty array.

Rules append to mandatory device restrictions before local allowlists are evaluated. A `hub-policy`
marker binds the canonical JSON digest of `version`, `hub`, `account_id`, `repository`, `owner_id`,
`revision` and `sources` into the effective policy. Request IDs and timestamps are excluded so
fresh responses for the same rules preserve consent and incremental preparation. Repository IDs
are verified separately: confirmed creation may replace an absent ID with the resulting immutable
ID, while refresh cannot replace an existing ID. Source arrays retain server order, and both
`exclude` and `memory_exclude` arrays are materialized before hashing.

The destination's content-policy digest is the canonical JSON digest of `version`, `owner_id`,
`revision` and `sources`, independently of the current actor. The strategy response's mandatory
policy must carry this value as `content_policy_digest`. Its inclusion in the mandatory-policy
digest binds preview, confirmation and receive to the same server rule revision. The backend
must compute it from its current rule registry at all of those boundaries.

Push refreshes the authenticated rules after confirmation and again against the materialized
destination before upload. Changed namespace identity, revision or rules requires a fresh preview
and renewed automatic consent. Copying a read-only source also retains that source repository's
mandatory rules. Subsequent pushes retain the rules of pinned sources and origin/upstream scopes,
including a copy's upstream repository. Scopes already resolved for the destination or immediate
copy source are not appended twice. This preserves the effective policy across local promotion.
Source remotes and local policy are checked before mutation; authenticated scopes and the source
snapshot are checked again after controlled destination materialization. Responses live in memory;
there is no offline fallback to an expired policy.
The source documents are neither imported as local authority nor uploaded with the session.

Share retains rules from the repository's immutable pin and its origin/upstream Hub scopes, even
when the share is uploaded to a different Hub. Each source uses credentials for its own Hub.
Unpinned remote sources first resolve an existing immutable repository ID. A local repository
without remotes resolves its name on the selected share Hub; a missing lookup still requires
the authenticated resolver to prove absence and namespace authority. Changed local policy,
repository pin or remote URLs invalidate the prepared share. Responses are refreshed after the
recipient check and immediately before upload; no cached or offline fallback grants publication.

Privacy export (`--privacy` or `--format privacy-envelope`) retains the same source pin and
origin/upstream rules and rechecks local policy, repository bindings and fresh source responses
before writing output. An export using a verified repository publication's key also applies account
rules and verifies the key again before output. An explicit `--viewing-public-key` selects the
recipient but does not bypass authenticated source rules. Only a repository without a remote
identity, origin or upstream can export using local/device rules alone; with an explicit key its
encrypted export is fully offline. Rule or recipient drift leaves the output file untouched.

This contract requires the corresponding backend resolver and mandatory-policy field. Export
does not upload its content or register Git receive receipts.

## Automatic publication consent

Successful pushes save a device-local source/public commit mapping under
`<git-common-dir>/agit/privacy-published/`, keyed by branch. The versioned receipt binds repository,
branch, source and published commit IDs, Hub/agent identity, URL, policy digest and viewing recipient.
Only a complete publication with matching ref acknowledgements writes this mapping. It records an
observed publication, not a promise about the remote's later state or permission for future pushes.

An RC child can receive `AGIT_RC_SUPERVISOR_PUSH_RESULT`, naming a caller-owned private request file.
The request binds a fresh request ID, source tip, branch, repository and destination identity. Push
validates this scope before transport and writes a matching receipt back only after publication
completes. A dry run, declined confirmation or failure cannot produce a successful result. The
supervisor also requires successful process exit and verifies the request ID; readable stdout
or an old receipt alone is not evidence that this child published its requested snapshot.

An optional `notification_id` binds the request to a durable local intent under
`<git-common-dir>/agit/rc-publications/`. Each intent binds one source, branch, repository and
immutable destination; retries retain its notification ID while using fresh child request IDs.
The local capture records the logical/native session, runtime and supervisor generation as
evidence, independently of peer/controller authorization. Push verifies the intent and saves its
exact projected candidate before transport, then persists the successful source/public receipt
before returning to the supervisor. A prepared candidate alone does not prove Git receive success;
interrupted attempts require retry or remote reconciliation against immutable admitted history.
Later source commits use separate entries, so branch advancement cannot overwrite pending
notification evidence. Corrupt, conflicting or full outbox state prevents that publication;
it does not authorize discarding earlier entries. These files stay outside published Git trees.

Different generated candidates for the same source have separate immutable notification IDs and
outcomes. Exact retries reuse an existing candidate with the same capture; they never replace an
uncertain candidate's contents. Candidate selection follows the ordinary consent, policy, recipient
and destination checks and does not authorize divergent Git history. Outbox format version 2 adds
the notification UUID to each source/destination filename; version 1 records remain readable under
their original filenames.

The private child result retains its exact original `request` and includes `publication`. When it
selects another retained candidate, it also includes `candidate`, a request differing only in
`notification_id`. The supervisor verifies both the attempt binding and the selected durable record:
the original and selected entries must have identical capture coordinates, and the selected entry
must already contain that exact completed publication. An unknown ID or prepared-only alternative
cannot establish success. This selector remains device-local and does not extend the peer DTOs.

Remote `commit.settled` notifications carry the acknowledged generated public commit ID. The
private source ID remains in device-local retry state. Successful Git receive and a persisted
publication result do not establish notification delivery. Pending notification evidence must
survive restart until a matching durable receiver acknowledgement, independently of later Git
publication. Missing or mismatched child results emit no settled notification. The current
local-owner integration requires the backend receiver and joint peer/controller acceptance.
Local-owner sessions emit `commit.localSettled` with `session_id` and `through_seq` for local
persistence. This event carries no private source SHA and does not establish Hub publication.
Executors advertise `local-settlement-v1` in `machine.describe.rpc_features`. Controllers must
keep local save status separate from remote publication and must not clear publication errors
on this event. `through_seq` covers the captured boundary in the current live journal; replay
does not prove that a public Git commit exists or grant authority to acknowledge one.

The shared RC delivery contract is `agit_peer::publication`. An enrolled executor sends
`Delivery { grant_id, controller_generation, notification }` to
`POST /api/peer/publications/confirm`. The notification binds its version/ID, executor owner
(Hub issuer and account), device ID/credential epoch, immutable repository ID, branch, generated
public commit, projected session ID and capture coordinates. Capture coordinates contain the
logical/native session IDs, runtime, local generation and optional incarnation/sequence coverage.
Unknown coverage cannot advance a presentation watermark. Source commit IDs and session text
are not accepted notification fields.

`Notification::digest` hashes compact UTF-8 JSON of this ordered array with SHA-256, returning
lowercase hexadecimal prefixed by `sha256:`:

```text
["agit-rc-publication-v1", notification_id,
 [issuer, account_id, device_id, credential_epoch],
 [repository_id, branch, public_commit, projected_session_id],
 [session_id, native_session_id, runtime, generation, incarnation, through_seq]]
```

Absent incarnation/coverage is JSON `null`. The current grant and controller generation are
outside this digest: reconnects retain notification identity and use fresh authorized delivery
attempts. The receiver independently validates current authority and admitted Git history.
Its durable `Receipt` binds version, receipt ID, notification ID/digest, repository and public
commit. `Acknowledgement` wraps that receipt with the attempt's grant/controller generation.
The HTTP client checks every binding and lease expiry; callers must additionally recheck their
live execution guard before recording delivery locally. A successful HTTP status without the
matching receipt does not acknowledge publication. The backend must implement the receiver's
current-authority lookup, immutable Git admission check and durable idempotency transaction.

Executors advertise `publication-delivery-v1`. The session event `session.publication.changed`
contains only `{session_id}`. The local supervisor emits it after retaining an intent and after
its push attempt ends, including failure. It prompts a new delivery scan and is not proof of
publication or receipt acceptance. Reconnect always rescans durable records; event replay does not
supply authority. A local-save event can arrive before its publication intent is written, so an
empty scan cannot clear a newly saved turn's pending publication state.

On an admitted session-controller connection,
`session.publication.deliver` accepts `{session_id, after?}` with optional shared `workspace_id`
routing metadata, which must match the authenticated caller's workspace. Other publication fields
remain executor-selected and unknown parameters are rejected. `after` is the notification UUID
returned as `next_after`. Unknown fields are refused. The executor resolves its roster lineage,
confirmed destination and exact granted native session/runtime locally. No caller-selected path,
source/public commit, credential, grant or receipt is accepted in the RPC parameters.

Each call processes at most 16 retained records with a 30-second overall deadline. Individual HTTP
attempts time out after 5 seconds and remain pending. A 25-second work budget ends the page at its
last processed notification so following the cursor can reach later entries before the next retry
round. File/Git work runs on blocking workers and HTTP waits do not hold the daemon mutex. The response contains
`session_id`, `items`, the snapshot's remaining `pending` count, `next_after`, and nullable
`retry_after_ms`. Items have `notification_id` and one of these statuses:

- `awaiting_publication`: no generated candidate is available yet.
- `retry`: a temporary transport/service failure retains pending work.
- `rejected`: the receiver or response binding refused this attempt; the record remains pending.
- `local_state_unavailable`: this entry cannot currently bind a notification; retain it and continue
  the page. Missing/invalid legacy cache state and local binding conflicts are never acknowledged.
- `acknowledged`: includes the frozen `notification`, durable `receipt`, and nullable `coverage`.

Acknowledged records are also returned, allowing recovery after a lost RPC response without another
HTTP request. `coverage` is `{session_id, incarnation, generation, through_seq}` only when the saved
capture matches the daemon instance and current live session generation/runtime/native identity;
otherwise it is `null`. The immutable notification still retains its historical capture. Controllers
must fence RPC responses by their active connection and stream; historical capture alone cannot
advance a resumed stream. Pending counts describe the scanned records, not future local turns.

Follow `next_after` until null, then restart at the beginning for subsequent retry rounds. Retained
IDs make a fresh round safe after controller restart. Use bounded backoff for temporary failures;
a binding/authorization rejection requires refreshed authority or corrected state. Delivery creates
no publication consent and never re-pushes source commits. Cancellation, timeout or local-state
failure can leave a partially processed batch; read the durable records again. Enrollment,
destination and live controller authority are rechecked after HTTP before accepting the receipt.
The backend endpoint and joint active-path acceptance remain separate integration requirements.

Local publication receipts include `projected_session_id`, read from the generated Git commit's
metadata. For a legacy receipt without it, the CLI locates the original isolated publication cache
using the saved destination URL and recipient fingerprint. It reconstructs the exact stored public
commit from its declared session snapshot, checks the policy binding and reads the verified session
ID. Current branch tips and native/live IDs do not supply a substitute. Missing objects or mismatches
leave the record unchanged. This local recovery does not prove remote admission; the backend still
performs its independent Git validation. Outbox records freeze a `notification` before delivery and
store its matching durable receipt in `acknowledged`.
An accepted receipt can resolve an interrupted `prepared` candidate only for that exact generated
commit. Later branch advancement does not change the pending notification. The capture records
the daemon instance and the known journal boundary; absent coverage cannot be replaced with the
current stream's sequence when the notification is delivered after reconnect or restart.

Acknowledged outbox entries can be reclaimed when another durable receipt covers the same
repository/destination, policy/recipient, executor, projected session and captured logical/native
stream incarnation/generation. Its sequence coverage must include every known old sequence, and
both its raw source ancestry and raw public ancestry must contain the old commits. Local Git
replacement/graft/shallow overlays do not establish this relation. Pending entries, the current
source tip and the dominating receipt remain. Cleanup rechecks records and the source tip under
the outbox mutation lease, unlinks a bounded set and syncs the directory; partial cleanup is safe
because the covering ACK is already durable. Missing objects or a busy cache defer cleanup.

Controllers may recover a newer covering receipt instead of every earlier ACK. Missing reclaimed
items do not retract already acknowledged progress. Only matching non-null live coverage can
advance a stream watermark. Replaying retained receipts retries cleanup without another HTTP
confirmation, and the retained latest source record prevents unchanged-source re-publication.

A device-local RC repository selects its first Hub destination with
`agit push <local-owner>/<local-repo>@<branch> --to <owner>/<repo>`. The normal preview and
confirmation apply to that exact destination. The local checkout and desktop identity stay in
place; `agit.desktopPublication` records the confirmed repository slug and immutable Hub identity
in local Git configuration. Later pushes of the local target reuse that binding. `--to` cannot
replace it or redirect an ordinary Hub checkout. Privacy source rules resolve against the bound
Hub repository, and automatic publication still requires enabled `push.auto` and explicit consent.

Enable `push.auto`, then run an explicit push to review the current repository policy and outgoing
content. Its confirmation also authorizes future automatic publication with that policy. A
successful publication stores a device-local receipt under
`<git-common-dir>/agit/privacy-auto-consent.json`, bound to the Hub, account, destination ID/URL,
audience, policy digest, recipient fingerprint and consent format. The receipt contains no key or
original session content and is never part of the published tree.

Stop-hook and RC supervisor publication children set `AGIT_AUTO_PUSH=1` and remove inherited
`AGIT_YES`. That mode requires
both the enabled local preference and an exact current receipt; setting the environment marker or
passing `--yes` alone grants nothing. It cannot create or promote a destination. Each automatic
push generates a fresh preview, processes new content under the policy, verifies captured objects
and repeats destination/key checks. Missing or changed authorization keeps the saved turn local
and directs the user to an explicit push. Current preparation reinspects complete ancestry;
unchanged generated objects and ciphertext are reused.

Policy, recipient or destination changes invalidate consent. Reconfirming does not authorize a
force-push or erase old remote content; projected-history migration remains a separate operation.


`push --all` prepares every settled local session branch and skips repository file lines. Original-history tracking refs cannot prove that
the current policy projection is already published. Destination binding compares generated refs
with the remote, so unchanged projections remain idempotent.

`agit share` applies the same frozen projection to a saved VIEW/LOG or an adopted live transcript.
It preserves the requested sequence boundary, uses stable aliases and replacement rules, scans the
rewritten result with repository-local secret mappings, and shows complete selected public records
and metadata before confirmation, including full tool inputs and results. The terminal preview
escapes control characters without changing the outgoing records. Share and privacy export report
omission reasons against one-based source LOG positions and explain whether the output includes
recoverable originals. Those reports cover the source LOG used to establish provenance; the share
payload still contains only the selected records. The versioned share transport is described above.
Publication rechecks policy and viewing recipient after confirmation. Unclaimed live sessions
without a repository refuse any registered secrets remaining after projection.

## Browser initialization command contract

`AGIT_HUB_URL=https://hub.example agit privacy init OWNER/REPO --browser --json --yes`
uses the existing CLI JSON envelope. Its `result.value` has `operation: privacy_initialize`,
`repository`, `hub`, immutable `agent_id`, and `config_version`. Exit code 8 with
`status: setup_required` also returns `setup_url`; exit code 0 with `status: ready` means the
existing publishing-key API returned a validated current public key. API failures are not setup
requirements. No password or private key enters the CLI in this mode.

The companion website contract is
`/@OWNER/REPO/settings?setup=initialize&expected_agent_id=ID#repository-password` on the
selected Hub (including any base path). The website retains this destination across login,
requires management permission, validates the identity, and offers initialization only.
It generates and wraps the key with the existing protocol. The CLI creates or resolves the
encrypted repository using the existing identity/mode guards; retries only read key readiness.
Missing repositories require `--yes` and are private unless `--public` is selected.

Release the CLI supporting `--browser` before the website quickstart requires it. Deploy the
matching website direct-link handling before advertising browser onboarding. Keep auto-push
disabled during initial setup and enable it afterward only at the user's selection.
