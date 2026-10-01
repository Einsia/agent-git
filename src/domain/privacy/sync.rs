//! Dictionary synchronization is independently retryable and carries only encrypted envelopes.

use super::crypto::{Envelope, UserKey};
use super::keys::{self, CloudKey};
use super::storage::Store;
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::path::Path;
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
}

#[derive(Default, Serialize, Deserialize)]
pub struct Report {
    pub uploaded: usize,
    pub downloaded: usize,
    pub deferred: usize,
}

pub fn run(
    store: &mut Store,
    key: &UserKey,
    cache_root: &Path,
    transport: &impl Transport,
    budget: Duration,
) -> crate::Result<Report> {
    let deadline = Instant::now() + budget;
    let mut report = Report::default();
    store.encrypt_pending(key)?;
    for package in store.outbox(100)? {
        if Instant::now() >= deadline {
            report.deferred += 1;
            break;
        }
        match transport.upload(&package.id, &package.envelope) {
            Ok(receipt) if receipt.package_id == package.id && receipt.sequence > 0 => {
                store.acknowledge(&package.id)?;
                report.uploaded += 1;
            }
            _ => report.deferred += 1,
        }
    }
    let receive = |store: &mut Store, id: &str, version: i64| -> crate::Result<()> {
        let historical = if version == key.version {
            None
        } else {
            Some(
                keys::cached(cache_root, &key.owner, Some(version)).or_else(|_| {
                    let document = transport.key(version)?;
                    let historical = UserKey::new(key.owner.clone(), document.version, {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD.decode(&document.key)?
                    })?;
                    ensure!(
                        document.account_id == key.owner.account && historical.version == version,
                        "cloud privacy key identity changed"
                    );
                    let _ = keys::cache(cache_root, &historical);
                    Ok::<_, anyhow::Error>(historical)
                })?,
            )
        };
        let envelope = transport.download(id)?;
        ensure!(
            envelope.key_version == version,
            "privacy inventory disagrees with its envelope"
        );
        store.import(id, &envelope, historical.as_ref().unwrap_or(key))
    };
    for (id, version) in store.retries()? {
        if Instant::now() >= deadline {
            report.deferred += 1;
            return Ok(report);
        }
        if receive(store, &id, version).is_ok() {
            store.clear_retry(&id)?;
            report.downloaded += 1;
        } else {
            report.deferred += 1;
        }
    }
    let mut cursor = store.inventory_cursor()?;
    while Instant::now() < deadline {
        let inventory = transport.inventory(cursor)?;
        ensure!(
            inventory.packages.len() <= 100,
            "privacy inventory exceeds its item budget"
        );
        for entry in &inventory.packages {
            if Instant::now() >= deadline {
                report.deferred += 1;
                return Ok(report);
            }
            ensure!(entry.sequence > cursor, "privacy inventory is not ordered");
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
            break;
        }
        ensure!(
            !inventory.packages.is_empty() && inventory.next_after == Some(cursor),
            "invalid privacy inventory cursor"
        );
    }
    Ok(report)
}
