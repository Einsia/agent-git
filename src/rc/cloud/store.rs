//! Private endpoint credentials and executor policy stay in the daemon namespace.

use agit_peer::{
    Identity,
    access::{Access, Policy, Resource, Rule},
    client::Client,
    cloud::DeviceCredential,
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::{
    fs::OpenOptions,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
};

const MAX_RECORD: u64 = 128 * 1024;
const POLICY_FILE: &str = "cloud-access.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u32,
    credential: DeviceCredential,
    private_key: Vec<u8>,
    #[serde(default)]
    inbound_enabled: Option<bool>,
}

pub struct Enrollment {
    pub credential: DeviceCredential,
    pub identity: Identity,
    pub inbound_enabled: bool,
}

fn filename(hub: &str) -> crate::Result<String> {
    let client = Client::new(hub)?;
    Ok(format!(
        "cloud-device-{}.json",
        hex::encode(Sha256::digest(client.origin().as_bytes()))
    ))
}

pub fn enrollment_lock(hub: &str) -> crate::Result<std::fs::File> {
    let path = super::super::rc_dir()?
        .join(filename(hub)?)
        .with_extension("lock");
    let file = private_lock(&path)?;
    fs2::FileExt::try_lock_exclusive(&file).context("cloud enrollment is being updated")?;
    Ok(file)
}

fn private_lock(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        crate::infra::windows_security::open_private_control(path)
    }
    #[cfg(unix)]
    {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
}

fn read<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> crate::Result<Option<T>> {
    #[cfg(unix)]
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path);
    #[cfg(windows)]
    let opened = crate::infra::windows_security::open_private_read(path);
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot open private cloud state"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit,
        "cloud state must be a bounded private file owned by this user"
    );
    #[cfg(unix)]
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "cloud state must be a private file owned by this user"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "cloud state exceeds its size limit"
    );
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("invalid private cloud state"))
}

fn write(path: &Path, value: &impl Serialize) -> crate::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    #[cfg(windows)]
    crate::infra::windows_security::write_private_file(path, &bytes)?;
    #[cfg(unix)]
    {
        let directory = path.parent().context("cloud state directory is missing")?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        std::fs::File::open(directory)?.sync_all()?;
    }
    Ok(())
}

pub fn load(hub: &str) -> crate::Result<Option<Enrollment>> {
    load_in(&super::super::rc_dir()?, hub)
}

fn load_in(directory: &Path, hub: &str) -> crate::Result<Option<Enrollment>> {
    let Some(stored) = read::<Stored>(&directory.join(filename(hub)?), MAX_RECORD)? else {
        return Ok(None);
    };
    ensure!(
        matches!(stored.version, 1 | 2)
            && (stored.version == 1 || stored.inbound_enabled.is_some())
            && stored.credential.device.owner.issuer == Client::new(hub)?.origin(),
        "cloud enrollment origin or version mismatch"
    );
    let identity = Identity::from_der(
        stored.credential.device.certificate.as_der().to_vec(),
        stored.private_key,
    )?;
    Ok(Some(Enrollment {
        credential: stored.credential,
        identity,
        inbound_enabled: stored.inbound_enabled.unwrap_or(true),
    }))
}

pub fn save(enrollment: &Enrollment) -> crate::Result<()> {
    save_in(&super::super::rc_dir()?, enrollment)
}

fn save_in(directory: &Path, enrollment: &Enrollment) -> crate::Result<()> {
    ensure!(
        enrollment.credential.device.certificate == *enrollment.identity.certificate(),
        "cloud enrollment certificate does not match its identity"
    );
    write(
        &directory.join(filename(&enrollment.credential.device.owner.issuer)?),
        &Stored {
            version: 2,
            credential: enrollment.credential.clone(),
            private_key: enrollment.identity.private_key_der().to_vec(),
            inbound_enabled: Some(enrollment.inbound_enabled),
        },
    )
}

fn intent_path(hub: &str) -> crate::Result<PathBuf> {
    Ok(super::super::rc_dir()?.join(filename(hub)?.replace("cloud-device-", "cloud-inbound-")))
}

pub(super) fn verify_signed_in_owner(owner: &agit_peer::access::Principal) -> crate::Result<()> {
    let credential = crate::infra::credentials::load_checked(&owner.issuer)?
        .context("Sign in to the device owner's account before enabling Cloud access")?;
    let account = match credential.account_id {
        Some(ref account) => account.clone(),
        None => crate::hub::Client::for_credential(&owner.issuer, &credential)
            .me()?
            .account_id
            .context("Cloud did not return an account identity")?,
    };
    ensure!(
        account == owner.account_id,
        "This daemon is enrolled to another account. Sign in as its owner or use a separate AGIT_HOME for the new account"
    );
    Ok(())
}

pub fn request_inbound(hub: &str) -> crate::Result<()> {
    let api = Client::new(hub)?;
    let _lock = enrollment_lock(api.origin())?;
    if let Some(mut enrollment) = load(api.origin())? {
        verify_signed_in_owner(&enrollment.credential.device.owner)?;
        grant_enrolling_owner(&enrollment)?;
        enrollment.inbound_enabled = true;
        save(&enrollment)?;
    }
    write(&intent_path(api.origin())?, &api.origin())
}

pub(super) fn inbound_pending(hub: &str) -> crate::Result<bool> {
    Ok(read::<String>(&intent_path(hub)?, MAX_RECORD)?.is_some())
}

