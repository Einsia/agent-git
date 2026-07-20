# Encryption-as-access-control: per-session readable-by sets

Status: approved design (2026-07-20). Decisions locked by the owner; implementation
proceeds wave-by-wave. Produced by a 3-approach / 3-judge design panel, synthesized
and then narrowed by owner decisions.

## Goal

Make "who can read a session" a per-session property enforced by **encryption**, not
only by the hub ACL:

- readable to the **public**,
- readable to **certain people**,
- default: readable to **team members but NOT the public**.

## Winning shape: zero-trust E2E with a team KEK

Client-held keys; the hub stores only ciphertext + un-unwrappable envelopes. A fully
compromised hub cannot read a private session. Structurally this is the age/PGP
per-recipient model **plus** a per-org group key (Team KEK) so the team-default case
does not cause rewrap storms.

### Two axes, kept separate

1. **Authorization** (existing `acl::decide`, unchanged): who may *fetch* bytes.
   Server-enforced, **retroactive** (can deny a removed reader future pulls). Governs
   git smart-http + the JSON API; 404 non-disclosure preserved.
2. **Confidentiality** (new keybox): who can *decrypt* fetched bytes. Crypto-enforced,
   **non-retroactive**.

They fail independently: a hub compromise defeats axis 1 (attacker pulls ciphertext)
but not axis 2 (cannot decrypt). The client **derives** the keybox recipient set from
the folded ACL, so the two stay consistent; `acl::decide` gains no crypto inputs.
`agit hub doctor` reconciles drift (member-without-stanza = authorized-but-cannot-
decrypt; stanza-without-membership = holds-key-but-hub-refuses-fetch).

### Key hierarchy (three layers, each in one place)

1. **Per-user identity key**: the existing ed25519 signing key at
   `$AGIT_HOME/identity/ed25519` (`0600`, client-only, never uploaded). Its X25519
   encryption keypair is **derived from the same secret** via the ed25519->curve25519
   (Edwards->Montgomery) map, with HKDF domain separation. One on-disk secret both
   signs pushes and unwraps content keys. Public halves published to the hub registry.
2. **Per-session content key CK**: the 32-byte keyring master `crypt.rs` already
   consumes, now minted **per aid** (not machine-global). `rotate_key()` mints the next
   generation. `crypt.rs` seal/open/derive/nonce/wire-header are reused **byte-for-byte**.
3. **Per-org Team KEK (TK)**: a 32-byte symmetric group key with a generation counter
   (`orgs.current_kek_gen`). TK seals CK for team-readable sessions (one keybox stanza);
   TK is itself X25519-sealed to each member's pubkey (one `team_keks` row per member).

Wrapping chain: `content <-(AEAD, CK)-`; `CK <-(X25519 seal)- individual reader`
and/or `CK <-(AEAD under TK_gen)- team`; `TK_gen <-(X25519 seal)- each member`.

### Data model (all additive)

New tables via `CREATE TABLE IF NOT EXISTS` (invitations precedent, no schema-version
bump), new column via a back-fill list (TOTP_COLUMNS precedent). Both SQLite + Postgres.

- `identity_keys(username PK, ed25519_pub, x25519_pub, epoch, enroll_sig, created, revoked)`
  -- the ONE shared registry (serves provenance signing AND encryption recipient lookup).
  `enroll_sig = ed25519_sign(username || epoch || ed25519_pub || x25519_pub)`; the server
  verifies possession and can only ever **replace** a row (epoch bump), never mint one.
- `team_keks(org, gen, recipient, wrapped_kek, recipient_epoch, created, PRIMARY KEY(org,gen,recipient))`
  -- per-member envelopes of TK_gen; ciphertext only.
- `orgs.current_kek_gen BIGINT NOT NULL DEFAULT 0` (new ORG_COLUMNS back-fill list).
- `agents.encrypted BIGINT DEFAULT 0` -- optional, non-authoritative, list-UI only.

