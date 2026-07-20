//! Email-verification delivery + token helpers shared by the API handlers and the admin CLI.
//!
//! Verification is the anti-squatting gate: a session's committer email is attributed to a registered
//! account (a `VerifiedAs` provenance verdict) ONLY once that account has PROVEN control of the address.
//! The account's email of record lives in the shared identity registry (self-asserted at enroll); the
//! per-account `email_verified` flag on the users table is what this flow flips.
//!
//! DELIVERY is deliberately pluggable and defaults to **operator-forwarded** so the hub stays hermetic:
//!   (a) an INFO log line carrying the verification URL (token embedded), and
//!   (b) the admin CLI `agit-hub user verify-link <user>`, which prints the current URL to forward.
//! Real SMTP delivery is an operator/followup concern: when `AGIT_HUB_SMTP_URL` is set a production
//! deployment would hand the URL to an SMTP client here — intentionally NOT implemented now, so no heavy
//! SMTP crate is pulled into the hermetic hub. The token is NEVER returned to an unauthenticated
//! registrant in an API response — that would defeat verification.
use std::time::Duration;

use agit::hub::store::Store;

/// How long a verification token is valid: long enough for an operator to forward the link and a human
/// to click it, short enough that a leaked-but-stale link is already dead.
pub(crate) const VERIFY_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Build the verification URL a user clicks (or pastes into the Account page). The base comes from
/// `AGIT_HUB_BASE_URL` when set (e.g. `https://hub.corp.com`); otherwise a bare path is emitted for the
/// operator to prefix with wherever the UI is served. The token rides in the query string.
pub(crate) fn verification_url(token: &str) -> String {
    let base = std::env::var("AGIT_HUB_BASE_URL").ok().map(|s| s.trim_end_matches('/').to_string()).unwrap_or_default();
    format!("{base}/verify-email?token={token}")
}

/// Mint a verification token for `username`'s CURRENT registered email (from the identity registry) and
/// deliver it the operator-forwarded way (log the URL). Returns the URL, or `None` when the account has
/// no email on file yet — nothing to verify, so the token is minted lazily once an email is enrolled or
/// an explicit resend is requested.
pub(crate) async fn mint_and_deliver(store: &Store, username: &str) -> Option<String> {
    let email = store.get_identity_key(username).await.map(|k| k.email).unwrap_or_default();
    deliver(store, username, &email).await
}

/// Mint + deliver a token for an EXPLICIT `email` (e.g. the address just enrolled), skipping the registry
/// re-read. `None` on a blank email or a mint failure.
pub(crate) async fn deliver(store: &Store, username: &str, email: &str) -> Option<String> {
    if email.trim().is_empty() {
        return None;
    }
    let token = store.mint_email_token(username, email, VERIFY_TTL).await.ok()?;
    let url = verification_url(&token);
    // Hermetic delivery: the operator forwards this to the address being proven. The token is embedded
    // in the URL, so it never leaves via an unauthenticated response body.
    tracing::info!(user = %username, email = %email, "email verification link (forward to the address to verify it): {url}");
    Some(url)
}
