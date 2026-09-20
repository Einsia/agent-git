# RFC: Repository-scoped reversible secret placeholders

Status: Implemented. The supported boundaries and failure behavior are specified below.

## 1. Goal

The global low-entropy secret filter answers "does this session contain a value the user
registered explicitly", but refusing the push outright forces the user to choose between "keep
the whole session" and "publish the session to the remote". This RFC adds a repository-scoped
reversible projection: the local runtime keeps seeing the real value, Git and the hub see only
an opaque placeholder.

```text
runtime plaintext
  │ agit commit (matches on JSON semantic strings)
  ▼
repository-scoped placeholder ── agit push ──► hub / other devices
  │
  │ agit resume / run (only on an explicit local materialization)
  ▼
runtime plaintext
```

Session settlement protects native event content and generated commit messages, together
with user-controlled observations in `session/meta.json`: working directory, code origin,
observed branch and milestone. Metadata schema, session identity, object hashes and enum
values remain structural. Heuristic discovery completes before event objects are formed,
so values learned from observations also protect matching transcript content.

File commits protect selected UTF-8 shared files before publishing their tree, including
files captured by a first session commit. Memory collection and promotion use the same
dictionary, and runtime memory mirrors hydrate locally. A partially staged file keeps its
unstaged working copy; protection applies to the selected bytes. Outward export and share
protect semantic content before rendering can truncate it. Claimed live/history sessions
resolve the dictionary from the Agent repository that owns the native session, not from
the source-code working directory.

Arbitrary Git headers, tag messages, direct Git writes and history that predates protection
still pass through the publication gate. Protecting a new snapshot does not remove an old
plaintext object. Push validates the entire outgoing history and never rewrites it.

Local resume and same-repository discovery read observation placeholders through the local
dictionary without rewriting stored metadata. A missing mapping cannot establish repository
identity or equal worktree state. Shared/backend metadata reads retain placeholders; bounded
status inspection reports unavailable evidence when its local dictionary cannot be read.

Two kinds of candidate enter the dictionary automatically: literals the user registered
explicitly in the global vault, and heuristic hits in semantic content. Independent entropy
discovery supplements provider and contextual rules; it does not require their regexes to
match first. JSON property keys and values are both inspected. A candidate is protected by
default, and the user can allow its exact value by opaque id. Allowances suppress future local
projection and client findings without deleting the reverse mapping, so old placeholders still
hydrate. A property named `sha`, `signature`,
`session_id` or `provenance` does not exempt its value or subtree. Verified envelope identities,
typed Git headers resolved in the selected repository, and canonical existing secret tokens
have scoped exemptions; adjacent content is inspected normally. A settlement registers every
distinct new candidate it carries, however many: a long unsettled session full of identifiers
settles in one pass instead of being refused for its count. A candidate already present in this
repository's dictionary (an allowed one included) is not registered again. What bounds a
settlement is bytes: the distinct new values it registers may total 64 MiB, and past that it
refuses before any dictionary update is written. Per value, an oversized finding stays in the
clear and visible to the push scanner rather than becoming an irreversible record.

The independent tokenizer accepts ASCII letters, digits and `_-+/=.!@#$%^&*?~`.
It measures Shannon entropy in bits per character with these minimum length / entropy pairs:
hex (including hyphenated hex or `agit-`, `sha1-` and `sha256-` prefixes), 32 / 3.2; mixed-case alphabetic,
24 / 3.8; alphanumeric, 20 / 4.0; tokens containing symbols, 20 / 4.2. Tokens must contain
mixed-case letters, a digit, or use the hex alphabet. Credential-related JSON properties
add evidence and lower the pair to 10 / 3.5. These are best-effort discovery policies, not a
guarantee that every credential is recognized. A path-like spelling alone is not an exemption.

