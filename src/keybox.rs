//! The per-session keybox: content-key envelopes committed alongside an encrypted store (Wave 2 of the
//! encryption-recipients design, docs/plans/2026-07-20-encryption-recipients-design.md).
//!
//! `crypt.rs` seals transcript content under a per-session **content key** (CK) — a repo-local keyring
//! generation. This module answers the orthogonal question of *who can obtain that CK*: it wraps CK to
//! each reader and records the wrap in `.agit/keybox.jsonl`, one JSON object per line, committed at the
//! store root. The keybox is EXCLUDED from the crypt filter (`.gitattributes: /.agit/keybox.jsonl
//! -filter`) — it already holds wrap-ciphertext, and filtering it would double-encrypt and deadlock the
//! bootstrap (you cannot read the wrap without the CK the wrap protects).
//!
//! Two stanza kinds ship this wave, both self-contained (no hub required to open):
//!   * individual: CK X25519-sealed to a reader's public key (fresh ephemeral, ECDH, HKDF-SHA256,
//!     XChaCha20-Poly1305 with a RANDOM nonce — these are one-shot wraps, not the convergent content
//!     path, so a random nonce is correct and required);
//!   * public: CK in the clear (hex) — anyone with the repo can read.
//!
//! `crypt unlock` reads the keybox, opens the stanzas my identity can open (public, or user stanzas
//! wrapped to my derived X25519 key), and writes the recovered CKs into the repo-local keyring so
//! `crypt.rs` seals/opens normally. Opening NONE is fail-closed: no keyring is written, so the smudge
//! filter stays locked and refuses ciphertext rather than leaking it.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use curve25519_dalek::montgomery::MontgomeryPoint;
use hkdf::Hkdf;
use sha2::Sha256;

use crate::crypt::{Keyring, KeyringEntry};

/// Domain-separated HKDF label: the ECDH shared secret is expanded under this to key the wrap AEAD, so
/// keybox wraps never share key bytes with any other agit KDF use.
const WRAP_INFO: &[u8] = b"agit-keybox/v1/wrap";
/// Keybox wire version carried in every stanza's `v` field.
const KEYBOX_V: u32 = 1;

// ---------------------------------------------------------------------------------------------
// Stanzas
// ---------------------------------------------------------------------------------------------

/// One individual reader's envelope of a content key: the ephemeral X25519 public, the AEAD nonce, and
/// the wrap (CK sealed under `HKDF(ECDH(ephemeral, reader_x25519))`). `to`/`epoch` are informational
/// recipient metadata; opening is decided cryptographically by the wrap, not by the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserStanza {
    pub kid: u32,
    pub to: String,
    pub epoch: i64,
    pub epk: [u8; 32],
    pub nonce: [u8; 24],
    pub wrap: Vec<u8>,
}

/// A public content-key stanza: the CK in the clear, so the repo alone yields the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicStanza {
    pub kid: u32,
    pub key: [u8; 32],
}

/// One keybox line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stanza {
    User(UserStanza),
    Public(PublicStanza),
}

impl Stanza {
    /// The content-key generation this stanza wraps.
    pub fn kid(&self) -> u32 {
        match self {
            Stanza::User(u) => u.kid,
            Stanza::Public(p) => p.kid,
        }
    }

    /// The JSON object for this stanza (one keybox line).
    fn to_json(&self) -> serde_json::Value {
        match self {
            Stanza::User(u) => serde_json::json!({
                "v": KEYBOX_V,
                "kid": u.kid,
                "t": "user",
                "to": u.to,
                "epoch": u.epoch,
                "epk": b64_encode(&u.epk),
                "nonce": b64_encode(&u.nonce),
                "wrap": b64_encode(&u.wrap),
            }),
            Stanza::Public(p) => serde_json::json!({
                "v": KEYBOX_V,
                "kid": p.kid,
                "t": "public",
                "key": hex::encode(p.key),
            }),
        }
    }

    /// A single compact JSON line (no embedded newline).
    pub fn to_line(&self) -> String {
        self.to_json().to_string()
    }

