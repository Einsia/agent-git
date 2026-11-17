//! Dictionary synchronization is independently retryable and carries only encrypted envelopes.

use super::crypto::{Envelope, UserKey};
use super::keys::{self, CloudKey};
use super::storage::{Store, read_private, write_private};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Deserialize, Serialize)]
pub struct Entry {
    pub package_id: String,
    pub dictionary_id: String,
    pub key_version: i64,
    pub sequence: i64,
}

#[derive(Deserialize, Serialize)]
pub struct Inventory {
    pub packages: Vec<Entry>,
    pub next_after: Option<i64>,
}

#[derive(Deserialize)]
pub struct Receipt {
    pub package_id: String,
    pub sequence: i64,
}

pub trait Transport {
    fn ensure_key(&self) -> crate::Result<CloudKey>;
    fn key(&self, version: i64) -> crate::Result<CloudKey>;
    fn upload(&self, id: &str, envelope: &Envelope) -> crate::Result<Receipt>;
    fn inventory(&self, after: i64) -> crate::Result<Inventory>;
    fn download(&self, id: &str) -> crate::Result<Envelope>;
    fn upload_policy(&self, _id: &str, _envelope: &Envelope) -> crate::Result<Receipt> {
        anyhow::bail!("privacy policy synchronization is unavailable")
    }
    fn policy_inventory(&self, _after: i64) -> crate::Result<Inventory> {
        anyhow::bail!("privacy policy synchronization is unavailable")
    }
    fn download_policy(&self, _id: &str) -> crate::Result<Envelope> {
        anyhow::bail!("privacy policy synchronization is unavailable")
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Report {
    pub uploaded: usize,
    pub downloaded: usize,
    pub deferred: usize,
    #[serde(default)]
    pub policy_uploaded: usize,
    #[serde(default)]
    pub policy_received: usize,
    #[serde(default)]
    pub policy_unresolved: usize,
}

/// Who refused an upload that an immediate retry would send into the same refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// A 403 without a Hub error body was produced in front of the Hub, not by it.
    Edge,
    /// The Hub answered 403 for this account.
    Authorization,
    /// The Hub rejected the package itself.
    Rejected,
}

/// Only status codes that stay the same on an immediate retry are refusals. Transport
/// failures, 5xx, 408, 429 and 401 (the worker cannot renew access; the next foreground
/// request does) keep retrying on every sync.
#[cfg(feature = "cli")]
pub fn classify(error: &anyhow::Error) -> Option<(Refusal, u16, String)> {
    let api = error.downcast_ref::<crate::hub::client::ApiError>()?;
    let refusal = match (api.status, api.kind.as_str()) {
        (401 | 408 | 429, _) => return None,
        (403, "") => Refusal::Edge,
        (403, _) => Refusal::Authorization,
        (400..=499, _) => Refusal::Rejected,
        _ => return None,
    };
    Some((refusal, api.status, api.detail.trim().to_owned()))
}

/// Without the CLI's Hub client no transport error carries a Hub status to classify.
#[cfg(not(feature = "cli"))]
pub fn classify(_error: &anyhow::Error) -> Option<(Refusal, u16, String)> {
    None
}

const FIRST_RETRY_SECONDS: i64 = 10 * 60;
const LAST_RETRY_SECONDS: i64 = 6 * 60 * 60;

/// The latest refused upload for one Hub. While `retry_at` is in the future the outbox is not
/// sent at all, so a refusal costs one request per backoff step instead of one per package on
/// every push. Downloads are unaffected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub refusal: Refusal,
    pub status: u16,
    pub detail: String,
    pub failures: u32,
    pub retry_at: i64,
}

impl Health {
    pub fn path(root: &Path, hub_label: &str) -> PathBuf {
        let digest = Sha256::digest(hub_label.as_bytes());
        root.join("sync")
            .join(format!("{}.json", hex::encode(digest)))
    }

