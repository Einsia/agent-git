//! The Hub's persistent state: users.json / agents.json / auth.json (tokens).
//!
//! All under `<root>`, which is 0700, with the three files at 0600 — they hold credential digests
//! and access-control facts. Writes go through "temp file + rename": rename is atomic within one
//! filesystem, so a concurrent reader always sees either the complete old version or the complete
//! new one, never half-written JSON (the same holds if the process is killed).
//!
//! Read-modify-write goes through `update_*`, holding `LOCK` throughout. The Hub is one process with
//! many threads, so an in-process Mutex is enough; several processes writing one root concurrently
//! is out of scope (they would overwrite each other), and this does not pretend to survive it.

use super::acl::{AgentAcl, Role, Scope, Visibility};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// In-process mutex for read-modify-write. Coarse-grained (one lock for all three files), but every
/// write path in the Hub is a low-frequency human action.
static LOCK: Mutex<()> = Mutex::new(());

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
    #[serde(default)]
    pub members: Vec<Member>,
    #[serde(default)]
    pub created: String,
}

fn default_visibility() -> String {
    "private".into()
}

impl AgentMeta {
    pub fn new(name: &str, owner: Option<&str>, visibility: Visibility) -> AgentMeta {
        AgentMeta {
            name: name.to_string(),
            aid: None,
            owner: owner.map(|s| s.to_string()),
            visibility: visibility.as_str().to_string(),
            members: vec![],
            created: now_iso(),
        }
    }

    /// Metadata → the facts the authorization decision needs. **An unrecognized visibility is
    /// treated as private**, and an unrecognized role is dropped — hand-mangling agents.json errs in
    /// the direction of "locked down tighter".
    pub fn to_acl(&self) -> AgentAcl {
        AgentAcl {
            name: self.name.clone(),
            owner: self.owner.clone(),
            visibility: Visibility::parse(&self.visibility).unwrap_or(Visibility::Private),
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
}

// ─────────────────────────── token ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRec {
    /// A stable id for revocation. Old files have none → backfilled from the digest prefix on load
    /// (a digest is not a credential, so it is safe to use as an id).
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

// ─────────────────────────── Store ───────────────────────────

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: &Path) -> Store {
        Store { root: root.to_path_buf() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn users_path(&self) -> PathBuf {
        self.root.join("users.json")
    }

    fn agents_path(&self) -> PathBuf {
        self.root.join("agents.json")
    }

    fn auth_path(&self) -> PathBuf {
        self.root.join("auth.json")
    }

    // ── users ──

    pub fn users(&self) -> Vec<User> {
        read_list(&self.users_path(), "users")
    }

    pub fn user(&self, username: &str) -> Option<User> {
        let u = normalize_username(username);
        self.users().into_iter().find(|x| x.username == u)
    }

    /// Add a user. Err if the same name (after normalizing) already exists.
    pub fn add_user(&self, user: User) -> io::Result<()> {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut users = self.users();
        if users.iter().any(|x| x.username == user.username) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("user already exists: {}", user.username)));
        }
        users.push(user);
        write_list(&self.root, &self.users_path(), "users", &users)
    }

    // ── agent metadata ──

    pub fn agents(&self) -> Vec<AgentMeta> {
        read_list(&self.agents_path(), "agents")
    }

    pub fn agent(&self, name: &str) -> Option<AgentMeta> {
        self.agents().into_iter().find(|a| a.name == name)
    }

    /// `<name>.git` exists on disk but agents.json has no record of it → unowned and private.
    /// **Fail-safe**: a migrated-in old repo does not become world-pullable just because there is no
    /// record of it.
    pub fn agent_or_unowned(&self, name: &str) -> AgentMeta {
        self.agent(name).unwrap_or_else(|| AgentMeta {
            name: name.to_string(),
            aid: None,
            owner: None,
            visibility: "private".into(),
            members: vec![],
            created: String::new(),
        })
    }

    /// Read-modify-write agents.json, holding the lock throughout. The closure's return value is
    /// passed straight back out.
    pub fn update_agents<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<AgentMeta>) -> R,
    {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut agents = self.agents();
        let r = f(&mut agents);
        write_list(&self.root, &self.agents_path(), "agents", &agents)?;
        Ok(r)
    }

    // ── tokens ──

    pub fn tokens(&self) -> Vec<TokenRec> {
        let mut toks: Vec<TokenRec> = read_list(&self.auth_path(), "tokens");
        for t in &mut toks {
            if t.id.is_empty() {
                t.id = derive_token_id(&t.hash);
            }
        }
        toks
    }

    pub fn update_tokens<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Vec<TokenRec>) -> R,
    {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut toks = self.tokens();
        let r = f(&mut toks);
        write_list(&self.root, &self.auth_path(), "tokens", &toks)?;
        Ok(r)
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

// ─────────────────────── JSON file IO ───────────────────────

/// `{"<key>": [ ... ]}` → Vec<T>. Missing or broken file → an empty list (the Hub still starts; it
/// just means nobody has any permission).
fn read_list<T: for<'de> Deserialize<'de>>(path: &Path, key: &str) -> Vec<T> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return vec![];
    };
    v.get(key)
        .and_then(|a| a.as_array())
        // Parse record by record: one broken record only loses itself, rather than emptying the
        // whole list (which would silently let everyone in, or shut everyone out).
        .map(|arr| arr.iter().filter_map(|x| serde_json::from_value::<T>(x.clone()).ok()).collect())
        .unwrap_or_default()
}

fn write_list<T: Serialize>(root: &Path, path: &Path, key: &str, items: &[T]) -> io::Result<()> {
    ensure_root(root)?;
    let body = serde_json::to_string_pretty(&serde_json::json!({ key: items }))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_secret_atomic(path, body.as_bytes())
}

