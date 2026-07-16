//! 认证 —— 把"出示的凭据"变成"这是谁"（`Caller`）。**只回答身份，不回答权限**：
//! 能不能做某件事由 `acl::decide` 说了算，那是另一个函数、另一组测试。
//!
//! 两条路：
//!   - cookie 会话：给人用（浏览器）。
//!   - bearer/basic token：给 git 与脚本用。token 认出来的 Caller 会带上 `TokenGrant` 上界。
//!
//! 认不出来 = 匿名，不是报错 —— public agent 本来就允许匿名读。

use super::acl::{Caller, Scope, TokenGrant};
use super::kdf;
use super::session::Sessions;
use super::store::{Store, User};

/// 认证结果。`token_id` 是命中的 token —— 调用方拿它更新 last_used。
pub struct Authn {
    pub caller: Caller,
    pub token_id: Option<String>,
}

impl Authn {
    fn anonymous() -> Authn {
        Authn { caller: Caller::anonymous(), token_id: None }
    }
}

/// cookie sid + 出示的 token 候选（Authorization 头里可能有用户名/密码两段）→ Caller。
///
/// 会话优先：浏览器同时带 cookie 和 Authorization 时，以本人会话为准（token 只会更窄）。
pub fn authenticate(store: &Store, sessions: &Sessions, sid: Option<&str>, secrets: &[String]) -> Authn {
    if let Some(sid) = sid {
        if let Some(username) = sessions.lookup(sid) {
            // 会话还在、用户被删了 → 会话作废。删号必须当场失效，不能等 TTL 到期。
            if let Some(u) = store.user(&username) {
                return Authn { caller: user_caller(&u, None), token_id: None };
            }
            sessions.revoke(sid);
        }
    }

    for secret in secrets {
        // 服务器只有摘要：把出示的明文做同样的 hash 再常数时间比。
        let presented = crate::convo::sha256_hex(secret);
        for t in store.tokens() {
            if !kdf::ct_eq(&presented, &t.hash) {
                continue;
            }
            // 摘要对上了，但这个 token 未必能用：无主（老的全站 token）、过期、scope 认不出来。
            if !t.usable() {
                return Authn::anonymous();
            }
            let (Some(owner), Some(scope)) = (t.owner.as_deref(), Scope::parse(&t.scope)) else {
                return Authn::anonymous();
            };
            // 属主没了 → token 跟着死。否则删号之后他的 token 还能用。
            let Some(u) = store.user(owner) else {
                return Authn::anonymous();
            };
            let grant = TokenGrant { agent: t.agent.clone(), scope };
            return Authn { caller: user_caller(&u, Some(grant)), token_id: Some(t.id.clone()) };
        }
    }

    Authn::anonymous()
}

fn user_caller(u: &User, token: Option<TokenGrant>) -> Caller {
    Caller { user: Some(u.username.clone()), is_admin: u.is_admin, token }
}

/// 登录：用户名 + 密码 → User。用户不存在时**也走一遍 KDF**，否则响应快慢会把
/// "这个用户名存不存在"直接告诉外面（用户枚举）。
pub fn verify_login(store: &Store, username: &str, password: &str) -> Option<User> {
    match store.user(username) {
        Some(u) => kdf::verify_password(password, &u.salt, &u.kdf, &u.pw_hash).then_some(u),
        None => {
            let _ = kdf::hash_password(password, DUMMY_SALT_HEX, &kdf::current_kdf_id());
            None
        }
    }
}

/// 用户不存在时用来烧掉同等 CPU 的假盐。它不保护任何东西，只为让两条路径耗时接近。
const DUMMY_SALT_HEX: &str = "00000000000000000000000000000000";

/// last_used 至少隔这么久才写一次。每个请求都写 = 每个请求一次 fsync + 抢锁，
/// 而这个字段只是"最近用过吗"，精确到分钟足够。
const TOUCH_EVERY_SECS: i64 = 60;

