//! Sign-in requests waiting for a human's approval, one per Hub.
//!
//! Creating a browser or device-code sign-in gives the CLI a polling value (the browser `state`
//! or the `device_code`), and the first poll that presents it after the human approves receives
//! the Hub session. An agent runtime can stop the process that asked for the request before the
//! human answers, so the value is recorded here as soon as the Hub returns it, and any later
//! `agit login --complete` on the same `AGIT_HOME` can claim the approval.
//!
//! Once approved, the polling value is as good as the session, so a record gets the protections
//! of the credentials beside it: the private credential directory, a private file replaced
//! atomically, and the credential mutation lock around every change. There is one record per Hub
//! authority, so a new request for a Hub supersedes the previous one.
//!
//! The Hub hands out an approved session once, so a process that presents a value some other
//! process already claimed only hears that the request is gone. A claim therefore leaves a marker
//! naming the claimed value by digest, which lets a later `agit login --complete` for the same
//! request report the sign-in that already happened instead of a failure. Signing out forgets
//! the record, and a claim of it still in flight then saves nothing (see [`commit`]).

use super::{
    HubAuthority, HubCredential, create_credential_dir, local_state, mutation_guard, open_lock,
    write_private,
};
use crate::Result;
use anyhow::{Context, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A record is a few short fields; a larger file was not written by agit.
const LIMIT: u64 = 16 * 1024;

const EXTENSION: &str = "pending";

/// The marker of the request most recently claimed for a Hub.
const CLAIMED_EXTENSION: &str = "claimed";

/// A request lives as long as the Hub says, but never longer than this, so a corrupt expiry
/// cannot keep a record claimable indefinitely.
const LONGEST_LIFETIME: u64 = 24 * 60 * 60;

/// The shortest pause between two polls of one request, whatever interval the Hub asks for.
pub const MIN_INTERVAL: u64 = 2;

/// The longest pause between two polls, so a corrupt or hostile interval cannot turn a bounded
/// wait into a single check.
const LONGEST_INTERVAL: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flow {
    /// Approved on the Hub's CLI authorization page and polled by `state`.
    Browser,
    /// Approved by entering a user code on the Hub and polled by `device_code`.
    Device,
}

impl Flow {
    /// The Hub endpoint that hands out the session once the request is approved.
    pub fn poll_path(self) -> &'static str {
        match self {
            Self::Browser => "api/auth/cli/poll",
            Self::Device => "api/auth/device/token",
        }
    }

    /// The request body field that carries the polling value.
    pub fn poll_key(self) -> &'static str {
        match self {
            Self::Browser => "state",
            Self::Device => "device_code",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PendingLogin {
    /// The Hub address the request was created at.
    pub hub: String,
    pub flow: Flow,
    /// The polling value. Whoever presents it after the approval receives the session. A device
    /// code is never printed; a browser state appears only in the `--complete` command the
    /// non-interactive handoff prints for that request.
    pub secret: String,
    /// The pause the Hub asks for between polls, in seconds.
    pub interval: u64,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Where the human approves a device-code request, as the waiting flow showed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,
}

/// The link and code a human uses to approve a device-code request. Neither claims the approval:
/// the code only lets a signed-in human approve the request, so it can be shown again to a human
/// who never received it.
#[derive(Clone, Serialize, Deserialize)]
pub struct Approval {
    pub url: String,
    pub user_code: String,
}

impl std::fmt::Debug for PendingLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingLogin")
            .field("hub", &self.hub)
            .field("flow", &self.flow)
            .field("interval", &self.interval)
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl PendingLogin {
    /// A request the Hub created just now and keeps for `expires_in` seconds.
    pub fn new(hub: &str, flow: Flow, secret: &str, interval: u64, expires_in: u64) -> Self {
        let created_at = Utc::now();
        let lifetime = chrono::TimeDelta::try_seconds(expires_in.min(LONGEST_LIFETIME) as i64)
            .unwrap_or_default();
        Self {
            hub: hub.trim().trim_end_matches('/').to_owned(),
            flow,
            secret: secret.to_owned(),
            interval,
            created_at,
            expires_at: created_at + lifetime,
            approval: None,
        }
    }

    /// The same request with the link and code the human approves it with.
    pub fn with_approval(self, url: &str, user_code: &str) -> Self {
        Self {
            approval: Some(Approval {
                url: url.to_owned(),
                user_code: user_code.to_owned(),
            }),
            ..self
        }
    }

    /// The pause between two polls: the Hub's interval, within bounds that keep the wait useful.
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.interval.clamp(MIN_INTERVAL, LONGEST_INTERVAL))
    }

    /// How much longer the Hub keeps the request, bounded like its lifetime, so a corrupt expiry
    /// cannot push a deadline computed from it out of range.
    pub fn remaining(&self) -> Duration {
        (self.expires_at - Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO)
            .min(Duration::from_secs(LONGEST_LIFETIME))
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_zero()
    }
}