    /// Parse one keybox line. A malformed stanza is a loud error, never a silent skip: a keybox we
    /// cannot fully parse must not be treated as "no readers".
    pub fn from_line(line: &str) -> Result<Stanza> {
        let v: serde_json::Value =
            serde_json::from_str(line).with_context(|| format!("keybox: line is not valid JSON: {line}"))?;
        let kid = v
            .get("kid")
            .and_then(|k| k.as_u64())
            .context("keybox stanza has no numeric `kid`")? as u32;
        let t = v.get("t").and_then(|t| t.as_str()).context("keybox stanza has no `t` type")?;
        match t {
            "user" => {
                let to = v.get("to").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                let epoch = v.get("epoch").and_then(|x| x.as_i64()).unwrap_or(0);
                let epk = b64_decode_array::<32>(field_str(&v, "epk")?, "epk")?;
                let nonce = b64_decode_array::<24>(field_str(&v, "nonce")?, "nonce")?;
                let wrap = b64_decode(field_str(&v, "wrap")?).context("keybox: `wrap` is not valid base64")?;
                Ok(Stanza::User(UserStanza { kid, to, epoch, epk, nonce, wrap }))
            }
            "public" => {
                let key_hex = field_str(&v, "key")?;
                let raw = hex::decode(key_hex.trim()).context("keybox: public `key` is not valid hex")?;
                let key: [u8; 32] = raw
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("keybox: public `key` is not 32 bytes"))?;
                Ok(Stanza::Public(PublicStanza { kid, key }))
            }
            other => bail!("keybox: unknown stanza type `{other}` (this build understands `user` and `public`)"),
        }
    }
}

fn field_str<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .with_context(|| format!("keybox stanza is missing the `{key}` field"))
}

// ---------------------------------------------------------------------------------------------
// The wrap primitive: X25519 ECDH -> HKDF -> XChaCha20-Poly1305 (random nonce)
// ---------------------------------------------------------------------------------------------

/// The output of `wrap_ck_for_user`: the pieces a `user` stanza records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapped {
    pub epk: [u8; 32],
    pub nonce: [u8; 24],
    pub wrap: Vec<u8>,
}

/// X25519 ECDH: `scalar · point`, both operands clamped (idempotent for an already-clamped scalar). The
/// shared secret both wrap (`esk · reader_pub`) and unwrap (`reader_secret · epk`) compute.
fn ecdh(scalar: &[u8; 32], point: &[u8; 32]) -> [u8; 32] {
    MontgomeryPoint(*point).mul_clamped(*scalar).to_bytes()
}

/// Derive the 32-byte AEAD key for a wrap from an ECDH shared secret via HKDF-SHA256, binding both the
/// ephemeral public (`epk`) and the recipient's static public (`recipient_pub`) into the HKDF `info`.
/// The shared secret already incorporates both keys (standard ECIES), so this binding is defensive: it
/// domain-separates each wrap by its exact (ephemeral, recipient) pair and defeats any cross-context key
/// reuse. Wrap and unwrap compute the identical info (on unwrap `recipient_pub` is my own X25519 public).
fn wrap_key(shared: &[u8; 32], epk: &[u8; 32], recipient_pub: &[u8; 32]) -> [u8; 32] {
    let mut info = Vec::with_capacity(WRAP_INFO.len() + 64);
    info.extend_from_slice(WRAP_INFO);
    info.extend_from_slice(epk);
    info.extend_from_slice(recipient_pub);
    let hk = Hkdf::<Sha256>::new(None, shared);
    let mut key = [0u8; 32];
    hk.expand(&info, &mut key).expect("32 is a valid HKDF-SHA256 output length");
    key
}

/// 24 bytes of OS randomness for a one-shot wrap nonce. Random (NOT convergent): each wrap uses a fresh
/// ephemeral key, so there is no idempotence requirement and a random nonce is the correct choice.
fn random_nonce() -> Result<[u8; 24]> {
    let mut n = [0u8; 24];
    use rand::RngCore;
    rand::rngs::OsRng
        .try_fill_bytes(&mut n)
        .map_err(|e| anyhow::anyhow!("could not gather OS randomness for a keybox nonce: {e}"))?;
    Ok(n)
}

