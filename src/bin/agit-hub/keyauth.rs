//! Key-based hub auth: the anonymous challenge/response that turns "I hold the enrolled ed25519 key"
//! into a short-lived bearer token, without a copy-pasted secret. This is the HTTPS analog of pushing
//! with an SSH key on github.com — the hub has no SSH daemon, so the handshake rides git-smart-http's
//! own auth (a bearer token), the token just being auto-minted from the enrolled key.
//!
//! Two endpoints, both ANONYMOUS (they run before any caller gate, exactly like `login`/`register`):
//!   - `GET  /api/auth/challenge` mints a single-use nonce.
//!   - `POST /api/auth/key` verifies a signed assertion over that nonce and mints a minutes-lived token.
//!
//! The token is a normal row in the existing `tokens` table (see `cli::issue_token_ttl`), so
//! `auth::authenticate()` accepts it with ZERO change and NO schema migration is introduced.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use agit::hub::acl::Scope;
use agit::hub::store;

use crate::api::{is_hex_len, json_body, str_field, ED25519_PUB_HEX, ENROLL_SIG_HEX};
use crate::http::{Req, Resp};
use crate::server::Ctx;

/// How long a minted challenge nonce stays valid. Short: it only has to survive one
/// challenge -> sign -> exchange round-trip on the same machine. This is the server-side replay bound,
/// so even a client with a fast clock (a generous `expiry`) cannot outlive it.
const NONCE_TTL: Duration = Duration::from_secs(60);

/// How long a key-minted bearer token lives. MINUTES, not days: a leak is a short window, not a
/// standing secret (see the design's blast-radius note).
const TOKEN_TTL_MINUTES: i64 = 15;

/// The largest future window a client-set `expiry` may claim — clock-skew slack. An `expiry` further
/// ahead than this is refused, so a captured assertion cannot be given an absurdly long life.
const MAX_EXPIRY_AHEAD_SECS: i64 = 300;

/// The tokens-table `name` a key-minted token carries, so it is legible in the web UI token list as
/// device-key-minted and auto-expiring (rather than an anonymous `tok_...`).
const KEY_TOKEN_NAME: &str = "device-key auth (auto-expiring)";

/// A single-use, short-lived challenge-nonce store. In memory ONLY — never a table/column, so it stays
/// off the schema-migration path entirely. Keyed by the nonce hex, valued by the [`Instant`] it expires
/// at; pruned lazily on every access so a burst of unspent nonces cannot accumulate unboundedly.
pub(crate) struct AuthNonces {
    inner: Mutex<HashMap<String, Instant>>,
}

impl AuthNonces {
    pub(crate) fn new() -> Self {
        AuthNonces { inner: Mutex::new(HashMap::new()) }
    }

    /// Store a freshly minted nonce with a [`NONCE_TTL`] expiry.
    fn insert(&self, nonce: String) {
        let mut g = self.lock();
        prune(&mut g);
        g.insert(nonce, Instant::now() + NONCE_TTL);
    }

    /// Atomically CONSUME a nonce: remove-and-check under one lock hold, so a nonce can NEVER be used
    /// twice even under concurrency (two racing requests cannot both see the same entry — only the one
    /// whose `remove` returns `Some` wins). Returns true only if it was present AND not yet expired.
    fn consume(&self, nonce: &str) -> bool {
        let mut g = self.lock();
        prune(&mut g);
        match g.remove(nonce) {
            Some(expiry) => expiry > Instant::now(),
            None => false,
        }
    }

    /// Lock, recovering the inner map even if a prior holder panicked (poison): this store holds no
    /// invariant a panic could have corrupted, and a poisoned lock must not turn auth into a 500.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }
}

/// Drop every already-expired entry. Called on each access, so a stream of unconsumed challenges
/// self-cleans rather than growing without bound.
fn prune(map: &mut HashMap<String, Instant>) {
    let now = Instant::now();
    map.retain(|_, &mut expiry| expiry > now);
}

