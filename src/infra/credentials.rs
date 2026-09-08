//! Credential storage: an access token + refresh token pair.
//!
//! The access token is short-lived (one hour) and rides on every request; the refresh token is
//! long-lived (thirty days) and only buys a new access token. On a 401 the client refreshes once
//! and retries, so one sign-in lasts a month.
//!
//! # One file per hub
//!
//! Its home is a bounded authority digest under `~/.agit/credentials/` (see
//! [`crate::infra::config::credentials_path`]). "Switching `AGIT_HUB_URL` switches identity
//! without signing in again" holds either way; only the shape on disk differs: one shared file
//! forces every read and write to pull every hub's tokens into the same memory, and leaves
//! "hand over / delete just one hub's credentials" with no way to express it.

use super::hub_authority::HubAuthority;
use crate::Result;
use anyhow::{Context, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

/// Only a positively bound record can supply credentials for a selected authority.
pub fn load(hub: &str) -> Option<HubCredential> {
    load_checked(hub).ok().flatten()
}

/// A present canonical slot is authoritative even when it cannot supply credentials.
pub fn load_checked(hub: &str) -> Result<Option<HubCredential>> {
    let authority = HubAuthority::parse(hub)?;
    let dir = crate::infra::config::credentials_dir()?;
    load_from(&dir, &authority)
}

fn load_from(dir: &Path, authority: &HubAuthority) -> Result<Option<HubCredential>> {
    let path = dir.join(format!("{}.json", authority.storage_key()));
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "saved Hub credentials are not a regular file"
            );
            let cred =
                load_at(&path).ok_or_else(|| anyhow!("saved Hub credentials are unreadable"))?;
            ensure!(
                bound_to(&cred, authority),
                "saved Hub credentials belong to another authority"
            );
            return Ok(Some(cred));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(anyhow!("cannot inspect saved Hub credentials")),
    }
    let mut selected: Option<HubCredential> = None;
    for path in record_paths(dir)? {
        let Some(cred) = recognized_record(&path, true) else {
            continue;
        };
        if !bound_to(&cred, authority) {
            continue;
        }
        if let Some(previous) = &selected {
            ensure!(
                same_identity(previous, &cred),
                "conflicting saved Hub credentials; sign in again"
            );
        } else {
            selected = Some(cred);
        }
    }
    Ok(selected)
}

fn bound_to(cred: &HubCredential, authority: &HubAuthority) -> bool {
    cred.hub
        .as_deref()
        .is_some_and(|hub| authority.matches(hub))
}

fn same_identity(left: &HubCredential, right: &HubCredential) -> bool {
    left.username == right.username
        && left.email == right.email
        && left.access_token == right.access_token
        && left.access_expires_at == right.access_expires_at
        && left.refresh_token == right.refresh_token
        && left.refresh_expires_at == right.refresh_expires_at
}

fn record_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => return Err(anyhow!("cannot inspect saved Hub credentials")),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| anyhow!("cannot inspect saved Hub credentials"))?;
        let path = entry.path();
        if path
            .extension()
            .and_then(|part| part.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn recognized_record(path: &Path, legacy_only: bool) -> Option<HubCredential> {
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let cred = load_at(path)?;
    let hub = cred.hub.as_deref()?;
    let authority = HubAuthority::parse(hub).ok()?;
    if !crate::infra::config::legacy_hub_record_matches(path, hub)
        && (legacy_only
            || !crate::infra::config::hub_record_key_matches(path, &authority.storage_key()))
    {
        return None;
    }
    Some(cred)
}

/// Explicit-path tooling reads raw records without selecting a network destination.
pub fn load_at(path: &Path) -> Option<HubCredential> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Saving cannot transfer a credential between authorities.
pub fn save(hub: &str, cred: &HubCredential) -> Result<()> {
    let authority = HubAuthority::parse(hub)?;
    ensure!(
        cred.hub.is_none() || bound_to(cred, &authority),
        "credential authority does not match the selected Hub"
    );
    let cred = HubCredential {
        hub: Some(hub.trim().trim_end_matches('/').to_string()),
        ..cred.clone()
    };
    let path = crate::infra::config::credentials_path(hub)?;
    let _guard = mutation_guard(path.parent().context("credential directory is missing")?)?;
    save_at(&path, &cred)
}

/// Refresh results cannot replace a concurrent login or resurrect a signed-out credential.
pub fn save_refreshed(hub: &str, expected: &HubCredential, fresh: &HubCredential) -> Result<bool> {
    let authority = HubAuthority::parse(hub)?;
    ensure!(
        bound_to(expected, &authority)
            && bound_to(fresh, &authority)
            && expected.username == fresh.username,
        "refreshed credential identity does not match"
    );
    save_refreshed_at(
        &crate::infra::config::credentials_dir()?,
        &authority,
        expected,
        fresh,
    )
}

fn save_refreshed_at(
    dir: &Path,
    authority: &HubAuthority,
    expected: &HubCredential,
    fresh: &HubCredential,
) -> Result<bool> {
    let _guard = mutation_guard(dir)?;
    let Some(current) = load_from(dir, authority)? else {
        return Ok(false);
    };
    if !same_identity(&current, expected) {
        return Ok(false);
    }
    save_at(
        &dir.join(format!("{}.json", authority.storage_key())),
        fresh,
    )?;
    Ok(true)
}

fn mutation_guard(dir: &Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(dir).context("cannot create credential directory")?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(dir.with_extension("lock"))
        .context("cannot open credential mutation lock")?;
    fs2::FileExt::lock_exclusive(&file).context("cannot lock credential mutations")?;
    Ok(file)
}

pub fn save_at(path: &Path, cred: &HubCredential) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let body = format!("{}\n", serde_json::to_string_pretty(cred)?);
    #[cfg(windows)]
    super::windows_security::write_private_file(path, body.as_bytes())
        .with_context(|| format!("cannot write private credentials to {}", path.display()))?;
    #[cfg(not(windows))]
    {
        use std::io::Write as _;
        let directory = path.parent().context("credential directory is missing")?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        set_private(temporary.path())?;
        temporary.write_all(body.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(path)
            .map_err(|_| anyhow!("cannot persist saved Hub credentials"))?;
    }
    Ok(())
}

/// Signing out clears the selected slot and all positively bound compatibility records.
pub fn remove(hub: &str) -> Result<bool> {
    let authority = HubAuthority::parse(hub)?;
    let dir = crate::infra::config::credentials_dir()?;
    let _guard = mutation_guard(&dir)?;
    remove_from(&dir, &authority)
}

fn remove_from(dir: &Path, authority: &HubAuthority) -> Result<bool> {
    let canonical = dir.join(format!("{}.json", authority.storage_key()));
    let paths = record_paths(dir)?;
    let mut removed = match std::fs::remove_file(&canonical) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Err(anyhow!("cannot remove saved Hub credentials")),
    };
    for path in paths {
        if !recognized_record(&path, true).is_some_and(|cred| bound_to(&cred, authority)) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(anyhow!("cannot remove saved Hub credentials")),
        }
    }
    Ok(removed)
}

