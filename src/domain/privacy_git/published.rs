//! Retaining public ancestry requires rebuilding its complete graph from the declared payload.

use super::*;
use crate::domain::{privacy_publication::public_inspection_text, storage};
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicSnapshot {
    version: u32,
    session: PublicSession,
    metadata: Value,
    report: ProjectionReport,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicSession {
    log: String,
    view: String,
}

pub(super) struct Reused {
    #[cfg(test)]
    pub envelope_oid: String,
    pub inspection: String,
    pub report: ProjectionReport,
    pub session_id: String,
    pub policy_digest: String,
}

pub(super) fn reuse(
    source: &Repo,
    destination: &Repo,
    oid: &str,
    parents: &[String],
) -> Result<Option<Reused>> {
    let original = read(source, &["cat-file", "commit", oid], MAX_COMMIT)?;
    if !original.ends_with(COMMIT_SUFFIX.as_bytes()) {
        return Ok(None);
    }
    let bytes = read(
        source,
        &["cat-file", "blob", &format!("{oid}:privacy/envelope.json")],
        super::super::privacy_envelope::MAX_ENVELOPE_BYTES,
    )?;
    let envelope = PrivacyEnvelope::parse(&bytes)?;
    ensure!(
        envelope.snapshot_digest == digest_json(&envelope.public_projection)?
            && envelope.attachments.is_empty(),
        "published ancestry has an unsupported snapshot binding"
    );
    let public: PublicSnapshot = serde_json::from_value(envelope.public_projection.clone())
        .context("published ancestry has an unsupported public schema")?;
    ensure!(public.version == 1, "unsupported public snapshot version");
    public.report.validate(&envelope.policy_digest)?;
    let metadata: Meta = serde_json::from_value(public.metadata.clone())?;
    meta::validate(&metadata)?;
    let expected = if metadata.is_file_line() {
        ensure!(
            public.session.log.is_empty() && public.session.view.is_empty(),
            "public file line carries session content"
        );
        Meta::new_file_line()
    } else {
        ensure!(
            meta::is_bare_id(&metadata.session),
            "invalid public session identity"
        );
        let mut expected = Meta::new(
            metadata.session.clone(),
            "claude-code".into(),
            String::new(),
        );
        expected.kind = metadata.kind;
        expected.turn = metadata.turn;
        expected
    };
    let (expected_value, metadata_text) =
        super::super::privacy_metadata::git_metadata(&expected, public.metadata.get("privacy"))?;
    ensure!(
        expected_value == public.metadata,
        "published ancestry carries unprojected metadata"
    );
    let records = storage::parse_envelopes(&public.session.log)?;
    ensure!(
        records.len() == public.report.records,
        "invalid public processing report"
    );
    for record in records {
        ensure!(
            record.source == "claude-code" && record.session_id == metadata.session,
            "published ancestry carries native record identities"
        );
        validate_message(&record.content)?;
    }
    let mut files = if metadata.is_file_line() {
        BTreeMap::new()
    } else {
        storage::snapshot_files(&public.session.log, &public.session.view)?
    };
    files.insert(meta::FILE.into(), metadata_text.into_bytes());
    let inspection = public_inspection_text(
        envelope.public_projection,
        &public.session.log,
        &public.session.view,
        &envelope.policy_digest,
    )?;
    #[cfg(test)]
    let envelope_oid = write_object(destination, "blob", &bytes)?;
    files.insert("privacy/envelope.json".into(), bytes);
    let tree = write_tree(destination, files)?;
    let body = commit_body(&tree, parents);
    // Exact reconstruction rejects extra blobs, symlinks, native archives and unvalidated parents.
    ensure!(
        original == body.as_bytes(),
        "published ancestry differs from its declared public tree or verified parents"
    );
    ensure!(
        write_object(destination, "commit", body.as_bytes())? == oid,
        "published ancestry changed object identity"
    );
    Ok(Some(Reused {
        #[cfg(test)]
        envelope_oid,
        inspection,
        report: public.report,
        session_id: metadata.session,
        policy_digest: envelope.policy_digest,
    }))
}

fn logical_path(alias: &str) -> Result<&str> {
    let (root, path) = alias
        .strip_prefix('<')
        .and_then(|alias| alias.split_once(">/"))
        .context("public file has no logical root")?;
    ensure!(
        !root.is_empty()
            && root
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            && !path.contains(['\\', ':', '<', '>'])
            && !path.chars().any(char::is_control)
            && path
                .split('/')
                .all(|part| !matches!(part, "" | "." | ".." | ".git")),
        "invalid public logical path"
    );
    Ok(path)
}

fn fields(value: &Value, names: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .context("public message is not an object")?;
    ensure!(
        object.len() == names.len() && names.iter().all(|name| object.contains_key(*name)),
        "published ancestry carries unsupported message fields"
    );
    Ok(())
}

fn validate_message(value: &Value) -> Result<()> {
    public_record_fields(value)?;
    let role = value["type"].as_str().context("invalid public role")?;
    ensure!(matches!(role, "user" | "assistant"), "invalid public role");
    validate_classification(value)?;
    let message = &value["message"];
    fields(message, &["role", "content"])?;
    ensure!(message["role"] == role, "public message role mismatch");
    let blocks = message["content"]
        .as_array()
        .context("invalid public message blocks")?;
    ensure!(!blocks.is_empty(), "empty public message");
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                fields(block, &["type", "text"])?;
                ensure!(block["text"].is_string(), "invalid public text");
            }
            Some("thinking") => {
                ensure!(
                    role == "assistant",
                    "public thinking must belong to assistant"
                );
                fields(block, &["type", "thinking"])?;
                ensure!(block["thinking"].is_string(), "invalid public thinking");
            }
            Some("tool_use") => {
                fields(block, &["type", "id", "name", "input"])?;
                ensure!(block["id"].is_string(), "invalid public call identity");
                let name = block["name"].as_str().context("invalid public tool name")?;
                ensure!(!name.is_empty(), "invalid public tool name");
                if matches!(
                    name,
                    "Read" | "read_file" | "Edit" | "edit_file" | "Write" | "write_file"
                ) {
                    let input = block["input"]
                        .as_object()
                        .context("invalid public file tool input")?;
                    ensure!(
                        input.keys().all(|name| matches!(
                            name.as_str(),
                            "path"
                                | "file_path"
                                | "offset"
                                | "limit"
                                | "old_string"
                                | "new_string"
                                | "replace_all"
                                | "content"
                        )),
                        "unsupported public file tool input"
                    );
                    let paths = [input.get("path"), input.get("file_path")]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>();
                    ensure!(paths.len() == 1, "ambiguous public tool path");
                    logical_path(paths[0].as_str().context("invalid public tool path")?)?;
                }
            }
            Some("tool_result") => {
                fields(block, &["type", "tool_use_id", "content", "is_error"])?;
                ensure!(
                    block["tool_use_id"].is_string()
                        && block["content"].is_string()
                        && block["is_error"].is_boolean(),
                    "invalid public tool result"
                );
            }
            _ => anyhow::bail!("unsupported public message block"),
        }
    }
    Ok(())
}

