//! The Hub's persistent state: users, agents, tokens, and merge requests in a relational database.
//!
//! Two backends sit behind one [`Store`] enum:
//!   - **Postgres** (production) — selected when `AGIT_HUB_DB` is a `postgres://` URL.
//!   - **SQLite** (zero-config self-host + tests) — the default, a `hub.db` file under `<root>`.
//!
//! `<root>` is 0700 and `hub.db` (with its `-wal`/`-shm` sidecars) is 0600 — they hold credential
//! digests and access-control facts. The old JSON store's "temp file + rename" atomicity is now a
//! **database transaction**: the read-modify-write `update_*` methods `SELECT` the table, run the
//! caller's closure, then rewrite the table (`DELETE` + re-`INSERT`) inside one transaction, so a
//! concurrent reader always sees a consistent snapshot and the reconcile read+lookup+write stays one
//! critical section.
//!
//! Every method here is **async**. The axum server drives the shared sqlx pool directly (the handlers
//! `.await` the store); the sync CLI subcommands bridge to it with a short-lived tokio runtime. The
//! `update_*` closures run **synchronously** between the SELECT and the atomic rewrite — a closure
//! must not call back into a `Store` method, but that has never been needed (each only mutates the
//! `Vec` it is handed).
//!
//! Concurrent writers are serialized per backend: SQLite takes a process-wide async `Mutex` around a
//! tracked transaction, Postgres takes one global `pg_advisory_xact_lock` — both reproduce what the
//! old in-process Mutex gave for free, so two writers never clobber each other's `DELETE`+re-`INSERT`
//! snapshot. The SQLite transaction is a plain tracked `begin()`, so sqlx auto-rolls it back if the
//! handler future is dropped mid-write (a cancelled request cannot leave the write lock held).

use super::acl::{AgentAcl, Lifecycle, Role, Scope, Visibility};
use super::mr::Mr;
use serde::{Deserialize, Serialize};
use sqlx::Row as _;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The metadata schema version the running binary writes/expects. Recorded in a backup manifest so a
/// restore can note (and a future tool could gate on) the shape of the DB it is carrying.
pub fn schema_version() -> i64 {
    SCHEMA_VERSION
}

fn is_expired(iso: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(t) => chrono::Utc::now() >= t.with_timezone(&chrono::Utc),
        // An unreadable timestamp = do not dare treat it as valid. Failure errs toward "expired".
        Err(_) => true,
    }
}

// ─────────────────────────── users ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct User {
    pub username: String,
    pub pw_hash: String,
    pub salt: String,
    /// Derivation parameters, shaped like `argon2id$v=19$m=19456,t=2,p=1` — stored with the hash, so
    /// retuning them locks nobody out.
    pub kdf: String,
    #[serde(default)]
    pub is_admin: bool,
    #[serde(default)]
    pub created: String,
    /// The user's TOTP shared secret (base32, RFC 4648), or `None` if 2FA was never enrolled. Present
    /// while enrollment is PENDING (secret set, [`totp_enabled`](Self::totp_enabled) still false) and
    /// while 2FA is ACTIVE. This is a **symmetric secret** the server must hold in the clear to verify
    /// codes — it cannot be hashed like a password. It is therefore as sensitive as `pw_hash` and is
    /// stored the same way (in the users table, same file-mode/serialization discipline).
    /// NOTE: at-rest DB encryption is out of scope; this column holds the secret unencrypted, exactly
    /// as the password material is stored.
    #[serde(default)]
    pub totp_secret: Option<String>,
    /// Whether 2FA is ACTIVE. A non-null `totp_secret` with this `false` means an enrollment is pending
    /// (generated but not yet confirmed) and login is NOT yet gated on a second factor.
    #[serde(default)]
    pub totp_enabled: bool,
    /// sha256 digests of the one-time backup codes — never the plaintext (which is shown once, at
    /// confirm). A consumed code's digest is removed from this list.
    #[serde(default)]
    pub totp_backup_codes: Vec<String>,
    /// Whether this account's email has been VERIFIED (a challenge token minted for the address was
    /// consumed, or an admin force-marked it). `false` for every account until it verifies. This is the
    /// anti-squatting gate: [`Store::get_identity_keys_by_email`] attributes a self-asserted committer
    /// email to an account ONLY when this is `true`, so an UNVERIFIED (possibly squatted) email resolves
    /// to no identity and provenance degrades to `SignedUnregistered` instead of a false `VerifiedAs`.
    /// Added additively (back-filled onto older stores by `USER_COLUMNS`), so every pre-existing account
    /// reads back UNVERIFIED — the safe default.
    #[serde(default)]
    pub email_verified: bool,
    /// Whether this account is DISABLED (soft-suspended by an admin). A disabled account still exists —
    /// it keeps its username, agents, and history — but cannot log in (see `api_login`, which returns a
    /// clear 403 once the password itself has been verified, so it is never a password oracle). Default
    /// `false` (active). Added additively (back-filled onto older stores by `USER_COLUMNS`), so every
    /// pre-existing account reads back ENABLED — the safe, unchanged default.
    #[serde(default)]
    pub disabled: bool,
}

/// Username rules: lowercase [a-z0-9._-], 2..=32, no leading dot. Login names are case-insensitive →
/// normalize before storing, or "Alice" and "alice" become two accounts that can impersonate each
/// other.
///
/// This is a **syntactic** check only, applied to both usernames and — since the two share one URL
/// namespace segment — the owner half of a `/owner/name.git` path. Reserved names (see
/// [`is_reserved_account`]) still pass here so the migrated `_unclaimed` namespace stays routable;
/// creation paths refuse them separately.
pub fn valid_username(name: &str) -> bool {
    let n = name.len();
    (2..=32).contains(&n)
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
}

/// The account that adopts legacy null-owner repos at migration time. Reserved: nobody may register
/// it (see [`is_reserved_account`]), so no user or org can be handed the repos it holds — they stay
/// private/admin-only until re-owned via `agit-hub add --owner`. It is still a valid URL segment, so
/// `/_unclaimed/<name>.git` routes to those repos for the site admin.
pub const UNCLAIMED: &str = "_unclaimed";

/// Whether a name is reserved and may not be registered as a user or an org. Applied at every account
/// creation site (`user add`, `api_register`, `api_orgs_create`) — NOT inside `valid_username`, so a
/// reserved name stays a legal URL/namespace segment while being unclaimable.
pub fn is_reserved_account(name: &str) -> bool {
    name == UNCLAIMED
}

/// The **namespace segment** for an owner: the single canonical string used in every URL, repo
/// directory, and blob key. A user-owned agent stores the bare username (`alice` → `alice`); an
/// org-owned agent stores `org:<name>` (`org:acme` → `acme`). Because `valid_username` forbids `:`,
/// the `org:` prefix is unforgeable, and because a username and an org name may never share a bare
/// string (the unified-account rule, enforced at creation), this segment maps to exactly one account.
pub fn owner_ns(owner: &str) -> &str {
    owner.strip_prefix("org:").unwrap_or(owner)
}

/// Canonicalize a committer email for storage and lookup: trim surrounding whitespace and lowercase it.
/// Email local-parts are technically case-sensitive, but in practice nobody relies on that, and git
/// commits are attributed case-insensitively here — so "Dev@X.com" and "dev@x.com" address one identity.
pub fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

pub fn normalize_username(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

/// A stable device-key fingerprint for the composite `(username, key_fpr)` primary key: the hex of the
/// first 16 bytes of `sha256(ed25519_pub)`. Deterministic and collision-resistant enough to key one
/// device key per account row, and short enough to name on a command line (`agit identity revoke <fpr>`)
/// or an account page. Computed over the hex string as stored, so the same key always maps to the same
/// fingerprint regardless of who submits it.
pub fn ed25519_fingerprint(ed25519_pub: &str) -> String {
    // sha256_hex yields 64 hex chars (32 bytes); the first 32 are the first 16 bytes — a 128-bit tag.
    crate::convo::sha256_hex(ed25519_pub.trim())[..32].to_string()
}

/// Minimum password length. Hoisted here so the CLI (`read_new_password`) and self-service
/// registration (`api_register`) consume ONE constant and can never drift on password strength.
pub const MIN_PASSWORD_LEN: usize = 8;

// ─────────────────────────── agent metadata ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub username: String,
    /// "read" | "write" | "admin"
    pub role: String,
}

// ─────────────────────────── organizations ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgMember {
    pub username: String,
    /// "member" | "admin" — the **org** role, not the agent-level `acl::Role`.
    pub role: String,
}

impl OrgMember {
    /// The org role folded down to the agent-level [`Role`] it grants on every agent the org owns.
    /// "member" → Write (push/read), "admin" → Admin (manage). Junk drops (fail-safe), mirroring
    /// `to_acl`'s `Role::parse` filter — an unrecognized role grants nothing.
    pub fn agent_role(&self) -> Option<Role> {
        match self.role.as_str() {
            "member" => Some(Role::Write),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Org {
    pub name: String,
    #[serde(default)]
    pub members: Vec<OrgMember>,
    #[serde(default)]
    pub created: String,
    /// The org's active Team-KEK generation (encryption-recipients Wave 3). 0 = no TK has ever been
    /// minted. Carried on the struct (not just a bare column) so the whole-table `update_orgs`
    /// snapshot-rewrite preserves it across an unrelated member edit rather than resetting it to 0.
    #[serde(default)]
    pub current_kek_gen: i64,
    /// OPT-IN offline recovery recipient (encryption-recipients Wave 5, feature 1). Hex-encoded 32-byte
    /// X25519 public key held OFFLINE by an org owner (paper / air-gapped device); EMPTY = unset (the
    /// default, exactly the wave-1..4 behavior). When set, `agit hub team rekey` additionally seals the
    /// Team KEK to this key under the reserved `@recovery` recipient, so whoever holds the matching
    /// offline SECRET can re-seal the current TK to a lost-key member's fresh pubkey. This RE-TRUSTS the
    /// offline admin and WEAKENS forward secrecy for the team: the offline holder can decrypt every TK
    /// generation they were sealed to. Only the hub's PUBLIC half of nothing lives here — it is the
    /// recovery party's own public key, so the hub still never sees a plaintext TK.
    #[serde(default)]
    pub recovery_x25519: String,
    /// OPT-IN hub-assist escrow mode (encryption-recipients Wave 5, feature 2): `"none"` (the default,
    /// byte-for-byte wave-1..4 behavior) or `"hub-assist"`. When `hub-assist`, a session owner MAY wrap
    /// its content key under the hub's escrow key and store it, and the hub RELEASES that CK to any caller
    /// who passes the SAME `acl::decide(_, Read)` gate as git fetch. This RE-TRUSTS the hub and is the one
    /// path that gives retroactive-for-unfetched revocation. Settable only by an org owner.
    #[serde(default = "escrow_mode_none")]
    pub escrow_mode: String,
    /// Whether a plain (non-admin) MEMBER may create agents under the org. `1` = yes (the GitHub
    /// default — members can create), `0` = admins only. Stored as BIGINT for the same strict-decode
    /// reason as every other integer column. Additive: a fresh DB gets the column from `DDL`, older
    /// stores from the `ORG_COLUMNS` back-fill (DEFAULT 1), so every pre-existing org reads back as
    /// members-can-create — the permissive GitHub default, unchanged from before this field existed.
    #[serde(default = "members_can_create_default")]
    pub members_can_create: i64,
}

/// The default (and only opt-out) escrow mode: `none`. A dedicated fn so `#[serde(default)]` on a
/// deserialized org with no `escrow_mode` field reads back as the safe off state, never an empty string.
fn escrow_mode_none() -> String {
    "none".to_string()
}

/// The default for `Org::members_can_create`: `1` (members CAN create, the GitHub default). A dedicated
/// fn so `#[serde(default)]` on a deserialized org that predates the field reads back permissive rather
/// than as `0` (admins-only), keeping the pre-field behavior.
fn members_can_create_default() -> i64 {
    1
}

/// The reserved recipient id under which a per-org offline recovery envelope is filed in `team_keks`
/// (encryption-recipients Wave 5). It begins with `@`, which `valid_username` forbids, so it can never
/// collide with a real member's envelope.
pub const RECOVERY_RECIPIENT: &str = "@recovery";

/// One member's envelope of a Team KEK generation (encryption-recipients Wave 3): `wrapped_kek` is the
/// TK_gen X25519-sealed to `recipient`'s pubkey — CIPHERTEXT only, so the hub never holds a plaintext
/// TK. `recipient_epoch` is the identity-key epoch the seal targeted (stale-envelope detection).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamKekEnvelope {
    pub org: String,
    pub gen: i64,
    pub recipient: String,
    pub wrapped_kek: String,
    #[serde(default)]
    pub recipient_epoch: i64,
    #[serde(default)]
    pub created: String,
}

/// One session's content key sealed under the HUB's escrow public key (encryption-recipients Wave 5,
/// hub-assist escrow). `wrapped_ck` is CIPHERTEXT ONLY — CK X25519-sealed to the hub's per-hub escrow
/// public key, packed exactly like a `team_keks` envelope (`epk‖nonce‖ciphertext`, base64), so only the
/// hub PRIVATE key can open it. Keyed on (owner_ns, name, kid): one row per session content-key
/// generation. Only ever written when the owning org is in `escrow_mode = 'hub-assist'` and the owner
/// opts in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EscrowKey {
    pub owner: String,
    pub name: String,
    pub kid: i64,
    pub wrapped_ck: String,
    #[serde(default)]
    pub created: String,
}

/// A pending (or resolved) invitation into an org — the consent flow that replaces the silent
/// admin-add. An admin creates one PENDING row; the invited user alone flips it to `accepted` (which
/// mints the membership) or `declined`; an admin may `revoke` a still-pending one. The row is kept
/// after it resolves (status is a durable record, not a delete) — only `org rm` sweeps them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invitation {
    /// Unguessable id (`inv_<hex>`), minted server-side like a token id — it is the accept/decline
    /// handle handed to the invitee, so it must not be enumerable.
    pub id: String,
    /// The org, by its normalized name.
    pub org: String,
    /// The invited user's normalized username.
    pub invitee: String,
    /// The org role the membership gets on accept — "member" | "admin".
    pub role: String,
    /// "pending" | "accepted" | "declined" | "revoked".
    pub status: String,
    /// The admin who issued it.
    pub created_by: String,
    #[serde(default)]
    pub created: String,
}

impl Invitation {
    /// Whether this invitation is still awaiting the invitee's decision.
    pub fn is_pending(&self) -> bool {
        self.status == "pending"
    }
}

/// One DEVICE key a person has published in the ONE shared identity registry (encryption-recipients
/// design, Wave 1). It serves two lookups: provenance signing-key verification (the `ed25519_pub`) and
/// encryption recipient key-wrapping (the `x25519_pub`, used from Wave 2). Only ever public halves live
/// here; the private key never leaves the client.
///
/// **Many keys per account (SSH-keys style).** An account registers a SET of device keys, one per
/// machine, keyed by the composite `(username, key_fpr)` — so enrolling from a second machine ADDS a key
/// rather than overwriting the first, and provenance verification matches a session's signing key against
/// ANY of the account's non-revoked device keys. (Before this, `identity_keys` was `PRIMARY KEY(username)`
/// with a single key, so a second machine silently clobbered the first and every session signed on the
/// first machine then falsely read "key mismatch".)
///
/// `enroll_sig = ed25519_sign(username ‖ epoch ‖ ed25519_pub ‖ x25519_pub)` over the exact bytes of
/// [`crate::agent::identity_enroll_message`], verified server-side against the SUBMITTED `ed25519_pub`
/// — so a write proves possession of the private key and the hub can only replace a row, never mint a
/// valid one. `epoch` is monotonic PER DEVICE KEY (a higher epoch is a rotation of THAT key; a lower or
/// equal one is refused), and `revoked` is a set-once tombstone timestamp — a revoked device key no
/// longer counts as a provenance match (revocation actually de-attributes its sessions).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct IdentityKey {
    pub username: String,
    /// A stable per-device fingerprint of `ed25519_pub` — the second half of the composite primary key
    /// `(username, key_fpr)`, so ONE account can register MANY device keys (GitHub-SSH-keys style) without
    /// a second machine's enrollment overwriting the first. Derived by [`ed25519_fingerprint`] (the hex of
    /// the first 16 bytes of `sha256(ed25519_pub)`); the facade fills it in when a caller leaves it blank.
    #[serde(default)]
    pub key_fpr: String,
    pub ed25519_pub: String,
    pub x25519_pub: String,
    /// A human label for the device this key lives on (the machine hostname by default), self-asserted at
    /// enroll time — the "which of my machines is this?" hint the account page and `agit identity keys`
    /// render. Empty for a legacy single-key row back-filled to `'default'` on migration.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub epoch: i64,
    pub enroll_sig: String,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub revoked: Option<String>,
    /// The account's git committer email, self-asserted at enroll time (empty for a legacy row that
    /// pre-dates this column, or a client that sent none). It is the bridge from a session's committer
    /// email to a registered identity: provenance verification asks "does this email map to a registered
    /// account, and is the signing key that account's key?". Stored NORMALIZED (trimmed, lowercased).
    ///
    /// NOTE: this is NOT part of `enroll_sig` (the possession proof), so it is a self-asserted attribute —
    /// the hub does not verify email ownership (it has no email-verification flow). The forgery it defends
    /// against is a session signed by a key that is not the registered key of the claimed email; email
    /// squatting is a separate, documented limitation.
    #[serde(default)]
    pub email: String,
}

/// The outcome of an [`Store::add_identity_key`]: either the row was written, or it was refused
/// because the submitted epoch does not strictly advance the stored one (monotonic, no rollback). The
/// check is performed under the same write lock as the write, so it is race-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrollOutcome {
    /// The row was created (first enrollment) or replaced (a strictly higher epoch).
    Applied,
    /// Rejected: `submitted <= stored`. Carries the stored epoch so the API can explain the refusal.
    StaleEpoch { stored: i64 },
}

impl Org {
    /// Whether `user` can manage the org (add/remove members, and manage every agent it owns).
    pub fn is_admin(&self, user: &str) -> bool {
        self.members.iter().any(|m| m.username == user && m.role == "admin")
    }

    /// Whether `user` belongs to the org in any role.
    pub fn is_member(&self, user: &str) -> bool {
        self.members.iter().any(|m| m.username == user)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMeta {
    pub name: String,
    /// The agent's identity. **The authoritative value is agent.toml inside the store** (minted by
    /// the client, committed into history); this is only the Hub's cache of what it has seen, and
    /// may be null (the repo is still empty / has no agent.toml yet).
    #[serde(default)]
    pub aid: Option<String>,
    /// None = unowned: an old repo migrated in and not yet claimed. Only the site admin can touch it
    /// (see acl::decide).
    #[serde(default)]
    pub owner: Option<String>,
    /// "private" | "public". **New ones default to private.**
    #[serde(default = "default_visibility")]
    pub visibility: String,
    /// "active" | "archived" | "deleted". Absent in files written before lifecycles existed, which
    /// is exactly what `default_lifecycle` is for — an old agent is a live one.
    #[serde(default = "default_lifecycle")]
    pub lifecycle: String,
    /// One line, for the agent list. An agent nobody can describe is one nobody adopts.
    #[serde(default)]
    pub description: Option<String>,
    /// The agent this one was forked from, at fork time. A label for humans: it is **not** an
    /// identity link, since the fork gets its own aid the moment it is rebound.
    #[serde(default)]
    pub forked_from: Option<String>,
    /// The **aid** of the agent this one was forked from. Stored beside the name and not derived
    /// from it, because the name is a mutable label — the source can be renamed, and lineage keyed on
    /// a stale name would turn a routine fork back into a reported collision.
    ///
    /// Lineage only, never permission: `identity::reconcile` uses it to tell an inherited aid from a
    /// stolen one, and it can never cause an aid to be cached.
    #[serde(default)]
    pub forked_from_aid: Option<String>,
    /// The conflicting aid already reported for this agent, if any.
    ///
    /// A conflict is a **state**, not an event: it is re-derived on every read, and auditing each
    /// re-derivation grows audit.log without bound and buries the one row that matters under copies
    /// of itself. This is what makes the audit row fire on the transition into the state instead.
    #[serde(default)]
    pub aid_conflict: Option<String>,
    /// Usernames who starred this agent. Per-user, and deliberately not a count: the count is
    /// derivable, the list is not.
    #[serde(default)]
    pub stars: Vec<String>,
    #[serde(default)]
    pub members: Vec<Member>,
    #[serde(default)]
    pub created: String,
}

fn default_visibility() -> String {
    "private".into()
}

fn default_lifecycle() -> String {
    "active".into()
}

impl AgentMeta {
    pub fn new(name: &str, owner: Option<&str>, visibility: Visibility) -> AgentMeta {
        AgentMeta {
            name: name.to_string(),
            aid: None,
            owner: owner.map(|s| s.to_string()),
            visibility: visibility.as_str().to_string(),
            lifecycle: Lifecycle::Active.as_str().to_string(),
            description: None,
            forked_from: None,
            forked_from_aid: None,
            aid_conflict: None,
            stars: vec![],
            members: vec![],
            created: now_iso(),
        }
    }

    /// Metadata → the facts the authorization decision needs. **An unrecognized visibility is
    /// treated as private**, and an unrecognized role is dropped — hand-mangling agents errs in
    /// the direction of "locked down tighter".
    ///
    /// An unrecognized lifecycle reads as **archived**: tighter than active (nothing can be written
    /// through a state nobody can parse) but still visible, so the operator can see the agent and
    /// fix it. Falling back to `deleted` would be tighter still and is the wrong trade — a
    /// typo would silently erase an agent from every listing.
    pub fn to_acl(&self) -> AgentAcl {
        AgentAcl {
            name: self.name.clone(),
            owner: self.owner.clone(),
            visibility: Visibility::parse(&self.visibility).unwrap_or(Visibility::Private),
            lifecycle: Lifecycle::parse(&self.lifecycle).unwrap_or(Lifecycle::Archived),
            members: self
                .members
                .iter()
                .filter_map(|m| Role::parse(&m.role).map(|r| (m.username.clone(), r)))
                .collect(),
        }
    }

    /// If this agent is owned by an org, the org's name. The owner field is namespaced as
    /// `"org:<name>"`; `store::valid_username` forbids ':' , so this can never collide with a real
    /// username — meaning `acl::decide`'s owner check can never match a user against an org owner, and
    /// org access arrives ONLY through folded members (see `to_acl_with_org`).
    pub fn org_owner(&self) -> Option<&str> {
        self.owner.as_deref().and_then(|o| o.strip_prefix("org:"))
    }

    /// This agent's namespace segment (see [`owner_ns`]), if it has an owner. None only for the
    /// fail-safe synthesized `agent_or_unowned` value, which has no owner at all.
    pub fn owner_ns(&self) -> Option<&str> {
        self.owner.as_deref().map(owner_ns)
    }

    /// The namespace segment for building this agent's repo dir / blob key. Falls back to
    /// [`UNCLAIMED`] when the owner is None — but a real DB row always has an owner (the column is NOT
    /// NULL), so that fallback is only ever the synthesized fail-safe, which never reaches storage.
    pub fn seg(&self) -> &str {
        self.owner_ns().unwrap_or(UNCLAIMED)
    }

    /// The canonical scoped id `"<owner_ns>/<name>"` — what a token binds to and what `acl::decide`
    /// compares against.
    pub fn scoped(&self) -> String {
        format!("{}/{}", self.seg(), self.name)
    }

    /// Whether this row is the one addressed by URL segment `seg` and `name`.
    pub fn matches(&self, seg: &str, name: &str) -> bool {
        self.owner_ns() == Some(seg) && self.name == name
    }

    /// `to_acl`, plus the owning org's members folded into the ACL members list. Pure and sync — the
    /// already-resolved `org` is passed in, so `acl::decide` never learns "org" exists (org membership
    /// is expanded BEFORE it runs). The no-org path (`org = None`) is byte-for-byte `to_acl`.
    ///
    /// Folding only ever ADDS or RAISES a grant: a folded role is merged by max against any explicit
    /// per-agent member, so it can never lower an explicit member's role. Only usernames literally in
    /// `org.members` are inserted, so no non-member gains access.
    pub fn to_acl_with_org(&self, org: Option<&Org>) -> AgentAcl {
        let mut acl = self.to_acl();
        if let Some(org) = org {
            for om in &org.members {
                if let Some(role) = om.agent_role() {
                    match acl.members.iter_mut().find(|(n, _)| n == &om.username) {
                        // Dedupe by keeping the HIGHER role (Role is Ord: Admin > Write > Read).
                        Some(e) => {
                            if role > e.1 {
                                e.1 = role;
                            }
                        }
                        None => acl.members.push((om.username.clone(), role)),
                    }
                }
            }
        }
        acl
    }

    pub fn role_of(&self, user: &str) -> Option<Role> {
        self.members.iter().find(|m| m.username == user).and_then(|m| Role::parse(&m.role))
    }

    /// The parsed lifecycle, with the same fail-safe as `to_acl` — one source of truth for both, so
    /// a route can never read a state the decision point disagrees with.
    pub fn lifecycle(&self) -> Lifecycle {
        Lifecycle::parse(&self.lifecycle).unwrap_or(Lifecycle::Archived)
    }
}

// ─────────────────────────── token ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRec {
    /// A stable id for revocation. Old records may have none → backfilled from the digest prefix on
    /// load (a digest is not a credential, so it is safe to use as an id).
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// The token's owner. **Old tokens have no owner** — that was exactly the old "one token = the
    /// whole host" model. An ownerless token yields no identity under the new model and is dead
    /// (see `authenticate`); no admin permission is silently inherited.
    #[serde(default)]
    pub owner: Option<String>,
    /// Some(name) = valid for that one agent only.
    #[serde(default)]
    pub agent: Option<String>,
    /// "read" | "write". In old files this field is called access, with the same value range — an
    /// alias recognizes it directly.
    #[serde(alias = "access")]
    pub scope: String,
    /// **Only the token's sha256 digest is stored**, never the plaintext.
    pub hash: String,
    #[serde(default)]
    pub created: String,
    /// None = never expires.
    #[serde(default)]
    pub expires: Option<String>,
    #[serde(default)]
    pub last_used: Option<String>,
}

impl TokenRec {
    pub fn expired(&self) -> bool {
        self.expires.as_deref().map(is_expired).unwrap_or(false)
    }