An automatic repository candidate uses a 64 KiB plaintext cap and a padded ciphertext bucket
of at most 128 KiB, enough to hold a common 4096-bit PEM private key reversibly; a manually
registered global rule keeps its 512 UTF-8 byte cap. The dictionary reports an oversized
finding without partially replacing its header. Settlement, file/memory projection and outward
rendering must reject that incomplete result before publishing a version or emitting content.
The original native input remains available; a resource failure never means the input was clean.

PEM discovery covers the region from a private-key header through its matching
footer. If a semantic string ends before that footer, its remaining bytes belong
to the sensitive region. A header-only substitution must never leave captured key
material in a supposedly protected field. Overlapping redaction findings cover
their union, including credentials nested inside a PEM region.

RC delta protection buffers each item until completion before inspecting and
emitting its text. This also covers an empty global registry: provider rules and
multiline findings cannot rely on a registered literal's length to choose a safe
stream boundary. The buffer limit is 1 MiB per item and 8 MiB across streams sharing
a redactor. Overflow withholds the whole
item and emits an explicit protection-limit notice on completion; later chunks
cannot release an unchecked suffix. The native transcript remains available for
settlement. This boundary delays delta display until item completion.

## 2. Why the substitution cannot wait for `git push`

Git blobs, trees, commits and tags are all content-addressed objects. Swapping one secret for
a placeholder just before the push changes every OID along blob → tree → commit → tag; local
and remote stop being the same fast-forwardable history, and the version identity
`session/meta.json` defines no longer holds.

The substitution point therefore sits before AgentGit forms its first canonical Git object —
the boundary where `agit commit` wraps the runtime transcript into an envelope. `agit push`
keeps its full, fail-closed repo-wide scan as the last backstop for:

- plaintext history that predates the feature;
- content the user writes or commits directly, bypassing `agit commit`;
- unsupported shared-file carriers and commit/tag headers outside automatic projection;
- a corrupt dictionary, a missing keystore entry, or a conversion that did not finish.

This delivers what the user observes — what goes up is the key, what comes back hydrates on
this device — without faking Git's identity model.

## 3. Module boundary

Both chains live in the `domain::secret_filter` domain module and share these primitives:

- AES-256-GCM envelope encryption that fails closed on an authentication failure;
- linear Aho–Corasick matching over arbitrary UTF-8 literals;
- semantic traversal over JSON string values: serialized bytes carrying `\"`, `\\` or `\n`
  must not impersonate the original value.

Inside the module the two storage responsibilities stay apart; they do not go into one vault:

| Component | Scope | Contents | Lifetime |
| --- | --- | --- | --- |
| global filter vault | the whole device | the user's detection rules | user add / remove |
| repository dictionary | one Git checkout | placeholder key → secret | the local checkout |

A repository must still hydrate an already published placeholder after the global rule is
deleted, so the repository dictionary keeps its own encrypted copy instead of only a foreign
key pointing at a global rule id.

## 4. Storage and placeholders

Each repository's dictionary lives at:

```text
<repo>/.git/agit/secret-dictionary/vault.json
```

It sits inside Git metadata, so `git add`, push, an ordinary workspace scan and shared-file
export never carry it away. The file retains envelope encryption, but its KEK is created automatically at
`<repo>/.git/agit/secret-dictionary/keys/<vault-id>.key`. Repository storage does not
consult `secrets.keystore` or create Keychain entries. On Unix the key file is owner-only
(`0600`) in a private directory (`0700`). On Windows the key directory and temporary key file
receive an explicit current-user private ACL before key bytes are written; existing key files
are checked for private ownership and permissions through the handle used to read them.
A local backup containing this directory includes both the encrypted mappings and their key.
Treat it as sensitive data. Neither file is part of Git history or uploaded by push.

The global registration vault remains separate and uses the user's configured keystore.
Its configuration does not change when a repository dictionary is created or migrated.

Dictionaries without the `key_storage` marker use their existing configured keystore until
a locked operation successfully authenticates every record. That operation durably installs
the repository key and atomically marks the dictionary `repository-local-v1`. The vault id,
record ids, ciphertext and placeholders remain unchanged; the previous key is retained.
A retry accepts an identical local key but rejects a conflicting one. Strict read-only
inspection can read the existing storage without migrating it. Once marked local, missing
or corrupt local keys fail explicitly and never fall back to the global keystore.

