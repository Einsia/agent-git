//! Authenticated records may be reused only for identical key, ciphertext, and associated data.
use super::{DecryptedRecord, VaultFile};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
};

const CACHE_BYTES: usize = 64 * 1024 * 1024;
const CACHE_ENTRIES: usize = 4;
struct Entry {
    key: [u8; 32],
    records: Arc<Vec<DecryptedRecord>>,
    bytes: usize,
}
static CACHE: OnceLock<Mutex<VecDeque<Entry>>> = OnceLock::new();

pub(super) fn read(
    file: &VaultFile,
    dek: &[u8],
    authenticate: impl FnOnce() -> crate::Result<Vec<DecryptedRecord>>,
) -> crate::Result<Vec<DecryptedRecord>> {
    read_in(file, dek, authenticate, CACHE.get_or_init(Default::default))
}

fn read_in(
    file: &VaultFile,
    dek: &[u8],
    authenticate: impl FnOnce() -> crate::Result<Vec<DecryptedRecord>>,
    cache: &Mutex<VecDeque<Entry>>,
) -> crate::Result<Vec<DecryptedRecord>> {
    let mut digest = blake3::Hasher::new();
    digest.update(b"agit-authenticated-records-blake3-v1");
    let mut field = |bytes: &[u8]| {
        digest.update(&(bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    };
    // Callers unwrap the current key before this boundary; a missing or revoked key still fails.
    field(dek);
    field(file.vault_id.as_bytes());
    for record in &file.records {
        field(record.id.as_bytes());
        field(&record.version.to_le_bytes());
        field(record.sealed.nonce.as_bytes());
        field(record.sealed.ciphertext.as_bytes());
    }
    let key: [u8; 32] = digest.finalize().into();
    let cached = {
        let mut entries = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries
            .iter()
            .position(|entry| entry.key == key)
            .map(|index| {
                let entry = entries.remove(index).expect("the matched entry exists");
                let records = entry.records.clone();
                entries.push_back(entry);
                records
            })
    };
    if let Some(records) = cached {
        return Ok((*records).clone());
    }
    let records = authenticate()?;
    let bytes = records
        .iter()
        .map(|record| {
            std::mem::size_of::<DecryptedRecord>()
                + record.id.capacity()
                + record.name.capacity()
                + record.secret.capacity()
                + record.origins.capacity() * std::mem::size_of::<super::RecordOrigin>()
                + record.created_at.capacity()
                + record.updated_at.capacity()
                + record.declaration.as_ref().map_or(0, |value| {
                    serde_json::to_vec(value).map_or(CACHE_BYTES, |bytes| bytes.len())
                })
        })
        .sum::<usize>();
    if bytes <= CACHE_BYTES {
        let cached = Arc::new(records.clone());
        let mut entries = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|entry| entry.key != key);
        while entries.len() >= CACHE_ENTRIES
            || entries.iter().map(|entry| entry.bytes).sum::<usize>() + bytes > CACHE_BYTES
        {
            entries.pop_front();
        }
        entries.push_back(Entry {
            key,
            records: cached,
            bytes,
        });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use crate::domain::secret_filter::{RepositoryKeyStore, VaultStore};
    use zeroize::Zeroizing;

    #[test]
    fn warm_records_still_authenticate_changed_ciphertext_and_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::new(
            dir.path().join("vault.json"),
            RepositoryKeyStore::new(dir.path().join("keys")),
        );
        store
            .add(
                "test",
                Zeroizing::new("warm-cache-passphrase".into()),
                false,
            )
            .unwrap();
        let unlocked = store.unlock_existing().unwrap();
        let cache = std::sync::Mutex::default();
        let decode = |file: &super::VaultFile, dek: &[u8]| {
            super::read_in(
                file,
                dek,
                || super::super::decrypt_records_uncached(file, dek),
                &cache,
            )
        };
        let first = decode(&unlocked.file, &unlocked.dek).unwrap();
        let cached = decode(&unlocked.file, &unlocked.dek).unwrap();
        assert_eq!(first[0].secret.as_str(), cached[0].secret.as_str());
        super::read_in(
            &unlocked.file,
            &unlocked.dek,
            || panic!("identical authenticated records must use the cached snapshot"),
            &cache,
        )
        .unwrap();
        let mut changed = unlocked.file.clone();
        changed.records[0].sealed.ciphertext.push('A');
        assert!(decode(&changed, &unlocked.dek).is_err());
        assert!(decode(&unlocked.file, &[0; 32]).is_err());
        changed = unlocked.file.clone();
        changed.records[0].id.push('x');
        assert!(decode(&changed, &unlocked.dek).is_err());
        changed = unlocked.file.clone();
        changed.records[0].sealed.nonce.push('A');
        assert!(decode(&changed, &unlocked.dek).is_err());
        changed = unlocked.file.clone();
        changed.records[0].version += 1;
        assert!(decode(&changed, &unlocked.dek).is_err());
        changed.records.clear();
        assert!(decode(&changed, &unlocked.dek).unwrap().is_empty());
        use crate::domain::secret_filter::KeyStore;
        store.keys.delete(&unlocked.file.vault_id).unwrap();
        assert!(store.matcher().is_err());
    }
}
