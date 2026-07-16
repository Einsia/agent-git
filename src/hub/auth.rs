//! Authentication — turning "the credential presented" into "who this is" (`Caller`). It answers
//! **identity only, never permission**: whether something may be done is `acl::decide`'s call, in a
//! different function with a different set of tests.
//!
//! Two paths:
//!   - cookie sessions: for humans (browsers).
//!   - bearer/basic tokens: for git and scripts. A Caller recognized via a token carries a
//!     `TokenGrant` upper bound.
//!
//! Unrecognized = anonymous, not an error — public agents allow anonymous reads by design.

use super::acl::{Caller, Scope, TokenGrant};
use super::kdf;
use super::session::Sessions;
use super::store::{Store, User};

/// The authentication result. `token_id` is the token that matched — the caller uses it to update
/// last_used.
pub struct Authn {
    pub caller: Caller,
    pub token_id: Option<String>,
}

impl Authn {
    fn anonymous() -> Authn {
        Authn { caller: Caller::anonymous(), token_id: None }
    }
}

/// cookie sid + the presented token candidates (the Authorization header may carry both a username
/// and a password slot) → Caller.
///
/// The session wins: when a browser sends both a cookie and Authorization, the user's own session
/// decides (a token can only ever be narrower).
pub fn authenticate(store: &Store, sessions: &Sessions, sid: Option<&str>, secrets: &[String]) -> Authn {
    if let Some(sid) = sid {
        if let Some(username) = sessions.lookup(sid) {
            // Session alive but the user is gone → the session is void. Deleting an account must
            // take effect at once, not wait for the TTL to run out.
            if let Some(u) = store.user(&username) {
                return Authn { caller: user_caller(&u, None), token_id: None };
            }
            sessions.revoke(sid);
        }
    }

    for secret in secrets {
        // The server only holds digests: hash the presented plaintext the same way, then compare in
        // constant time.
        let presented = crate::convo::sha256_hex(secret);
        for t in store.tokens() {
            if !kdf::ct_eq(&presented, &t.hash) {
                continue;
            }
            // The digest matched, but the token may still be unusable: ownerless (an old host-wide
            // token), expired, or an unrecognized scope.
            if !t.usable() {
                return Authn::anonymous();
            }
            let (Some(owner), Some(scope)) = (t.owner.as_deref(), Scope::parse(&t.scope)) else {
                return Authn::anonymous();
            };
            // Owner gone → the token dies with them. Otherwise a deleted account's tokens keep
            // working.
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

/// Login: username + password → User. When the user does not exist it **still runs the KDF**;
/// otherwise the response time would tell the outside world whether a username exists (user
/// enumeration).
pub fn verify_login(store: &Store, username: &str, password: &str) -> Option<User> {
    match store.user(username) {
        Some(u) => kdf::verify_password(password, &u.salt, &u.kdf, &u.pw_hash).then_some(u),
        None => {
            let _ = kdf::hash_password(password, DUMMY_SALT_HEX, &kdf::current_kdf_id());
            None
        }
    }
}

/// A dummy salt for burning the same CPU when the user does not exist. It protects nothing; it only
/// keeps the two paths close in cost.
const DUMMY_SALT_HEX: &str = "00000000000000000000000000000000";

/// last_used is written at most once per this interval. Writing on every request would mean an
/// fsync plus lock contention per request, and the field only says "used recently?" — minute
/// granularity is plenty.
const TOUCH_EVERY_SECS: i64 = 60;

/// Update a token's last_used (throttled). A failed write does not affect the request itself.
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

    /// Issue a token, returning the plaintext.
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
        assert!(a.caller.token.is_none(), "a session must not carry a token upper bound");
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
        // The session table is in memory, the user table on disk — delete the account and the old
        // session must die on the spot.
        let f = fixture();
        let sid = f.sessions.create("ghost").unwrap();
        let a = authenticate(&f.store, &f.sessions, Some(&sid), &[]);
        assert!(a.caller.user.is_none());
        assert_eq!(f.sessions.lookup(&sid), None, "revoked along the way");
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
        let g = a.caller.token.expect("a recognized token must carry its upper bound");
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
        // Tokens in the old auth.json have no owner — that was exactly the "one token = the whole
        // host" model. The new model cannot derive an identity from them, so they are anonymous;
        // no permission is silently inherited.
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
        // git puts the token in the password slot and anything in the username — try both slots as
        // candidates.
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

    // ── Login ──

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
        // touch again right after a write — it must not write again (an unchanged value proves it).
        let f = fixture();
        issue(&f.store, "tok_1", Some("alice"), None, "read", None);
        f.store
            .update_tokens(|t| t[0].last_used = Some("2999-01-01T00:00:00Z".into()))
            .unwrap();
        touch_token(&f.store, "tok_1");
        assert_eq!(f.store.tokens()[0].last_used.as_deref(), Some("2999-01-01T00:00:00Z"));
    }
}