No new visibility/readers column: the readable-by set lives cryptographically in
`keybox.jsonl`; `agents_v2.members` stays the axis-1 authorization mirror.

New Store methods (targeted upserts/deletes, not whole-table snapshots):
`upsert_identity_key`, `get_identity_keys(usernames)`, `upsert_team_kek_envelopes`,
`get_team_kek_envelope(org, gen, recipient)`, `list_team_kek_gens(org)`.

### Wire format

Content wire format **unchanged**: `MAGIC(b"AGITCRYPT\0",10) || version=2 || key-id u32 LE
|| nonce(24) || ct+tag`. `key-id` is reinterpreted as the per-session CK generation (was
machine-global keyring index) -- a pure reinterpretation; v1 + key-id-0 fallbacks preserved.

New artifact `.agit/keybox.jsonl` at repo root, committed, **excluded from the crypt
filter** via `.gitattributes` (`/.agit/keybox.jsonl -filter`) -- it is already wrap-
ciphertext; filtering would double-encrypt and deadlock bootstrap. One JSON object per line:

- individual: `{"v":1,"kid":K,"t":"user","to":"bob","epoch":E,"epk":...,"nonce":...,"wrap":<CK sealed via HKDF(ecdh(epk,bob_x25519))+XChaCha20-Poly1305>}`
- team: `{"v":1,"kid":K,"t":"team","org":"acme","gen":G,"nonce":...,"wrap":<CK sealed under TK_G>}`
- public: `{"v":1,"kid":K,"t":"public","key":<hex CK in the clear>}`

Individual + public stanzas are self-contained (no-hub case works); team stanzas need
the TK envelope from `team_keks` (or an out-of-band TK file).

### Git filter interaction

Content path untouched. The only change: `keys_for_filter` gains an unwrap provider.
Encrypted sessions store their keyring at a repo-local path (`.git/agit-crypt/keyring`,
`0600`) resolved before the machine-global `$AGIT_HOME/crypt` one; legacy global-key /
no-hub path unchanged. A new `agit crypt unlock` runs on clone/checkout: reads
`keybox.jsonl`, finds my stanzas (`user` via my X25519 key / `team` via the TK envelope /
`public`), unwraps each kid's CK, writes the repo-local keyring; `keys_for_filter` then
feeds seal/open normally. Fail-closed (`filter.agit-crypt.required=true`) preserved:
unlock failure => smudge exits nonzero (loud ciphertext-refusal, never silent plaintext).
Adding/removing a reader edits only `keybox.jsonl`, so encrypted blobs never re-clean.

### API + CLI surface

Hub (all clear `acl::decide` where relevant; base releases ciphertext only):
- `POST /api/identity/enroll {ed25519_pub,x25519_pub,epoch,enroll_sig}` -- upsert caller's
  own row; requires `epoch > stored`; server verifies `enroll_sig`.
- `GET /api/identity/{user}` and `GET /api/identity?users=a,b,c` -- any authenticated
  caller (pubkeys are public); serves signing-verify AND wrap-recipient lookup.
- `POST /api/orgs/{org}/kek/envelopes {gen,envelopes:[...]}` -- org-admin publishes TK_gen
  envelopes, bumps `current_kek_gen`.
- `GET /api/orgs/{org}/kek/envelope?gen=G` -- returns the CALLER'S OWN envelope; gated by
  org membership; ciphertext only.
- `GET /api/orgs/{org}/kek/gens` -- generations available to the caller.
- Wave-5 opt-in: `POST /api/agents/{owner}/{name}/keys/release` -- only when org
  `escrow_mode='hub-assist'`; releases CK after `acl::decide(...,Read)=Allow`, fail-closed.

Client (`agit`): `identity enroll [--rotate]`, `identity show [user]`,
`-a <agent> encrypt [--readers a,b | --team | --public]`,
`-a <agent> readers add <user>|--public`, `readers rm <user>`, `readers ls`,
`-a <agent> rekey`, `crypt unlock`, `hub team rekey <org> [--rekey-all]`, `hub doctor`.

