//! Opt-in, convergent at-rest encryption for the agent store (Feature C).
//!
//! A transparent git clean/smudge filter, git-crypt style: plaintext jsonl lives in the working tree,
//! ciphertext is what git commits and pushes. On checkout the smudge filter decrypts; on staging the
//! clean filter encrypts. This makes a store pushed to a PUBLIC git remote WITH NO HUB ciphertext, not
//! plaintext transcripts. It is opt-in and only coherent for the no-hub case (the hub never holds the
//! key, so it cannot render or server-side-scan an encrypted store).
//!
//! ## Why convergent (deterministic) encryption
//!
//! A git clean filter MUST be a pure function of its input, or git reports the working file as
//! perpetually modified (it re-cleans the smudged tree to compare against the committed blob). We get
//! this exactly as git-crypt does: the AEAD nonce is derived deterministically from the plaintext via a
//! keyed MAC, so `clean(x)` has no internal randomness and always yields the identical bytes. The known,
//! accepted tradeoff (documented in the design): identical plaintexts encrypt to identical ciphertexts,
//! which leaks equality and approximate length to an observer of the public remote. Accepted for this
//! threat model ("someone who can read a public git remote"); the goal is confidentiality of transcript
//! CONTENT, not traffic analysis. This is git-crypt's posture.
//!
//! ## Crypto
//!
//! * AEAD: XChaCha20-Poly1305 (RustCrypto, pure-Rust, hermetic). 256-bit key, 24-byte nonce, 16-byte tag.
//!   Chosen over ChaCha20-Poly1305 (96-bit nonce) because the 192-bit nonce is derived, not random, and
//!   the wider space makes any residual collision math irrelevant; chosen over AES-GCM for nonce-reuse
//!   robustness and to avoid system OpenSSL.
//! * Key derivation: on-disk master = 32 random bytes (hex, 0600), minted once, mirroring the ed25519
//!   identity precedent. Two subkeys are split from it with HKDF-SHA256 under domain-separated `info`:
//!   `K_enc = HKDF(master, "agit-crypt/v1/enc")` keys the AEAD, `K_mac = HKDF(master, "agit-crypt/v1/nonce")`
//!   keys the nonce PRF. The two purposes never share key bytes.
//! * Convergent nonce: `nonce = HMAC-SHA256(K_mac, plaintext)[..24]`.
//!
//! ## Wire format of a clean-filtered blob
//!
//! `MAGIC (b"AGITCRYPT\0", 10 bytes) ‖ version u8=1 ‖ 24-byte nonce ‖ ciphertext‖tag`.
//!
//! The magic+version make smudge self-describing so it can pass through content that isn't agit-crypt
//! output (pre-encryption blobs, an un-wired clone). The trailing NUL in the magic is load-bearing: it
//! guarantees the committed blob contains a NUL, so the secret gate's `is_probably_binary` skips it
//! (ciphertext is high-entropy and would otherwise trip the entropy rule on every encrypted session and
//! block every commit/push). Any bit-flip in nonce or ciphertext fails Poly1305, so smudge exits nonzero.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Self-describing header magic. The trailing NUL is deliberate — see the module docs and the secret
/// gate interaction: it guarantees the committed blob trips `is_probably_binary`.
pub const MAGIC: &[u8] = b"AGITCRYPT\x00";
/// Wire-format version. Bumping it lets a future scheme coexist; smudge refuses versions it can't read.
pub const VERSION: u8 = 1;

const NONCE_LEN: usize = 24;
/// magic (10) + version (1)
const HEADER_LEN: usize = 10 + 1;

/// The derived working keys: never persisted, always recomputed from the on-disk master.
#[derive(Clone)]
pub struct Keys {
    /// Keys the XChaCha20-Poly1305 AEAD.
    pub enc: [u8; 32],
    /// Keys the HMAC-SHA256 convergent-nonce PRF.
    pub mac: [u8; 32],
}

/// `$AGIT_HOME/crypt/` — where the machine-global symmetric key lives, spanning every store.
fn crypt_dir(home: &Path) -> PathBuf {
    home.join("crypt")
}

/// The symmetric master key, stored 0600. Hex of 32 raw random bytes, exactly like the ed25519 key.
pub fn key_path(home: &Path) -> PathBuf {
    crypt_dir(home).join("agit-crypt.key")
}