fn record_path(dir: &Path, authority: &HubAuthority) -> PathBuf {
    dir.join(format!("{}.{EXTENSION}", authority.storage_key()))
}

fn claimed_path(dir: &Path, authority: &HubAuthority) -> PathBuf {
    dir.join(format!("{}.{CLAIMED_EXTENSION}", authority.storage_key()))
}

/// Record `request` as its Hub's request waiting for approval, replacing any earlier one, and
/// say whether that earlier one was still waiting.
///
/// The replacement waits for the claim lock, so it lands between two polls of the earlier
/// request and never between a poll that received its session and the commit of that session;
/// replacing it there would make the claim sign out a session the human approved.
pub fn save(request: &PendingLogin) -> Result<bool> {
    let _claim = claim_lock(&request.hub)?;
    save_in(&crate::infra::config::credentials_dir()?, request)
}

fn save_in(dir: &Path, request: &PendingLogin) -> Result<bool> {
    let authority = HubAuthority::parse(&request.hub)?;
    let _guard = mutation_guard(dir)?;
    let replaces = load_from(dir, &authority)
        .ok()
        .flatten()
        .is_some_and(|earlier| !earlier.expired());
    write_private(
        &record_path(dir, &authority),
        &serde_json::to_vec(request)?,
        "the recorded sign-in request",
    )?;
    Ok(replaces)
}

/// The request recorded for `hub`, whether or not it has expired. A record that cannot be
/// parsed, or that names another Hub authority, is no request.
pub fn load(hub: &str) -> Result<Option<PendingLogin>> {
    load_from(
        &crate::infra::config::credentials_dir()?,
        &HubAuthority::parse(hub)?,
    )
}

fn load_from(dir: &Path, authority: &HubAuthority) -> Result<Option<PendingLogin>> {
    Ok(read_bounded(&record_path(dir, authority))?
        .and_then(|body| serde_json::from_slice::<PendingLogin>(&body).ok())
        .filter(|request| authority.matches(&request.hub)))
}

/// The body of a small private file agit wrote, or `None` when it is absent or too large to be
/// one.
fn read_bounded(path: &Path) -> Result<Option<Vec<u8>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::Error::new(error))
                .with_context(|| format!("cannot inspect {}", path.display()));
        }
    };
    ensure!(
        metadata.is_file(),
        "the sign-in record {} is not a regular file",
        path.display()
    );
    let mut body = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(LIMIT + 1).read_to_end(&mut body))
        .with_context(|| format!("cannot read {}", path.display()))?;
    Ok((body.len() as u64 <= LIMIT).then_some(body))
}

/// The Hubs other than `hub` whose recorded requests still wait for approval.
pub fn waiting_elsewhere(hub: &str) -> Vec<String> {
    let Ok(dir) = crate::infra::config::credentials_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut hubs: Vec<String> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == EXTENSION)
        })
        .filter_map(|path| {
            let body = read_bounded(&path).ok().flatten()?;
            let request = serde_json::from_slice::<PendingLogin>(&body).ok()?;
            let authority = HubAuthority::parse(&request.hub).ok()?;
            (record_path(&dir, &authority) == path && !authority.matches(hub) && !request.expired())
                .then_some(request.hub)
        })
        .collect();
    hubs.sort();
    hubs
}

/// Forget the Hub's recorded request if it is still the one polled by `secret`, so a process
/// that finishes an older request never removes a newer one.
///
/// The caller holds the claim lock. A claim of the request that has received its session holds
/// that lock until the session is committed, so forgetting the request here, for example because
/// it expired, never withdraws a session the human approved; only a sign-out does that.
pub fn remove(hub: &str, secret: &str) -> Result<()> {
    remove_in(
        &crate::infra::config::credentials_dir()?,
        &HubAuthority::parse(hub)?,
        secret,
    )
}

fn remove_in(dir: &Path, authority: &HubAuthority, secret: &str) -> Result<()> {
    let _guard = mutation_guard(dir)?;
    if load_from(dir, authority)?.is_none_or(|request| request.secret != secret) {
        return Ok(());
    }
    let path = record_path(dir, authority);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(local_state::io_failure("cannot remove", &path, error)),
    }
}

/// Forget the Hub's recorded request, whichever it is, and its claim marker. The caller holds
/// the mutation lock.
pub(super) fn discard(dir: &Path, authority: &HubAuthority) {
    let _ = std::fs::remove_file(record_path(dir, authority));
    let _ = std::fs::remove_file(claimed_path(dir, authority));
}

