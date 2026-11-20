//! Files supplied with an authorized turn live in an immutable directory belonging to that session.
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path};

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
    session: &str,
    input: &str,
    parts: &Value,
    text: &str,
) -> anyhow::Result<String> {
    let files: Vec<Input> = serde_json::from_value(parts.clone())?;
    ensure!(
        !files.is_empty() && files.len() <= 4,
        "invalid number of attachments"
    );
    let input = uuid::Uuid::parse_str(input)
        .context("attachments require an input id")?
        .to_string();
    let directory = root.join(hex::encode(Sha256::digest(session.as_bytes())));
    crate::infra::config::create_state_dir(&directory)?;
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
    let target = directory.join(input);
    let mut references = Vec::new();
    let temporary = tempfile::Builder::new()
        .prefix(".incoming-")
        .tempdir_in(&directory)?;
    for (name, filename, bytes) in &prepared {
        let mut output = crate::infra::config::state_file_options()
            .create_new(true)
            .write(true)
            .open(temporary.path().join(filename))?;
        output.write_all(bytes)?;
        output.sync_all()?;
        references.push(serde_json::json!({"name":name,"path":target.join(filename)}));
    }
    // Publish the entire set together. A retry may reuse it only if every object still matches.
    if let Err(error) = std::fs::rename(temporary.path(), &target) {
        ensure!(
            std::fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir()),
            "could not publish input attachments: {error}"
        );
        ensure!(
            std::fs::read_dir(&target)?.count() == prepared.len(),
            "input id already has different attachments"
        );
        for (_, filename, bytes) in &prepared {
            let path = target.join(filename);
            let metadata = std::fs::symlink_metadata(&path).context("attachment set changed")?;
            ensure!(
                metadata.is_file()
                    && metadata.len() == bytes.len() as u64
                    && std::fs::read(path)? == *bytes,
                "attachment content changed"
            );
        }
    }
    Ok(format!(
        "{text}\n\nAttached files on this device:\n{}",
        serde_json::to_string(&references)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_second_file_leaves_no_partial_set_and_sessions_are_isolated() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let id = uuid::Uuid::new_v4().to_string();
        let file = |value: &[u8]| {
            serde_json::json!({
                "name": "report.txt", "data": STANDARD.encode(value), "sha256":hex::encode(Sha256::digest(value))
            })
        };
        let mut invalid = file(b"second");
        invalid["sha256"] = serde_json::json!("invalid");
        let broken = serde_json::json!([file(b"first"), invalid]);
        assert!(stage(root.path(), "alice", &id, &broken, "read").is_err());
        let session_dir = root.path().join(hex::encode(Sha256::digest(b"alice")));
        assert!(!session_dir.join(&id).exists());
        let path_for = |session: &str, value: &[u8]| -> anyhow::Result<std::path::PathBuf> {
            let message = stage(
                root.path(),
                session,
                &id,
                &serde_json::json!([file(value)]),
                "read",
            )?;
            let refs: Value = serde_json::from_str(message.lines().last().unwrap())?;
            Ok(refs[0]["path"].as_str().unwrap().into())
        };
        let alice = path_for("alice", b"first")?;
        let bob = path_for("bob", b"other")?;
        assert_ne!(alice.parent(), bob.parent());
        assert_eq!(std::fs::read(alice)?, b"first");
        assert_eq!(std::fs::read(bob)?, b"other");
        Ok(())
    }

    #[test]
    fn retries_reuse_verified_files_and_display_names_cannot_choose_paths() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let id = uuid::Uuid::new_v4().to_string();
        let data = |bytes: &[u8]| serde_json::json!([{"name":"../../other.txt","data":STANDARD.encode(bytes),"sha256":hex::encode(Sha256::digest(bytes))}]);
        let first = stage(root.path(), "session", &id, &data(b"abc"), "read")?;
        assert_eq!(
            stage(root.path(), "session", &id, &data(b"abc"), "read")?,
            first
        );
        assert!(stage(root.path(), "session", &id, &data(b"different"), "read").is_err());
        assert!(stage(root.path(), "session", "../escape", &data(b"abc"), "read").is_err());
        let refs: Value = serde_json::from_str(first.lines().last().unwrap())?;
        let path = Path::new(refs[0]["path"].as_str().unwrap());
        assert!(path.starts_with(root.path()));
        assert_eq!(std::fs::read(path)?, b"abc");
        Ok(())
    }
}