/// Read + hex-decode + validate the 32-byte master, or `None` if the file is absent.
///
/// A corrupt (bad hex / wrong length) key is an error, not `None`: a wired filter with a garbled key
/// must fail loudly, never mint a fresh one and silently strand every blob the old one encrypted.
pub fn read_master(home: &Path) -> Result<Option<[u8; 32]>> {
    let path = key_path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let raw = hex::decode(text.trim())
        .with_context(|| format!("{} is not valid hex — the crypt key is corrupt", path.display()))?;
    let bytes: [u8; 32] = raw
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("{} is not a 32-byte key", path.display()))?;
    Ok(Some(bytes))
}

/// Load the master key, minting it once on first use (0600) and persisting before returning.
///
/// A key already on disk is reused verbatim: rotating it would strand every blob it ever encrypted.
/// Mirrors `agent::load_or_create_signing_key`.
pub fn load_or_create_master(home: &Path) -> Result<[u8; 32]> {
    if let Some(k) = read_master(home)? {
        return Ok(k);
    }
    std::fs::create_dir_all(crypt_dir(home))
        .with_context(|| format!("cannot create {}", crypt_dir(home).display()))?;
    let mut key = [0u8; 32];
    use rand::RngCore;
    rand::rngs::OsRng
        .try_fill_bytes(&mut key)
        .map_err(|e| anyhow::anyhow!("could not gather OS randomness for the crypt key: {e}"))?;
    crate::agent::write_secret_0600(&key_path(home), &hex::encode(key))?;
    Ok(key)
}

/// Split the 32-byte master into the two domain-separated subkeys with HKDF-SHA256.
///
/// `K_enc` and `K_mac` come from the same extract but different `info`, so they never share key bytes.
pub fn derive_subkeys(master: &[u8; 32]) -> Keys {
    let hk = Hkdf::<Sha256>::new(None, master);
    let mut enc = [0u8; 32];
    hk.expand(b"agit-crypt/v1/enc", &mut enc)
        .expect("32 is a valid HKDF-SHA256 output length");
    let mut mac = [0u8; 32];
    hk.expand(b"agit-crypt/v1/nonce", &mut mac)
        .expect("32 is a valid HKDF-SHA256 output length");
    Keys { enc, mac }
}

/// The keys the filter uses, requiring an EXISTING master (never minting on the filter path).
///
/// clean and smudge run under `required=true`: if the key is missing, git must ABORT rather than stage
/// plaintext or check out ciphertext, so a missing key is a loud error here, not a fresh mint.
pub fn keys_for_filter(home: &Path) -> Result<Keys> {
    let master = read_master(home)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no crypt key at {} — this store's filter is wired but the key is absent.\n\
             \x20      Import a teammate's key with `agit a encrypt --import <keyfile>`, or `agit a encrypt` to mint one.",
            key_path(home).display()
        )
    })?;
    Ok(derive_subkeys(&master))
}

/// Is `blob` agit-crypt output? (Does it carry the self-describing magic?)
pub fn is_ciphertext(blob: &[u8]) -> bool {
    blob.starts_with(MAGIC)
}

/// Encrypt `plaintext` to a self-describing blob. Pure function of `(keys, plaintext)` — no randomness,
/// so `seal(k, x) == seal(k, x)` byte-for-byte. This is what the clean filter emits.
pub fn seal(keys: &Keys, plaintext: &[u8]) -> Vec<u8> {
    // Convergent nonce: HMAC-SHA256(K_mac, plaintext) truncated to 24 bytes.
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&keys.mac)
        .expect("HMAC-SHA256 accepts a 32-byte key");
    mac.update(plaintext);
    let digest = mac.finalize().into_bytes();
    let nonce_bytes: [u8; NONCE_LEN] = digest[..NONCE_LEN]
        .try_into()
        .expect("HMAC-SHA256 output is 32 bytes, always >= 24");

    let cipher = XChaCha20Poly1305::new(Key::from_slice(&keys.enc));
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce_bytes), plaintext)
        .expect("XChaCha20-Poly1305 in-memory encryption is infallible for realistic sizes");

    let mut out = Vec::with_capacity(HEADER_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    out
}