fn public_record_fields(value: &Value) -> Result<()> {
    let object = value
        .as_object()
        .context("public message is not an object")?;
    ensure!(
        object.contains_key("type")
            && object.contains_key("message")
            && object.keys().all(|name| {
                matches!(
                    name.as_str(),
                    "type" | "message" | "isMeta" | "isCompactSummary" | "promptSource"
                )
            }),
        "published ancestry carries unsupported message fields"
    );
    Ok(())
}

fn validate_classification(value: &Value) -> Result<()> {
    let mut markers = 0;
    if let Some(marker) = value.get("isMeta") {
        ensure!(marker == true, "invalid public isMeta marker");
        markers += 1;
    }
    if let Some(marker) = value.get("isCompactSummary") {
        ensure!(marker == true, "invalid public compact-summary marker");
        markers += 1;
    }
    if let Some(marker) = value.get("promptSource") {
        ensure!(marker == "system", "invalid public prompt source");
        markers += 1;
    }
    ensure!(markers <= 1, "ambiguous public message classification");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_message;
    use serde_json::json;

    fn message(role: &str, marker: serde_json::Value) -> serde_json::Value {
        let mut value = json!({
            "type": role,
            "message": {"role": role, "content": [{"type": "text", "text": "internal"}]}
        });
        if let Some((key, marker)) = marker.as_object().and_then(|object| {
            object
                .iter()
                .next()
                .map(|(key, marker)| (key.clone(), marker.clone()))
        }) {
            value[key] = marker;
        }
        value
    }

    #[test]
    fn accepts_fixed_public_classification_markers() {
        for role in ["user", "assistant"] {
            for marker in [
                json!({"isMeta": true}),
                json!({"isCompactSummary": true}),
                json!({"promptSource": "system"}),
            ] {
                validate_message(&message(role, marker)).unwrap();
            }
        }
    }

    #[test]
    fn accepts_thinking_only_on_assistant_messages() {
        let value = json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "masked"},
                {"type": "text", "text": "reply"}
            ]}
        });
        validate_message(&value).unwrap();
    }

    #[test]
    fn rejects_malformed_thinking_blocks() {
        for value in [
            json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{"type":"thinking","thinking":1}]}
            }),
            json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{"type":"thinking","thinking":"x","signature":"opaque"}]}
            }),
            json!({
                "type": "user",
                "message": {"role": "user", "content": [{"type":"thinking","thinking":"x"}]}
            }),
        ] {
            assert!(validate_message(&value).is_err());
        }
    }

    #[test]
    fn rejects_ambiguous_or_invalid_public_classification_markers() {
        assert!(validate_message(&message("user", json!({"isMeta": false}))).is_err());
        assert!(
            validate_message(&json!({
                "type": "assistant",
                "isMeta": true,
                "promptSource": "system",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "internal"}]}
            }))
            .is_err()
        );
    }
}
