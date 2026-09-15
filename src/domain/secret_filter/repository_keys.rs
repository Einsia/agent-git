//! Repository mappings own their local key material independently of global registration.

use super::{FileKeyStore, KeyStore, SelectedKeyStore};
use anyhow::{Context, bail};
use std::path::PathBuf;
use std::sync::Arc;
use zeroize::Zeroizing;

const STORAGE_ID: &str = "repository-local-v1";

#[derive(Clone)]
pub struct RepositoryKeyStore {
    local: FileKeyStore,
    legacy: Arc<dyn KeyStore>,
}

struct ConfiguredLegacyKeys;

impl KeyStore for ConfiguredLegacyKeys {
    fn get(&self, vault_id: &str) -> crate::Result<Zeroizing<Vec<u8>>> {
        SelectedKeyStore::from_config()?.get(vault_id)
    }

    fn get_bounded(&self, vault_id: &str) -> crate::Result<Zeroizing<Vec<u8>>> {
        SelectedKeyStore::from_config()?.get_bounded(vault_id)
    }

    fn set(&self, _vault_id: &str, _key: &[u8]) -> crate::Result<()> {
        bail!("repository dictionaries cannot create global keystore entries")
    }

    fn delete(&self, _vault_id: &str) -> crate::Result<()> {
        bail!("repository migration does not remove global keystore entries")
    }
}

impl RepositoryKeyStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            local: FileKeyStore::new(dir),
            legacy: Arc::new(ConfiguredLegacyKeys),
        }
    }

    fn local_path(&self, vault_id: &str) -> crate::Result<PathBuf> {
        anyhow::ensure!(
            uuid::Uuid::parse_str(vault_id).is_ok() && vault_id.len() == 36,
            "the repository dictionary has an invalid vault identity"
        );
        Ok(self.local.dir().join(format!("{vault_id}.key")))
    }
}

impl KeyStore for RepositoryKeyStore {
    fn get(&self, vault_id: &str) -> crate::Result<Zeroizing<Vec<u8>>> {
        self.get_bounded(vault_id)
    }

    fn get_bounded(&self, vault_id: &str) -> crate::Result<Zeroizing<Vec<u8>>> {
        self.local_path(vault_id)?;
        #[cfg(unix)]
        return self.local.get_bounded(vault_id);
        #[cfg(not(unix))]
        local_files::read(&self.local_path(vault_id)?)
    }

    fn set(&self, vault_id: &str, key: &[u8]) -> crate::Result<()> {
        self.local_path(vault_id)?;
        #[cfg(unix)]
        return self.local.set(vault_id, key);
        #[cfg(not(unix))]
        local_files::write(&self.local_path(vault_id)?, key)
    }