/// Unix seconds now — the granularity `expires_at` and the client `expiry` speak.
fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `GET /api/auth/challenge` (ANONYMOUS) — mint a random, single-use nonce and return
/// `{ "nonce": "<hex>", "expires_at": <unix-seconds> }`. The nonce comes from the SAME CSPRNG the token
/// mint uses (`kdf::gen_secret` -> `/dev/urandom`), never a clock-derived value, so it is unguessable.
pub(crate) fn api_auth_challenge(ctx: &Ctx) -> Resp {
    let nonce = match agit::hub::kdf::gen_secret() {
        Ok(n) => n,
        // The CSPRNG is unavailable: refuse rather than emit a predictable nonce.
        Err(_) => return Resp::err(500, "could not mint a challenge"),
    };
    ctx.auth_nonces.insert(nonce.clone());
    Resp::json(serde_json::json!({
        "nonce": nonce,
        "expires_at": unix_now() + NONCE_TTL.as_secs() as i64,
    }))
}

/// This hub's own canonical base URL — the audience a signed assertion must name. When the operator has
/// pinned `AGIT_HUB_PUBLIC_URL` (`Cfg::public_url`) that value is AUTHORITATIVE and server-controlled, so
/// it cannot be spoofed by a request header. Otherwise we fall back to `{scheme}://{Host}` reconstructed
/// from the request, where `scheme` follows `Cfg::tls` (https when TLS is terminated in front). The
/// fallback is best-effort: it still stops a naive cross-hub replay (the signed audience names hub A, so
/// it will not equal hub B's reconstructed audience) but a caller that also controls the `Host` header
/// could match it — pinning `public_url` closes that gap, which is why the deployment sets it.
fn canonical_audience(ctx: &Ctx, req: &Req) -> String {
    if let Some(u) = ctx.cfg.public_url.as_deref() {
        return u.trim_end_matches('/').to_string();
    }
    // Unpinned: the audience is reconstructed from the request Host, which a caller can set. Safe for a
    // single-hub deployment (there is no other hub to be confused with), but on a multi-hub setup a
    // signature for hub A could be replayed to an unpinned hub B. Warn ONCE so this cannot ship silently;
    // the operator closes the gap by setting AGIT_HUB_PUBLIC_URL.
    static WARN_UNPINNED: std::sync::Once = std::sync::Once::new();
    WARN_UNPINNED.call_once(|| {
        tracing::warn!(
            "key-auth audience is unpinned (AGIT_HUB_PUBLIC_URL unset); falling back to the request Host. \
             Set AGIT_HUB_PUBLIC_URL to this hub's canonical URL to bind the audience against cross-hub replay."
        );
    });
    let scheme = if ctx.cfg.tls { "https" } else { "http" };
    format!("{scheme}://{}", req.host()).trim_end_matches('/').to_string()
}

