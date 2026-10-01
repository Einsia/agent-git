# RFC Local Privacy and Recoverable Secret Dictionaries

Status: Accepted requirements; implemented, verified, deployed, and released.

This RFC specifies AgentGit's default privacy behavior, the independent local
modules that implement it, the cloud storage contract, and the implementation
and verification plan. Privacy processing is a best-effort local transformation.
Its failure must never reject an otherwise valid conversation operation.

This decision supersedes the fail-closed publication and local-only dictionary
requirements in `05_global_secret_filter.md`,
`06_repository_secret_dictionary.md`, and `explicit-secret-acceptance.md`.
The backend's confirmed-publication RFC is superseded by the storage-only contract
documented in its `docs/local-privacy-storage.md`.

## 1 Final requirements

1. Secret detection runs locally in an independent, efficient module. It has no
   dependency on Git traversal, authentication, network requests, or key storage.
2. Every value finally classified as a secret is assigned a reversible mapping
   from an opaque replacement token to its complete original value. Existing
   tokens and mappings remain valid across upgrades and policy changes.
3. Local code automatically replaces detected secrets when their mappings can
   be persisted. The user does not review findings or configure keys to proceed.
4. An incomplete scan retains useful findings. Persist and replace those
   findings where possible; leave unprocessed input unchanged.
5. The cloud automatically provisions a dedicated encryption key for each user.
   Clients prefer that key or its valid cache. A login token is not an encryption
   key, and acquiring the encryption key is not a condition of successful login.
6. When encryption is unavailable, dictionary originals may temporarily remain
   in owner-private local storage. They must never be uploaded as a plaintext
   dictionary. Automatically encrypt and synchronize them when a key is available.
7. Every push schedules synchronization of the required encrypted dictionary
   records, including previously pending records. Dictionary synchronization is
   independent of Git publication and cannot make it fail.
8. Authenticated devices belonging to the same user can obtain the user key and
   encrypted mappings, decrypt locally, and restore known placeholders locally.
9. Preserve global default, global user, and repository user policy scopes.
   Explicit user blocks take precedence over allows; user allows override
   default heuristic classification. Apply one policy consistently at every
   local transformation entry point.
10. Privacy errors, corrupt state, lock contention, timeouts, resource limits,
    worker crashes, key failures, and synchronization failures skip the affected
    privacy work. They do not prevent recording, importing, opening, resuming,
    exporting, sharing, daemon startup, or uploading conversations.
11. The server performs no secret scanning, redaction, privacy policy evaluation,
    privacy auditing, or privacy-based publication rejection. It stores user
    encryption keys, client-produced commits, and encrypted dictionary packages.
12. Ordinary authentication, access control, quotas, transport checks, Git
    object integrity, provenance, and repository consistency remain independent
    business contracts. Privacy degradation cannot waive those checks.

"Nonblocking" means both that privacy is not a success precondition and that
privacy work has bounded foreground waiting. It does not mean that CPU work has
zero latency. A skipped transformation can publish original sensitive content.
Partial work is never described as a clean or complete scan. Cross-device
recovery is available once the corresponding mapping is successfully synced;
publication is allowed before that condition holds.

## 2 Scope and exclusions

The default path covers initial imports, new conversation events, generated
conversation descriptions and commit text, live conversation projections, and
local runtime restoration. All textual conversation fields, including tool
inputs and outputs, use the same secret semantics. Typed protocol identifiers
are not rewritten. All Git LFS payloads are outside privacy processing, including
text stored through LFS: do not download, scan, replace, or register their contents
in a secret dictionary. LFS continues to enforce its independent authorization,
size, checksum, transfer, and storage contracts. It is an existing attachment
storage mechanism, not a dependency of the privacy module.

Explicit diagnostic commands may report their own unsuccessful diagnostic
operation, but they are never implicit publication prerequisites. Persona
masking for usernames, paths, hosts, and IP addresses remains a separate local
transform. It must not replace secret mappings with irreversible generic masks.

Existing Git objects are immutable. This RFC does not rewrite published history,
force-push branches, or pretend that upgrading removes originals from old
commits. Initial imports protect input before object creation; existing history
continues to publish without a privacy gate. A separate explicit migration can
produce new sanitized history without changing existing objects.

## 3 Module boundaries

