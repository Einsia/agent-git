//! Hub 的持久状态：users.json / agents.json / auth.json（token）。
//!
//! 都在 `<root>` 下，`<root>` 是 0700，三个文件是 0600 —— 它们装着凭据摘要与访问控制事实。
//! 写用"临时文件 + rename"：rename 在同一文件系统上是原子的，于是并发的读者永远看到
//! 完整的旧版本或完整的新版本，不会读到写了一半的 JSON（进程被 kill 也一样）。
//!
//! 读-改-写走 `update_*`，全程持 `LOCK`。Hub 是单进程多线程，一个进程内的 Mutex 就够；
//! 多进程同时写同一个 root 不在支持范围内（会互相覆盖），这里不假装能扛。

use super::acl::{AgentAcl, Role, Scope, Visibility};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 读-改-写的进程内互斥。粒度粗（三个文件共用一把），但 Hub 的写路径都是低频人操作。
static LOCK: Mutex<()> = Mutex::new(());

pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn is_expired(iso: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(t) => chrono::Utc::now() >= t.with_timezone(&chrono::Utc),
        // 时间戳读不懂 = 不敢当它有效。失败方向朝"已过期"。
        Err(_) => true,
    }
}

// ─────────────────────────── 用户 ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub pw_hash: String,
    pub salt: String,
    /// 派生参数，形如 `argon2id$v=19$m=19456,t=2,p=1` —— 跟 hash 一起存，调参不锁人。
    pub kdf: String,
    #[serde(default)]
    pub is_admin: bool,
    #[serde(default)]
    pub created: String,
}

/// 用户名规则：小写 [a-z0-9._-]，2..=32，不许前导点。登录名大小写不敏感 → 存前先 normalize，
/// 否则 "Alice" 与 "alice" 会变成两个能互相冒充的账号。
pub fn valid_username(name: &str) -> bool {
    let n = name.len();
    (2..=32).contains(&n)
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn normalize_username(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

// ─────────────────────────── agent 元数据 ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub username: String,
    /// "read" | "write" | "admin"
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMeta {
    pub name: String,
    /// agent 身份。**权威值在 store 里的 agent.toml**（客户端铸造、提交进历史）；
    /// 这里只是 Hub 见过之后的缓存，可能为 null（库还空着 / 还没有 agent.toml）。
    #[serde(default)]
    pub aid: Option<String>,
    /// None = 无主：老仓库迁移过来还没认领。只有站点管理员碰得到（见 acl::decide）。
    #[serde(default)]
    pub owner: Option<String>,
    /// "private" | "public"。**新建默认 private**。
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

    /// 元数据 → 授权判定要的事实。**认不出来的 visibility 一律当 private**，
    /// 认不出来的 role 一律丢掉 —— 手改坏 agents.json 的失败方向是"关得更紧"。
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
    /// 撤销用的稳定 id。老文件没有 → 载入时按摘要前缀补（摘要不是凭据，可以当 id 用）。
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// token 的属主。**老 token 没有属主** —— 那正是"一个 token = 整个 host"的老模型。
    /// 无主 token 在新模型里认不出身份，一律失效（见 `authenticate`），不静默继承管理员权限。
    #[serde(default)]
    pub owner: Option<String>,
    /// Some(name) = 只对这一个 agent 有效。
    #[serde(default)]
    pub agent: Option<String>,
    /// "read" | "write"。老文件里这个字段叫 access，值域一样 —— 用 alias 直接认。
    #[serde(alias = "access")]
    pub scope: String,
    /// **只存 token 的 sha256 摘要**，不落明文。
    pub hash: String,
    #[serde(default)]
    pub created: String,
    /// None = 永不过期。
    #[serde(default)]
    pub expires: Option<String>,
    #[serde(default)]
    pub last_used: Option<String>,
}

impl TokenRec {
    pub fn expired(&self) -> bool {
        self.expires.as_deref().map(is_expired).unwrap_or(false)
    }

    /// 能不能用来认证：要有属主（老的无主 token 不行）、scope 认得出、没过期。
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

    // ── 用户 ──

    pub fn users(&self) -> Vec<User> {
        read_list(&self.users_path(), "users")
    }

    pub fn user(&self, username: &str) -> Option<User> {
        let u = normalize_username(username);
        self.users().into_iter().find(|x| x.username == u)
    }

    /// 加用户。同名（normalize 后）已存在 → Err。
    pub fn add_user(&self, user: User) -> io::Result<()> {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut users = self.users();
        if users.iter().any(|x| x.username == user.username) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("用户已存在: {}", user.username)));
        }
        users.push(user);
        write_list(&self.root, &self.users_path(), "users", &users)
    }

    // ── agent 元数据 ──

    pub fn agents(&self) -> Vec<AgentMeta> {
        read_list(&self.agents_path(), "agents")
    }

    pub fn agent(&self, name: &str) -> Option<AgentMeta> {
        self.agents().into_iter().find(|a| a.name == name)
    }

    /// 磁盘上有 `<name>.git`、agents.json 里却没记 → 无主私有。**失败安全**：
    /// 迁移过来的老仓库不会因为"没记录"而变成谁都能拉。
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

    /// 读-改-写 agents.json，全程持锁。闭包返回值透传出来。
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

    // ── token ──

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

