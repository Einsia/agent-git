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
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn is_expired(iso: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(t) => chrono::Utc::now() >= t.with_timezone(&chrono::Utc),
        // An unreadable timestamp = do not dare treat it as valid. Failure errs toward "expired".
        Err(_) => true,
    }
}

// ─────────────────────────── users ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// Username rules: lowercase [a-z0-9._-], 2..=32, no leading dot. Login names are case-insensitive →
/// normalize before storing, or "Alice" and "alice" become two accounts that can impersonate each
/// other.
pub fn valid_username(name: &str) -> bool {
    let n = name.len();
    (2..=32).contains(&n)
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn normalize_username(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

// ─────────────────────────── agent metadata ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub username: String,
    /// "read" | "write" | "admin"
    pub role: String,
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

fn row_user(r: &impl Cols) -> User {
    User {
        username: r.text("username"),
        pw_hash: r.text("pw_hash"),
        salt: r.text("salt"),
        kdf: r.text("kdf"),
        is_admin: r.int("is_admin") != 0,
        created: r.text("created"),
    }
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
    "CREATE TABLE IF NOT EXISTS users (\
       username TEXT PRIMARY KEY, pw_hash TEXT NOT NULL, salt TEXT NOT NULL, \
       kdf TEXT NOT NULL, is_admin BIGINT NOT NULL DEFAULT 0, created TEXT NOT NULL DEFAULT '')",
    "CREATE TABLE IF NOT EXISTS agents (\
       name TEXT PRIMARY KEY, aid TEXT, owner TEXT, \
       visibility TEXT NOT NULL DEFAULT 'private', lifecycle TEXT NOT NULL DEFAULT 'active', \
       description TEXT, forked_from TEXT, forked_from_aid TEXT, aid_conflict TEXT, \
       stars TEXT NOT NULL DEFAULT '[]', members TEXT NOT NULL DEFAULT '[]', \
       created TEXT NOT NULL DEFAULT '')",
    "CREATE INDEX IF NOT EXISTS agents_aid ON agents(aid)",
    "CREATE TABLE IF NOT EXISTS tokens (\
       id TEXT PRIMARY KEY, name TEXT NOT NULL, owner TEXT, agent TEXT, \
       scope TEXT NOT NULL, hash TEXT NOT NULL, created TEXT NOT NULL DEFAULT '', \
       expires TEXT, last_used TEXT)",
    "CREATE TABLE IF NOT EXISTS mrs (\
       target_agent TEXT NOT NULL, id BIGINT NOT NULL, data TEXT NOT NULL, \
       PRIMARY KEY (target_agent, id))",
];

/// Stamp the schema version idempotently. A single fixed row (id=1) plus `ON CONFLICT DO NOTHING`,
/// **not** read-MAX-then-INSERT: two Hubs booting against one fresh Postgres at the same moment would
/// both read 0 and both insert, leaving two rows. The upsert makes the second boot a no-op. Both
/// SQLite (≥3.24) and Postgres support this form.
const STAMP_VERSION: &str = "INSERT INTO schema_version (id, version) VALUES (1, 1) ON CONFLICT DO NOTHING";

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

    pub fn root(&self) -> &Path {
        match self {
            Store::Sqlite(s) => &s.root,
            Store::Pg(s) => &s.root,
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

    // ── agent metadata ──

    pub async fn agents(&self) -> Vec<AgentMeta> {
        match self {
            Store::Sqlite(s) => s.agents().await,
            Store::Pg(s) => s.agents().await,
        }
    }

    pub async fn agent(&self, name: &str) -> Option<AgentMeta> {
        self.agents().await.into_iter().find(|a| a.name == name)
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
    pub async fn agent_or_unowned(&self, name: &str) -> AgentMeta {
        // Built through `new` rather than field-by-field: a field added later must not be able to
        // acquire a laxer default here than a real agent gets.
        self.agent(name).await.unwrap_or_else(|| AgentMeta {
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

    /// Every MR whose **target** is this agent, oldest first (the id order MRs were opened in).
    pub async fn mrs_for(&self, target: &str) -> Vec<Mr> {
        let mut v: Vec<Mr> = self.mrs().await.into_iter().filter(|m| m.target.agent == target).collect();
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
    pub async fn rename_in_mrs(&self, from: &str, to: &str) -> io::Result<()> {
        self.update_mrs(|mrs| {
            for m in mrs.iter_mut() {
                if m.target.agent == from {
                    m.target.agent = to.to_string();
                }
                if m.source.agent == from {
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

    async fn migrate(&self) -> io::Result<()> {
        for stmt in DDL {
            sqlx::query(stmt).execute(&self.pool).await.map_err(err)?;
        }
        sqlx::query(STAMP_VERSION).execute(&self.pool).await.map_err(err)?;
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
        sqlx::query("INSERT INTO users (username, pw_hash, salt, kdf, is_admin, created) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&user.username)
            .bind(&user.pw_hash)
            .bind(&user.salt)
            .bind(&user.kdf)
            .bind(user.is_admin as i64)
            .bind(&user.created)
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
            sqlx::query("INSERT INTO mrs (target_agent, id, data) VALUES (?, ?, ?)")
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
        sqlx::query("INSERT INTO users (username, pw_hash, salt, kdf, is_admin, created) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(&user.username)
            .bind(&user.pw_hash)
            .bind(&user.salt)
            .bind(&user.kdf)
            .bind(user.is_admin as i64)
            .bind(&user.created)
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
            sqlx::query("INSERT INTO mrs (target_agent, id, data) VALUES ($1, $2, $3)")
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
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn tmp_store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open_sqlite(d.path()).await.unwrap();
        (d, s)
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
        })
        .await
        .unwrap();
        assert!(s.user("ALICE").await.is_some());
        assert!(s.user("Alice").await.is_some());
        assert!(s.user("bob").await.is_none());
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
        let m = s.agent_or_unowned("legacy").await;
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
        let m = s.agent("shared").await.unwrap();
        assert_eq!(m.owner.as_deref(), Some("alice"));
        assert_eq!(m.visibility, "public");
        assert_eq!(m.role_of("bob"), Some(Role::Write));
        assert_eq!(m.role_of("eve"), None);
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
        assert_eq!(s.agent("billing").await.unwrap().aid.as_deref(), Some("agt_pay"));
        assert_eq!(s.agent_by_aid("agt_pay").await.unwrap().name, "billing", "by-aid follows the rename");
        assert!(s.agent("payments").await.is_none());
    }

    // ── merge requests ──

    fn mk_mr(id: usize, source: &str, target: &str) -> Mr {
        use super::super::mr::Endpoint;
        Mr {
            id,
            source: Endpoint { aid: Some("agt_src".into()), agent: source.into(), git_ref: "main".into() },
            target: Endpoint { aid: Some("agt_dst".into()), agent: target.into(), git_ref: "main".into() },
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
        let pay = s.mrs_for("payments").await;
        assert_eq!(pay.len(), 2);
        assert_eq!(pay.iter().map(|m| m.id).collect::<Vec<_>>(), vec![1, 2], "oldest first");
        assert_eq!(pay[0].dialogue_transcript.as_deref(), Some("a: ...\nb: ..."));
        assert_eq!(s.mrs_for("other").await.len(), 1);
        assert!(s.mrs_for("nobody").await.is_empty());
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
        s.rename_in_mrs("payments", "billing").await.unwrap();
        assert_eq!(s.mrs_for("billing").await.len(), 1);
        assert!(s.mrs_for("payments").await.is_empty());
        assert_eq!(s.mrs_for("other").await[0].source.agent, "billing");
        // The identity is untouched by a label change.
        assert_eq!(s.mrs_for("billing").await[0].target.aid.as_deref(), Some("agt_dst"));
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
        let m = s.agent("good").await.expect("the row must survive a broken JSON column");
        assert!(m.members.is_empty(), "a broken members column reads as no members");
        assert_eq!(m.to_acl().visibility, Visibility::Private);
    }

    #[tokio::test]
    async fn one_unparseable_mr_row_does_not_drop_the_rest() {
        // A single mrs.data that will not deserialize must lose only itself, mirroring the JSON
        // store's per-record tolerance.
        let (_d, s) = tmp_store().await;
        s.update_mrs(|m| m.push(mk_mr(1, "fork", "payments"))).await.unwrap();
        raw_exec(&s, "INSERT INTO mrs (target_agent, id, data) VALUES ('payments', 999, 'not json')").await;
        let pay = s.mrs_for("payments").await;
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
}
