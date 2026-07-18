//! Content-addressed blob storage behind a small trait, with two backends: a local filesystem store
//! (the zero-config self-host + test default) and an S3-compatible store (Garage, or any S3 endpoint).
//!
//! **Content-addressed AND per-agent namespaced.** `put` computes `sha256(bytes)` server-side (the
//! client is never trusted) and returns the hex digest — that digest *is* the address. But the storage
//! key is prefixed by the agent: fs path `<root>/blobs/<agent>/<sha256>`, S3 key
//! `blobs/<agent>/<sha256>`. Reason: the agent's ACL is the access boundary. A global `blobs/<sha>`
//! namespace would let anyone who can read *any* agent fetch a blob uploaded under a *private* one just
//! by presenting its digest — a cross-agent disclosure oracle. Per-agent prefixing makes
//! `get(agent, digest)` only resolve blobs uploaded through that agent, so the existing non-disclosure
//! gate (`acl::decide`) fully covers blobs.
//!
//! The `<agent>` is always a `valid_agent_name` value (validated by `gate()` before it reaches a key),
//! and `<sha256>` is validated to `[0-9a-f]{64}` before any backend call — so the fs path is only ever
//! built from a validated hex string (no traversal / key injection).
//!
//! Dispatch mirrors [`super::store::Store`]: a concrete `enum Blobs { Fs, S3 }` held by value, not
//! `Arc<dyn>`, keeping native `async fn` (no `async-trait`) and staying dyn-free.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]

use std::io;
use std::path::{Path, PathBuf};

use s3::creds::Credentials;
use s3::error::S3Error;
use s3::region::Region;
use s3::Bucket;
use sha2::{Digest, Sha256};

/// Per-object upload ceiling. A blob is a large artifact an agent references — not a release archive,
/// not a disk image — so 100 MiB is generous while still bounding memory (a blob is buffered whole to
/// hash it).
pub const BLOB_MAX: u64 = 100 * 1024 * 1024;