pub(super) fn clear_inbound_request(hub: &str) -> crate::Result<()> {
    match std::fs::remove_file(intent_path(hub)?) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

pub fn origins() -> crate::Result<Vec<String>> {
    let directory = super::super::rc_dir()?;
    let mut origins = origins_in(&directory)?;
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("cloud-inbound-") && name.ends_with(".json") {
            let hub: String =
                read(&entry.path(), MAX_RECORD)?.context("inbound request disappeared")?;
            ensure!(
                entry.path() == intent_path(&hub)?,
                "inbound request origin mismatch"
            );
            if !origins.contains(&hub) {
                origins.push(hub);
            }
            ensure!(origins.len() <= 64, "too many cloud origins");
        }
    }
    Ok(origins)
}

fn origins_in(directory: &Path) -> crate::Result<Vec<String>> {
    let mut origins = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("cloud-device-") || !name.ends_with(".json") {
            continue;
        }
        ensure!(origins.len() < 64, "too many cloud enrollments");
        let stored: Stored =
            read(&entry.path(), MAX_RECORD)?.context("cloud enrollment disappeared")?;
        ensure!(
            name == filename(&stored.credential.device.owner.issuer)?,
            "cloud enrollment filename does not match its origin"
        );
        let hub = stored.credential.device.owner.issuer;
        if load_in(directory, &hub)?.is_some_and(|enrollment| enrollment.inbound_enabled) {
            origins.push(hub);
        }
    }
    Ok(origins)
}

pub fn status(hub: &str) -> crate::Result<serde_json::Value> {
    let enrollment = load(hub)?;
    Ok(serde_json::json!({
        "enrollment_pending": inbound_pending(hub)?,
        "inbound_enabled": enrollment.as_ref().is_some_and(|value| value.inbound_enabled),
        "device": enrollment.map(|value| value.credential.device),
    }))
}

pub fn policy() -> crate::Result<Policy> {
    Ok(read(&super::super::rc_dir()?.join(POLICY_FILE), 4 * 1024 * 1024)?.unwrap_or_default())
}

pub fn save_policy(policy: &Policy) -> crate::Result<()> {
    policy.validate()?;
    write(&super::super::rc_dir()?.join(POLICY_FILE), policy)
}

fn policy_lock() -> crate::Result<std::fs::File> {
    let file = private_lock(&super::super::rc_dir()?.join("cloud-access.lock"))?;
    fs2::FileExt::try_lock_exclusive(&file).context("cloud resource policy is being updated")?;
    Ok(file)
}

pub fn grant(rule: Rule) -> crate::Result<Policy> {
    let _lock = policy_lock()?;
    grant_unlocked(rule)
}

fn grant_unlocked(rule: Rule) -> crate::Result<Policy> {
    let old = policy()?;
    let mut rules = old.rules().to_vec();
    rules.retain(|existing| {
        existing.principal != rule.principal || existing.resource != rule.resource
    });
    rules.push(rule);
    let policy = Policy::new(
        old.revision()
            .checked_add(1)
            .context("cloud policy revision exhausted")?,
        rules,
    )?;
    save_policy(&policy)?;
    Ok(policy)
}

pub fn grant_enrolling_owner(enrollment: &Enrollment) -> crate::Result<()> {
    let _lock = policy_lock()?;
    let owner = &enrollment.credential.device.owner;
    let current = policy()?;
    if !current
        .rules()
        .iter()
        .any(|rule| &rule.principal == owner && rule.resource == Resource::Machine)
    {
        grant_unlocked(Rule {
            principal: owner.clone(),
            resource: Resource::Machine,
            access: Access::Admin,
        })?;
    }
    Ok(())
}

pub fn directory() -> crate::Result<PathBuf> {
    super::super::rc_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agit_peer::{
        access::Principal,
        cloud::{Device, Secret},
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn cloud_identity_roundtrips_privately_and_rejects_exposed_or_substituted_records() {
        let directory = tempfile::tempdir().unwrap();
        let identity = Identity::generate().unwrap();
        let hub = "https://cloud.example";
        let enrollment = Enrollment {
            credential: DeviceCredential {
                device: Device {
                    id: "device".into(),
                    owner: Principal {
                        issuer: hub.into(),
                        account_id: "account".into(),
                    },
                    machine_id: "machine".into(),
                    display_name: "machine".into(),
                    certificate: identity.certificate().clone(),
                    credential_epoch: 1,
                },
                token: Secret::new(uuid::Uuid::new_v4().to_string()),
            },
            identity,
            inbound_enabled: false,
        };
        save_in(directory.path(), &enrollment).unwrap();
        let loaded = load_in(directory.path(), hub).unwrap().unwrap();
        assert!(!loaded.inbound_enabled);
        assert!(origins_in(directory.path()).unwrap().is_empty());
        assert_eq!(
            loaded.identity.certificate(),
            enrollment.identity.certificate()
        );
        assert_eq!(
            loaded.credential.token.expose(),
            enrollment.credential.token.expose()
        );
        let path = directory.path().join(filename(hub).unwrap());
        let mut enabled = loaded;
        enabled.inbound_enabled = true;
        save_in(directory.path(), &enabled).unwrap();
        assert_eq!(origins_in(directory.path()).unwrap(), vec![hub]);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        legacy["version"] = serde_json::json!(1);
        legacy.as_object_mut().unwrap().remove("inbound_enabled");
        write(&path, &legacy).unwrap();
        assert!(
            load_in(directory.path(), hub)
                .unwrap()
                .unwrap()
                .inbound_enabled
        );
        #[cfg(unix)]
        {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load_in(directory.path(), hub).is_err());
            std::fs::remove_file(&path).unwrap();
            let target = directory.path().join("another-file");
            std::fs::write(&target, "{}").unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert!(load_in(directory.path(), hub).is_err());
        }
    }
}
