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
//! Keyed (current) form, v2: `MAGIC (b"AGITCRYPT\0", 10 bytes) ‖ version u8=2 ‖ key-id u32 LE (4 bytes)
//! ‖ 24-byte nonce ‖ ciphertext‖tag`.
//! Legacy form, v1: `MAGIC ‖ version u8=1 ‖ 24-byte nonce ‖ ciphertext‖tag` — no key-id; still read, and
//! decrypted under key-id 0 (the sole key of a pre-keyring store).
//!
//! The key-id lets smudge/decrypt select the RIGHT key from the on-disk keyring: after `--rotate` the
//! current key seals new blobs, but a blob a retired key sealed still names that key's id and decrypts.
//! `clean` always seals under the CURRENT key. The convergent-nonce property is preserved per key
//! (`nonce = HMAC(K_mac_of_that_key, plaintext)`), so idempotence holds once the working tree is
//! renormalized under the current key.
//!
//! The magic+version make smudge self-describing so it can pass through content that isn't agit-crypt
//! output (pre-encryption blobs, an un-wired clone). The trailing NUL in the magic is load-bearing: it
//! guarantees the committed blob contains a NUL, so the secret gate's `is_probably_binary` skips it
//! (ciphertext is high-entropy and would otherwise trip the entropy rule on every encrypted session and
//! block every commit/push). Any bit-flip in nonce or ciphertext fails Poly1305, so smudge exits nonzero.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
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
/// Legacy wire version: the header carries NO key-id, so it decrypts under key-id 0 (a pre-keyring
/// store's sole key). Still read forever; never written by this build.
pub const VERSION: u8 = 1;
/// Keyed wire version: the header carries the 4-byte key-id that sealed the blob, so a rotated keyring
/// can pick the right key at smudge time. This is what `seal`/clean writes today.
pub const VERSION_KEYED: u8 = 2;

const NONCE_LEN: usize = 24;
const KEY_ID_LEN: usize = 4;
/// v1 (legacy) header: magic (10) + version (1).
const HEADER_LEN_V1: usize = MAGIC.len() + 1;
/// v2 (keyed) header: magic (10) + version (1) + key-id (4).
const HEADER_LEN_V2: usize = MAGIC.len() + 1 + KEY_ID_LEN;

/// The two domain-separated subkeys derived from ONE master: `enc` keys the AEAD, `mac` keys the
/// convergent-nonce PRF. Never persisted, always recomputed from the on-disk master.
#[derive(Clone)]
pub struct Subkeys {
    /// Keys the XChaCha20-Poly1305 AEAD.
    pub enc: [u8; 32],
    /// Keys the HMAC-SHA256 convergent-nonce PRF.
    pub mac: [u8; 32],
}

/// The keys the clean/smudge filter uses: every keyring entry's derived subkeys indexed by key-id, plus
/// which id is CURRENT. `seal` (clean) always uses + stamps the current key; `open` (smudge) selects the
/// key by the blob's header key-id, so retired keys still decrypt. A single-key file (or `derive_subkeys`)
/// yields a one-entry ring at id 0.
#[derive(Clone)]
pub struct Keys {
    current_id: u32,
    by_id: BTreeMap<u32, Subkeys>,
}

impl Keys {
    /// The id `seal`/clean stamps into new blobs.
    pub fn current_id(&self) -> u32 {
        self.current_id
    }
    fn current(&self) -> &Subkeys {
        self.by_id.get(&self.current_id).expect("the current key is always present in the ring")
    }
    fn for_id(&self, id: u32) -> Option<&Subkeys> {
        self.by_id.get(&id)
    }
}

/// `$AGIT_HOME/crypt/` — where the machine-global symmetric key lives, spanning every store.
fn crypt_dir(home: &Path) -> PathBuf {
    home.join("crypt")
}

/// The symmetric key material, stored 0600. Either a legacy single hex line, or the multi-key keyring
/// format written since key rotation landed. `key_path`'s name is unchanged for back-compat.
pub fn key_path(home: &Path) -> PathBuf {
    crypt_dir(home).join("agit-crypt.key")
}

