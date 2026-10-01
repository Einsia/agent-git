//! Recoverable session snapshots inside the authenticated private layer.
//!
//! LOG and VIEW are canonical envelope JSONL. Restoration reconstructs content-addressed session
//! files; source paths are historical evidence and never destinations or execution authority.

use super::{meta, privacy_envelope::decode_bounded, storage};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use zeroize::{Zeroize, Zeroizing};

pub const PRIVATE_LAYER_VERSION: u32 = 1;
const MAX_LAYER_BYTES: usize = 4 * 1024 * 1024 - 16;

pub(crate) use super::transcript::recovery::{EVIDENCE_FIELD, EVIDENCE_PREFIX, EvidenceReference};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateSession {
    pub log: String,
    pub view: String,
}

/// Every content byte and reverse mapping in this structure is private.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateLayer {
    pub version: u32,
    pub session: PrivateSession,
    pub metadata: Value,
    pub path_aliases: BTreeMap<String, String>,
    /// Literal values that must retain protection when original text moves to another device.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub protected_values: BTreeSet<String>,
}

impl std::fmt::Debug for PrivateLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivateLayer(<redacted>)")
    }
}

impl Drop for PrivateLayer {
    fn drop(&mut self) {
        self.session.log.zeroize();
        self.session.view.zeroize();
        erase_json(&mut self.metadata);
        for (mut alias, mut source) in std::mem::take(&mut self.path_aliases) {
            alias.zeroize();
            source.zeroize();
        }
        for mut value in std::mem::take(&mut self.protected_values) {
            value.zeroize();
        }
    }
}

impl PrivateLayer {
    /// Base64 session bytes alone must fit before allocating their encoded copies.
    pub(crate) fn check_session_size(log: usize, view: usize) -> Result<()> {
        let encoded = |length: usize| length.checked_add(2)?.checked_div(3)?.checked_mul(4);
        ensure!(
            encoded(log)
                .zip(encoded(view))
                .and_then(|(log, view)| log.checked_add(view))
                .is_some_and(|total| total <= MAX_LAYER_BYTES),
            "private session exceeds the envelope budget"
        );
        Ok(())
    }

    pub fn new(
        log: &str,
        view: &str,
        metadata: Value,
        path_aliases: BTreeMap<String, String>,
    ) -> Result<Self> {
        Self::check_session_size(log.len(), view.len())?;
        let layer = Self {
            version: PRIVATE_LAYER_VERSION,
            session: PrivateSession {
                log: STANDARD.encode(log),
                view: STANDARD.encode(view),
            },
            metadata,
            path_aliases,
            protected_values: BTreeSet::new(),
        };
        layer.validate()?;
        Ok(layer)
    }

    pub fn session_bytes(&self) -> Result<(Zeroizing<String>, Zeroizing<String>)> {
        let log = decode_bounded(&self.session.log, MAX_LAYER_BYTES, "private LOG")?;
        let view = decode_bounded(&self.session.view, MAX_LAYER_BYTES, "private VIEW")?;
        Ok((
            Zeroizing::new(
                String::from_utf8(log).map_err(|_| anyhow::anyhow!("private LOG is not UTF-8"))?,
            ),
            Zeroizing::new(
                String::from_utf8(view)
                    .map_err(|_| anyhow::anyhow!("private VIEW is not UTF-8"))?,
            ),
        ))
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == PRIVATE_LAYER_VERSION,
            "unsupported private layer version"
        );
        let encoded = Zeroizing::new(serde_json::to_vec(self)?);
        ensure!(
            encoded.len() <= MAX_LAYER_BYTES,
            "private layer exceeds the envelope budget"
        );
        let snapshot: meta::Meta = serde_json::from_value(self.metadata.clone())
            .map_err(|_| anyhow::anyhow!("invalid private session metadata"))?;
        meta::validate(&snapshot)
            .map_err(|_| anyhow::anyhow!("invalid private session metadata"))?;
        let (log, view) = self.session_bytes()?;
        storage::snapshot_files(&log, &view)
            .map_err(|_| anyhow::anyhow!("private LOG or VIEW is not a valid session snapshot"))?;
        ensure!(
            self.protected_values
                .iter()
                .all(|value| !value.is_empty() && value.len() <= 64 * 1024),
            "invalid private protection value"
        );
        for (alias, source) in &self.path_aliases {
            validate_alias(alias)?;
            ensure!(
                !source.is_empty() && !source.chars().any(char::is_control),
                "invalid private path source"
            );
        }
        Ok(())
    }

    pub(crate) fn import_protection(&self, repo: &super::repo::Repo) -> Result<()> {
        self.validate()?;
        let _ = super::privacy::service::manage(
            Some(repo.root()),
            super::privacy::management::Command {
                action: "remember".into(),
                global: false,
                id: None,
                name: None,
                secret: Some(serde_json::to_string(&self.protected_values)?),
            },
        );
        Ok(())
    }

    /// Materialize into a fresh private staging directory, never into a path named by the payload.
    pub fn restore(
        &self,
        parent: &std::path::Path,
        workspace: &std::path::Path,
    ) -> Result<tempfile::TempDir> {
        self.validate()?;
        let workspace = workspace
            .canonicalize()
            .context("recovery workspace is unavailable")?;
        ensure!(workspace.is_dir(), "recovery workspace must be a directory");
        let staging = tempfile::Builder::new()
            .prefix("privacy-recovery-")
            .tempdir_in(parent)?;
        let (log, view) = self.session_bytes()?;
        storage::write_snapshot(staging.path(), &log, &view)?;
        let mut metadata = self.metadata.clone();
        let values = metadata
            .as_object_mut()
            .context("private metadata must be an object")?;
        values.insert(
            "layout".into(),
            serde_json::to_value(meta::LayoutVersion::CURRENT)?,
        );
        values.insert(
            "cwd".into(),
            Value::String(workspace.to_string_lossy().into_owned()),
        );
        // Imported runtime ownership and machine observations cannot claim the new device.
        for key in [
            "runtime_instances",
            "baseline_bytes",
            "cwd_state",
            "code_state",
            "cwd_is_agent_repository",
        ] {
            values.remove(key);
        }
        let session_dir = staging.path().join("session");
        std::fs::create_dir_all(&session_dir)?;
        std::fs::write(
            staging.path().join(meta::FILE),
            serde_json::to_vec(&metadata)?,
        )?;
        std::fs::write(
            session_dir.join("private-path-aliases.json"),
            serde_json::to_vec(&self.path_aliases)?,
        )?;
        erase_json(&mut metadata);
        Ok(staging)
    }
}

