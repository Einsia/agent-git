//! Native inbox delivery keeps the existing runtime as the sole transcript writer.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const MAX_MESSAGE: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
pub struct Request {
    pub workspace_id: String,
    pub session_id: String,
    pub client_msg_id: String,
    pub message: String,
}

impl Request {
    pub fn validate(&self) -> crate::Result<()> {
        ensure!(
            valid_id(&self.session_id) && valid_id(&self.client_msg_id),
            "an exact session UUID and client message UUID are required"
        );
        ensure!(
            !self.message.trim().is_empty() && self.message.len() <= MAX_MESSAGE,
            "the message must be nonempty and fit the native inbox limit"
        );
        Ok(())
    }
}

pub fn valid_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value)
}

pub struct Prepared {
    pub request: Request,
    pub transcript: PathBuf,
    pub cwd: PathBuf,
    pub codex: PathBuf,
    pub hub: String,
    pub connection: String,
    pub account: String,
    pub username: String,
    pub receipts: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    digest: String,
    status: String,
}

fn sync_directory(path: &Path) -> crate::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn open_regular(path: &Path) -> crate::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "native inbox input must be a regular file"
    );
    Ok(file)
}

fn verify_transcript(path: &Path, session: &str) -> crate::Result<()> {
    let file = open_regular(path)?;
    let mut bytes = Vec::new();
    file.take(128 * 1024).read_to_end(&mut bytes)?;
    let header = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .context("missing transcript header")?;
    let header: Value = serde_json::from_slice(header)?;
    ensure!(
        header["type"] == "session_meta" && header["payload"]["id"] == session,
        "native transcript identity does not match the requested session"
    );
    Ok(())
}

impl Prepared {
    pub async fn deliver(self) -> crate::Result<Value> {
        self.request.validate()?;
        verify_transcript(&self.transcript, &self.request.session_id)?;
        std::fs::create_dir_all(&self.receipts)?;
        ensure!(
            !std::fs::symlink_metadata(&self.receipts)?
                .file_type()
                .is_symlink(),
            "native receipt directory cannot be a symlink"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.receipts, std::fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(windows)]
        crate::infra::windows_security::private_directory(&self.receipts)?;
        sync_directory(&self.receipts)?;
        if let Some(parent) = self.receipts.parent() {
            sync_directory(parent)?;
        }
        let scope = serde_json::to_vec(&(
            &self.hub,
            &self.connection,
            &self.request.workspace_id,
            &self.request.session_id,
            &self.account,
            &self.request.client_msg_id,
        ))?;
        let key = hex::encode(Sha256::digest(scope));
        let path = self.receipts.join(format!("{key}.json"));
        let digest = hex::encode(Sha256::digest(self.request.message.as_bytes()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // Persist the claim before native submission; uncertain delivery never permits a replay.
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(&serde_json::to_vec(&Receipt {
                    digest: digest.clone(),
                    status: "unknown".into(),
                })?)?;
                file.sync_all()?;
                sync_directory(&self.receipts)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut bytes = Vec::new();
                open_regular(&path)?.take(4096).read_to_end(&mut bytes)?;
                let receipt: Receipt = serde_json::from_slice(&bytes)?;
                ensure!(
                    receipt.digest == digest,
                    "client message id already belongs to different content"
                );
                return Ok(
                    json!({"client_msg_id":self.request.client_msg_id,"status":receipt.status}),
                );
            }
            Err(error) => return Err(error.into()),
        }
        let message = format!(
            "[AgentGit workspace message from @{}]\n{}",
            self.username, self.request.message
        );
        let mut child = tokio::process::Command::new(&self.codex)
            .args([
                "queue",
                "--thread",
                &self.request.session_id,
                "--message",
                &message,
            ])
            .current_dir(&self.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("could not start the native Codex inbox; delivery is unknown")?;
        let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
            .await
            .context(
                "native inbox did not confirm delivery; retry with the same client message id",
            )??;
        ensure!(
            status.success(),
            "native inbox did not confirm delivery; check that Codex supports `codex queue` and retry with the same client message id"
        );
        let mut file = tempfile::NamedTempFile::new_in(&self.receipts)?;
        file.write_all(&serde_json::to_vec(&Receipt {
            digest,
            status: "queued".into(),
        })?)?;
        file.as_file().sync_all()?;
        file.persist(&path)?;
        sync_directory(&self.receipts)?;
        Ok(json!({"client_msg_id":self.request.client_msg_id,"status":"queued"}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(root: &Path, session: &str, client_id: &str) -> Prepared {
        Prepared {
            request: Request {
                workspace_id: "workspace".into(),
                session_id: session.into(),
                client_msg_id: client_id.into(),
                message: "Please investigate this bug.".into(),
            },
            transcript: root.join("transcript.jsonl"),
            cwd: root.into(),
            codex: root.join("codex"),
            hub: "https://hub.test".into(),
            connection: "machine".into(),
            account: "member".into(),
            username: "collaborator".into(),
            receipts: root.join("receipts"),
        }
    }

    #[test]
    fn identity_and_size_are_validated_before_native_delivery() {
        let mut value = prepared(
            Path::new("unused"),
            &uuid::Uuid::new_v4().to_string(),
            &uuid::Uuid::new_v4().to_string(),
        );
        assert!(value.request.validate().is_ok());
        value.request.session_id = "a session name".into();
        assert!(value.request.validate().is_err());
        value.request.session_id = uuid::Uuid::new_v4().to_string();
        value.request.message = "x".repeat(MAX_MESSAGE + 1);
        assert!(value.request.validate().is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn receipts_deduplicate_retries_and_bind_content_to_the_authenticated_member() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            root.path().join("transcript.jsonl"),
            format!(
                "{}\n",
                json!({"type":"session_meta","payload":{"id":session}})
            ),
        )
        .unwrap();
        let script = root.path().join("codex");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> native-calls\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            prepared(root.path(), &session, &id)
                .deliver()
                .await
                .unwrap()["status"],
            "queued"
        );
        let first = std::fs::read(root.path().join("native-calls")).unwrap();
        assert!(String::from_utf8_lossy(&first).contains(&session));
        assert!(String::from_utf8_lossy(&first).contains("@collaborator"));
        prepared(root.path(), &session, &id)
            .deliver()
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("native-calls")).unwrap(),
            first
        );
        let mut changed = prepared(root.path(), &session, &id);
        changed.request.message.push('!');
        assert!(changed.deliver().await.is_err());
        let mut other = prepared(root.path(), &session, &id);
        other.account = "another member".into();
        other.deliver().await.unwrap();
        assert!(
            std::fs::read(root.path().join("native-calls"))
                .unwrap()
                .len()
                > first.len()
        );
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' attempted >> native-calls\nexit 1\n",
        )
        .unwrap();
        let unknown_id = uuid::Uuid::new_v4().to_string();
        assert!(
            prepared(root.path(), &session, &unknown_id)
                .deliver()
                .await
                .is_err()
        );
        let calls = std::fs::read(root.path().join("native-calls")).unwrap();
        assert_eq!(
            prepared(root.path(), &session, &unknown_id)
                .deliver()
                .await
                .unwrap()["status"],
            "unknown"
        );
        assert_eq!(
            std::fs::read(root.path().join("native-calls")).unwrap(),
            calls
        );
        let wrong_session = uuid::Uuid::new_v4().to_string();
        assert!(
            prepared(root.path(), &wrong_session, &id)
                .deliver()
                .await
                .is_err()
        );
    }
}