/// Wrap a content key to an individual reader: fresh ephemeral X25519 keypair, ECDH against the reader's
/// public, HKDF-SHA256 to a wrap key, then XChaCha20-Poly1305 seal of the 32-byte CK under a random
/// nonce. Only the reader (holding the matching X25519 secret) can recompute the ECDH secret and open it.
pub fn wrap_ck_for_user(ck: &[u8; 32], recipient_x25519_pub: &[u8; 32]) -> Result<Wrapped> {
    let esk = crate::crypt::random_master().context("minting a keybox ephemeral key")?;
    let epk = crate::agent::x25519_public_from_secret(&esk);
    let shared = ecdh(&esk, recipient_x25519_pub);
    let key = wrap_key(&shared, &epk, recipient_x25519_pub);
    let nonce = random_nonce()?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
    let wrap = cipher
        .encrypt(XNonce::from_slice(&nonce), ck.as_ref())
        .map_err(|_| anyhow::anyhow!("keybox: wrapping the content key failed"))?;
    Ok(Wrapped { epk, nonce, wrap })
}

/// Recover the content key from an individual stanza using my X25519 secret. ECDH against the stanza's
/// ephemeral public reproduces the wrap key; a failed AEAD tag (this stanza was wrapped to someone else,
/// or tampered) is a plain error, so the caller can try the next stanza.
pub fn unwrap_ck(stanza: &UserStanza, my_x25519_secret: &[u8; 32]) -> Result<[u8; 32]> {
    let shared = ecdh(my_x25519_secret, &stanza.epk);
    let my_pub = crate::agent::x25519_public_from_secret(my_x25519_secret);
    let key = wrap_key(&shared, &stanza.epk, &my_pub);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
    let pt = cipher
        .decrypt(XNonce::from_slice(&stanza.nonce), stanza.wrap.as_ref())
        .map_err(|_| anyhow::anyhow!("keybox: this stanza does not open with my identity"))?;
    pt.as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("keybox: unwrapped content key is not 32 bytes"))
}

/// Build a fully-formed individual stanza for a reader.
pub fn user_stanza(ck: &[u8; 32], kid: u32, to: &str, epoch: i64, recipient_x25519_pub: &[u8; 32]) -> Result<Stanza> {
    let w = wrap_ck_for_user(ck, recipient_x25519_pub)?;
    Ok(Stanza::User(UserStanza {
        kid,
        to: to.to_string(),
        epoch,
        epk: w.epk,
        nonce: w.nonce,
        wrap: w.wrap,
    }))
}

/// Build a public stanza: the CK recorded in the clear.
pub fn public_stanza(ck: &[u8; 32], kid: u32) -> Stanza {
    Stanza::Public(PublicStanza { kid, key: *ck })
}

// ---------------------------------------------------------------------------------------------
// Keybox file: .agit/keybox.jsonl
// ---------------------------------------------------------------------------------------------

/// The keybox path relative to the store root — also the exact `.gitattributes` exclusion pattern
/// (`/.agit/keybox.jsonl -filter`).
pub const KEYBOX_REL: &str = ".agit/keybox.jsonl";

/// The committed keybox artifact at the store root.
pub fn keybox_path(store: &Path) -> PathBuf {
    store.join(".agit").join("keybox.jsonl")
}

/// Read every stanza from the store's keybox. An ABSENT keybox is `Ok(vec![])` (this store is not
/// keybox-encrypted); a present-but-malformed line is a loud error.
pub fn read_keybox(store: &Path) -> Result<Vec<Stanza>> {
    let path = keybox_path(store);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push(Stanza::from_line(line)?);
    }
    Ok(out)
}