/// One key in the on-disk keyring: a short id plus its 32-byte master. Ids are never reused, so a retired
/// key stays selectable by the blobs that named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyringEntry {
    pub id: u32,
    pub master: [u8; 32],
}

/// The on-disk keyring: the CURRENT key (what `clean` seals under) plus retired keys kept for decrypt
/// only. A legacy single-key file loads as a one-entry ring with `current = 0`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyring {
    pub current: u32,
    pub keys: Vec<KeyringEntry>,
}

impl Keyring {
    /// The current key's 32-byte master.
    pub fn current_master(&self) -> [u8; 32] {
        self.keys
            .iter()
            .find(|e| e.id == self.current)
            .map(|e| e.master)
            .expect("a keyring always retains its current key")
    }
    /// The next unused key-id: max existing + 1 (ids are never reused).
    fn next_id(&self) -> u32 {
        self.keys.iter().map(|e| e.id).max().map_or(0, |m| m + 1)
    }
    /// Mint `new_master` as a fresh CURRENT key, retaining the old current (and every already-retired
    /// key) for decrypt only. Returns the new key-id.
    pub fn rotate(&mut self, new_master: [u8; 32]) -> u32 {
        let id = self.next_id();
        self.keys.push(KeyringEntry { id, master: new_master });
        self.current = id;
        id
    }
}

/// 32 bytes of OS randomness for a fresh master key.
pub fn random_master() -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    use rand::RngCore;
    rand::rngs::OsRng
        .try_fill_bytes(&mut key)
        .map_err(|e| anyhow::anyhow!("could not gather OS randomness for the crypt key: {e}"))?;
    Ok(key)
}

/// Hex-decode + validate one 32-byte master, with a loud error on bad hex / wrong length.
fn decode_master(hexstr: &str, path: &Path) -> Result<[u8; 32]> {
    let raw = hex::decode(hexstr.trim())
        .with_context(|| format!("{} is not valid hex — the crypt key is corrupt", path.display()))?;
    raw.as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("{} is not a 32-byte key", path.display()))
}

/// Parse the on-disk key material into a keyring, understanding BOTH formats:
///   * legacy single-key file — one bare hex line → id 0, current;
///   * keyring file — `current = N` plus `key <id> = <hex>` lines.
///
/// A corrupt file is a loud error, never a silent fresh mint: a wired filter with garbled key material
/// must fail rather than strand every blob the real key encrypted.
fn parse_keyring(text: &str, path: &Path) -> Result<Keyring> {
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        bail!("{} is empty — the crypt key is missing", path.display());
    }
    // A directive line (`current = …` / `key N = …`) marks the keyring format; its absence means the
    // legacy single-key file (a bare hex master, no `=`).
    let is_keyring = lines.iter().any(|l| l.contains('='));
    if !is_keyring {
        let master = decode_master(&lines[0], path)?;
        return Ok(Keyring { current: 0, keys: vec![KeyringEntry { id: 0, master }] });
    }
    let mut current: Option<u32> = None;
    let mut keys: Vec<KeyringEntry> = Vec::new();
    for l in &lines {
        let Some((k, v)) = l.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k == "current" {
            current = Some(v.parse().with_context(|| {
                format!("{}: `current` must be a key-id number, got `{v}`", path.display())
            })?);
        } else if let Some(idtok) = k.strip_prefix("key ") {
            let id: u32 = idtok.trim().parse().with_context(|| {
                format!("{}: bad key id `{}`", path.display(), idtok.trim())
            })?;
            let master = decode_master(v.trim_matches('"'), path)?;
            keys.push(KeyringEntry { id, master });
        }
    }
    let current = current
        .ok_or_else(|| anyhow::anyhow!("{}: keyring has no `current` key id", path.display()))?;
    if keys.is_empty() {
        bail!("{}: keyring declares no keys", path.display());
    }
    if !keys.iter().any(|e| e.id == current) {
        bail!("{}: `current = {current}` names no key in the ring", path.display());
    }
    Ok(Keyring { current, keys })
}