/// 更新 token 的 last_used（限频）。写失败不影响请求本身。
pub fn touch_token(store: &Store, id: &str) {
    let now = chrono::Utc::now();
    let fresh = store
        .tokens()
        .iter()
        .find(|t| t.id == id)
        .and_then(|t| t.last_used.clone())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds() < TOUCH_EVERY_SECS)
        .unwrap_or(false);
    if fresh {
        return;
    }
    let _ = store.update_tokens(|toks| {
        if let Some(t) = toks.iter_mut().find(|t| t.id == id) {
            t.last_used = Some(super::store::now_iso());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::store::{now_iso, TokenRec};

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
        sessions: Sessions,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        add_user(&store, "alice", "pw-alice-123", false);
        add_user(&store, "root", "pw-root-123", true);
        Fixture { _dir: dir, store, sessions: Sessions::new() }
    }

    fn add_user(store: &Store, name: &str, pw: &str, admin: bool) {
        let salt = kdf::gen_salt().unwrap();
        let kdf_id = kdf::current_kdf_id();
        store
            .add_user(User {
                username: name.into(),
                pw_hash: kdf::hash_password(pw, &salt, &kdf_id).unwrap(),
                salt,
                kdf: kdf_id,
                is_admin: admin,
                created: now_iso(),
            })
            .unwrap();
    }

    /// 发一个 token，返回明文。
    fn issue(store: &Store, id: &str, owner: Option<&str>, agent: Option<&str>, scope: &str, expires: Option<&str>) -> String {
        let secret = kdf::gen_secret().unwrap();
        store
            .update_tokens(|t| {
                t.push(TokenRec {
                    id: id.into(),
                    name: id.into(),
                    owner: owner.map(|s| s.into()),
                    agent: agent.map(|s| s.into()),
                    scope: scope.into(),
                    hash: crate::convo::sha256_hex(&secret),
                    created: now_iso(),
                    expires: expires.map(|s| s.into()),
                    last_used: None,
                })
            })
            .unwrap();
        secret
    }

    #[test]
    fn no_credentials_is_anonymous() {
        let f = fixture();
        let a = authenticate(&f.store, &f.sessions, None, &[]);
        assert!(a.caller.user.is_none());
        assert!(!a.caller.is_admin);
        assert!(a.caller.token.is_none());
    }

    #[test]
    fn session_cookie_identifies_the_user() {
        let f = fixture();
        let sid = f.sessions.create("alice").unwrap();
        let a = authenticate(&f.store, &f.sessions, Some(&sid), &[]);
        assert_eq!(a.caller.user.as_deref(), Some("alice"));
        assert!(a.caller.token.is_none(), "会话不该带 token 上界");
    }

    #[test]
    fn admin_flag_comes_from_the_user_record() {
        let f = fixture();
        let sid = f.sessions.create("root").unwrap();
        assert!(authenticate(&f.store, &f.sessions, Some(&sid), &[]).caller.is_admin);
        let sid = f.sessions.create("alice").unwrap();
        assert!(!authenticate(&f.store, &f.sessions, Some(&sid), &[]).caller.is_admin);
    }

    #[test]
    fn session_for_a_deleted_user_is_dead() {
        // 会话表在内存里、用户表在磁盘上 —— 删了号，旧会话必须当场失效。
        let f = fixture();
        let sid = f.sessions.create("ghost").unwrap();
        let a = authenticate(&f.store, &f.sessions, Some(&sid), &[]);
        assert!(a.caller.user.is_none());
        assert_eq!(f.sessions.lookup(&sid), None, "顺手撤掉");
    }

    #[test]
    fn bogus_session_is_anonymous() {
        let f = fixture();
        let a = authenticate(&f.store, &f.sessions, Some("deadbeef"), &[]);
        assert!(a.caller.user.is_none());
    }

    #[test]
    fn token_identifies_owner_and_carries_its_grant() {
        let f = fixture();
        let secret = issue(&f.store, "tok_1", Some("alice"), Some("proj"), "write", None);
        let a = authenticate(&f.store, &f.sessions, None, &[secret]);
        assert_eq!(a.caller.user.as_deref(), Some("alice"));
        assert_eq!(a.token_id.as_deref(), Some("tok_1"));
        let g = a.caller.token.expect("token 认出来就必须带上界");
        assert_eq!(g.agent.as_deref(), Some("proj"));
        assert_eq!(g.scope, Scope::Write);
    }

    #[test]
    fn unbound_token_has_no_agent_binding() {
        let f = fixture();
        let secret = issue(&f.store, "tok_1", Some("alice"), None, "read", None);
        let a = authenticate(&f.store, &f.sessions, None, &[secret]);
        let g = a.caller.token.unwrap();
        assert!(g.agent.is_none());
        assert_eq!(g.scope, Scope::Read);
    }

    #[test]
    fn legacy_ownerless_token_authenticates_nobody() {
        // 老 auth.json 的 token 没有属主 —— 那正是"一个 token = 整个 host"的模型。
        // 新模型认不出身份，一律当匿名，不静默继承任何权限。
        let f = fixture();
        let secret = issue(&f.store, "tok_legacy", None, None, "write", None);
        let a = authenticate(&f.store, &f.sessions, None, &[secret]);
        assert!(a.caller.user.is_none());
        assert!(a.caller.token.is_none());
    }

    #[test]
    fn expired_token_authenticates_nobody() {
        let f = fixture();
        let secret = issue(&f.store, "tok_1", Some("alice"), None, "write", Some("2000-01-01T00:00:00Z"));
        assert!(authenticate(&f.store, &f.sessions, None, &[secret]).caller.user.is_none());
    }

    #[test]
    fn token_of_a_deleted_owner_is_dead() {
        let f = fixture();
        let secret = issue(&f.store, "tok_1", Some("ghost"), None, "write", None);
        assert!(authenticate(&f.store, &f.sessions, None, &[secret]).caller.user.is_none());
    }

    #[test]
    fn token_with_junk_scope_authenticates_nobody() {
        let f = fixture();
        let secret = issue(&f.store, "tok_1", Some("alice"), None, "superuser", None);
        assert!(authenticate(&f.store, &f.sessions, None, &[secret]).caller.user.is_none());
    }

    #[test]
    fn wrong_token_is_anonymous() {
        let f = fixture();
        issue(&f.store, "tok_1", Some("alice"), None, "write", None);
        let a = authenticate(&f.store, &f.sessions, None, &["not-the-token".to_string()]);
        assert!(a.caller.user.is_none());
    }

    #[test]
    fn basic_auth_username_slot_also_works() {
        // git 会把 token 填在密码位、用户名随便填 —— 两段都当候选试。
        let f = fixture();
        let secret = issue(&f.store, "tok_1", Some("alice"), None, "read", None);
        let a = authenticate(&f.store, &f.sessions, None, &["git".to_string(), secret]);
        assert_eq!(a.caller.user.as_deref(), Some("alice"));
    }

    #[test]
    fn session_wins_over_token() {
        let f = fixture();
        let secret = issue(&f.store, "tok_1", Some("alice"), None, "read", None);
        let sid = f.sessions.create("root").unwrap();
        let a = authenticate(&f.store, &f.sessions, Some(&sid), &[secret]);
        assert_eq!(a.caller.user.as_deref(), Some("root"));
        assert!(a.caller.token.is_none());
    }

    // ── 登录 ──

    #[test]
    fn login_checks_the_password() {
        let f = fixture();
        assert!(verify_login(&f.store, "alice", "pw-alice-123").is_some());
        assert!(verify_login(&f.store, "alice", "pw-alice-124").is_none());
        assert!(verify_login(&f.store, "alice", "").is_none());
    }

    #[test]
    fn login_is_case_insensitive_on_username() {
        let f = fixture();
        assert!(verify_login(&f.store, "ALICE", "pw-alice-123").is_some());
    }

    #[test]
    fn login_for_unknown_user_fails_without_panicking() {
        let f = fixture();
        assert!(verify_login(&f.store, "nobody", "whatever").is_none());
    }

    #[test]
    fn touch_updates_last_used() {
        let f = fixture();
        issue(&f.store, "tok_1", Some("alice"), None, "read", None);
        assert!(f.store.tokens()[0].last_used.is_none());
        touch_token(&f.store, "tok_1");
        assert!(f.store.tokens()[0].last_used.is_some());
    }

    #[test]
    fn touch_is_throttled() {
        // 刚写过就再 touch 一次 —— 不该再写（值不变即证明没重写）。
        let f = fixture();
        issue(&f.store, "tok_1", Some("alice"), None, "read", None);
        f.store
            .update_tokens(|t| t[0].last_used = Some("2999-01-01T00:00:00Z".into()))
            .unwrap();
        touch_token(&f.store, "tok_1");
        assert_eq!(f.store.tokens()[0].last_used.as_deref(), Some("2999-01-01T00:00:00Z"));
    }
}
