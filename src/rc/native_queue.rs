//! Durable operation claims prevent retries from adding the same native work twice.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::PathBuf,
};

pub struct Request {
    pub source: crate::protocol::NativeSourceRef,
    pub native_id: String,
    pub workspace_id: String,
    pub hub: String,
    pub account: String,
    pub client_id: String,
    pub message: String,
    pub username: Option<String>,
    pub receipts: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub digest: String,
    pub client_id: String,
    pub status: String,
    pub queue_id: Option<String>,
    pub turn_id: Option<String>,
    #[serde(default)]
    pub log_path: Option<PathBuf>,
    #[serde(default)]
    pub queue_cursor: Option<String>,
    #[serde(default)]
    pub log_cursor: u64,
    #[serde(default)]
    pub log_turn_id: Option<String>,
    #[serde(default)]
    pub log_skip: bool,
}

pub(crate) struct Claim {
    path: PathBuf,
    _lock: ClaimLock,
    pub receipt: Receipt,
    pub fresh: bool,
}

struct ClaimLock(File);

impl Drop for ClaimLock {
    fn drop(&mut self) {
        // A forked child can retain the open file description until exec. Closing
        // this descriptor alone would keep retries locked behind that child.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

impl Request {
    pub(crate) fn claim(&self) -> crate::Result<Claim> {
        ensure!(
            crate::rc::native_inbox::valid_id(&self.client_id),
            "a client message UUID is required"
        );
        ensure!(
            !self.message.trim().is_empty()
                && self.message.len() <= crate::rc::native_inbox::MAX_MESSAGE,
            "the message must be nonempty and fit the native queue limit"
        );
        let scope = serde_json::to_vec(&(
            &self.hub,
            &self.workspace_id,
            &self.source.source_id,
            &self.native_id,
            &self.account,
            &self.client_id,
        ))?;
        let key = hex::encode(Sha256::digest(scope));
        let digest = hex::encode(Sha256::digest(self.message.as_bytes()));
        crate::infra::config::create_state_dir(&self.receipts)?;
        ensure!(
            !std::fs::symlink_metadata(&self.receipts)?
                .file_type()
                .is_symlink(),
            "queue receipt directory cannot be a symlink"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.receipts, std::fs::Permissions::from_mode(0o700))?;
            if let Some(parent) = self.receipts.parent() {
                File::open(parent)?.sync_all()?;
            }
        }
        #[cfg(windows)]
        crate::infra::windows_security::private_directory(&self.receipts)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lock = options.open(self.receipts.join(format!("{key}.lock")))?;
        ensure!(
            lock.metadata()?.is_file(),
            "queue claim lock is not a regular file"
        );
        fs2::FileExt::try_lock_exclusive(&lock)
            .context("this queue operation is still in flight; retry the same client message id")?;
        let lock = ClaimLock(lock);
        let path = self.receipts.join(format!("{key}.json"));
        let fresh = !path.try_exists()?;
        let receipt = if fresh {
            Receipt {
                digest,
                client_id: uuid::Uuid::new_v4().to_string(),
                status: "unknown".into(),
                queue_id: None,
                turn_id: None,
                log_path: None,
                queue_cursor: None,
                log_cursor: 0,
                log_turn_id: None,
                log_skip: false,
            }
        } else {
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let file = options.open(&path)?;
            ensure!(
                file.metadata()?.is_file(),
                "queue receipt is not a regular file"
            );
            let mut bytes = Vec::new();
            file.take(4097).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 4096, "queue receipt exceeds its size limit");
            let receipt: Receipt = serde_json::from_slice(&bytes)?;
            ensure!(
                receipt.digest == digest,
                "client message id already belongs to different content"
            );
            ensure!(
                crate::rc::native_inbox::valid_id(&receipt.client_id),
                "invalid native queue correlation id"
            );
            receipt
        };
        let claim = Claim {
            path,
            _lock: lock,
            receipt,
            fresh,
        };
        if fresh {
            claim.save()?;
        }
        Ok(claim)
    }

    pub(crate) fn message(&self) -> String {
        crate::rc::native_inbox::labelled(self.username.as_deref(), &self.message)
    }
}