    /// Whether it can authenticate: needs an owner (old ownerless tokens cannot), a recognizable
    /// scope, and no expiry.
    pub fn usable(&self) -> bool {
        self.owner.is_some() && Scope::parse(&self.scope).is_some() && !self.expired()
    }
}

/// Entries in an old auth.json have no id. A digest is not a credential (the plaintext cannot be
/// recovered from it), so using its prefix as a stable id is safe.
fn derive_token_id(hash: &str) -> String {
    format!("tok_{}", hash.chars().take(12).collect::<String>())
}

pub fn new_token_id() -> io::Result<String> {
    Ok(format!("tok_{}", &super::kdf::gen_secret()?[..12]))
}

/// Mint an invitation id: `inv_` + 16 CSPRNG hex chars. Unguessable, mirroring [`new_token_id`] — the
/// id is the accept/decline handle, so it must not be enumerable.
pub fn new_invite_id() -> io::Result<String> {
    Ok(format!("inv_{}", &super::kdf::gen_secret()?[..16]))
}

/// root is a credential directory: 0700, owner-only. When the directory already exists the mode has
/// no effect (mode only applies at creation), so tighten it explicitly afterwards.
pub fn ensure_root(root: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    // 0700 owner-only on Unix; on Windows directory security is by ACL, so the mode is a no-op there.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(root).or_else(|e| if root.is_dir() { Ok(()) } else { Err(e) })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

// ─────────────────────────── row mapping ───────────────────────────

/// Uniform column access over the two backend row types, so the domain-struct construction below is
/// written once. Every read is fail-safe: a missing or wrong-typed column yields the type's empty
/// value, mirroring the JSON store's leniency (a hand-mangled record loses only itself).
trait Cols {
    fn text(&self, col: &str) -> String;
    fn opt(&self, col: &str) -> Option<String>;
    fn int(&self, col: &str) -> i64;
}

impl Cols for sqlx::sqlite::SqliteRow {
    fn text(&self, col: &str) -> String {
        self.try_get::<String, _>(col).unwrap_or_default()
    }
    fn opt(&self, col: &str) -> Option<String> {
        self.try_get::<Option<String>, _>(col).unwrap_or(None)
    }
    fn int(&self, col: &str) -> i64 {
        // SQLite has a single dynamic INTEGER type, so an i64 read always decodes it.
        self.try_get::<i64, _>(col).unwrap_or(0)
    }
}

impl Cols for sqlx::postgres::PgRow {
    fn text(&self, col: &str) -> String {
        self.try_get::<String, _>(col).unwrap_or_default()
    }
    fn opt(&self, col: &str) -> Option<String> {
        self.try_get::<Option<String>, _>(col).unwrap_or(None)
    }
    fn int(&self, col: &str) -> i64 {
        // Postgres is strict about decode types: an i64 only decodes INT8/BIGINT, never INT4. The
        // integer columns (is_admin, schema_version.version) are therefore declared BIGINT — see DDL
        // — so this i64 read is correct on both backends. Reading them as i32 here would be the other
        // valid fix; declaring BIGINT keeps a single code path.
        self.try_get::<i64, _>(col).unwrap_or(0)
    }
}

/// TEXT column holding serde_json → Vec<T>. A parse error defaults to empty, matching the JSON
/// store, where a broken `members`/`stars` value dropped only itself rather than the whole record.
fn parse_json_vec<T: for<'de> Deserialize<'de>>(s: &str) -> Vec<T> {
    if s.is_empty() {
        return vec![];
    }
    serde_json::from_str(s).unwrap_or_default()
}

/// Serialize a slice to a JSON TEXT column value; an (unreachable) serialization failure degrades to
/// an empty array rather than corrupting the row. The inverse of [`parse_json_vec`].
fn json_text<T: Serialize>(v: &[T]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}

/// Row-counts of the core credential tables, taken from a live store to stamp a backup manifest and
/// then re-taken from the finished snapshot to prove it captured the same data (never a silent short
/// backup). Only the tables an operator would notice missing: users, agents, tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RowCounts {
    pub users: i64,
    pub agents: i64,
    pub tokens: i64,
}

/// `SELECT COUNT(*)` the three core tables over an arbitrary SQLite pool.
async fn count_on_pool(pool: &sqlx::SqlitePool) -> io::Result<RowCounts> {
    async fn one(pool: &sqlx::SqlitePool, table: &str) -> io::Result<i64> {
        let row = sqlx::query(&format!("SELECT COUNT(*) AS n FROM {table}")).fetch_one(pool).await.map_err(err)?;
        Ok(row.int("n"))
    }
    Ok(RowCounts {
        users: one(pool, "users").await?,
        agents: one(pool, "agents").await?,
        tokens: one(pool, "tokens").await?,
    })
}

/// Count the core tables in a standalone SQLite file (read-only, no migration) — used to verify a
/// backup snapshot captured the live rows. The file is opened on its own short-lived pool that is
/// closed before returning.
pub async fn count_sqlite_rows(path: &Path) -> io::Result<RowCounts> {
    use sqlx::sqlite::SqliteConnectOptions;
    let opts = SqliteConnectOptions::new().filename(path).read_only(true);
    let pool = sqlx::SqlitePool::connect_with(opts).await.map_err(err)?;
    let counts = count_on_pool(&pool).await;
    pool.close().await;
    counts
}

fn row_user(r: &impl Cols) -> User {
    User {
        username: r.text("username"),
        pw_hash: r.text("pw_hash"),
        salt: r.text("salt"),
        kdf: r.text("kdf"),
        is_admin: r.int("is_admin") != 0,
        created: r.text("created"),
        // The Cols readers are fail-safe (a missing column yields the empty value), so even a store
        // that predates `add_totp_columns` reads back as "no 2FA" rather than erroring.
        totp_secret: r.opt("totp_secret"),
        totp_enabled: r.int("totp_enabled") != 0,
        totp_backup_codes: parse_json_vec(&r.text("totp_backup_codes")),
        // Fail-safe like every other Cols read: a store that predates `USER_COLUMNS` has no
        // `email_verified` column, which reads back as 0 → UNVERIFIED, the safe default.
        email_verified: r.int("email_verified") != 0,
        // Fail-safe like every other Cols read: a store that predates the `disabled` column reads back
        // as 0 → ENABLED (active), the safe default that leaves every pre-existing account able to log in.
        disabled: r.int("disabled") != 0,
    }
}

/// One row of the single-use, expiring email-verification token store. The `token` is a CSPRNG value
/// (a random capability, not a secret to hash) that the operator forwards to the address being proven;
/// consuming it marks the owning account's email verified. Single-use (deleted on consume) and expiring
/// (`expires`), so a leaked-but-stale link cannot be replayed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmailVerifyToken {
    pub token: String,
    pub username: String,
    pub email: String,
    pub expires: String,
    #[serde(default)]
    pub created: String,
}

/// One row of the single-use, expiring PASSWORD-RESET token store. Structurally a sibling of
/// [`EmailVerifyToken`] but living in its OWN table with a `prt_` prefix, so a reset capability and a
/// verification capability are never interchangeable. Consuming it (deleting the row) authorizes setting
/// a fresh password for `username` without the old one. Single-use + expiring, so a leaked-but-stale
/// link cannot be replayed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PasswordResetToken {
    pub token: String,
    pub username: String,
    pub expires: String,
    #[serde(default)]
    pub created: String,
}

fn row_agent(r: &impl Cols) -> AgentMeta {
    let visibility = r.text("visibility");
    let lifecycle = r.text("lifecycle");
    AgentMeta {
        name: r.text("name"),
        aid: r.opt("aid"),
        owner: r.opt("owner"),
        visibility: if visibility.is_empty() { default_visibility() } else { visibility },
        lifecycle: if lifecycle.is_empty() { default_lifecycle() } else { lifecycle },
        description: r.opt("description"),
        forked_from: r.opt("forked_from"),
        forked_from_aid: r.opt("forked_from_aid"),
        aid_conflict: r.opt("aid_conflict"),
        stars: parse_json_vec(&r.text("stars")),
        members: parse_json_vec(&r.text("members")),
        created: r.text("created"),
    }
}

fn row_token(r: &impl Cols) -> TokenRec {
    let mut t = TokenRec {
        id: r.text("id"),
        name: r.text("name"),
        owner: r.opt("owner"),
        agent: r.opt("agent"),
        scope: r.text("scope"),
        hash: r.text("hash"),
        created: r.text("created"),
        expires: r.opt("expires"),
        last_used: r.opt("last_used"),
    };
    // Old records with no id: backfill a stable one from the digest, exactly as the JSON store did.
    if t.id.is_empty() {
        t.id = derive_token_id(&t.hash);
    }
    t
}

/// mrs.data is the whole `Mr` as JSON. A row that will not parse is skipped, matching the JSON
/// store's per-record tolerance.
fn row_mr(r: &impl Cols) -> Option<Mr> {
    serde_json::from_str(&r.text("data")).ok()
}

fn row_org(r: &impl Cols) -> Org {
    Org {
        name: r.text("name"),
        members: parse_json_vec(&r.text("members")),
        created: r.text("created"),
        // Fail-safe: a store that predates the column reads back as gen 0 (no TK), never an error.
        current_kek_gen: r.int("current_kek_gen"),
        // Fail-safe defaults (Wave 5): a store predating these columns reads back unset/off — exactly
        // the wave-1..4 behavior. `r.text` yields "" for a missing/NULL column; an empty escrow_mode is
        // normalized to "none" here so downstream comparisons never see the empty string.
        recovery_x25519: r.text("recovery_x25519"),
        escrow_mode: {
            let m = r.text("escrow_mode");
            if m.is_empty() { escrow_mode_none() } else { m }
        },
        // Fail-safe: a store that predates the column (before the back-fill runs) reads back as 0. That
        // would be the RESTRICTIVE reading (admins-only), the opposite of the GitHub default this field
        // preserves — but the `ORG_COLUMNS` back-fill (DEFAULT 1) runs at boot before any request is
        // served, so every legacy row is 1 by the time it is read here.
        members_can_create: r.int("members_can_create"),
    }
}

fn row_escrow_key(r: &impl Cols) -> EscrowKey {
    EscrowKey {
        owner: r.text("owner"),
        name: r.text("name"),
        kid: r.int("kid"),
        wrapped_ck: r.text("wrapped_ck"),
        created: r.text("created"),
    }
}

fn row_team_kek(r: &impl Cols) -> TeamKekEnvelope {
    TeamKekEnvelope {
        org: r.text("org"),
        gen: r.int("gen"),
        recipient: r.text("recipient"),
        wrapped_kek: r.text("wrapped_kek"),
        recipient_epoch: r.int("recipient_epoch"),
        created: r.text("created"),
    }
}

fn row_identity_key(r: &impl Cols) -> IdentityKey {
    IdentityKey {
        username: r.text("username"),
        key_fpr: r.text("key_fpr"),
        ed25519_pub: r.text("ed25519_pub"),
        x25519_pub: r.text("x25519_pub"),
        label: r.text("label"),
        epoch: r.int("epoch"),
        enroll_sig: r.text("enroll_sig"),
        created: r.text("created"),
        revoked: r.opt("revoked"),
        email: r.text("email"),
    }
}

fn row_invitation(r: &impl Cols) -> Invitation {
    Invitation {
        id: r.text("id"),
        org: r.text("org"),
        invitee: r.text("invitee"),
        role: r.text("role"),
        status: r.text("status"),
        created_by: r.text("created_by"),
        created: r.text("created"),
    }
}

// ─────────────────────────── schema ───────────────────────────

/// One portable migration set for both backends. Only portable constructs are used (no SERIAL /
/// AUTOINCREMENT, no JSONB, no BOOLEAN, no native timestamps), so the DDL string is identical for
/// Postgres and SQLite; only the DML placeholder (`$1` vs `?`) differs and lives in each impl.
///
/// Integer columns are **BIGINT** (INT8), never INTEGER (INT4): Postgres decodes strictly, and the
/// `Cols::int` reader is `i64` — a plain INTEGER column would make `is_admin` and `version` fail to
/// decode on Postgres (silently, via `unwrap_or(0)`, dropping every user's admin bit and breaking
/// boot). SQLite treats "BIGINT" as INTEGER affinity, so the same DDL is correct there.
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS schema_version (id INTEGER PRIMARY KEY, version BIGINT NOT NULL)",
    // totp_* are the second-factor columns (added additively; also back-filled onto existing DBs by
    // `add_totp_columns`). totp_secret is nullable (null = never enrolled); totp_enabled is the active
    // flag; totp_backup_codes is a JSON array of sha256 digests. See User for the sensitivity note.
    // email_verified is the anti-squatting gate (added additively; also back-filled onto existing DBs by
    // `USER_COLUMNS`, exactly like the totp_* columns). 0 = unverified (the default), 1 = verified.
    // disabled is the admin soft-suspend flag (added additively; also back-filled by `USER_COLUMNS`).
    // 0 = active (the default), 1 = disabled (cannot log in).
    "CREATE TABLE IF NOT EXISTS users (\
       username TEXT PRIMARY KEY, pw_hash TEXT NOT NULL, salt TEXT NOT NULL, \
       kdf TEXT NOT NULL, is_admin BIGINT NOT NULL DEFAULT 0, created TEXT NOT NULL DEFAULT '', \
       totp_secret TEXT, totp_enabled BIGINT NOT NULL DEFAULT 0, totp_backup_codes TEXT NOT NULL DEFAULT '[]', \
       email_verified BIGINT NOT NULL DEFAULT 0, disabled BIGINT NOT NULL DEFAULT 0)",
    // Identity is (owner, name): a composite PRIMARY KEY, and owner is NOT NULL (it is the namespace).
    // No surrogate id — `update_agents` rewrites the whole table every write, so a surrogate would be
    // re-minted each time and buys nothing; the composite PK fits the snapshot model on both backends.
    "CREATE TABLE IF NOT EXISTS agents (\
       owner TEXT NOT NULL, name TEXT NOT NULL, aid TEXT, \
       visibility TEXT NOT NULL DEFAULT 'private', lifecycle TEXT NOT NULL DEFAULT 'active', \
       description TEXT, forked_from TEXT, forked_from_aid TEXT, aid_conflict TEXT, \
       stars TEXT NOT NULL DEFAULT '[]', members TEXT NOT NULL DEFAULT '[]', \
       created TEXT NOT NULL DEFAULT '', PRIMARY KEY (owner, name))",
    "CREATE INDEX IF NOT EXISTS agents_aid ON agents(aid)",
    "CREATE TABLE IF NOT EXISTS tokens (\
       id TEXT PRIMARY KEY, name TEXT NOT NULL, owner TEXT, agent TEXT, \
       scope TEXT NOT NULL, hash TEXT NOT NULL, created TEXT NOT NULL DEFAULT '', \
       expires TEXT, last_used TEXT)",
    // An MR is keyed on its TARGET agent, which is now (owner_ns, name) — so the target namespace is a
    // column and part of the PK. `id` is still unique per target agent.
    "CREATE TABLE IF NOT EXISTS mrs (\
       target_owner TEXT NOT NULL, target_agent TEXT NOT NULL, id BIGINT NOT NULL, data TEXT NOT NULL, \
       PRIMARY KEY (target_owner, target_agent, id))",
    // current_kek_gen is the org's active Team-KEK generation (encryption-recipients Wave 3): 0 = the
    // org has never minted a TK. Added additively (a fresh DB gets it here; older stores get it via the
    // `ORG_COLUMNS` back-fill, like the TOTP columns). BIGINT for the same strict-decode reason as every
    // other integer column.
    // recovery_x25519 (opt-in offline recovery recipient, Wave 5) defaults to '' = unset; escrow_mode
    // (opt-in hub-assist escrow, Wave 5) defaults to 'none' = off. Both are additive (fresh DBs get them
    // here; older stores get them via the `ORG_COLUMNS` back-fill), and with the defaults every wave-1..4
    // behavior is byte-for-byte unchanged.
    // members_can_create (1 = a plain member may create agents under the org, the GitHub default; 0 =
    // admins only) is additive like the Wave-5 columns above: a fresh DB gets it here, older stores get
    // it via the `ORG_COLUMNS` back-fill (DEFAULT 1), so every pre-existing org stays members-can-create.
    "CREATE TABLE IF NOT EXISTS orgs (\
       name TEXT PRIMARY KEY, members TEXT NOT NULL DEFAULT '[]', created TEXT NOT NULL DEFAULT '', \
       current_kek_gen BIGINT NOT NULL DEFAULT 0, \
       recovery_x25519 TEXT NOT NULL DEFAULT '', escrow_mode TEXT NOT NULL DEFAULT 'none', \
       members_can_create BIGINT NOT NULL DEFAULT 1)",
    // Org invitations (the consent flow). id is the unguessable accept/decline handle; status is one
    // of pending|accepted|declined|revoked. Added additively to DDL (a whole new table, unlike the
    // TOTP columns) so a fresh DB gets it and every existing store gets it created at boot via the
    // `CREATE TABLE IF NOT EXISTS` — no version bump or back-fill migration needed.
    "CREATE TABLE IF NOT EXISTS invitations (\
       id TEXT PRIMARY KEY, org TEXT NOT NULL, invitee TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'member', \
       status TEXT NOT NULL DEFAULT 'pending', created_by TEXT NOT NULL DEFAULT '', created TEXT NOT NULL DEFAULT '')",
    "CREATE INDEX IF NOT EXISTS invitations_org ON invitations(org)",
    "CREATE INDEX IF NOT EXISTS invitations_invitee ON invitations(invitee)",
    // The single-use, expiring email-verification token store. `token` is the CSPRNG capability handed to
    // the operator to forward; `expires` gates replay of a stale link; the row is DELETED on consume so a
    // token is single-use. Added additively as a whole new table (like `invitations`) via CREATE TABLE IF
    // NOT EXISTS — a fresh DB gets it here and every existing store gets it at boot, no version bump.
    "CREATE TABLE IF NOT EXISTS email_verify_tokens (\
       token TEXT PRIMARY KEY, username TEXT NOT NULL, email TEXT NOT NULL, \
       expires TEXT NOT NULL, created TEXT NOT NULL DEFAULT '')",
    // The single-use, expiring PASSWORD-RESET token store — a SEPARATE space from email-verify tokens
    // (distinct table + `prt_` prefix), so an email-verification token can NEVER be replayed to reset a
    // password and vice-versa: the two consume paths query disjoint tables. `token` is the CSPRNG
    // capability the operator forwards; `expires` gates a stale link; the row is DELETED on consume so a
    // token is single-use. No `email` column — a reset targets the account, not an address. Added
    // additively like every other table above, so no schema-version bump.
    "CREATE TABLE IF NOT EXISTS password_reset_tokens (\
       token TEXT PRIMARY KEY, username TEXT NOT NULL, \
       expires TEXT NOT NULL, created TEXT NOT NULL DEFAULT '')",
    // The ONE shared identity registry (encryption-recipients design, Wave 1): a person's published
    // ed25519 + X25519 public halves, self-signed via enroll_sig. Serves BOTH provenance signing-key
    // lookup AND (Wave 2+) encryption recipient key-wrapping. The primary key is COMPOSITE
    // `(username, key_fpr)` — ONE account, MANY device keys (SSH-keys style) — see IDENTITY_KEYS_DDL.
    IDENTITY_KEYS_DDL,
    // NOTE: the index on `email` is NOT here. `email` is a back-filled column (IDENTITY_COLUMNS), and on
    // an existing store CREATE TABLE IF NOT EXISTS is a no-op so `email` does not exist until the back-fill
    // runs. Creating the index in the DDL (before the back-fill) would fail with "no such column: email".
    // It is created via IDENTITY_EMAIL_INDEX after the back-fill in both migrate() paths.
    // Per-org Team-KEK envelopes (encryption-recipients Wave 3): one row per (org, generation, member),
    // holding TK_gen X25519-sealed to that member's pubkey. `wrapped_kek` is CIPHERTEXT only — the hub
    // never sees a plaintext TK. `recipient_epoch` records which identity-key epoch the seal targeted, so
    // a client can detect a stale envelope after a key rotation. Added additively like `invitations`.
    "CREATE TABLE IF NOT EXISTS team_keks (\
       org TEXT NOT NULL, gen BIGINT NOT NULL, recipient TEXT NOT NULL, wrapped_kek TEXT NOT NULL, \
       recipient_epoch BIGINT NOT NULL DEFAULT 0, created TEXT NOT NULL DEFAULT '', \
       PRIMARY KEY (org, gen, recipient))",
    "CREATE INDEX IF NOT EXISTS team_keks_org_gen ON team_keks(org, gen)",
    // Per-session content keys sealed under the HUB escrow public key (encryption-recipients Wave 5,
    // hub-assist escrow). One row per (owner_ns, name, kid); `wrapped_ck` is CIPHERTEXT only. Populated
    // only for sessions whose owning org opted into `escrow_mode = 'hub-assist'` and whose owner opted in.
    // Added additively like `team_keks` — no schema-version bump.
    "CREATE TABLE IF NOT EXISTS escrow_keys (\
       owner TEXT NOT NULL, name TEXT NOT NULL, kid BIGINT NOT NULL, wrapped_ck TEXT NOT NULL, \
       created TEXT NOT NULL DEFAULT '', PRIMARY KEY (owner, name, kid))",
];

/// The second-factor columns, added onto a `users` table that predates them. A **fresh** DB already
/// has them (they are in `DDL`); this back-fills older stores at boot. Idempotent by construction:
/// Postgres uses `ADD COLUMN IF NOT EXISTS`; SQLite (which has no such clause for ADD COLUMN) simply
/// ignores the "duplicate column" error, so re-running against an already-migrated store is a no-op.
/// Not version-gated on purpose — a fresh DB stamped at the current version still gets its `users`
/// table straight from `DDL`, so keying the back-fill off `schema_version` would double-add there.
const TOTP_COLUMNS: &[&str] = &[
    "totp_secret TEXT",
    "totp_enabled BIGINT NOT NULL DEFAULT 0",
    "totp_backup_codes TEXT NOT NULL DEFAULT '[]'",
];

/// The email-verification column added onto a `users` table that predates it (email-verification wave),
/// back-filled at boot exactly like [`TOTP_COLUMNS`]. A **fresh** DB already has it (it is in `DDL`);
/// this migrates older stores. Idempotent by construction: Postgres uses `ADD COLUMN IF NOT EXISTS`;
/// SQLite ignores the "duplicate column" error. With the DEFAULT 0, every pre-existing account reads
/// back UNVERIFIED — the safe anti-squatting default.
///
/// `disabled` (the admin soft-suspend flag) rides the SAME back-fill: a fresh DB gets it from `DDL`,
/// older stores get it here. DEFAULT 0 = active, so every pre-existing account stays able to log in.
const USER_COLUMNS: &[&str] = &["email_verified BIGINT NOT NULL DEFAULT 0", "disabled BIGINT NOT NULL DEFAULT 0"];

/// The org columns added onto an `orgs` table that predates them (encryption-recipients Wave 3),
/// back-filled at boot exactly like [`TOTP_COLUMNS`]. A **fresh** DB already has them (they are in
/// `DDL`); this migrates older stores. Idempotent by construction: Postgres uses `ADD COLUMN IF NOT
/// EXISTS`; SQLite ignores the "duplicate column" error, so re-running is a no-op.
const ORG_COLUMNS: &[&str] = &[
    "current_kek_gen BIGINT NOT NULL DEFAULT 0",
    // Wave 5 opt-in escape hatches. Defaults keep every wave-1..4 behavior byte-for-byte unchanged.
    "recovery_x25519 TEXT NOT NULL DEFAULT ''",
    "escrow_mode TEXT NOT NULL DEFAULT 'none'",
    // Member-create policy. DEFAULT 1 = members CAN create (the GitHub default), so every org that
    // predates this column stays permissive after the back-fill.
    "members_can_create BIGINT NOT NULL DEFAULT 1",
];

