//! TOTP (RFC 6238) second factor + one-time backup codes for the hub.
//!
//! The OTP math is **not** hand-rolled: it goes through the well-reviewed, RFC-compliant `totp-rs`
//! crate. This module is a thin, testable wrapper that fixes the parameters (SHA1 / 6 digits / 30s
//! step / ±1 step skew — the interop-safe defaults every authenticator app speaks) and owns the
//! backup-code format.
//!
//! Two kinds of secret material live here, and they are hashed differently on purpose:
//!   - the **TOTP shared secret** is a symmetric key the server must keep in the clear to verify
//!     codes (it cannot be hashed — see `store::User::totp_secret`);
//!   - **backup codes** are one-shot, high-entropy (64-bit CSPRNG) strings, so — exactly like API
//!     tokens (see `kdf.rs`) — only their sha256 digest is stored, never the plaintext.

use totp_rs::{Algorithm, Secret, TOTP};

/// The issuer shown in the authenticator app (the `otpauth://totp/<ISSUER>:<account>` label and the
/// `issuer=` query parameter).
pub const ISSUER: &str = "agit-hub";
/// 6-digit codes — the universal authenticator-app default.
pub const DIGITS: usize = 6;
/// Accept the code from the previous, current, and next 30s window: ±1 step of clock skew.
pub const SKEW: u8 = 1;
/// 30-second time step (the RFC 6238 / authenticator-app default).
pub const STEP: u64 = 30;
/// How many one-time backup codes a confirm mints.
pub const BACKUP_CODES: usize = 10;
/// Hex characters of CSPRNG entropy per backup code (16 hex = 64 bits).
const BACKUP_HEX_LEN: usize = 16;

/// Generate a fresh base32-encoded TOTP secret (CSPRNG, via `totp-rs`'s `gen_secret`). This is the
/// string persisted in `User::totp_secret` and fed back into [`provisioning_uri`] / [`verify`].
pub fn gen_secret() -> String {
    Secret::generate_secret().to_encoded().to_string()
}

/// Build a verifier for a stored base32 secret bound to `account` (the username). `None` if the
/// secret does not decode — which cannot happen for a server-minted secret, so callers treat `None`
/// as "verification fails / no URI".
fn totp_for(secret_b32: &str, account: &str) -> Option<TOTP> {
    let bytes = Secret::Encoded(secret_b32.to_string()).to_bytes().ok()?;
    TOTP::new(Algorithm::SHA1, DIGITS, SKEW, STEP, bytes, Some(ISSUER.to_string()), account.to_string()).ok()
}

/// The `otpauth://totp/agit-hub:<account>?secret=...&issuer=agit-hub` provisioning URI to hand to an
/// authenticator app (issuer=`agit-hub`, account=username).
pub fn provisioning_uri(secret_b32: &str, account: &str) -> Option<String> {
    totp_for(secret_b32, account).map(|t| t.get_url())
}

/// Verify a submitted code against the secret, allowing ±1 time step (see [`SKEW`]). A malformed
/// secret or a `SystemTime` error both resolve to `false` — verification never fails *open*.
pub fn verify(secret_b32: &str, account: &str, code: &str) -> bool {
    match totp_for(secret_b32, account) {
        Some(t) => t.check_current(code.trim()).unwrap_or(false),
        None => false,
    }
}

/// The current valid code for a secret. Used only by tests (and never by a request handler — the
/// server has no reason to *produce* a user's code); returns `None` on a malformed secret or clock
/// error.
pub fn current_code(secret_b32: &str, account: &str) -> Option<String> {
    totp_for(secret_b32, account).and_then(|t| t.generate_current().ok())
}

/// Normalize a backup code for hashing/compare: trim, lowercase, drop the display dashes — so the
/// grouped form shown to the user (`abcd-ef01-2345-6789`) and the raw form both verify.
fn normalize_backup(code: &str) -> String {
    code.trim().to_ascii_lowercase().chars().filter(|c| *c != '-').collect()
}

/// The stored digest of a backup code (sha256 of the normalized form). High-entropy CSPRNG input, so
/// a fast hash is right here — the same reasoning API tokens use (see `kdf.rs`).
pub fn hash_backup_code(code: &str) -> String {
    crate::convo::sha256_hex(&normalize_backup(code))
}