On macOS, reading the previous key may require authorization once during migration. After
migration, repository protection and hydration no longer access Keychain. For manually
registered global secrets, select "Always Allow" to retain authorization for the same signed
executable. "Allow" grants a single access. Rebuilding an ad-hoc-signed executable or changing
its signing identity may require authorization again; the CLI does not weaken Keychain ACLs
or suppress an authorization decision.

A random record id is generated the first time a secret is met in that repository; the same
secret in the same repository reuses one record, and another repository generates a different
id. The placeholder format is:

```text
{{AGIT_SECRET_V1:<random-vault-id>:<random-record-id>}}
```

The key is never `SHA-256(secret)`, a truncated hash or deterministic encryption. A
deterministic digest of a low-entropy secret hands whoever holds the remote content an offline
dictionary oracle, and it leaks that two repositories use the same value.

A placeholder is a versioned, repository-scoped opaque capability. Only a token matching the
local dictionary in full is hydrated; an unknown or malformed token, or one belonging to
another repository, is kept verbatim and quietly — never guessed at, never fetched over
the network, never replaced with an empty string. Malformed lookalikes remain ordinary input
for discovery; they do not acquire the opaque-token exemption.

## 5. Write path

`agit commit` parses every parsable JSONL line into a `serde_json::Value` and walks string
values and arrays recursively; matching and replacement happen on the decoded UTF-8 string,
which is then serialized canonically. Effective repository rules protect each of their
occurrences verbatim. A secret carrying quotes,
backslashes, newlines or Unicode therefore matches under exactly the same semantics as
ordinary characters.

The dictionary payload records `schema_version=2` and `projection_version=1` separately. One
conversion compiles the currently effective repository records, the registered global rules
and the new heuristic candidates into a single leftmost-longest matcher:

1. an existing dictionary entry keeps mapping to its original key even once the global rule is
   deleted;
2. a newly hit global rule appends an encrypted record to the repository dictionary and gets a
   new key from it;
3. every dictionary change persists atomically before any Git object is written;
4. an existing placeholder span is opaque; matching never runs again inside a token;
5. preset allowances and user allowances are subtracted after entropy, built-in rules and
   explicit registrations contribute their candidates; user allowances win over blocks;
6. output is built hit by hit as a stream, never materializing "all hit ranges", so auxiliary
   memory stays bounded on highly repetitive input.

The management commands offer no show / decrypt / export:

```text
agit secrets review [--repo <path>] [--json]
agit secrets allow <record-id> [--repo <path>]
agit secrets unallow <record-id> [--repo <path>]
agit secrets block add <label> [--stdin] [--allow-short] [--repo <path>]
agit secrets block remove <record-id> [--repo <path>]
```

Repository `allow` applies to an exact record from any origin and wins over global registrations
and repository blocks. The persisted `heuristic_disposition` field carries this decision for
every record origin. The device's `$AGIT_HOME/.agit-allow-secrets` file supplies additional
exact-value allowances to both automatic projection and client scans. `unallow` restores the
record's default protection; `block remove` clears only the explicit bit. A v1 record is read as
a legacy explicit block and remains protected unless allowed.

The envelope's `_object_hash`, the root session claim and the remote-visible meta are all
computed from the placeholder projection; they must not carry a deterministic digest of the
plaintext projection. On a secret hit during live RC, including a repository-only block, only
the hash of the protected projection is sent; such an item gives up the remote cross-check against
the unredacted hash, so the hub cannot verify guesses offline against a low-entropy candidate.

## 6. Read path

Git clone / fetch / pull always keep the worktree and the object database in the remote's
placeholder form; writing plaintext back after checkout would dirty the repository at once,
and the next push would leak it again.

