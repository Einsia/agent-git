//! Credential storage: an access token + refresh token pair.
//!
//! The access token is short-lived (one hour) and rides on every request; the refresh token is
//! long-lived (thirty days) and only buys a new access token. On a 401 the client refreshes once
//! and retries, so one sign-in lasts a month.
//!
//! # One file per hub
//!
//! Its home is `~/.agit/credentials/<hub-host>.json` (see
//! [`crate::infra::config::credentials_path`]). "Switching `AGIT_HUB_URL` switches identity
//! without signing in again" holds either way; only the shape on disk differs: one shared file
//! forces every read and write to pull every hub's tokens into the same memory, and leaves
//! "hand over / delete just one hub's credentials" with no way to express it.

use crate::Result;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One hub's credentials. The whole file is this single object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubCredential {
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
    /// Which hub these credentials belong to (the full address). The file name keeps only the
    /// host key and the address cannot be recovered from it, while `logout --all` revokes the
    /// server-side session hub by hub and has to know where to send that.
    /// [`save`] fills it in; a file written without it reads back as `None`.
    #[serde(default)]
    pub hub: Option<String>,
    pub access_token: String,
    pub access_expires_at: String,
    pub refresh_token: String,
    pub refresh_expires_at: String,
}

impl HubCredential {
    /// Whether the access token has expired.
    ///
    /// A malformed timestamp counts as **not expired**: better a 401 from the server, which has
    /// the accurate answer, than a local misjudgment that spends an extra refresh. The server
    /// makes the same call the other way (fail closed).
    pub fn access_expired(&self) -> bool {
        expired(&self.access_expires_at)
    }

    pub fn refresh_expired(&self) -> bool {
        expired(&self.refresh_expires_at)
    }
}

fn expired(ts: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => chrono::Utc::now() > t.with_timezone(&chrono::Utc),
        Err(_) => false,
    }
}

/// Read one hub's credentials. A missing file means "not signed in yet", not an error.
pub fn load(hub: &str) -> Option<HubCredential> {
    load_at(&crate::infra::config::credentials_path(hub).ok()?)
}

/// Read from an explicit path (for tests and tools).
pub fn load_at(path: &Path) -> Option<HubCredential> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write one hub's credentials (0600).
pub fn save(hub: &str, cred: &HubCredential) -> Result<()> {
    let cred = HubCredential {
        hub: Some(hub.trim().trim_end_matches('/').to_string()),
        ..cred.clone()
    };
    let cred = &cred;
    save_at(&crate::infra::config::credentials_path(hub)?, cred)
}

pub fn save_at(path: &Path, cred: &HubCredential) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(cred)?))
        .with_context(|| format!("cannot write {}", path.display()))?;
    set_private(path)?;
    Ok(())
}

/// Delete one hub's credentials. Returns whether there was one.
pub fn remove(hub: &str) -> Result<bool> {
    let p = crate::infra::config::credentials_path(hub)?;
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("cannot remove {}", p.display())),
    }
}

/// Every hub host key signed in on this machine (the file name minus `.json`).
///
/// Host keys, not full URLs: the URL does not persist, and a logout-style hint only has to name
/// which hubs you are still signed in to.
pub fn logged_in_hosts() -> Vec<String> {
    let Ok(dir) = crate::infra::config::credentials_dir() else {
        return vec![];
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".json").map(str::to_string)
        })
        .collect();
    out.sort();
    out
}

/// Every credentials file on this machine: (host key, credentials).
///
/// A file that cannot be read (corrupt, not JSON) is still listed, with `None` for the
/// credentials: revocation candidates can skip it, cleanup cannot — with only a broken file
/// left, `logout --all` still has to delete it.
pub fn all() -> Vec<(String, Option<HubCredential>)> {
    let Ok(dir) = crate::infra::config::credentials_dir() else {
        return vec![];
    };
    logged_in_hosts()
        .into_iter()
        .map(|host| {
            let cred = load_at(&dir.join(format!("{host}.json")));
            (host, cred)
        })
        .collect()
}

/// Clear every hub's credentials. Returns how many were deleted.
pub fn remove_all() -> Result<usize> {
    let dir = crate::infra::config::credentials_dir()?;
    let mut n = 0usize;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(0);
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("json") {
            std::fs::remove_file(&p).with_context(|| format!("cannot remove {}", p.display()))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Credentials for the current hub.
pub fn current() -> Option<HubCredential> {
    load(&crate::infra::config::hub_url())
}

pub fn current_user() -> Option<String> {
    current().map(|c| c.username)
}

/// The current account's email (used for git commit).
pub fn current_email() -> Option<String> {
    current().and_then(|c| c.email)
}

pub fn is_logged_in() -> bool {
    current().is_some_and(|c| !c.refresh_expired())
}

#[cfg(unix)]
fn set_private(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p)?.permissions();
    perm.set_mode(0o600);
    std::fs::set_permissions(p, perm)
        .with_context(|| format!("cannot chmod 0600 on {}", p.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private(_p: &Path) -> Result<()> {
    crate::warn(
        "this platform cannot set 0600; the credentials file may be readable by other users on this machine",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::config::hub_host_key;

    fn cred(access_exp: &str, refresh_exp: &str) -> HubCredential {
        HubCredential {
            username: "alice".into(),
            email: Some("alice@example.com".into()),
            hub: None,
            access_token: "agit_at_x".into(),
            access_expires_at: access_exp.into(),
            refresh_token: "agit_rt_x".into(),
            refresh_expires_at: refresh_exp.into(),
        }
    }

    fn future() -> String {
        (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()
    }

    fn past() -> String {
        (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339()
    }

    /// Pins one file per hub: what is written reads back, and two hubs never touch each other.
    #[test]
    fn roundtrip_and_per_hub_isolation() {
        let d = tempfile::tempdir().unwrap();
        let local = d
            .path()
            .join(format!("{}.json", hub_host_key("http://localhost:8177")));
        let corp = d
            .path()
            .join(format!("{}.json", hub_host_key("https://hub.corp.com")));
        save_at(&local, &cred(&future(), &future())).unwrap();
        save_at(&corp, &cred(&future(), &future())).unwrap();

        assert!(load_at(&local).is_some());
        assert!(load_at(&corp).is_some());
        assert_ne!(local, corp, "each hub isolates into its own file");
        assert!(load_at(&d.path().join("unknown.json")).is_none());
    }

    /// A trailing slash, the scheme and the path must not change where it lands — otherwise
    /// someone who signed in once is signed out by spelling the hub a different way.
    #[test]
    fn the_same_hub_written_differently_lands_on_one_file() {
        for h in [
            "http://h:8177",
            "http://h:8177/",
            "https://h:8177",
            "http://h:8177/api/",
        ] {
            assert_eq!(hub_host_key(h), "h_8177", "{h}");
        }
    }

    #[test]
    fn missing_file_is_none_not_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(
            load_at(&d.path().join("nope.json")).is_none(),
            "not signed in yet is a normal state"
        );
    }

    #[test]
    fn expiry_of_both_tokens() {
        assert!(cred(&past(), &future()).access_expired());
        assert!(!cred(&past(), &future()).refresh_expired());
        assert!(cred(&past(), &past()).refresh_expired());
        // A malformed timestamp counts as not expired: the server's 401 arbitrates.
        assert!(!cred("garbage", &future()).access_expired());
    }

    #[cfg(unix)]
    #[test]
    fn credentials_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("h.json");
        save_at(&p, &cred(&future(), &future())).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the credentials file must be 0600");
    }
}