    fn delete(&self, vault_id: &str) -> crate::Result<()> {
        let path = self.local_path(vault_id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("cannot remove {}", path.display())),
        }
    }

    fn storage_id(&self) -> Option<&'static str> {
        Some(STORAGE_ID)
    }

    fn get_for_storage(
        &self,
        vault_id: &str,
        storage: Option<&str>,
        bounded: bool,
    ) -> crate::Result<Zeroizing<Vec<u8>>> {
        self.local_path(vault_id)?;
        match storage {
            Some(STORAGE_ID) => self.get_bounded(vault_id),
            None if bounded => self.legacy.get_bounded(vault_id),
            None => self.legacy.get(vault_id),
            Some(_) => bail!("the repository dictionary uses an unsupported key store"),
        }
    }

    fn migrate_key(&self, vault_id: &str, key: &[u8]) -> crate::Result<()> {
        let path = self.local_path(vault_id)?;
        match std::fs::symlink_metadata(&path) {
            Ok(_) => anyhow::ensure!(
                self.get_bounded(vault_id)?.as_slice() == key,
                "the repository dictionary's local key conflicts with its existing key"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.set(vault_id, key)?
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

// Non-Unix repository files inherit the enclosing directory's ACL, like native transcripts.
// This does not enable the opt-in global file keystore on those platforms.
#[cfg(any(not(unix), test))]
mod local_files {
    use anyhow::Context;
    use base64::Engine;
    use std::io::Write;
    use std::path::Path;
    use zeroize::Zeroizing;

    pub(super) fn read(path: &Path) -> crate::Result<Zeroizing<Vec<u8>>> {
        use crate::adapter::native_snapshot::{Limits, read_file_bytes};
        let bytes = Zeroizing::new(read_file_bytes(
            path,
            Limits {
                bytes: 64,
                working_bytes: 65,
                ..Limits::default()
            },
        )?);
        let text = std::str::from_utf8(&bytes)?;
        let key = Zeroizing::new(base64::engine::general_purpose::STANDARD.decode(text.trim())?);
        super::super::validate_key(&key, "repository key")?;
        Ok(key)
    }

    pub(super) fn write(path: &Path, key: &[u8]) -> crate::Result<()> {
        super::super::validate_key(key, "repository key")?;
        let parent = path
            .parent()
            .context("the repository key has no parent directory")?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        let text = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(key));
        file.write_all(text.as_bytes())?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist_noclobber(path).map_err(|error| error.error)?;
        read(path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::secret_filter::{Matcher, RepositoryDictionary, VaultStore};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    struct LegacyKeys {
        values: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        reads: Arc<AtomicUsize>,
    }

    impl KeyStore for LegacyKeys {
        fn get(&self, id: &str) -> crate::Result<Zeroizing<Vec<u8>>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.values
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(Zeroizing::new)
                .context("legacy keystore is unavailable")
        }

        fn set(&self, id: &str, key: &[u8]) -> crate::Result<()> {
            self.values
                .lock()
                .unwrap()
                .insert(id.to_owned(), key.to_vec());
            Ok(())
        }

        fn delete(&self, id: &str) -> crate::Result<()> {
            self.values.lock().unwrap().remove(id);
            Ok(())
        }
    }

    fn keys(dir: &std::path::Path, legacy: &LegacyKeys) -> RepositoryKeyStore {
        RepositoryKeyStore {
            local: FileKeyStore::new(dir.join("keys")),
            legacy: Arc::new(legacy.clone()),
        }
    }

    const INPUT: &str = "{\"message\":\"fixture-registered-value\"}\n";

    #[test]
    fn repository_creation_and_reopening_never_access_the_global_keystore() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = LegacyKeys::default();
        let path = dir.path().join("vault.json");
        let dictionary = RepositoryDictionary::new(path.clone(), keys(dir.path(), &legacy));
        let protected = dictionary
            .protect_jsonl(
                INPUT,
                &Matcher::for_test(&[("registered", "fixture-registered-value")]),
            )
            .unwrap();
        assert!(!protected.text.contains("fixture-registered-value"));
        let reopened = RepositoryDictionary::new(path.clone(), keys(dir.path(), &legacy));
        assert_eq!(reopened.hydrate_jsonl(&protected.text).unwrap().text, INPUT);
        assert_eq!(legacy.reads.load(Ordering::SeqCst), 0);
        assert!(legacy.values.lock().unwrap().is_empty());
        let file = super::super::read_vault(&path).unwrap();
        assert_eq!(file.key_storage.as_deref(), Some(STORAGE_ID));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let key_path = keys(dir.path(), &legacy)
                .local_path(&file.vault_id)
                .unwrap();
            assert_eq!(
                std::fs::metadata(key_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn migration_preserves_placeholders_and_readonly_inspection_does_not_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let legacy = LegacyKeys::default();
        let original = RepositoryDictionary::new(path.clone(), legacy.clone());
        let protected = original
            .protect_jsonl(
                INPUT,
                &Matcher::for_test(&[("registered", "fixture-registered-value")]),
            )
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let local = RepositoryDictionary::new(path.clone(), keys(dir.path(), &legacy));
        assert_eq!(
            local
                .hydrate_pair_readonly(&protected.text, INPUT)
                .unwrap()
                .0
                .text,
            INPUT
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!dir.path().join("keys").exists());

        assert_eq!(
            local.protect_jsonl(INPUT, &Matcher::empty()).unwrap().text,
            protected.text
        );
        let after = super::super::read_vault(&path).unwrap();
        let mut expected: serde_json::Value = serde_json::from_slice(&before).unwrap();
        expected["key_storage"] = STORAGE_ID.into();
        assert_eq!(serde_json::to_value(&after).unwrap(), expected);
        let reads = legacy.reads.load(Ordering::SeqCst);
        legacy.values.lock().unwrap().clear();
        let reopened = RepositoryDictionary::new(path, keys(dir.path(), &legacy));
        assert_eq!(reopened.hydrate_jsonl(&protected.text).unwrap().text, INPUT);
        assert_eq!(legacy.reads.load(Ordering::SeqCst), reads);
    }

    #[test]
    fn missing_local_keys_never_reopen_the_global_keystore() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = LegacyKeys::default();
        let path = dir.path().join("vault.json");
        let dictionary = RepositoryDictionary::new(path.clone(), keys(dir.path(), &legacy));
        dictionary
            .protect_jsonl(
                INPUT,
                &Matcher::for_test(&[("registered", "fixture-registered-value")]),
            )
            .unwrap();
        let file = super::super::read_vault(&path).unwrap();
        let store = keys(dir.path(), &legacy);
        std::fs::remove_file(store.local_path(&file.vault_id).unwrap()).unwrap();
        assert!(dictionary.hydrate_jsonl(INPUT).is_err());
        assert_eq!(legacy.reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn migration_retries_an_installed_key_but_never_overwrites_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = LegacyKeys::default();
        let store = keys(dir.path(), &legacy);
        let id = uuid::Uuid::new_v4().to_string();
        store.migrate_key(&id, &[7; 32]).unwrap();
        store.migrate_key(&id, &[7; 32]).unwrap();
        assert!(store.migrate_key(&id, &[8; 32]).is_err());
        assert_eq!(store.get(&id).unwrap().as_slice(), &[7; 32]);
    }

    #[test]
    fn global_registration_keeps_its_selected_keystore() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = LegacyKeys::default();
        let path = dir.path().join("vault.json");
        let global = VaultStore::new(path.clone(), legacy.clone());
        global
            .add(
                "explicit",
                Zeroizing::new("fixture-registered-value".into()),
                false,
            )
            .unwrap();
        assert!(
            super::super::read_vault(&path)
                .unwrap()
                .key_storage
                .is_none()
        );
        assert_eq!(legacy.values.lock().unwrap().len(), 1);
        assert!(!dir.path().join("keys").exists());
        global.matcher().unwrap();
        assert!(legacy.reads.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn corrupt_dictionary_cannot_install_a_migration_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let legacy = LegacyKeys::default();
        RepositoryDictionary::new(path.clone(), legacy.clone())
            .protect_jsonl(
                INPUT,
                &Matcher::for_test(&[("registered", "fixture-registered-value")]),
            )
            .unwrap();
        let mut file = super::super::read_vault(&path).unwrap();
        file.records[0].sealed.ciphertext = "corrupt".into();
        super::super::write_vault(&path, &file).unwrap();
        let local = RepositoryDictionary::new(path, keys(dir.path(), &legacy));
        assert!(local.review().is_err());
        assert!(!dir.path().join("keys").exists());
    }

    #[test]
    fn portable_local_files_roundtrip_and_reject_replacement_or_oversized_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys/local.key");
        local_files::write(&path, &[7; 32]).unwrap();
        assert_eq!(local_files::read(&path).unwrap().as_slice(), &[7; 32]);
        assert!(local_files::write(&path, &[8; 32]).is_err());
        assert_eq!(local_files::read(&path).unwrap().as_slice(), &[7; 32]);
        std::fs::write(&path, vec![b'A'; 65]).unwrap();
        assert!(local_files::read(&path).is_err());
        #[cfg(unix)]
        {
            let link = dir.path().join("linked.key");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(local_files::read(&link).is_err());
        }
    }
}