fn validate_alias(alias: &str) -> Result<()> {
    let (root, suffix) = alias
        .strip_prefix('<')
        .and_then(|value| value.split_once('>'))
        .context("private path alias has no logical root")?;
    ensure!(
        !root.is_empty()
            && root
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "private path alias root is invalid"
    );
    if !suffix.is_empty() {
        let relative = suffix
            .strip_prefix('/')
            .context("private path alias suffix is invalid")?;
        ensure!(
            !relative.contains(['\\', ':', '<', '>'])
                && !relative.chars().any(char::is_control)
                && relative
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != ".."),
            "private path alias suffix is invalid"
        );
    }
    Ok(())
}

fn erase_json(value: &mut Value) {
    match value {
        Value::String(text) => text.zeroize(),
        Value::Array(values) => {
            for value in values {
                erase_json(value);
            }
        }
        Value::Object(values) => {
            for (mut key, mut value) in std::mem::take(values) {
                key.zeroize();
                erase_json(&mut value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        privacy_envelope::{PrivacyEnvelope, ViewingRecipient, digest_bytes},
        transcript,
    };
    use crypto_box::SecretKey;

    /// Oversized input is rejected before Base64 copies or parsing an invalid session body.
    #[test]
    fn private_session_budget_precedes_encoding_and_parsing() {
        assert!(PrivateLayer::check_session_size(usize::MAX, 1).is_err());
        let source = "x".repeat(MAX_LAYER_BYTES);
        let error = PrivateLayer::new(&source, &source, Value::Null, BTreeMap::new()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("private session exceeds the envelope budget")
        );
    }

    #[test]
    fn encrypted_layer_restores_a_readable_session_with_rebound_workspace() {
        let session = format!("agit-{}", "a".repeat(40));
        let log = transcript::wrap_lines(
            "{\"type\":\"message\",\"content\":\"original evidence\"}\n",
            "codex",
            &session,
        );
        let mut metadata =
            meta::Meta::new(session, "codex".into(), "/old/private/workspace".into());
        metadata
            .runtime_instances
            .push("old-runtime-instance".into());
        metadata.baseline_bytes = Some(42);
        let aliases = BTreeMap::from([(
            "<workspace>/src/main.rs".into(),
            "/old/private/workspace/src/main.rs".into(),
        )]);
        let mut layer =
            PrivateLayer::new(&log, &log, serde_json::to_value(metadata).unwrap(), aliases)
                .unwrap();
        let key = SecretKey::from([9; 32]);
        let recipient = ViewingRecipient::from_base64(
            "viewer".into(),
            &STANDARD.encode(key.public_key().as_bytes()),
        )
        .unwrap();
        let envelope = PrivacyEnvelope::seal_layer(
            digest_bytes(b"policy"),
            digest_bytes(b"snapshot"),
            serde_json::json!({"safe": true}),
            &layer,
            &recipient,
            Vec::new(),
        )
        .unwrap();
        let restored = envelope.open_layer(&key).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let directory = restored.restore(parent.path(), workspace.path()).unwrap();
        assert_eq!(
            storage::materialize_worktree(directory.path(), meta::LOG_FILE).unwrap(),
            log
        );
        assert_eq!(
            storage::materialize_worktree(directory.path(), meta::VIEW_FILE).unwrap(),
            log
        );
        let rebound = meta::read(directory.path()).unwrap();
        assert_eq!(
            rebound.cwd,
            workspace.path().canonicalize().unwrap().to_string_lossy()
        );
        assert!(rebound.runtime_instances.is_empty());
        assert!(rebound.baseline_bytes.is_none());
        assert!(!directory.path().join("attachments").exists());
        assert_eq!(
            restored.path_aliases["<workspace>/src/main.rs"],
            "/old/private/workspace/src/main.rs"
        );
        layer
            .path_aliases
            .insert("<workspace>/../outside".into(), "/old/outside".into());
        assert!(layer.restore(parent.path(), workspace.path()).is_err());
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1);
    }
}