/// Decrypt a self-describing agit-crypt blob. Errors (never returns garbage) on a missing/unknown magic,
/// a truncated blob, an unknown version, or a failed Poly1305 tag (wrong key OR tampering).
pub fn open(keys: &Keys, blob: &[u8]) -> Result<Vec<u8>> {
    if !is_ciphertext(blob) {
        bail!("not agit-crypt output (missing magic)");
    }
    if blob.len() < HEADER_LEN + NONCE_LEN {
        bail!("truncated agit-crypt blob ({} bytes)", blob.len());
    }
    let version = blob[MAGIC.len()];
    if version != VERSION {
        bail!("unsupported agit-crypt wire version {version} (this build reads v{VERSION})");
    }
    let nonce_bytes = &blob[HEADER_LEN..HEADER_LEN + NONCE_LEN];
    let ct = &blob[HEADER_LEN + NONCE_LEN..];
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&keys.enc));
    cipher
        .decrypt(XNonce::from_slice(nonce_bytes), ct)
        .map_err(|_| {
            anyhow::anyhow!(
                "agit-crypt: authentication failed — wrong key, or the ciphertext was tampered with"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        derive_subkeys(&[7u8; 32])
    }

    // 1. Convergence: the clean filter is a pure function — same plaintext, identical bytes.
    #[test]
    fn seal_is_deterministic() {
        let k = keys();
        let x = b"{\"role\":\"user\",\"content\":\"hello\"}\n";
        assert_eq!(seal(&k, x), seal(&k, x));
    }

    // 2. Round-trip across empty, small, and >1 MiB inputs.
    #[test]
    fn round_trip_various_sizes() {
        let k = keys();
        for x in [
            Vec::new(),
            b"small jsonl line\n".to_vec(),
            vec![0xABu8; 1024 * 1024 + 7],
        ] {
            assert_eq!(open(&k, &seal(&k, &x)).unwrap(), x);
        }
    }

    // 3. The git idempotence identity: clean(smudge(c)) == c, so git never sees "modified".
    #[test]
    fn clean_of_smudge_is_identity() {
        let k = keys();
        let x = b"transcript bytes \xff\x00\x01 with binary\n".to_vec();
        let c = seal(&k, &x); // committed blob
        let smudged = open(&k, &c).unwrap(); // checkout
        let re_cleaned = seal(&k, &smudged); // git re-cleans to compare
        assert_eq!(re_cleaned, c, "clean(smudge(c)) must equal c byte-for-byte");
    }

    // 4. Smudge passthrough: input without the magic is not agit-crypt output.
    #[test]
    fn passthrough_without_magic() {
        let plain = b"plaintext that predates encryption\n";
        assert!(!is_ciphertext(plain));
    }

    // 5. Tamper-evidence: a flipped byte in the nonce, ciphertext, or tag fails open().
    #[test]
    fn tamper_is_detected() {
        let k = keys();
        let c = seal(&k, b"authenticated payload");
        // nonce sits right after the header; the tag is the last 16 bytes; ciphertext in between.
        for pos in [HEADER_LEN, HEADER_LEN + NONCE_LEN, c.len() - 1] {
            let mut bad = c.clone();
            bad[pos] ^= 0x01;
            assert!(open(&k, &bad).is_err(), "flip at {pos} must fail auth");
        }
    }

    // 6. Equality leak (the documented, accepted tradeoff) — asserted so it can't regress silently.
    #[test]
    fn equal_plaintext_equal_ciphertext_distinct_otherwise() {
        let k = keys();
        assert_eq!(seal(&k, b"same"), seal(&k, b"same"));
        assert_ne!(seal(&k, b"same"), seal(&k, b"different"));
    }

    // 7. Wrong key: decrypting under a different master fails (auth error, not garbage plaintext).
    #[test]
    fn wrong_key_fails() {
        let c = seal(&derive_subkeys(&[1u8; 32]), b"secret");
        assert!(open(&derive_subkeys(&[2u8; 32]), &c).is_err());
    }

    // 8. Subkey separation: K_enc != K_mac and neither equals the master.
    #[test]
    fn subkeys_are_separated() {
        let master = [9u8; 32];
        let k = derive_subkeys(&master);
        assert_ne!(k.enc, k.mac);
        assert_ne!(k.enc, master);
        assert_ne!(k.mac, master);
    }

    // load/mint: a minted key is 32 bytes, 0600, and reused verbatim on the next load.
    #[test]
    fn master_is_minted_once_and_reused() {
        let home = tempfile::tempdir().unwrap();
        let k1 = load_or_create_master(home.path()).unwrap();
        let k2 = load_or_create_master(home.path()).unwrap();
        assert_eq!(k1, k2, "a minted key must be reused, never rotated");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(key_path(home.path())).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the key file must be 0600");
        }
    }

    // The filter path never mints: a missing key is a loud error, not a fresh (useless) key.
    #[test]
    fn filter_refuses_when_key_absent() {
        let home = tempfile::tempdir().unwrap();
        assert!(keys_for_filter(home.path()).is_err());
        assert!(read_master(home.path()).unwrap().is_none(), "and it did not mint one");
    }
}
