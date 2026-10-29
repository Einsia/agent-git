# Plain publication with precise credential replacement

Status: Accepted direction; implementation in progress.

This document replaces encrypted session publication with one plain format. It keeps the
local, reversible and nonblocking contracts of [the local privacy RFC](rfc-local-privacy.md)
and narrows what is replaced automatically. Where the two disagree, this document wins.
[The envelope protocol](privacy-envelope.md) stays the reference for reading sessions that
were already published encrypted.

## 1 Decision

1. **One produced format.** New clients publish every session as plain. `session_owner_v2`
   stops being produced. The Hub keeps reading stored encrypted snapshots, and keeps
   accepting them from released clients that still produce them, until those clients age out.
2. **Replace only high-confidence credentials**: provider signatures, PEM private keys,
   credential-field assignments and values the user explicitly blocked. Entropy alone is
   evidence of opacity, not of a credential, so it is reported and replaced only on request.
3. **Short typed placeholders** that never enter protocol fields.
4. **Replacement happens at capture and commit**, incrementally, before Git objects exist.
   Push verifies integrity and transports objects. It never scans and never refuses.
5. **The Hub re-checks after accepting a push**, without blocking it. A session with a
   high-confidence hit is held back from public surfaces and its owner is told.
6. **Reversible restore stays account-scoped.** The account dictionary keeps its role; the
   account key gains KMS protection; the owner can reveal placeholders in the browser, and
   everyone else only ever receives placeholders.
7. **Anonymization is opt-in** per repository or per user, and uses the same dictionary so a
   resumed session gets its original paths back.

## 2 Why

The platform holds account keys and decrypts for the web, search and recovery. End-to-end
encryption was therefore never a guarantee against the operator; it protected readers from
each other, and it cost a second rendering stack, a second session format and a second set
of budgets. The observed failures came from that cost, not from a missing cipher:

- Four mechanisms overlapped: reversible secret replacement, session envelopes, projection-only
  path aliasing, and fragment-key shares, next to a repository-password envelope and a
  separate fail-closed scanner.
- Entropy discovery replaced runtime identifiers and ordinary code tokens. One audited batch of
  15,021 replaced values contained no credential. Placeholders landed in protocol fields, so a
  Codex `call_id` grew past its length limit and the runtime refused the resumed session.
- Encrypted views projected events into one runtime shape, omitted whole screens of tool
  activity, and attributed commits to a synthetic privacy author.
- The browser, the server and the CLI each enforced different envelope budgets, so a session
  could be accepted, stored and still be unreadable.
- A session's mode became immutable at first push, and dictionary synchronization could fail
  at the network edge without anyone noticing, because failures were silent by design.

## 3 Trade-offs against the session-owner design

The session-owner design ([envelope protocol](privacy-envelope.md)), and the
repository-password design before it, made three promises. This design keeps the first,
replaces the second with repository membership plus anonymization, and stops claiming the
third, which never held.

| Promise | Session-owner design | This design |
| --- | --- | --- |
| Credentials never reach readers | Sanitized projection for readers; original behind grants | Placeholders for every reader; the owner reveals locally |
| Repository members see only what the owner grants | Per-session and per-commit grants | Repository membership is the boundary; anonymization and path exclusion narrow content |
| The operator cannot read originals | Not actually held: the Hub keeps KMS-protected private keys and serves content keys | Not claimed |

What we keep from that design: KMS protection of account key material, now applied to the
dictionary key; session registration and inventory, now recording plain only; the workspace
alias idea, now stored reversibly in the dictionary; and device-managed mandatory exclusions.

What we give up: a private repository member who was not granted originals could previously
read only a projection. They now read the plain session with credentials replaced. Repository
visibility and membership become the only reader boundary, which is the boundary every other
repository artifact already has. Teams that need more use anonymization and path exclusion.

What we gain: one format that the Hub renders with the shared IR, search and translation over
real content, runtime and author attribution preserved, no projection budgets, no receipt
mode lock-in for new sessions, and no reason for placeholders to touch protocol fields.