```text
CLI / desktop / daemon adapters
             |
      PrivacyService facade
             |
      isolated local worker
       /       |          \
 policy     detector    projector
 snapshot      |          |
               +---- dictionary ---- local storage
                                      |
                         key provider and sync outbox
                                      |
                   authenticated cloud storage transport

client-produced content --> Git object creation --> ordinary publication
```

### Detector

Accept borrowed decoded content and immutable compiled policy inputs. Emit
bounded match batches with source locations, rule identity, and completeness.
No filesystem, Git, keyring, HTTP, CLI output, or persistence imports belong here.
Keep literal Aho-Corasick matching, keyword prefilters, and cached lazy regexes.
Isolate rule failures and retain completed matches. Preserve cross-chunk state
for supported streaming rules, including multiline credentials.

### Policy

Compile global defaults, global user settings, and repository user settings to
one versioned immutable snapshot. Resolve overlapping ranges deterministically.
An explicit block wins even when a default exception or user allow also matches.
An allowed heuristic value is not replaced. Existing reverse mappings survive
allows, rule removal, and source changes. Retain existing allow matching
semantics during migration and record rule provenance separately from mapping
existence; copying a mapping must not accidentally create an eternal block.

### Dictionary

Own stable IDs, original values, provenance, durable mapping versions, aliases,
and pending sync state. Reuse a mapping within its dictionary when available.
Do not expose unkeyed hashes of original values as public tokens or indexes.
Use a private in-memory value index for deduplication. Independently generated
tokens on different devices can coexist as aliases for the same original.

### Projector

Replace decoded semantic strings using confirmed dictionary records. Preserve
JSON structure and protocol identity. Existing valid placeholders are opaque.
Only adopt a new replacement after its mapping is durably saved. Failure to save
one batch leaves that batch's originals unchanged, without discarding completed
batches. Read-side hydration is local and leaves unknown tokens untouched.

### Local storage and cryptography

Store mappings beneath private application or repository metadata, outside Git
trees. Persist batches and incrementally load/cache records rather than rewrite
and decrypt a complete vault for every string. Use atomic publication of local
state and bounded lock acquisition. Corrupt old ciphertext remains recoverable
evidence; new pending mappings must not overwrite it as an empty dictionary.

Use authenticated encryption with fresh nonces. Bind the application domain,
Hub, account, dictionary identity, package identity, format, and key version in
associated data. The implementation encrypts each package directly with the user's
versioned cloud key using AES-256-GCM. Historical key lookup retains access to earlier versions and stable
placeholder IDs. Never reuse access tokens or signing keys for this purpose.

### Key provider

Obtain a dedicated user key through the authenticated Hub client. Cache by Hub,
account, and key version. The new key cache uses private local files and does not require the OS credential
store. Legacy key access stays inside the disposable worker; it cannot prompt or
hold up a business operation beyond the worker deadline.
Key discovery, cache errors, and refresh failures cannot fail login or startup.
On key unavailability, use local pending plaintext storage with owner-private
permissions. On recovery, encrypt pending mappings before removing plaintext.
Account changes invalidate in-memory key routing. Pending mappings retain their
ownership and destination rather than silently migrating to another account.

### Synchronization

Own an independent durable outbox of encrypted packages and acknowledgements.
Upload immutable idempotent packages; never overwrite an entire multi-device
dictionary using last-writer-wins. Download package inventories, decrypt locally,
and merge stable records and aliases. Authenticate package identities and detect
conflicting mappings locally. Failed imports affect only the relevant package.

Each push triggers a bounded synchronization attempt or schedules one, even if
Git has no new refs. Retry pending work on later push, login, and background
opportunities. A CLI without a running daemon retains its outbox for the next
opportunity. Do not spawn an unbounded family of background retries.

### Facade and worker

The facade returns usable content and structured status, not a privacy error
that propagates through business code:

```rust
struct PrivacyOutcome<T> {
    content: T,
    status: PrivacyStatus, // Complete, Partial, or Skipped
    replacements: usize,
    unresolved: usize,
    consumed: Option<usize>, // Streaming input boundary when applicable.
}
```