### Recipient operations (owner decisions applied)

- ADD individual reader: fetch + verify their key, X25519-seal CK for the current kid
  (and past kids if `--full-history`), append `user` stanza, add to ACL `Read`. O(1) in
  team size, no content re-encrypt.
- ADD team member: seal current TK to their X25519, insert one `team_keks` row. O(1),
  instantly unlocks every team-wrapped session.
- REMOVE individual reader: **eager** -- rotate the session CK (`rotate_key`), re-wrap to
  remaining readers, drop the stanza, remove from ACL. Instant forward secrecy.
- REMOVE team member: `hub team rekey <org>` rotates TK (gen++), re-seals to remaining
  members (O(members)); **default eager** per-session CK re-wrap under TK'; `--rekey-all`
  is the bulk form.
- ROTATE: per-session `rekey` O(1); org-wide TK gen++ O(members).

### Custody / recovery -- DECISION: client-only, no hub escrow (default)

Private ed25519 (and derived X25519) live at `$AGIT_HOME/identity/ed25519` (`0600`), never
uploaded; the hub holds only public halves. Consequence (stated ruthlessly): a lost device
with no user backup locks that user out of every private session (data exists, unreadable).

Default recovery = **recovery-by-regroup**: re-enroll a fresh keypair (epoch+1, self-
signed); any still-enrolled teammate re-seals the current TK to the new pubkey (O(1)) and
the user instantly regains read on every team session. Users SHOULD back up the 32-byte
identity secret (paper/QR/password-manager). Genuinely unrecoverable: a session wrapped
ONLY to a single lost individual (no team stanza, no other holder).

### TOFU -- DECISION: hard-fail on pubkey change

A changed pubkey (epoch bump) stops the operation until the user explicitly re-pins after
an out-of-band fingerprint check. Blocks hub key-substitution MITM; noisier onboarding.

## Non-goals (explicit)

- **Retroactive revocation of already-fetched data.** "Remove a reader" is forward-only.
- **Metadata confidentiality.** The hub sees session existence, sizes, DAG shape,
  timestamps, **file paths + commit messages** (the content filter encrypts neither), and
  the **recipient set** (keybox is cleartext).
- **Convergent-nonce equal-line leak** -- improved to per-session scope, not eliminated.
- **Registry key-substitution MITM** -- mitigated (enroll_sig + epoch + TOFU + ed25519-
  also-signs), not solved; transparency log deferred.
- **Availability against the hub** -- encryption does not replace the ACL.

## Implementation waves

1. **Shared identity registry** (foundation; also ships provenance cross-team trust).
   `identity_keys` table, enroll/get API, `agit identity enroll/show`, ed25519->X25519
   derivation, `enroll_sig` self-signature + verification.
2. **Per-session keyring + individual/public keybox** (no-hub-capable). Reinterpret key-id
   as per-session CK gen; repo-local keyring resolution; `keybox.jsonl` + `.gitattributes`
   exclusion; `crypt unlock`; `encrypt`; `readers add/rm/ls` (eager remove); `rekey`.
3. **Team KEK** (churn/recovery win). `team_keks` table, `current_kek_gen`, envelope
   publish/fetch API, `team` stanza, team unlock, `hub team rekey`.
4. **Zero-config team default + day-2 hygiene.** Auto-emit the team stanza on encrypt under
   an org; `hub doctor` drift reconciliation; `--rekey-all`.
5. **Optional opt-in escrow (off by default).** (a) per-org offline recovery recipient;
   (b) hub-assist `acl::decide`-gated CK release for orgs that accept hub trust.

## Deferred / later refinements

- Per-team (sub-group) KEKs to bound blast radius (v1 is per-org with short generations).
- Merkle transparency log of enrollments.
