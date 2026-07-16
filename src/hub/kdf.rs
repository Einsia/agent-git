//! Password derivation and randomness — all of the Hub's "secret" arithmetic lives here.
//!
//! Passwords are **not** sha256. sha256 is for tokens: a token is 32 CSPRNG bytes, unguessable on
//! its own, and the digest only buys "reading the store does not hand out usable credentials".
//! Passwords are human-chosen and low-entropy, so they demand a salted slow KDF — argon2id here
//! (memory-hard, which blunts bulk GPU/ASIC cracking).
//!
//! The parameters are stored alongside the hash (the `kdf` field, shaped like
//! `argon2id$v=19$m=19456,t=2,p=1`), so retuning them later will not lock existing users out:
//! verification computes with the **stored** parameters.

use argon2::{Algorithm, Argon2, Params, Version};
use std::io::Read;

/// Length of the derived hash (bytes).
const HASH_LEN: usize = 32;
/// Salt length (bytes). argon2 requires >= 8.
const SALT_LEN: usize = 16;

/// The kdf id for the current default parameters. New users are stored with it.
pub fn current_kdf_id() -> String {
    let p = default_params();
    format!("argon2id$v=19$m={},t={},p={}", p.m_cost(), p.t_cost(), p.p_cost())
}

/// OWASP's recommended argon2id tier (19 MiB / 2 passes / 1 lane).
fn default_params() -> Params {
    Params::default()
}

/// `argon2id$v=19$m=19456,t=2,p=1` → a usable Argon2 instance. Unrecognized → None (refuse rather
/// than guess).
fn argon2_from_kdf(kdf: &str) -> Option<Argon2<'static>> {
    let mut it = kdf.split('$');
    if it.next()? != "argon2id" {
        return None;
    }
    let version = match it.next()? {
        "v=19" => Version::V0x13,
        _ => return None,
    };
    let (mut m, mut t, mut p) = (None, None, None);
    for kv in it.next()?.split(',') {
        let (k, v) = kv.split_once('=')?;
        let v: u32 = v.parse().ok()?;
        match k {
            "m" => m = Some(v),
            "t" => t = Some(v),
            "p" => p = Some(v),
            _ => return None,
        }
    }
    let params = Params::new(m?, t?, p?, Some(HASH_LEN)).ok()?;
    Some(Argon2::new(Algorithm::Argon2id, version, params))
}

/// Derive the password hash (hex) with the given kdf parameters and salt. Unrecognized params → None.
pub fn hash_password(password: &str, salt_hex: &str, kdf: &str) -> Option<String> {
    let salt = hex::decode(salt_hex).ok()?;
    if salt.len() < 8 {
        return None; // A too-short salt brings rainbow tables back to life; fail instead
    }
    let a = argon2_from_kdf(kdf)?;
    let mut out = [0u8; HASH_LEN];
    a.hash_password_into(password.as_bytes(), &salt, &mut out).ok()?;
    Some(hex::encode(out))
}

/// Verify a password: recompute with the **stored** salt and parameters, compare in constant time.
/// Anything unrecognized along the way → false (no admission).
pub fn verify_password(password: &str, salt_hex: &str, kdf: &str, expected_hex: &str) -> bool {
    match hash_password(password, salt_hex, kdf) {
        Some(got) => ct_eq(&got, expected_hex),
        None => false,
    }
}

/// A fresh salt (hex).
pub fn gen_salt() -> std::io::Result<String> {
    Ok(hex::encode(random_bytes::<SALT_LEN>()?))
}

/// 32 CSPRNG bytes → hex. If OS entropy is unavailable this **errors**; it never falls back to a
/// predictable time-derived value to mint credentials.
pub fn gen_secret() -> std::io::Result<String> {
    Ok(hex::encode(random_bytes::<32>()?))
}

fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

/// Fixed-length constant-time comparison (everything compared here is an equal-length hex digest;
/// avoids leaking information by short-circuiting byte by byte).
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let salt = gen_salt().unwrap();
        let kdf = current_kdf_id();
        let h = hash_password("correct horse battery staple", &salt, &kdf).unwrap();
        assert!(verify_password("correct horse battery staple", &salt, &kdf, &h));
        assert!(!verify_password("Correct horse battery staple", &salt, &kdf, &h));
        assert!(!verify_password("", &salt, &kdf, &h));
    }

    #[test]
    fn hash_is_not_a_bare_sha256() {
        // Regression gate: if anyone "simplifies" this back to sha256(password), this fires.
        let salt = "00112233445566778899aabbccddeeff";
        let h = hash_password("hunter2", salt, &current_kdf_id()).unwrap();
        assert_ne!(h, crate::convo::sha256_hex("hunter2"));
        assert_ne!(h, crate::convo::sha256_hex(&format!("{salt}hunter2")));
    }

    #[test]
    fn salt_changes_the_hash() {
        // Same password + different salt → different hash: the antidote to one rainbow table
        // cracking the whole store.
        let kdf = current_kdf_id();
        let a = hash_password("hunter2", &gen_salt().unwrap(), &kdf).unwrap();
        let b = hash_password("hunter2", &gen_salt().unwrap(), &kdf).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn stored_params_are_used_not_the_current_default() {
        // Old hashes are stored with old parameters — retuning the defaults must not lock existing
        // users out.
        let salt = gen_salt().unwrap();
        let weak = "argon2id$v=19$m=8,t=1,p=1";
        let h = hash_password("hunter2", &salt, weak).unwrap();
        assert!(verify_password("hunter2", &salt, weak, &h));
        // Verifying an old hash with the current defaults must fail (proves the params really do
        // feed the computation).
        assert!(!verify_password("hunter2", &salt, &current_kdf_id(), &h));
    }

    #[test]
    fn unknown_kdf_never_verifies() {
        let salt = gen_salt().unwrap();
        assert!(hash_password("x", &salt, "sha256").is_none());
        assert!(hash_password("x", &salt, "argon2i$v=19$m=8,t=1,p=1").is_none());
        assert!(hash_password("x", &salt, "argon2id$v=16$m=8,t=1,p=1").is_none());
        assert!(!verify_password("x", &salt, "sha256", &crate::convo::sha256_hex("x")));
    }

    #[test]
    fn short_salt_is_refused() {
        assert!(hash_password("x", "0011", &current_kdf_id()).is_none());
        assert!(hash_password("x", "", &current_kdf_id()).is_none());
    }

    #[test]
    fn secrets_are_random_and_hex() {
        let a = gen_secret().unwrap();
        let b = gen_secret().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn ct_eq_matches_eq() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(ct_eq("", ""));
    }
}
