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

/// A sentinel target for the throwaway mint on the NONEXISTENT-account branch. The leading NUL can
/// never be a real username (usernames are `[a-z0-9._-]`), so this row never addresses a real account,
/// and it is discarded the instant it is minted, so it never accumulates.
// A username no real account can ever hold (spaces + parens are rejected by valid_username), so the
// throwaway can never collide with a real user's reset tokens, AND it is Postgres-safe: Postgres text
// rejects a NUL byte, so a NUL-prefixed sentinel silently defeated the equalizer on Pg (the mint INSERT
// errored and the miss path skipped the write, reopening the timing oracle in production).
const RESET_PROBE_TARGET: &str = "reset probe (nonexistent account)";

/// Equalize the WORK a reset request does for a NONEXISTENT account against the real (mint + deliver)
/// path, so response time cannot enumerate accounts. The hit path does a DB INSERT (mint) plus an audit
/// append (~0.1-1s); the old miss path short-circuited read-only (~0.6ms) — a ~1000x oracle despite the
/// byte-identical response body. This mints a REAL reset token to a throwaway sentinel target — the same
/// INSERT the hit path performs — then immediately DISCARDS it (consumes the row, so nothing lingers).
/// The token is NEVER logged, returned, or delivered. Returns the minted token (proof the DB write ran),
/// or `None` on a mint failure, mirroring [`mint_and_deliver`]'s own `None`.
pub(crate) async fn equalize_nonexistent(store: &Store) -> Option<String> {
    let token = store.mint_password_reset_token(RESET_PROBE_TARGET, RESET_TTL).await.ok()?;
    // Discard at once: delete the throwaway row so it never accumulates and can never be used. Its
    // username is unusable anyway (a NUL-prefixed sentinel that is no real account).
    let _ = store.consume_password_reset_token(&token).await;
    Some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn equalize_nonexistent_mints_then_discards() {
        // The miss-path equalizer must do the SAME DB write the hit path does (mint a token) but leave
        // NOTHING behind (discarded, never delivered), so a nonexistent-account request is not a fast,
        // read-only timing oracle.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_sqlite(dir.path()).await.unwrap();
        assert_eq!(store.password_reset_token_count().await, 0, "baseline: no tokens");

        let token = equalize_nonexistent(&store).await.expect("a throwaway token is minted");
        assert!(token.starts_with("prt_"), "the equalizer mints a real reset token: {token}");
        // Discarded: the throwaway row does not accumulate.
        assert_eq!(store.password_reset_token_count().await, 0, "the throwaway token is discarded, never left behind");

        // Repeated misses never accumulate rows (bounded work, no growth).
        for _ in 0..5 {
            equalize_nonexistent(&store).await.expect("mint");
        }
        assert_eq!(store.password_reset_token_count().await, 0, "repeated misses leave nothing behind");
    }
}
