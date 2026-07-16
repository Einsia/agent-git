//! 口令派生与随机数 —— Hub 里所有"机密"的算术都在这。
//!
//! 密码**不是** sha256。sha256 是给 token 用的：token 是 32 字节 CSPRNG，本身就没得猜，
//! 摘要只为"库被读走也不直接泄露可用凭据"。密码是人选的、熵很低，必须上带盐的慢 KDF ——
//! 这里用 argon2id（内存硬，挡 GPU/ASIC 批量爆破）。
//!
//! 参数跟着 hash 一起存（`kdf` 字段，形如 `argon2id$v=19$m=19456,t=2,p=1`），于是以后调参
//! 不会把老用户锁在门外：验的时候按**存的**参数算。

use argon2::{Algorithm, Argon2, Params, Version};
use std::io::Read;

/// 派生出的 hash 长度（字节）。
const HASH_LEN: usize = 32;
/// 盐长度（字节）。argon2 要求 >= 8。
const SALT_LEN: usize = 16;

/// 当前默认参数下的 kdf 标识。新用户按它存。
pub fn current_kdf_id() -> String {
    let p = default_params();
    format!("argon2id$v=19$m={},t={},p={}", p.m_cost(), p.t_cost(), p.p_cost())
}

/// OWASP 对 argon2id 的推荐档（19 MiB / 2 轮 / 1 并行）。
fn default_params() -> Params {
    Params::default()
}

/// `argon2id$v=19$m=19456,t=2,p=1` → 可用的 Argon2 实例。认不出来就 None（宁可拒绝，不猜）。
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

/// 按给定 kdf 参数与盐派生密码 hash（hex）。参数认不出来 → None。
pub fn hash_password(password: &str, salt_hex: &str, kdf: &str) -> Option<String> {
    let salt = hex::decode(salt_hex).ok()?;
    if salt.len() < 8 {
        return None; // 盐太短 = 彩虹表又活了；宁可失败
    }
    let a = argon2_from_kdf(kdf)?;
    let mut out = [0u8; HASH_LEN];
    a.hash_password_into(password.as_bytes(), &salt, &mut out).ok()?;
    Some(hex::encode(out))
}

/// 验密码：用**存的**盐与参数重算，常数时间比。任何一步认不出来 → false（不放行）。
pub fn verify_password(password: &str, salt_hex: &str, kdf: &str, expected_hex: &str) -> bool {
    match hash_password(password, salt_hex, kdf) {
        Some(got) => ct_eq(&got, expected_hex),
        None => false,
    }
}

/// 新盐（hex）。
pub fn gen_salt() -> std::io::Result<String> {
    Ok(hex::encode(random_bytes::<SALT_LEN>()?))
}

/// 32 字节 CSPRNG → hex。拿不到 OS 熵就**报错**，绝不退回可预测的时间值来发凭据。
pub fn gen_secret() -> std::io::Result<String> {
    Ok(hex::encode(random_bytes::<32>()?))
}

fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

/// 定长常数时间比较（这里比的都是等长 hex 摘要；避免逐字节短路泄露信息）。
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
        // 回归闸：万一有人"简化"回 sha256(password)，这里会响。
        let salt = "00112233445566778899aabbccddeeff";
        let h = hash_password("hunter2", salt, &current_kdf_id()).unwrap();
        assert_ne!(h, crate::convo::sha256_hex("hunter2"));
        assert_ne!(h, crate::convo::sha256_hex(&format!("{salt}hunter2")));
    }

    #[test]
    fn salt_changes_the_hash() {
        // 同密码 + 不同盐 → 不同 hash：这是"一张彩虹表打穿整库"的解药。
        let kdf = current_kdf_id();
        let a = hash_password("hunter2", &gen_salt().unwrap(), &kdf).unwrap();
        let b = hash_password("hunter2", &gen_salt().unwrap(), &kdf).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn stored_params_are_used_not_the_current_default() {
        // 老 hash 用老参数存 —— 调默认参数不该把老用户锁在门外。
        let salt = gen_salt().unwrap();
        let weak = "argon2id$v=19$m=8,t=1,p=1";
        let h = hash_password("hunter2", &salt, weak).unwrap();
        assert!(verify_password("hunter2", &salt, weak, &h));
        // 用当前默认参数去验老 hash 必然对不上（证明参数确实参与了运算）。
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