/// 老 auth.json 的条目没有 id。摘要不是凭据（拿不回明文），拿它的前缀当稳定 id 是安全的。
fn derive_token_id(hash: &str) -> String {
    format!("tok_{}", hash.chars().take(12).collect::<String>())
}

pub fn new_token_id() -> io::Result<String> {
    Ok(format!("tok_{}", &super::kdf::gen_secret()?[..12]))
}

// ─────────────────────── JSON 文件读写 ───────────────────────

/// `{"<key>": [ ... ]}` → Vec<T>。文件不存在/坏了 → 空表（Hub 照常起，只是没人有权限）。
fn read_list<T: for<'de> Deserialize<'de>>(path: &Path, key: &str) -> Vec<T> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return vec![];
    };
    v.get(key)
        .and_then(|a| a.as_array())
        // 逐条解析：一条坏记录只丢它自己，不至于让整张表变空（那会静默地把所有人放行/挡住）。
        .map(|arr| arr.iter().filter_map(|x| serde_json::from_value::<T>(x.clone()).ok()).collect())
        .unwrap_or_default()
}

fn write_list<T: Serialize>(root: &Path, path: &Path, key: &str, items: &[T]) -> io::Result<()> {
    ensure_root(root)?;
    let body = serde_json::to_string_pretty(&serde_json::json!({ key: items }))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_secret_atomic(path, body.as_bytes())
}

/// root 是凭据目录：0700，owner-only。目录早已存在时 mode 不生效（mode 只在创建时用），
/// 所以创建后再显式收紧一次。
pub fn ensure_root(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .or_else(|e| if root.is_dir() { Ok(()) } else { Err(e) })?;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
}

/// 0600 临时文件 → rename。读者要么看到完整旧版、要么看到完整新版，不会撞见半截 JSON。
/// 从一开始就用 0600 打开（先写后 chmod 有窗口，且 fd 不受 chmod 影响）。
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
        f.sync_all()?; // rename 之后才 fsync 就晚了：崩溃可能留下一个空文件顶掉旧版
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
        assert!(!valid_username("Alice")); // 大写要先 normalize
        assert!(!valid_username(".hidden"));
        assert!(!valid_username("a/b"));
        assert!(!valid_username("a b"));
        assert!(!valid_username(""));
        assert!(!valid_username(&"x".repeat(33)));
        assert_eq!(normalize_username("  Alice "), "alice");
    }

    #[test]
    fn user_lookup_is_case_insensitive() {
        // "Alice" 和 "alice" 必须是同一个人，否则可以注册出一个能冒充对方的同名账号。
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
        // 磁盘上有仓库、agents.json 里没记 —— 不能变成"谁都能拉"。
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
            visibility: "PUBLIC".into(), // 手改坏了
            members: vec![Member { username: "bob".into(), role: "superuser".into() }],
            created: String::new(),
        };
        let acl = m.to_acl();
        assert_eq!(acl.visibility, Visibility::Private);
        assert!(acl.members.is_empty(), "认不出的角色要丢掉，不能当成有权限");
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
        // serde 缺省也得是 private —— 手写的 agents.json 漏了字段不能等于公开。
        let m: AgentMeta = serde_json::from_str(r#"{"name":"x","hash":"y"}"#).unwrap();
        assert_eq!(m.visibility, "private");
        assert!(m.owner.is_none());
    }

    #[test]
    fn legacy_auth_json_is_read_but_unusable() {
        // 老格式：{name, hash, access}，没有 owner。认得出来（好报错），但不能用来认证。
        let (d, s) = tmp_store();
        ensure_root(d.path()).unwrap();
        std::fs::write(
            d.path().join("auth.json"),
            r#"{"tokens":[{"name":"ci","hash":"deadbeefcafe0123","access":"write"}]}"#,
        )
        .unwrap();
        let toks = s.tokens();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].scope, "write", "老字段 access 要认成 scope");
        assert_eq!(toks[0].id, "tok_deadbeefcafe", "没有 id 时按摘要补一个稳定 id");
        assert!(toks[0].owner.is_none());
        assert!(!toks[0].usable(), "无主 token 必须失效 —— 那正是全站通行证的老模型");
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
        assert!(!mk(None).expired(), "没写过期时间 = 不过期");
        assert!(mk(Some("2000-01-01T00:00:00Z")).expired());
        assert!(!mk(Some("2999-01-01T00:00:00Z")).expired());
        assert!(mk(Some("不是时间")).expired(), "读不懂的时间戳要当过期，不能当有效");
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
        assert!(!s.auth_path().with_extension("tmp").exists(), "临时文件要被 rename 掉，不该留下");
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
        // 整张表变空 = 所有 ACL 消失。一条坏记录只丢它自己。
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