Authorized local `show` and `view` output, runtime materialization (`resume` / `run`), and
local memory mirrors hydrate known mappings. Display reads do not create or migrate a vault.
`show --raw` retains canonical placeholders. Missing mappings leave the exact token unchanged,
without routine warnings. Corrupt or unreadable existing dictionaries fail explicitly.

Export, share, RC live events, and peer history are outward boundaries and retain protection.
A device-local history source does not make its remote recipient a local restoration surface.
RC sessions with no selected Agent repository withhold suspicious content with a protection
error; they cannot invent a repository or claim an irreversible mask is locally recoverable.
The native transcript remains the local source. Privacy import creates an independent local
copy, then projects secrets using its selected Agent repository before recording it. Optional
path/host/IP anonymization follows secret projection and remains intentionally irreversible.

## 7. Security boundary

This design protects secret values replaced by placeholders in published Git history and
protects the encrypted vault file on its own. It does not protect a complete local dictionary
directory backup: new and migrated dictionaries keep the key beside the ciphertext (§4), so
that backup carries the ability to decrypt every mapping. Treat repository copies containing
Git metadata, backups of `$AGIT_HOME/repos`, and offline disks containing these files as
sensitive. Git push excludes this local metadata, but filesystem backups do not.

Only an unmigrated dictionary uses the legacy keystore layout. With the legacy file keystore
(05, §3.2), its key is under `$AGIT_HOME/keystore/`; with the OS backend, the key remains in
the system credential store. Such a dictionary requires both its encrypted file and access
to the legacy key. A backup of `$AGIT_HOME` can already include both the repository and its
file-keystore key, so even that legacy layout does not imply backup isolation.

The design does not defend against an attacker who controls the local process, can read the
local key and ciphertext, has access to the legacy keystore, or controls the runtime files.
A decrypted secret lives briefly in process memory during materialization, and once it
reaches the runtime it falls under the runtime's own plaintext-transcript security boundary.

A collaborator who knows a valid placeholder can replay it within the same remote history. v1
treats such a token as a repository-internal capability: it resolves only when the user
explicitly materializes the session, never automatically inside a pull hook, a shell, a Git
checkout or the background daemon. Opening automatic hydration to untrusted collaborators
takes a further authentication tag bound to the content object / JSON path; "a random id is
hard to guess" is not a replay defense.

## 8. Failure semantics and migration

- no dictionary: this repository has no mapping; the write path may create one on the first
  hit, the read path keeps the unknown token;
- a dictionary with a missing keystore entry, corrupt JSON or a failed AEAD authentication:
  read and write both fail closed;
- the atomic write fails: no Git commit referencing the new placeholder is formed;
- a new global explicit rule hits an already settled plaintext prefix: the continuity check
  refuses and asks for an explicit migration; a new heuristic record may complete its forward
  projection in the next snapshot, but the object bytes of the parent commit are not
  rewritten. That decision compares the record sources that would really change the settled
  prefix, not "did this run create a record", so a no-op, a parse failure or a CAS conflict
  still recovers on a retry after the dictionary has persisted;
- a single heuristic hit over the repository record capacity: local settlement refuses before
  publishing a version; the native source remains intact, and a PEM header is never replaced
  on its own in a way that conceals the remaining sensitive block. The payload of a base64
  data URL whose bytes open with a media or archive file header (an inline screenshot, a PDF,
  a gzip or zip body) is neither a candidate nor such a hit, so a pasted image never blocks
  settlement; the same bytes outside that carrier, or a token that merely starts like a file
  header, are still reported;
- old history still holds plaintext: the push gate keeps refusing, and commits/tags are never
  rewritten in the background;
- another device holds only the placeholder: it preserves unresolved tokens quietly and
  never asks the hub for the secret dictionary.

## 9. Acceptance criteria

- the same secret reuses one key inside a repository, and gets a different key across
  repositories;
- the vault, the Git tree, commit messages, logs and warnings never carry the original secret;
- quotes, backslashes, newlines, Unicode, overlaps and spanning JSON fields all behave
  deterministically;