/// The `identity_keys` table, one DDL shared by both backends. The primary key is COMPOSITE
/// `(username, key_fpr)`: one account registers MANY device keys (SSH-keys style), one row per device. A
/// FRESH DB gets this shape straight from `DDL`; a store that still has the OLD single-key shape
/// (`PRIMARY KEY(username)`, no `key_fpr`/`label`) is rebuilt into this shape at boot by
/// `migrate_identity_keys_multikey`. epoch is BIGINT for the same strict-decode reason as every other
/// integer column; `label`/`email` default to '' so a back-filled legacy row is well-formed.
const IDENTITY_KEYS_DDL: &str = "CREATE TABLE IF NOT EXISTS identity_keys (\
   username TEXT NOT NULL, key_fpr TEXT NOT NULL, ed25519_pub TEXT NOT NULL, x25519_pub TEXT NOT NULL, \
   label TEXT NOT NULL DEFAULT '', epoch BIGINT NOT NULL DEFAULT 0, enroll_sig TEXT NOT NULL, \
   created TEXT NOT NULL DEFAULT '', revoked TEXT, email TEXT NOT NULL DEFAULT '', \
   PRIMARY KEY (username, key_fpr))";

/// The identity_keys columns added onto a registry that predates them (provenance signed-push
/// verification), back-filled at boot exactly like [`ORG_COLUMNS`]. A **fresh** DB already has them (in
/// `DDL`); this migrates older stores. Idempotent: Postgres uses `ADD COLUMN IF NOT EXISTS`; SQLite
/// ignores the "duplicate column" error. With the '' default, every wave-1 enrollment is unchanged.
/// Run BEFORE `migrate_identity_keys_multikey` so a legacy single-key table has its `email` column before
/// the multi-key rebuild copies rows out of it.
const IDENTITY_COLUMNS: &[&str] = &["email TEXT NOT NULL DEFAULT ''"];

/// The index on identity_keys.email, created AFTER `IDENTITY_COLUMNS` back-fills the column (not in
/// `DDL`): on an existing store the column does not exist until the back-fill runs, so an in-DDL index
/// would fail with "no such column: email" before the ALTER. Idempotent (IF NOT EXISTS).
const IDENTITY_EMAIL_INDEX: &str = "CREATE INDEX IF NOT EXISTS identity_keys_email ON identity_keys(email)";

/// Stamp the schema version idempotently. A single fixed row (id=1) plus `ON CONFLICT DO NOTHING`,
/// **not** read-MAX-then-INSERT: two Hubs booting against one fresh Postgres at the same moment would
/// both read 0 and both insert, leaving two rows. The upsert makes the second boot a no-op. Both
/// SQLite (≥3.24) and Postgres support this form.
const STAMP_VERSION: &str = "INSERT INTO schema_version (id, version) VALUES (1, 2) ON CONFLICT DO NOTHING";

/// The current schema version. Bumped to 2 for the (owner, name) scoping: a store stamped below this
/// runs `migrate_v2` at boot before serving.
const SCHEMA_VERSION: i64 = 2;

/// The one global advisory-lock key Postgres `update_*` transactions take (ASCII "AGIT_HUB" as an
/// i64). One key for all three tables reproduces the old single in-process Mutex: every read-modify-
/// write serializes against every other, so two concurrent snapshot-rewrites cannot clobber each
/// other and the reconcile TOCTOU (read + holder-lookup + write) stays one critical section.
const PG_ADVISORY_KEY: i64 = 0x4147_4954_5F48_5542;

fn err<E: std::error::Error + Send + Sync + 'static>(e: E) -> io::Error {
    io::Error::other(e)
}

fn is_pg_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("postgres://") || s.starts_with("postgresql://")
}

// ─────────────────────────── v1 → v2 migration (owner-scoping) ───────────────────────────
//
// Both backends share this shape; only the row types differ. The DDL above already `CREATE TABLE IF
// NOT EXISTS`es the NEW-shape tables, which is a no-op on a v1 store (the old tables already exist),
// so this rebuilds the old-shape `agents`/`mrs` into the composite-PK shape and re-homes null-owner
// rows to `_unclaimed`. Guarded by `schema_version < 2`, and structured to survive a crash: the table
// rebuild and the FS reorg are both idempotent, and the version is bumped ONLY after the filesystem
// has been reorganized, so an interrupted run re-runs cleanly instead of stranding repos.

/// The portable statements that rebuild `agents` into the composite-PK, owner-NOT-NULL shape. Runs on
/// an old-shape table (copies `owner` through `COALESCE(owner,'_unclaimed')`) and is idempotent on an
/// already-migrated one (owner is then already non-null, so the COALESCE is a no-op).
fn agents_rebuild_ddl() -> [String; 5] {
    [
        "DROP TABLE IF EXISTS agents_v2".to_string(),
        "CREATE TABLE agents_v2 (\
           owner TEXT NOT NULL, name TEXT NOT NULL, aid TEXT, \
           visibility TEXT NOT NULL DEFAULT 'private', lifecycle TEXT NOT NULL DEFAULT 'active', \
           description TEXT, forked_from TEXT, forked_from_aid TEXT, aid_conflict TEXT, \
           stars TEXT NOT NULL DEFAULT '[]', members TEXT NOT NULL DEFAULT '[]', \
           created TEXT NOT NULL DEFAULT '', PRIMARY KEY (owner, name))"
            .to_string(),
        format!(
            "INSERT INTO agents_v2 (owner, name, aid, visibility, lifecycle, description, forked_from, forked_from_aid, aid_conflict, stars, members, created) \
             SELECT COALESCE(owner, '{UNCLAIMED}'), name, aid, visibility, lifecycle, description, forked_from, forked_from_aid, aid_conflict, stars, members, created FROM agents"
        ),
        "DROP TABLE agents".to_string(),
        "ALTER TABLE agents_v2 RENAME TO agents".to_string(),
    ]
}

/// The map name → namespace segment used to backfill MR endpoints and re-home repos/blobs on disk.
/// Built from the still-v1 `agents` rows (name was unique then), defaulting a null owner to
/// `_unclaimed`.
fn seg_map(pairs: &[(String, Option<String>)]) -> HashMap<String, String> {
    pairs.iter().map(|(name, owner)| (name.clone(), owner.as_deref().map(owner_ns).unwrap_or(UNCLAIMED).to_string())).collect()
}

/// Patch one old MR's endpoints with the owner they belong under, resolved from the seg map (a target
/// or source whose agent has no row falls back to `_unclaimed`, matching the repo re-homing).
fn backfill_mr_owner(m: &mut Mr, map: &HashMap<String, String>) {
    let seg = |name: &str| map.get(name).cloned().unwrap_or_else(|| UNCLAIMED.to_string());
    m.target.owner = seg(&m.target.agent);
    m.source.owner = seg(&m.source.agent);
}

/// Move `root/<name>.git` → `root/<seg>/<name>.git` and `root/blobs/<name>` → `root/blobs/<seg>/<name>`
/// for every agent, using the resolved namespace segment (null owner → `_unclaimed`). Idempotent:
/// each move is skipped when the destination already exists, and after a full run no flat `<name>.git`
/// remains at the root. Orphan repos on disk with no agent row are re-homed to `_unclaimed`.
fn reorg_fs(root: &Path, map: &HashMap<String, String>) {
    // Repos: scan the flat `<name>.git` dirs still at the root (snapshot first so freshly-created
    // `<seg>/` dirs are never re-scanned). An entry with no agent row lands under `_unclaimed`.
    let mut repos: Vec<String> = vec![];
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let fname = e.file_name().to_string_lossy().into_owned();
            if let Some(name) = fname.strip_suffix(".git") {
                repos.push(name.to_string());
            }
        }
    }
    for name in repos {
        let seg = map.get(&name).map(String::as_str).unwrap_or(UNCLAIMED);
        let src = root.join(format!("{name}.git"));
        let dst_dir = root.join(seg);
        let dst = dst_dir.join(format!("{name}.git"));
        if dst.exists() {
            continue; // already re-homed
        }
        let _ = std::fs::create_dir_all(&dst_dir);
        let _ = std::fs::rename(&src, &dst);
    }
    // Blobs share the same collision hazard as repos but WITHOUT a `.git` suffix to tell a flat
    // `blobs/<name>/` apart from a `blobs/<seg>/` namespace container. Iterating and renaming in place
    // could re-capture a just-created `<seg>/` container as a later move source (when an agent's bare
    // name equals another agent's owner segment), silently stranding private blobs. So stage every flat
    // blob dir OUT of the top level first, then place each into `<seg>/<name>`: once phase 1 has cleared
    // the top level of flat name dirs, creating `<seg>/` containers can never re-capture one. Names in
    // the v1→v2 map are globally unique (name was the PK at v1), so there are no key collisions.
    let blobs = root.join("blobs");
    let staging = blobs.join(".migrating-v2");
    // Phase 1: flat `blobs/<name>` -> `blobs/.migrating-v2/<name>`.
    for name in map.keys() {
        let src = blobs.join(name);
        if !src.is_dir() {
            continue;
        }
        let staged = staging.join(name);
        if staged.exists() {
            continue; // a prior (crashed) run already staged it
        }
        let _ = std::fs::create_dir_all(&staging);
        let _ = std::fs::rename(&src, &staged);
    }
    // Phase 2: `blobs/.migrating-v2/<name>` -> `blobs/<seg>/<name>` (no flat name dirs remain up top).
    for (name, seg) in map {
        let staged = staging.join(name);
        if !staged.is_dir() {
            continue;
        }
        let dst_dir = blobs.join(seg);
        let dst = dst_dir.join(name);
        if dst.exists() {
            // Already re-homed by a prior run; drop the redundant staged copy (content-addressed, so
            // the reachable dst is authoritative).
            let _ = std::fs::remove_dir_all(&staged);
            continue;
        }
        let _ = std::fs::create_dir_all(&dst_dir);
        let _ = std::fs::rename(&staged, &dst);
    }
    let _ = std::fs::remove_dir(&staging); // empty on success
}

// ─────────────────────────── Store (enum facade) ───────────────────────────

/// The persistence handle. A concrete enum rather than `dyn Store`: the `update_*` methods are
/// generic over a closure (needed so the read-modify-write critical section keeps the ergonomic
/// closure API), and a generic method is not object-safe. Dispatch is by `match`; both inner pools
/// are `Clone`, so `Store` is `Clone` and threads cheaply into every request `Ctx`.
#[derive(Clone)]
pub enum Store {
    Sqlite(SqliteStore),
    Pg(PgStore),
}

impl Store {
    /// Open the configured backend and run migrations. `AGIT_HUB_DB` = a `postgres://` URL selects
    /// Postgres; anything else (unset, or a non-URL value) selects the SQLite `hub.db` under `<root>`.
    ///
    /// Async: the caller supplies the runtime (the axum server awaits it during boot; the CLI wraps
    /// it in a short-lived `block_on`).
    pub async fn open(root: &Path) -> io::Result<Store> {
        ensure_root(root)?;
        let store = match std::env::var("AGIT_HUB_DB") {
            Ok(url) if is_pg_url(&url) => Store::Pg(PgStore::connect(&url, root.to_path_buf())?),
            _ => Store::Sqlite(SqliteStore::connect(root.to_path_buf())?),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Open the SQLite backend under `<root>` unconditionally, ignoring `AGIT_HUB_DB`. Used by tests
    /// (and any caller that wants the zero-config file backend regardless of the environment).
    pub async fn open_sqlite(root: &Path) -> io::Result<Store> {
        ensure_root(root)?;
        let store = Store::Sqlite(SqliteStore::connect(root.to_path_buf())?);
        store.migrate().await?;
        Ok(store)
    }

    /// Open the configured backend WITHOUT running migrations — a read-only handle for a short-lived,
    /// out-of-process reader (the pre-receive provenance check) that must not take the schema write locks
    /// the serving process already owns. Honors `AGIT_HUB_DB` exactly like [`Store::open`]. The reader
    /// only ever runs SELECTs; on SQLite those are WAL reads that never block the writer, so a push's
    /// provenance lookup cannot stall or be stalled by the live hub. A registry column the serving hub has
    /// not yet added simply makes a SELECT error, which every caller treats as "no attribution".
    pub async fn open_readonly(root: &Path) -> io::Result<Store> {
        ensure_root(root)?;
        Ok(match std::env::var("AGIT_HUB_DB") {
            Ok(url) if is_pg_url(&url) => Store::Pg(PgStore::connect(&url, root.to_path_buf())?),
            _ => Store::Sqlite(SqliteStore::connect(root.to_path_buf())?),
        })
    }

    pub fn root(&self) -> &Path {
        match self {
            Store::Sqlite(s) => &s.root,
            Store::Pg(s) => &s.root,
        }
    }

    /// One-word backend name for status banners and admin messages (`sqlite` / `postgres`).
    pub fn backend(&self) -> &'static str {
        match self {
            Store::Sqlite(_) => "sqlite",
            Store::Pg(_) => "postgres",
        }
    }

    /// Human-readable description of where credentials actually land, for CLI success messages and the
    /// startup banner. The SQLite backend writes the `hub.db` file under `<root>` (0600); the Postgres
    /// backend writes to the configured database — never a `users.json` file in either case.
    pub fn describe(&self) -> String {
        match self {
            Store::Sqlite(_) => format!("SQLite {} (0600)", self.root().join("hub.db").display()),
            Store::Pg(_) => "Postgres (AGIT_HUB_DB)".to_string(),
        }
    }

    /// Create tables (idempotent) and stamp schema_version. Run once at boot; forces the lazy pool
    /// to establish its first connection, so a bad `AGIT_HUB_DB` surfaces here with a clear error.
    pub async fn migrate(&self) -> io::Result<()> {
        match self {
            Store::Sqlite(s) => s.migrate().await,
            Store::Pg(s) => s.migrate().await,
        }
    }

    /// Take a CONSISTENT snapshot of the SQLite database into `dest` via `VACUUM INTO` — the online,
    /// transaction-safe copy path that folds any `-wal`/`-shm` state into a single self-contained file
    /// (never a raw `cp` of a live WAL db). `dest` must not already exist (SQLite refuses to overwrite).
    /// Errs on a Postgres store: that backend is dumped with `pg_dump`, not this.
    pub async fn backup_sqlite_to(&self, dest: &Path) -> io::Result<()> {
        match self {
            Store::Sqlite(s) => s.backup_to(dest).await,
            Store::Pg(_) => Err(io::Error::other("backup_sqlite_to is only valid for the SQLite backend")),
        }
    }

    /// Live source row-counts (users, agents, tokens) read through the store's own pool — which sees
    /// committed WAL frames — so a backup can record them and later prove the snapshot captured them.
    /// Only meaningful on SQLite (the backup snapshot path); errs on Postgres.
    pub async fn row_counts(&self) -> io::Result<RowCounts> {
        match self {
            Store::Sqlite(s) => count_on_pool(&s.pool).await,
            Store::Pg(_) => Err(io::Error::other("row_counts is only valid for the SQLite backend")),
        }
    }

    // ── users ──

    pub async fn users(&self) -> Vec<User> {
        match self {
            Store::Sqlite(s) => s.users().await,
            Store::Pg(s) => s.users().await,
        }
    }

    pub async fn user(&self, username: &str) -> Option<User> {
        let u = normalize_username(username);
        self.users().await.into_iter().find(|x| x.username == u)
    }

    /// Add a user. Err (AlreadyExists) if the same name (after normalizing) already exists.
    pub async fn add_user(&self, user: User) -> io::Result<()> {
        match self {
            Store::Sqlite(s) => s.add_user(user).await,
            Store::Pg(s) => s.add_user(user).await,
        }
    }

    /// Read-modify-write the users table in one transaction — the same serialization discipline the
    /// other `update_*` methods use (SQLite async write mutex + tracked tx; Postgres advisory-lock
    /// tx). The closure runs synchronously between the read and the atomic rewrite and must not call
    /// back into `Store`.
    pub async fn update_users<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<User>) -> R,
    {
        match self {
            Store::Sqlite(s) => s.update_users(f).await,
            Store::Pg(s) => s.update_users(f).await,
        }
    }

    /// Set a user's password material (hash + salt + kdf), leaving every other field untouched.
    /// Returns `Ok(true)` if the user existed and was updated, `Ok(false)` if no such user. The
    /// username is normalized like every other lookup, so "Alice" and "alice" address one account.
    pub async fn set_password(&self, username: &str, pw_hash: &str, salt: &str, kdf: &str) -> io::Result<bool> {
        let u = normalize_username(username);
        self.update_users(|users| match users.iter_mut().find(|x| x.username == u) {
            Some(user) => {
                user.pw_hash = pw_hash.to_string();
                user.salt = salt.to_string();
                user.kdf = kdf.to_string();
                true
            }
            None => false,
        })
        .await
    }

    /// Set a user's `email_verified` flag, leaving every other field untouched. `Ok(true)` if the user
    /// existed, `Ok(false)` if not. Username is normalized like every other lookup. This is the one write
    /// that flips the anti-squatting gate — driven by consuming a verification token, an authenticated
    /// re-enroll that CHANGES the email (reset to false), or an admin force-verify (set to true).
    pub async fn set_email_verified(&self, username: &str, verified: bool) -> io::Result<bool> {
        let u = normalize_username(username);
        self.update_users(move |users| match users.iter_mut().find(|x| x.username == u) {
            Some(user) => {
                user.email_verified = verified;
                true
            }
            None => false,
        })
        .await
    }

    /// Set a user's `disabled` flag (the admin soft-suspend), leaving every other field untouched.
    /// `Ok(true)` if the user existed, `Ok(false)` if not. Username is normalized like every other
    /// lookup. A disabled account cannot log in (`api_login` returns a 403 AFTER verifying the password,
    /// so this is never a password oracle); enabling it restores login. The caller is responsible for
    /// revoking the target's live sessions on DISABLE — this only flips the persisted flag.
    pub async fn set_user_disabled(&self, username: &str, disabled: bool) -> io::Result<bool> {
        let u = normalize_username(username);
        self.update_users(move |users| match users.iter_mut().find(|x| x.username == u) {
            Some(user) => {
                user.disabled = disabled;
                true
            }
            None => false,
        })
        .await
    }

    // ── email-verification tokens ──
    //
    // A single-use, expiring token store (targeted insert/delete, not the whole-table snapshot), sharing
    // the exact write critical section every other writer uses. The token is a CSPRNG capability handed to
    // the operator to forward to the address being proven; consuming it deletes the row (single-use) and
    // yields the (username, email) to mark verified. The token is NEVER returned to an unauthenticated
    // registrant — that would defeat verification.

