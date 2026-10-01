//! Credential storage: an access token + refresh token pair.
//!
//! The Hub supplies the expiry of each token. Access tokens authenticate requests, while
//! refresh tokens exchange for a new pair whose expiry starts at the successful renewal.
//!
//! # One file per hub
//!
//! Its home is a bounded authority digest under `~/.agit/credentials/` (see
//! [`crate::infra::config::credentials_path`]). "Switching `AGIT_HUB_URL` switches identity
//! without signing in again" holds either way; only the shape on disk differs: one shared file
//! forces every read and write to pull every hub's tokens into the same memory, and leaves
//! "hand over / delete just one hub's credentials" with no way to express it.

use super::hub_authority::HubAuthority;
use super::local_state;
use crate::Result;
use anyhow::{Context, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod pending;

/// One hub's credentials. The whole file is this single object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubCredential {
    /// Immutable account identity returned by the authenticated Hub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
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
    left.account_id == right.account_id && same_token_identity(left, right)
}

fn same_token_identity(left: &HubCredential, right: &HubCredential) -> bool {
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
    let (path, cred) = bound_for_saving(hub, cred)?;
    let _guard = mutation_guard(path.parent().context("credential directory is missing")?)?;
    save_at(&path, &cred)
}

/// `cred` bound to `hub`, and the path it is saved at; a credential bound to another authority
/// is refused.
fn bound_for_saving(hub: &str, cred: &HubCredential) -> Result<(PathBuf, HubCredential)> {
    let authority = HubAuthority::parse(hub)?;
    ensure!(
        cred.hub.is_none() || bound_to(cred, &authority),
        "credential authority does not match the selected Hub"
    );
    let cred = HubCredential {
        hub: Some(hub.trim().trim_end_matches('/').to_string()),
        ..cred.clone()
    };
    Ok((crate::infra::config::credentials_path(hub)?, cred))
}

