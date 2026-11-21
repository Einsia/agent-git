//! Files supplied with an authorized turn live in an immutable directory belonging to that session.
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

mod custody;
#[cfg(test)]
pub(super) use custody::MANAGED;
pub(crate) use custody::{Owner, Reference, retire_device, snapshot_account};
pub(super) use custody::{
    authorize_waiting, forget_unowned_waiting, migrate_legacy_waiting, recover_retired_waiting,
    update_waiting,
};

pub(super) struct Staged {
    pub message: String,
    pub reference: Reference,
}

#[derive(Debug)]
pub(super) enum Unavailable {
    Legacy,
    Expired,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Legacy => "this input has unmanaged legacy attachments; send a new input",
            Self::Expired => "this input's attachments were cleared or retired; send a new input",
        })
    }
}
impl std::error::Error for Unavailable {}

const MAX_BYTES: usize = 512 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    name: String,
    data: String,
    sha256: String,
}

pub(super) fn stage(
    root: &Path,
    owner: Owner,
    session: &str,
    input: &str,
    parts: &Value,
    text: &str,
) -> anyhow::Result<Staged> {
    let files: Vec<Input> = serde_json::from_value(parts.clone())?;
    ensure!(
        !files.is_empty() && files.len() <= 4,
        "invalid number of attachments"
    );
    let input = uuid::Uuid::parse_str(input)
        .context("attachments require an input id")?
        .to_string();
    let mut total = 0;
    let mut prepared = Vec::new();
    for file in files {
        ensure!(
            file.name.len() <= 200 && !file.name.chars().any(char::is_control),
            "invalid attachment name"
        );
        ensure!(
            file.data.len() <= 4 * MAX_BYTES.div_ceil(3),
            "attachment too large"
        );
        let bytes = STANDARD.decode(file.data)?;
        total += bytes.len();
        ensure!(total <= MAX_BYTES, "attachments too large");
        let digest = hex::encode(Sha256::digest(&bytes));
        ensure!(digest == file.sha256, "attachment content changed");
        let suffix = file
            .name
            .rsplit_once('.')
            .map(|(_, s)| s.to_ascii_lowercase())
            .unwrap_or_default();
        let extension = match suffix.as_str() {
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "pdf" | "txt" | "md" | "csv" | "json"
            | "opus" | "ogg" | "mp3" | "wav" | "xlsx" | "docx" => suffix.as_str(),
            _ => "bin",
        };
        let filename = format!("{}-{digest}.{extension}", prepared.len());
        prepared.push((file.name, filename, bytes));
    }
    let (reference, target) = custody::publish(root, owner, session, &input, &prepared)?;
    let mut references = Vec::new();
    for (name, filename, _) in &prepared {
        references.push(serde_json::json!({"name":name,"path":target.join(filename)}));
    }
    let message = format!(
        "{text}\n\nAttached files on this device:\n{}",
        serde_json::to_string(&references)?
    );
    Ok(Staged { message, reference })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(bytes: &[u8]) -> Value {
        serde_json::json!({"name":"../../report.txt", "data":STANDARD.encode(bytes),
            "sha256":hex::encode(Sha256::digest(bytes))})
    }

    fn path(staged: &Staged) -> std::path::PathBuf {
        let refs: Value = serde_json::from_str(staged.message.lines().last().unwrap()).unwrap();
        refs[0]["path"].as_str().unwrap().into()
    }

    #[test]
    fn invalid_second_file_leaves_no_partial_set_and_sessions_are_isolated() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut invalid = file(b"second");
        invalid["sha256"] = serde_json::json!("invalid");
        assert!(
            stage(
                root.path(),
                Owner::Local,
                "alice",
                &id,
                &serde_json::json!([file(b"first"), invalid]),
                "read"
            )
            .is_err()
        );
        assert!(!root.path().join("owned-v1").exists());
        let alice = stage(
            root.path(),
            Owner::Local,
            "alice",
            &id,
            &serde_json::json!([file(b"first")]),
            "read",
        )?;
        let bob = stage(
            root.path(),
            Owner::Local,
            "bob",
            &id,
            &serde_json::json!([file(b"other")]),
            "read",
        )?;
        assert_ne!(path(&alice).parent(), path(&bob).parent());
        assert_eq!(std::fs::read(path(&alice))?, b"first");
        assert_eq!(std::fs::read(path(&bob))?, b"other");
        let refused = path(&alice);
        drop(alice);
        assert_eq!(std::fs::read(refused)?, b"first");
        assert!(path(&bob).is_file());
        Ok(())
    }

    #[test]
    fn retries_reuse_verified_files_and_display_names_cannot_choose_paths() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let id = uuid::Uuid::new_v4().to_string();
        let data = serde_json::json!([file(b"abc")]);
        let first = stage(root.path(), Owner::Local, "session", &id, &data, "read")?;
        let retry = stage(root.path(), Owner::Local, "session", &id, &data, "read")?;
        assert_eq!(first.message, retry.message);
        drop(retry);
        assert!(
            stage(
                root.path(),
                Owner::Local,
                "session",
                &id,
                &serde_json::json!([file(b"different")]),
                "read"
            )
            .is_err()
        );
        assert!(
            stage(
                root.path(),
                Owner::Local,
                "session",
                "../escape",
                &data,
                "read"
            )
            .is_err()
        );
        let payload = path(&first);
        assert!(payload.starts_with(root.path()));
        assert_eq!(std::fs::read(&payload)?, b"abc");
        let resumed = stage(root.path(), Owner::Local, "session", &id, &data, "read")?;
        assert_eq!(path(&resumed), payload);
        assert_eq!(std::fs::read(&payload)?, b"abc");
        drop(resumed);
        assert!(
            payload.is_file(),
            "a failed retry cannot clear the transcript path"
        );
        Ok(())
    }
}