Run fallible CPU and storage work in a terminable worker process with bounded
requests, responses, concurrency, memory, and deadlines. Catching a Rust panic
alone does not isolate an abort, deadlock, or memory exhaustion. Reuse workers
for resident applications and batch CLI input to amortize startup and IPC.
Workers emit acknowledgements only for valid output whose new mappings are
durable. On termination retain acknowledged batches and pass through remaining
input. A temporary circuit breaker limits repeated failures. Worker health must
not become a daemon-start or publication condition.

Diagnostics contain categories and counts, never originals or key material.
Default operations deduplicate warnings and do not prompt for confirmation.

## 4 Local write and read flows

### New content

1. Select the repository and immutable native event boundary using business
   identity rules, independently of privacy state.
2. Read the policy and available dictionary snapshot with bounded work.
3. Scan only new complete content units; collect partial findings as they arrive.
4. Persist discovered mappings, encrypted when possible and locally pending
   plaintext otherwise.
5. Replace only records whose mappings have been confirmed durable.
6. Return transformed units and unchanged skipped units to object construction.
7. Compute envelope and Git identities from that final representation.

Preserve the already committed representation of earlier events. A later rule
change, key failure, or skipped scan cannot reproject the settled prefix, expose
an earlier placeholder, or create a false continuity failure. Local continuity
checkpoints do not require an unlocked secret dictionary. Any native original
digest used for these checkpoints remains local rather than becoming a public
offline verification oracle for low-entropy originals.

### Publication

Publish existing canonical objects through ordinary identity and integrity
checks. Remove the automatic repository-wide secret gate. Independently attempt
encrypted dictionary synchronization; it is neither a prerequisite nor a reason
to roll back a successful ref update. Do not automatically rewrite objects just
before push. Diagnostic scanning remains explicitly invocable locally.

### Restore

Fetch Git content without hydrating its worktree or object database. Fetch the
user's encrypted dictionary packages independently. Hydrate known placeholders
only into explicit local display/runtime materializations. Preserve unknown or
unavailable tokens and return a partial result; opening the conversation still
works. A repository reader does not automatically gain access to the user's
private dictionary. Replaying a token from remote content must not cause
automatic execution or write plaintext into Git.

## 5 Cloud storage contract

Cloud responsibilities are authenticated key provisioning, authorized opaque
storage, pagination, idempotency, and normal resource limits. They do not include
scanning even for diagnostics, decrypting dictionary originals, classifying
content, or deciding publication based on privacy.

Use an account-scoped key endpoint that atomically ensures a dedicated random
key exists. Existing users require no migration action or settings. Responses
are authenticated, non-cacheable by intermediaries, and excluded from logs.
Retain version identifiers for later rotation. Keys are stored through the
backend's protected account data facilities; no key material enters source,
examples, command lines, CI settings, or deployment manifests.

Provide account-private immutable package upload and paginated list/download
endpoints. A package has an opaque ID, dictionary ID, key version, format, and
ciphertext. Repeating the same ID and bytes is idempotent; different bytes under
the same ID conflict without replacing the original. The server treats the body
as opaque encrypted material. Ordinary body limits and authorization still apply.
Dictionary access is separate from public repository read access.

The cloud holds user encryption keys, so this design must not claim that the
cloud is cryptographically unable to decrypt. All actual privacy processing and
restoration nevertheless occur on the client. Publication fallback may retain
original content; the cloud stores the client's final bytes without inspection.

## 6 Failure contract

| Condition | Privacy outcome | Business outcome |
| --- | --- | --- |
| Detector initialization fails | Skip privacy | Continue |
| Scan ends early or exceeds budget | Protect durable completed findings | Continue |
| User key cannot be obtained | Save mappings locally as pending plaintext | Continue |
| Optional key cache fails | Use available memory key or pending plaintext | Continue |
| Mapping persistence fails | Keep originals for uncommitted replacements | Continue |
| Encryption fails | Retain local mappings; do not upload plaintext dictionary | Continue |
| Dictionary upload fails | Retain outbox and retry later | Continue |
| Worker crashes, aborts, or times out | Keep acknowledged units; skip remaining work | Continue |
| Dictionary hydration fails | Keep unavailable placeholders | Continue |
| Legacy dictionary migration fails | Preserve old evidence and skip affected work | Continue |
| Authentication or Git integrity fails | Not a privacy outcome | Preserve business failure |