/// Compute the lowercase sha256 hex of some bytes — the content address.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// A digest is well-formed iff it is exactly 64 lowercase hex chars. Anything else could never name a
/// real blob (put only ever returns this shape) and must not reach a backend key.
pub fn valid_digest(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An agent key segment is safe iff it is non-empty, has no path separators and no `..`. `gate()`
/// already guarantees `valid_agent_name`; this is defence in depth at the storage boundary.
fn valid_agent_key(a: &str) -> bool {
    !a.is_empty() && !a.contains('/') && !a.contains('\\') && !a.contains("..") && !a.contains('\0')
}

/// The shared contract. Native `async fn` in trait (the repo pulls in no `async-trait`); the trait is
/// only ever used through the concrete [`Blobs`] enum, never `dyn`, so the missing auto-`Send` bound
/// the lint warns about is a non-issue here.
#[allow(async_fn_in_trait)]
pub trait BlobStore: Send + Sync {
    /// Store `bytes` for `agent`. Returns the server-computed lowercase sha256 hex (the address).
    async fn put(&self, agent: &str, bytes: &[u8]) -> io::Result<String>;
    /// Fetch the blob at `(agent, digest)`. `Ok(None)` = absent.
    async fn get(&self, agent: &str, digest: &str) -> io::Result<Option<Vec<u8>>>;
    /// Whether `(agent, digest)` exists.
    async fn exists(&self, agent: &str, digest: &str) -> io::Result<bool>;
    /// Remove `(agent, digest)`. Absent is not an error (idempotent). Not routed in v1 — kept for
    /// future GC/lifecycle work.
    async fn delete(&self, agent: &str, digest: &str) -> io::Result<()>;
    /// Move **every** blob owned by `old` to be owned by `new` (a bulk per-agent rename). Absent
    /// source = success (the agent simply had no blobs). Blobs are keyed by the agent NAME, so an
    /// agent rename must carry them along or they are stranded under the old name and unreachable.
    async fn rename_agent(&self, old: &str, new: &str) -> io::Result<()>;
    /// Remove **every** blob owned by `agent` (a bulk per-agent purge). Absent = success (idempotent).
    /// A purge must destroy these, or a NEW agent later recycling the name would read the previous
    /// owner's PRIVATE blobs — the same recycled-name leak the tokens/MRs cleanup already closes.
    async fn delete_agent(&self, agent: &str) -> io::Result<()>;
}

/// Backend dispatch, mirroring `Store::Sqlite`/`Store::Pg`.
pub enum Blobs {
    Fs(FsBlobs),
    S3(S3Blobs),
}

impl Blobs {
    /// Env-driven, fs-default backend selection (mirrors `AGIT_HUB_DB` in `Store::open`):
    ///
    ///   - `AGIT_HUB_S3_ENDPOINT` set (non-empty) → the S3 backend, reading `AGIT_HUB_S3_BUCKET`,
    ///     `AGIT_HUB_S3_ACCESS_KEY`, `AGIT_HUB_S3_SECRET_KEY`, and `AGIT_HUB_S3_REGION` (default
    ///     "garage"). Path-style is always on (Garage requires it).
    ///   - else → the filesystem backend under `<root>/blobs` (created 0700).
    ///
    /// **Fail-closed at boot**: an S3 endpoint set but bucket/keys missing or empty returns an error —
    /// it never silently falls back to fs. A misconfigured S3 that quietly wrote to local disk would be
    /// a data-placement surprise; surface it at boot exactly as a bad `AGIT_HUB_DB` surfaces.
    pub async fn open(root: &Path) -> io::Result<Blobs> {
        match std::env::var("AGIT_HUB_S3_ENDPOINT") {
            Ok(endpoint) if !endpoint.trim().is_empty() => Ok(Blobs::S3(S3Blobs::open(endpoint.trim())?)),
            _ => {
                let dir = root.join("blobs");
                // 0700, reusing the exact mode logic ensure_root uses for <root>.
                super::store::ensure_root(&dir)?;
                Ok(Blobs::Fs(FsBlobs { dir }))
            }
        }
    }

    /// One line for the startup banner: `filesystem <dir>` or `s3 <endpoint>/<bucket>`.
    pub fn describe(&self) -> String {
        match self {
            Blobs::Fs(f) => format!("filesystem {}", f.dir.display()),
            Blobs::S3(s) => format!("s3 {}/{}", s.endpoint, s.bucket_name),
        }
    }

    pub async fn put(&self, agent: &str, bytes: &[u8]) -> io::Result<String> {
        match self {
            Blobs::Fs(b) => b.put(agent, bytes).await,
            Blobs::S3(b) => b.put(agent, bytes).await,
        }
    }
    pub async fn get(&self, agent: &str, digest: &str) -> io::Result<Option<Vec<u8>>> {
        match self {
            Blobs::Fs(b) => b.get(agent, digest).await,
            Blobs::S3(b) => b.get(agent, digest).await,
        }
    }
    pub async fn exists(&self, agent: &str, digest: &str) -> io::Result<bool> {
        match self {
            Blobs::Fs(b) => b.exists(agent, digest).await,
            Blobs::S3(b) => b.exists(agent, digest).await,
        }
    }
    pub async fn delete(&self, agent: &str, digest: &str) -> io::Result<()> {
        match self {
            Blobs::Fs(b) => b.delete(agent, digest).await,
            Blobs::S3(b) => b.delete(agent, digest).await,
        }
    }
    pub async fn rename_agent(&self, old: &str, new: &str) -> io::Result<()> {
        match self {
            Blobs::Fs(b) => b.rename_agent(old, new).await,
            Blobs::S3(b) => b.rename_agent(old, new).await,
        }
    }
    pub async fn delete_agent(&self, agent: &str) -> io::Result<()> {
        match self {
            Blobs::Fs(b) => b.delete_agent(agent).await,
            Blobs::S3(b) => b.delete_agent(agent).await,
        }
    }
}

// ─────────────────────────── filesystem backend ───────────────────────────

/// Blobs on local disk under `<dir>/<agent>/<sha256>`. The zero-config self-host + test default.
///
/// tokio here is built WITHOUT the `fs` feature, so all filesystem work runs on the blocking pool via
/// `spawn_blocking` (the exact pattern the git/content handlers already use), never on an async worker.
pub struct FsBlobs {
    pub dir: PathBuf,
}

impl FsBlobs {
    fn object_path(&self, agent: &str, digest: &str) -> PathBuf {
        self.dir.join(agent).join(digest)
    }
}

/// Create a directory 0700 (owner-only) on Unix; a no-op mode on Windows.
fn mkdir_700(p: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(p).or_else(|e| if p.is_dir() { Ok(()) } else { Err(e) })
}

impl BlobStore for FsBlobs {
    async fn put(&self, agent: &str, bytes: &[u8]) -> io::Result<String> {
        if !valid_agent_key(agent) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid agent key"));
        }
        let agent_dir = self.dir.join(agent);
        // Hashing 100 MiB is real CPU work and the write is blocking IO — both belong on the blocking
        // pool, so move the bytes there and do everything (hash → temp write → atomic rename) at once.
        let data = bytes.to_vec();
        tokio::task::spawn_blocking(move || -> io::Result<String> {
            let digest = sha256_hex(&data);
            mkdir_700(&agent_dir)?;
            let final_path = agent_dir.join(&digest);
            // Atomic write: a temp file in the same dir, then rename into place. tempfile creates the
            // temp 0600 on Unix already; content-addressed, so a concurrent identical write is harmless
            // (the rename just replaces bytes with identical bytes).
            let mut f = tempfile::NamedTempFile::new_in(&agent_dir)?;
            {
                use std::io::Write;
                f.write_all(&data)?;
                f.flush()?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            f.persist(&final_path).map_err(|e| e.error)?;
            Ok(digest)
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
    }

    async fn get(&self, agent: &str, digest: &str) -> io::Result<Option<Vec<u8>>> {
        if !valid_agent_key(agent) || !valid_digest(digest) {
            return Ok(None);
        }
        let path = self.object_path(agent, digest);
        tokio::task::spawn_blocking(move || match std::fs::read(&path) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
    }

    async fn exists(&self, agent: &str, digest: &str) -> io::Result<bool> {
        if !valid_agent_key(agent) || !valid_digest(digest) {
            return Ok(false);
        }
        let path = self.object_path(agent, digest);
        tokio::task::spawn_blocking(move || match std::fs::metadata(&path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
    }

    async fn delete(&self, agent: &str, digest: &str) -> io::Result<()> {
        if !valid_agent_key(agent) || !valid_digest(digest) {
            return Ok(());
        }
        let path = self.object_path(agent, digest);
        tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
    }

    async fn rename_agent(&self, old: &str, new: &str) -> io::Result<()> {
        // Both names are already `valid_agent_name` at the call sites; re-validate at the storage
        // boundary (defence in depth) so a key can never build a path with a separator or `..`.
        if !valid_agent_key(old) || !valid_agent_key(new) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid agent key"));
        }
        let from = self.dir.join(old);
        let to = self.dir.join(new);
        // Directory rename is blocking fs work — same blocking-pool discipline as every other op here.
        tokio::task::spawn_blocking(move || match std::fs::rename(&from, &to) {
            Ok(()) => Ok(()),
            // The agent had no blob dir: nothing to move is success, not failure.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
    }

    async fn delete_agent(&self, agent: &str) -> io::Result<()> {
        if !valid_agent_key(agent) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid agent key"));
        }
        let dir = self.dir.join(agent);
        tokio::task::spawn_blocking(move || match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            // No blob dir = nothing to purge = success (idempotent).
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
    }
}

// ─────────────────────────── S3 / Garage backend ───────────────────────────

/// Blobs in an S3-compatible bucket under key `blobs/<agent>/<sha256>`. Natively async (rust-s3's own
/// client); only instantiated when `AGIT_HUB_S3_ENDPOINT` is set, so the fs default pays only compile
/// cost.
pub struct S3Blobs {
    bucket: Box<Bucket>,
    /// Kept for the startup banner.
    endpoint: String,
    bucket_name: String,
}

impl S3Blobs {
    /// Build the client from the S3 env. Fail-closed: bucket or either key missing/empty → error.
    fn open(endpoint: &str) -> io::Result<S3Blobs> {
        fn require(key: &str) -> io::Result<String> {
            match std::env::var(key) {
                Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{key} must be set (and non-empty) when AGIT_HUB_S3_ENDPOINT is configured"),
                )),
            }
        }
        let bucket_name = require("AGIT_HUB_S3_BUCKET")?;
        let access = require("AGIT_HUB_S3_ACCESS_KEY")?;
        let secret = require("AGIT_HUB_S3_SECRET_KEY")?;
        let region_name =
            std::env::var("AGIT_HUB_S3_REGION").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "garage".to_string());

        let region = Region::Custom { region: region_name, endpoint: endpoint.to_string() };
        let creds = Credentials::new(Some(&access), Some(&secret), None, None, None)
            .map_err(|e| io::Error::other(format!("S3 credentials: {e}")))?;
        // Path-style is mandatory for Garage (and safe for MinIO/any endpoint that lacks vhost DNS).
        let bucket = Bucket::new(&bucket_name, region, creds)
            .map_err(|e| io::Error::other(format!("S3 bucket: {e}")))?
            .with_path_style();
        Ok(S3Blobs { bucket, endpoint: endpoint.to_string(), bucket_name })
    }

    fn key(agent: &str, digest: &str) -> String {
        format!("blobs/{agent}/{digest}")
    }
}

/// Whether an S3 error is a 404 (object absent), so get/head/delete can map it to None/false/Ok.
fn s3_is_404(e: &S3Error) -> bool {
    matches!(e, S3Error::HttpFailWithBody(404, _))
}

fn s3_err(e: S3Error) -> io::Error {
    io::Error::other(format!("s3: {e}"))
}

impl BlobStore for S3Blobs {
    async fn put(&self, agent: &str, bytes: &[u8]) -> io::Result<String> {
        if !valid_agent_key(agent) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid agent key"));
        }
        // Hashing up to BLOB_MAX (100 MiB) of SHA-256 is real CPU work — keep it off the async worker,
        // exactly as FsBlobs::put deliberately does. (The upload itself is already async IO via rust-s3.)
        let data = bytes.to_vec();
        let digest = tokio::task::spawn_blocking(move || sha256_hex(&data)).await.map_err(|e| io::Error::other(e.to_string()))?;
        let resp = self.bucket.put_object(Self::key(agent, &digest), bytes).await.map_err(s3_err)?;
        let code = resp.status_code();
        if !(200..300).contains(&code) {
            return Err(io::Error::other(format!("s3 put returned {code}")));
        }
        Ok(digest)
    }

    async fn get(&self, agent: &str, digest: &str) -> io::Result<Option<Vec<u8>>> {
        if !valid_agent_key(agent) || !valid_digest(digest) {
            return Ok(None);
        }
        match self.bucket.get_object(Self::key(agent, digest)).await {
            Ok(resp) => {
                let code = resp.status_code();
                if code == 404 {
                    return Ok(None);
                }
                if !(200..300).contains(&code) {
                    return Err(io::Error::other(format!("s3 get returned {code}")));
                }
                Ok(Some(resp.bytes().to_vec()))
            }
            Err(e) if s3_is_404(&e) => Ok(None),
            Err(e) => Err(s3_err(e)),
        }
    }

    async fn exists(&self, agent: &str, digest: &str) -> io::Result<bool> {
        if !valid_agent_key(agent) || !valid_digest(digest) {
            return Ok(false);
        }
        match self.bucket.head_object(Self::key(agent, digest)).await {
            Ok((_, code)) => Ok((200..300).contains(&code)),
            Err(e) if s3_is_404(&e) => Ok(false),
            Err(e) => Err(s3_err(e)),
        }
    }

    async fn delete(&self, agent: &str, digest: &str) -> io::Result<()> {
        if !valid_agent_key(agent) || !valid_digest(digest) {
            return Ok(());
        }
        match self.bucket.delete_object(Self::key(agent, digest)).await {
            Ok(_) => Ok(()),
            Err(e) if s3_is_404(&e) => Ok(()),
            Err(e) => Err(s3_err(e)),
        }
    }

    async fn rename_agent(&self, old: &str, new: &str) -> io::Result<()> {
        if !valid_agent_key(old) || !valid_agent_key(new) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid agent key"));
        }
        // Page through every object under `blobs/<old>/`, copy each to `blobs/<new>/<sha>`, then delete
        // the old key. S3 has no atomic prefix rename, so this is a copy-then-delete per object. Correct
        // by construction and tolerant: an absent prefix lists as empty and is a no-op success.
        let old_prefix = format!("blobs/{old}/");
        let results = self.bucket.list(old_prefix.clone(), None).await.map_err(s3_err)?;
        for page in results {
            for obj in page.contents {
                // The key tail after `blobs/<old>/` is the sha; re-anchor it under the new agent. Skip a
                // pseudo-"directory" marker (a key that is exactly the prefix) — it has no sha tail.
                let Some(sha) = obj.key.strip_prefix(&old_prefix).filter(|s| !s.is_empty()) else {
                    continue;
                };
                let new_key = format!("blobs/{new}/{sha}");
                // Copy first; only delete the source once the copy is confirmed 2xx, so a failed copy
                // can never lose the blob.
                let code = self.bucket.copy_object_internal(&obj.key, &new_key).await.map_err(s3_err)?;
                if !(200..300).contains(&code) {
                    return Err(io::Error::other(format!("s3 copy returned {code}")));
                }
                self.bucket.delete_object(&obj.key).await.map_err(s3_err)?;
            }
        }
        Ok(())
    }

    async fn delete_agent(&self, agent: &str) -> io::Result<()> {
        if !valid_agent_key(agent) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid agent key"));
        }
        // List then delete, page by page. An absent prefix lists empty → nothing to delete → success.
        let prefix = format!("blobs/{agent}/");
        let results = self.bucket.list(prefix, None).await.map_err(s3_err)?;
        for page in results {
            for obj in page.contents {
                self.bucket.delete_object(&obj.key).await.map_err(s3_err)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // sha256("hello world") — precomputed, so `put` is checked against a known-good address.
    const HELLO_DIGEST: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    fn fs(dir: &Path) -> FsBlobs {
        FsBlobs { dir: dir.join("blobs") }
    }

    #[tokio::test]
    async fn put_returns_the_correct_sha_and_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let b = fs(d.path());
        let got = b.put("agentx", b"hello world").await.unwrap();
        assert_eq!(got, HELLO_DIGEST, "put returns the server-computed sha256");
        let back = b.get("agentx", &got).await.unwrap().expect("round-trips");
        assert_eq!(back, b"hello world");
    }

    #[tokio::test]
    async fn missing_get_is_none_and_exists_reflects_state() {
        let d = tempfile::tempdir().unwrap();
        let b = fs(d.path());
        assert!(b.get("agentx", HELLO_DIGEST).await.unwrap().is_none(), "absent → None");
        assert!(!b.exists("agentx", HELLO_DIGEST).await.unwrap());
        let got = b.put("agentx", b"hello world").await.unwrap();
        assert!(b.exists("agentx", &got).await.unwrap());
        b.delete("agentx", &got).await.unwrap();
        assert!(b.get("agentx", &got).await.unwrap().is_none(), "delete then get → None");
    }

    #[tokio::test]
    async fn re_upload_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let b = fs(d.path());
        let a = b.put("agentx", b"same bytes").await.unwrap();
        let c = b.put("agentx", b"same bytes").await.unwrap();
        assert_eq!(a, c, "content-addressed: identical bytes → identical address, no error");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn perms_are_0600_file_and_0700_dir() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let b = fs(d.path());
        let digest = b.put("agentx", b"hello world").await.unwrap();
        let file = b.object_path("agentx", &digest);
        let fmode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600, "blob files are owner-only");
        let dmode = std::fs::metadata(d.path().join("blobs").join("agentx")).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700, "the agent dir is owner-only");
    }

    #[tokio::test]
    async fn per_agent_namespace_is_isolated() {
        // The security boundary: a blob put under agent "a" is NOT reachable via agent "b" by digest.
        let d = tempfile::tempdir().unwrap();
        let b = fs(d.path());
        let digest = b.put("a", b"secret").await.unwrap();
        assert!(b.get("a", &digest).await.unwrap().is_some());
        assert!(b.get("b", &digest).await.unwrap().is_none(), "same digest, other agent → absent");
        assert!(!b.exists("b", &digest).await.unwrap());
    }

    #[tokio::test]
    async fn malformed_digest_and_agent_never_touch_disk() {
        let d = tempfile::tempdir().unwrap();
        let b = fs(d.path());
        assert!(b.get("agentx", "not-a-digest").await.unwrap().is_none());
        assert!(b.get("agentx", "../../etc/passwd").await.unwrap().is_none());
        assert!(b.get("../evil", HELLO_DIGEST).await.unwrap().is_none());
        assert!(b.put("../evil", b"x").await.is_err(), "an unsafe agent key is rejected outright");
    }

    #[tokio::test]
    async fn rename_agent_moves_blobs_to_the_new_name() {
        // The rename fix: a blob put under the old name is reachable under the new name and gone from
        // the old, so an agent rename doesn't strand its blobs.
        let d = tempfile::tempdir().unwrap();
        let b = fs(d.path());
        let digest = b.put("old", b"payload").await.unwrap();
        b.rename_agent("old", "new").await.unwrap();
        assert_eq!(b.get("new", &digest).await.unwrap().as_deref(), Some(&b"payload"[..]), "reachable under new name");
        assert!(b.get("old", &digest).await.unwrap().is_none(), "gone from the old name");
    }

    #[tokio::test]
    async fn rename_agent_with_no_blobs_is_ok() {
        // An agent that never uploaded a blob has no dir to move — a no-op, not an error.
        let d = tempfile::tempdir().unwrap();
        let b = fs(d.path());
        b.rename_agent("neveruploaded", "renamed").await.expect("absent source is success");
    }

    #[tokio::test]
    async fn delete_agent_closes_the_recycled_name_leak() {
        // The purge fix, at the storage layer: after delete_agent, the bytes are gone, so a new agent
        // recycling the SAME name (same key namespace) cannot read the previous owner's blob.
        let d = tempfile::tempdir().unwrap();
        let b = fs(d.path());
        let digest = b.put("recycled", b"private bytes").await.unwrap();
        assert!(b.get("recycled", &digest).await.unwrap().is_some());
        b.delete_agent("recycled").await.unwrap();
        assert!(b.get("recycled", &digest).await.unwrap().is_none(), "the recycled name reads nothing — leak closed");
        // Idempotent: purging an agent with no blobs is fine.
        b.delete_agent("recycled").await.expect("absent is success");
    }

    #[tokio::test]
    async fn bulk_ops_reject_unsafe_agent_keys() {
        let d = tempfile::tempdir().unwrap();
        let b = fs(d.path());
        assert!(b.rename_agent("../evil", "ok").await.is_err());
        assert!(b.rename_agent("ok", "../evil").await.is_err());
        assert!(b.delete_agent("../evil").await.is_err());
    }

    #[tokio::test]
    async fn corruption_is_visible_to_a_re_hash() {
        // The read-time verification the handler relies on: after corrupting the on-disk bytes, the
        // fetched bytes no longer hash to the digest.
        let d = tempfile::tempdir().unwrap();
        let b = fs(d.path());
        let digest = b.put("agentx", b"hello world").await.unwrap();
        std::fs::write(b.object_path("agentx", &digest), b"tampered").unwrap();
        let back = b.get("agentx", &digest).await.unwrap().unwrap();
        assert_ne!(sha256_hex(&back), digest, "re-hash catches the tamper");
    }
}