## 4 Detection

The detector keeps its existing engine: one keyword automaton, lazily compiled rules, bounded
findings and partial results. Findings carry a confidence tier.

| Tier | Sources | Default |
| --- | --- | --- |
| High | Provider rules (vendored gitleaks and AgentGit rules), PEM private-key regions, credential-field assignments (`password = …`, `"api_key": "…"`), user and repository explicit blocks | Replaced |
| Heuristic | Bare high-entropy tokens and gitleaks' `generic-api-key` | Reported by explicit diagnostics; replaced only when the user opts in |

`generic-api-key` is a heuristic and not a provider rule: it accepts any opaque value after a name
containing `auth`, `key`, `token` and similar words and almost any separator, including a comma, so
a runtime's beta list (`"oauth-2025-04-20","interleaved-thinking-…"`) or a document's `fileKey`
matches it. Credential-field assignments cover its real cases with a stricter grammar: the name must
be a credential name and not a resource token (`file_token`, `nextPageToken`), and an unquoted value
that reads as code is not a credential: a reference or path (`token: &State`,
`secret_filter::Session`), or a digit-free camel-case identifier in a program declaration (`const
token = AuthProvider`, `let mut secret = Store`, `static token = Provider;`). The same camel-shaped
value in a shell, `.env`, INI or YAML assignment is still one, including shell `local`/`readonly`
and YAML flow mappings, whatever follows it.

User allows still override default heuristics, and explicit blocks still win over allows. An
explicit block never needs a heuristic. Turning heuristic replacement on later does not rewrite
committed history; it applies to newly captured content.

## 5 Placeholders

New replacements use `‹CLASS:KIND:ID›`, for example `‹secret:github-pat:k3m7x2q6w4ab›`.

- `CLASS` is `secret` for credentials. Anonymization uses `path`, `user`, `host` and
  `omitted` (section 8).
- `KIND` names the rule family in lowercase ASCII, at most 32 bytes: `github-pat`,
  `aws-access-token`, `private-key`, `url-password`, `credential`, `blocked`, `high-entropy`.
  It tells a reader what was removed without revealing it.
- `ID` is 12 characters of lowercase base32 drawn at random. It identifies a record in the
  account dictionary. It is not derived from the value, so equal tokens in two sessions reveal
  only that one account replaced the same value, never the value itself.

`{{AGIT_SECRET_V1:…}}` and `{{AGIT_SECRET_V2:…}}` tokens stay valid, opaque and restorable.
Nothing rewrites history to the new spelling. A value first replaced under a legacy token gets a
new-format alias on its next replacement.

Placeholders never enter protocol fields. The native schema layout already names identity and
linkage fields per runtime (Codex `call_id` and item IDs, Claude Code tool-use IDs, OpenCode,
Hermes, OpenClaw and WorkBuddy references); opaque carriers such as thinking `signature`,
`redacted_thinking` data and Codex `encrypted_content` join that list. Those fields are not
scanned for replacement at all. A credential inside a runtime-opaque blob is not displayed by
any reader, and replacing it would break the runtime on resume.

## 6 Where work happens

Capture, import and commit call the existing privacy facade before object creation, with the
same bounded worker and the same fail-open contract: a skipped or partial scan publishes what
it could not protect, and the server re-check (section 7) is the backstop.

Push verifies object integrity, registers new sessions as plain and transports objects. It
does not scan, does not rewrite objects, and has no privacy refusal. Dictionary
synchronization is scheduled independently, as before.

A session registered as `session_owner_v2` by an older client continues as plain. A registered
mode is otherwise immutable, so the Hub accepts exactly one transition, from `session_owner_v2`
to `plain` at the next revision, requested by the session owner. The owner's next push
registers that revision and publishes a plain snapshot whose parents are the accepted encrypted
commits. Earlier encrypted snapshots stay encrypted and readable. Another account cannot change
the mode of a session it does not own; its client reports that and publishes the rest.