Partial, skipped, pending-encryption, and pending-sync statuses remain distinct.
No hidden strict default and no requirement to pass an acceptance flag is allowed.
Explicit diagnostic failures cannot reappear as implicit prerequisites elsewhere.

## 7 Performance and resource requirements

The maintained contract is incremental work with bounded foreground waiting.
Literal matching uses one compiled automaton; heuristic scanning uses keyword
prefilters and reusable compiled rules. Avoid per-record linear lookups, scanning
all historical snapshots on each push, decrypting the same vault for every
operation, and holding the dictionary write lock during scanning or HTTP calls.

Measure cold and warm imports, append settlement, dense findings, long multiline
secrets, worker timeout behavior, and multi-device package merging. Report real
input sizes, elapsed time, allocations or peak memory, and scan completeness in
benchmark results. Do not claim throughput from old source comments. Resource
caps produce partial/skipped privacy results, not publication refusals.

## 8 Implementation plan

### Phase A Storage-only server

- Remove privacy gates from push, private-to-public visibility changes, PR and
  merge publication, LFS publication, and every shared expose/scan caller.
- Retain authorization, immutable refs, provenance, transport and LFS integrity,
  repository locks, quota checks, and snapshot-bound visibility confirmation.
- Add automatic account key provisioning and opaque encrypted package storage.
- Maintain focused route coverage for user isolation, idempotency, and acceptance of
  synthetic secret-shaped conversation content without scanner invocation.
- Remove or reconcile obsolete privacy acceptance flags, reports, UI claims,
  documentation, and tests that require server privacy rejection.

### Phase B Independent local privacy service

- Extract detector and policy interfaces from repository traversal and vault I/O.
- Implement complete/partial/skipped outcomes and bounded worker execution.
- Implement durable mapping batches, pending plaintext, cloud key selection,
  authenticated encryption, and migration preserving old token identities.
- Use one mapping path for heuristic and explicitly registered secrets.
- Keep optional persona processing separate from reversible secret protection.

### Phase C Business integration

- Route import/commit and merge archive settlement through the facade before
  object creation; stabilize historical prefixes and native continuity.
- Remove implicit local push and share rejection gates. Keep local diagnostic
  scans explicit and independent.
- Integrate live RC projections, export, runtime restoration, and desktop-backed
  operation paths without requiring a healthy worker or keyring at startup.
- Treat compatibility with unavailable/older storage endpoints as pending sync.

### Phase D Synchronization and recovery

- Connect authenticated login/key prefetch and every push to the durable outbox.
- Implement bounded encrypted package upload, inventory download, local merge,
  retry, account routing, and cross-device hydration.
- Preserve pending packages through offline operation and process restart.
- Document backend-first rollout; an old backend retaining privacy gates cannot
  provide the new guarantee until upgraded.

### Phase E Verification and delivery

- Run the smallest relevant unit and integration targets after each phase.
- Exercise representative end-to-end default flows with privacy fault injection.
- Verify a second independent local device can restore synchronized mappings.
- Test concurrent device packages, retry idempotency, partial scans, worker death,
  failed dictionary writes, missing keys, and continued conversation settlement.
- Run targeted performance experiments and applicable formatting/lint checks.
- Record implementation state and actual validation evidence below. Do not mark
  this RFC implemented or the associated goal complete while required paths or
  tests remain unfinished.

## 9 Acceptance criteria

- An unconfigured user can log in, import, record, and upload a conversation.
- A synthetic secret is replaced, its mapping survives locally, and another
  authorized device restores it after encrypted synchronization.
- Partial scans preserve useful reversible replacements.
- Missing keys, corrupt dictionaries, write failures, failed uploads, worker
  crashes, and timeouts do not fail otherwise valid conversation operations.
- A later successful operation can continue the same conversation after privacy
  degradation without a false history-rewrite error.
- Two devices' additions are retained, and another account cannot fetch their
  encryption keys or private dictionary packages.
- Secret dictionary plaintext is never uploaded; fallback originals in ordinary
  conversation content follow the explicitly accepted fail-open contract.
- Server publication paths perform no privacy processing or privacy rejection.
- Authentication, authorization, and content integrity regressions are absent.
- Foreground work is bounded and routine appends do not rescan retained history.

## 10 Implementation evidence