/// `POST /api/auth/key` (ANONYMOUS) with body
/// `{ "username", "ed25519_pub", "nonce", "audience", "expiry", "sig" }`.
///
/// Every failure is a clean 401/400 — never a panic, never a 500 on bad input. The checks run IN ORDER;
/// the nonce is consumed FIRST (single-use) so even a request that fails a later check has burned its
/// nonce. On success a minutes-lived, write-scoped bearer token owned by `username` is minted and
/// returned as `{ "token": "<secret>", "expires_at": <unix-seconds> }`.
pub(crate) async fn api_auth_key(ctx: &Ctx, req: &Req, body: &[u8]) -> Resp {
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let (Some(username), Some(ed25519_pub), Some(nonce), Some(audience), Some(sig)) = (
        str_field(&v, "username"),
        str_field(&v, "ed25519_pub"),
        str_field(&v, "nonce"),
        str_field(&v, "audience"),
        str_field(&v, "sig"),
    ) else {
        return Resp::err(400, "want username, ed25519_pub, nonce, audience, expiry, sig");
    };
    // expiry is client-set unix seconds; a missing/non-integer value is a 400 (read as i64).
    let Some(expiry) = v.get("expiry").and_then(|x| x.as_i64()) else {
        return Resp::err(400, "want an integer expiry (unix seconds)");
    };
    // Fixed-width shape checks BEFORE any store work, so a malformed field never reaches a lookup.
    if !is_hex_len(&ed25519_pub, ED25519_PUB_HEX) {
        return Resp::err(400, "ed25519_pub must be 32-byte hex (64 chars)");
    }
    if !is_hex_len(&sig, ENROLL_SIG_HEX) {
        return Resp::err(400, "sig must be a 64-byte ed25519 signature (128 hex chars)");
    }

    // (a) Consume the nonce FIRST, atomically. Unknown / expired / already-consumed -> 401. Doing this
    // before every other check makes a nonce strictly single-use regardless of where a request then fails.
    if !ctx.auth_nonces.consume(&nonce) {
        return Resp::err(401, "challenge nonce is unknown, expired, or already used");
    }
    // (b) The assertion must be addressed to THIS hub (anti cross-hub replay). See `canonical_audience`.
    if audience.trim_end_matches('/') != canonical_audience(ctx, req) {
        return Resp::err(401, "audience does not match this hub");
    }
    // (c) The client-set expiry must be in the near future: past -> already dead, too far ahead -> absurd.
    let now = unix_now();
    if expiry <= now || expiry > now + MAX_EXPIRY_AHEAD_SECS {
        return Resp::err(401, "assertion expiry is in the past or too far ahead");
    }
    // (d) The key must be ENROLLED under this account and NOT revoked. `list_identity_keys` returns only
    // non-revoked keys, so a revoked (or never-enrolled) fingerprint simply is not found -> 401.
    let key_fpr = store::ed25519_fingerprint(&ed25519_pub);
    if !ctx.store.list_identity_keys(&username).await.iter().any(|k| k.key_fpr == key_fpr) {
        return Resp::err(401, "no enrolled device key for this account matches");
    }
    // Owner missing or disabled (admin-suspended) -> 401, mirroring how `authenticate()` treats a
    // suspended owner: a disabled account must not be able to mint fresh credentials.
    match ctx.store.user(&username).await {
        Some(u) if !u.disabled => {}
        _ => return Resp::err(401, "account is unavailable"),
    }
    // (e) Verify the signature over the CANONICAL assertion bytes. The `agit-hub-auth-v1` prefix
    // domain-separates these from the enroll message, so an enroll signature can never verify here.
    let msg = agit::agent::identity_auth_message(&audience, &username, &ed25519_pub, &nonce, expiry);
    if !agit::agent::verify_hex(&ed25519_pub, &msg, &sig) {
        return Resp::err(401, "signature does not verify against the presented key");
    }

    // (f) Mint a short-lived (minutes) write-scoped token owned by the account, through the SAME
    // tokens-table INSERT the CLI uses. No agent binding: it authenticates the account for any push it is
    // otherwise allowed. Legible in the UI via `KEY_TOKEN_NAME`.
    let ttl = chrono::Duration::minutes(TOKEN_TTL_MINUTES);
    match crate::cli::issue_token_ttl(&ctx.store, KEY_TOKEN_NAME, &username, None, Scope::Write, Some(ttl)).await {
        Ok(secret) => Resp::json(serde_json::json!({
            "token": secret,
            "expires_at": unix_now() + TOKEN_TTL_MINUTES * 60,
        })),
        Err(_) => Resp::err(500, "could not mint a token"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::sync::Arc;

    use agit::agent;
    use agit::hub::blob::Blobs;
    use agit::hub::session::Sessions;
    use agit::hub::store::{IdentityKey, Store, User};
    use agit::hub::{auth, kdf};
    use ed25519_dalek::SigningKey;

    use crate::limits::{ConnLimiter, TokenBuckets, LOGIN_CONC};
    use crate::server::{Cfg, CtxInner, EscrowKeypair};

    struct Fixture {
        _dir: tempfile::TempDir,
        ctx: Ctx,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_sqlite(dir.path()).await.unwrap();
        let blobs = Blobs::open(dir.path()).await.unwrap();
        let cfg = Cfg {
            host: "127.0.0.1".parse::<IpAddr>().unwrap(),
            port: 8177,
            tls: false,
            insecure: false,
            trusted_proxies: vec![],
            registration: false,
            public_url: None,
        };
        let ctx = Ctx(Arc::new(CtxInner {
            store,
            blobs,
            cfg,
            sessions: Sessions::new(),
            limiter: Arc::new(ConnLimiter::default()),
            login_gate: Arc::new(tokio::sync::Semaphore::new(LOGIN_CONC)),
            token_rl: Arc::new(TokenBuckets::new()),
            register_rl: Arc::new(TokenBuckets::new()),
            metrics: Arc::new(agit::hub::metrics::Metrics::new()),
            escrow: EscrowKeypair {
                secret: [7u8; 32],
                public: agit::agent::x25519_public_from_secret(&[7u8; 32]),
            },
            auth_nonces: Arc::new(AuthNonces::new()),
        }));
        Fixture { _dir: dir, ctx }
    }

    /// A signing key from a throwaway home dir, and its ed25519 public as hex.
    fn a_key(dir: &std::path::Path) -> (SigningKey, String) {
        let sk = agent::load_or_create_signing_key(dir).unwrap();
        let pub_hex = hex::encode(sk.verifying_key().to_bytes());
        (sk, pub_hex)
    }

    async fn add_user(store: &Store, name: &str) {
        let salt = kdf::gen_salt().unwrap();
        let kdf_id = kdf::current_kdf_id();
        store
            .add_user(User {
                username: name.into(),
                pw_hash: kdf::hash_password("password-123", &salt, &kdf_id).unwrap(),
                salt,
                kdf: kdf_id,
                is_admin: false,
                created: store::now_iso(),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    /// Enroll `pub_hex` under `username` directly in the store (bypassing the enroll HTTP handler — the
    /// trust root here is the enrolled row, which is what key-auth reads).
    async fn enroll(store: &Store, username: &str, pub_hex: &str) -> String {
        let key_fpr = store::ed25519_fingerprint(pub_hex);
        let row = IdentityKey {
            username: username.into(),
            key_fpr: key_fpr.clone(),
            ed25519_pub: pub_hex.into(),
            x25519_pub: hex::encode([0u8; 32]),
            label: "test-device".into(),
            epoch: 1,
            enroll_sig: String::new(),
            created: store::now_iso(),
            revoked: None,
            email: String::new(),
        };
        store.add_identity_key(row).await.unwrap();
        key_fpr
    }

    /// A POST /api/auth/key `Req` view whose Host makes the fallback audience `http://localhost:8177`.
    fn key_req() -> Req {
        Req {
            method: "POST".into(),
            target: "/api/auth/key".into(),
            headers: vec![("host".into(), "localhost:8177".into())],
            content_length: 0,
        }
    }

    const AUDIENCE: &str = "http://localhost:8177";

    /// Mint a challenge and return its nonce.
    fn challenge(ctx: &Ctx) -> String {
        let r = api_auth_challenge(ctx);
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        v["nonce"].as_str().unwrap().to_string()
    }

    /// Build the signed key-auth body for a given key/nonce/audience/expiry.
    fn key_body(sk: &SigningKey, username: &str, pub_hex: &str, nonce: &str, audience: &str, expiry: i64) -> Vec<u8> {
        let msg = agent::identity_auth_message(audience, username, pub_hex, nonce, expiry);
        let sig = agent::sign_hex(sk, &msg);
        serde_json::to_vec(&serde_json::json!({
            "username": username,
            "ed25519_pub": pub_hex,
            "nonce": nonce,
            "audience": audience,
            "expiry": expiry,
            "sig": sig,
        }))
        .unwrap()
    }

    fn token_from(resp: &Resp) -> String {
        assert_eq!(resp.status, 200, "expected a minted token, body: {}", String::from_utf8_lossy(&resp.body));
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        v["token"].as_str().unwrap().to_string()
    }

    /// The happy path end-to-end: a valid signature over a fresh nonce mints a usable token, and that
    /// token then authenticates a subsequent request AS the account through `authenticate()`.
    #[tokio::test]
    async fn valid_signature_mints_a_token_that_authenticates() {
        let f = fixture().await;
        let kd = tempfile::tempdir().unwrap();
        let (sk, pub_hex) = a_key(kd.path());
        add_user(&f.ctx.store, "alice").await;
        enroll(&f.ctx.store, "alice", &pub_hex).await;

        let nonce = challenge(&f.ctx);
        let expiry = unix_now() + 120;
        let body = key_body(&sk, "alice", &pub_hex, &nonce, AUDIENCE, expiry);
        let resp = api_auth_key(&f.ctx, &key_req(), &body).await;
        let token = token_from(&resp);

        // The minted token authenticates as alice through the UNCHANGED token auth path.
        let authn = auth::authenticate(&f.ctx.store, &f.ctx.sessions, None, &[token]).await;
        assert_eq!(authn.caller.user.as_deref(), Some("alice"));
        assert_eq!(authn.caller.token.as_ref().map(|g| g.scope), Some(Scope::Write));
    }

    /// The minted token's expiry is MINUTES, not days: it lands within a tight bound around
    /// `now + TOKEN_TTL_MINUTES`, nowhere near a day out.
    #[tokio::test]
    async fn minted_token_expiry_is_minutes_not_days() {
        let f = fixture().await;
        let kd = tempfile::tempdir().unwrap();
        let (sk, pub_hex) = a_key(kd.path());
        add_user(&f.ctx.store, "alice").await;
        enroll(&f.ctx.store, "alice", &pub_hex).await;

        let nonce = challenge(&f.ctx);
        let body = key_body(&sk, "alice", &pub_hex, &nonce, AUDIENCE, unix_now() + 120);
        let resp = api_auth_key(&f.ctx, &key_req(), &body).await;
        let _ = token_from(&resp);

        // Read the stored token row and parse its expiry.
        let toks = f.ctx.store.tokens().await;
        let tok = toks.iter().find(|t| t.name == KEY_TOKEN_NAME).expect("the minted token is in the table");
        let expires = tok.expires.as_deref().expect("a key-minted token must have an expiry");
        let exp = chrono::NaiveDateTime::parse_from_str(expires, "%Y-%m-%dT%H:%M:%SZ").unwrap().and_utc();
        let delta = (exp - chrono::Utc::now()).num_seconds();
        // Within a minute of the 15-minute TTL, and FAR under a day (86_400s).
        assert!((TOKEN_TTL_MINUTES * 60 - 60..=TOKEN_TTL_MINUTES * 60 + 60).contains(&delta), "delta was {delta}s");
        assert!(delta < 3_600, "a minutes-lived token must expire in under an hour, got {delta}s");
    }

    /// A nonce that has already expired is rejected (no token minted).
    #[tokio::test]
    async fn expired_nonce_is_rejected() {
        let f = fixture().await;
        let kd = tempfile::tempdir().unwrap();
        let (sk, pub_hex) = a_key(kd.path());
        add_user(&f.ctx.store, "alice").await;
        enroll(&f.ctx.store, "alice", &pub_hex).await;

        // Insert a nonce that is already in the past, then try to use it.
        let nonce = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string();
        f.ctx.auth_nonces.lock().insert(nonce.clone(), Instant::now() - Duration::from_secs(1));
        let body = key_body(&sk, "alice", &pub_hex, &nonce, AUDIENCE, unix_now() + 120);
        let resp = api_auth_key(&f.ctx, &key_req(), &body).await;
        assert_eq!(resp.status, 401);
    }

    /// A nonce is single-use: the SECOND exchange over the same nonce fails, even with a valid signature.
    #[tokio::test]
    async fn reused_nonce_is_rejected() {
        let f = fixture().await;
        let kd = tempfile::tempdir().unwrap();
        let (sk, pub_hex) = a_key(kd.path());
        add_user(&f.ctx.store, "alice").await;
        enroll(&f.ctx.store, "alice", &pub_hex).await;

        let nonce = challenge(&f.ctx);
        let body = key_body(&sk, "alice", &pub_hex, &nonce, AUDIENCE, unix_now() + 120);
        // First use: succeeds.
        let first = api_auth_key(&f.ctx, &key_req(), &body).await;
        assert_eq!(first.status, 200);
        // Second use of the SAME nonce: rejected.
        let second = api_auth_key(&f.ctx, &key_req(), &body).await;
        assert_eq!(second.status, 401);
    }

    /// A wrong audience (a signature made for a different hub) is rejected.
    #[tokio::test]
    async fn wrong_audience_is_rejected() {
        let f = fixture().await;
        let kd = tempfile::tempdir().unwrap();
        let (sk, pub_hex) = a_key(kd.path());
        add_user(&f.ctx.store, "alice").await;
        enroll(&f.ctx.store, "alice", &pub_hex).await;

        let nonce = challenge(&f.ctx);
        let body = key_body(&sk, "alice", &pub_hex, &nonce, "https://evil.example.com", unix_now() + 120);
        let resp = api_auth_key(&f.ctx, &key_req(), &body).await;
        assert_eq!(resp.status, 401);
    }

    /// A past expiry is rejected.
    #[tokio::test]
    async fn past_expiry_is_rejected() {
        let f = fixture().await;
        let kd = tempfile::tempdir().unwrap();
        let (sk, pub_hex) = a_key(kd.path());
        add_user(&f.ctx.store, "alice").await;
        enroll(&f.ctx.store, "alice", &pub_hex).await;

        let nonce = challenge(&f.ctx);
        let body = key_body(&sk, "alice", &pub_hex, &nonce, AUDIENCE, unix_now() - 10);
        let resp = api_auth_key(&f.ctx, &key_req(), &body).await;
        assert_eq!(resp.status, 401);
    }

    /// An unenrolled fingerprint (a valid signature by a key that account never enrolled) is rejected.
    #[tokio::test]
    async fn unenrolled_fingerprint_is_rejected() {
        let f = fixture().await;
        let kd = tempfile::tempdir().unwrap();
        let (sk, pub_hex) = a_key(kd.path());
        add_user(&f.ctx.store, "alice").await;
        // NOTE: no enroll for this key.

        let nonce = challenge(&f.ctx);
        let body = key_body(&sk, "alice", &pub_hex, &nonce, AUDIENCE, unix_now() + 120);
        let resp = api_auth_key(&f.ctx, &key_req(), &body).await;
        assert_eq!(resp.status, 401);
    }

    /// A REVOKED key can no longer mint a token, even with a valid signature.
    #[tokio::test]
    async fn revoked_key_is_rejected() {
        let f = fixture().await;
        let kd = tempfile::tempdir().unwrap();
        let (sk, pub_hex) = a_key(kd.path());
        add_user(&f.ctx.store, "alice").await;
        let key_fpr = enroll(&f.ctx.store, "alice", &pub_hex).await;
        assert!(f.ctx.store.revoke_identity_key("alice", &key_fpr).await.unwrap());

        let nonce = challenge(&f.ctx);
        let body = key_body(&sk, "alice", &pub_hex, &nonce, AUDIENCE, unix_now() + 120);
        let resp = api_auth_key(&f.ctx, &key_req(), &body).await;
        assert_eq!(resp.status, 401);
    }

    /// A disabled (admin-suspended) owner cannot mint a token, mirroring `authenticate()`.
    #[tokio::test]
    async fn disabled_owner_is_rejected() {
        let f = fixture().await;
        let kd = tempfile::tempdir().unwrap();
        let (sk, pub_hex) = a_key(kd.path());
        add_user(&f.ctx.store, "alice").await;
        enroll(&f.ctx.store, "alice", &pub_hex).await;
        assert!(f.ctx.store.set_user_disabled("alice", true).await.unwrap());

        let nonce = challenge(&f.ctx);
        let body = key_body(&sk, "alice", &pub_hex, &nonce, AUDIENCE, unix_now() + 120);
        let resp = api_auth_key(&f.ctx, &key_req(), &body).await;
        assert_eq!(resp.status, 401);
    }

    /// A signature made by a DIFFERENT key (over the right assertion, for an enrolled key's pubkey) is
    /// rejected: the `sig` must be by the private half of the presented `ed25519_pub`.
    #[tokio::test]
    async fn signature_by_a_different_key_is_rejected() {
        let f = fixture().await;
        let kd = tempfile::tempdir().unwrap();
        let (_sk, pub_hex) = a_key(kd.path());
        add_user(&f.ctx.store, "alice").await;
        enroll(&f.ctx.store, "alice", &pub_hex).await;

        // A second, UNRELATED key signs the assertion, but the body still presents alice's enrolled pubkey.
        let other = tempfile::tempdir().unwrap();
        let (other_sk, _other_pub) = a_key(other.path());
        let nonce = challenge(&f.ctx);
        let expiry = unix_now() + 120;
        let msg = agent::identity_auth_message(AUDIENCE, "alice", &pub_hex, &nonce, expiry);
        let sig = agent::sign_hex(&other_sk, &msg); // wrong signer
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "alice",
            "ed25519_pub": pub_hex,
            "nonce": nonce,
            "audience": AUDIENCE,
            "expiry": expiry,
            "sig": sig,
        }))
        .unwrap();
        let resp = api_auth_key(&f.ctx, &key_req(), &body).await;
        assert_eq!(resp.status, 401);
    }

    /// DOMAIN SEPARATION: a signature over the ENROLL message (same fields it shares) replayed as the
    /// auth `sig` must NOT verify. This is the cross-protocol replay the `agit-hub-auth-v1` prefix blocks.
    #[tokio::test]
    async fn enroll_signature_replayed_as_auth_is_rejected() {
        let f = fixture().await;
        let kd = tempfile::tempdir().unwrap();
        let (sk, pub_hex) = a_key(kd.path());
        add_user(&f.ctx.store, "alice").await;
        enroll(&f.ctx.store, "alice", &pub_hex).await;

        let nonce = challenge(&f.ctx);
        let expiry = unix_now() + 120;
        // Sign an ENROLL message (a different domain), then submit it as the auth `sig`.
        let x_pub = hex::encode(agent::x25519_public_from_secret(&agent::derive_x25519_secret(&sk)));
        let enroll_msg = agent::identity_enroll_message("alice", expiry, &pub_hex, &x_pub);
        let enroll_sig = agent::sign_hex(&sk, &enroll_msg);
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "alice",
            "ed25519_pub": pub_hex,
            "nonce": nonce,
            "audience": AUDIENCE,
            "expiry": expiry,
            "sig": enroll_sig,
        }))
        .unwrap();
        let resp = api_auth_key(&f.ctx, &key_req(), &body).await;
        assert_eq!(resp.status, 401);
    }

    /// Malformed input never panics or 500s: a non-JSON body and a short pubkey are clean 400s.
    #[tokio::test]
    async fn malformed_input_is_a_clean_400() {
        let f = fixture().await;
        let bad = api_auth_key(&f.ctx, &key_req(), b"not json").await;
        assert_eq!(bad.status, 400);

        let short = serde_json::to_vec(&serde_json::json!({
            "username": "alice",
            "ed25519_pub": "abcd",
            "nonce": "x",
            "audience": AUDIENCE,
            "expiry": unix_now() + 60,
            "sig": "00",
        }))
        .unwrap();
        let resp = api_auth_key(&f.ctx, &key_req(), &short).await;
        assert_eq!(resp.status, 400);
    }

    /// The challenge endpoint mints distinct nonces and stores them; a consumed nonce leaves the store.
    #[tokio::test]
    async fn challenge_mints_and_consume_is_single_use() {
        let f = fixture().await;
        let n1 = challenge(&f.ctx);
        let n2 = challenge(&f.ctx);
        assert_ne!(n1, n2, "each challenge is a distinct nonce");
        assert_eq!(f.ctx.auth_nonces.len(), 2);
        assert!(f.ctx.auth_nonces.consume(&n1));
        assert!(!f.ctx.auth_nonces.consume(&n1), "a nonce cannot be consumed twice");
        assert_eq!(f.ctx.auth_nonces.len(), 1, "consuming removes the entry");
    }
}
