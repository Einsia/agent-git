//! Public metadata projection for privacy-aware publication.
//!
//! Session metadata is useful for structure but its raw `cwd`, session identity, runtime claims,
//! and worktree details are local evidence. This module emits an explicit field allowlist and
//! leaves the complete `Meta` value for the encrypted private layer.

use crate::domain::meta::{Completeness, Kind, Line, Meta, WorktreeStatus};
use crate::domain::privacy::{MetadataMode, PrivacyPolicy};
use crate::domain::privacy_paths::{AliasRoot, PathAliasStore};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Map, Value, json};

/// Public metadata that is safe to serialize with a projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MetadataProjection {
    pub schema_version: u32,
    pub value: Value,
}

/// Presentation metadata stays outside the fields used to route local runtimes and workspaces.
pub fn validate_public(value: &Value) -> Result<()> {
    let fields = value
        .as_object()
        .context("public privacy metadata must be an object")?;
    ensure!(
        fields.get("schema_version") == Some(&json!(1)),
        "unsupported public privacy metadata version"
    );
    ensure!(
        fields.contains_key("line") && fields.contains_key("kind"),
        "public privacy metadata lacks its session shape"
    );
    for (name, value) in fields {
        let valid = match name.as_str() {
            "schema_version" => value == &json!(1),
            "line" | "kind" | "runtime" | "completeness" | "workspace" | "origin" | "worktree" => {
                value.is_string()
            }
            "turn" | "staged" | "unstaged" | "untracked" | "conflicted" => value
                .as_u64()
                .is_some_and(|number| u32::try_from(number).is_ok()),
            _ => false,
        };
        ensure!(valid, "unsupported public privacy metadata field");
    }
    Ok(())
}

/// The optional presentation object is serialized identically by generation and ancestry checks.
/// An absent presentation retains the canonical legacy storage metadata bytes.
pub fn git_metadata(storage: &Meta, projection: Option<&Value>) -> Result<(Value, String)> {
    let mut value = serde_json::to_value(storage)?;
    match projection {
        Some(projection) => {
            validate_public(projection)?;
            crate::domain::meta::validate(storage)?;
            value["privacy"] = projection.clone();
            let text = format!("{}\n", serde_json::to_string_pretty(&value)?);
            Ok((value, text))
        }
        None => Ok((value, crate::domain::meta::to_text(storage)?)),
    }
}

/// Project session metadata with an explicit, policy-controlled field allowlist.
pub fn project_meta(
    policy: &PrivacyPolicy,
    aliases: &mut PathAliasStore,
    meta: &Meta,
    _branch: Option<&str>,
) -> Result<MetadataProjection> {
    let mut value = Map::new();
    value.insert("schema_version".into(), json!(1));
    value.insert("line".into(), json!(line_name(meta.line)));
    value.insert("kind".into(), json!(kind_name(meta.kind)));
    if !meta.runtime.is_empty() {
        value.insert("runtime".into(), json!(meta.runtime));
    }
    if let Some(turn) = meta.turn {
        value.insert("turn".into(), json!(turn));
    }
    if let Some(completeness) = meta.completeness {
        value.insert(
            "completeness".into(),
            json!(completeness_name(completeness)),
        );
    }
    if !meta.cwd.is_empty() {
        let mut roots = Vec::with_capacity(policy.external_roots.len() + 1);
        if let Some(workspace) = &policy.workspace {
            roots.push(AliasRoot::new("workspace", &workspace.to_string_lossy())?);
        }
        for root in &policy.external_roots {
            roots.push(AliasRoot::new(&root.label, &root.path.to_string_lossy())?);
        }
        let logical_path = aliases.alias_for(&meta.cwd, &roots)?;
        value.insert("workspace".into(), json!(logical_path));
    }
    if policy.metadata == MetadataMode::Project
        && let Some(state) = &meta.cwd_state
    {
        if let Some(origin) = state
            .origin
            .as_deref()
            .and_then(crate::domain::meta::sanitize_git_origin)
        {
            value.insert("origin".into(), json!(origin));
        }
        value.insert("worktree".into(), json!(worktree_name(state.worktree)));
        value.insert("staged".into(), json!(state.staged));
        value.insert("unstaged".into(), json!(state.unstaged));
        value.insert("untracked".into(), json!(state.untracked));
        value.insert("conflicted".into(), json!(state.conflicted));
    }
    Ok(MetadataProjection {
        schema_version: 1,
        value: Value::Object(value),
    })
}

fn line_name(line: Line) -> &'static str {
    match line {
        Line::Session => "session",
        Line::File => "file",
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Turn => "turn",
        Kind::Merge => "merge",
        Kind::View => "view",
        Kind::File => "file",
        Kind::Archive => "archive",
    }
}

fn completeness_name(value: Completeness) -> &'static str {
    match value {
        Completeness::Exact => "exact",
        Completeness::Partial => "partial",
        Completeness::Unknown => "unknown",
    }
}

fn worktree_name(value: WorktreeStatus) -> &'static str {
    match value {
        WorktreeStatus::Clean => "clean",
        WorktreeStatus::Dirty => "dirty",
        WorktreeStatus::Conflicted => "conflicted",
        WorktreeStatus::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::meta::{CwdState, WorktreeStatus};
    #[test]
    fn minimal_metadata_omits_raw_identity_and_project_fields_are_sanitized() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("src/main.rs");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let mut meta = Meta::new(
            "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "codex".into(),
            root.path().display().to_string(),
        );
        meta.cwd_state = Some(CwdState {
            origin: Some("https://alice:secret@example.test/team/app.git?token=bad".into()),
            head: Some("abcdef".into()),
            branch: Some("customer-work".into()),
            worktree: WorktreeStatus::Dirty,
            staged: 1,
            unstaged: 2,
            untracked: 3,
            conflicted: 0,
            status_digest: Some("private-status".into()),
        });
        let mut aliases = PathAliasStore::default();
        let minimal = PrivacyPolicy {
            workspace: Some(root.path().to_path_buf()),
            ..PrivacyPolicy::default()
        };
        let minimal_value = project_meta(&minimal, &mut aliases, &meta, None)
            .unwrap()
            .value;
        assert!(minimal_value.get("workspace").is_some());
        assert!(minimal_value.get("cwd").is_none());
        assert!(minimal_value.get("session").is_none());
        assert!(minimal_value.get("origin").is_none());
        let project = PrivacyPolicy {
            metadata: MetadataMode::Project,
            ..minimal
        };
        let project_value = project_meta(&project, &mut aliases, &meta, None)
            .unwrap()
            .value;
        let origin = project_value.get("origin").unwrap().as_str().unwrap();
        assert!(!origin.contains("secret"));
        assert!(
            !serde_json::to_string(&project_value)
                .unwrap()
                .contains("private-status")
        );
        assert!(project_value.get("branch").is_none());
        let synthetic = Meta::new(
            "agit-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            "claude-code".into(),
            String::new(),
        );
        let (public, text) = git_metadata(&synthetic, Some(&project_value)).unwrap();
        assert_eq!(public["privacy"], project_value);
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), public);
        assert!(serde_json::from_str::<Meta>(&text).unwrap().cwd.is_empty());
        assert_eq!(
            git_metadata(&synthetic, None).unwrap().1,
            crate::domain::meta::to_text(&synthetic).unwrap()
        );
        let mut unrecognized = project_value;
        unrecognized["cwd"] = json!("/private/source");
        assert!(git_metadata(&synthetic, Some(&unrecognized)).is_err());
    }
}