/// Mint [`BACKUP_CODES`] one-time backup codes: returns `(plaintext, hashes)`. The plaintext is shown
/// to the user exactly once; only the hashes are persisted. `Err` if the CSPRNG is unavailable —
/// never fall back to a predictable value to mint credentials.
pub fn gen_backup_codes(n: usize) -> std::io::Result<(Vec<String>, Vec<String>)> {
    let mut plain = Vec::with_capacity(n);
    let mut hashes = Vec::with_capacity(n);
    for _ in 0..n {
        // Reuse the hub CSPRNG (64 hex chars); take 64 bits and group into 4-char blocks for legibility.
        let raw = super::kdf::gen_secret()?;
        let block: String = raw.chars().take(BACKUP_HEX_LEN).collect();
        let code = block.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("-");
        hashes.push(hash_backup_code(&code));
        plain.push(code);
    }
    Ok((plain, hashes))
}

/// If `code` matches one of the stored backup-code hashes, return the list with that hash removed —
/// the code is now spent (one-time use). `None` = no match, so the caller leaves the list untouched.
/// Constant-time compare per hash; consumes exactly one even in the (impossible) event of a duplicate.
pub fn consume_backup_code(code: &str, hashes: &[String]) -> Option<Vec<String>> {
    let presented = hash_backup_code(code);
    let mut matched = false;
    let mut remaining = Vec::with_capacity(hashes.len());
    for h in hashes {
        if !matched && super::kdf::ct_eq(&presented, h) {
            matched = true; // spend exactly one
            continue;
        }
        remaining.push(h.clone());
    }
    matched.then_some(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_base32_and_random() {
        let a = gen_secret();
        let b = gen_secret();
        assert_ne!(a, b, "each secret is freshly generated");
        assert!(!a.is_empty());
        // base32 (RFC 4648) alphabet, no padding in these secrets.
        assert!(a.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()), "base32: {a}");
    }

    #[test]
    fn provisioning_uri_carries_issuer_and_account() {
        let s = gen_secret();
        let uri = provisioning_uri(&s, "alice").unwrap();
        assert!(uri.starts_with("otpauth://totp/"), "{uri}");
        assert!(uri.contains("issuer=agit-hub"), "{uri}");
        assert!(uri.contains("agit-hub:alice"), "{uri}");
        assert!(uri.contains(&format!("secret={s}")), "{uri}");
    }

    #[test]
    fn current_code_verifies_and_a_wrong_one_does_not() {
        let s = gen_secret();
        let code = current_code(&s, "alice").unwrap();
        assert_eq!(code.len(), DIGITS);
        assert!(verify(&s, "alice", &code));
        // A code the server did not mint is rejected.
        let wrong = if code == "000000" { "111111" } else { "000000" };
        assert!(!verify(&s, "alice", wrong));
    }

    #[test]
    fn backup_codes_hash_never_reveals_plaintext() {
        let (plain, hashes) = gen_backup_codes(BACKUP_CODES).unwrap();
        assert_eq!(plain.len(), BACKUP_CODES);
        assert_eq!(hashes.len(), BACKUP_CODES);
        for (p, h) in plain.iter().zip(&hashes) {
            assert_ne!(p, h, "the stored value is a digest, not the plaintext");
            assert_eq!(h, &hash_backup_code(p), "digest is reproducible from the code");
            // Displayed grouped form; hashing is dash/case-insensitive.
            assert!(p.contains('-'));
            assert_eq!(hash_backup_code(&p.to_uppercase()), *h);
        }
    }

    #[test]
    fn a_backup_code_works_once_then_is_gone() {
        let (plain, hashes) = gen_backup_codes(3).unwrap();
        let remaining = consume_backup_code(&plain[1], &hashes).expect("valid code matches");
        assert_eq!(remaining.len(), 2, "exactly one consumed");
        // Re-using the same code against the reduced set no longer matches.
        assert!(consume_backup_code(&plain[1], &remaining).is_none(), "spent code is rejected");
        // The other codes still work.
        assert!(consume_backup_code(&plain[0], &remaining).is_some());
    }

    #[test]
    fn unknown_backup_code_matches_nothing() {
        let (_plain, hashes) = gen_backup_codes(3).unwrap();
        assert!(consume_backup_code("not-a-real-code", &hashes).is_none());
    }
}