/// Overwrite the store's keybox with `stanzas`, one JSON object per line (creates `.agit/`).
pub fn write_keybox(store: &Path, stanzas: &[Stanza]) -> Result<()> {
    let path = keybox_path(store);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("cannot create {}", d.display()))?;
    }
    let mut s = String::new();
    for st in stanzas {
        s.push_str(&st.to_line());
        s.push('\n');
    }
    std::fs::write(&path, s).with_context(|| format!("cannot write {}", path.display()))
}

/// Append one stanza to the store's keybox WITHOUT rewriting existing lines — the O(1) `readers add`
/// path, which must never touch (re-clean) encrypted content blobs.
pub fn append_stanza(store: &Path, stanza: &Stanza) -> Result<()> {
    let mut stanzas = read_keybox(store)?;
    stanzas.push(stanza.clone());
    write_keybox(store, &stanzas)
}

/// The distinct reader names carried by `user` stanzas at `kid`.
pub fn readers_at(stanzas: &[Stanza], kid: u32) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for s in stanzas {
        if let Stanza::User(u) = s {
            if u.kid == kid && seen.insert(u.to.clone()) {
                out.push(u.to.clone());
            }
        }
    }
    out
}

/// Whether a public stanza exists at `kid`.
pub fn is_public_at(stanzas: &[Stanza], kid: u32) -> bool {
    stanzas.iter().any(|s| matches!(s, Stanza::Public(p) if p.kid == kid))
}

// ---------------------------------------------------------------------------------------------
// Unlock: recover the repo-local keyring from a keybox + my identity
// ---------------------------------------------------------------------------------------------

/// Recover the per-session keyring from a keybox using my X25519 secret: the content key for every kid I
/// can open (public stanzas, or user stanzas wrapped to me), with `current` = the highest recovered kid
/// (the newest generation I may seal under). `None` when I can open NONE — fail-closed: the caller MUST
/// NOT write a keyring, so the smudge filter stays locked and refuses ciphertext.
pub fn recover_keyring(stanzas: &[Stanza], my_x25519_secret: &[u8; 32]) -> Option<Keyring> {
    let mut cks: BTreeMap<u32, [u8; 32]> = BTreeMap::new();
    for s in stanzas {
        match s {
            Stanza::Public(p) => {
                cks.entry(p.kid).or_insert(p.key);
            }
            Stanza::User(u) => {
                if let Ok(ck) = unwrap_ck(u, my_x25519_secret) {
                    cks.entry(u.kid).or_insert(ck);
                }
            }
        }
    }
    if cks.is_empty() {
        return None;
    }
    let current = *cks.keys().max().expect("non-empty map has a max key");
    let keys = cks.into_iter().map(|(id, master)| KeyringEntry { id, master }).collect();
    Some(Keyring { current, keys })
}

// ---------------------------------------------------------------------------------------------
// TOFU pin store + recipient resolution
// ---------------------------------------------------------------------------------------------

/// `$AGIT_HOME/identity/pins/` — one file per pinned user, each holding that user's hex X25519 pubkey.
fn pins_dir(home: &Path) -> PathBuf {
    home.join("identity").join("pins")
}

/// A safe on-disk file name for a pinned user. Usernames are validated elsewhere, but the pin path is a
/// filesystem write, so a name that could escape the pins dir (`/`, `..`, empty) is refused here too.
fn pin_path(home: &Path, user: &str) -> Result<PathBuf> {
    let u = user.trim();
    let ok = !u.is_empty()
        && u != "."
        && u != ".."
        && !u.contains('/')
        && !u.contains('\\')
        && u.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if !ok {
        bail!("`{user}` is not a usable username for a TOFU pin");
    }
    Ok(pins_dir(home).join(u))
}

/// The pinned X25519 pubkey for `user`, or `None` if this machine has never pinned them.
pub fn read_pin(home: &Path, user: &str) -> Result<Option<[u8; 32]>> {
    let path = pin_path(home, user)?;
    match std::fs::read_to_string(&path) {
        Ok(t) => Ok(Some(decode_x25519_hex(t.trim())?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read pin {}", path.display())),
    }
}

/// Pin (or, with `repin`, re-pin) `user`'s X25519 pubkey.
pub fn write_pin(home: &Path, user: &str, key: &[u8; 32]) -> Result<()> {
    let path = pin_path(home, user)?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("cannot create {}", d.display()))?;
    }
    std::fs::write(&path, format!("{}\n", hex::encode(key)))
        .with_context(|| format!("cannot write pin {}", path.display()))
}