/// Forget every recorded request and claim marker, and count the requests. The caller holds the
/// mutation lock.
pub(super) fn discard_all(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut forgotten = 0;
    for path in entries.flatten().map(|entry| entry.path()) {
        let Some(extension) = path.extension() else {
            continue;
        };
        if (extension == EXTENSION || extension == CLAIMED_EXTENSION)
            && std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file())
        {
            let removed = std::fs::remove_file(&path).is_ok();
            if removed && extension == EXTENSION {
                forgotten += 1;
            }
        }
    }
    forgotten
}

#[derive(Serialize, Deserialize)]
struct Claimed {
    /// The digest of the claimed polling value, which names the request without holding it.
    digest: String,
    /// When the claimed request was created. Credentials saved since then are its sign-in.
    created_at: DateTime<Utc>,
}

fn digest(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// Why the session a claim received was not saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withdrawal {
    /// No request is recorded for the Hub any more: a sign-out forgot it.
    Forgotten,
    /// A newer sign-in request for the Hub replaced it.
    Replaced,
}

impl Withdrawal {
    /// How the Hub's recorded request `current` withdraws `request`, if it does.
    pub fn of(request: &PendingLogin, current: Option<&PendingLogin>) -> Option<Self> {
        match current {
            Some(current) if current.secret == request.secret => None,
            Some(_) => Some(Self::Replaced),
            None => Some(Self::Forgotten),
        }
    }
}

/// Save `credential`, the session a poll of `request` received, then leave the claim marker and
/// forget the record, all under the credential mutation lock. A sign-out forgets records and
/// credentials under the same lock, so it either follows this commit and removes the credentials,
/// or precedes it and nothing is saved. A claim never forgets the record before the credentials
/// are saved, so a process that finds the record gone while no sign-out or expiry ended the
/// request finds the credentials saved.
///
/// `recorded` says whether `request` was its Hub's recorded request when the claim began: only
/// then does the record's removal or replacement withdraw the claim, because a request that was
/// never recorded is one no sign-out knows about. Returns why nothing was saved, or `None` once
/// the credentials are saved; a marker or record left behind after that does not undo the
/// sign-in, so neither fails the commit.
pub fn commit(
    hub: &str,
    request: &PendingLogin,
    recorded: bool,
    credential: &HubCredential,
) -> Result<Option<Withdrawal>> {
    let (path, credential) = super::bound_for_saving(hub, credential)?;
    commit_in(
        path.parent().context("credential directory is missing")?,
        &path,
        &HubAuthority::parse(hub)?,
        request,
        recorded,
        &credential,
    )
}

fn commit_in(
    dir: &Path,
    path: &Path,
    authority: &HubAuthority,
    request: &PendingLogin,
    recorded: bool,
    credential: &HubCredential,
) -> Result<Option<Withdrawal>> {
    let _guard = mutation_guard(dir)?;
    let withdrawal = Withdrawal::of(request, load_from(dir, authority)?.as_ref());
    if recorded && withdrawal.is_some() {
        return Ok(withdrawal);
    }
    super::save_at(path, credential)?;
    let marker = Claimed {
        digest: digest(&request.secret),
        created_at: request.created_at,
    };
    let _ = serde_json::to_vec(&marker)
        .map_err(anyhow::Error::from)
        .and_then(|body| {
            write_private(
                &claimed_path(dir, authority),
                &body,
                "the claimed sign-in marker",
            )
        });
    if withdrawal.is_none() {
        let _ = std::fs::remove_file(record_path(dir, authority));
    }
    Ok(None)
}

/// Serialize finishing one Hub's sign-in requests, from the poll that can consume an approval
/// until the credentials it yields are committed. A process that waited for the lock reads the
/// record and the claim marker as the holder left them: a request another process finished has
/// its marker and saved credentials, so it is reported signed in instead of polled; without the
/// lock it could read the Hub's "already used" answer before the winner saved, and report a
/// failed sign-in.
///
/// Replacing the record ([`save`]) and forgetting it for any reason but a sign-out ([`remove`])
/// happen under this lock too, so neither can withdraw a claim that already received its
/// session. A sign-out does not take it: it cancels such a claim (see [`commit`]).
///
/// The wait is bounded (see [`local_state::lock_exclusive`]), and no holder keeps the lock across
/// a wait for the human.
pub fn claim_lock(hub: &str) -> Result<std::fs::File> {
    let (file, path) = open_claim_lock(hub)?;
    local_state::lock_exclusive(&file, &path, "the sign-in lock")?;
    Ok(file)
}

