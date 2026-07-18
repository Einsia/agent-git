//! Connection / login / per-token rate limits + wire size constants. Verbatim from the monolith,
//! minus the socket-timeout constants (now hyper/tower concerns).
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Largest git request body (a push pack) streamed to http-backend.
pub(crate) const MAX_BODY: usize = 512 * 1024 * 1024;
/// Body cap for the JSON API. The API only takes small objects; a 512MB allowance makes no sense.
pub(crate) const API_MAX_BODY: usize = 64 * 1024;
/// Cap on concurrent in-flight requests (was the accept-time Semaphore; now tower ConcurrencyLimitLayer).
pub(crate) const MAX_CONN: usize = 64;
/// Cap on in-flight connections from a single IP (stops one source's slowloris from filling the
/// whole pool). Half the pool, so there are slots left for everyone else.
pub(crate) const PER_IP_MAX: usize = 32;
/// How many argon2 runs may be in flight at once. Each argon2 wants 19MiB + a full core; uncapped =
/// an amplifier.
pub(crate) const LOGIN_CONC: usize = 4;
/// Cap on how much of git http-backend's CGI output we buffer looking for the header/body separator.
pub(crate) const MAX_CGI_HEADERS: usize = 64 * 1024;

/// In-flight connection count per IP.
#[derive(Default)]
pub(crate) struct ConnLimiter {
    pub(crate) map: Mutex<HashMap<IpAddr, usize>>,
}

impl ConnLimiter {
    pub(crate) fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<IpGuard> {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.entry(ip).or_insert(0);
        if *n >= PER_IP_MAX {
            if *n == 0 {
                m.remove(&ip);
            }
            return None;
        }
        *n += 1;
        Some(IpGuard { limiter: self.clone(), ip })
    }
}

/// Per-token request budget — **a different question from the per-IP cap**, which counts *concurrent
/// connections* from one address and exists to stop a slowloris filling the thread pool. This counts
/// *requests over time* from one credential, and exists because a token is a robot: a wedged CI loop
/// or a leaked token hammers the Hub from an address that may be shared (a NAT, a proxy) with people
/// who have done nothing wrong. Keying the budget on the credential charges the right party.
///
/// A token bucket, so a normal `git clone` — a burst of requests, then nothing — is unaffected,
/// while a sustained hammer settles to the refill rate.
pub(crate) struct TokenBuckets {
    pub(crate) inner: Mutex<HashMap<String, Bucket>>,
}

pub(crate) struct Bucket {
    pub(crate) tokens: f64,
    pub(crate) last: Instant,
}

/// Sustained rate, requests/second/token. A clone or a push is a handful of requests; anything
/// steadily above this is a loop, not a person.
pub(crate) const TOKEN_RATE_PER_SEC: f64 = 4.0;
/// Burst allowance. Deliberately generous: a fetch of a big store fans out into many requests at
/// once, and throttling a legitimate clone would be worse than the problem being solved.
pub(crate) const TOKEN_BURST: f64 = 240.0;

impl TokenBuckets {
    pub(crate) fn new() -> TokenBuckets {
        TokenBuckets { inner: Mutex::new(HashMap::new()) }
    }

    pub(crate) fn allow(&self, id: &str) -> bool {
        self.allow_at(id, Instant::now())
    }

    /// The clock is a parameter so the refill can be tested without sleeping.
    ///
    /// The map only ever grows a key per **authenticated** token id, so it is bounded by the number
    /// of issued tokens — an unauthenticated flood cannot make it allocate.
    pub(crate) fn allow_at(&self, id: &str, now: Instant) -> bool {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let b = m.entry(id.to_string()).or_insert(Bucket { tokens: TOKEN_BURST, last: now });
        // saturating_duration_since, not `-`: Instant subtraction panics if now precedes last, and
        // two threads can read the clock out of order.
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.last = now;
        b.tokens = (b.tokens + elapsed * TOKEN_RATE_PER_SEC).min(TOKEN_BURST);
        if b.tokens < 1.0 {
            return false;
        }
        b.tokens -= 1.0;
        true
    }
}

/// One held per-IP slot; decremented on drop (panic-safe).
pub(crate) struct IpGuard {
    pub(crate) limiter: Arc<ConnLimiter>,
    pub(crate) ip: IpAddr,
}

impl Drop for IpGuard {
    fn drop(&mut self) {
        let mut m = self.limiter.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

// The login concurrency cap is now a `tokio::sync::Semaphore` (see CtxInner::login_gate): the login
// handler runs on the async runtime and `.await`s a permit, so a std Condvar wait here would block a
// worker thread while the permit it needs is released by another async task — a self-deadlock. The
// async semaphore yields instead of blocking.