    /// A missing or unreadable record means no refusal is known, never a blocked outbox.
    pub fn load(path: &Path) -> Option<Self> {
        let bytes = read_private(path, 64 * 1024).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Delays double from the first step and stop growing at the last, so an edge policy that
    /// is fixed later is picked up without user action.
    fn delay(failures: u32) -> i64 {
        let doublings = failures.saturating_sub(1).min(16);
        (FIRST_RETRY_SECONDS << doublings).min(LAST_RETRY_SECONDS)
    }

    fn settle(
        path: &Path,
        previous: Option<Self>,
        refused: Option<(Refusal, u16, String)>,
        now: i64,
    ) -> crate::Result<()> {
        let Some((refusal, status, detail)) = refused else {
            return Self::forget(path);
        };
        let failures = previous
            .map_or(0, |health| health.failures)
            .saturating_add(1);
        let health = Self {
            refusal,
            status,
            detail,
            failures,
            retry_at: now + Self::delay(failures),
        };
        write_private(path, &serde_json::to_vec(&health)?)
    }

    pub fn forget(path: &Path) -> crate::Result<()> {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    /// One sentence that says who refused, and the next step for that cause.
    pub fn describe(&self, now: i64) -> (String, String) {
        let wait = self.retry_at - now;
        let retry = if wait <= 0 {
            "agit retries on the next push".to_owned()
        } else if wait < 3600 {
            format!("agit retries automatically in {} min", (wait + 59) / 60)
        } else {
            format!("agit retries automatically in {} h", (wait + 3599) / 3600)
        };
        let said = if self.detail.is_empty() {
            format!("HTTP {}", self.status)
        } else {
            format!("HTTP {}: {}", self.status, self.detail)
        };
        match self.refusal {
            Refusal::Edge => (
                format!(
                    "privacy dictionary upload was blocked before it reached the Hub (HTTP {}); this is a network policy, not a sign-in problem",
                    self.status
                ),
                format!("{retry}; if it persists, report it with the output of `agit doctor`"),
            ),
            Refusal::Authorization => (
                format!("the Hub refused privacy dictionary upload for this account ({said})"),
                "run `agit login`, then push again".to_owned(),
            ),
            Refusal::Rejected => (
                format!("the Hub rejected a privacy dictionary package ({said})"),
                format!("{retry}; if it persists, report it with the output of `agit doctor`"),
            ),
        }
    }
}

fn health_path(hub: &str, policy: bool) -> Option<PathBuf> {
    let label = crate::infra::hub_authority::HubAuthority::parse(hub)
        .ok()?
        .label();
    let root = keys::root().ok()?;
    Some(Health::path(
        &if policy {
            root.join("policy-transport")
        } else {
            root
        },
        &label,
    ))
}

/// The refusal recorded for `hub`, read without contacting it.
pub fn recorded_refusal(hub: &str) -> Option<Health> {
    Health::load(&health_path(hub, false)?)
}

pub fn recorded_policy_refusal(hub: &str) -> Option<Health> {
    Health::load(&health_path(hub, true)?)
}

/// A new sign-in is the remedy an account refusal asks for, so it must not wait out the
/// backoff that refusal started.
pub fn forget_refusal(hub: &str) {
    for policy in [false, true] {
        if let Some(path) = health_path(hub, policy) {
            let _ = Health::forget(&path);
        }
    }
}

fn receiving_key(
    key: &UserKey,
    version: i64,
    root: &Path,
    transport: &impl Transport,
) -> crate::Result<UserKey> {
    use base64::Engine;
    if version == key.version {
        return UserKey::new(
            key.owner.clone(),
            version,
            base64::engine::general_purpose::STANDARD.decode(key.encoded().as_str())?,
        );
    }
    keys::cached(root, &key.owner, Some(version)).or_else(|_| {
        let document = transport.key(version)?;
        ensure!(
            document.account_id == key.owner.account && document.version == version,
            "cloud privacy key identity changed"
        );
        let historical = UserKey::new(
            key.owner.clone(),
            version,
            base64::engine::general_purpose::STANDARD.decode(&document.key)?,
        )?;
        let _ = keys::cache(root, &historical);
        Ok(historical)
    })
}

fn dictionary_receive(
    store: &mut Store,
    key: &UserKey,
    root: &Path,
    transport: &impl Transport,
    deadline: Instant,
    report: &mut Report,
) -> crate::Result<()> {
    let receive = |store: &mut Store, id: &str, version: i64| -> crate::Result<()> {
        let historical = receiving_key(key, version, root, transport)?;
        let envelope = transport.download(id)?;
        ensure!(
            envelope.key_version == version,
            "privacy inventory disagrees with its envelope"
        );
        store.import(id, &envelope, &historical)
    };
    for (id, version) in store.retries()? {
        if Instant::now() >= deadline {
            report.deferred += 1;
            return Ok(());
        }
        if receive(store, &id, version).is_ok() {
            store.clear_retry(&id)?;
            report.downloaded += 1;
        } else {
            report.deferred += 1;
        }
    }
    let mut cursor = store.inventory_cursor()?;
    for _ in 0..8 {
        if Instant::now() >= deadline {
            report.deferred += 1;
            return Ok(());
        }
        let inventory = transport.inventory(cursor)?;
        validate_inventory(&inventory, cursor)?;
        for entry in &inventory.packages {
            if Instant::now() >= deadline {
                report.deferred += 1;
                return Ok(());
            }
            if receive(store, &entry.package_id, entry.key_version).is_ok() {
                report.downloaded += 1;
            } else {
                store.retry(&entry.package_id, entry.key_version)?;
                report.deferred += 1;
            }
            cursor = entry.sequence;
            store.save_inventory_cursor(cursor)?;
        }
        if inventory.next_after.is_none() {
            return Ok(());
        }
    }
    report.deferred += 1;
    Ok(())
}

fn validate_inventory(inventory: &Inventory, mut after: i64) -> crate::Result<()> {
    ensure!(
        inventory.packages.len() <= 100,
        "privacy inventory exceeds its item budget"
    );
    for entry in &inventory.packages {
        ensure!(
            entry.sequence > after
                && entry.key_version > 0
                && uuid::Uuid::parse_str(&entry.package_id).is_ok()
                && uuid::Uuid::parse_str(&entry.dictionary_id).is_ok(),
            "invalid privacy inventory entry"
        );
        after = entry.sequence;
    }
    ensure!(
        inventory.next_after.is_none()
            || (!inventory.packages.is_empty() && inventory.next_after == Some(after)),
        "invalid privacy inventory cursor"
    );
    Ok(())
}

fn policy_receive(
    store: &mut Store,
    key: &UserKey,
    root: &Path,
    transport: &impl Transport,
    deadline: Instant,
    report: &mut Report,
) -> crate::Result<()> {
    store.db.execute(
        "UPDATE metadata SET value='0' WHERE key='policy_receipt_complete'",
        [],
    )?;
    let mut dictionary = super::dictionary::Dictionary::default();
    // Loading uses cached keys only; dictionary failures cannot consume the policy receive budget.
    store.load_since(&mut dictionary, 0, |version| {
        if version == key.version {
            use base64::Engine;
            UserKey::new(
                key.owner.clone(),
                version,
                base64::engine::general_purpose::STANDARD.decode(key.encoded().as_str())?,
            )
        } else {
            keys::cached(root, &key.owner, Some(version))
        }
    })?;
    store.resolve_policy(&dictionary)?;
    let mut cursor = store.policy_progress()?.received;
    for _ in 0..8 {
        if Instant::now() >= deadline {
            report.deferred += 1;
            return Ok(());
        }
        let inventory = transport.policy_inventory(cursor)?;
        validate_inventory(&inventory, cursor)?;
        for entry in &inventory.packages {
            if Instant::now() >= deadline {
                report.deferred += 1;
                return Ok(());
            }
            let historical = receiving_key(key, entry.key_version, root, transport)?;
            let envelope = transport.download_policy(&entry.package_id)?;
            ensure!(
                envelope.dictionary_id == entry.dictionary_id
                    && envelope.key_version == entry.key_version,
                "privacy policy inventory disagrees with its envelope"
            );
            // Any failed offered package stops receipt at this position, never skipping a gap.
            store.receive_policy(
                &entry.package_id,
                entry.sequence,
                cursor,
                &envelope,
                &historical,
                &dictionary,
            )?;
            cursor = entry.sequence;
            report.policy_received += 1;
        }
        if inventory.next_after.is_none() {
            store.db.execute(
                "UPDATE metadata SET value='1' WHERE key='policy_receipt_complete'",
                [],
            )?;
            return Ok(());
        }
    }
    report.deferred += 1;
    Ok(())
}

pub fn run(
    store: &mut Store,
    key: &UserKey,
    cache_root: &Path,
    transport: &impl Transport,
    budget: Duration,
) -> crate::Result<Report> {
    let started = Instant::now();
    let deadline = started + budget;
    let mut report = Report::default();
    store.encrypt_pending(key)?;
    store.seed_policy()?;
    let health_path = Health::path(cache_root, &key.owner.hub);
    let previous = Health::load(&health_path);
    let now = chrono::Utc::now().timestamp();
    let mut account_refused = previous
        .as_ref()
        .is_some_and(|health| health.retry_at > now && health.refusal == Refusal::Authorization);
    if previous
        .as_ref()
        .is_none_or(|health| health.retry_at <= now)
    {
        let mut refused = None;
        for package in store.outbox(100)? {
            if Instant::now() >= started + budget / 4 {
                report.deferred += 1;
                break;
            }
            match transport.upload(&package.id, &package.envelope) {
                Ok(receipt) if receipt.package_id == package.id && receipt.sequence > 0 => {
                    store.acknowledge(&package.id)?;
                    report.uploaded += 1;
                }
                Ok(_) => report.deferred += 1,
                Err(error) => {
                    report.deferred += 1;
                    if let Some(reason) = classify(&error) {
                        account_refused = reason.0 == Refusal::Authorization;
                        if account_refused || refused.is_none() {
                            refused = Some(reason);
                        }
                        if account_refused {
                            break;
                        }
                    }
                }
            }
        }
        let _ = Health::settle(&health_path, previous, refused, now);
    } else {
        report.deferred += 1;
    }
    // A refusal specific to dictionary packages must not pace removal-only policy packages.
    let policy_health = Health::path(&cache_root.join("policy-transport"), &key.owner.hub);
    let previous = Health::load(&policy_health);
    if !account_refused
        && previous
            .as_ref()
            .is_none_or(|health| health.retry_at <= now)
        && Instant::now() < started + budget / 2
    {
        let mut refused = None;
        let additions = store.outbox(1)?.is_empty();
        for package in store.policy_outbox(key, additions)? {
            if Instant::now() >= started + budget / 2 {
                report.deferred += 1;
                break;
            }
            match transport.upload_policy(&package.id, &package.envelope) {
                Ok(receipt) if receipt.package_id == package.id && receipt.sequence > 0 => {
                    store.acknowledge_policy(&package.id, receipt.sequence)?;
                    report.policy_uploaded += 1;
                }
                Ok(_) => report.deferred += 1,
                Err(error) => {
                    report.deferred += 1;
                    if let Some(reason) = classify(&error) {
                        let account = reason.0 == Refusal::Authorization;
                        if account || refused.is_none() {
                            refused = Some(reason);
                        }
                        if account {
                            break;
                        }
                    }
                }
            }
        }
        let _ = Health::settle(&policy_health, previous, refused, now);
    } else {
        report.deferred += 1;
    }
    // Each stream gets time even when the other stream fails or has an unbounded backlog.
    if dictionary_receive(
        store,
        key,
        cache_root,
        transport,
        started + budget * 3 / 4,
        &mut report,
    )
    .is_err()
    {
        report.deferred += 1;
    }
    if policy_receive(store, key, cache_root, transport, deadline, &mut report).is_err() {
        report.deferred += 1;
    }
    report.policy_unresolved = store.policy_progress()?.unresolved_rules;
    Ok(report)
}