/// Display names come from validated metadata, never untrusted filenames.
pub fn logged_in_hosts() -> Vec<String> {
    let mut out: Vec<_> = all()
        .into_iter()
        .filter_map(|(label, cred)| cred.map(|_| label))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Invalid records remain visible for local cleanup but cannot nominate a revoke destination.
pub fn all() -> Vec<(String, Option<HubCredential>)> {
    all_checked().unwrap_or_default()
}

/// Explicit cleanup must distinguish an unreadable directory from absent credentials.
pub fn all_checked() -> Result<Vec<(String, Option<HubCredential>)>> {
    let dir = crate::infra::config::credentials_dir()?;
    let paths = record_paths(&dir)?;
    Ok(paths
        .into_iter()
        .map(|path| match recognized_record(&path, false) {
            Some(cred) => (
                super::hub_authority::safe_label(cred.hub.as_deref().unwrap_or_default()),
                Some(cred),
            ),
            None => ("unbound credential record".into(), None),
        })
        .collect())
}

/// Explicit global logout clears every local credential record, including unbound records.
pub fn remove_all() -> Result<usize> {
    let dir = crate::infra::config::credentials_dir()?;
    let _guard = mutation_guard(&dir)?;
    let mut removed = 0;
    for path in record_paths(&dir)? {
        std::fs::remove_file(&path).map_err(|_| anyhow!("cannot remove saved Hub credentials"))?;
        removed += 1;
    }
    Ok(removed)
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

#[cfg(not(any(unix, windows)))]
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
        let local = d.path().join(format!(
            "{}.json",
            hub_host_key("http://localhost:8177").unwrap()
        ));
        let corp = d.path().join(format!(
            "{}.json",
            hub_host_key("https://hub.corp.com").unwrap()
        ));
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
            assert_eq!(
                hub_host_key(h).unwrap(),
                hub_host_key("http://h:8177").unwrap(),
                "{h}"
            );
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
    fn bound(hub: &str) -> HubCredential {
        HubCredential {
            hub: Some(hub.into()),
            ..cred(&future(), &future())
        }
    }

    fn legacy(dir: &Path, value: &HubCredential) -> PathBuf {
        let path = dir.join(format!(
            "{}.json",
            crate::infra::config::legacy_hub_host_key(value.hub.as_deref().unwrap())
        ));
        save_at(&path, value).unwrap();
        path
    }

    #[test]
    fn canonical_slot_blocks_corrupt_or_foreign_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let value = bound("HTTP://source.test:8177");
        let authority = HubAuthority::parse(value.hub.as_deref().unwrap()).unwrap();
        let old = legacy(dir.path(), &value);
        let canonical = dir.path().join(format!("{}.json", authority.storage_key()));
        assert_eq!(
            load_from(dir.path(), &authority).unwrap().unwrap().username,
            "alice"
        );
        let old_bytes = std::fs::read(&old).unwrap();
        for bytes in [
            b"broken".to_vec(),
            serde_json::to_vec(&bound("http://foreign.test:8177")).unwrap(),
            serde_json::to_vec(&cred(&future(), &future())).unwrap(),
        ] {
            std::fs::write(&canonical, &bytes).unwrap();
            assert!(load_from(dir.path(), &authority).is_err());
            assert_eq!(std::fs::read(&canonical).unwrap(), bytes);
            assert_eq!(std::fs::read(&old).unwrap(), old_bytes);
        }
        std::fs::remove_file(&canonical).unwrap();
        std::fs::create_dir(&canonical).unwrap();
        assert!(load_from(dir.path(), &authority).is_err());
    }

    #[test]
    fn compatibility_candidates_require_exact_filenames_and_unambiguous_identity() {
        let dir = tempfile::tempdir().unwrap();
        let value = bound("HTTP://node.test:8177/base");
        let authority = HubAuthority::parse(value.hub.as_deref().unwrap()).unwrap();
        let old = legacy(dir.path(), &value);
        let alias = HubCredential {
            hub: Some("https://NODE.test:8177/other".into()),
            ..value.clone()
        };
        let alias_path = legacy(dir.path(), &alias);
        assert!(load_from(dir.path(), &authority).unwrap().is_some());
        let conflict = HubCredential {
            refresh_token: "different-refresh".into(),
            ..alias.clone()
        };
        save_at(&alias_path, &conflict).unwrap();
        assert!(load_from(dir.path(), &authority).is_err());
        std::fs::remove_file(alias_path).unwrap();
        std::fs::rename(&old, dir.path().join("unrecognized.json")).unwrap();
        assert!(load_from(dir.path(), &authority).unwrap().is_none());
    }

    #[test]
    fn selected_logout_preserves_colliding_foreign_legacy_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = bound("HTTP://foreign.test:8177");
        let own = bound("http://selected.test:8177");
        let authority = HubAuthority::parse(own.hub.as_deref().unwrap()).unwrap();
        let foreign_path = legacy(dir.path(), &foreign);
        let foreign_bytes = std::fs::read(&foreign_path).unwrap();
        let own_path = legacy(dir.path(), &own);
        let canonical = dir.path().join(format!("{}.json", authority.storage_key()));
        save_at(&canonical, &own).unwrap();
        assert!(remove_from(dir.path(), &authority).unwrap());
        assert!(!own_path.exists());
        assert!(!canonical.exists());
        assert_eq!(std::fs::read(foreign_path).unwrap(), foreign_bytes);
        assert!(!remove_from(dir.path(), &authority).unwrap());
    }

    #[test]
    fn refresh_compare_and_swap_preserves_login_logout_and_rotated_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let value = bound("http://node.test:8177");
        let authority = HubAuthority::parse(value.hub.as_deref().unwrap()).unwrap();
        let canonical = dir.path().join(format!("{}.json", authority.storage_key()));
        let refreshed = HubCredential {
            access_token: "fresh-access".into(),
            refresh_token: "fresh-refresh".into(),
            ..value.clone()
        };
        assert!(!save_refreshed_at(dir.path(), &authority, &value, &refreshed).unwrap());
        save_at(&canonical, &value).unwrap();
        assert!(save_refreshed_at(dir.path(), &authority, &value, &refreshed).unwrap());
        for replacement in [
            HubCredential {
                username: "bob".into(),
                ..value.clone()
            },
            refreshed.clone(),
        ] {
            save_at(&canonical, &replacement).unwrap();
            let before = std::fs::read(&canonical).unwrap();
            assert!(!save_refreshed_at(dir.path(), &authority, &value, &refreshed).unwrap());
            assert_eq!(std::fs::read(&canonical).unwrap(), before);
        }
        std::fs::write(&canonical, b"invalid").unwrap();
        assert!(save_refreshed_at(dir.path(), &authority, &value, &refreshed).is_err());
    }

    #[test]
    fn explicit_save_rejects_foreign_or_invalid_metadata_before_writing() {
        let foreign = bound("http://foreign.test:8177");
        for hub in [
            "http://selected.test:8177",
            "http://selected.test?secret=value",
            "http://user:password@selected.test",
        ] {
            assert!(save(hub, &foreign).is_err());
        }
    }
    #[test]
    fn legacy_case_aliases_follow_the_filesystem_identity() {
        let dir = tempfile::tempdir().unwrap();
        let value = bound("http://node.test:8177");
        let original = legacy(dir.path(), &value);
        let alias = HubCredential {
            hub: Some("http://NODE.test:8177".into()),
            ..value.clone()
        };
        let expected = dir.path().join("NODE.test_8177.json");
        let authority = HubAuthority::parse(value.hub.as_deref().unwrap()).unwrap();
        save_at(&original, &alias).unwrap();
        if expected.exists() {
            assert!(load_from(dir.path(), &authority).unwrap().is_some());
            assert!(remove_from(dir.path(), &authority).unwrap());
            assert!(!original.exists());
        } else {
            assert!(load_from(dir.path(), &authority).unwrap().is_none());
            assert!(!remove_from(dir.path(), &authority).unwrap());
            assert!(original.exists());
        }
    }
}