- many repeated hits do not first build an unbounded match `Vec`;
- settling again after a commit still passes continuity;
- resume, merge, doctor and diff judge a native transcript against hydrated committed content:
  a record registered after settlement whose value sits inside the settled prefix leaves an
  untouched transcript resumable instead of reporting a rewrite;
- resume hydrates when the dictionary is present, and keeps the token quietly when it
  is not;
- a missing keystore entry and an authentication failure on any record both fail closed;
- push still refuses a secret in old history or outside the protection surface;
- a retry after the heuristic dictionary has persisted still allows forward projection, while
  an explicit/global rule hitting an old prefix is still refused;
- a settlement carrying more new candidates than any fixed batch size still enters the
  dictionary in one pass, a common long PEM is reversible, and an over-capacity PEM stays
  visible to the push scanner;
- a registered hit during live RC sends no unredacted plaintext hash.


## 10. Identity evidence and coverage

An identity exemption applies to a proven occurrence, not to all equal byte strings. The
shared identity module parses whole candidates, checks the repository's declared object format,
and resolves abbreviations across all object types. Commit-labelled results require commits.
AgentGit full and display-short version IDs retain the SHA-1 version contract; a reserved
version tag must peel to the suffix it names and the version must contain valid metadata.

Supported native Git/agit evidence is reconstructed from paired Codex `exec_command` function
calls or Claude Code `Bash` calls and their returned text. Only literal single commands with
known identity-output forms qualify. Shell scripts, pipelines and custom log formats do not
supply that proof. A later assistant citation labelled `commit`, `version`, `ref` or `revision`
can reuse the verified object relationship. Credential fields, user-authored keys and unrelated
text still undergo discovery. Explicit registrations override even a proven identity occurrence.

Publication reconstructs this evidence from validated saved LOG events. No process-local cache
or dictionary entry is needed to remember an identity. Native session fields additionally match
the runtime instances recorded in valid session metadata. `cwd_is_agent_repository` records
whether the native cwd shared the Agent repository's object database at settlement; it permits
read-only validation after relocation, including a clone without the original dictionary. It
never authorizes a new runtime owner or selects a writable session. References into a separate
code repository require that repository to remain available for verification.

Live display derives its text and paths from the protected native record. A delta containing a
resolvable object identity waits for its completed native record, which supplies the command
context needed to preserve that occurrence. Saved native evidence also seeds history paging,
live sharing and settlement of newly appended regions after restart.

LOG/VIEW index entries are exempt only at their storage location and only when their event
relationships validate. A blob reused at an ordinary shared-file path loses that structural
exemption. Metadata observations remain scanned: fields such as `milestone`, `cwd`, branch and
origin are not a way to waive arbitrary content. Content hashes must recompute. Git header
references and parsed LFS pointer OIDs have similarly narrow field exemptions.

Identity reconstruction is bounded. Candidate resolutions and native command associations are
capped; historical evidence reads share a byte budget. Unavailable, malformed or exhausted
evidence grants fewer exemptions. It cannot establish that an unchecked carrier is clean.

Automatic file projection supports UTF-8 text, with decoded JSON keys and values in JSON/JSONL
carriers. Non-UTF-8 or NUL-bearing shared files are retained with an explicit unsupported-text
notice. Memory collection retains unsupported content in its native source and reports the
reason. Binary/archive/container decoding is outside this text protection boundary. Publication
reports binary/unreadable coverage separately; absence of text findings is not proof that a
binary payload is safe. LFS text payload inspection also reports size and availability failures.

Repository `allow` is a local decision to stop future projection and client findings for that
exact value; old tokens continue to hydrate. Strict server policy does not inherit a local allow
decision, so it can still reject the plaintext.
The device allowlist matches complete values, never provider-prefix substrings. Inline pragmas
and explicit publication overrides are disclosure decisions with their existing client/server
scope. Default suspicion handling uses reversible projection and needs none of these overrides.
