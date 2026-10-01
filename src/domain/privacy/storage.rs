//! A private transactional journal acknowledges mappings only after durable commit.

use super::crypto::{self, Envelope, Owner, UserKey};
use super::dictionary::{Dictionary, Fragment, Record, token_identity};
use anyhow::{Context, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use zeroize::Zeroizing;

const MAX_STORE_BYTES: i64 = 512 * 1024 * 1024;
const MAX_LOAD_BYTES: usize = 128 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Contents {
    format: u32,
    records: Vec<Fragment>,
}

pub struct StoredPackage {
    pub id: String,
    pub envelope: Envelope,
}

pub struct Store {
    pub(super) db: Connection,
    pub dictionary_id: String,
    owner: Option<Owner>,
}

pub fn private_directory(path: &Path) -> crate::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && !parent.exists()
    {
        private_directory(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
            "privacy directory is not owned by this user"
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    crate::infra::windows_security::private_directory(path)?;
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!("private privacy storage is unavailable on this platform");
    Ok(())
}

pub fn write_private(path: &Path, bytes: &[u8]) -> crate::Result<()> {
    let parent = path.parent().context("privacy file needs a directory")?;
    private_directory(parent)?;
    #[cfg(windows)]
    crate::infra::windows_security::write_private_file(path, bytes)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|e| e.error)?;
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub fn read_private(path: &Path, limit: usize) -> crate::Result<Zeroizing<Vec<u8>>> {
    use std::io::Read;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "privacy file is not private"
        );
        file
    };
    #[cfg(windows)]
    let file = {
        crate::infra::windows_security::validate_path(path, false, true)?;
        std::fs::File::open(path)?
    };
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "privacy file exceeds its byte budget");
    Ok(bytes)
}

impl Store {
    pub(crate) fn empty() -> crate::Result<Self> {
        Ok(Self {
            db: Connection::open_in_memory()?,
            dictionary_id: uuid::Uuid::nil().to_string(),
            owner: None,
        })
    }

    pub(crate) fn read_only(directory: &Path, owner: Option<Owner>) -> crate::Result<Self> {
        let path = std::fs::canonicalize(directory)?.join("journal.sqlite");
        ensure!(
            std::fs::symlink_metadata(&path)?.is_file(),
            "privacy journal is not a regular file"
        );
        let db = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.busy_timeout(std::time::Duration::ZERO)?;
        let saved: Option<String> = db
            .query_row("SELECT value FROM metadata WHERE key='owner'", [], |row| {
                row.get(0)
            })
            .optional()?;
        let saved: Option<Owner> = saved.as_deref().map(serde_json::from_str).transpose()?;
        ensure!(
            saved.is_none() || saved == owner,
            "privacy journal belongs to another account"
        );
        let dictionary_id = db.query_row(
            "SELECT value FROM metadata WHERE key='dictionary'",
            [],
            |row| row.get(0),
        )?;
        Ok(Self {
            db,
            dictionary_id,
            owner: saved,
        })
    }