/// [`claim_lock`] without waiting: `None` while another process holds the lock.
pub fn try_claim_lock(hub: &str) -> Result<Option<std::fs::File>> {
    let (file, path) = open_claim_lock(hub)?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(file)),
        Err(error) if local_state::is_contended(&error) => Ok(None),
        Err(error) => Err(local_state::io_failure("cannot lock", &path, error)),
    }
}

fn open_claim_lock(hub: &str) -> Result<(std::fs::File, PathBuf)> {
    let authority = HubAuthority::parse(hub)?;
    let dir = crate::infra::config::credentials_dir()?;
    create_credential_dir(&dir)?;
    let path = dir.join(format!("{}.login.lock", authority.storage_key()));
    let file = open_lock(&path)
        .map_err(|error| local_state::io_failure("cannot open the sign-in lock", &path, error))?;
    Ok((file, path))
}

/// The Hub's saved credentials when they are the sign-in of the request polled by `secret`: an
/// agit process on this `AGIT_HOME` claimed that request, and the credentials were saved after
/// it was created.
pub fn signed_in_by(hub: &str, secret: &str) -> Option<HubCredential> {
    let dir = crate::infra::config::credentials_dir().ok()?;
    let authority = HubAuthority::parse(hub).ok()?;
    let body = read_bounded(&claimed_path(&dir, &authority)).ok()??;
    let marker: Claimed = serde_json::from_slice(&body).ok()?;
    if marker.digest != digest(secret) {
        return None;
    }
    let path = crate::infra::config::credentials_path(hub).ok()?;
    let modified: DateTime<Utc> = std::fs::symlink_metadata(path)
        .ok()?
        .modified()
        .ok()?
        .into();
    if modified < marker.created_at {
        return None;
    }
    super::load(hub)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record holds a secret as good as a session once approved, so it must be private like
    /// the credentials; and a process finishing an older request must leave a newer one alone,
    /// or a login that superseded it would lose its only claimable copy. A claim marker names
    /// its request by digest only, so it never holds a value that could be presented again. A
    /// claim of a recorded request saves its session only while the record still names it; one
    /// that saves after a newer request replaced the record, or after a sign-out forgot it, signs
    /// the account back in behind the human's back.
    #[test]
    fn records_are_private_per_authority_and_only_their_own_request_removes_them() {
        let dir = tempfile::tempdir().unwrap();
        let hub = "http://node.test:8177";
        let authority = HubAuthority::parse(hub).unwrap();
        let older = PendingLogin::new(hub, Flow::Device, "SYNTHETIC-older", 5, 600);
        let newer = PendingLogin::new(hub, Flow::Browser, "SYNTHETIC-newer", 2, 600);
        assert!(!save_in(dir.path(), &older).unwrap());
        assert!(save_in(dir.path(), &newer).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(record_path(dir.path(), &authority))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let other = HubAuthority::parse("http://other.test:8177").unwrap();
        assert!(load_from(dir.path(), &other).unwrap().is_none());

        remove_in(dir.path(), &authority, &older.secret).unwrap();
        let kept = load_from(dir.path(), &authority).unwrap().unwrap();
        assert_eq!(kept.secret, newer.secret);
        assert_eq!(kept.flow, Flow::Browser);
        assert!(!kept.expired());
        assert!(!format!("{kept:?}").contains("SYNTHETIC"));

        let saved = dir.path().join(format!("{}.json", authority.storage_key()));
        let credential = HubCredential {
            account_id: None,
            username: "synthetic".into(),
            email: None,
            hub: Some(hub.into()),
            access_token: "SYNTHETIC-access".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "SYNTHETIC-refresh".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let commit = |request: &PendingLogin, recorded: bool| {
            commit_in(
                dir.path(),
                &saved,
                &authority,
                request,
                recorded,
                &credential,
            )
            .unwrap()
        };
        assert_eq!(commit(&older, true), Some(Withdrawal::Replaced));
        assert!(!saved.exists() && !claimed_path(dir.path(), &authority).exists());
        assert_eq!(commit(&older, false), None);
        assert_eq!(
            load_from(dir.path(), &authority).unwrap().unwrap().secret,
            newer.secret
        );
        assert_eq!(commit(&newer, true), None);
        assert!(load_from(dir.path(), &authority).unwrap().is_none());
        assert!(super::super::load_at(&saved).is_some());
        let marker = std::fs::read(claimed_path(dir.path(), &authority)).unwrap();
        let marker: Claimed = serde_json::from_slice(&marker).unwrap();
        assert_eq!(marker.digest, digest(&newer.secret));
        assert!(!marker.digest.contains("SYNTHETIC"));
        std::fs::remove_file(&saved).unwrap();
        assert_eq!(commit(&newer, true), Some(Withdrawal::Forgotten));
        assert!(!saved.exists());
    }
}