    /// Mint a fresh verification token for `(username, email)` that expires after `ttl`, and return it.
    /// Username + email are normalized so the consumed pair matches the account and the by-email lookup.
    pub async fn mint_email_token(&self, username: &str, email: &str, ttl: Duration) -> io::Result<String> {
        let username = normalize_username(username);
        let email = normalize_email(email);
        // A random capability, not a secret to hash: 32 CSPRNG bytes as hex, prefixed for legibility.
        let token = format!("evt_{}", super::kdf::gen_secret()?);
        let expires = (chrono::Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::hours(24)))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let row = EmailVerifyToken { token: token.clone(), username, email, expires, created: now_iso() };
        match self {
            Store::Sqlite(s) => s.mint_email_token(&row).await?,
            Store::Pg(s) => s.mint_email_token(&row).await?,
        }
        Ok(token)
    }

    /// Consume a verification token: delete it (single-use) and return the `(username, email)` it proved,
    /// or `None` for an unknown or expired token. The delete happens even for an expired token (cleanup);
    /// an expired one still yields `None`, so a stale link can never mark an account verified.
    pub async fn consume_email_token(&self, token: &str) -> Option<(String, String)> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        match self {
            Store::Sqlite(s) => s.consume_email_token(token).await,
            Store::Pg(s) => s.consume_email_token(token).await,
        }
    }

    // ── password-reset tokens ──
    //
    // A SEPARATE single-use, expiring token space from the email-verification tokens above (distinct
    // table + `prt_` prefix). The two never cross: `consume_password_reset_token` only reads
    // `password_reset_tokens`, so an `evt_` verification token is not present there and can never reset a
    // password, and `consume_email_token` only reads `email_verify_tokens`, so a `prt_` reset token can
    // never mark an email verified. The token is a CSPRNG capability the operator forwards to a
    // locked-out user; consuming it authorizes a password set WITHOUT the old password.

    /// Mint a fresh password-reset token for `username` that expires after `ttl`, and return it. Username
    /// is normalized like every other lookup so the consumed row addresses the same account.
    pub async fn mint_password_reset_token(&self, username: &str, ttl: Duration) -> io::Result<String> {
        let username = normalize_username(username);
        // A random capability, not a secret to hash: 32 CSPRNG bytes as hex, prefixed for legibility and
        // to keep the reset-token space textually distinct from the `evt_` verification space.
        let token = format!("prt_{}", super::kdf::gen_secret()?);
        let expires = (chrono::Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::hours(1)))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let row = PasswordResetToken { token: token.clone(), username, expires, created: now_iso() };
        match self {
            Store::Sqlite(s) => s.mint_password_reset_token(&row).await?,
            Store::Pg(s) => s.mint_password_reset_token(&row).await?,
        }
        Ok(token)
    }

    /// Consume a password-reset token: delete it (single-use) and return the `username` it authorizes, or
    /// `None` for an unknown or expired token. The delete happens even for an expired token (cleanup); an
    /// expired one still yields `None`, so a stale link can never reset a password.
    pub async fn consume_password_reset_token(&self, token: &str) -> Option<String> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        match self {
            Store::Sqlite(s) => s.consume_password_reset_token(token).await,
            Store::Pg(s) => s.consume_password_reset_token(token).await,
        }
    }

    /// How many live (not-yet-consumed) password-reset tokens exist. An observability/test seam: the
    /// anti-enumeration request path mints for a real account and NOTHING for an unknown one, and this is
    /// the honest way to assert that without exposing the tokens themselves.
    pub async fn password_reset_token_count(&self) -> usize {
        match self {
            Store::Sqlite(s) => s.password_reset_token_count().await,
            Store::Pg(s) => s.password_reset_token_count().await,
        }
    }

    // ── agent metadata ──

    pub async fn agents(&self) -> Vec<AgentMeta> {
        match self {
            Store::Sqlite(s) => s.agents().await,
            Store::Pg(s) => s.agents().await,
        }
    }

    /// Resolve one agent by its namespace segment + name. `seg` is the URL owner segment (see
    /// [`owner_ns`]): user `alice` → `alice`, org `org:acme` → `acme`. At most one row matches, because
    /// a username and an org name may never share a bare string (the unified-account rule).
    pub async fn agent_scoped(&self, seg: &str, name: &str) -> Option<AgentMeta> {
        self.agents().await.into_iter().find(|a| a.matches(seg, name))
    }

    /// Resolve an identity to the agent currently wearing it. **The aid is the identity, the name is
    /// only a label** — this is what lets a `.agit.toml` pinned to an aid survive a rename.
    ///
    /// Only ever one answer: `super::identity::reconcile` refuses to cache an aid a second agent
    /// already holds, so the first match is the only match.
    pub async fn agent_by_aid(&self, aid: &str) -> Option<AgentMeta> {
        if aid.is_empty() {
            return None;
        }
        self.agents().await.into_iter().find(|a| a.aid.as_deref() == Some(aid))
    }

    /// `<name>.git` exists on disk but there is no record of it → unowned and private.
    /// **Fail-safe**: a migrated-in old repo does not become world-pullable just because there is no
    /// record of it.
    pub async fn agent_or_unowned(&self, seg: &str, name: &str) -> AgentMeta {
        // Built through `new` rather than field-by-field: a field added later must not be able to
        // acquire a laxer default here than a real agent gets. The fail-safe carries owner:None (so it
        // is byte-for-byte the SAME whether the owner account is missing, the agent is missing, or the
        // agent exists-but-invisible) — that identity is what keeps `gate` from leaking existence.
        self.agent_scoped(seg, name).await.unwrap_or_else(|| AgentMeta {
            created: String::new(),
            ..AgentMeta::new(name, None, Visibility::Private)
        })
    }

    /// Read-modify-write the agents table in one transaction. The closure's return value is passed
    /// straight back out. The closure runs synchronously between the read and the atomic rewrite; it
    /// must not call back into `Store` (that would re-enter the transaction).
    pub async fn update_agents<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<AgentMeta>) -> R,
    {
        match self {
            Store::Sqlite(s) => s.update_agents(f).await,
            Store::Pg(s) => s.update_agents(f).await,
        }
    }

    // ── merge requests ──

    pub async fn mrs(&self) -> Vec<Mr> {
        match self {
            Store::Sqlite(s) => s.mrs().await,
            Store::Pg(s) => s.mrs().await,
        }
    }

    /// Every MR whose **target** is this agent `(seg, name)`, oldest first (the id order MRs were
    /// opened in).
    pub async fn mrs_for(&self, seg: &str, name: &str) -> Vec<Mr> {
        let mut v: Vec<Mr> =
            self.mrs().await.into_iter().filter(|m| m.target.owner == seg && m.target.agent == name).collect();
        v.sort_by_key(|m| m.id);
        v
    }

    pub async fn update_mrs<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<Mr>) -> R,
    {
        match self {
            Store::Sqlite(s) => s.update_mrs(f).await,
            Store::Pg(s) => s.update_mrs(f).await,
        }
    }

    /// Carry an agent's MRs across a rename. The **aid does not move** — it never changes — but the
    /// name on each endpoint is a label, and a stale label is a dead link and a lie about who the MR
    /// is between.
    /// Carry an agent's MRs across a rename within one namespace `seg`. Only the label moves — the aid
    /// never changes — so endpoints in the same namespace whose name was `from` become `to`.
    pub async fn rename_in_mrs(&self, seg: &str, from: &str, to: &str) -> io::Result<()> {
        self.update_mrs(|mrs| {
            for m in mrs.iter_mut() {
                if m.target.owner == seg && m.target.agent == from {
                    m.target.agent = to.to_string();
                }
                if m.source.owner == seg && m.source.agent == from {
                    m.source.agent = to.to_string();
                }
            }
        })
        .await
    }

    // ── tokens ──

    pub async fn tokens(&self) -> Vec<TokenRec> {
        match self {
            Store::Sqlite(s) => s.tokens().await,
            Store::Pg(s) => s.tokens().await,
        }
    }

    pub async fn update_tokens<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<TokenRec>) -> R,
    {
        match self {
            Store::Sqlite(s) => s.update_tokens(f).await,
            Store::Pg(s) => s.update_tokens(f).await,
        }
    }

    // ── organizations ──

    pub async fn orgs(&self) -> Vec<Org> {
        match self {
            Store::Sqlite(s) => s.orgs().await,
            Store::Pg(s) => s.orgs().await,
        }
    }

    /// Look one org up by name. Normalizes like `user()` — org names live in the same lowercase
    /// namespace as usernames.
    pub async fn org(&self, name: &str) -> Option<Org> {
        let n = normalize_username(name);
        self.orgs().await.into_iter().find(|o| o.name == n)
    }

    pub async fn update_orgs<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<Org>) -> R,
    {
        match self {
            Store::Sqlite(s) => s.update_orgs(f).await,
            Store::Pg(s) => s.update_orgs(f).await,
        }
    }

    // ── org invitations ──

    pub async fn invitations(&self) -> Vec<Invitation> {
        match self {
            Store::Sqlite(s) => s.invitations().await,
            Store::Pg(s) => s.invitations().await,
        }
    }

    pub async fn update_invitations<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<Invitation>) -> R,
    {
        match self {
            Store::Sqlite(s) => s.update_invitations(f).await,
            Store::Pg(s) => s.update_invitations(f).await,
        }
    }

    // ── identity registry (MANY device keys per account) ──
    //
    // Targeted per-key upsert/gets (not the whole-table snapshot the other tables use): the registry is
    // keyed by the composite `(username, key_fpr)` and can grow large, so a per-enroll table rewrite would
    // be wasteful. Each write runs in the exact same critical section every other writer uses (the SQLite
    // async write mutex / the Postgres advisory-lock transaction), and the monotonic per-key epoch check
    // happens inside that section so it is race-free.

    /// The account's PRIMARY device key — its latest non-revoked key (used by `GET /api/me`, the enroll
    /// email-change check, and encryption recipient wrapping, which stays single-effective-key this wave).
    /// `None` when the account has enrolled no key, or has revoked them all. Username is normalized.
    pub async fn get_primary_identity_key(&self, username: &str) -> Option<IdentityKey> {
        let u = normalize_username(username);
        match self {
            Store::Sqlite(s) => s.get_primary_identity_key(&u).await,
            Store::Pg(s) => s.get_primary_identity_key(&u).await,
        }
    }

    /// Back-compat alias for the account's primary device key. Kept so call sites that meant "the
    /// account's key" resolve to its latest non-revoked device key under the multi-key registry.
    pub async fn get_identity_key(&self, username: &str) -> Option<IdentityKey> {
        self.get_primary_identity_key(username).await
    }

    /// Every NON-REVOKED device key of one account, latest first. Empty for an account that never enrolled
    /// or has revoked them all. Username is normalized.
    pub async fn list_identity_keys(&self, username: &str) -> Vec<IdentityKey> {
        let u = normalize_username(username);
        match self {
            Store::Sqlite(s) => s.list_identity_keys(&u).await,
            Store::Pg(s) => s.list_identity_keys(&u).await,
        }
    }

    /// The primary device key for a batch of users. Unknown users are simply omitted (non-disclosing) —
    /// the result is not padded and order is not guaranteed. Duplicate/blank names are harmless.
    pub async fn get_identity_keys(&self, usernames: &[String]) -> Vec<IdentityKey> {
        let names: Vec<String> = usernames.iter().map(|u| normalize_username(u)).filter(|u| !u.is_empty()).collect();
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            if let Some(k) = self.get_primary_identity_key(&n).await {
                out.push(k);
            }
        }
        out
    }

    /// ADD (or idempotently re-enroll) the caller's own device key, keyed by `(username, key_fpr)`. Adding
    /// a key NEVER overwrites the caller's OTHER device keys — that is the whole point of the multi-key
    /// registry. Re-enrolling the SAME device key (same `key_fpr`) refreshes its x25519/label/email and
    /// requires a strictly higher epoch (monotonic PER KEY, no rollback); `created` is preserved and any
    /// prior `revoked` tombstone is cleared, so re-enrolling un-revokes. The signature/possession check is
    /// the API layer's job; this method owns only the atomic monotonic write. `key_fpr` is filled from
    /// `ed25519_pub` when the caller leaves it blank.
    pub async fn add_identity_key(&self, mut row: IdentityKey) -> io::Result<EnrollOutcome> {
        row.username = normalize_username(&row.username);
        // The committer email is the attribution key; normalize it the same way (trim + lowercase) so a
        // by-email lookup matches regardless of the case git recorded the committer under.
        row.email = normalize_email(&row.email);
        if row.key_fpr.trim().is_empty() {
            row.key_fpr = ed25519_fingerprint(&row.ed25519_pub);
        }
        match self {
            Store::Sqlite(s) => s.add_identity_key(row).await,
            Store::Pg(s) => s.add_identity_key(row).await,
        }
    }

    /// Revoke ONE of an account's device keys (tombstone it). `Ok(true)` when a matching, not-already-
    /// revoked key was tombstoned; `Ok(false)` when the account has no such live key. Caller-only: the API
    /// passes the AUTHENTICATED caller as `username`, so a non-owner can never revoke someone else's key
    /// (a `(username, key_fpr)` naming another account's key simply matches no row of the caller's).
    pub async fn revoke_identity_key(&self, username: &str, key_fpr: &str) -> io::Result<bool> {
        let u = normalize_username(username);
        let fpr = key_fpr.trim().to_string();
        if fpr.is_empty() {
            return Ok(false);
        }
        match self {
            Store::Sqlite(s) => s.revoke_identity_key(&u, &fpr).await,
            Store::Pg(s) => s.revoke_identity_key(&u, &fpr).await,
        }
    }

    /// ALL non-revoked device keys of the VERIFIED account owning `email`, latest first — the server-side
    /// lookup behind provenance match-ANY and the `by-email` endpoint. Empty when the email maps to no
    /// account, to an unverified account, or (ambiguously) to 2+ accounts.
    ///
    /// Email is normalized (trim + lowercase) before matching. A blank email never matches (the "unset"
    /// sentinel). A revoked key does not count toward ownership resolution — an account whose only key for
    /// this email is revoked does not own it here.
    ///
    /// **The email-squatting defense.** The registry `email` is SELF-ASSERTED at enroll time (not covered
    /// by `enroll_sig`), so anyone can enroll a key claiming `ceo@corp.com`. Attribution is therefore gated
    /// on VERIFICATION: the owning account's `users.email_verified` must be `true`, or this returns empty.
    /// An unverified (possibly squatted) email resolves to NO keys, so provenance verification degrades that
    /// session to `SignedUnregistered` instead of minting a false `VerifiedAs`. The ambiguity rule applies
    /// first: an email on 2+ accounts is not attributable regardless of any account's verified state.
    pub async fn get_identity_keys_by_email(&self, email: &str) -> Vec<IdentityKey> {
        let e = normalize_email(email);
        if e.is_empty() {
            return vec![];
        }
        // Resolve the email to EXACTLY ONE owning account (ambiguity → nothing). Only non-revoked keys
        // count toward ownership.
        let owner = match self {
            Store::Sqlite(s) => s.identity_email_owner(&e).await,
            Store::Pg(s) => s.identity_email_owner(&e).await,
        };
        let Some(owner) = owner else {
            return vec![];
        };
        // Anti-squatting gate: attribute ONLY when the owning account has VERIFIED this email.
        match self.user(&owner).await {
            Some(u) if u.email_verified => {}
            _ => return vec![],
        }
        // Return the whole SET of the owner's non-revoked device keys — a session signed by ANY of them is
        // this person's.
        self.list_identity_keys(&owner).await
    }

    // ── team-KEK envelopes (encryption-recipients Wave 3) ──
    //
    // Targeted upserts/gets, keyed by (org, gen, recipient), sharing the exact write critical section
    // every other writer uses (the SQLite async write mutex / the Postgres advisory-lock transaction).
    // The hub only ever stores CIPHERTEXT `wrapped_kek` — the client computes every X25519 seal.

    /// Upsert a batch of TK_gen envelopes for `org` (one row per recipient). Idempotent on the
    /// (org, gen, recipient) PK: republishing overwrites the ciphertext. `org` is normalized to the
    /// canonical name namespace, exactly like [`Store::org`].
    pub async fn upsert_team_kek_envelopes(&self, org: &str, gen: i64, rows: &[TeamKekEnvelope]) -> io::Result<()> {
        let org = normalize_username(org);
        match self {
            Store::Sqlite(s) => s.upsert_team_kek_envelopes(&org, gen, rows).await,
            Store::Pg(s) => s.upsert_team_kek_envelopes(&org, gen, rows).await,
        }
    }

    /// One recipient's own envelope of TK at `gen`, or `None` if none exists. Callers must scope this
    /// to the AUTHENTICATED recipient — the store returns whatever row is asked for; the API layer owns
    /// the "you may fetch only your own" rule.
    pub async fn get_team_kek_envelope(&self, org: &str, gen: i64, recipient: &str) -> Option<TeamKekEnvelope> {
        let org = normalize_username(org);
        let recipient = normalize_username(recipient);
        match self {
            Store::Sqlite(s) => s.get_team_kek_envelope(&org, gen, &recipient).await,
            Store::Pg(s) => s.get_team_kek_envelope(&org, gen, &recipient).await,
        }
    }

    /// The distinct TK generations `org` has any envelopes for, ascending. Empty if the org has never
    /// published a TK.
    pub async fn list_team_kek_gens(&self, org: &str) -> Vec<i64> {
        let org = normalize_username(org);
        match self {
            Store::Sqlite(s) => s.list_team_kek_gens(&org).await,
            Store::Pg(s) => s.list_team_kek_gens(&org).await,
        }
    }

    /// The org's active Team-KEK generation (0 = never minted, or unknown org).
    pub async fn get_current_kek_gen(&self, org: &str) -> i64 {
        self.org(org).await.map(|o| o.current_kek_gen).unwrap_or(0)
    }

    /// Set the org's active Team-KEK generation. Runs in the same write critical section as every other
    /// org write; a missing org is a no-op (the caller has already resolved it).
    pub async fn set_current_kek_gen(&self, org: &str, gen: i64) -> io::Result<()> {
        let org = normalize_username(org);
        match self {
            Store::Sqlite(s) => s.set_current_kek_gen(&org, gen).await,
            Store::Pg(s) => s.set_current_kek_gen(&org, gen).await,
        }
    }

    /// Upsert one session's hub-escrowed content key (encryption-recipients Wave 5). Idempotent on the
    /// (owner, name, kid) PK: re-escrowing the same kid overwrites the ciphertext. `wrapped_ck` is CK
    /// sealed to the hub escrow public key — ciphertext only.
    pub async fn upsert_escrow_key(&self, key: &EscrowKey) -> io::Result<()> {
        match self {
            Store::Sqlite(s) => s.upsert_escrow_key(key).await,
            Store::Pg(s) => s.upsert_escrow_key(key).await,
        }
    }

    /// Every hub-escrowed content-key row for one session, ascending by kid. Empty if the session has no
    /// escrowed keys (the default — escrow is opt-in).
    pub async fn get_escrow_keys(&self, owner: &str, name: &str) -> Vec<EscrowKey> {
        match self {
            Store::Sqlite(s) => s.get_escrow_keys(owner, name).await,
            Store::Pg(s) => s.get_escrow_keys(owner, name).await,
        }
    }
}

// ─────────────────────────── SQLite backend ───────────────────────────

#[derive(Clone)]
pub struct SqliteStore {
    pool: sqlx::SqlitePool,
    root: PathBuf,
    /// One writer at a time. An **async** mutex (safe to hold across `.await`, unlike `std::sync::Mutex`)
    /// held for the whole read-modify-write, reproducing the old single global in-process LOCK. Shared
    /// across `Store` clones via `Arc`. With it in place a plain tracked `pool.begin()` is enough — no
    /// raw `BEGIN IMMEDIATE`, so there is no read-then-upgrade SQLITE_BUSY race, and (crucially) sqlx
    /// tracks the transaction and auto-rolls it back if the handler future is dropped mid-write. A raw
    /// `BEGIN` is invisible to sqlx (transaction_depth stays 0), so on cancellation the connection would
    /// return to the pool still inside the write transaction and wedge every future writer.
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl SqliteStore {
    /// Build the lazy pool over `<root>/hub.db`. WAL + a busy timeout still matter for the rare
    /// cross-process writer (a `docker exec … token add` while the server runs): SQLite is
    /// single-writer, so the second waits for the lock instead of erroring "database is locked".
    fn connect(root: PathBuf) -> io::Result<SqliteStore> {
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
        let opts = SqliteConnectOptions::new()
            .filename(root.join("hub.db"))
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(5))
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().max_connections(5).connect_lazy_with(opts);
        Ok(SqliteStore { pool, root, write_lock: Arc::new(tokio::sync::Mutex::new(())) })
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("hub.db")
    }

