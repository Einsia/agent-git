//! Browser sessions (cookies) — authentication for humans. Scripts and git use tokens and never
//! touch this.
//!
//! The design trade: **no signing; an unguessable random id plus a server-side table**. Signed
//! cookies (the JWT approach) save you the table, at the cost of not being able to log out or kick
//! anyone (a signature stays valid until it expires). The session table lives in memory: a process
//! restart means everyone logs in again, which is acceptable in exchange for "revocation takes
//! effect immediately" and "there is nothing on the server to crack offline".
//!
//! The table stores the **sha256** of the sid, not the sid itself — dumping memory (core dump /
//! swap) still yields no usable cookie.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// Cookie name. A `__Host-` prefix would be better (the browser then enforces Secure + Path=/ + no
/// Domain), but it requires HTTPS, and local http://localhost development is the main path — the
/// session would simply stop working.
pub const COOKIE: &str = "agit_session";

/// Session lifetime. Enough for a working day, not so long that "forgot to log out" stays open
/// forever.
pub const TTL: Duration = Duration::from_secs(12 * 3600);

/// Cap on total sessions. Blocks "unlimited logins → unlimited memory". At the cap, sweep the
/// expired ones first; if it is still full, refuse.
const MAX_SESSIONS: usize = 4096;

struct Sess {
    user: String,
    expires: SystemTime,
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<HashMap<String, Sess>>,
}

impl Sessions {
    pub fn new() -> Sessions {
        Sessions::default()
    }

    /// Issue a new session, returning the **plaintext sid** (this once only; afterwards the server
    /// holds nothing but its digest).
    pub fn create(&self, user: &str) -> std::io::Result<String> {
        let sid = super::kdf::gen_secret()?; // 32 CSPRNG bytes = 256 bits, not guessable
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= MAX_SESSIONS {
            let now = SystemTime::now();
            m.retain(|_, s| s.expires > now);
            if m.len() >= MAX_SESSIONS {
                return Err(std::io::Error::other("session limit reached, please try again later"));
            }
        }
        m.insert(
            crate::convo::sha256_hex(&sid),
            Sess { user: user.to_string(), expires: SystemTime::now() + TTL },
        );
        Ok(sid)
    }

    /// sid → username. Expired ones are dropped on the spot and return None.
    pub fn lookup(&self, sid: &str) -> Option<String> {
        let key = crate::convo::sha256_hex(sid);
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let s = m.get(&key)?;
        if s.expires <= SystemTime::now() {
            m.remove(&key);
            return None;
        }
        Some(s.user.clone())
    }

    /// Log out: revocation takes effect immediately.
    pub fn revoke(&self, sid: &str) {
        let key = crate::convo::sha256_hex(sid);
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

/// The `Set-Cookie` value.
///   HttpOnly — JS cannot read it, so XSS cannot steal the session
///   SameSite=Lax — blocks cross-site POST (CSRF); Lax still lets top-level navigation carry the
///                  cookie, so following a link in still works
///   Secure — only added under TLS; on local http development it would make the browser drop the
///            cookie outright
///   Max-Age — kept in step with the server-side TTL
pub fn set_cookie(sid: &str, secure: bool) -> String {
    format!(
        "{COOKIE}={sid}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}{}",
        TTL.as_secs(),
        if secure { "; Secure" } else { "" }
    )
}

/// For logout: same name, empty value, Max-Age=0, so the browser throws it away at once.
pub fn clear_cookie(secure: bool) -> String {
    format!("{COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0{}", if secure { "; Secure" } else { "" })
}

/// Dig the sid out of the `Cookie:` header. The format is `a=1; b=2`.
pub fn parse_cookie(header: &str) -> Option<String> {
    header.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == COOKIE).then(|| v.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_lookup_revoke() {
        let s = Sessions::new();
        let sid = s.create("alice").unwrap();
        assert_eq!(s.lookup(&sid).as_deref(), Some("alice"));
        s.revoke(&sid);
        assert_eq!(s.lookup(&sid), None, "dead the moment you log out");
    }

    #[test]
    fn sids_are_unique_and_unguessable() {
        let s = Sessions::new();
        let a = s.create("alice").unwrap();
        let b = s.create("alice").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64, "256 bit hex");
    }

    #[test]
    fn plaintext_sid_is_not_stored() {
        // The table should hold digests only — no directly usable cookie to fish out of memory or a
        // core dump.
        let s = Sessions::new();
        let sid = s.create("alice").unwrap();
        let m = s.inner.lock().unwrap();
        assert!(!m.contains_key(&sid));
        assert!(m.contains_key(&crate::convo::sha256_hex(&sid)));
    }

    #[test]
    fn unknown_sid_is_rejected() {
        let s = Sessions::new();
        s.create("alice").unwrap();
        assert_eq!(s.lookup("deadbeef"), None);
        assert_eq!(s.lookup(""), None);
    }

    #[test]
    fn expired_session_is_rejected_and_dropped() {
        let s = Sessions::new();
        let sid = "manual";
        s.inner.lock().unwrap().insert(
            crate::convo::sha256_hex(sid),
            Sess { user: "alice".into(), expires: SystemTime::now() - Duration::from_secs(1) },
        );
        assert_eq!(s.lookup(sid), None);
        assert_eq!(s.len(), 0, "expired ones get swept along the way");
    }

    #[test]
    fn sessions_are_independent() {
        let s = Sessions::new();
        let a = s.create("alice").unwrap();
        let b = s.create("bob").unwrap();
        s.revoke(&a);
        assert_eq!(s.lookup(&a), None);
        assert_eq!(s.lookup(&b).as_deref(), Some("bob"), "revoking one must not affect the others");
    }

    #[test]
    fn cookie_flags() {
        let c = set_cookie("abc", false);
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Max-Age=43200"));
        assert!(!c.contains("Secure"), "without TLS, Secure must not be set or the browser drops it");
        assert!(set_cookie("abc", true).contains("; Secure"));
        assert!(clear_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn cookie_parsing() {
        assert_eq!(parse_cookie("agit_session=abc").as_deref(), Some("abc"));
        assert_eq!(parse_cookie("x=1; agit_session=abc; y=2").as_deref(), Some("abc"));
        assert_eq!(parse_cookie(" agit_session = abc ").as_deref(), Some("abc"));
        assert_eq!(parse_cookie("other=abc"), None);
        assert_eq!(parse_cookie(""), None);
        // Do not mistake agit_session_x for agit_session
        assert_eq!(parse_cookie("agit_session_other=abc"), None);
    }
}
