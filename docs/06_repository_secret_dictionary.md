# RFC: Repository-scoped reversible secret placeholders

Status: Implemented by the merge request that introduces this document.

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

v1 automatically protects only a session's `session/log.jsonl`, `session/VIEW` and the commit
subject generated from that turn's user prompt. Shared files, arbitrary Git headers, tag
messages and history that predates the feature are still refused by the existing push gate;
the tool cannot claim to rewrite those objects without changing their commit IDs.

Two kinds of candidate enter the dictionary automatically: literals the user registered
explicitly in the global vault, and heuristic hits the built-in rules find in a session's
semantic content fields. A heuristic record is protected by default, and the user can set it
to allow by opaque id; allow only switches off further projection, it does not delete the
reverse mapping, so an old placeholder still hydrates. Protocol identity fields — session /
object / ref / schema / signature / timestamp — never enter the dictionary automatically, and
a JSON key is never a heuristic candidate. One settlement adds at most 1024 distinct
candidates; a candidate already present in this repository's dictionary (an allowed one
included) does not spend that budget again, so a long append-only session is never blocked
forever at its 1025th historical candidate. Going over the limit on additions fails before any
Git object is written.

An automatic repository candidate uses a 64 KiB plaintext cap and a padded ciphertext bucket
of at most 128 KiB, enough to hold a common 4096-bit PEM private key reversibly; a manually
registered global rule keeps its 512 UTF-8 byte cap. A heuristic hit that exceeds even the
repository record cap leaves that input verbatim in the settlement, with no partial
replacement that could mask the large hit and no dictionary record; the repo-wide push gate
downstream still recognizes and refuses that plaintext.

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
- shared files and commit/tag headers outside the session's automatic protection surface;
- a corrupt dictionary, a missing keyring, or a conversion that did not finish.

This delivers what the user observes — what goes up is the key, what comes back hydrates on
this device — without faking Git's identity model.

## 3. Module boundary

Both chains live in the one `domain::secret_filter` domain module because they share four
security primitives:

- the KEK in the OS keyring;
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
export never carry it away. The file reuses the global vault's envelope encryption; the KEK
lives only in the system credential store, and the vault file holds only the wrapped DEK and
per-record AEAD ciphertext. Copying the repository directory without the matching keyring
entry must fail explicitly; an absent keyring entry is never read as an empty dictionary.

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
another repository, is kept verbatim and warned about — never guessed at, never fetched over
the network, never replaced with an empty string.

## 5. Write path

`agit commit` parses every parsable JSONL line into a `serde_json::Value` and walks string
values and arrays recursively; matching and replacement happen on the decoded UTF-8 string,
which is then serialized canonically. An explicit block and an already effective repository
record still protect each of their occurrences verbatim. A secret carrying quotes,
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
5. `explicit_block OR (heuristic_origin AND disposition=protect)` decides whether a record is
   effective;
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

An explicit block always wins over an allow. `block remove` clears only the explicit bit;
while the heuristic disposition is still protect, the record stays effective. A v1 repository
record is read as a legacy explicit block, so an upgrade never silently relaxes protection.

The envelope's `_object_hash`, the root session claim and the remote-visible meta are all
computed from the placeholder projection; they must not carry a deterministic digest of the
plaintext projection. On a locally registered hit during live RC, only the hash of the
redacted projection is sent; such an item deliberately gives up the remote cross-check against
the unredacted hash, so the hub cannot verify guesses offline against a low-entropy candidate.

## 6. Read path

Git clone / fetch / pull always keep the worktree and the object database in the remote's
placeholder form; writing plaintext back after checkout would dirty the repository at once,
and the next push would leak it again.

The repository dictionary is unlocked in memory, and known tokens hydrated, only when the
user explicitly runs `agit resume` / `agit run` and materializes `session/VIEW` into the local
runtime. The hydrated result goes straight to the runtime adapter and is never written back to
Git. A session stays usable with no dictionary on this device: the tokens are kept and an
unresolved count is reported.

## 7. Security boundary

This design protects against a leak of the remote, the logs, an ordinary backup or the vault
file on its own; it does not defend against an attacker who already controls the local
process, the system credential store or the runtime files. A decrypted secret lives briefly in
process memory during materialization, and once it reaches the runtime it falls under the
runtime's own plaintext-transcript security boundary.

A collaborator who knows a valid placeholder can replay it within the same remote history. v1
treats such a token as a repository-internal capability: it resolves only when the user
explicitly materializes the session, never automatically inside a pull hook, a shell, a Git
checkout or the background daemon. Opening automatic hydration to untrusted collaborators
takes a further authentication tag bound to the content object / JSON path; "a random id is
hard to guess" is not a replay defense.

## 8. Failure semantics and migration

- no dictionary: this repository has no mapping; the write path may create one on the first
  hit, the read path keeps the unknown token;
- a dictionary with a missing keyring, corrupt JSON or a failed AEAD authentication: read and
  write both fail closed;
- the atomic write fails: no Git commit referencing the new placeholder is formed;
- a new global explicit rule hits an already settled plaintext prefix: the continuity check
  refuses and asks for an explicit migration; a new heuristic record may complete its forward
  projection in the next snapshot, but the object bytes of the parent commit are not
  rewritten. That decision compares the record sources that would really change the settled
  prefix, not "did this run create a record", so a no-op, a parse failure or a CAS conflict
  still recovers on a retry after the dictionary has persisted;
- a single heuristic hit over the repository record capacity: local settlement keeps the full
  plaintext and the push gate refuses; one short rule hit (a PEM header, say) is never
  replaced on its own in a way that hides the full rule;
- old history still holds plaintext: the push gate keeps refusing, and commits/tags are never
  rewritten in the background;
- another device holds only the placeholder: it warns explicitly about unresolved tokens and
  never asks the hub for the secret dictionary.

## 9. Acceptance criteria

- the same secret reuses one key inside a repository, and gets a different key across
  repositories;
- the vault, the Git tree, commit messages, logs and warnings never carry the original secret;
- quotes, backslashes, newlines, Unicode, overlaps and spanning JSON fields all behave
  deterministically;
- many repeated hits do not first build an unbounded match `Vec`;
- settling again after a commit still passes continuity;
- resume hydrates when the dictionary is present, and keeps the token with a warning when it
  is not;
- a missing keyring and an authentication failure on any record both fail closed;
- push still refuses a secret in old history or outside the protection surface;
- a retry after the heuristic dictionary has persisted still allows forward projection, while
  an explicit/global rule hitting an old prefix is still refused;
- appending the 1025th candidate after 1024 existing ones still enters the dictionary, a
  common long PEM is reversible, and an over-capacity PEM stays visible to the push scanner;
- a registered hit during live RC sends no unredacted plaintext hash.