    /// `VACUUM INTO <dest>`: an online, consistent copy of the whole database (schema + rows) into a
    /// single file, with any WAL frames folded in. Runs on a normal pooled connection, so it never
    /// takes the write lock or blocks readers. The path is bound as a value (SQLite has no placeholder
    /// for it in this statement, so it is single-quote-escaped and inlined). The destination file is
    /// then tightened to 0600 — it is a verbatim copy of the credential-digest store.
    async fn backup_to(&self, dest: &Path) -> io::Result<()> {
        let path = dest.to_str().ok_or_else(|| io::Error::other("backup destination path is not valid UTF-8"))?;
        // Fold every committed WAL frame into the main db file BEFORE the snapshot. On a LIVE hub the
        // main `hub.db` is frozen at the last checkpoint baseline while freshly committed data lives in
        // the `-wal` held open by the running server; under concurrent writers `VACUUM INTO`
        // intermittently reads ONLY that baseline and drops the committed WAL frames -> a near-empty
        // backup that still succeeds. `wal_checkpoint(TRUNCATE)` forces all committed frames into the
        // main db (and resets the WAL) so `VACUUM INTO` below cannot miss them. Best-effort on the
        // return code (a concurrent reader can keep TRUNCATE from resetting the file), but the frame
        // transfer that matters still happens; the caller's row-count verify is the hard backstop.
        let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&self.pool).await;
        // Escape single quotes for the inlined SQL string literal.
        let escaped = path.replace('\'', "''");
        sqlx::query(&format!("VACUUM INTO '{escaped}'")).execute(&self.pool).await.map_err(err)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    async fn migrate(&self) -> io::Result<()> {
        for stmt in DDL {
            sqlx::query(stmt).execute(&self.pool).await.map_err(err)?;
        }
        sqlx::query(STAMP_VERSION).execute(&self.pool).await.map_err(err)?;
        self.migrate_v2().await?;
        // Back-fill the 2FA columns onto stores created before they existed. On a fresh DB the DDL
        // above already created them, so the ADD COLUMN fails with "duplicate column" — expected and
        // ignored (SQLite has no ADD COLUMN IF NOT EXISTS).
        for &col in TOTP_COLUMNS {
            let _ = sqlx::query(&format!("ALTER TABLE users ADD COLUMN {col}")).execute(&self.pool).await;
        }
        // Back-fill the email_verified column onto stores predating email verification (no-op /
        // "duplicate column" on a fresh DB, expected and ignored, exactly like the TOTP columns).
        for &col in USER_COLUMNS {
            let _ = sqlx::query(&format!("ALTER TABLE users ADD COLUMN {col}")).execute(&self.pool).await;
        }
        // Back-fill the Wave-3 org columns onto stores predating them (no-op on a fresh DB, where the DDL
        // above already added them — SQLite then errors "duplicate column", which is expected/ignored).
        for &col in ORG_COLUMNS {
            let _ = sqlx::query(&format!("ALTER TABLE orgs ADD COLUMN {col}")).execute(&self.pool).await;
        }
        // Back-fill the identity_keys email column onto registries predating provenance verification
        // (no-op / "duplicate column" on a fresh DB, expected and ignored, exactly like the columns above).
        // Runs BEFORE the multi-key rebuild so a legacy single-key table has `email` before its rows are
        // copied across.
        for &col in IDENTITY_COLUMNS {
            let _ = sqlx::query(&format!("ALTER TABLE identity_keys ADD COLUMN {col}")).execute(&self.pool).await;
        }
        // Reshape a legacy single-key identity_keys (PRIMARY KEY(username)) into the multi-key composite
        // shape, back-filling key_fpr + label='default' for each existing row. No-op once already reshaped.
        self.migrate_identity_keys_multikey().await?;
        // Now that `email` is guaranteed to exist (fresh via DDL, or just back-filled above) on the FINAL
        // (reshaped) table, its index is safe to create. Doing this in DDL would fail "no such column".
        sqlx::query(IDENTITY_EMAIL_INDEX).execute(&self.pool).await.map_err(err)?;
        // create_if_missing may not honor the mode; tighten hub.db AND its WAL sidecars to 0600, the
        // same guarantee write_secret_atomic gave the old JSON files. The DDL/stamp above already
        // wrote, so in WAL mode the -wal/-shm sidecars now exist and get locked down too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p600 = std::fs::Permissions::from_mode(0o600);
            let _ = std::fs::set_permissions(self.db_path(), p600.clone());
            for ext in ["hub.db-wal", "hub.db-shm"] {
                let side = self.root.join(ext);
                if side.exists() {
                    let _ = std::fs::set_permissions(&side, p600.clone());
                }
            }
        }
        Ok(())
    }

    /// Reshape a legacy single-key `identity_keys` (`PRIMARY KEY(username)`, no `key_fpr`/`label`) into the
    /// multi-key composite shape (`PRIMARY KEY(username, key_fpr)`). SQLite cannot change a primary key in
    /// place, so this rebuilds the table: rename the old one aside, create the new shape via
    /// `IDENTITY_KEYS_DDL`, copy each old row across with `key_fpr` derived from its `ed25519_pub` and
    /// `label = 'default'` (one device key per old row), then drop the old table. Detection is by the
    /// presence of the `key_fpr` column — a fresh DB (already the new shape) and an already-reshaped store
    /// both no-op. The whole rebuild runs under the write lock, so a concurrent boot cannot interleave.
    async fn migrate_identity_keys_multikey(&self) -> io::Result<()> {
        // A `SELECT key_fpr` that errors means the column is absent → legacy shape. Ok(_) (even 0 rows)
        // means the new shape is already present.
        if sqlx::query("SELECT key_fpr FROM identity_keys LIMIT 1").fetch_optional(&self.pool).await.is_ok() {
            return Ok(());
        }
        let _guard = self.write_lock.lock().await;
        // Re-check under the lock (another booting process may have just reshaped it).
        if sqlx::query("SELECT key_fpr FROM identity_keys LIMIT 1").fetch_optional(&self.pool).await.is_ok() {
            return Ok(());
        }
        // The legacy table has `email` (the IDENTITY_COLUMNS back-fill ran just before us). Read every row.
        let olds = sqlx::query(
            "SELECT username, ed25519_pub, x25519_pub, epoch, enroll_sig, created, revoked, email FROM identity_keys",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(err)?;
        let mut tx = self.pool.begin().await.map_err(err)?;
        // Drop the pre-existing email index (it is bound to the old table name; recreating it later on the
        // new table would otherwise collide) and rename the legacy table aside.
        sqlx::query("DROP INDEX IF EXISTS identity_keys_email").execute(&mut *tx).await.map_err(err)?;
        sqlx::query("ALTER TABLE identity_keys RENAME TO identity_keys_legacy").execute(&mut *tx).await.map_err(err)?;
        sqlx::query(IDENTITY_KEYS_DDL).execute(&mut *tx).await.map_err(err)?;
        for r in &olds {
            let ed = r.text("ed25519_pub");
            let fpr = ed25519_fingerprint(&ed);
            sqlx::query(
                "INSERT INTO identity_keys (username, key_fpr, ed25519_pub, x25519_pub, label, epoch, enroll_sig, created, revoked, email) \
                 VALUES (?, ?, ?, ?, 'default', ?, ?, ?, ?, ?)",
            )
            .bind(r.text("username"))
            .bind(&fpr)
            .bind(&ed)
            .bind(r.text("x25519_pub"))
            .bind(r.int("epoch"))
            .bind(r.text("enroll_sig"))
            .bind(r.text("created"))
            .bind(r.opt("revoked"))
            .bind(r.text("email"))
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        }
        sqlx::query("DROP TABLE identity_keys_legacy").execute(&mut *tx).await.map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    /// v1 → v2 owner-scoping migration for SQLite. See the module-level migration notes. No-op once
    /// `schema_version >= 2`.
    async fn migrate_v2(&self) -> io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let ver = sqlx::query("SELECT version FROM schema_version WHERE id = 1")
            .fetch_optional(&self.pool)
            .await
            .map_err(err)?
            .map(|r| r.int("version"))
            .unwrap_or(0);
        if ver >= SCHEMA_VERSION {
            return Ok(());
        }
        // Read the v1 rows (name unique then) to build the seg map and patch MR endpoints BEFORE the
        // rebuild drops the old tables.
        let agent_rows = sqlx::query("SELECT name, owner FROM agents").fetch_all(&self.pool).await.map_err(err)?;
        let pairs: Vec<(String, Option<String>)> = agent_rows.iter().map(|r| (r.text("name"), r.opt("owner"))).collect();
        let map = seg_map(&pairs);
        let mr_rows = sqlx::query("SELECT data FROM mrs").fetch_all(&self.pool).await.map_err(err)?;
        let mut mrs: Vec<Mr> = mr_rows.iter().filter_map(row_mr).collect();
        for m in mrs.iter_mut() {
            backfill_mr_owner(m, &map);
        }
        let mut tx = self.pool.begin().await.map_err(err)?;
        for stmt in agents_rebuild_ddl() {
            sqlx::query(&stmt).execute(&mut *tx).await.map_err(err)?;
        }
        sqlx::query("CREATE INDEX IF NOT EXISTS agents_aid ON agents(aid)").execute(&mut *tx).await.map_err(err)?;
        sqlx::query("DROP TABLE IF EXISTS mrs").execute(&mut *tx).await.map_err(err)?;
        sqlx::query(
            "CREATE TABLE mrs (target_owner TEXT NOT NULL, target_agent TEXT NOT NULL, id BIGINT NOT NULL, data TEXT NOT NULL, PRIMARY KEY (target_owner, target_agent, id))",
        )
        .execute(&mut *tx)
        .await
        .map_err(err)?;
        for m in &mrs {
            let data = serde_json::to_string(m).map_err(err)?;
            sqlx::query("INSERT INTO mrs (target_owner, target_agent, id, data) VALUES (?, ?, ?, ?)")
                .bind(&m.target.owner)
                .bind(&m.target.agent)
                .bind(m.id as i64)
                .bind(data)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        // Re-home repos + blobs on disk, THEN record v2 — so an interrupted run re-migrates instead of
        // leaving repos stranded at the old flat paths.
        reorg_fs(&self.root, &map);
        sqlx::query("UPDATE schema_version SET version = ? WHERE id = 1").bind(SCHEMA_VERSION).execute(&self.pool).await.map_err(err)?;
        Ok(())
    }

    async fn users(&self) -> Vec<User> {
        match sqlx::query("SELECT * FROM users").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().map(row_user).collect(),
            Err(_) => vec![],
        }
    }

    async fn add_user(&self, user: User) -> io::Result<()> {
        // Serialized with the update_* writers: without the lock, a deferred begin() racing another
        // writer can surface a raw "database is locked" instead of the clean AlreadyExists the unique
        // constraint gives.
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        let existing: Option<sqlx::sqlite::SqliteRow> =
            sqlx::query("SELECT 1 AS one FROM users WHERE username = ?").bind(&user.username).fetch_optional(&mut *tx).await.map_err(err)?;
        if existing.is_some() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("user already exists: {}", user.username)));
        }
        sqlx::query("INSERT INTO users (username, pw_hash, salt, kdf, is_admin, created, totp_secret, totp_enabled, totp_backup_codes, email_verified, disabled) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&user.username)
            .bind(&user.pw_hash)
            .bind(&user.salt)
            .bind(&user.kdf)
            .bind(user.is_admin as i64)
            .bind(&user.created)
            .bind(&user.totp_secret)
            .bind(user.totp_enabled as i64)
            .bind(json_text(&user.totp_backup_codes))
            .bind(user.email_verified as i64)
            .bind(user.disabled as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db) if db.is_unique_violation() => {
                    io::Error::new(io::ErrorKind::AlreadyExists, format!("user already exists: {}", user.username))
                }
                _ => err(e),
            })?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn update_users<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<User>) -> R,
    {
        // Same read-modify-write critical section as the other writers: the async write mutex, then a
        // tracked transaction that auto-rolls back if the handler future is dropped mid-write.
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        let rows = sqlx::query("SELECT * FROM users").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<User> = rows.iter().map(row_user).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM users").execute(&mut *tx).await.map_err(err)?;
        for u in &list {
            sqlx::query("INSERT INTO users (username, pw_hash, salt, kdf, is_admin, created, totp_secret, totp_enabled, totp_backup_codes, email_verified, disabled) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
                .bind(&u.username)
                .bind(&u.pw_hash)
                .bind(&u.salt)
                .bind(&u.kdf)
                .bind(u.is_admin as i64)
                .bind(&u.created)
                .bind(&u.totp_secret)
                .bind(u.totp_enabled as i64)
                .bind(json_text(&u.totp_backup_codes))
                .bind(u.email_verified as i64)
                .bind(u.disabled as i64)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn agents(&self) -> Vec<AgentMeta> {
        match sqlx::query("SELECT * FROM agents").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().map(row_agent).collect(),
            Err(_) => vec![],
        }
    }

    async fn update_agents<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<AgentMeta>) -> R,
    {
        // One writer at a time (the async mutex), then a plain tracked transaction. sqlx auto-rolls
        // this back on drop, so a client disconnect mid-write releases the connection clean instead of
        // wedging the pool's single writer inside an untracked BEGIN IMMEDIATE.
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        let rows = sqlx::query("SELECT * FROM agents").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<AgentMeta> = rows.iter().map(row_agent).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM agents").execute(&mut *tx).await.map_err(err)?;
        for a in &list {
            sqlx::query(
                "INSERT INTO agents (name, aid, owner, visibility, lifecycle, description, forked_from, forked_from_aid, aid_conflict, stars, members, created) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&a.name)
            .bind(&a.aid)
            .bind(&a.owner)
            .bind(&a.visibility)
            .bind(&a.lifecycle)
            .bind(&a.description)
            .bind(&a.forked_from)
            .bind(&a.forked_from_aid)
            .bind(&a.aid_conflict)
            .bind(serde_json::to_string(&a.stars).unwrap_or_else(|_| "[]".into()))
            .bind(serde_json::to_string(&a.members).unwrap_or_else(|_| "[]".into()))
            .bind(&a.created)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn mrs(&self) -> Vec<Mr> {
        match sqlx::query("SELECT data FROM mrs").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().filter_map(row_mr).collect(),
            Err(_) => vec![],
        }
    }

    async fn update_mrs<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<Mr>) -> R,
    {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        let rows = sqlx::query("SELECT data FROM mrs").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<Mr> = rows.iter().filter_map(row_mr).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM mrs").execute(&mut *tx).await.map_err(err)?;
        for m in &list {
            let data = serde_json::to_string(m).map_err(err)?;
            sqlx::query("INSERT INTO mrs (target_owner, target_agent, id, data) VALUES (?, ?, ?, ?)")
                .bind(&m.target.owner)
                .bind(&m.target.agent)
                .bind(m.id as i64)
                .bind(data)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn tokens(&self) -> Vec<TokenRec> {
        match sqlx::query("SELECT * FROM tokens").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().map(row_token).collect(),
            Err(_) => vec![],
        }
    }

    async fn update_tokens<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<TokenRec>) -> R,
    {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        let rows = sqlx::query("SELECT * FROM tokens").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<TokenRec> = rows.iter().map(row_token).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM tokens").execute(&mut *tx).await.map_err(err)?;
        for t in &list {
            sqlx::query(
                "INSERT INTO tokens (id, name, owner, agent, scope, hash, created, expires, last_used) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&t.id)
            .bind(&t.name)
            .bind(&t.owner)
            .bind(&t.agent)
            .bind(&t.scope)
            .bind(&t.hash)
            .bind(&t.created)
            .bind(&t.expires)
            .bind(&t.last_used)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn orgs(&self) -> Vec<Org> {
        match sqlx::query("SELECT * FROM orgs").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().map(row_org).collect(),
            Err(_) => vec![],
        }
    }

    async fn update_orgs<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<Org>) -> R,
    {
        // Same serialization as the other three writers: the async write mutex, then a tracked
        // transaction so a dropped handler future auto-rolls back instead of wedging the pool.
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        let rows = sqlx::query("SELECT * FROM orgs").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<Org> = rows.iter().map(row_org).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM orgs").execute(&mut *tx).await.map_err(err)?;
        for o in &list {
            sqlx::query("INSERT INTO orgs (name, members, created, current_kek_gen, recovery_x25519, escrow_mode, members_can_create) VALUES (?, ?, ?, ?, ?, ?, ?)")
                .bind(&o.name)
                .bind(serde_json::to_string(&o.members).unwrap_or_else(|_| "[]".into()))
                .bind(&o.created)
                .bind(o.current_kek_gen)
                .bind(&o.recovery_x25519)
                .bind(&o.escrow_mode)
                .bind(o.members_can_create)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn invitations(&self) -> Vec<Invitation> {
        match sqlx::query("SELECT * FROM invitations").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().map(row_invitation).collect(),
            Err(_) => vec![],
        }
    }

    async fn update_invitations<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<Invitation>) -> R,
    {
        // Same snapshot-rewrite critical section as the other four writers: the async write mutex, then
        // a tracked transaction so a dropped handler future auto-rolls back instead of wedging the pool.
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        let rows = sqlx::query("SELECT * FROM invitations").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<Invitation> = rows.iter().map(row_invitation).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM invitations").execute(&mut *tx).await.map_err(err)?;
        for i in &list {
            sqlx::query("INSERT INTO invitations (id, org, invitee, role, status, created_by, created) VALUES (?, ?, ?, ?, ?, ?, ?)")
                .bind(&i.id)
                .bind(&i.org)
                .bind(&i.invitee)
                .bind(&i.role)
                .bind(&i.status)
                .bind(&i.created_by)
                .bind(&i.created)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn get_primary_identity_key(&self, username: &str) -> Option<IdentityKey> {
        // The account's latest non-revoked device key: newest `created` first, `key_fpr` as a stable
        // tiebreak when two keys share a (second-resolution) timestamp.
        let row = sqlx::query(
            "SELECT * FROM identity_keys WHERE username = ? AND (revoked IS NULL OR revoked = '') \
             ORDER BY created DESC, key_fpr DESC LIMIT 1",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await
        .ok()??;
        Some(row_identity_key(&row))
    }

    async fn list_identity_keys(&self, username: &str) -> Vec<IdentityKey> {
        let rows = sqlx::query(
            "SELECT * FROM identity_keys WHERE username = ? AND (revoked IS NULL OR revoked = '') \
             ORDER BY created DESC, key_fpr DESC",
        )
        .bind(username)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();
        rows.iter().map(row_identity_key).collect()
    }

    async fn add_identity_key(&self, row: IdentityKey) -> io::Result<EnrollOutcome> {
        // The same read-modify-write critical section every other writer runs in: the async write
        // mutex, then a tracked transaction. The per-key epoch read + the write share the one transaction,
        // so the monotonic check cannot be raced by a concurrent enroll. Keyed on (username, key_fpr), so
        // adding a NEW device key never touches the account's OTHER keys.
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        let stored: Option<i64> = sqlx::query("SELECT epoch FROM identity_keys WHERE username = ? AND key_fpr = ?")
            .bind(&row.username)
            .bind(&row.key_fpr)
            .fetch_optional(&mut *tx)
            .await
            .map_err(err)?
            .map(|r| r.int("epoch"));
        if let Some(stored) = stored {
            if row.epoch <= stored {
                return Ok(EnrollOutcome::StaleEpoch { stored });
            }
        }
        // ON CONFLICT on the COMPOSITE key keeps this device's original `created` (only its first enroll
        // stamps it) and refreshes the rest — including clearing `revoked`, so re-enrolling un-revokes.
        sqlx::query(
            "INSERT INTO identity_keys (username, key_fpr, ed25519_pub, x25519_pub, label, epoch, enroll_sig, created, revoked, email) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(username, key_fpr) DO UPDATE SET \
               ed25519_pub = excluded.ed25519_pub, x25519_pub = excluded.x25519_pub, label = excluded.label, \
               epoch = excluded.epoch, enroll_sig = excluded.enroll_sig, revoked = excluded.revoked, \
               email = excluded.email",
        )
        .bind(&row.username)
        .bind(&row.key_fpr)
        .bind(&row.ed25519_pub)
        .bind(&row.x25519_pub)
        .bind(&row.label)
        .bind(row.epoch)
        .bind(&row.enroll_sig)
        .bind(&row.created)
        .bind(&row.revoked)
        .bind(&row.email)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(EnrollOutcome::Applied)
    }

    async fn revoke_identity_key(&self, username: &str, key_fpr: &str) -> io::Result<bool> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        let res = sqlx::query(
            "UPDATE identity_keys SET revoked = ? WHERE username = ? AND key_fpr = ? \
             AND (revoked IS NULL OR revoked = '')",
        )
        .bind(now_iso())
        .bind(username)
        .bind(key_fpr)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn identity_email_owner(&self, email: &str) -> Option<String> {
        // The single account owning `email` via a NON-REVOKED key, or None when the email maps to zero or
        // (ambiguously) 2+ distinct accounts. `LIMIT 2` distinguishes "exactly one" from "more than one".
        let rows = sqlx::query(
            "SELECT DISTINCT username FROM identity_keys \
             WHERE email = ? AND email <> '' AND (revoked IS NULL OR revoked = '') LIMIT 2",
        )
        .bind(email)
        .fetch_all(&self.pool)
        .await
        .ok()?;
        match rows.as_slice() {
            [only] => Some(only.text("username")),
            _ => None,
        }
    }

    async fn mint_email_token(&self, row: &EmailVerifyToken) -> io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        sqlx::query("INSERT INTO email_verify_tokens (token, username, email, expires, created) VALUES (?, ?, ?, ?, ?)")
            .bind(&row.token)
            .bind(&row.username)
            .bind(&row.email)
            .bind(&row.expires)
            .bind(&row.created)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn consume_email_token(&self, token: &str) -> Option<(String, String)> {
        // Single-use: read + DELETE in one write-locked transaction so two racing consumers cannot both
        // succeed. The expiry check happens AFTER the delete (a stale token is cleaned up either way), so
        // an expired token yields None even though it was removed.
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.ok()?;
        let row = sqlx::query("SELECT username, email, expires FROM email_verify_tokens WHERE token = ?")
            .bind(token)
            .fetch_optional(&mut *tx)
            .await
            .ok()??;
        sqlx::query("DELETE FROM email_verify_tokens WHERE token = ?").bind(token).execute(&mut *tx).await.ok()?;
        tx.commit().await.ok()?;
        if is_expired(&row.text("expires")) {
            return None;
        }
        Some((row.text("username"), row.text("email")))
    }

    async fn mint_password_reset_token(&self, row: &PasswordResetToken) -> io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        sqlx::query("INSERT INTO password_reset_tokens (token, username, expires, created) VALUES (?, ?, ?, ?)")
            .bind(&row.token)
            .bind(&row.username)
            .bind(&row.expires)
            .bind(&row.created)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn consume_password_reset_token(&self, token: &str) -> Option<String> {
        // Single-use: read + DELETE in one write-locked transaction so two racing consumers cannot both
        // succeed. The expiry check happens AFTER the delete (a stale token is cleaned up either way), so
        // an expired token yields None even though it was removed.
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.ok()?;
        let row = sqlx::query("SELECT username, expires FROM password_reset_tokens WHERE token = ?")
            .bind(token)
            .fetch_optional(&mut *tx)
            .await
            .ok()??;
        // Consuming a reset revokes ALL of that user's outstanding reset tokens, not just the presented
        // one, so an older leaked reset link cannot be replayed after the user has already reset.
        sqlx::query("DELETE FROM password_reset_tokens WHERE username = ?").bind(row.text("username")).execute(&mut *tx).await.ok()?;
        tx.commit().await.ok()?;
        if is_expired(&row.text("expires")) {
            return None;
        }
        Some(row.text("username"))
    }

    async fn password_reset_token_count(&self) -> usize {
        sqlx::query("SELECT token FROM password_reset_tokens").fetch_all(&self.pool).await.map(|r| r.len()).unwrap_or(0)
    }

    async fn upsert_team_kek_envelopes(&self, org: &str, gen: i64, rows: &[TeamKekEnvelope]) -> io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        for row in rows {
            sqlx::query(
                "INSERT INTO team_keks (org, gen, recipient, wrapped_kek, recipient_epoch, created) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(org, gen, recipient) DO UPDATE SET \
                   wrapped_kek = excluded.wrapped_kek, recipient_epoch = excluded.recipient_epoch",
            )
            .bind(org)
            .bind(gen)
            .bind(normalize_username(&row.recipient))
            .bind(&row.wrapped_kek)
            .bind(row.recipient_epoch)
            .bind(&row.created)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn get_team_kek_envelope(&self, org: &str, gen: i64, recipient: &str) -> Option<TeamKekEnvelope> {
        match sqlx::query("SELECT * FROM team_keks WHERE org = ? AND gen = ? AND recipient = ?")
            .bind(org)
            .bind(gen)
            .bind(recipient)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(r)) => Some(row_team_kek(&r)),
            _ => None,
        }
    }

    async fn list_team_kek_gens(&self, org: &str) -> Vec<i64> {
        match sqlx::query("SELECT DISTINCT gen FROM team_keks WHERE org = ? ORDER BY gen ASC")
            .bind(org)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => rows.iter().map(|r| r.int("gen")).collect(),
            Err(_) => vec![],
        }
    }

    async fn set_current_kek_gen(&self, org: &str, gen: i64) -> io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        // Monotonic at the SQL level: only ever advance the generation, so a stale concurrent publish
        // cannot roll it back even if it passed the API-layer check. gen <= current is a silent no-op.
        sqlx::query("UPDATE orgs SET current_kek_gen = ? WHERE name = ? AND ? > current_kek_gen")
            .bind(gen)
            .bind(org)
            .bind(gen)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn upsert_escrow_key(&self, key: &EscrowKey) -> io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(err)?;
        sqlx::query(
            "INSERT INTO escrow_keys (owner, name, kid, wrapped_ck, created) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(owner, name, kid) DO UPDATE SET wrapped_ck = excluded.wrapped_ck",
        )
        .bind(&key.owner)
        .bind(&key.name)
        .bind(key.kid)
        .bind(&key.wrapped_ck)
        .bind(&key.created)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn get_escrow_keys(&self, owner: &str, name: &str) -> Vec<EscrowKey> {
        match sqlx::query("SELECT * FROM escrow_keys WHERE owner = ? AND name = ? ORDER BY kid ASC")
            .bind(owner)
            .bind(name)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => rows.iter().map(row_escrow_key).collect(),
            Err(_) => vec![],
        }
    }
}

// ─────────────────────────── Postgres backend ───────────────────────────

#[derive(Clone)]
pub struct PgStore {
    pool: sqlx::PgPool,
    root: PathBuf,
}

impl PgStore {
    fn connect(url: &str, root: PathBuf) -> io::Result<PgStore> {
        use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
        use std::str::FromStr;
        let opts = PgConnectOptions::from_str(url).map_err(err)?;
        // A bounded acquire timeout so a wrong/unreachable AGIT_HUB_DB surfaces at boot in seconds
        // (via migrate's first query) instead of hanging on sqlx's 30s default while it retries.
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(Duration::from_secs(8))
            .connect_lazy_with(opts);
        Ok(PgStore { pool, root })
    }

    async fn migrate(&self) -> io::Result<()> {
        for stmt in DDL {
            sqlx::query(stmt).execute(&self.pool).await.map_err(err)?;
        }
        // Idempotent single-row stamp — no read-MAX-then-INSERT race under concurrent boot.
        sqlx::query(STAMP_VERSION).execute(&self.pool).await.map_err(err)?;
        self.migrate_v2().await?;
        // Back-fill the 2FA columns onto stores predating them (no-op on a fresh DB). Postgres has a
        // native IF NOT EXISTS, so this is cleanly idempotent without swallowing errors.
        for &col in TOTP_COLUMNS {
            sqlx::query(&format!("ALTER TABLE users ADD COLUMN IF NOT EXISTS {col}")).execute(&self.pool).await.map_err(err)?;
        }
        // Back-fill the email_verified column onto stores predating email verification (no-op on a fresh
        // DB). Postgres has a native IF NOT EXISTS, so this is cleanly idempotent without swallowing errors.
        for &col in USER_COLUMNS {
            sqlx::query(&format!("ALTER TABLE users ADD COLUMN IF NOT EXISTS {col}")).execute(&self.pool).await.map_err(err)?;
        }
        // Back-fill the Wave-3 org columns onto stores predating them (no-op on a fresh DB). Postgres has
        // a native IF NOT EXISTS, so this is cleanly idempotent without swallowing errors.
        for &col in ORG_COLUMNS {
            sqlx::query(&format!("ALTER TABLE orgs ADD COLUMN IF NOT EXISTS {col}")).execute(&self.pool).await.map_err(err)?;
        }
        // Back-fill the identity_keys email column onto registries predating provenance verification
        // (no-op on a fresh DB). Postgres has a native IF NOT EXISTS, so this is cleanly idempotent. Runs
        // BEFORE the multi-key rebuild so a legacy single-key table has `email` before its rows are copied.
        for &col in IDENTITY_COLUMNS {
            sqlx::query(&format!("ALTER TABLE identity_keys ADD COLUMN IF NOT EXISTS {col}")).execute(&self.pool).await.map_err(err)?;
        }
        // Reshape a legacy single-key identity_keys into the multi-key composite shape (back-filling
        // key_fpr + label='default' per row). No-op once already reshaped.
        self.migrate_identity_keys_multikey().await?;
        // Create the email index only after the column is guaranteed present on the FINAL (reshaped) table.
        sqlx::query(IDENTITY_EMAIL_INDEX).execute(&self.pool).await.map_err(err)?;
        Ok(())
    }

    /// Reshape a legacy single-key `identity_keys` into the multi-key composite shape, the Postgres twin of
    /// [`SqliteStore::migrate_identity_keys_multikey`]. Detected by the absence of `key_fpr`; runs under the
    /// one global advisory lock so two hubs booting against one Postgres serialize. Rebuilds via rename +
    /// `IDENTITY_KEYS_DDL` + per-row copy (key_fpr from ed25519_pub, label='default') rather than an
    /// in-place PK swap, so it mirrors the SQLite path exactly and needs no pgcrypto for the fingerprint.
    async fn migrate_identity_keys_multikey(&self) -> io::Result<()> {
        if sqlx::query("SELECT key_fpr FROM identity_keys LIMIT 1").fetch_optional(&self.pool).await.is_ok() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        // Re-check under the advisory lock (a peer boot may have reshaped it between our check and the
        // lock). Probe the CATALOG, not `SELECT key_fpr`: on a legacy table that column is absent, and a
        // failed statement aborts THIS Postgres transaction — every later statement in the rebuild then
        // dies with "current transaction is aborted". information_schema never errors, so the tx stays
        // usable. (The pre-lock guard above is on the autocommit pool, where a failed probe is isolated.)
        let already = sqlx::query(
            "SELECT 1 FROM information_schema.columns WHERE table_name = 'identity_keys' AND column_name = 'key_fpr'",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(err)?
        .is_some();
        if already {
            return Ok(());
        }
        let olds = sqlx::query(
            "SELECT username, ed25519_pub, x25519_pub, epoch, enroll_sig, created, revoked, email FROM identity_keys",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(err)?;
        sqlx::query("DROP INDEX IF EXISTS identity_keys_email").execute(&mut *tx).await.map_err(err)?;
        sqlx::query("ALTER TABLE identity_keys RENAME TO identity_keys_legacy").execute(&mut *tx).await.map_err(err)?;
        sqlx::query(IDENTITY_KEYS_DDL).execute(&mut *tx).await.map_err(err)?;
        for r in &olds {
            let ed = r.text("ed25519_pub");
            let fpr = ed25519_fingerprint(&ed);
            sqlx::query(
                "INSERT INTO identity_keys (username, key_fpr, ed25519_pub, x25519_pub, label, epoch, enroll_sig, created, revoked, email) \
                 VALUES ($1, $2, $3, $4, 'default', $5, $6, $7, $8, $9)",
            )
            .bind(r.text("username"))
            .bind(&fpr)
            .bind(&ed)
            .bind(r.text("x25519_pub"))
            .bind(r.int("epoch"))
            .bind(r.text("enroll_sig"))
            .bind(r.text("created"))
            .bind(r.opt("revoked"))
            .bind(r.text("email"))
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        }
        sqlx::query("DROP TABLE identity_keys_legacy").execute(&mut *tx).await.map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    /// v1 → v2 owner-scoping migration for Postgres. The rebuild runs under the one global advisory
    /// lock so two hubs booting against one Postgres serialize; the rebuild itself is idempotent, so a
    /// redundant concurrent run is harmless. No-op once `schema_version >= 2`.
    async fn migrate_v2(&self) -> io::Result<()> {
        let ver = sqlx::query("SELECT version FROM schema_version WHERE id = 1")
            .fetch_optional(&self.pool)
            .await
            .map_err(err)?
            .map(|r| r.int("version"))
            .unwrap_or(0);
        if ver >= SCHEMA_VERSION {
            return Ok(());
        }
        let agent_rows = sqlx::query("SELECT name, owner FROM agents").fetch_all(&self.pool).await.map_err(err)?;
        let pairs: Vec<(String, Option<String>)> = agent_rows.iter().map(|r| (r.text("name"), r.opt("owner"))).collect();
        let map = seg_map(&pairs);
        let mr_rows = sqlx::query("SELECT data FROM mrs").fetch_all(&self.pool).await.map_err(err)?;
        let mut mrs: Vec<Mr> = mr_rows.iter().filter_map(row_mr).collect();
        for m in mrs.iter_mut() {
            backfill_mr_owner(m, &map);
        }
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        for stmt in agents_rebuild_ddl() {
            sqlx::query(&stmt).execute(&mut *tx).await.map_err(err)?;
        }
        sqlx::query("CREATE INDEX IF NOT EXISTS agents_aid ON agents(aid)").execute(&mut *tx).await.map_err(err)?;
        sqlx::query("DROP TABLE IF EXISTS mrs").execute(&mut *tx).await.map_err(err)?;
        sqlx::query(
            "CREATE TABLE mrs (target_owner TEXT NOT NULL, target_agent TEXT NOT NULL, id BIGINT NOT NULL, data TEXT NOT NULL, PRIMARY KEY (target_owner, target_agent, id))",
        )
        .execute(&mut *tx)
        .await
        .map_err(err)?;
        for m in &mrs {
            let data = serde_json::to_string(m).map_err(err)?;
            sqlx::query("INSERT INTO mrs (target_owner, target_agent, id, data) VALUES ($1, $2, $3, $4)")
                .bind(&m.target.owner)
                .bind(&m.target.agent)
                .bind(m.id as i64)
                .bind(data)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        reorg_fs(&self.root, &map);
        sqlx::query("UPDATE schema_version SET version = $1 WHERE id = 1").bind(SCHEMA_VERSION).execute(&self.pool).await.map_err(err)?;
        Ok(())
    }

    /// Take the one global advisory lock at the head of every read-modify-write transaction. Held
    /// until the transaction ends (`_xact_`), so the SELECT → closure → DELETE+re-INSERT snapshot
    /// runs alone: the second concurrent writer blocks here until the first commits, instead of
    /// SELECTing the pre-DELETE table and wiping the first writer's just-committed rows.
    async fn lock(tx: &mut sqlx::PgConnection) -> io::Result<()> {
        sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(PG_ADVISORY_KEY).execute(&mut *tx).await.map_err(err)?;
        Ok(())
    }

    async fn users(&self) -> Vec<User> {
        match sqlx::query("SELECT * FROM users").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().map(row_user).collect(),
            Err(_) => vec![],
        }
    }

    async fn add_user(&self, user: User) -> io::Result<()> {
        // No advisory lock needed: the username PRIMARY KEY is the authority. A concurrent duplicate
        // loses the INSERT (unique violation → AlreadyExists), not the SELECT-then-INSERT check.
        let mut tx = self.pool.begin().await.map_err(err)?;
        let existing: Option<sqlx::postgres::PgRow> =
            sqlx::query("SELECT 1 AS one FROM users WHERE username = $1").bind(&user.username).fetch_optional(&mut *tx).await.map_err(err)?;
        if existing.is_some() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("user already exists: {}", user.username)));
        }
        sqlx::query("INSERT INTO users (username, pw_hash, salt, kdf, is_admin, created, totp_secret, totp_enabled, totp_backup_codes, email_verified, disabled) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)")
            .bind(&user.username)
            .bind(&user.pw_hash)
            .bind(&user.salt)
            .bind(&user.kdf)
            .bind(user.is_admin as i64)
            .bind(&user.created)
            .bind(&user.totp_secret)
            .bind(user.totp_enabled as i64)
            .bind(json_text(&user.totp_backup_codes))
            .bind(user.email_verified as i64)
            .bind(user.disabled as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db) if db.is_unique_violation() => {
                    io::Error::new(io::ErrorKind::AlreadyExists, format!("user already exists: {}", user.username))
                }
                _ => err(e),
            })?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn update_users<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<User>) -> R,
    {
        // The same single advisory-lock critical section every other read-modify-write runs in.
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        let rows = sqlx::query("SELECT * FROM users").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<User> = rows.iter().map(row_user).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM users").execute(&mut *tx).await.map_err(err)?;
        for u in &list {
            sqlx::query("INSERT INTO users (username, pw_hash, salt, kdf, is_admin, created, totp_secret, totp_enabled, totp_backup_codes, email_verified, disabled) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)")
                .bind(&u.username)
                .bind(&u.pw_hash)
                .bind(&u.salt)
                .bind(&u.kdf)
                .bind(u.is_admin as i64)
                .bind(&u.created)
                .bind(&u.totp_secret)
                .bind(u.totp_enabled as i64)
                .bind(json_text(&u.totp_backup_codes))
                .bind(u.email_verified as i64)
                .bind(u.disabled as i64)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn agents(&self) -> Vec<AgentMeta> {
        match sqlx::query("SELECT * FROM agents").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().map(row_agent).collect(),
            Err(_) => vec![],
        }
    }

    async fn update_agents<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<AgentMeta>) -> R,
    {
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        let rows = sqlx::query("SELECT * FROM agents").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<AgentMeta> = rows.iter().map(row_agent).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM agents").execute(&mut *tx).await.map_err(err)?;
        for a in &list {
            sqlx::query(
                "INSERT INTO agents (name, aid, owner, visibility, lifecycle, description, forked_from, forked_from_aid, aid_conflict, stars, members, created) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(&a.name)
            .bind(&a.aid)
            .bind(&a.owner)
            .bind(&a.visibility)
            .bind(&a.lifecycle)
            .bind(&a.description)
            .bind(&a.forked_from)
            .bind(&a.forked_from_aid)
            .bind(&a.aid_conflict)
            .bind(serde_json::to_string(&a.stars).unwrap_or_else(|_| "[]".into()))
            .bind(serde_json::to_string(&a.members).unwrap_or_else(|_| "[]".into()))
            .bind(&a.created)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn mrs(&self) -> Vec<Mr> {
        match sqlx::query("SELECT data FROM mrs").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().filter_map(row_mr).collect(),
            Err(_) => vec![],
        }
    }

    async fn update_mrs<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<Mr>) -> R,
    {
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        let rows = sqlx::query("SELECT data FROM mrs").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<Mr> = rows.iter().filter_map(row_mr).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM mrs").execute(&mut *tx).await.map_err(err)?;
        for m in &list {
            let data = serde_json::to_string(m).map_err(err)?;
            sqlx::query("INSERT INTO mrs (target_owner, target_agent, id, data) VALUES ($1, $2, $3, $4)")
                .bind(&m.target.owner)
                .bind(&m.target.agent)
                .bind(m.id as i64)
                .bind(data)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn tokens(&self) -> Vec<TokenRec> {
        match sqlx::query("SELECT * FROM tokens").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().map(row_token).collect(),
            Err(_) => vec![],
        }
    }

    async fn update_tokens<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<TokenRec>) -> R,
    {
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        let rows = sqlx::query("SELECT * FROM tokens").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<TokenRec> = rows.iter().map(row_token).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM tokens").execute(&mut *tx).await.map_err(err)?;
        for t in &list {
            sqlx::query(
                "INSERT INTO tokens (id, name, owner, agent, scope, hash, created, expires, last_used) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(&t.id)
            .bind(&t.name)
            .bind(&t.owner)
            .bind(&t.agent)
            .bind(&t.scope)
            .bind(&t.hash)
            .bind(&t.created)
            .bind(&t.expires)
            .bind(&t.last_used)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn orgs(&self) -> Vec<Org> {
        match sqlx::query("SELECT * FROM orgs").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().map(row_org).collect(),
            Err(_) => vec![],
        }
    }

    async fn update_orgs<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<Org>) -> R,
    {
        // The same single advisory-lock critical section every other read-modify-write runs in, so an
        // org edit a create/transfer depends on cannot interleave with an agents/tokens/mrs rewrite.
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        let rows = sqlx::query("SELECT * FROM orgs").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<Org> = rows.iter().map(row_org).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM orgs").execute(&mut *tx).await.map_err(err)?;
        for o in &list {
            sqlx::query("INSERT INTO orgs (name, members, created, current_kek_gen, recovery_x25519, escrow_mode, members_can_create) VALUES ($1, $2, $3, $4, $5, $6, $7)")
                .bind(&o.name)
                .bind(serde_json::to_string(&o.members).unwrap_or_else(|_| "[]".into()))
                .bind(&o.created)
                .bind(o.current_kek_gen)
                .bind(&o.recovery_x25519)
                .bind(&o.escrow_mode)
                .bind(o.members_can_create)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn invitations(&self) -> Vec<Invitation> {
        match sqlx::query("SELECT * FROM invitations").fetch_all(&self.pool).await {
            Ok(rows) => rows.iter().map(row_invitation).collect(),
            Err(_) => vec![],
        }
    }

    async fn update_invitations<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<Invitation>) -> R,
    {
        // The same single advisory-lock critical section every other read-modify-write runs in, so an
        // accept (invitations rewrite + orgs rewrite) can't interleave with a concurrent org edit.
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        let rows = sqlx::query("SELECT * FROM invitations").fetch_all(&mut *tx).await.map_err(err)?;
        let mut list: Vec<Invitation> = rows.iter().map(row_invitation).collect();
        let r = f(&mut list);
        sqlx::query("DELETE FROM invitations").execute(&mut *tx).await.map_err(err)?;
        for i in &list {
            sqlx::query("INSERT INTO invitations (id, org, invitee, role, status, created_by, created) VALUES ($1, $2, $3, $4, $5, $6, $7)")
                .bind(&i.id)
                .bind(&i.org)
                .bind(&i.invitee)
                .bind(&i.role)
                .bind(&i.status)
                .bind(&i.created_by)
                .bind(&i.created)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(r)
    }

    async fn get_primary_identity_key(&self, username: &str) -> Option<IdentityKey> {
        let row = sqlx::query(
            "SELECT * FROM identity_keys WHERE username = $1 AND (revoked IS NULL OR revoked = '') \
             ORDER BY created DESC, key_fpr DESC LIMIT 1",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await
        .ok()??;
        Some(row_identity_key(&row))
    }

    async fn list_identity_keys(&self, username: &str) -> Vec<IdentityKey> {
        let rows = sqlx::query(
            "SELECT * FROM identity_keys WHERE username = $1 AND (revoked IS NULL OR revoked = '') \
             ORDER BY created DESC, key_fpr DESC",
        )
        .bind(username)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();
        rows.iter().map(row_identity_key).collect()
    }

    async fn add_identity_key(&self, row: IdentityKey) -> io::Result<EnrollOutcome> {
        // The one global advisory-lock critical section every read-modify-write runs in, so the per-key
        // epoch read + the write are one atomic section and the monotonic check cannot be raced. Keyed on
        // (username, key_fpr), so adding a NEW device key never touches the account's OTHER keys.
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        let stored: Option<i64> = sqlx::query("SELECT epoch FROM identity_keys WHERE username = $1 AND key_fpr = $2")
            .bind(&row.username)
            .bind(&row.key_fpr)
            .fetch_optional(&mut *tx)
            .await
            .map_err(err)?
            .map(|r| r.int("epoch"));
        if let Some(stored) = stored {
            if row.epoch <= stored {
                return Ok(EnrollOutcome::StaleEpoch { stored });
            }
        }
        // ON CONFLICT on the COMPOSITE key keeps this device's original `created` and refreshes the rest,
        // clearing `revoked` (re-enrolling un-revokes).
        sqlx::query(
            "INSERT INTO identity_keys (username, key_fpr, ed25519_pub, x25519_pub, label, epoch, enroll_sig, created, revoked, email) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (username, key_fpr) DO UPDATE SET \
               ed25519_pub = excluded.ed25519_pub, x25519_pub = excluded.x25519_pub, label = excluded.label, \
               epoch = excluded.epoch, enroll_sig = excluded.enroll_sig, revoked = excluded.revoked, \
               email = excluded.email",
        )
        .bind(&row.username)
        .bind(&row.key_fpr)
        .bind(&row.ed25519_pub)
        .bind(&row.x25519_pub)
        .bind(&row.label)
        .bind(row.epoch)
        .bind(&row.enroll_sig)
        .bind(&row.created)
        .bind(&row.revoked)
        .bind(&row.email)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(EnrollOutcome::Applied)
    }

    async fn revoke_identity_key(&self, username: &str, key_fpr: &str) -> io::Result<bool> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        let res = sqlx::query(
            "UPDATE identity_keys SET revoked = $1 WHERE username = $2 AND key_fpr = $3 \
             AND (revoked IS NULL OR revoked = '')",
        )
        .bind(now_iso())
        .bind(username)
        .bind(key_fpr)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn identity_email_owner(&self, email: &str) -> Option<String> {
        let rows = sqlx::query(
            "SELECT DISTINCT username FROM identity_keys \
             WHERE email = $1 AND email <> '' AND (revoked IS NULL OR revoked = '') LIMIT 2",
        )
        .bind(email)
        .fetch_all(&self.pool)
        .await
        .ok()?;
        match rows.as_slice() {
            [only] => Some(only.text("username")),
            _ => None,
        }
    }

    async fn mint_email_token(&self, row: &EmailVerifyToken) -> io::Result<()> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        sqlx::query("INSERT INTO email_verify_tokens (token, username, email, expires, created) VALUES ($1, $2, $3, $4, $5)")
            .bind(&row.token)
            .bind(&row.username)
            .bind(&row.email)
            .bind(&row.expires)
            .bind(&row.created)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn consume_email_token(&self, token: &str) -> Option<(String, String)> {
        // Single-use: read + DELETE in one advisory-locked transaction so two racing consumers cannot both
        // succeed. The expiry check happens AFTER the delete, so an expired token yields None even though
        // it was removed.
        let mut tx = self.pool.begin().await.ok()?;
        Self::lock(&mut tx).await.ok()?;
        let row = sqlx::query("SELECT username, email, expires FROM email_verify_tokens WHERE token = $1")
            .bind(token)
            .fetch_optional(&mut *tx)
            .await
            .ok()??;
        sqlx::query("DELETE FROM email_verify_tokens WHERE token = $1").bind(token).execute(&mut *tx).await.ok()?;
        tx.commit().await.ok()?;
        if is_expired(&row.text("expires")) {
            return None;
        }
        Some((row.text("username"), row.text("email")))
    }

    async fn mint_password_reset_token(&self, row: &PasswordResetToken) -> io::Result<()> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        sqlx::query("INSERT INTO password_reset_tokens (token, username, expires, created) VALUES ($1, $2, $3, $4)")
            .bind(&row.token)
            .bind(&row.username)
            .bind(&row.expires)
            .bind(&row.created)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn consume_password_reset_token(&self, token: &str) -> Option<String> {
        // Single-use: read + DELETE in one advisory-locked transaction so two racing consumers cannot both
        // succeed. The expiry check happens AFTER the delete, so an expired token yields None even though
        // it was removed.
        let mut tx = self.pool.begin().await.ok()?;
        Self::lock(&mut tx).await.ok()?;
        let row = sqlx::query("SELECT username, expires FROM password_reset_tokens WHERE token = $1")
            .bind(token)
            .fetch_optional(&mut *tx)
            .await
            .ok()??;
        // Consuming a reset revokes ALL of that user's outstanding reset tokens, not just the presented
        // one, so an older leaked reset link cannot be replayed after the user has already reset.
        sqlx::query("DELETE FROM password_reset_tokens WHERE username = $1").bind(row.text("username")).execute(&mut *tx).await.ok()?;
        tx.commit().await.ok()?;
        if is_expired(&row.text("expires")) {
            return None;
        }
        Some(row.text("username"))
    }

    async fn password_reset_token_count(&self) -> usize {
        sqlx::query("SELECT token FROM password_reset_tokens").fetch_all(&self.pool).await.map(|r| r.len()).unwrap_or(0)
    }

    async fn upsert_team_kek_envelopes(&self, org: &str, gen: i64, rows: &[TeamKekEnvelope]) -> io::Result<()> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        for row in rows {
            sqlx::query(
                "INSERT INTO team_keks (org, gen, recipient, wrapped_kek, recipient_epoch, created) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (org, gen, recipient) DO UPDATE SET \
                   wrapped_kek = excluded.wrapped_kek, recipient_epoch = excluded.recipient_epoch",
            )
            .bind(org)
            .bind(gen)
            .bind(normalize_username(&row.recipient))
            .bind(&row.wrapped_kek)
            .bind(row.recipient_epoch)
            .bind(&row.created)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn get_team_kek_envelope(&self, org: &str, gen: i64, recipient: &str) -> Option<TeamKekEnvelope> {
        match sqlx::query("SELECT * FROM team_keks WHERE org = $1 AND gen = $2 AND recipient = $3")
            .bind(org)
            .bind(gen)
            .bind(recipient)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(r)) => Some(row_team_kek(&r)),
            _ => None,
        }
    }

    async fn list_team_kek_gens(&self, org: &str) -> Vec<i64> {
        match sqlx::query("SELECT DISTINCT gen FROM team_keks WHERE org = $1 ORDER BY gen ASC")
            .bind(org)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => rows.iter().map(|r| r.int("gen")).collect(),
            Err(_) => vec![],
        }
    }

    async fn set_current_kek_gen(&self, org: &str, gen: i64) -> io::Result<()> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        // Monotonic at the SQL level: only ever advance the generation, so a stale concurrent publish
        // cannot roll it back even if it passed the API-layer check. gen <= current is a silent no-op.
        sqlx::query("UPDATE orgs SET current_kek_gen = $1 WHERE name = $2 AND $1 > current_kek_gen")
            .bind(gen)
            .bind(org)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn upsert_escrow_key(&self, key: &EscrowKey) -> io::Result<()> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        Self::lock(&mut tx).await?;
        sqlx::query(
            "INSERT INTO escrow_keys (owner, name, kid, wrapped_ck, created) VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (owner, name, kid) DO UPDATE SET wrapped_ck = excluded.wrapped_ck",
        )
        .bind(&key.owner)
        .bind(&key.name)
        .bind(key.kid)
        .bind(&key.wrapped_ck)
        .bind(&key.created)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(())
    }

    async fn get_escrow_keys(&self, owner: &str, name: &str) -> Vec<EscrowKey> {
        match sqlx::query("SELECT * FROM escrow_keys WHERE owner = $1 AND name = $2 ORDER BY kid ASC")
            .bind(owner)
            .bind(name)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => rows.iter().map(row_escrow_key).collect(),
            Err(_) => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn tmp_store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open_sqlite(d.path()).await.unwrap();
        (d, s)
    }

    /// Create a minimal user row (no real credentials) — enough for the email-verified gate, which only
    /// reads `users.email_verified` for the account an enrolled email maps to.
    async fn add_named_user(s: &Store, name: &str) {
        s.add_user(User { username: name.into(), created: now_iso(), ..Default::default() }).await.unwrap();
    }

    /// Run a raw statement against the SQLite backend — the test-only escape hatch used to plant a
    /// deliberately malformed row (the SQL analog of hand-mangling the old JSON files).
    async fn raw_exec(store: &Store, sql: &str) {
        if let Store::Sqlite(s) = store {
            sqlx::query(sql).execute(&s.pool).await.unwrap();
        }
    }

    #[test]
    fn usernames_are_validated_and_normalized() {
        assert!(valid_username("alice"));
        assert!(valid_username("a.b_c-2"));
        assert!(!valid_username("a"));
        assert!(!valid_username("Alice")); // uppercase must be normalized first
        assert!(!valid_username(".hidden"));
        assert!(!valid_username("a/b"));
        assert!(!valid_username("a b"));
        assert!(!valid_username(""));
        assert!(!valid_username(&"x".repeat(33)));
        assert_eq!(normalize_username("  Alice "), "alice");
    }

    #[tokio::test]
    async fn user_lookup_is_case_insensitive() {
        // "Alice" and "alice" must be the same person, or you could register a same-name account
        // that impersonates the other.
        let (_d, s) = tmp_store().await;
        s.add_user(User {
            username: "alice".into(),
            pw_hash: "h".into(),
            salt: "s".into(),
            kdf: "k".into(),
            is_admin: true,
            created: now_iso(),
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(s.user("ALICE").await.is_some());
        assert!(s.user("Alice").await.is_some());
        assert!(s.user("bob").await.is_none());
    }

    #[tokio::test]
    async fn set_password_updates_only_the_pw_material_and_reports_missing() {
        let (_d, s) = tmp_store().await;
        s.add_user(User {
            username: "alice".into(),
            pw_hash: "old_hash".into(),
            salt: "old_salt".into(),
            kdf: "old_kdf".into(),
            is_admin: true,
            created: "2026-01-01T00:00:00Z".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        // Case-insensitive, like every other lookup — "ALICE" addresses the same row.
        assert!(s.set_password("ALICE", "new_hash", "new_salt", "new_kdf").await.unwrap());
        let u = s.user("alice").await.unwrap();
        assert_eq!(u.pw_hash, "new_hash");
        assert_eq!(u.salt, "new_salt");
        assert_eq!(u.kdf, "new_kdf");
        // Untouched fields survive the rewrite.
        assert!(u.is_admin, "admin bit must not be disturbed by a password change");
        assert_eq!(u.created, "2026-01-01T00:00:00Z");
        // A missing user reports false rather than silently creating one.
        assert!(!s.set_password("ghost", "h", "s", "k").await.unwrap());
        assert!(s.user("ghost").await.is_none());
    }

    #[tokio::test]
    async fn duplicate_user_is_refused() {
        let (_d, s) = tmp_store().await;
        let u = User {
            username: "alice".into(),
            pw_hash: "h".into(),
            salt: "s".into(),
            kdf: "k".into(),
            is_admin: false,
            created: now_iso(),
            ..Default::default()
        };
        s.add_user(u.clone()).await.unwrap();
        let e = s.add_user(u).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn db_file_is_0600_and_root_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let (d, s) = tmp_store().await;
        s.add_user(User {
            username: "alice".into(),
            pw_hash: "h".into(),
            salt: "s".into(),
            kdf: "k".into(),
            is_admin: true,
            created: now_iso(),
            ..Default::default()
        })
        .await
        .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d.path().join("hub.db")), 0o600, "the DB holds credential digests: owner-only");
        assert_eq!(mode(d.path()), 0o700);
    }

    #[tokio::test]
    async fn unknown_agent_is_private_and_unowned() {
        // Repo on disk, no record — it must not turn into "anyone can pull it".
        let (_d, s) = tmp_store().await;
        let m = s.agent_or_unowned("alice", "legacy").await;
        assert_eq!(m.visibility, "private");
        assert!(m.owner.is_none());
        let acl = m.to_acl();
        assert_eq!(acl.visibility, Visibility::Private);
        assert!(acl.owner.is_none());
    }

    #[test]
    fn broken_visibility_falls_back_to_private() {
        let m = AgentMeta {
            visibility: "PUBLIC".into(), // hand-mangled
            members: vec![Member { username: "bob".into(), role: "superuser".into() }],
            ..AgentMeta::new("x", Some("alice"), Visibility::Public)
        };
        let acl = m.to_acl();
        assert_eq!(acl.visibility, Visibility::Private);
        assert!(acl.members.is_empty(), "an unrecognized role must be dropped, not treated as a permission");
    }

    #[test]
    fn a_broken_lifecycle_reads_as_archived_not_as_active() {
        // Tighter than active — nothing is written through a state nobody can parse — but still
        // visible, so the agent can be found and the record fixed. `deleted` would be tighter and
        // wrong: a typo must not erase an agent from every listing.
        let m = AgentMeta { lifecycle: "Active".into(), ..AgentMeta::new("x", Some("alice"), Visibility::Public) };
        assert_eq!(m.lifecycle(), Lifecycle::Archived);
        assert_eq!(m.to_acl().lifecycle, Lifecycle::Archived, "to_acl and lifecycle() must never disagree");
    }

    #[test]
    fn an_agent_record_written_before_lifecycles_reads_as_active() {
        // The upgrade path: an old serialized record has no lifecycle field at all, and every agent
        // in it is live.
        let m: AgentMeta = serde_json::from_str(r#"{"name":"old","visibility":"public"}"#).unwrap();
        assert_eq!(m.lifecycle(), Lifecycle::Active);
        assert_eq!(m.description, None);
        assert!(m.stars.is_empty());
    }

    #[tokio::test]
    async fn agents_roundtrip_through_db() {
        let (_d, s) = tmp_store().await;
        s.update_agents(|a| {
            let mut m = AgentMeta::new("shared", Some("alice"), Visibility::Public);
            m.members.push(Member { username: "bob".into(), role: "write".into() });
            a.push(m);
        })
        .await
        .unwrap();
        let m = s.agent_scoped("alice", "shared").await.unwrap();
        assert_eq!(m.owner.as_deref(), Some("alice"));
        assert_eq!(m.visibility, "public");
        assert_eq!(m.role_of("bob"), Some(Role::Write));
        assert_eq!(m.role_of("eve"), None);
    }

    #[test]
    fn owner_ns_and_reserved_helpers() {
        assert_eq!(owner_ns("alice"), "alice");
        assert_eq!(owner_ns("org:acme"), "acme");
        assert_eq!(owner_ns("_unclaimed"), "_unclaimed");
        assert!(is_reserved_account("_unclaimed"));
        assert!(!is_reserved_account("alice"));
        // Reserved names stay syntactically valid so `/_unclaimed/<name>.git` still routes.
        assert!(valid_username("_unclaimed"));
    }

    #[tokio::test]
    async fn two_owners_hold_the_same_name_independently() {
        // The heart of (owner, name) scoping: daru/frontend and kaisen/frontend coexist.
        let (_d, s) = tmp_store().await;
        s.update_agents(|a| {
            a.push(AgentMeta::new("frontend", Some("daru"), Visibility::Private));
            let mut k = AgentMeta::new("frontend", Some("kaisen"), Visibility::Public);
            k.aid = Some("agt_k".into());
            a.push(k);
            // An org-owned agent: its namespace segment is the org's bare name.
            a.push(AgentMeta::new("shared", Some("org:acme"), Visibility::Private));
        })
        .await
        .unwrap();
        assert_eq!(s.agents().await.len(), 3, "same name under two owners coexists (composite PK)");
        assert_eq!(s.agent_scoped("daru", "frontend").await.unwrap().visibility, "private");
        assert_eq!(s.agent_scoped("kaisen", "frontend").await.unwrap().visibility, "public");
        assert_eq!(s.agent_scoped("daru", "frontend").await.unwrap().scoped(), "daru/frontend");
        assert_eq!(s.agent_scoped("kaisen", "frontend").await.unwrap().scoped(), "kaisen/frontend");
        // org:acme is addressed as /acme, never by the raw stored string.
        assert!(s.agent_scoped("acme", "shared").await.is_some());
        assert!(s.agent_scoped("org:acme", "shared").await.is_none());
        // The fail-safe for an absent (owner, name) is owner:None / private.
        let missing = s.agent_or_unowned("ghost", "frontend").await;
        assert!(missing.owner.is_none());
        assert_eq!(missing.visibility, "private");
    }

    /// Regression: the v1->v2 blob reorg must not strand a private agent's blobs when one agent's bare
    /// name equals another agent's owner segment. Agent `bob` (flat blobs/bob) and agent `proj` owned by
    /// user `bob` (proj's blobs move under blobs/bob/) share the top-level blobs/ space; an in-place
    /// rename could re-capture the just-created blobs/bob/ container as bob's source and strand proj.
    #[test]
    fn reorg_fs_does_not_strand_blobs_on_a_name_equals_owner_collision() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let blobs = root.join("blobs");
        std::fs::create_dir_all(blobs.join("bob")).unwrap();
        std::fs::write(blobs.join("bob").join("sha_bob"), b"bob-blob").unwrap();
        std::fs::create_dir_all(blobs.join("proj")).unwrap();
        std::fs::write(blobs.join("proj").join("sha_proj"), b"proj-blob").unwrap();
        // agent `bob` owned by `alice` (seg alice); agent `proj` owned by `bob` (seg bob).
        let mut map = std::collections::HashMap::new();
        map.insert("bob".to_string(), "alice".to_string());
        map.insert("proj".to_string(), "bob".to_string());

        super::reorg_fs(root, &map);

        // Both agents' blobs reachable at their scoped paths, whatever the map iteration order.
        assert_eq!(
            std::fs::read(blobs.join("alice").join("bob").join("sha_bob")).unwrap(),
            b"bob-blob",
            "bob's blob must land at blobs/alice/bob/"
        );
        assert_eq!(
            std::fs::read(blobs.join("bob").join("proj").join("sha_proj")).unwrap(),
            b"proj-blob",
            "proj's blob must land at blobs/bob/proj/ (not stranded under bob's move)"
        );
        assert!(!blobs.join(".migrating-v2").exists(), "the staging dir is cleaned up");
    }

    #[tokio::test]
    async fn v1_migration_rehomes_null_owner_and_moves_files_idempotently() {
        use sqlx::sqlite::SqliteConnectOptions;
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        ensure_root(root).unwrap();
        // Hand-build a v1 store: old agents schema (name PRIMARY KEY, nullable owner), version 1, a
        // null-owner "legacy" row, an alice-owned "frontend", and a v1 MR (endpoints have no owner).
        {
            let pool = sqlx::SqlitePool::connect_with(SqliteConnectOptions::new().filename(root.join("hub.db")).create_if_missing(true))
                .await
                .unwrap();
            sqlx::query("CREATE TABLE schema_version (id INTEGER PRIMARY KEY, version BIGINT NOT NULL)").execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO schema_version (id, version) VALUES (1, 1)").execute(&pool).await.unwrap();
            sqlx::query(
                "CREATE TABLE agents (name TEXT PRIMARY KEY, aid TEXT, owner TEXT, visibility TEXT NOT NULL DEFAULT 'private', \
                 lifecycle TEXT NOT NULL DEFAULT 'active', description TEXT, forked_from TEXT, forked_from_aid TEXT, aid_conflict TEXT, \
                 stars TEXT NOT NULL DEFAULT '[]', members TEXT NOT NULL DEFAULT '[]', created TEXT NOT NULL DEFAULT '')",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO agents (name, owner, visibility) VALUES ('legacy', NULL, 'private')").execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO agents (name, owner, visibility) VALUES ('frontend', 'alice', 'public')").execute(&pool).await.unwrap();
            sqlx::query("CREATE TABLE mrs (target_agent TEXT NOT NULL, id BIGINT NOT NULL, data TEXT NOT NULL, PRIMARY KEY (target_agent, id))")
                .execute(&pool)
                .await
                .unwrap();
            let mr = r#"{"id":1,"source":{"aid":null,"agent":"frontend","git_ref":"main"},"target":{"aid":null,"agent":"frontend","git_ref":"main"},"title":"t","author":"alice","state":"open","created":"2026-01-01T00:00:00Z"}"#;
            sqlx::query("INSERT INTO mrs (target_agent, id, data) VALUES ('frontend', 1, ?)").bind(mr).execute(&pool).await.unwrap();
            pool.close().await;
        }
        // Flat (v1) repo + blob layout on disk.
        std::fs::create_dir_all(root.join("legacy.git")).unwrap();
        std::fs::create_dir_all(root.join("frontend.git")).unwrap();
        std::fs::create_dir_all(root.join("blobs").join("frontend")).unwrap();
        std::fs::write(root.join("blobs").join("frontend").join("deadbeef"), b"x").unwrap();

        // Boot runs migrate_v2.
        let s = Store::open_sqlite(root).await.unwrap();

        // Rows: null owner → `_unclaimed`, others preserved and now addressed by segment.
        assert_eq!(s.agent_scoped("_unclaimed", "legacy").await.unwrap().owner.as_deref(), Some("_unclaimed"));
        assert!(s.agent_scoped("alice", "frontend").await.is_some());
        // Files re-homed under the namespace segment; the flat paths are gone.
        assert!(root.join("_unclaimed").join("legacy.git").is_dir(), "legacy repo re-homed to _unclaimed");
        assert!(root.join("alice").join("frontend.git").is_dir(), "owned repo re-homed under its owner");
        assert!(!root.join("legacy.git").exists() && !root.join("frontend.git").exists(), "flat repos gone");
        assert!(root.join("blobs").join("alice").join("frontend").join("deadbeef").exists(), "blob re-homed under owner");
        assert!(!root.join("blobs").join("frontend").join("deadbeef").exists(), "flat blob gone");
        // MR endpoints backfilled with the owner segment.
        let mrs = s.mrs_for("alice", "frontend").await;
        assert_eq!(mrs.len(), 1);
        assert_eq!(mrs[0].target.owner, "alice");
        assert_eq!(mrs[0].source.owner, "alice");

        // Idempotent: re-opening runs migrate() again but the version guard makes migrate_v2 a no-op.
        let s2 = Store::open_sqlite(root).await.unwrap();
        assert!(s2.agent_scoped("_unclaimed", "legacy").await.is_some());
        assert!(s2.agent_scoped("alice", "frontend").await.is_some());
        assert_eq!(s2.mrs_for("alice", "frontend").await.len(), 1);
    }

    #[test]
    fn new_agent_meta_defaults_to_private() {
        assert_eq!(AgentMeta::new("x", Some("alice"), Visibility::Private).visibility, "private");
        // The serde default must be private too — a hand-written record missing the field must not
        // amount to public.
        let m: AgentMeta = serde_json::from_str(r#"{"name":"x","hash":"y"}"#).unwrap();
        assert_eq!(m.visibility, "private");
        assert!(m.owner.is_none());
    }

    #[tokio::test]
    async fn a_token_row_with_no_owner_is_read_but_unusable() {
        // The old auth.json model: a token with no owner (the "one token = the whole host" era).
        // Recognized (so it can be reported) and its id backfilled from the digest, but unusable for
        // authentication — no permission is silently inherited.
        let (_d, s) = tmp_store().await;
        s.update_tokens(|t| {
            t.push(TokenRec {
                id: String::new(), // no id: must be backfilled from the digest on read
                name: "ci".into(),
                owner: None,
                agent: None,
                scope: "write".into(),
                hash: "deadbeefcafe0123".into(),
                created: now_iso(),
                expires: None,
                last_used: None,
            })
        })
        .await
        .unwrap();
        let toks = s.tokens().await;
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].scope, "write");
        assert_eq!(toks[0].id, "tok_deadbeefcafe", "with no id, backfill a stable one from the digest");
        assert!(toks[0].owner.is_none());
        assert!(!toks[0].usable(), "an ownerless token must be dead — that is exactly the old site-wide-pass model");
    }

    #[test]
    fn token_expiry() {
        let mk = |exp: Option<&str>| TokenRec {
            id: "tok_1".into(),
            name: "ci".into(),
            owner: Some("alice".into()),
            agent: None,
            scope: "read".into(),
            hash: "h".into(),
            created: now_iso(),
            expires: exp.map(|s| s.to_string()),
            last_used: None,
        };
        assert!(!mk(None).expired(), "no expiry written = never expires");
        assert!(mk(Some("2000-01-01T00:00:00Z")).expired());
        assert!(!mk(Some("2999-01-01T00:00:00Z")).expired());
        assert!(mk(Some("not a time")).expired(), "an unreadable timestamp counts as expired, not valid");
        assert!(mk(Some("2999-01-01T00:00:00Z")).usable());
        assert!(!mk(Some("2000-01-01T00:00:00Z")).usable());
    }

    #[tokio::test]
    async fn tokens_roundtrip_and_clear_replaces_content() {
        let (_d, s) = tmp_store().await;
        s.update_tokens(|t| {
            t.push(TokenRec {
                id: "tok_a".into(),
                name: "one".into(),
                owner: Some("alice".into()),
                agent: Some("x".into()),
                scope: "write".into(),
                hash: "h1".into(),
                created: now_iso(),
                expires: None,
                last_used: None,
            })
        })
        .await
        .unwrap();
        assert_eq!(s.tokens().await.len(), 1);
        // The atomic-replace semantics the old temp-file+rename gave, now the transaction gives.
        s.update_tokens(|t| t.clear()).await.unwrap();
        assert!(s.tokens().await.is_empty());
    }

    // ── aid: the identity, as opposed to the name ──

    #[tokio::test]
    async fn an_agent_resolves_by_aid() {
        let (_d, s) = tmp_store().await;
        s.update_agents(|a| {
            let mut m = AgentMeta::new("payments", Some("alice"), Visibility::Private);
            m.aid = Some("agt_pay".into());
            a.push(m);
            a.push(AgentMeta::new("other", Some("bob"), Visibility::Private));
        })
        .await
        .unwrap();
        assert_eq!(s.agent_by_aid("agt_pay").await.unwrap().name, "payments");
        assert!(s.agent_by_aid("agt_nope").await.is_none());
        assert!(s.agent_by_aid("").await.is_none(), "an agent with no aid cached must not match the empty string");
    }

    #[tokio::test]
    async fn a_rename_preserves_the_aid() {
        // The footgun this exists to close: a rename must not mint a new identity, or every
        // .agit.toml pinned to the old aid is orphaned by a cosmetic edit.
        let (_d, s) = tmp_store().await;
        s.update_agents(|a| {
            let mut m = AgentMeta::new("payments", Some("alice"), Visibility::Private);
            m.aid = Some("agt_pay".into());
            a.push(m);
        })
        .await
        .unwrap();
        s.update_agents(|a| a[0].name = "billing".into()).await.unwrap();
        assert_eq!(s.agent_scoped("alice", "billing").await.unwrap().aid.as_deref(), Some("agt_pay"));
        assert_eq!(s.agent_by_aid("agt_pay").await.unwrap().name, "billing", "by-aid follows the rename");
        assert!(s.agent_scoped("alice", "payments").await.is_none());
    }

    // ── merge requests ──

    fn mk_mr(id: usize, source: &str, target: &str) -> Mr {
        use super::super::mr::Endpoint;
        Mr {
            id,
            source: Endpoint { aid: Some("agt_src".into()), owner: "alice".into(), agent: source.into(), git_ref: "main".into() },
            target: Endpoint { aid: Some("agt_dst".into()), owner: "alice".into(), agent: target.into(), git_ref: "main".into() },
            title: "reconcile the payments memory".into(),
            author: "alice".into(),
            state: "open".into(),
            created: now_iso(),
            updated: String::new(),
            dialogue_transcript: Some("a: ...\nb: ...".into()),
            comments: vec![],
        }
    }

    #[tokio::test]
    async fn mrs_roundtrip_and_filter_by_target() {
        let (_d, s) = tmp_store().await;
        s.update_mrs(|m| {
            m.push(mk_mr(1, "fork", "payments"));
            m.push(mk_mr(2, "fork", "payments"));
            m.push(mk_mr(1, "x", "other"));
        })
        .await
        .unwrap();
        let pay = s.mrs_for("alice", "payments").await;
        assert_eq!(pay.len(), 2);
        assert_eq!(pay.iter().map(|m| m.id).collect::<Vec<_>>(), vec![1, 2], "oldest first");
        assert_eq!(pay[0].dialogue_transcript.as_deref(), Some("a: ...\nb: ..."));
        assert_eq!(s.mrs_for("alice", "other").await.len(), 1);
        assert!(s.mrs_for("alice", "nobody").await.is_empty());
    }

    #[tokio::test]
    async fn a_rename_carries_the_mrs_with_it() {
        // Otherwise one rename leaves every MR pointing at a name that no longer exists.
        let (_d, s) = tmp_store().await;
        s.update_mrs(|m| {
            m.push(mk_mr(1, "fork", "payments"));
            m.push(mk_mr(1, "payments", "other")); // payments as the *source* moves too
        })
        .await
        .unwrap();
        s.rename_in_mrs("alice", "payments", "billing").await.unwrap();
        assert_eq!(s.mrs_for("alice", "billing").await.len(), 1);
        assert!(s.mrs_for("alice", "payments").await.is_empty());
        assert_eq!(s.mrs_for("alice", "other").await[0].source.agent, "billing");
        // The identity is untouched by a label change.
        assert_eq!(s.mrs_for("alice", "billing").await[0].target.aid.as_deref(), Some("agt_dst"));
    }

    // ── per-row / per-column serde tolerance (the SQL analog of the JSON store's leniency) ──

    #[test]
    fn a_malformed_json_column_yields_an_empty_vec_not_a_panic() {
        // The mechanism behind the store's fail-safe read: a broken members/stars value loses only
        // itself, never the whole record.
        assert!(parse_json_vec::<Member>("{ not json").is_empty());
        assert!(parse_json_vec::<String>("").is_empty());
        assert_eq!(parse_json_vec::<String>(r#"["alice","bob"]"#), vec!["alice", "bob"]);
    }

    #[tokio::test]
    async fn a_row_with_a_broken_members_column_still_yields_an_agent() {
        // Plant a row whose members JSON will not parse; the agent must still read, with empty
        // members and a private (fail-safe) ACL — not vanish, and not panic.
        let (_d, s) = tmp_store().await;
        raw_exec(&s, "INSERT INTO agents (name, owner, members) VALUES ('good', 'alice', 'not json')").await;
        let m = s.agent_scoped("alice", "good").await.expect("the row must survive a broken JSON column");
        assert!(m.members.is_empty(), "a broken members column reads as no members");
        assert_eq!(m.to_acl().visibility, Visibility::Private);
    }

    #[tokio::test]
    async fn one_unparseable_mr_row_does_not_drop_the_rest() {
        // A single mrs.data that will not deserialize must lose only itself, mirroring the JSON
        // store's per-record tolerance.
        let (_d, s) = tmp_store().await;
        s.update_mrs(|m| m.push(mk_mr(1, "fork", "payments"))).await.unwrap();
        raw_exec(&s, "INSERT INTO mrs (target_owner, target_agent, id, data) VALUES ('alice', 'payments', 999, 'not json')").await;
        let pay = s.mrs_for("alice", "payments").await;
        assert_eq!(pay.len(), 1, "the good MR survives; the broken row is skipped");
        assert_eq!(pay[0].id, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_update_agents_do_not_lose_writes() {
        // The transaction now provides the serialization the old global LOCK did. Each update rewrites
        // the whole table (DELETE + re-INSERT); without a critical section per writer, concurrent
        // rewrites would clobber each other. Eight racing writers must all survive — the same guarantee
        // the reconcile TOCTOU (read + holder-lookup + write in one tx) leans on.
        //
        // SQLite serializes via the process-wide async write mutex; the Postgres path (untested live
        // here) serializes via one global pg_advisory_xact_lock, so this test's intent covers both.
        let (_d, s) = tmp_store().await;
        let mut handles = vec![];
        for i in 0..8 {
            let s = s.clone();
            handles.push(tokio::spawn(async move {
                s.update_agents(move |list| {
                    list.push(AgentMeta::new(&format!("a{i}"), Some("alice"), Visibility::Private));
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(s.agents().await.len(), 8, "every concurrent writer's row must survive; the tx replaces the old LOCK");
    }

    // ── organizations ──

    fn org_member(username: &str, role: &str) -> OrgMember {
        OrgMember { username: username.into(), role: role.into() }
    }

    #[tokio::test]
    async fn orgs_roundtrip_through_db() {
        let (_d, s) = tmp_store().await;
        s.update_orgs(|orgs| {
            orgs.push(Org {
                name: "acme".into(),
                members: vec![org_member("bob", "admin"), org_member("carol", "member")],
                created: now_iso(),
                current_kek_gen: 0,
                recovery_x25519: String::new(),
                escrow_mode: "none".into(),
                members_can_create: 1,
            });
        })
        .await
        .unwrap();
        let o = s.org("acme").await.unwrap();
        assert_eq!(o.members.len(), 2);
        assert!(o.is_admin("bob"));
        assert!(o.is_member("carol"));
        assert!(!o.is_admin("carol"));
        // Lookup is case-insensitive, like user().
        assert!(s.org("ACME").await.is_some());
        assert_eq!(s.orgs().await.len(), 1);
    }

    fn kek_row(org: &str, gen: i64, recipient: &str, wrapped: &str, epoch: i64) -> TeamKekEnvelope {
        TeamKekEnvelope {
            org: org.into(),
            gen,
            recipient: recipient.into(),
            wrapped_kek: wrapped.into(),
            recipient_epoch: epoch,
            created: now_iso(),
        }
    }

    #[tokio::test]
    async fn team_keks_roundtrip_and_upsert_is_idempotent() {
        let (_d, s) = tmp_store().await;
        s.upsert_team_kek_envelopes(
            "acme",
            1,
            &[kek_row("acme", 1, "alice", "seal-a1", 3), kek_row("acme", 1, "bob", "seal-b1", 0)],
        )
        .await
        .unwrap();
        let a = s.get_team_kek_envelope("acme", 1, "alice").await.unwrap();
        assert_eq!(a.wrapped_kek, "seal-a1");
        assert_eq!(a.recipient_epoch, 3);
        assert_eq!(s.get_team_kek_envelope("acme", 1, "bob").await.unwrap().wrapped_kek, "seal-b1");
        // A recipient with no row, and a gen with no rows, are both None (non-disclosing).
        assert!(s.get_team_kek_envelope("acme", 1, "carol").await.is_none());
        assert!(s.get_team_kek_envelope("acme", 2, "alice").await.is_none());
        // Case-insensitive recipient lookup, like every other username.
        assert!(s.get_team_kek_envelope("ACME", 1, "ALICE").await.is_some());

        // Re-publishing overwrites the ciphertext for the same (org, gen, recipient) PK, never duplicates.
        s.upsert_team_kek_envelopes("acme", 1, &[kek_row("acme", 1, "alice", "seal-a1-v2", 4)]).await.unwrap();
        let a2 = s.get_team_kek_envelope("acme", 1, "alice").await.unwrap();
        assert_eq!(a2.wrapped_kek, "seal-a1-v2");
        assert_eq!(a2.recipient_epoch, 4);

        // A second generation coexists; list_team_kek_gens reports both, ascending.
        s.upsert_team_kek_envelopes("acme", 2, &[kek_row("acme", 2, "alice", "seal-a2", 4)]).await.unwrap();
        assert_eq!(s.list_team_kek_gens("acme").await, vec![1, 2]);
        assert!(s.list_team_kek_gens("nope").await.is_empty());
    }

    /// Wave-5 store guards. A fresh org reads back with the escape hatches OFF (recovery unset, escrow
    /// `none`) — byte-for-byte the wave-1..4 default — and both survive the whole-table `update_orgs`
    /// snapshot rewrite once set. Escrow keys round-trip on their (owner, name, kid) PK, idempotently.
    #[tokio::test]
    async fn wave5_org_escape_hatches_default_off_and_persist() {
        let (_d, s) = tmp_store().await;
        s.update_orgs(|l| l.push(Org { name: "acme".into(), members: vec![org_member("alice", "admin")], created: now_iso(), current_kek_gen: 0, recovery_x25519: String::new(), escrow_mode: "none".into(), members_can_create: 1 }))
            .await
            .unwrap();
        // Default OFF.
        let o = s.org("acme").await.unwrap();
        assert_eq!(o.recovery_x25519, "", "recovery is unset by default");
        assert_eq!(o.escrow_mode, "none", "escrow is off by default");

        // Set both, then prove they survive an unrelated member edit (the snapshot-rewrite hazard).
        s.update_orgs(|l| {
            let o = l.iter_mut().find(|o| o.name == "acme").unwrap();
            o.recovery_x25519 = "ab".repeat(32);
            o.escrow_mode = "hub-assist".into();
        })
        .await
        .unwrap();
        s.update_orgs(|l| l.iter_mut().find(|o| o.name == "acme").unwrap().members.push(org_member("bob", "member")))
            .await
            .unwrap();
        let o = s.org("acme").await.unwrap();
        assert_eq!(o.recovery_x25519, "ab".repeat(32), "recovery survives a member edit");
        assert_eq!(o.escrow_mode, "hub-assist", "escrow mode survives a member edit");
    }

    #[tokio::test]
    async fn wave5_escrow_keys_roundtrip_and_upsert_is_idempotent() {
        let (_d, s) = tmp_store().await;
        assert!(s.get_escrow_keys("acme", "frontend").await.is_empty(), "no escrow keys by default");
        let mk = |kid: i64, w: &str| EscrowKey { owner: "acme".into(), name: "frontend".into(), kid, wrapped_ck: w.into(), created: now_iso() };
        s.upsert_escrow_key(&mk(0, "seal-0")).await.unwrap();
        s.upsert_escrow_key(&mk(1, "seal-1")).await.unwrap();
        let rows = s.get_escrow_keys("acme", "frontend").await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kid, 0);
        assert_eq!(rows[0].wrapped_ck, "seal-0");
        assert_eq!(rows[1].wrapped_ck, "seal-1");
        // Re-escrowing the same kid overwrites, never duplicates.
        s.upsert_escrow_key(&mk(0, "seal-0-v2")).await.unwrap();
        let rows = s.get_escrow_keys("acme", "frontend").await;
        assert_eq!(rows.len(), 2, "same (owner,name,kid) is an upsert, not a new row");
        assert_eq!(rows[0].wrapped_ck, "seal-0-v2");
        // A different agent has its own, separate rows.
        assert!(s.get_escrow_keys("acme", "backend").await.is_empty());
    }

    #[tokio::test]
    async fn current_kek_gen_bumps_monotonically_and_survives_org_edits() {
        let (_d, s) = tmp_store().await;
        s.update_orgs(|l| l.push(Org { name: "acme".into(), members: vec![org_member("alice", "admin")], created: now_iso(), current_kek_gen: 0, recovery_x25519: String::new(), escrow_mode: "none".into(), members_can_create: 1 }))
            .await
            .unwrap();
        assert_eq!(s.get_current_kek_gen("acme").await, 0);
        s.set_current_kek_gen("acme", 1).await.unwrap();
        assert_eq!(s.get_current_kek_gen("acme").await, 1);
        s.set_current_kek_gen("acme", 2).await.unwrap();
        assert_eq!(s.get_current_kek_gen("acme").await, 2);

        // SQL-level monotonicity: a stale (lower) generation must NOT roll the current back, and an
        // equal generation is an idempotent no-op — the guard defends even a caller that skips the API check.
        s.set_current_kek_gen("acme", 1).await.unwrap();
        assert_eq!(s.get_current_kek_gen("acme").await, 2, "a lower gen must not roll back the current");
        s.set_current_kek_gen("acme", 2).await.unwrap();
        assert_eq!(s.get_current_kek_gen("acme").await, 2, "an equal gen is a no-op");

        // The regression this guards: an unrelated whole-table org rewrite (adding a member) must NOT
        // reset current_kek_gen to its DEFAULT 0 — the generation is carried on the struct and re-INSERTed.
        s.update_orgs(|l| {
            if let Some(o) = l.iter_mut().find(|o| o.name == "acme") {
                o.members.push(org_member("bob", "member"));
            }
        })
        .await
        .unwrap();
        assert_eq!(s.get_current_kek_gen("acme").await, 2, "a member edit must preserve the KEK generation");
        assert_eq!(s.org("acme").await.unwrap().members.len(), 2);
    }

    #[tokio::test]
    async fn broken_org_members_column_still_yields_org() {
        // A members JSON that will not parse loses only itself (empty members), never the whole row —
        // the same per-record tolerance agents get.
        let (_d, s) = tmp_store().await;
        raw_exec(&s, "INSERT INTO orgs (name, members) VALUES ('acme', 'not json')").await;
        let o = s.org("acme").await.expect("the row must survive a broken JSON column");
        assert!(o.members.is_empty(), "a broken members column reads as no members");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_update_orgs_do_not_lose_writes() {
        // Same guarantee as concurrent_update_agents: the write_lock / advisory_lock serializes the
        // whole DELETE+re-INSERT snapshot, so eight racing pushes all survive.
        let (_d, s) = tmp_store().await;
        let mut handles = vec![];
        for i in 0..8 {
            let s = s.clone();
            handles.push(tokio::spawn(async move {
                s.update_orgs(move |list| {
                    list.push(Org { name: format!("org{i}"), members: vec![], created: now_iso(), current_kek_gen: 0, recovery_x25519: String::new(), escrow_mode: "none".into(), members_can_create: 1 });
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(s.orgs().await.len(), 8, "every concurrent org writer's row must survive");
    }

    #[test]
    fn to_acl_with_org_folds_and_dedupes_max() {
        // Agent owned by org:acme, with an explicit per-agent member carol=read. Folding org admin bob
        // and org member carol must yield (bob, Admin) and RAISE carol to Write (the higher role wins),
        // and must never lower a pre-existing explicit admin.
        let org = Org {
            name: "acme".into(),
            members: vec![org_member("bob", "admin"), org_member("carol", "member"), org_member("mallory", "bogus")],
            created: now_iso(),
            current_kek_gen: 0,
            recovery_x25519: String::new(),
            escrow_mode: "none".into(),
            members_can_create: 1,
        };
        let m = AgentMeta {
            members: vec![Member { username: "carol".into(), role: "read".into() }, Member { username: "dave".into(), role: "admin".into() }],
            ..AgentMeta::new("shared", Some("org:acme"), Visibility::Private)
        };
        let acl = m.to_acl_with_org(Some(&org));
        let role_of = |u: &str| acl.members.iter().find(|(n, _)| n == u).map(|(_, r)| *r);
        assert_eq!(role_of("bob"), Some(Role::Admin), "org admin folds to Admin");
        assert_eq!(role_of("carol"), Some(Role::Write), "org member folds to Write, raising the explicit read");
        assert_eq!(role_of("dave"), Some(Role::Admin), "a pre-existing explicit admin is not lowered");
        assert_eq!(role_of("mallory"), None, "a junk org role folds to nothing (fail-safe)");
    }

    #[test]
    fn to_acl_unchanged_without_org() {
        // The no-org path must be byte-for-byte to_acl(), so every existing decide test still holds.
        let m = AgentMeta {
            members: vec![Member { username: "bob".into(), role: "write".into() }],
            ..AgentMeta::new("x", Some("alice"), Visibility::Public)
        };
        let a = m.to_acl();
        let b = m.to_acl_with_org(None);
        assert_eq!(a.owner, b.owner);
        assert_eq!(a.visibility, b.visibility);
        assert_eq!(a.members, b.members);
    }

    /// Build a device-key row. `created` is passed explicitly so tests can order the primary-key
    /// tiebreak deterministically (now_iso has only second resolution).
    fn ik(user: &str, ed: &str, email: &str, epoch: i64, created: &str) -> IdentityKey {
        IdentityKey {
            username: user.into(),
            key_fpr: String::new(), // the facade derives it from ed25519_pub
            ed25519_pub: ed.into(),
            x25519_pub: "b".repeat(64),
            label: String::new(),
            epoch,
            enroll_sig: "sig".into(),
            created: created.into(),
            revoked: None,
            email: email.into(),
        }
    }

    /// Per-DEVICE-KEY monotonicity: re-enrolling the SAME device key (same ed → same key_fpr) refuses a
    /// non-advancing epoch and preserves `created`; a strictly higher epoch replaces that key's row. A
    /// DIFFERENT key is a separate row, never a monotonic conflict.
    #[tokio::test]
    async fn identity_add_is_monotonic_per_key_and_preserves_created() {
        let (_d, s) = tmp_store().await;
        let k = "a".repeat(64);
        let first = ik("Alice", &k, "", 0, &now_iso()); // mixed-case: the facade normalizes it
        assert_eq!(s.add_identity_key(first).await.unwrap(), EnrollOutcome::Applied);
        let stored0 = s.get_primary_identity_key("alice").await.unwrap();
        assert_eq!(stored0.username, "alice");
        assert!(!stored0.key_fpr.is_empty(), "the facade filled in a fingerprint");
        let created0 = stored0.created.clone();

        // A non-advancing epoch for the SAME key is refused and changes nothing.
        assert_eq!(
            s.add_identity_key(ik("alice", &k, "", 0, &now_iso())).await.unwrap(),
            EnrollOutcome::StaleEpoch { stored: 0 }
        );
        // A strictly higher epoch for the SAME key replaces that key's row, keeping the original `created`.
        assert_eq!(s.add_identity_key(ik("alice", &k, "", 1, &now_iso())).await.unwrap(), EnrollOutcome::Applied);
        let bumped = s.get_primary_identity_key("alice").await.unwrap();
        assert_eq!(bumped.epoch, 1);
        assert_eq!(bumped.created, created0, "created is preserved across a same-key replace");
        assert_eq!(s.list_identity_keys("alice").await.len(), 1, "a same-key rotation is still ONE device key");

        // Batch primary get returns only known users; an unknown one is omitted, not padded.
        let batch = s.get_identity_keys(&["alice".into(), "nobody".into()]).await;
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].username, "alice");
        assert!(s.get_primary_identity_key("nobody").await.is_none());
    }

    /// Enrolling TWO different device keys for one account registers BOTH (neither overwrites the other) —
    /// the whole point of the SSH-keys reshape. The primary is the latest non-revoked key.
    #[tokio::test]
    async fn enrolling_two_keys_registers_both() {
        let (_d, s) = tmp_store().await;
        let laptop = "a".repeat(64);
        let desktop = "c".repeat(64);
        s.add_identity_key(ik("alice", &laptop, "", 0, "2026-01-01T00:00:00Z")).await.unwrap();
        s.add_identity_key(ik("alice", &desktop, "", 0, "2026-02-01T00:00:00Z")).await.unwrap();

        let keys = s.list_identity_keys("alice").await;
        assert_eq!(keys.len(), 2, "both device keys are registered; neither clobbers the other");
        let eds: std::collections::HashSet<_> = keys.iter().map(|k| k.ed25519_pub.clone()).collect();
        assert!(eds.contains(&laptop) && eds.contains(&desktop));
        // The primary is the LATEST non-revoked key (newest `created`).
        assert_eq!(s.get_primary_identity_key("alice").await.unwrap().ed25519_pub, desktop);
    }

    /// `get_primary_identity_key` returns the latest NON-REVOKED key: revoking the newest falls back to the
    /// next, and revoking all of them yields None.
    #[tokio::test]
    async fn get_primary_identity_key_returns_latest_nonrevoked() {
        let (_d, s) = tmp_store().await;
        let old = "a".repeat(64);
        let new = "c".repeat(64);
        s.add_identity_key(ik("alice", &old, "", 0, "2026-01-01T00:00:00Z")).await.unwrap();
        s.add_identity_key(ik("alice", &new, "", 0, "2026-02-01T00:00:00Z")).await.unwrap();
        assert_eq!(s.get_primary_identity_key("alice").await.unwrap().ed25519_pub, new);
        // Revoke the newest → the primary falls back to the older key.
        let new_fpr = ed25519_fingerprint(&new);
        assert!(s.revoke_identity_key("alice", &new_fpr).await.unwrap());
        assert_eq!(s.get_primary_identity_key("alice").await.unwrap().ed25519_pub, old);
        // Revoke the last one → no primary at all.
        assert!(s.revoke_identity_key("alice", &ed25519_fingerprint(&old)).await.unwrap());
        assert!(s.get_primary_identity_key("alice").await.is_none());
    }

    /// A user cannot revoke ANOTHER user's key: revoke is scoped to `(caller, key_fpr)`, so bob naming
    /// alice's fingerprint tombstones nothing.
    #[tokio::test]
    async fn revoke_is_caller_scoped_and_cannot_touch_another_users_key() {
        let (_d, s) = tmp_store().await;
        let alice_key = "a".repeat(64);
        s.add_identity_key(ik("alice", &alice_key, "", 0, &now_iso())).await.unwrap();
        let fpr = ed25519_fingerprint(&alice_key);
        // bob tries to revoke alice's key by fingerprint — nothing matches HIS rows.
        assert!(!s.revoke_identity_key("bob", &fpr).await.unwrap(), "a non-owner revokes nothing");
        assert_eq!(s.list_identity_keys("alice").await.len(), 1, "alice's key is untouched");
        // alice can revoke her own.
        assert!(s.revoke_identity_key("alice", &fpr).await.unwrap());
        assert!(s.list_identity_keys("alice").await.is_empty());
    }

    #[tokio::test]
    async fn identity_lookup_by_email_maps_committer_to_all_account_keys() {
        let (_d, s) = tmp_store().await;
        // Accounts must exist AND have a VERIFIED email — attribution is gated on verification (the
        // anti-squatting property), so an enrolled-but-unverified email maps to nothing.
        for u in ["alice", "bob"] {
            add_named_user(&s, u).await;
        }
        // alice enrolls TWO device keys under the same committer email.
        s.add_identity_key(ik("alice", &"a".repeat(64), "Alice@Corp.com", 0, "2026-01-01T00:00:00Z")).await.unwrap();
        s.add_identity_key(ik("alice", &"e".repeat(64), "Alice@Corp.com", 0, "2026-02-01T00:00:00Z")).await.unwrap();
        s.add_identity_key(ik("bob", &"b".repeat(64), "bob@corp.com", 0, &now_iso())).await.unwrap();

        // Before verification, even an unambiguous match resolves to nothing (the squatting defense).
        assert!(
            s.get_identity_keys_by_email("alice@corp.com").await.is_empty(),
            "an UNVERIFIED email is not attributable"
        );
        s.set_email_verified("alice", true).await.unwrap();
        s.set_email_verified("bob", true).await.unwrap();

        // The committer email resolves to ALL of alice's non-revoked device keys; match is case/space-insensitive.
        let hits = s.get_identity_keys_by_email("  alice@corp.com ").await;
        assert_eq!(hits.len(), 2, "both of alice's device keys are returned");
        assert!(hits.iter().all(|k| k.username == "alice"));
        let eds: std::collections::HashSet<_> = hits.iter().map(|k| k.ed25519_pub.clone()).collect();
        assert!(eds.contains(&"a".repeat(64)) && eds.contains(&"e".repeat(64)));

        // Revoking one key drops it from the set but keeps the account attributable via the rest.
        assert!(s.revoke_identity_key("alice", &ed25519_fingerprint(&"a".repeat(64))).await.unwrap());
        let after = s.get_identity_keys_by_email("alice@corp.com").await;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].ed25519_pub, "e".repeat(64));

        // An email nobody enrolled, and a blank email, both map to nothing.
        assert!(s.get_identity_keys_by_email("ghost@corp.com").await.is_empty());
        assert!(s.get_identity_keys_by_email("").await.is_empty());

        // Ambiguity: two accounts claiming one email is not attributable — even when both are verified.
        for u in ["carol", "dave"] {
            add_named_user(&s, u).await;
            s.set_email_verified(u, true).await.unwrap();
        }
        s.add_identity_key(ik("carol", &"c".repeat(64), "shared@corp.com", 0, &now_iso())).await.unwrap();
        s.add_identity_key(ik("dave", &"d".repeat(64), "shared@corp.com", 0, &now_iso())).await.unwrap();
        assert!(s.get_identity_keys_by_email("shared@corp.com").await.is_empty(), "an ambiguous email is not a hit");
    }

    #[tokio::test]
    async fn a_newly_created_account_is_unverified() {
        let (_d, s) = tmp_store().await;
        add_named_user(&s, "alice").await;
        assert!(!s.user("alice").await.unwrap().email_verified, "a fresh account starts UNVERIFIED");
        // set_email_verified flips it, and reports whether the user existed.
        assert!(s.set_email_verified("alice", true).await.unwrap());
        assert!(s.user("alice").await.unwrap().email_verified);
        assert!(s.set_email_verified("alice", false).await.unwrap());
        assert!(!s.user("alice").await.unwrap().email_verified);
        // A missing user reports false rather than silently creating one.
        assert!(!s.set_email_verified("ghost", true).await.unwrap());
    }

    #[tokio::test]
    async fn email_token_is_single_use_and_expiring() {
        let (_d, s) = tmp_store().await;
        // A valid token consumes exactly once and yields the (username, email) it proved.
        let token = s.mint_email_token("Alice", "Alice@Corp.com", Duration::from_secs(3600)).await.unwrap();
        assert!(token.starts_with("evt_"), "token is a legible CSPRNG capability");
        let got = s.consume_email_token(&token).await.expect("a valid token consumes");
        assert_eq!(got, ("alice".to_string(), "alice@corp.com".to_string()), "username + email are normalized");
        // Single-use: a second consume of the same token is None (the row was deleted on first use).
        assert!(s.consume_email_token(&token).await.is_none(), "a token is single-use");
        // An unknown/garbage token is None, not an error.
        assert!(s.consume_email_token("evt_nope").await.is_none());
        assert!(s.consume_email_token("").await.is_none());

        // An already-expired token (ttl 0) is refused even on its first use.
        let expired = s.mint_email_token("bob", "bob@corp.com", Duration::from_secs(0)).await.unwrap();
        assert!(s.consume_email_token(&expired).await.is_none(), "an expired token is never accepted");
        // ...and it was still cleaned up, so a retry is also None.
        assert!(s.consume_email_token(&expired).await.is_none());
    }

    #[tokio::test]
    async fn password_reset_token_is_single_use_and_expiring() {
        let (_d, s) = tmp_store().await;
        // A valid token consumes exactly once and yields the (normalized) username it authorizes.
        let token = s.mint_password_reset_token("Alice", Duration::from_secs(3600)).await.unwrap();
        assert!(token.starts_with("prt_"), "reset token carries the distinct prt_ prefix");
        assert_eq!(s.consume_password_reset_token(&token).await.as_deref(), Some("alice"), "username is normalized");
        // Single-use: a second consume of the same token is None (the row was deleted on first use).
        assert!(s.consume_password_reset_token(&token).await.is_none(), "a reset token is single-use");
        // Unknown/garbage tokens are None, not an error.
        assert!(s.consume_password_reset_token("prt_nope").await.is_none());
        assert!(s.consume_password_reset_token("").await.is_none());
        // An already-expired token (ttl 0) is refused even on its first use, and cleaned up.
        let expired = s.mint_password_reset_token("bob", Duration::from_secs(0)).await.unwrap();
        assert!(s.consume_password_reset_token(&expired).await.is_none(), "an expired reset token is never accepted");
        assert!(s.consume_password_reset_token(&expired).await.is_none());
    }

    #[tokio::test]
    async fn consuming_a_reset_revokes_all_the_users_outstanding_reset_tokens() {
        // A completed reset must invalidate EVERY outstanding reset link for that user, not just the one
        // used, so an older leaked reset link cannot be replayed after the user has already reset (#2).
        let (_d, s) = tmp_store().await;
        let t1 = s.mint_password_reset_token("alice", Duration::from_secs(3600)).await.unwrap();
        let t2 = s.mint_password_reset_token("alice", Duration::from_secs(3600)).await.unwrap();
        let bob = s.mint_password_reset_token("bob", Duration::from_secs(3600)).await.unwrap();
        assert_eq!(s.password_reset_token_count().await, 3);
        // Consuming alice's first token authorizes alice AND revokes her sibling t2.
        assert_eq!(s.consume_password_reset_token(&t1).await.as_deref(), Some("alice"));
        assert!(s.consume_password_reset_token(&t2).await.is_none(), "a sibling reset token is revoked once one is consumed");
        // Another user's token is untouched: only the consuming user's reset tokens are revoked.
        assert_eq!(s.consume_password_reset_token(&bob).await.as_deref(), Some("bob"), "another user's reset token is unaffected");
    }

    #[tokio::test]
    async fn reset_and_verify_token_spaces_never_cross() {
        // The core separation invariant: the two token spaces are DISJOINT tables. A verification token
        // can never be consumed as a reset token, and a reset token can never be consumed as a
        // verification token — so leaking one can never grant the other's authority.
        let (_d, s) = tmp_store().await;
        let evt = s.mint_email_token("alice", "alice@corp.com", Duration::from_secs(3600)).await.unwrap();
        let prt = s.mint_password_reset_token("alice", Duration::from_secs(3600)).await.unwrap();
        // Cross-consume: each is rejected by the OTHER space's consumer.
        assert!(s.consume_password_reset_token(&evt).await.is_none(), "an email-verify token is NOT a reset token");
        assert!(s.consume_email_token(&prt).await.is_none(), "a reset token is NOT an email-verify token");
        // A failed cross-consume must not have burned the token in its own space: each still works there.
        assert!(s.consume_email_token(&evt).await.is_some(), "the verify token still consumes in its own space");
        assert_eq!(s.consume_password_reset_token(&prt).await.as_deref(), Some("alice"), "the reset token still consumes in its own space");
    }

    #[tokio::test]
    async fn by_email_attributes_only_after_verification() {
        // The end-to-end anti-squatting property at the store layer: enroll an email, and it is NOT
        // attributable until the account verifies. A token consume marks it verified.
        let (_d, s) = tmp_store().await;
        add_named_user(&s, "alice").await;
        s.add_identity_key(ik("alice", &"a".repeat(64), "alice@corp.com", 0, &now_iso())).await.unwrap();
        // Unverified: an exact unique match resolves to no identity.
        assert!(s.get_identity_keys_by_email("alice@corp.com").await.is_empty());
        // Verify by minting + consuming a token, then flipping the flag (the api layer does this pairing).
        let token = s.mint_email_token("alice", "alice@corp.com", Duration::from_secs(3600)).await.unwrap();
        let (user, _email) = s.consume_email_token(&token).await.unwrap();
        s.set_email_verified(&user, true).await.unwrap();
        // Now it attributes.
        let hits = s.get_identity_keys_by_email("alice@corp.com").await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].username, "alice");
        // Revoking verification closes the door again.
        s.set_email_verified("alice", false).await.unwrap();
        assert!(s.get_identity_keys_by_email("alice@corp.com").await.is_empty());
    }

    #[tokio::test]
    async fn set_user_disabled_toggles_and_persists() {
        let (_d, s) = tmp_store().await;
        add_named_user(&s, "alice").await;
        // A fresh account is enabled (disabled == false).
        assert!(!s.user("alice").await.unwrap().disabled);
        // Disable it, and the flag round-trips through the DELETE + re-INSERT snapshot rewrite.
        assert!(s.set_user_disabled("alice", true).await.unwrap());
        assert!(s.user("alice").await.unwrap().disabled);
        // Enable restores it; case-insensitive lookup addresses the same account.
        assert!(s.set_user_disabled("ALICE", false).await.unwrap());
        assert!(!s.user("alice").await.unwrap().disabled);
        // An unknown user reports false (no such row), and nothing is created.
        assert!(!s.set_user_disabled("ghost", true).await.unwrap());
        assert!(s.user("ghost").await.is_none());
    }

    #[tokio::test]
    async fn disabled_column_backfills_false() {
        // Upgrade path: a users table that predates the `disabled` column. Re-running migrate must add it
        // (idempotent ADD COLUMN IF NOT EXISTS / duplicate-ignored on SQLite) and every pre-existing row
        // must read back disabled=false — the safe default that leaves old accounts able to log in.
        let (_d, s) = tmp_store().await;
        add_named_user(&s, "olduser").await;
        raw_exec(&s, "ALTER TABLE users DROP COLUMN disabled").await;
        if let Store::Sqlite(inner) = &s {
            inner.migrate().await.expect("migrate must back-fill the disabled column");
        }
        // The row predating the column reads back ENABLED, and the flag is writable again post-migration.
        assert!(!s.user("olduser").await.unwrap().disabled, "a pre-column row reads disabled=false");
        assert!(s.set_user_disabled("olduser", true).await.unwrap());
        assert!(s.user("olduser").await.unwrap().disabled);
    }

    #[tokio::test]
    async fn migrate_recovers_an_identity_keys_that_predates_the_email_column() {
        // Upgrade path: a registry has identity_keys WITHOUT the provenance `email` column and no email
        // index. Re-running migrate must NOT fail on "no such column: email" (the index-before-back-fill
        // bug this test guards) and must restore both the column and its index.
        let (_d, s) = tmp_store().await;
        raw_exec(&s, "DROP INDEX IF EXISTS identity_keys_email").await;
        raw_exec(&s, "ALTER TABLE identity_keys DROP COLUMN email").await;
        if let Store::Sqlite(inner) = &s {
            inner
                .migrate()
                .await
                .expect("migrate must recover a registry that predates the email column");
        }
        // The column (and its lookup) are restored — an enroll with a VERIFIED email round-trips.
        add_named_user(&s, "erin").await;
        s.set_email_verified("erin", true).await.unwrap();
        s.add_identity_key(ik("erin", &"e".repeat(64), "erin@corp.com", 0, &now_iso())).await.unwrap();
        let hits = s.get_identity_keys_by_email("erin@corp.com").await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].username, "erin");
    }

    /// The SSH-keys reshape migration: a LEGACY single-key `identity_keys` (`PRIMARY KEY(username)`, no
    /// `key_fpr`/`label`) with an existing row is rebuilt into the multi-key composite shape at boot —
    /// key_fpr back-filled from the row's ed25519_pub, label 'default' — and the pre-reshape enrollment
    /// still verifies (resolves by email, and the fingerprint is stable/derivable).
    #[tokio::test]
    async fn migrate_reshapes_a_legacy_single_key_table() {
        let (_d, s) = tmp_store().await;
        add_named_user(&s, "frank").await;
        s.set_email_verified("frank", true).await.unwrap();
        // Hand-build the OLD single-key table shape and plant a pre-reshape row in it. The `email` column
        // is present (this store predates only the multi-key reshape, not email verification).
        raw_exec(&s, "DROP INDEX IF EXISTS identity_keys_email").await;
        raw_exec(&s, "DROP TABLE identity_keys").await;
        raw_exec(
            &s,
            "CREATE TABLE identity_keys (\
               username TEXT PRIMARY KEY, ed25519_pub TEXT NOT NULL, x25519_pub TEXT NOT NULL, \
               epoch BIGINT NOT NULL DEFAULT 0, enroll_sig TEXT NOT NULL, created TEXT NOT NULL DEFAULT '', \
               revoked TEXT, email TEXT NOT NULL DEFAULT '')",
        )
        .await;
        let ed = "f".repeat(64);
        raw_exec(
            &s,
            &format!(
                "INSERT INTO identity_keys (username, ed25519_pub, x25519_pub, epoch, enroll_sig, created, email) \
                 VALUES ('frank', '{ed}', '{}', 2, 'sig', '2026-01-01T00:00:00Z', 'frank@corp.com')",
                "b".repeat(64)
            ),
        )
        .await;

        // Re-run migrate: it must reshape the legacy table into the composite shape without losing the row.
        if let Store::Sqlite(inner) = &s {
            inner.migrate().await.expect("migrate must reshape a legacy single-key identity_keys");
        }

        // The pre-reshape enrollment survives as one device key: key_fpr back-filled, label 'default'.
        let keys = s.list_identity_keys("frank").await;
        assert_eq!(keys.len(), 1, "the legacy row becomes exactly one device key");
        assert_eq!(keys[0].ed25519_pub, ed);
        assert_eq!(keys[0].key_fpr, ed25519_fingerprint(&ed), "key_fpr is derived from the row's ed25519_pub");
        assert_eq!(keys[0].label, "default", "a back-filled legacy row is labelled 'default'");
        assert_eq!(keys[0].epoch, 2, "the rotation epoch is preserved");
        // It still verifies by email (the whole point — a pre-reshape row is not de-attributed).
        let hits = s.get_identity_keys_by_email("frank@corp.com").await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].username, "frank");
        // And the reshaped table now supports a SECOND device key for the same account.
        s.add_identity_key(ik("frank", &"a".repeat(64), "frank@corp.com", 0, "2026-03-01T00:00:00Z")).await.unwrap();
        assert_eq!(s.list_identity_keys("frank").await.len(), 2, "the reshaped table is truly multi-key");
    }

    #[test]
    fn org_owner_parse() {
        let org_owned = AgentMeta { owner: Some("org:acme".into()), ..AgentMeta::new("x", None, Visibility::Private) };
        assert_eq!(org_owned.org_owner(), Some("acme"));
        let user_owned = AgentMeta::new("x", Some("alice"), Visibility::Private);
        assert_eq!(user_owned.org_owner(), None);
        let unowned = AgentMeta::new("x", None, Visibility::Private);
        assert_eq!(unowned.org_owner(), None);
        // The unforgeability argument: ':' is not a valid username char, so "org:x" can never equal a
        // real username — org access can never arrive through decide's owner branch.
        assert!(!valid_username("org:acme"));
        assert!(!valid_username(":"));
    }
}