The implementation resides in `src/domain/privacy/`. `detector` and `policy`
operate without storage or networking. `dictionary`, `crypto`, `storage`, `keys`,
`sync`, `management`, `projector`, `continuity`, `worker`, and `service` have
separate responsibilities. The backend storage API and optional schema migration
are implemented in the companion backend change. Production deployment is verified
in all regions. CLI 0.2.16 is released on GitHub and npm with delivery verification
complete.

### Concrete local boundaries

- `service` batches complete JSONL records, validates response framing and carrier
  identity, and waits at most 750 ms per foreground transform. It uses a bounded
  channel and immediate fallback on lock contention. A failed worker is terminated;
  a circuit breaker prevents repeated restarts until the failed process is reaped.
- Workers bound frames to 64 MiB, individual transformation units to 8 MiB, and
  ordinary batches to 128 KiB. A worker watchdog and process memory limits contain
  blocked or aborting dependencies. The streaming projector retains a bounded suffix
  for cross-chunk literals and multiline secrets, then releases incomplete work.
- The private SQLite journal uses transactional append, WAL, full synchronization,
  immediate lock failure, and a bounded store. A replacement is acknowledged only
  after its complete reverse mapping is durable. Long originals are fragmented
  inside encrypted packages and reassembled before their aliases become usable.
- Package IDs are immutable. AES-GCM associated data binds Hub, account, dictionary,
  package, format and key version. Upload acknowledgements and download retry state
  are separate from Git refs. Server-assigned inventory sequences include late
  offline uploads regardless of their client-generated package IDs.
- Account and pending journals are routed separately. Pending generations bind to
  an account once; a later signed-out generation cannot silently change that owner.
  Duplicate values from different devices retain every independently created alias.
- Policy decisions are separate from recovery records. Removing a block or allowing
  a candidate never deletes its original. Repository scope uses the common Git
  directory, so branch worktrees share repository policy.
- Read-side hydration does not create a journal, register rules, or migrate records.
  Missing originals remain opaque. Explicit lineage discovery declines candidates
  it cannot prove and remains independent from ordinary import.
- Native settlement records an optional local checkpoint containing the exact tip,
  native prefix length, and digest. It does not reuse runtime materialization
  baselines. With no usable checkpoint or mapping, legacy continuity compares all
  structure and surrounding literals while treating unresolved opaque spans as
  unknown. The already committed representation is retained unchanged; unknown
  originals cannot establish whether the value inside such a span changed.
- Recording, import, milestone/commit text, live output, explicit redacted export,
  share, clone/resume/merge restoration and diagnostics use the facade. Push has no
  implicit privacy scan. Login, push (including no-op), fetch and clone schedule
  bounded independent synchronization. LFS payloads never enter privacy processing.

### Verification evidence

The focused validation includes detector partial results, policy precedence,
durable projection with long values, owner-bound authenticated encryption,
idempotent package import, lock failure, and alias recovery. The executable
`privacy_lifecycle` integration test uses separate device homes and real HTTP:
offline plaintext staging, rejected upload, retry, encrypted-only transfer,
idempotency, concurrent device aliases, download, exact restoration, and local storage failure passthrough.
Existing integration coverage now checks status and native merge behavior with
unavailable dictionaries, a locked legacy dependency that forces worker termination,
subsequent recovery without prefix rewriting, and import lineage with the privacy facade.

Completed checks include the CLI privacy and lineage unit targets, Git transport
unit target, native merge integration, status integration, import-lineage preview,
local publication preconditions, upload identity/confirmation/network categories,
doctor and input categories, strict all-target
Clippy, and the library build without default features. Backend validation includes
the storage protocol against PostgreSQL, LFS and publication paths, frontend
settings/type checks, and strict all-target Clippy. The required Linux, macOS,
Windows, musl, distribution compatibility and npm checks all pass on the final
implementation. The merged source tree is identical to the checked merge-request
tree. Live model-credential checks remain opt-in and were not run; deterministic
native runtime and protocol checks cover the maintained local contracts.

`AGENTS.md` requires a distinct maintained purpose for each test. Obsolete privacy
refusal tests, deleted LFS scanning tests, and duplicate legacy projection tests
are removed. Existing meaningful transport, integrity, identity and continuity
coverage is retained or updated; no separate test is required merely because a
bug was fixed.

