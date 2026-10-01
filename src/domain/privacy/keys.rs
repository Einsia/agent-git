//! Cloud keys are cached by immutable owner; authentication tokens are never encryption keys.

use super::crypto::{Owner, UserKey};
use super::storage::{read_private, write_private};
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, Zeroizing};

#[derive(Serialize, Deserialize)]
pub struct CloudKey {
    pub account_id: String,
    pub version: i64,
    pub key: String,
}

impl Drop for CloudKey {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

pub fn owner(hub: &str, account: &str) -> crate::Result<Owner> {
    ensure!(
        !account.is_empty() && account.len() <= 256,
        "invalid privacy account"
    );
    Ok(Owner {
        hub: crate::infra::hub_authority::HubAuthority::parse(hub)?.label(),
        account: account.to_owned(),
    })
}

pub fn root() -> crate::Result<PathBuf> {
    Ok(crate::infra::config::agit_home()?.join("privacy"))
}

pub fn owner_directory(root: &Path, owner: &Owner) -> PathBuf {
    let digest = Sha256::digest(serde_json::to_vec(owner).expect("owner contains only strings"));
    root.join("accounts").join(hex::encode(digest))
}

pub fn pending_root(root: &Path, hub: &str, username: Option<&str>) -> crate::Result<PathBuf> {
    let authority = crate::infra::hub_authority::HubAuthority::parse(hub)?;
    let digest = Sha256::digest(serde_json::to_vec(&(authority.label(), username))?);
    Ok(root.join("pending").join(hex::encode(digest)))
}

pub fn pending_directory(root: &Path, hub: &str, username: Option<&str>) -> crate::Result<PathBuf> {
    let mut directory = pending_root(root, hub, username)?;
    for _ in 0..64 {
        if super::storage::Store::binding(&directory)?.is_none() {
            return Ok(directory);
        }
        directory = directory.join("next");
    }
    anyhow::bail!("pending privacy storage exceeds its generation budget")
}

pub fn pending_generations(
    root: &Path,
    hub: &str,
    username: Option<&str>,
) -> crate::Result<Vec<PathBuf>> {
    let mut directory = pending_root(root, hub, username)?;
    let mut paths = Vec::new();
    for _ in 0..64 {
        if !directory.join("journal.sqlite").exists() {
            break;
        }
        paths.push(directory.clone());
        directory = directory.join("next");
    }
    Ok(paths)
}

pub fn cache(root: &Path, key: &UserKey) -> crate::Result<()> {
    let document = CloudKey {
        account_id: key.owner.account.clone(),
        version: key.version,
        key: key.encoded().to_string(),
    };
    let bytes = Zeroizing::new(serde_json::to_vec(&document)?);
    let directory = owner_directory(root, &key.owner).join("keys");
    let path = directory.join(format!("{}.json", key.version));
    if path.try_exists()? {
        let existing = read_private(&path, 2048)?;
        ensure!(
            existing.as_slice() == bytes.as_slice(),
            "privacy key version cannot be replaced"
        );
    } else {
        write_private(&path, &bytes)?;
    }
    let latest = read_private(&directory.join("current"), 32)
        .ok()
        .and_then(|bytes| std::str::from_utf8(&bytes).ok()?.parse::<i64>().ok())
        .unwrap_or(0);
    write_private(
        &directory.join("current"),
        key.version.max(latest).to_string().as_bytes(),
    )
}

pub fn cached(root: &Path, owner: &Owner, version: Option<i64>) -> crate::Result<UserKey> {
    let directory = owner_directory(root, owner).join("keys");
    let version = match version {
        Some(version) => version,
        None => std::str::from_utf8(&read_private(&directory.join("current"), 32)?)?.parse()?,
    };
    ensure!(version > 0, "invalid privacy key version");
    let bytes = read_private(&directory.join(format!("{version}.json")), 2048)?;
    let document: CloudKey = serde_json::from_slice(&bytes)?;
    ensure!(
        document.account_id == owner.account && document.version == version,
        "privacy key belongs to another owner or version"
    );
    UserKey::new(
        owner.clone(),
        version,
        STANDARD
            .decode(&document.key)
            .context("invalid cached privacy key")?,
    )
}

pub fn decode(hub: &str, document: &CloudKey) -> crate::Result<UserKey> {
    UserKey::new(
        owner(hub, &document.account_id)?,
        document.version,
        STANDARD
            .decode(&document.key)
            .context("invalid cloud privacy key")?,
    )
}