/// Read the on-disk key material as a keyring, or `None` if the file is absent.
pub fn load_keyring(home: &Path) -> Result<Option<Keyring>> {
    let path = key_path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    parse_keyring(&text, &path).map(Some)
}

/// Persist a keyring (0600), sorted by id, with the loud never-commit header.
pub fn save_keyring(home: &Path, ring: &Keyring) -> Result<()> {
    std::fs::create_dir_all(crypt_dir(home))
        .with_context(|| format!("cannot create {}", crypt_dir(home).display()))?;
    let mut s = String::from(
        "# agit crypt keyring — the CURRENT key plus retired keys (decrypt-only).\n\
         # NEVER commit or push this file. No escrow: losing it loses every encrypted blob.\n",
    );
    s.push_str(&format!("current = {}\n", ring.current));
    let mut sorted = ring.keys.clone();
    sorted.sort_by_key(|e| e.id);
    for e in &sorted {
        s.push_str(&format!("key {} = {}\n", e.id, hex::encode(e.master)));
    }
    crate::agent::write_secret_0600(&key_path(home), &s)
}

/// Read + hex-decode + validate the CURRENT 32-byte master, or `None` if the file is absent. Works over
/// both the legacy single-key file and the keyring.
///
/// A corrupt (bad hex / wrong length) key is an error, not `None`: a wired filter with a garbled key
/// must fail loudly, never mint a fresh one and silently strand every blob the old one encrypted.
pub fn read_master(home: &Path) -> Result<Option<[u8; 32]>> {
    Ok(load_keyring(home)?.map(|r| r.current_master()))
}

/// Load the master key, minting it once on first use (0600, legacy single-key form) and persisting
/// before returning.
///
/// A key already on disk is reused verbatim: rotating it silently would strand every blob it ever
/// encrypted — explicit rotation is `rotate_key`. Mirrors `agent::load_or_create_signing_key`.
pub fn load_or_create_master(home: &Path) -> Result<[u8; 32]> {
    if let Some(k) = read_master(home)? {
        return Ok(k);
    }
    std::fs::create_dir_all(crypt_dir(home))
        .with_context(|| format!("cannot create {}", crypt_dir(home).display()))?;
    let key = random_master()?;
    crate::agent::write_secret_0600(&key_path(home), &hex::encode(key))?;
    Ok(key)
}

/// Explicit key rotation: mint a fresh CURRENT key, retain every existing key for decrypt-only, persist
/// the ring, and return the new current key-id. Loads a legacy single-key file as id 0 first, so a rotate
/// on a pre-keyring store keeps the old key selectable for its old (v1) blobs.
pub fn rotate_key(home: &Path) -> Result<u32> {
    let mut ring = load_keyring(home)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no crypt key at {} to rotate — run `agit a encrypt` first",
            key_path(home).display()
        )
    })?;
    let id = ring.rotate(random_master()?);
    save_keyring(home, &ring)?;
    Ok(id)
}

/// Split a 32-byte master into the two domain-separated subkeys with HKDF-SHA256.
///
/// `K_enc` and `K_mac` come from the same extract but different `info`, so they never share key bytes.
/// The `agit-crypt/v1/*` info labels are the KDF domain (unchanged by wire versioning), so a given
/// master's derived subkeys are stable — a legacy v1 blob decrypts identically before and after this
/// keyring change.
pub fn derive_one(master: &[u8; 32]) -> Subkeys {
    let hk = Hkdf::<Sha256>::new(None, master);
    let mut enc = [0u8; 32];
    hk.expand(b"agit-crypt/v1/enc", &mut enc)
        .expect("32 is a valid HKDF-SHA256 output length");
    let mut mac = [0u8; 32];
    hk.expand(b"agit-crypt/v1/nonce", &mut mac)
        .expect("32 is a valid HKDF-SHA256 output length");
    Subkeys { enc, mac }
}