    pub fn binding(directory: &Path) -> crate::Result<Option<Owner>> {
        if !directory.join("journal.sqlite").exists() {
            return Ok(None);
        }
        let db = Connection::open_with_flags(
            std::fs::canonicalize(directory)?.join("journal.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.busy_timeout(std::time::Duration::ZERO)?;
        let value: Option<String> = db
            .query_row("SELECT value FROM metadata WHERE key='owner'", [], |row| {
                row.get(0)
            })
            .optional()?;
        value
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(Into::into)
    }

    pub fn open(directory: &Path, owner: Option<Owner>) -> crate::Result<Self> {
        private_directory(directory)?;
        let path = std::fs::canonicalize(directory)?.join("journal.sqlite");
        if !path.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            #[cfg(windows)]
            if !path.exists() {
                crate::infra::windows_security::write_private_file(&path, &[])?;
            }
        }
        ensure!(
            std::fs::symlink_metadata(&path)?.is_file(),
            "privacy journal is not a regular file"
        );
        let db = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.busy_timeout(std::time::Duration::ZERO)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;
            CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS packages (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
                dictionary_id TEXT NOT NULL, encrypted INTEGER NOT NULL,
                payload BLOB NOT NULL, uploaded INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS retry (id TEXT PRIMARY KEY, version INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS policy (
                scope TEXT NOT NULL, token TEXT NOT NULL, name TEXT NOT NULL,
                block INTEGER, allow INTEGER, PRIMARY KEY(scope, token));
            INSERT OR IGNORE INTO metadata VALUES ('policy_generation', '0');
            INSERT OR IGNORE INTO metadata VALUES ('bytes', '0');
            CREATE TRIGGER IF NOT EXISTS packages_insert AFTER INSERT ON packages BEGIN
                UPDATE metadata SET value=CAST(value AS INTEGER)+length(NEW.payload) WHERE key='bytes'; END;
            CREATE TRIGGER IF NOT EXISTS packages_update AFTER UPDATE OF payload ON packages BEGIN
                UPDATE metadata SET value=CAST(value AS INTEGER)+length(NEW.payload)-length(OLD.payload) WHERE key='bytes'; END;",
        )?;
        let new_id = uuid::Uuid::new_v4().to_string();
        db.execute(
            "INSERT OR IGNORE INTO metadata VALUES ('dictionary', ?1)",
            [&new_id],
        )?;
        let dictionary_id: String = db.query_row(
            "SELECT value FROM metadata WHERE key='dictionary'",
            [],
            |r| r.get(0),
        )?;
        uuid::Uuid::parse_str(&dictionary_id).context("invalid stored dictionary identity")?;
        let saved_owner: Option<String> = db
            .query_row("SELECT value FROM metadata WHERE key='owner'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let saved_owner: Option<Owner> = saved_owner
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        ensure!(
            saved_owner.is_none() || saved_owner == owner,
            "privacy journal belongs to another account"
        );
        let mut store = Self {
            db,
            dictionary_id,
            owner: saved_owner,
        };
        if let Some(owner) = owner {
            store.bind(&owner)?;
        }
        Ok(store)
    }

    /// Ownership can be assigned once; an account switch cannot retarget pending originals.
    pub fn bind(&mut self, owner: &Owner) -> crate::Result<()> {
        if let Some(current) = &self.owner {
            ensure!(
                current == owner,
                "privacy journal belongs to another account"
            );
            return Ok(());
        }
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let encoded = serde_json::to_string(owner)?;
        transaction.execute(
            "INSERT OR IGNORE INTO metadata VALUES ('owner', ?1)",
            [&encoded],
        )?;
        let stored: String =
            transaction.query_row("SELECT value FROM metadata WHERE key='owner'", [], |r| {
                r.get(0)
            })?;
        ensure!(
            stored == encoded,
            "privacy journal belongs to another account"
        );
        transaction.commit()?;
        self.owner = Some(owner.clone());
        Ok(())
    }

    pub fn owner(&self) -> Option<&Owner> {
        self.owner.as_ref()
    }

    pub fn append(&mut self, records: &[Record], key: Option<&UserKey>) -> crate::Result<()> {
        if let Some(key) = key {
            self.bind(&key.owner)?;
        }
        let mut packages = Vec::new();
        let mut batch = Contents {
            format: 1,
            records: vec![],
        };
        let mut bytes = 0;
        for record in records {
            for fragment in record.fragments() {
                let size = Zeroizing::new(serde_json::to_vec(&fragment)?).len();
                if bytes + size + 128 > crypto::MAX_PLAINTEXT_BYTES && !batch.records.is_empty() {
                    packages.push(std::mem::replace(
                        &mut batch,
                        Contents {
                            format: 1,
                            records: vec![],
                        },
                    ));
                    bytes = 0;
                }
                bytes += size + 1;
                batch.records.push(fragment);
            }
        }
        if !batch.records.is_empty() {
            packages.push(batch);
        }
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut stored: i64 = tx.query_row(
            "SELECT CAST(value AS INTEGER) FROM metadata WHERE key='bytes'",
            [],
            |r| r.get(0),
        )?;
        for contents in packages {
            let id = uuid::Uuid::new_v4().to_string();
            let plaintext = Zeroizing::new(serde_json::to_vec(&contents)?);
            let payload = if let Some(key) = key {
                Zeroizing::new(serde_json::to_vec(&crypto::seal(
                    key,
                    &self.dictionary_id,
                    &id,
                    &plaintext,
                )?)?)
            } else {
                plaintext
            };
            stored += payload.len() as i64;
            ensure!(stored <= MAX_STORE_BYTES, "privacy journal is full");
            tx.execute(
                "INSERT INTO packages (id,dictionary_id,encrypted,payload) VALUES (?1,?2,?3,?4)",
                params![id, self.dictionary_id, key.is_some(), payload.as_slice()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Loading is incremental in a resident worker; unreadable packages do not hide valid ones.
    pub fn load_since(
        &self,
        dictionary: &mut Dictionary,
        after: i64,
        mut key: impl FnMut(i64) -> crate::Result<UserKey>,
    ) -> crate::Result<(i64, bool)> {
        let mut query = self.db.prepare("SELECT sequence,id,encrypted,payload FROM packages WHERE sequence>?1 ORDER BY sequence")?;
        let mut rows = query.query([after])?;
        let mut cursor = after;
        let mut complete = true;
        let mut loaded = 0;
        while let Some(row) = rows.next()? {
            let sequence: i64 = row.get(0)?;
            let id: String = row.get(1)?;
            let encrypted: bool = row.get(2)?;
            let payload = Zeroizing::new(row.get::<_, Vec<u8>>(3)?);
            loaded += payload.len();
            if loaded > MAX_LOAD_BYTES {
                return Ok((cursor, false));
            }
            let result = (|| -> crate::Result<()> {
                let plaintext = if encrypted {
                    let envelope: Envelope = serde_json::from_slice(&payload)?;
                    let key = key(envelope.key_version)?;
                    ensure!(
                        self.owner.as_ref() == Some(&key.owner),
                        "privacy key belongs to another account"
                    );
                    crypto::open(&key, &id, &envelope)?
                } else {
                    payload
                };
                let contents: Contents = serde_json::from_slice(&plaintext)?;
                ensure!(contents.format == 1, "unsupported privacy contents");
                for fragment in contents.records {
                    dictionary.accept_fragment(fragment)?;
                }
                Ok(())
            })();
            complete &= result.is_ok();
            // Failed packages are revisited after worker reload or key refresh.
            if complete {
                cursor = sequence;
            }
        }
        Ok((cursor, complete))
    }

    pub fn encrypt_pending(&mut self, key: &UserKey) -> crate::Result<()> {
        self.bind(&key.owner)?;
        loop {
            let pending = self
                .db
                .query_row(
                    "SELECT id,dictionary_id,payload FROM packages WHERE encrypted=0 LIMIT 1",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Vec<u8>>(2)?,
                        ))
                    },
                )
                .optional()?;
            let Some((id, dictionary, plaintext)) = pending else {
                break;
            };
            let plaintext = Zeroizing::new(plaintext);
            let encrypted = serde_json::to_vec(&crypto::seal(key, &dictionary, &id, &plaintext)?)?;
            let tx = self
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let size: i64 = tx.query_row(
                "SELECT CAST(value AS INTEGER) FROM metadata WHERE key='bytes'",
                [],
                |row| row.get(0),
            )?;
            ensure!(
                size + encrypted.len() as i64 - plaintext.len() as i64 <= MAX_STORE_BYTES,
                "privacy journal is full"
            );
            tx.execute(
                "UPDATE packages SET payload=?2,encrypted=1 WHERE id=?1 AND encrypted=0",
                params![id, encrypted],
            )?;
            tx.commit()?;
        }
        // Checkpointing removes obsolete plaintext pages from the live WAL when readers permit it.
        self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }

    pub fn outbox(&self, limit: usize) -> crate::Result<Vec<StoredPackage>> {
        let mut query = self.db.prepare("SELECT id,payload FROM packages WHERE encrypted=1 AND uploaded=0 ORDER BY sequence LIMIT ?1")?;
        let rows = query.query_map([limit.min(100) as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        rows.map(|row| {
            let (id, bytes) = row?;
            Ok(StoredPackage {
                id,
                envelope: serde_json::from_slice(&bytes)?,
            })
        })
        .collect()
    }

    pub fn acknowledge(&self, id: &str) -> crate::Result<()> {
        self.db.execute(
            "UPDATE packages SET uploaded=1 WHERE id=?1 AND encrypted=1",
            [id],
        )?;
        Ok(())
    }

    /// Remote bytes enter the journal only after authentication and content validation.
    pub fn import(&mut self, id: &str, envelope: &Envelope, key: &UserKey) -> crate::Result<()> {
        self.bind(&key.owner)?;
        let plaintext = crypto::open(key, id, envelope)?;
        let contents: Contents = serde_json::from_slice(&plaintext)?;
        ensure!(
            contents.format == 1 && !contents.records.is_empty(),
            "invalid privacy package contents"
        );
        let mut validation = Dictionary::default();
        for fragment in contents.records {
            ensure!(
                token_identity(&fragment.token).is_some(),
                "invalid privacy token"
            );
            validation.accept_fragment(fragment)?;
        }
        let payload = serde_json::to_vec(envelope)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<Vec<u8>> = tx
            .query_row("SELECT payload FROM packages WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(existing) = existing {
            ensure!(existing == payload, "conflicting privacy package");
        } else {
            let size: i64 = tx.query_row(
                "SELECT CAST(value AS INTEGER) FROM metadata WHERE key='bytes'",
                [],
                |r| r.get(0),
            )?;
            ensure!(
                size + payload.len() as i64 <= MAX_STORE_BYTES,
                "privacy journal is full"
            );
            tx.execute("INSERT INTO packages (id,dictionary_id,encrypted,payload,uploaded) VALUES (?1,?2,1,?3,1)", params![id,envelope.dictionary_id,payload])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn inventory_cursor(&self) -> crate::Result<i64> {
        let value: Option<String> = self
            .db
            .query_row("SELECT value FROM metadata WHERE key='cursor'", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(value.map(|value| value.parse()).transpose()?.unwrap_or(0))
    }

    pub fn save_inventory_cursor(&self, cursor: i64) -> crate::Result<()> {
        self.db.execute("INSERT INTO metadata VALUES ('cursor',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [cursor.to_string()])?;
        Ok(())
    }

    pub fn retry(&self, id: &str, version: i64) -> crate::Result<()> {
        uuid::Uuid::parse_str(id)?;
        self.db.execute(
            "INSERT OR IGNORE INTO retry VALUES (?1,?2)",
            params![id, version],
        )?;
        Ok(())
    }

    pub fn retries(&self) -> crate::Result<Vec<(String, i64)>> {
        let mut query = self.db.prepare("SELECT id,version FROM retry LIMIT 100")?;
        Ok(query
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?)
    }

    pub fn clear_retry(&self, id: &str) -> crate::Result<()> {
        self.db.execute("DELETE FROM retry WHERE id=?1", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::privacy::{
        dictionary::Origin,
        policy,
        projector::{Mode, Projector, Status},
    };

    /// Publication needs durable reversibility across offline staging, encryption and another
    /// device. A replacement before commit, lossy chunking, or owner-blind decryption breaks it.
    #[test]
    fn durable_projection_restores_on_another_device_without_sharing_plaintext() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let owner = Owner {
            hub: "hub.example".into(),
            account: "alice-id".into(),
        };
        let key = || UserKey::new(owner.clone(), 1, vec![7; 32]).unwrap();
        let secret = "AKIA2E7YQXK4NMZ5VJ3T";
        let long = "long-private-value\\\"".repeat(4000);
        let policy = policy::Snapshot::compile(
            &[policy::Literal {
                value: &long,
                source: policy::Source::RepositoryUser,
            }],
            &[],
            true,
        )
        .unwrap();
        let store = Store::open(&first.path().join("privacy"), None).unwrap();
        let mut projector = Projector {
            store,
            dictionary: Dictionary::default(),
            policy,
            key: None,
            complete: true,
        };
        let input =
            serde_json::json!({"_session_id": secret, "content": [secret, long]}).to_string();
        let protected = projector.transform(&input, Mode::ProtectJsonl);
        assert!(protected.status == Status::Complete);
        assert_eq!(protected.replacements, 2);
        let parsed: serde_json::Value = serde_json::from_str(&protected.content).unwrap();
        assert_eq!(parsed["_session_id"], secret);
        assert!(!parsed["content"].to_string().contains(secret));
        assert!(
            projector.store.outbox(100).unwrap().is_empty(),
            "plaintext is ineligible for transport"
        );
        let stable = projector.transform(&input, Mode::ProtectJsonl);
        assert_eq!(stable.content, protected.content);
        assert_eq!(
            projector
                .transform(&protected.content, Mode::ProtectJsonl)
                .replacements,
            0
        );
        drop(projector);

        let mut store = Store::open(&first.path().join("privacy"), None).unwrap();
        store.encrypt_pending(&key()).unwrap();
        let mut destination =
            Store::open(&second.path().join("privacy"), Some(owner.clone())).unwrap();
        let envelopes = store.outbox(100).unwrap();
        assert!(!envelopes.is_empty());
        for package in &envelopes {
            let bytes = serde_json::to_string(&package.envelope).unwrap();
            assert!(!bytes.contains(secret) && !bytes.contains("long-private-value"));
            destination
                .import(&package.id, &package.envelope, &key())
                .unwrap();
            destination
                .import(&package.id, &package.envelope, &key())
                .unwrap();
            store.acknowledge(&package.id).unwrap();
        }
        assert!(store.outbox(100).unwrap().is_empty());
        let mut dictionary = Dictionary::default();
        assert!(
            destination
                .load_since(&mut dictionary, 0, |_| Ok(key()))
                .unwrap()
                .1
        );
        assert_eq!(dictionary.records().count(), 2);
        let mut restored = Projector {
            store: destination,
            dictionary,
            policy: policy::Snapshot::compile(&[], &[], true).unwrap(),
            key: Some(key()),
            complete: true,
        };
        let hydrated = restored.transform(&protected.content, Mode::HydrateJsonl);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&hydrated.content).unwrap(),
            serde_json::from_str::<serde_json::Value>(&input).unwrap()
        );
        let package = &envelopes[0];
        let wrong_owner = UserKey::new(
            Owner {
                hub: owner.hub.clone(),
                account: "bob-id".into(),
            },
            1,
            vec![7; 32],
        )
        .unwrap();
        assert!(crypto::open(&wrong_owner, &package.id, &package.envelope).is_err());
        assert!(
            crypto::open(&key(), &uuid::Uuid::new_v4().to_string(), &package.envelope).is_err()
        );
        assert!(restored.store.bind(&wrong_owner.owner).is_err());

        let lock = Connection::open(second.path().join("privacy/journal.sqlite")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let unknown = "AKIA5RJ2NV7MQXP3TC6Z";
        let attempted = restored.transform(unknown, Mode::ProtectText);
        assert_eq!(attempted.content, unknown);
        assert!(attempted.status == Status::Partial);
        assert!(restored.dictionary.for_value(unknown).is_none());
        lock.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            restored.transform(unknown, Mode::ProtectText).replacements,
            1
        );

        let alias = Record::new(&restored.store.dictionary_id, secret, Origin::Heuristic).unwrap();
        let alias_token = alias.token.clone();
        restored
            .store
            .append(&[alias], restored.key.as_ref())
            .unwrap();
        restored
            .store
            .load_since(&mut restored.dictionary, 0, |_| Ok(key()))
            .unwrap();
        assert_eq!(
            restored.transform(&alias_token, Mode::HydrateText).content,
            secret
        );
    }
}