## 7 Server re-check

After a push is accepted, the Hub scans the newly published plain session snapshots with the
high tier of the same detector (the backend links this crate's detector). It never runs inside
the receive request and it never rejects a push.

A hit marks the session *suspected credential* with the rule kinds and the commit that carried
them. A suspected session is left out of search results, translation, and the repository
session lists shown to readers without write access. Its owner is notified: the owner's alert
list and the session page show it to repository writers, and an email is sent when the Hub has
mail delivery configured. Writers keep seeing the session so they can fix it. A direct link
keeps working, because immutable reads are served to shared caches where a per-reader decision
would leak; rendering the hit spans masked is the step that covers direct readers.

The flag clears when a later snapshot of the session no longer contains a hit. A writer can
dismiss it after review; the dismissal lasts until a later snapshot carries more findings.
Legacy encrypted snapshots are not re-checked: readers only ever saw their sanitized projection.

## 8 Restore and reveal

The account dictionary remains the only source of originals:

- The CLI stores mappings locally, encrypts packages with the account key, and synchronizes
  them; every device of the account restores placeholders on resume and materialization.
- The account key is stored under KMS envelope protection, the same facility that protects
  session keys. Existing plaintext keys are wrapped in place; the plaintext column is removed
  in a later contract step once every row is wrapped.
- In the browser, the signed-in owner can reveal placeholders of their own dictionary. The page
  fetches the key and packages only after an explicit action, decrypts with WebCrypto in memory,
  and shows each value masked until it is clicked. A revealed value is drawn outside the
  document's text (not selectable, not part of copy, not part of a code block's copy button).
  Other readers never receive a key or a package and only see the typed placeholder.

## 9 Anonymization

Off by default; enabled per repository or for the user. When enabled, capture replaces:

- the home directory with `‹path:home:…›` (shown as `~`),
- the session's project root with `‹path:workspace:…›` (shown as `<workspace>`),
- the local user name and host name with `‹user:name:…›` and `‹host:name:…›`,
- user-defined literal or pattern rules with `‹custom:LABEL:…›`.

These are dictionary records like secrets, so resume restores the original paths. Path
exclusion replaces the whole result of a tool call that read an excluded path with an
`‹omitted:tool-result:…›` record, again restorable by its owner. Runtime user-level memory
(`~/.claude/**`, `~/.codex/**` and their equivalents) is excluded by default even when the rest
of anonymization is off, because those files describe the person rather than the task.

## 10 Budgets

Encrypted production disappears, and with it the client input, private-layer, projection and
envelope budgets. The remaining limits are ordinary transport and storage limits that the Hub
owns. The Hub publishes them in its configuration document (`/api/hub/config`), and clients and
the web read them there instead of compiling their own copies. Reading legacy envelopes uses the published envelope limit, so a stored
snapshot the server accepted is always readable by the browser.

## 11 Compatibility and rollout

- The backend ships first: plain revision for encrypted sessions, re-check, KMS wrapping and
  published limits. The CLI follows; the web reveal follows the KMS change.
- Responses keep `encryption_enabled` for 0.2.13 and 0.2.14 clients, and the receipt-less
  receive path stays for released clients.
- The Hub keeps accepting `session_owner_v2` receipts from released clients. Removing that
  acceptance is a later decision with its own announcement.
- The backend pins this crate by revision. A detector or placeholder change merges here first
  and is then adopted by a backend pin update.

## 12 Removal

After the steps above land: the encrypted publication producer (`privacy_publication`,
`privacy_layer`, envelope sealing, receipt outbox for encrypted snapshots, original-access
grants and `privacy unlock`), the repository-password remnants, the fail-closed scanner in
`domain::secrets` once `agit scan` runs on the privacy detector, and the browser's decryption
renderer for new sessions. Reading stored encrypted sessions keeps the envelope parser and the
original-key path until stored encrypted sessions are migrated or retired.