/// root is a credential directory: 0700, owner-only. When the directory already exists the mode has
/// no effect (mode only applies at creation), so tighten it explicitly afterwards.
pub fn ensure_root(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .or_else(|e| if root.is_dir() { Ok(()) } else { Err(e) })?;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
}

/// 0600 temp file → rename. A reader sees either the complete old version or the complete new one,
/// never half a JSON. Opened 0600 from the start (writing first and chmod'ing after leaves a window,
/// and an already-open fd is unaffected by the chmod anyway).
fn write_secret_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?; // fsync after the rename would be too late: a crash could leave an empty file in place of the old version
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path());
        (d, s)
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

    #[test]
    fn user_lookup_is_case_insensitive() {
        // "Alice" and "alice" must be the same person, or you could register a same-name account
        // that impersonates the other.
        let (_d, s) = tmp_store();
        s.add_user(User {
            username: "alice".into(),
            pw_hash: "h".into(),
            salt: "s".into(),
            kdf: "k".into(),
            is_admin: true,
            created: now_iso(),
        })
        .unwrap();
        assert!(s.user("ALICE").is_some());
        assert!(s.user("Alice").is_some());
        assert!(s.user("bob").is_none());
    }

    #[test]
    fn duplicate_user_is_refused() {
        let (_d, s) = tmp_store();
        let u = User {
            username: "alice".into(),
            pw_hash: "h".into(),
            salt: "s".into(),
            kdf: "k".into(),
            is_admin: false,
            created: now_iso(),
        };
        s.add_user(u.clone()).unwrap();
        assert!(s.add_user(u).is_err());
    }

    #[test]
    fn secret_files_are_0600_and_root_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let (d, s) = tmp_store();
        s.add_user(User {
            username: "alice".into(),
            pw_hash: "h".into(),
            salt: "s".into(),
            kdf: "k".into(),
            is_admin: true,
            created: now_iso(),
        })
        .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&s.users_path()), 0o600);
        assert_eq!(mode(d.path()), 0o700);
    }

    #[test]
    fn unknown_agent_is_private_and_unowned() {
        // Repo on disk, no record in agents.json — it must not turn into "anyone can pull it".
        let (_d, s) = tmp_store();
        let m = s.agent_or_unowned("legacy");
        assert_eq!(m.visibility, "private");
        assert!(m.owner.is_none());
        let acl = m.to_acl();
        assert_eq!(acl.visibility, Visibility::Private);
        assert!(acl.owner.is_none());
    }

    #[test]
    fn broken_visibility_falls_back_to_private() {
        let m = AgentMeta {
            name: "x".into(),
            aid: None,
            owner: Some("alice".into()),
            visibility: "PUBLIC".into(), // hand-mangled
            members: vec![Member { username: "bob".into(), role: "superuser".into() }],
            created: String::new(),
        };
        let acl = m.to_acl();
        assert_eq!(acl.visibility, Visibility::Private);
        assert!(acl.members.is_empty(), "an unrecognized role must be dropped, not treated as a permission");
    }

    #[test]
    fn agents_roundtrip_through_disk() {
        let (_d, s) = tmp_store();
        s.update_agents(|a| {
            let mut m = AgentMeta::new("shared", Some("alice"), Visibility::Public);
            m.members.push(Member { username: "bob".into(), role: "write".into() });
            a.push(m);
        })
        .unwrap();
        let m = s.agent("shared").unwrap();
        assert_eq!(m.owner.as_deref(), Some("alice"));
        assert_eq!(m.visibility, "public");
        assert_eq!(m.role_of("bob"), Some(Role::Write));
        assert_eq!(m.role_of("eve"), None);
    }

    #[test]
    fn new_agent_meta_defaults_to_private() {
        assert_eq!(AgentMeta::new("x", Some("alice"), Visibility::Private).visibility, "private");
        // The serde default must be private too — a hand-written agents.json missing the field must
        // not amount to public.
        let m: AgentMeta = serde_json::from_str(r#"{"name":"x","hash":"y"}"#).unwrap();
        assert_eq!(m.visibility, "private");
        assert!(m.owner.is_none());
    }

    #[test]
    fn legacy_auth_json_is_read_but_unusable() {
        // The old format: {name, hash, access}, no owner. Recognized (so it can be reported), but
        // unusable for authentication.
        let (d, s) = tmp_store();
        ensure_root(d.path()).unwrap();
        std::fs::write(
            d.path().join("auth.json"),
            r#"{"tokens":[{"name":"ci","hash":"deadbeefcafe0123","access":"write"}]}"#,
        )
        .unwrap();
        let toks = s.tokens();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].scope, "write", "the old access field must be read as scope");
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

    #[test]
    fn atomic_write_leaves_no_tmp_and_replaces_content() {
        let (_d, s) = tmp_store();
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
        .unwrap();
        s.update_tokens(|t| t.clear()).unwrap();
        assert!(s.tokens().is_empty());
        assert!(!s.auth_path().with_extension("tmp").exists(), "the temp file must be renamed away, not left behind");
    }

    #[test]
    fn corrupt_json_yields_empty_not_panic() {
        let (d, s) = tmp_store();
        ensure_root(d.path()).unwrap();
        std::fs::write(d.path().join("users.json"), "{ not json").unwrap();
        assert!(s.users().is_empty());
    }

    #[test]
    fn one_broken_record_does_not_drop_the_rest() {
        // An emptied list = every ACL vanishes. One broken record only loses itself.
        let (d, s) = tmp_store();
        ensure_root(d.path()).unwrap();
        std::fs::write(
            d.path().join("agents.json"),
            r#"{"agents":[{"nope":1},{"name":"good","owner":"alice","visibility":"public"}]}"#,
        )
        .unwrap();
        let a = s.agents();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "good");
    }
}