impl Claim {
    pub fn save(&self) -> crate::Result<()> {
        let parent = self
            .path
            .parent()
            .context("queue receipt has no directory")?;
        let bytes = serde_json::to_vec(&self.receipt)?;
        ensure!(bytes.len() <= 4096, "queue receipt exceeds its size limit");
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn reconcile_log(&mut self, path: &std::path::Path, native_id: &str) -> crate::Result<()> {
        use std::io::{BufRead, Seek, SeekFrom};
        let mut file = crate::rc::native_inbox::open_regular(path)?;
        crate::rc::native_inbox::verify_open_transcript(&mut file, native_id)?;
        if self.receipt.log_path.as_deref() != Some(path)
            || file.metadata()?.len() < self.receipt.log_cursor
        {
            self.receipt.log_path = Some(path.to_path_buf());
            self.receipt.log_cursor = 0;
            self.receipt.log_turn_id = None;
            self.receipt.log_skip = false;
        }
        file.seek(SeekFrom::Start(self.receipt.log_cursor))?;
        let mut reader = std::io::BufReader::new(file);
        let mut budget = 4 * 1024 * 1024;
        const LINE_LIMIT: usize = 64 * 1024;
        while budget > 0 {
            let limit = budget.min(LINE_LIMIT);
            let mut line = Vec::new();
            (&mut reader)
                .take(limit as u64)
                .read_until(b'\n', &mut line)?;
            if line.is_empty() {
                break;
            }
            budget -= line.len();
            let complete = line.ends_with(b"\n");
            if !complete && line.len() < LINE_LIMIT && !self.receipt.log_skip {
                break;
            }
            self.receipt.log_cursor += line.len() as u64;
            if self.receipt.log_skip || !complete {
                self.receipt.log_skip = !complete;
                continue;
            }
            let Ok(record) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if record["type"] != "event_msg" {
                continue;
            }
            let payload = &record["payload"];
            if payload["type"] == "task_started" {
                self.receipt.log_turn_id = payload["turn_id"]
                    .as_str()
                    .filter(|id| crate::rc::harness::validate_native_turn_id(id).is_ok())
                    .map(str::to_owned);
            }
            if payload["type"] == "user_message" && payload["client_id"] == self.receipt.client_id {
                self.receipt.status = "delivered".into();
                self.receipt.turn_id = self.receipt.log_turn_id.clone();
                break;
            }
        }
        self.save()
    }

    pub fn reply(&self, client_id: &str) -> Value {
        json!({"client_msg_id":client_id,"native_client_id":self.receipt.client_id,"status":self.receipt.status,
            "native_queue_id":self.receipt.queue_id,"native_turn_id":self.receipt.turn_id})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(root: &std::path::Path) -> Request {
        Request {
            source: crate::protocol::NativeSourceRef {
                source_id: format!("src-{}", uuid::Uuid::new_v4()),
                generation: 1,
            },
            native_id: uuid::Uuid::new_v4().to_string(),
            workspace_id: "workspace".into(),
            hub: "https://hub.invalid".into(),
            account: "member".into(),
            client_id: uuid::Uuid::new_v4().to_string(),
            message: "Investigate the queue".into(),
            username: None,
            receipts: root.join("receipts"),
        }
    }

    #[test]
    fn claims_survive_restart_and_keep_source_member_and_content_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let mut request = request(root.path());
        let claim = request.claim().unwrap();
        assert!(claim.fresh);
        let native_correlation = claim.receipt.client_id.clone();
        assert!(
            request.claim().is_err(),
            "an in-flight claim must exclude a second sender"
        );
        drop(claim);
        let claim = request.claim().unwrap();
        assert!(!claim.fresh);
        assert_eq!(claim.receipt.status, "unknown");
        assert_eq!(claim.receipt.client_id, native_correlation);
        drop(claim);
        request.message.push_str(" changed");
        assert!(request.claim().is_err());
        request.message = "Investigate the queue".into();
        request.source.generation += 1;
        assert!(
            !request.claim().unwrap().fresh,
            "generation refresh must not replay native work"
        );
        request.source.source_id = format!("src-{}", uuid::Uuid::new_v4());
        assert!(request.claim().unwrap().fresh);
        request.account = "another-member".into();
        assert!(request.claim().unwrap().fresh);
    }
    #[test]
    fn dropping_a_claim_unlocks_while_a_duplicate_descriptor_remains_open() {
        let root = tempfile::tempdir().unwrap();
        let request = request(root.path());
        let claim = request.claim().unwrap();
        let duplicate = claim._lock.0.try_clone().unwrap();
        assert!(request.claim().is_err());
        drop(claim);
        let retry = request.claim().unwrap();
        assert!(!retry.fresh);
        drop(duplicate);
    }

    #[test]
    fn recovery_skips_large_records_and_retries_an_unfinished_native_line() {
        let root = tempfile::tempdir().unwrap();
        let request = request(root.path());
        let mut claim = request.claim().unwrap();
        let path = root.path().join("rollout.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"session_meta","payload":{"id":request.native_id}})
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn"}})
        )
        .unwrap();
        file.write_all(&vec![b'x'; 5 * 1024 * 1024]).unwrap();
        file.write_all(b"\n").unwrap();
        let message = json!({"type":"event_msg","payload":{"type":"user_message","client_id":claim.receipt.client_id}}).to_string();
        file.write_all(message.as_bytes()).unwrap();
        claim.reconcile_log(&path, &request.native_id).unwrap();
        assert_eq!(claim.receipt.status, "unknown");
        assert!(claim.receipt.log_cursor <= 4 * 1024 * 1024);
        drop(claim);
        let mut claim = request.claim().unwrap();
        claim.reconcile_log(&path, &request.native_id).unwrap();
        assert_eq!(claim.receipt.status, "unknown");
        file.write_all(b"\n").unwrap();
        claim.reconcile_log(&path, &request.native_id).unwrap();
        assert_eq!(claim.receipt.status, "delivered");
        assert_eq!(claim.receipt.turn_id.as_deref(), Some("turn"));
    }
}