/// Refresh results cannot replace a concurrent login or resurrect a signed-out credential.
pub fn save_refreshed(
    hub: &str,
    expected: &HubCredential,
    fresh: &HubCredential,
) -> Result<Option<HubCredential>> {
    let authority = HubAuthority::parse(hub)?;
    ensure!(
        bound_to(expected, &authority)
            && bound_to(fresh, &authority)
            && expected.account_id == fresh.account_id
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

/// Verified account metadata cannot replace a concurrent sign-in or revive a signed-out slot.
pub fn save_verified_account(
    hub: &str,
    expected: &HubCredential,
    account_id: &str,
) -> Result<bool> {
    let authority = HubAuthority::parse(hub)?;
    ensure!(
        bound_to(expected, &authority),
        "credential authority does not match"
    );
    ensure!(
        !account_id.is_empty()
            && expected
                .account_id
                .as_deref()
                .is_none_or(|saved| saved == account_id),
        "verified account does not match saved identity"
    );
    let mut verified = expected.clone();
    verified.account_id = Some(account_id.to_owned());
    save_refreshed_at(
        &crate::infra::config::credentials_dir()?,
        &authority,
        expected,
        &verified,
    )
    .map(|saved| saved.is_some())
}

fn save_refreshed_at(
    dir: &Path,
    authority: &HubAuthority,
    expected: &HubCredential,
    fresh: &HubCredential,
) -> Result<Option<HubCredential>> {
    let _guard = mutation_guard(dir)?;
    let Some(current) = load_from(dir, authority)? else {
        return Ok(None);
    };
    // Verified metadata may enrich an unchanged token pair, but cannot replace a known identity.
    if !same_token_identity(&current, expected)
        || expected
            .account_id
            .as_ref()
            .is_some_and(|account| current.account_id.as_ref() != Some(account))
        || current
            .account_id
            .as_ref()
            .zip(fresh.account_id.as_ref())
            .is_some_and(|(current, fresh)| current != fresh)
    {
        return Ok(None);
    }
    let fresh = HubCredential {
        account_id: current.account_id.or_else(|| fresh.account_id.clone()),
        ..fresh.clone()
    };
    save_at(
        &dir.join(format!("{}.json", authority.storage_key())),
        &fresh,
    )?;
    Ok(Some(fresh))
}

/// Open a credential lock file the way every credential writer does.
fn open_lock(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn create_credential_dir(dir: &Path) -> Result<()> {
    super::config::create_state_dir(dir)
        .map_err(|error| local_state::io_failure("cannot create credential directory", dir, error))
}

fn mutation_guard(dir: &Path) -> Result<std::fs::File> {
    create_credential_dir(dir)?;
    let path = dir.with_extension("lock");
    let file = open_lock(&path).map_err(|error| {
        local_state::io_failure("cannot open credential mutation lock", &path, error)
    })?;
    local_state::lock_exclusive(&file, &path, "the credential mutation lock")?;
    Ok(file)
}

/// Prove that [`save`] can persist a credential before a caller spends a one-time Hub
/// authorization on obtaining one.
///
/// It performs the filesystem steps saving performs: creating the credential directory, opening
/// the mutation lock for writing, creating a temporary file beside the credentials and renaming
/// it over another. A missing lock file is not created; creating a temporary file in its
/// directory proves the same permission, so a failed check leaves no file behind. A lock another
/// process holds is not a failure here, because saving waits for it. A refused write is reported
/// as [`local_state::LocalStateError::NotWritable`].
pub fn preflight_writable() -> Result<()> {
    let dir = crate::infra::config::credentials_dir()?;
    create_credential_dir(&dir)?;
    let temporary = |dir: &Path| {
        tempfile::NamedTempFile::new_in(dir).map_err(|error| {
            local_state::io_failure("cannot create a temporary file in", dir, error)
        })
    };
    let lock_path = dir.with_extension("lock");
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(lock) => match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => {}
            Err(error) if local_state::is_contended(&error) => {}
            Err(error) => {
                return Err(local_state::io_failure(
                    "cannot lock credential mutations",
                    &lock_path,
                    error,
                ));
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            drop(temporary(lock_path.parent().unwrap_or(&dir))?);
        }
        Err(error) => {
            return Err(local_state::io_failure(
                "cannot open credential mutation lock",
                &lock_path,
                error,
            ));
        }
    }
    let probe = temporary(&dir)?;
    #[cfg(not(windows))]
    {
        // Saving replaces the credential file by rename; the replaced path is removed when
        // `target` drops, so the probe leaves nothing behind.
        let target = temporary(&dir)?.into_temp_path();
        probe.persist(&target).map_err(|error| {
            local_state::io_failure("cannot rename a temporary file to", &target, error.error)
        })?;
    }
    #[cfg(windows)]
    drop(probe);
    Ok(())
}

/// [`preflight_writable`] for a diagnostic, which creates no state: while the credential
/// directory does not exist yet, its nearest existing ancestor must accept a new file, since
/// saving creates the directory there. Returns whether the credential directory exists.
#[cfg(feature = "cli")]
pub fn probe_writable() -> Result<bool> {
    let dir = crate::infra::config::credentials_dir()?;
    if dir.is_dir() {
        return preflight_writable().map(|()| true);
    }
    let existing = dir
        .ancestors()
        .find(|ancestor| ancestor.is_dir())
        .context("no ancestor of the credential directory exists")?;
    tempfile::NamedTempFile::new_in(existing).map_err(|error| {
        local_state::io_failure("cannot create a temporary file in", existing, error)
    })?;
    Ok(false)
}

/// Serialize renewal through persistence without preventing a concurrent login or logout.
/// The separate mutation lock still fences the final write against an identity change.
#[cfg(feature = "cli")]
pub(crate) fn refresh_guard(hub: &str) -> Result<std::fs::File> {
    let authority = HubAuthority::parse(hub)?;
    let dir = crate::infra::config::credentials_dir()?;
    create_credential_dir(&dir)?;
    let path = dir.join(format!("{}.refresh.lock", authority.storage_key()));
    let file = open_lock(&path).map_err(|error| {
        local_state::io_failure("cannot open Hub credential refresh lock", &path, error)
    })?;
    local_state::lock_exclusive(&file, &path, "the Hub credential refresh lock")?;
    Ok(file)
}

pub fn save_at(path: &Path, cred: &HubCredential) -> Result<()> {
    let body = format!("{}\n", serde_json::to_string_pretty(cred)?);
    write_private(path, body.as_bytes(), "saved Hub credentials")?;
    // File metadata invalidates the tokenless sidecar if another writer replaces the credential.
    #[cfg(feature = "cli")]
    let _ = save_account_cache(path, cred.account_id.as_deref());
    Ok(())
}

/// Replace `path` with `body`, readable by this user only. A reader sees the previous file or
/// the new one, never a partial write, because the body is complete in a private temporary file
/// before it takes the name.
fn write_private(path: &Path, body: &[u8], what: &str) -> Result<()> {
    if let Some(d) = path.parent() {
        super::config::create_state_dir(d)?;
    }
    #[cfg(windows)]
    super::windows_security::write_private_file(path, body)
        .with_context(|| format!("cannot write {what} to {}", path.display()))?;
    #[cfg(not(windows))]
    {
        use std::io::Write as _;
        let directory = path.parent().context("credential directory is missing")?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(|error| {
            local_state::io_failure("cannot create a temporary file in", directory, error)
        })?;
        set_private(temporary.path())?;
        temporary.write_all(body)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(path)
            .map_err(|_| anyhow!("cannot persist {what}"))?;
    }
    Ok(())
}

#[cfg(feature = "cli")]
#[derive(Serialize, Deserialize)]
struct AccountCache {
    account_id: Option<String>,
    length: u64,
    modified: u128,
}

#[cfg(feature = "cli")]
fn credential_stamp(path: &Path) -> Option<(u64, u128)> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some((
        metadata.len(),
        metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos(),
    ))
}

#[cfg(feature = "cli")]
fn save_account_cache(path: &Path, account_id: Option<&str>) -> Result<()> {
    let Some((length, modified)) = credential_stamp(path) else {
        return Ok(());
    };
    let cache = AccountCache {
        account_id: account_id.map(str::to_owned),
        length,
        modified,
    };
    crate::telemetry::state::write_json(&path.with_extension("identity"), &cache)
}

/// No token-bearing file or network request is needed to describe the selected analytics identity.
#[cfg(feature = "cli")]
pub fn analytics_account(hub: &str) -> (Option<String>, &'static str) {
    let Ok(path) = crate::infra::config::credentials_path(hub) else {
        return (None, "unavailable");
    };
    analytics_account_at(&path)
}

#[cfg(feature = "cli")]
fn analytics_account_at(path: &Path) -> (Option<String>, &'static str) {
    let Some(stamp) = credential_stamp(path) else {
        return (None, "signed_out");
    };
    let cache =
        crate::telemetry::state::read_json::<AccountCache>(&path.with_extension("identity"), 2048)
            .ok()
            .flatten();
    let Some(cache) = cache.filter(|c| (c.length, c.modified) == stamp) else {
        return (None, "signed_in_id_missing");
    };
    let account_id = cache.account_id.filter(|id| {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    });
    let state = if account_id.is_some() {
        "identified"
    } else {
        "signed_in_id_missing"
    };
    (account_id, state)
}

/// What a sign-out removed.
#[derive(Default)]
pub struct Removed {
    /// How many credential records were removed.
    pub records: usize,
    /// The removed credentials bound to a Hub. A sign-out reads and revokes credentials before it
    /// removes them, so a sign-in that saves in between leaves a session only these name; the
    /// sign-out revokes it with them.
    pub bound: Vec<HubCredential>,
}

/// Forget the Hub's sign-in request waiting for approval, and its claim marker, as a sign-out
/// does before it reads the credentials it revokes. A claim of that request in flight commits
/// under the same lock (see [`pending::commit`]), so it either committed already, and the
/// sign-out reads and revokes its credentials, or it finds the request forgotten and saves
/// nothing.
pub fn forget_request(hub: &str) -> Result<()> {
    let authority = HubAuthority::parse(hub)?;
    let dir = crate::infra::config::credentials_dir()?;
    if !dir.is_dir() {
        return Ok(());
    }
    let _guard = mutation_guard(&dir)?;
    pending::discard(&dir, &authority);
    Ok(())
}

/// Signing out clears the selected slot and all positively bound compatibility records, and
/// forgets the Hub's sign-in request waiting for approval, which could otherwise sign the
/// account back in when a later command claims it. The credentials are read under the lock
/// every save takes, so the ones returned include any a sign-in saved after the caller read
/// them.
pub fn remove(hub: &str) -> Result<Removed> {
    let authority = HubAuthority::parse(hub)?;
    let dir = crate::infra::config::credentials_dir()?;
    let _guard = mutation_guard(&dir)?;
    pending::discard(&dir, &authority);
    remove_from(&dir, &authority)
}

fn remove_from(dir: &Path, authority: &HubAuthority) -> Result<Removed> {
    let canonical = dir.join(format!("{}.json", authority.storage_key()));
    let paths = record_paths(dir)?;
    let mut removed = Removed::default();
    let _ = std::fs::remove_file(canonical.with_extension("identity"));
    let saved = std::fs::symlink_metadata(&canonical)
        .is_ok_and(|metadata| metadata.is_file())
        .then(|| load_at(&canonical))
        .flatten()
        .filter(|cred| bound_to(cred, authority));
    match std::fs::remove_file(&canonical) {
        Ok(()) => {
            removed.records += 1;
            removed.bound.extend(saved);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(anyhow!("cannot remove saved Hub credentials")),
    }
    for path in paths {
        let Some(cred) = recognized_record(&path, true).filter(|cred| bound_to(cred, authority))
        else {
            continue;
        };
        let _ = std::fs::remove_file(path.with_extension("identity"));
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed.records += 1;
                removed.bound.push(cred);
            }
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

/// Forget every sign-in request waiting for approval, and every claim marker, as a global
/// sign-out does before it reads the credentials it revokes (see [`forget_request`]); a leftover
/// approved request would otherwise sign an account back in when a later command claims it.
/// Returns whether a request was forgotten.
pub fn forget_pending() -> Result<bool> {
    let dir = crate::infra::config::credentials_dir()?;
    if !dir.is_dir() {
        return Ok(false);
    }
    let _guard = mutation_guard(&dir)?;
    Ok(pending::discard_all(&dir) > 0)
}

/// Explicit global logout clears every local credential record, including unbound records, and
/// returns what it removed as [`remove`] does.
pub fn remove_all() -> Result<Removed> {
    let dir = crate::infra::config::credentials_dir()?;
    if !dir.is_dir() {
        return Ok(Removed::default());
    }
    let _guard = mutation_guard(&dir)?;
    pending::discard_all(&dir);
    let mut removed = Removed::default();
    for path in record_paths(&dir)? {
        let cred = recognized_record(&path, false);
        let _ = std::fs::remove_file(path.with_extension("identity"));
        std::fs::remove_file(&path).map_err(|_| anyhow!("cannot remove saved Hub credentials"))?;
        removed.records += 1;
        removed.bound.extend(cred);
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

    #[cfg(all(unix, feature = "cli"))]
    #[test]
    fn first_state_writers_create_private_home() {
        use std::os::unix::{fs::PermissionsExt, process::CommandExt};
        const CHILD: &str = "AGIT_TEST_PRIVATE_STATE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "infra::credentials::tests::first_state_writers_create_private_home",
                ])
                .env(CHILD, "1");
            unsafe {
                child.pre_exec(|| {
                    libc::umask(0o002);
                    Ok(())
                });
            }
            assert!(child.status().unwrap().success());
            return;
        }
        let root = tempfile::tempdir().unwrap();
        for name in ["credentials", "telemetry"] {
            let home = root.path().join(name);
            let directory = home.join(name);
            let _guard = if name == "credentials" {
                mutation_guard(&directory).unwrap()
            } else {
                crate::telemetry::state::gate(&directory, true).unwrap()
            };
            for path in [&home, &directory] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
    }

    fn cred(access_exp: &str, refresh_exp: &str) -> HubCredential {
        HubCredential {
            account_id: None,
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

    #[test]
    fn account_identity_roundtrips_and_legacy_credentials_remain_readable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credential.json");
        let mut value = cred(&future(), &future());
        value.account_id = Some("account-alice".into());
        save_at(&path, &value).unwrap();
        assert_eq!(
            load_at(&path).unwrap().account_id.as_deref(),
            Some("account-alice")
        );
        let mut legacy = serde_json::to_value(value).unwrap();
        legacy.as_object_mut().unwrap().remove("account_id");
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(load_at(&path).unwrap().account_id.is_none());
    }

    #[cfg(feature = "cli")]
    #[test]
    fn analytics_identity_cache_excludes_tokens_and_rejects_stale_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credential.json");
        let mut credential = cred(&future(), &future());
        credential.account_id = Some("account-alice".into());
        save_at(&path, &credential).unwrap();
        assert_eq!(
            analytics_account_at(&path),
            (Some("account-alice".into()), "identified")
        );
        let body = std::fs::read_to_string(path.with_extension("identity")).unwrap();
        assert!(!body.contains("token"));
        assert!(!body.contains("email"));
        assert!(!body.contains("username"));
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(analytics_account_at(&path), (None, "signed_in_id_missing"));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(analytics_account_at(&path), (None, "signed_out"));
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

    /// A selected sign-out removes only the records bound to its Hub and returns exactly those,
    /// because it revokes every returned session at that session's Hub; returning a foreign
    /// record would sign out an account the sign-out never selected.
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
        let removed = remove_from(dir.path(), &authority).unwrap();
        assert!(!own_path.exists());
        assert!(!canonical.exists());
        assert_eq!(std::fs::read(foreign_path).unwrap(), foreign_bytes);
        assert_eq!(removed.records, 2);
        assert_eq!(removed.bound.len(), removed.records);
        assert!(removed.bound.iter().all(|cred| cred.hub == own.hub));
        assert_eq!(remove_from(dir.path(), &authority).unwrap().records, 0);
    }

    #[test]
    fn refresh_compare_and_swap_preserves_login_logout_and_rotated_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let value = HubCredential {
            account_id: Some("alice-account".into()),
            ..bound("http://node.test:8177")
        };
        let authority = HubAuthority::parse(value.hub.as_deref().unwrap()).unwrap();
        let canonical = dir.path().join(format!("{}.json", authority.storage_key()));
        let refreshed = HubCredential {
            access_token: "fresh-access".into(),
            refresh_token: "fresh-refresh".into(),
            ..value.clone()
        };
        assert!(
            save_refreshed_at(dir.path(), &authority, &value, &refreshed)
                .unwrap()
                .is_none()
        );
        save_at(&canonical, &value).unwrap();
        assert!(
            save_refreshed_at(dir.path(), &authority, &value, &refreshed)
                .unwrap()
                .is_some()
        );
        for replacement in [
            HubCredential {
                account_id: None,
                username: "bob".into(),
                ..value.clone()
            },
            refreshed.clone(),
            HubCredential {
                account_id: Some("another-account".into()),
                ..value.clone()
            },
            HubCredential {
                account_id: None,
                ..value.clone()
            },
        ] {
            save_at(&canonical, &replacement).unwrap();
            let before = std::fs::read(&canonical).unwrap();
            assert!(
                save_refreshed_at(dir.path(), &authority, &value, &refreshed)
                    .unwrap()
                    .is_none()
            );
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
            assert!(remove_from(dir.path(), &authority).unwrap().records > 0);
            assert!(!original.exists());
        } else {
            assert!(load_from(dir.path(), &authority).unwrap().is_none());
            assert_eq!(remove_from(dir.path(), &authority).unwrap().records, 0);
            assert!(original.exists());
        }
    }
}
