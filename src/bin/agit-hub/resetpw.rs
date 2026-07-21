//! Password-reset delivery + token helpers shared by the API handlers and the admin CLI.
//!
//! This is the self-service recovery path for a user locked out of their account (forgotten password,
//! no admin on hand). It reuses the EXACT operator-forwarded delivery shape of email verification
//! (`emailverify.rs`): a single-use, expiring token is minted, embedded in a `<base>/reset-password`
//! URL, and delivered hermetically — logged as an INFO line and printable via the admin CLI
//! (`agit-hub user reset-link <user>`). No SMTP is pulled into the hub.
//!
//! The reset token lives in its OWN store space (a distinct table + `prt_` prefix, see
//! `Store::mint_password_reset_token`), so an email-verification token can NEVER be replayed to reset a
//! password and vice-versa. Consuming a reset token authorizes setting a new password WITHOUT the old
//! one; it is the only self-service door that bypasses the current-password check, which is exactly why
//! the capability is single-use, short-lived, and never returned in an unauthenticated response body.
use std::time::Duration;

use agit::hub::store::Store;

/// How long a reset token is valid: short — a password reset is an urgent, one-shot recovery action, so
/// a tighter window than email verification (24h) limits the blast radius of a leaked-but-stale link.
pub(crate) const RESET_TTL: Duration = Duration::from_secs(60 * 60);

/// Build the reset URL a locked-out user clicks. The base comes from `AGIT_HUB_BASE_URL` when set (e.g.
/// `https://hub.corp.com`); otherwise a bare path is emitted for the operator to prefix. The token rides
/// in the query string, exactly like the verification URL.
pub(crate) fn reset_url(token: &str) -> String {
    let base = std::env::var("AGIT_HUB_BASE_URL").ok().map(|s| s.trim_end_matches('/').to_string()).unwrap_or_default();
    format!("{base}/reset-password?token={token}")
}

/// Mint a password-reset token for `username` and deliver it the operator-forwarded way (log the URL).
/// Returns the URL, or `None` on a mint failure (e.g. no system entropy). The CALLER is responsible for
/// only invoking this for an account that exists — the anti-enumeration request handler checks existence
/// first and stays silent either way, so this helper never itself discloses whether an account exists.
pub(crate) async fn mint_and_deliver(store: &Store, username: &str) -> Option<String> {
    let token = store.mint_password_reset_token(username, RESET_TTL).await.ok()?;
    let url = reset_url(&token);
    // Hermetic delivery: the operator forwards this to the locked-out user. The token is embedded in the
    // URL, so it never leaves via an unauthenticated response body.
    tracing::info!(user = %username, "password reset link (forward to the account owner to reset their password): {url}");
    Some(url)
}