/// Decode a 32-byte X25519 pubkey from hex, with a loud error on bad hex / wrong length.
pub fn decode_x25519_hex(hexstr: &str) -> Result<[u8; 32]> {
    let raw = hex::decode(hexstr.trim()).context("not valid hex for an X25519 public key")?;
    raw.as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("an X25519 public key must be 32 bytes"))
}

/// Best-effort fetch of a user's published X25519 pubkey from the hub registry. `None` on any failure
/// (no hub configured, unreachable, unknown user, malformed row) — offline resolution then falls back to
/// the local pin.
fn hub_x25519(user: &str) -> Option<[u8; 32]> {
    let ep = crate::hubapi::HubEndpoint::resolve().ok()?;
    let v = ep.get_identity(user).ok()??;
    let hexs = v.get("x25519_pub").and_then(|x| x.as_str())?;
    decode_x25519_hex(hexs).ok()
}

/// Resolve a recipient's X25519 pubkey for wrapping, applying TOFU. Candidate source: `key_override` (an
/// explicit hex key, for offline use), else the hub registry (best-effort). The candidate is compared to
/// any existing pin: first sighting pins it; a CHANGED key HARD-FAILS unless `repin`. With no candidate
/// and no pin -> a clear error. Returns the key to wrap under, having pinned it when appropriate.
pub fn resolve_recipient(home: &Path, user: &str, key_override: Option<&str>, repin: bool) -> Result<[u8; 32]> {
    let candidate: Option<[u8; 32]> = match key_override {
        Some(h) => Some(decode_x25519_hex(h)?),
        None => hub_x25519(user),
    };
    let pinned = read_pin(home, user)?;
    match (candidate, pinned) {
        (Some(cand), Some(pin)) if cand == pin => Ok(cand),
        (Some(cand), Some(_)) if repin => {
            write_pin(home, user, &cand)?;
            Ok(cand)
        }
        (Some(_), Some(pin)) => bail!(
            "TOFU: the key just fetched for `{user}` DIFFERS from the pinned key\n\
             \x20      pinned  {}\n\
             \x20      This blocks a hub key-substitution. If the change is real, verify the fingerprint\n\
             \x20      out of band, then re-pin: agit identity pin {user} --repin",
            hex::encode(pin)
        ),
        (Some(cand), None) => {
            // First sighting: pin it (TOFU).
            write_pin(home, user, &cand)?;
            Ok(cand)
        }
        (None, Some(pin)) => Ok(pin), // offline: trust the existing pin
        (None, None) => bail!(
            "no key for `{user}`: no hub reachable and nothing pinned locally.\n\
             \x20      Pin it out of band: agit identity pin {user} --key <hex-x25519-pub>"
        ),
    }
}

