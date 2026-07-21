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
pub async fn authenticate(store: &Store, sessions: &Sessions, sid: Option<&str>, secrets: &[String]) -> Authn {
    if let Some(sid) = sid {
        if let Some(username) = sessions.lookup(sid) {
            // Session alive but the user is gone → the session is void. Deleting an account must
            // take effect at once, not wait for the TTL to run out.
            if let Some(u) = store.user(&username).await {
                // A disabled (admin-suspended) account is void the same way a deleted one is: its
                // live session must stop authenticating on the spot, not linger until the TTL. Enable
                // restores it. Mirror the deleted-owner branch → anonymous.
                if u.disabled {
                    return Authn::anonymous();
                }
                return Authn { caller: user_caller(&u, None), token_id: None };
            }
            sessions.revoke(sid);
        }
    }

    for secret in secrets {
        // The server only holds digests: hash the presented plaintext the same way, then compare in
        // constant time.
        let presented = crate::convo::sha256_hex(secret);
        for t in store.tokens().await {
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
            let Some(u) = store.user(owner).await else {
                return Authn::anonymous();
            };
            // Owner disabled (admin-suspended) → the token stops authenticating, exactly like a
            // deleted owner. Without this a suspended account's pre-existing bearer token keeps
            // working and the admin-disable is bypassable. Enable restores it.
            if u.disabled {
                return Authn::anonymous();
            }
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
pub async fn verify_login(store: &Store, username: &str, password: &str) -> Option<User> {
    let user = store.user(username).await;
    let password = password.to_string();
    // argon2 is deliberately CPU- and memory-heavy. Run the KDF on the blocking pool so a login does
    // not peg an async worker thread (which would stall every other in-flight request on it). The
    // store read above already happened async; only the derivation is offloaded.
    tokio::task::spawn_blocking(move || match user {
        Some(u) => kdf::verify_password(&password, &u.salt, &u.kdf, &u.pw_hash).then_some(u),
        None => {
            let _ = kdf::hash_password(&password, DUMMY_SALT_HEX, &kdf::current_kdf_id());
            None
        }
    })
    .await
    .ok()
    .flatten()
}

/// A dummy salt for burning the same CPU when the user does not exist. It protects nothing; it only
/// keeps the two paths close in cost.
const DUMMY_SALT_HEX: &str = "00000000000000000000000000000000";

/// last_used is written at most once per this interval. Writing on every request would mean an
/// fsync plus lock contention per request, and the field only says "used recently?" — minute
/// granularity is plenty.
const TOUCH_EVERY_SECS: i64 = 60;

/// Update a token's last_used (throttled). A failed write does not affect the request itself.
pub async fn touch_token(store: &Store, id: &str) {
    let now = chrono::Utc::now();
    let fresh = store
        .tokens()
        .await
        .iter()
        .find(|t| t.id == id)
        .and_then(|t| t.last_used.clone())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds() < TOUCH_EVERY_SECS)
        .unwrap_or(false);
    if fresh {
        return;
    }
    let _ = store
        .update_tokens(|toks| {
            if let Some(t) = toks.iter_mut().find(|t| t.id == id) {
                t.last_used = Some(super::store::now_iso());
            }
        })
        .await;
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

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_sqlite(dir.path()).await.unwrap();
        add_user(&store, "alice", "pw-alice-123", false).await;
        add_user(&store, "root", "pw-root-123", true).await;
        Fixture { _dir: dir, store, sessions: Sessions::new() }
    }

    async fn add_user(store: &Store, name: &str, pw: &str, admin: bool) {
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
                ..Default::default()
            })
            .await
            .unwrap();
    }

    /// Issue a token, returning the plaintext.
    async fn issue(store: &Store, id: &str, owner: Option<&str>, agent: Option<&str>, scope: &str, expires: Option<&str>) -> String {
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
            .await
            .unwrap();
        secret
    }

    #[tokio::test]
    async fn no_credentials_is_anonymous() {
        let f = fixture().await;
        let a = authenticate(&f.store, &f.sessions, None, &[]).await;
        assert!(a.caller.user.is_none());
        assert!(!a.caller.is_admin);
        assert!(a.caller.token.is_none());
    }

    #[tokio::test]
    async fn session_cookie_identifies_the_user() {
        let f = fixture().await;
        let sid = f.sessions.create("alice").unwrap();
        let a = authenticate(&f.store, &f.sessions, Some(&sid), &[]).await;
        assert_eq!(a.caller.user.as_deref(), Some("alice"));
        assert!(a.caller.token.is_none(), "a session must not carry a token upper bound");
    }

    #[tokio::test]
    async fn admin_flag_comes_from_the_user_record() {
        let f = fixture().await;
        let sid = f.sessions.create("root").unwrap();
        assert!(authenticate(&f.store, &f.sessions, Some(&sid), &[]).await.caller.is_admin);
        let sid = f.sessions.create("alice").unwrap();
        assert!(!authenticate(&f.store, &f.sessions, Some(&sid), &[]).await.caller.is_admin);
    }

    #[tokio::test]
    async fn session_for_a_deleted_user_is_dead() {
        // The session table is in memory, the user table on disk — delete the account and the old
        // session must die on the spot.
        let f = fixture().await;
        let sid = f.sessions.create("ghost").unwrap();
        let a = authenticate(&f.store, &f.sessions, Some(&sid), &[]).await;
        assert!(a.caller.user.is_none());
        assert_eq!(f.sessions.lookup(&sid), None, "revoked along the way");
    }

    #[tokio::test]
    async fn bogus_session_is_anonymous() {
        let f = fixture().await;
        let a = authenticate(&f.store, &f.sessions, Some("deadbeef"), &[]).await;
        assert!(a.caller.user.is_none());
    }

    #[tokio::test]
    async fn token_identifies_owner_and_carries_its_grant() {
        let f = fixture().await;
        let secret = issue(&f.store, "tok_1", Some("alice"), Some("proj"), "write", None).await;
        let a = authenticate(&f.store, &f.sessions, None, &[secret]).await;
        assert_eq!(a.caller.user.as_deref(), Some("alice"));
        assert_eq!(a.token_id.as_deref(), Some("tok_1"));
        let g = a.caller.token.expect("a recognized token must carry its upper bound");
        assert_eq!(g.agent.as_deref(), Some("proj"));
        assert_eq!(g.scope, Scope::Write);
    }

    #[tokio::test]
    async fn unbound_token_has_no_agent_binding() {
        let f = fixture().await;
        let secret = issue(&f.store, "tok_1", Some("alice"), None, "read", None).await;
        let a = authenticate(&f.store, &f.sessions, None, &[secret]).await;
        let g = a.caller.token.unwrap();
        assert!(g.agent.is_none());
        assert_eq!(g.scope, Scope::Read);
    }

    #[tokio::test]
    async fn legacy_ownerless_token_authenticates_nobody() {
        // Tokens in the old auth.json have no owner — that was exactly the "one token = the whole
        // host" model. The new model cannot derive an identity from them, so they are anonymous;
        // no permission is silently inherited.
        let f = fixture().await;
        let secret = issue(&f.store, "tok_legacy", None, None, "write", None).await;
        let a = authenticate(&f.store, &f.sessions, None, &[secret]).await;
        assert!(a.caller.user.is_none());
        assert!(a.caller.token.is_none());
    }

    #[tokio::test]
    async fn expired_token_authenticates_nobody() {
        let f = fixture().await;
        let secret = issue(&f.store, "tok_1", Some("alice"), None, "write", Some("2000-01-01T00:00:00Z")).await;
        assert!(authenticate(&f.store, &f.sessions, None, &[secret]).await.caller.user.is_none());
    }

    #[tokio::test]
    async fn token_of_a_deleted_owner_is_dead() {
        let f = fixture().await;
        let secret = issue(&f.store, "tok_1", Some("ghost"), None, "write", None).await;
        assert!(authenticate(&f.store, &f.sessions, None, &[secret]).await.caller.user.is_none());
    }

    #[tokio::test]
    async fn token_with_junk_scope_authenticates_nobody() {
        let f = fixture().await;
        let secret = issue(&f.store, "tok_1", Some("alice"), None, "superuser", None).await;
        assert!(authenticate(&f.store, &f.sessions, None, &[secret]).await.caller.user.is_none());
    }

    #[tokio::test]
    async fn disabled_owner_token_authenticates_nobody_and_enable_restores() {
        // A pre-existing bearer token whose owner is DISABLED (admin soft-suspend) must stop
        // authenticating on the spot — otherwise admin-disable is bypassable via the old token. Enable
        // restores it. A non-disabled owner's token is unaffected.
        let f = fixture().await;
        let secret = issue(&f.store, "tok_1", Some("alice"), Some("proj"), "write", None).await;

        // While enabled: the token authorizes alice.
        let a = authenticate(&f.store, &f.sessions, None, &[secret.clone()]).await;
        assert_eq!(a.caller.user.as_deref(), Some("alice"), "an active owner's token authorizes");

        // Disable alice: the SAME token now authenticates nobody.
        assert!(f.store.set_user_disabled("alice", true).await.unwrap());
        let a = authenticate(&f.store, &f.sessions, None, &[secret.clone()]).await;
        assert!(a.caller.user.is_none(), "a disabled owner's token must not authenticate");
        assert!(a.caller.token.is_none());
        assert!(a.token_id.is_none());

        // Enable restores it.
        assert!(f.store.set_user_disabled("alice", false).await.unwrap());
        let a = authenticate(&f.store, &f.sessions, None, &[secret]).await;
        assert_eq!(a.caller.user.as_deref(), Some("alice"), "enable restores the token");

        // A different, never-disabled owner's token is unaffected throughout.
        let root_secret = issue(&f.store, "tok_root", Some("root"), None, "read", None).await;
        assert_eq!(
            authenticate(&f.store, &f.sessions, None, &[root_secret]).await.caller.user.as_deref(),
            Some("root"),
            "a non-disabled user's token still authorizes"
        );
    }

    #[tokio::test]
    async fn disabled_users_session_is_dead_and_enable_restores() {
        // A disabled account's LIVE session must stop authenticating too, not linger until the TTL.
        let f = fixture().await;
        let sid = f.sessions.create("alice").unwrap();
        assert_eq!(
            authenticate(&f.store, &f.sessions, Some(&sid), &[]).await.caller.user.as_deref(),
            Some("alice"),
            "an active user's session identifies them"
        );

        assert!(f.store.set_user_disabled("alice", true).await.unwrap());
        let a = authenticate(&f.store, &f.sessions, Some(&sid), &[]).await;
        assert!(a.caller.user.is_none(), "a disabled user's session must not authenticate");

        assert!(f.store.set_user_disabled("alice", false).await.unwrap());
        assert_eq!(
            authenticate(&f.store, &f.sessions, Some(&sid), &[]).await.caller.user.as_deref(),
            Some("alice"),
            "enable restores the session"
        );
    }

    #[tokio::test]
    async fn wrong_token_is_anonymous() {
        let f = fixture().await;
        issue(&f.store, "tok_1", Some("alice"), None, "write", None).await;
        let a = authenticate(&f.store, &f.sessions, None, &["not-the-token".to_string()]).await;
        assert!(a.caller.user.is_none());
    }

    #[tokio::test]
    async fn basic_auth_username_slot_also_works() {
        // git puts the token in the password slot and anything in the username — try both slots as
        // candidates.
        let f = fixture().await;
        let secret = issue(&f.store, "tok_1", Some("alice"), None, "read", None).await;
        let a = authenticate(&f.store, &f.sessions, None, &["git".to_string(), secret]).await;
        assert_eq!(a.caller.user.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn session_wins_over_token() {
        let f = fixture().await;
        let secret = issue(&f.store, "tok_1", Some("alice"), None, "read", None).await;
        let sid = f.sessions.create("root").unwrap();
        let a = authenticate(&f.store, &f.sessions, Some(&sid), &[secret]).await;
        assert_eq!(a.caller.user.as_deref(), Some("root"));
        assert!(a.caller.token.is_none());
    }

    // ── Login ──

    #[tokio::test]
    async fn login_checks_the_password() {
        let f = fixture().await;
        assert!(verify_login(&f.store, "alice", "pw-alice-123").await.is_some());
        assert!(verify_login(&f.store, "alice", "pw-alice-124").await.is_none());
        assert!(verify_login(&f.store, "alice", "").await.is_none());
    }

    #[tokio::test]
    async fn login_is_case_insensitive_on_username() {
        let f = fixture().await;
        assert!(verify_login(&f.store, "ALICE", "pw-alice-123").await.is_some());
    }

    #[tokio::test]
    async fn login_for_unknown_user_fails_without_panicking() {
        let f = fixture().await;
        assert!(verify_login(&f.store, "nobody", "whatever").await.is_none());
    }

    #[tokio::test]
    async fn touch_updates_last_used() {
        let f = fixture().await;
        issue(&f.store, "tok_1", Some("alice"), None, "read", None).await;
        assert!(f.store.tokens().await[0].last_used.is_none());
        touch_token(&f.store, "tok_1").await;
        assert!(f.store.tokens().await[0].last_used.is_some());
    }

    #[tokio::test]
    async fn touch_is_throttled() {
        // touch again right after a write — it must not write again (an unchanged value proves it).
        let f = fixture().await;
        issue(&f.store, "tok_1", Some("alice"), None, "read", None).await;
        f.store
            .update_tokens(|t| t[0].last_used = Some("2999-01-01T00:00:00Z".into()))
            .await
            .unwrap();
        touch_token(&f.store, "tok_1").await;
        assert_eq!(f.store.tokens().await[0].last_used.as_deref(), Some("2999-01-01T00:00:00Z"));
    }
}