/// Derive a one-entry `Keys` (id 0, current) from a single master — the single-key path and what the
/// unit tests exercise. `seal` under this stamps key-id 0.
pub fn derive_subkeys(master: &[u8; 32]) -> Keys {
    let mut by_id = BTreeMap::new();
    by_id.insert(0u32, derive_one(master));
    Keys { current_id: 0, by_id }
}

/// Derive a full `Keys` from a keyring: every entry's subkeys, current id preserved.
pub fn derive_keyring(ring: &Keyring) -> Keys {
    let mut by_id = BTreeMap::new();
    for e in &ring.keys {
        by_id.insert(e.id, derive_one(&e.master));
    }
    Keys { current_id: ring.current, by_id }
}

/// The keys the filter uses, requiring an EXISTING keyring (never minting on the filter path).
///
/// clean and smudge run under `required=true`: if the key is missing, git must ABORT rather than stage
/// plaintext or check out ciphertext, so a missing key is a loud error here, not a fresh mint.
pub fn keys_for_filter(home: &Path) -> Result<Keys> {
    let ring = load_keyring(home)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no crypt key at {} — this store's filter is wired but the key is absent.\n\
             \x20      Import a teammate's key with `agit a encrypt --import <keyfile>`, or `agit a encrypt` to mint one.",
            key_path(home).display()
        )
    })?;
    Ok(derive_keyring(&ring))
}

/// Is `blob` agit-crypt output? (Does it carry the self-describing magic?)
pub fn is_ciphertext(blob: &[u8]) -> bool {
    blob.starts_with(MAGIC)
}

/// Encrypt `plaintext` to a self-describing v2 blob under the ring's CURRENT key, stamping its key-id.
/// Pure function of `(keys, plaintext)` — no randomness, so `seal(k, x) == seal(k, x)` byte-for-byte.
/// This is what the clean filter emits.
pub fn seal(keys: &Keys, plaintext: &[u8]) -> Vec<u8> {
    let sk = keys.current();
    let id = keys.current_id;
    // Convergent nonce: HMAC-SHA256(K_mac, plaintext) truncated to 24 bytes. Per-key: a rotated key has a
    // different K_mac, so the nonce space is separate, but idempotence still holds under one key.
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&sk.mac)
        .expect("HMAC-SHA256 accepts a 32-byte key");
    mac.update(plaintext);
    let digest = mac.finalize().into_bytes();
    let nonce_bytes: [u8; NONCE_LEN] = digest[..NONCE_LEN]
        .try_into()
        .expect("HMAC-SHA256 output is 32 bytes, always >= 24");

    let cipher = XChaCha20Poly1305::new(Key::from_slice(&sk.enc));
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce_bytes), plaintext)
        .expect("XChaCha20-Poly1305 in-memory encryption is infallible for realistic sizes");

    let mut out = Vec::with_capacity(HEADER_LEN_V2 + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION_KEYED);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    out
}