Ordinary publication validation covers unattended creation, automatic continuation,
source Git IDs and tags, receipt reconciliation after a lost response, a cold clone,
and missing or corrupt newly referenced LFS payloads. Accepted LFS history is not
downloaded again for privacy inspection. Publication diagnostics describe payload
integrity and explicitly mark publication-time secret scanning as not performed.
The Windows cross-device lifecycle check passes with real private storage and HTTP;
the low-level journal fixture creates a dedicated private child directory rather
than assuming the test runner's temporary directory has a private Windows ACL.

### Operational limits

A skipped or over-budget portion can retain sensitive originals, as required by
the accepted default failure behavior. Cross-device recovery waits for successful
package synchronization. Legacy records whose old keys are permanently unavailable
cannot be reconstructed. Store, policy and memory limits cause optional privacy
work to fall back; they never become upload gates. The Hub holds encryption keys,
so this design is not end-to-end encryption. Deploy the backend changes before
releasing the CLI: an older server's privacy gates cannot be bypassed by the new
client guarantee.

### Integration with repository encryption and current runtime delivery

Cloud policy-source resolution and administration are removed. Repository envelope receipts
retain only authenticated recipient, immutable source/ref, schema, and client metadata bindings;
they do not evaluate secret findings or cloud content exclusions. Server summary/projection
validation no longer rejects content because a field name resembles a secret or a value resembles
a local path. Existing repository encryption remains an explicit transport mode separate from
the automatic per-account dictionary key.

Share and privacy export delegate optional local policy loading and path projection to the
bounded worker. Nested secret operations execute inside that same disposable process rather
than launching additional workers. A failed optional projection uses the default in-memory
projection; selected private records remain in the encrypted envelope. Alias and encrypted
publication cache locks use immediate acquisition so contention cannot wait indefinitely.

Runtime fragment delivery consumes each successful streaming result immediately and flushes
only the retained tail at item completion. Scanner unavailability and oversized input release
the original fragment. Completion cannot discard bytes already consumed by the privacy facade.

Test maintenance removes fail-closed privacy fixtures, cloud-rule resolver fixtures, and duplicate
legacy scanner assertions inside command tests. Durable alias recovery, selected history,
transport integrity, staged/unstaged isolation, and live fragment delivery retain distinct checks.

### Delivery evidence

- [CLI implementation and required CI](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/merge_requests/358)
  merged as `02eb36958448b5c3c321a3b1924d8dab924ac2c4`. Pipeline `24151`
  verifies the same source tree, including native-title protection.
- [Backend implementation](https://git.xiaoaojianghu.fun:114/dev/agentgit/AgentGit-backend/-/merge_requests/705)
  is deployed from source `93a2f8ff345e8b4113ec35503740a8b7602d944d`.
  Expand migration 58 owns account keys and encrypted packages; its absence
  remains isolated from ordinary application readiness.
- [Staging receipt](https://git.xiaoaojianghu.fun:114/dev/agentgit/gitops/-/jobs/92150/artifacts/file/deployment-receipt.json)
  and [production receipt](https://git.xiaoaojianghu.fun:114/dev/agentgit/gitops/-/jobs/92183/artifacts/file/deployment-receipt.json)
  verify image `sha256:b5bfc4400e976673f5c364232236a1738410e9017e703594d09e3aa9e2fcf65f`
  in `ap-southeast-1`, `us-east-2`, and `us-west-2`. Production completed at
  `2026-10-01T19:54:02Z` with runtime, frontend, public transport, compatibility,
  and functional acceptance checks passing.
- [CLI 0.2.16](https://github.com/Einsia/agent-git/releases/tag/agit-v0.2.16)
  is a published stable release from the mirrored source
  `015c555518c69fb2cbf5c84c71c436af36f2161a`.
  [Release verification](https://github.com/Einsia/agent-git/actions/runs/36922897288)
  passes for Linux and macOS on both architectures and Windows x64. All five
  archives are present, and `SHA256SUMS` matches their published asset digests.
- [npm publication and installation verification](https://github.com/Einsia/agent-git/actions/runs/36927554447)
  passes. The main package, `create-agit`, and all five platform packages are
  publicly available at `0.2.16`, with `latest` pointing to that version.
  The installation preflight resolves a published platform package and verifies
  `agit --version`; public registry metadata confirms exact dependency versions
  and provenance attestations for the complete package family.