/// `agit identity pin <user>` core: obtain the candidate (override or hub), apply TOFU, write the pin.
/// Unlike `resolve_recipient`, pinning REQUIRES a candidate — there is nothing to pin from a pin alone.
pub fn pin_user(home: &Path, user: &str, key_override: Option<&str>, repin: bool) -> Result<[u8; 32]> {
    let candidate = match key_override {
        Some(h) => decode_x25519_hex(h)?,
        None => hub_x25519(user).with_context(|| {
            format!("no hub key found for `{user}` — pass --key <hex-x25519-pub> to pin offline")
        })?,
    };
    match read_pin(home, user)? {
        Some(pin) if pin == candidate => Ok(candidate),
        Some(_) if !repin => bail!(
            "a DIFFERENT key is already pinned for `{user}`.\n\
             \x20      Verify the new fingerprint out of band, then: agit identity pin {user} --repin"
        ),
        _ => {
            write_pin(home, user, &candidate)?;
            Ok(candidate)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// base64 (std alphabet, padded) — hermetic, no crate
// ---------------------------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 encode (RFC 4648, padded).
pub fn b64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { B64[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn b64_val(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a' + 26) as u32),
        b'0'..=b'9' => Some((c - b'0' + 52) as u32),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Standard base64 decode (RFC 4648, padded). Whitespace is ignored; any other non-alphabet byte is an
/// error.
pub fn b64_decode(s: &str) -> Result<Vec<u8>> {
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    for chunk in clean.chunks(4) {
        if chunk.len() != 4 {
            bail!("base64: input length is not a multiple of 4");
        }
        let mut acc = 0u32;
        let mut pad = 0;
        for (i, &c) in chunk.iter().enumerate() {
            if c == b'=' {
                pad += 1;
                acc <<= 6;
                // padding only valid in the last one or two positions
                if i < 2 {
                    bail!("base64: misplaced padding");
                }
            } else {
                let v = b64_val(c).ok_or_else(|| anyhow::anyhow!("base64: invalid character"))?;
                if pad > 0 {
                    bail!("base64: data after padding");
                }
                acc = (acc << 6) | v;
            }
        }
        out.push((acc >> 16) as u8);
        if pad < 2 {
            out.push((acc >> 8) as u8);
        }
        if pad < 1 {
            out.push(acc as u8);
        }
    }
    Ok(out)
}

/// base64-decode into a fixed-size array, erroring if the length is wrong.
fn b64_decode_array<const N: usize>(s: &str, what: &str) -> Result<[u8; N]> {
    let raw = b64_decode(s).with_context(|| format!("keybox: `{what}` is not valid base64"))?;
    raw.as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("keybox: `{what}` must be {N} bytes, got {}", raw.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn identity_secret(seed: u8) -> ([u8; 32], [u8; 32]) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let secret = crate::agent::derive_x25519_secret(&sk);
        let public = crate::agent::x25519_public_from_secret(&secret);
        (secret, public)
    }

    // (a) An individual wrap round-trips for the intended recipient and NOT for a different key.
    #[test]
    fn wrap_unwrap_round_trips_only_for_the_recipient() {
        let ck = [0x42u8; 32];
        let (bob_secret, bob_pub) = identity_secret(1);
        let (carol_secret, _carol_pub) = identity_secret(2);

        let w = wrap_ck_for_user(&ck, &bob_pub).unwrap();
        let stanza = UserStanza { kid: 0, to: "bob".into(), epoch: 0, epk: w.epk, nonce: w.nonce, wrap: w.wrap };

        assert_eq!(unwrap_ck(&stanza, &bob_secret).unwrap(), ck, "bob opens his own wrap");
        assert!(unwrap_ck(&stanza, &carol_secret).is_err(), "a different key must NOT open it");
    }

    // A fresh ephemeral per wrap: two wraps of the same CK to the same reader differ, yet both open.
    #[test]
    fn each_wrap_uses_a_fresh_ephemeral() {
        let ck = [7u8; 32];
        let (secret, public) = identity_secret(3);
        let a = wrap_ck_for_user(&ck, &public).unwrap();
        let b = wrap_ck_for_user(&ck, &public).unwrap();
        assert_ne!(a.epk, b.epk, "ephemeral publics must differ");
        assert_ne!(a.wrap, b.wrap, "ciphertext must differ under a fresh ephemeral/nonce");
        for w in [a, b] {
            let s = UserStanza { kid: 0, to: "x".into(), epoch: 0, epk: w.epk, nonce: w.nonce, wrap: w.wrap };
            assert_eq!(unwrap_ck(&s, &secret).unwrap(), ck);
        }
    }

    // Stanza JSON round-trips through the wire form (both kinds).
    #[test]
    fn stanza_json_round_trips() {
        let ck = [9u8; 32];
        let (_secret, public) = identity_secret(4);
        let user = user_stanza(&ck, 3, "bob", 1, &public).unwrap();
        let public_st = public_stanza(&ck, 3);
        for s in [user, public_st] {
            let line = s.to_line();
            assert!(!line.contains('\n'), "a stanza must be one line");
            assert_eq!(Stanza::from_line(&line).unwrap(), s, "stanza must round-trip through JSON");
        }
    }

    // (c) A public stanza yields the CK from the repo alone (no key needed).
    #[test]
    fn public_stanza_recovers_ck_with_no_key() {
        let ck = [0xABu8; 32];
        let stanzas = vec![public_stanza(&ck, 0)];
        // recover_keyring opens public stanzas regardless of the identity secret passed.
        let ring = recover_keyring(&stanzas, &[0u8; 32]).expect("public CK is recoverable");
        assert_eq!(ring.current_master(), ck, "the public CK is recovered from the repo alone");
    }

    // (b)/fail-closed at the logic layer: bob recovers a keyring, a non-reader recovers NOTHING.
    #[test]
    fn only_a_reader_recovers_the_keyring() {
        let ck = [0x11u8; 32];
        let (bob_secret, bob_pub) = identity_secret(5);
        let (mallory_secret, _) = identity_secret(6);
        let stanzas = vec![user_stanza(&ck, 0, "bob", 0, &bob_pub).unwrap()];

        let bob_ring = recover_keyring(&stanzas, &bob_secret).expect("bob is a reader");
        assert_eq!(bob_ring.current_master(), ck);
        assert!(recover_keyring(&stanzas, &mallory_secret).is_none(), "a non-reader recovers nothing (fail-closed)");
    }

    // recover_keyring picks the HIGHEST kid as current (newest CK generation).
    #[test]
    fn recover_picks_highest_kid_as_current() {
        let ck0 = [1u8; 32];
        let ck1 = [2u8; 32];
        let (secret, public) = identity_secret(7);
        let stanzas = vec![
            user_stanza(&ck0, 0, "me", 0, &public).unwrap(),
            user_stanza(&ck1, 1, "me", 0, &public).unwrap(),
        ];
        let ring = recover_keyring(&stanzas, &secret).unwrap();
        assert_eq!(ring.current, 1, "current is the newest generation");
        assert_eq!(ring.current_master(), ck1);
        // both generations retained for decrypt.
        assert_eq!(ring.keys.len(), 2);
    }

    // (h) TOFU: a changed recipient key hard-fails, and does not silently overwrite the pin.
    #[test]
    fn tofu_changed_key_hard_fails() {
        let home = tempfile::tempdir().unwrap();
        let (_s1, k1) = identity_secret(8);
        let (_s2, k2) = identity_secret(9);

        // First sighting via an explicit key pins it.
        let got = resolve_recipient(home.path(), "bob", Some(&hex::encode(k1)), false).unwrap();
        assert_eq!(got, k1, "first sighting returns and pins the key");
        assert_eq!(read_pin(home.path(), "bob").unwrap(), Some(k1));

        // A CHANGED key hard-fails without --repin, and the pin is unchanged.
        let err = resolve_recipient(home.path(), "bob", Some(&hex::encode(k2)), false).unwrap_err();
        assert!(err.to_string().contains("TOFU"), "must be a TOFU failure: {err}");
        assert_eq!(read_pin(home.path(), "bob").unwrap(), Some(k1), "the pin must NOT change on a hard-fail");

        // --repin accepts the new key.
        let got2 = resolve_recipient(home.path(), "bob", Some(&hex::encode(k2)), true).unwrap();
        assert_eq!(got2, k2);
        assert_eq!(read_pin(home.path(), "bob").unwrap(), Some(k2), "repin updates the pin");
    }

    #[test]
    fn base64_round_trips_and_matches_vectors() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        for v in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", &[0u8, 255, 1, 254, 128][..]] {
            assert_eq!(b64_decode(&b64_encode(v)).unwrap(), v, "base64 must round-trip");
        }
        assert!(b64_decode("Zg=").is_err(), "bad length errors");
        assert!(b64_decode("****").is_err(), "invalid chars error");
    }
}