/// Decrypt a self-describing agit-crypt blob, selecting the key by the header's key-id (v2) or falling
/// back to key-id 0 (legacy v1). Errors (never returns garbage) on a missing/unknown magic, a truncated
/// blob, an unknown version, a key-id absent from the ring, or a failed Poly1305 tag (wrong key OR
/// tampering).
pub fn open(keys: &Keys, blob: &[u8]) -> Result<Vec<u8>> {
    if !is_ciphertext(blob) {
        bail!("not agit-crypt output (missing magic)");
    }
    // is_ciphertext only guarantees the 10-byte magic; the version byte is at index MAGIC.len(), so a
    // blob that is EXACTLY the magic must error cleanly here, not panic. A panic in open() would crash
    // the git smudge process on checkout of such a (corrupted/crafted) blob.
    if blob.len() < HEADER_LEN_V1 {
        bail!("truncated agit-crypt blob ({} bytes)", blob.len());
    }
    let version = blob[MAGIC.len()];
    let (sk, header_len) = match version {
        VERSION => {
            // Legacy: no key-id — it was sealed by the original single key, id 0.
            let sk = keys.for_id(0).ok_or_else(|| {
                anyhow::anyhow!(
                    "agit-crypt: a legacy (v1) blob needs key-id 0, which is not in the keyring"
                )
            })?;
            (sk, HEADER_LEN_V1)
        }
        VERSION_KEYED => {
            if blob.len() < HEADER_LEN_V2 {
                bail!("truncated agit-crypt blob ({} bytes)", blob.len());
            }
            let id = u32::from_le_bytes(
                blob[MAGIC.len() + 1..MAGIC.len() + 1 + KEY_ID_LEN]
                    .try_into()
                    .expect("KEY_ID_LEN bytes are present after the length check"),
            );
            let sk = keys.for_id(id).ok_or_else(|| {
                anyhow::anyhow!(
                    "agit-crypt: no key-id {id} in the keyring — the key that sealed this blob was \
                     removed and it cannot be decrypted"
                )
            })?;
            (sk, HEADER_LEN_V2)
        }
        v => bail!(
            "unsupported agit-crypt wire version {v} (this build reads v{VERSION} and v{VERSION_KEYED})"
        ),
    };
    if blob.len() < header_len + NONCE_LEN {
        bail!("truncated agit-crypt blob ({} bytes)", blob.len());
    }
    let nonce_bytes = &blob[header_len..header_len + NONCE_LEN];
    let ct = &blob[header_len + NONCE_LEN..];
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&sk.enc));
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

    // Regression: open() must ERROR, not panic, on a blob that is exactly the 10-byte magic. It passes
    // is_ciphertext (which only checks the magic), and the version byte lives one past it — reading it
    // without a length guard would panic and crash the git smudge process on checkout.
    #[test]
    fn open_on_magic_only_blob_errors_without_panicking() {
        let k = keys();
        assert!(is_ciphertext(MAGIC), "the bare magic looks like ciphertext to the filter");
        let err = open(&k, MAGIC).unwrap_err();
        assert!(err.to_string().contains("truncated"), "clean error, not a panic: {err}");
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
        // nonce sits right after the (v2) header; the tag is the last 16 bytes; ciphertext in between.
        for pos in [HEADER_LEN_V2, HEADER_LEN_V2 + NONCE_LEN, c.len() - 1] {
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
        let sk = derive_one(&master);
        assert_ne!(sk.enc, sk.mac);
        assert_ne!(sk.enc, master);
        assert_ne!(sk.mac, master);
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

    // A multi-key keyring round-trips through save/load, and `current_master` selects the current entry.
    #[test]
    fn keyring_round_trips() {
        let home = tempfile::tempdir().unwrap();
        let ring = Keyring {
            current: 1,
            keys: vec![
                KeyringEntry { id: 0, master: [0x11; 32] },
                KeyringEntry { id: 1, master: [0x22; 32] },
            ],
        };
        save_keyring(home.path(), &ring).unwrap();
        let back = load_keyring(home.path()).unwrap().unwrap();
        assert_eq!(back, ring, "the keyring must round-trip byte-for-byte through the file");
        assert_eq!(back.current_master(), [0x22; 32], "current selects id 1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(key_path(home.path())).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the keyring file must be 0600");
        }
    }

    // Back-compat: an existing single-key file (one bare hex line, no key-id) loads as the current key,
    // id 0.
    #[test]
    fn legacy_single_key_file_loads_as_current_id0() {
        let home = tempfile::tempdir().unwrap();
        let master = [0x5a; 32];
        std::fs::create_dir_all(crypt_dir(home.path())).unwrap();
        crate::agent::write_secret_0600(&key_path(home.path()), &hex::encode(master)).unwrap();

        let ring = load_keyring(home.path()).unwrap().unwrap();
        assert_eq!(ring.current, 0);
        assert_eq!(ring.keys, vec![KeyringEntry { id: 0, master }]);
        assert_eq!(read_master(home.path()).unwrap(), Some(master), "read_master returns the current key");
    }

    // A pre-keyring v1 WIRE blob (no key-id in the header) decrypts under key-id 0 — the on-disk wire
    // back-compat anchor.
    #[test]
    fn legacy_v1_wire_blob_decrypts_under_id0() {
        let master = [0x33u8; 32];
        let sk = derive_one(&master);
        let plaintext = b"a blob written by a pre-keyring agit\n";
        // Hand-build the v1 wire: MAGIC ‖ VERSION(1) ‖ nonce ‖ ct — exactly what the old `seal` emitted.
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&sk.mac).unwrap();
        mac.update(plaintext);
        let nonce: [u8; NONCE_LEN] = mac.finalize().into_bytes()[..NONCE_LEN].try_into().unwrap();
        let ct = XChaCha20Poly1305::new(Key::from_slice(&sk.enc))
            .encrypt(XNonce::from_slice(&nonce), plaintext.as_ref())
            .unwrap();
        let mut v1 = Vec::new();
        v1.extend_from_slice(MAGIC);
        v1.push(VERSION);
        v1.extend_from_slice(&nonce);
        v1.extend_from_slice(&ct);

        // A single-key ring (id 0) opens the legacy v1 blob unchanged.
        assert_eq!(open(&derive_subkeys(&master), &v1).unwrap(), plaintext);
    }

    // KEY ROTATION: a blob sealed under key-id 0 still decrypts after `rotate_key` mints id 1, NEW writes
    // are stamped with the current id 1, and dropping the retired key is what makes the old blob unreadable.
    #[test]
    fn rotate_retains_old_key_and_advances_current() {
        let home = tempfile::tempdir().unwrap();
        let _ = load_or_create_master(home.path()).unwrap(); // enable: single key, id 0
        let keys_v0 = keys_for_filter(home.path()).unwrap();
        assert_eq!(keys_v0.current_id(), 0);

        let plaintext = b"session line under the original key\n";
        let blob_v0 = seal(&keys_v0, plaintext);
        assert_eq!(blob_v0[MAGIC.len()], VERSION_KEYED, "new writes are v2");
        let id0 = u32::from_le_bytes(
            blob_v0[MAGIC.len() + 1..MAGIC.len() + 1 + KEY_ID_LEN].try_into().unwrap(),
        );
        assert_eq!(id0, 0, "sealed under key-id 0 before rotation");

        // Rotate: mint id 1 as current, retain id 0.
        let new_id = rotate_key(home.path()).unwrap();
        assert_eq!(new_id, 1);
        let keys_v1 = keys_for_filter(home.path()).unwrap();
        assert_eq!(keys_v1.current_id(), 1, "the current key advanced to id 1");

        // The old id-0 blob STILL decrypts under the rotated ring (retired key retained).
        assert_eq!(open(&keys_v1, &blob_v0).unwrap(), plaintext, "an old blob must survive rotation");

        // A NEW write is sealed under id 1, and differs from the id-0 ciphertext.
        let blob_v1 = seal(&keys_v1, plaintext);
        let id1 = u32::from_le_bytes(
            blob_v1[MAGIC.len() + 1..MAGIC.len() + 1 + KEY_ID_LEN].try_into().unwrap(),
        );
        assert_eq!(id1, 1, "new writes use the new current key");
        assert_ne!(blob_v0, blob_v1, "the two keys produce distinct ciphertext");
        assert_eq!(open(&keys_v1, &blob_v1).unwrap(), plaintext);

        // Proof that retention is load-bearing: a ring holding only id 1 cannot read the id-0 blob, and
        // the failure is a loud error, never garbage.
        let ring = load_keyring(home.path()).unwrap().unwrap();
        let cur = ring.keys.iter().find(|e| e.id == 1).unwrap().master;
        let only_v1 = derive_keyring(&Keyring { current: 1, keys: vec![KeyringEntry { id: 1, master: cur }] });
        assert!(open(&only_v1, &blob_v0).is_err(), "without key-id 0 the old blob cannot be read");
    }
}
