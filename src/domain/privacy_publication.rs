//! Frozen session projection shared by privacy-aware publication entry points.
//!
//! Native records are inputs, never public output. Public records use recognized message fields;
//! unsupported records and tool content without an authorized source remain private.

use super::{
    meta::{self, Meta},
    privacy::{CandidateAction, PrivacyPolicy},
    privacy_envelope::{PrivacyEnvelope, ViewingRecipient, digest_json},
    privacy_layer::{EVIDENCE_FIELD, EVIDENCE_PREFIX, EvidenceReference, PrivateLayer},
    privacy_metadata,
    privacy_paths::{PathAliasStore, decode_file_uri, normalize_path},
    redact::Redactor,
    storage,
    transcript::{self, Envelope},
};
use anyhow::{Context, Result, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use zeroize::Zeroizing;

mod snapshot;
pub(crate) use snapshot::PublicationSnapshot;

const PUBLIC_SESSION: &str = "agit-0000000000000000000000000000000000000001";
const OMITTED: &str = "[Content omitted by privacy policy]";

/// Per-sequence source admission budget, before secret hydration and private-layer encoding.
pub const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;

/// Admission bounds the source before JSON parsing, hydration and publication copies.
pub(crate) fn check_input(log: &str, view: &str) -> Result<()> {
    ensure!(
        log.len() <= MAX_INPUT_BYTES && view.len() <= MAX_INPUT_BYTES,
        "session exceeds the privacy preparation input budget"
    );
    Ok(())
}

#[cfg(feature = "cli")]
pub(crate) fn wrap_native(text: &str, runtime: &str, session: &str) -> Result<String> {
    check_input(text, "")?;
    let mut wrapped = String::new();
    for line in text.lines() {
        let record = transcript::wrap_lines(line, runtime, session);
        ensure!(
            record.len() <= MAX_INPUT_BYTES.saturating_sub(wrapped.len()),
            "session exceeds the privacy preparation input budget"
        );
        wrapped.push_str(&record);
    }
    Ok(wrapped)
}

/// Both sequences come from the same immutable local commit under the publication input budget.
pub(crate) fn read_snapshot(
    repo: &super::repo::Repo,
    commit: &str,
) -> Result<(Meta, String, String)> {
    let metadata = storage::metadata_local(repo.root(), commit)?;
    ensure!(
        metadata.is_session_line(),
        "privacy publication requires a session snapshot"
    );
    let (log, view) =
        storage::materialize_pair_local(repo.root(), commit, MAX_INPUT_BYTES, MAX_INPUT_BYTES)
            .context("cannot read session within the privacy preparation input budget")?;
    Ok((metadata, log, view))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionNotice {
    /// Position in the complete source LOG, independent of the selected VIEW.
    pub record: usize,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionReport {
    pub records: usize,
    pub omissions: Vec<ProjectionNotice>,
    pub replacements: usize,
    pub secret_matches: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<ProjectionDetails>,
}

impl ProjectionReport {
    pub fn validate(&self, policy_digest: &str) -> Result<()> {
        ensure!(
            self.omissions
                .iter()
                .all(|notice| notice.record < self.records),
            "invalid omission record"
        );
        if let Some(details) = &self.details {
            ensure!(
                details.policy_version == super::privacy::POLICY_VERSION
                    && details.policy_digest == policy_digest,
                "processing report policy binding differs from publication"
            );
            ensure!(
                details.decisions.iter().all(|decision| decision
                    .record
                    .is_none_or(|record| record < self.records)
                    && decision.matches > 0),
                "invalid processing decision position or count"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionDetails {
    pub policy_version: u32,
    pub policy_digest: String,
    pub decisions: Vec<ProjectionDecision>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionDecision {
    /// A source LOG position, or metadata when absent.
    pub record: Option<usize>,
    pub path: Option<String>,
    pub rule: String,
    pub action: ProjectionAction,
    pub matches: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionAction {
    AllowPath,
    ExcludeSource,
    RewriteText,
    MaskSecret,
}

type DecisionKey = (Option<usize>, Option<String>, String, ProjectionAction);

/// The public and private halves are produced together from the same validated snapshot.
#[derive(Serialize, Deserialize)]
pub struct SessionProjection {
    #[serde(with = "private_text")]
    source_log: Zeroizing<String>,
    log: String,
    view: String,
    metadata: Value,
    report: ProjectionReport,
    policy_digest: String,
    private: PrivateLayer,
    dependencies: ProjectionDependencies,
}

mod private_text {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &Zeroizing<String>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        value.as_str().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Zeroizing<String>, D::Error> {
        String::deserialize(deserializer).map(Zeroizing::new)
    }
}

#[derive(Serialize, Deserialize)]
struct FrozenInput {
    branch: Option<String>,
    log: String,
    view: String,
    metadata: Meta,
    additional: Vec<super::privacy::mandatory::MandatoryPolicy>,
}

pub(crate) fn project_worker(
    root: Option<&Path>,
    text: &str,
) -> Result<super::privacy::projector::Outcome> {
    let input: FrozenInput = serde_json::from_str(text)?;
    let repo = root.map(super::repo::Repo::at);
    let projection = project_frozen_local(
        repo.as_ref(),
        input.branch.as_deref(),
        &input.log,
        &input.view,
        &input.metadata,
        &input.additional,
    )?;
    Ok(super::privacy::projector::Outcome {
        content: serde_json::to_string(&projection)?,
        status: super::privacy::projector::Status::Complete,
        replacements: projection.report.replacements,
        unresolved: 0,
        consumed: None,
    })
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectionDependencies {
    paths: BTreeMap<String, String>,
    aliases: BTreeMap<String, String>,
    unstable: bool,
    reserved_through: u64,
}

impl ProjectionDependencies {
    pub(crate) fn matches(
        &self,
        policy: &PrivacyPolicy,
        aliases: &PathAliasStore,
        branch: &str,
    ) -> Result<bool> {
        if self.unstable || aliases.reservation_boundary() < self.reserved_through {
            return Ok(false);
        }
        let current = aliases.private_mappings();
        if self
            .aliases
            .iter()
            .any(|(alias, source)| current.get(alias) != Some(source))
        {
            return Ok(false);
        }
        for (path, expected) in &self.paths {
            if digest_json(&serde_json::to_value(
                policy.evaluate_file(Path::new(path), Some(branch)),
            )?)? != *expected
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl SessionProjection {
    pub(crate) fn dependencies(&self) -> &ProjectionDependencies {
        &self.dependencies
    }
    pub fn log(&self) -> &str {
        &self.log
    }

    pub fn view(&self) -> &str {
        &self.view
    }

    pub fn report(&self) -> &ProjectionReport {
        &self.report
    }

    /// Generated identities are not content. The scanner still sees every message body
    /// and public metadata field, while ciphertext remains a separately validated carrier.
    #[cfg(test)]
    pub(crate) fn inspection_text(&self) -> Result<String> {
        public_inspection_text(
            self.public_value(),
            &self.log,
            &self.view,
            &self.policy_digest,
        )
    }

    /// Select public events by their original coordinates, so rewritten omission markers cannot
    /// change the meaning of a user's turn range or widen a VIEW-only export.
    pub fn select(&self, original: &str) -> Result<String> {
        let mapped = self
            .source_log
            .split_inclusive('\n')
            .zip(self.log.split_inclusive('\n'))
            .collect::<BTreeMap<_, _>>();
        let mut selected = String::new();
        for line in original.split_inclusive('\n') {
            selected.push_str(
                mapped
                    .get(line)
                    .context("selection is outside the frozen LOG")?,
            );
        }
        Ok(selected)
    }

    pub fn public_value(&self) -> Value {
        json!({
            "version": 1,
            "session": {"log": self.log, "view": self.view},
            "metadata": self.metadata,
            "report": self.report,
        })
    }

    pub fn share_public_value(&self, protected: &str, presentation: &str) -> Value {
        json!({
            "version": 1,
            "kind": "share",
            "session": {"log": protected, "view": protected},
            "metadata": self.metadata,
            "presentation": presentation,
        })
    }

    /// Encode a public Git snapshot without native identities or unprojected metadata.
    /// The caller allocates the public session identity once in device-local publication state.
    pub(crate) fn prepare_git(&mut self, session: &str) -> Result<BTreeMap<String, Vec<u8>>> {
        let original: Meta = serde_json::from_value(self.private.metadata.clone())?;
        let public = if original.is_file_line() {
            Meta::new_file_line()
        } else {
            ensure!(meta::is_bare_id(session), "invalid public session identity");
            let rebind = |text: &str| -> Result<String> {
                Ok(storage::parse_envelopes(text)?
                    .into_iter()
                    .map(|mut envelope| {
                        envelope.session_id = session.into();
                        storage::envelope_line(&envelope)
                    })
                    .collect())
            };
            self.log = rebind(&self.log)?;
            self.view = rebind(&self.view)?;
            let mut public = Meta::new(session.into(), "claude-code".into(), String::new());
            public.kind = original.kind;
            public.turn = original.turn;
            public
        };
        let (metadata, text) = privacy_metadata::git_metadata(&public, Some(&self.metadata))?;
        self.metadata = metadata;
        let mut files = if public.is_file_line() {
            BTreeMap::new()
        } else {
            storage::snapshot_files(&self.log, &self.view)?
        };
        files.insert(meta::FILE.into(), text.into_bytes());
        Ok(files)
    }

    /// This fingerprint stays local: hashing original content into a public identifier would
    /// let someone test guesses against private bytes.
    pub(crate) fn private_fingerprint(&self) -> Result<String> {
        use zeroize::Zeroizing;
        let bytes = Zeroizing::new(serde_json::to_vec(&self.private)?);
        Ok(super::privacy_envelope::digest_bytes(&bytes))
    }

    /// The digest commits to public bytes; plaintext hashes are not publication identifiers.
    pub fn seal(&self, recipient: &ViewingRecipient) -> Result<PrivacyEnvelope> {
        let public = self.public_value();
        PrivacyEnvelope::seal_layer(
            self.policy_digest.clone(),
            digest_json(&public)?,
            public,
            &self.private,
            recipient,
            Vec::new(),
        )
    }

    /// Selection limits both halves of a share. Full LOG provenance may authorize a selected
    /// tool result, but unselected events and reverse mappings must not enter its private layer.
    pub fn seal_share(
        &self,
        selected: &str,
        protected: &str,
        presentation: &str,
        recipient: &ViewingRecipient,
    ) -> Result<PrivacyEnvelope> {
        self.select(selected)?;
        storage::snapshot_files(protected, protected)?;
        ensure!(
            selected.lines().count() == protected.lines().count(),
            "share protection changed the selected event count"
        );
        let aliases = self
            .private
            .path_aliases
            .iter()
            .filter(|(alias, _)| protected.contains(alias.as_str()))
            .map(|(alias, source)| (alias.clone(), source.clone()))
            .collect();
        let (log, _) = self.private.session_bytes()?;
        let mapped = self
            .source_log
            .split_inclusive('\n')
            .zip(log.split_inclusive('\n'))
            .collect::<BTreeMap<_, _>>();
        let mut original = Zeroizing::new(String::new());
        for line in selected.split_inclusive('\n') {
            original.push_str(
                mapped
                    .get(line)
                    .context("selection is outside the frozen LOG")?,
            );
        }
        let mut private =
            PrivateLayer::new(&original, &original, self.private.metadata.clone(), aliases)?;
        let values = storage::parse_envelopes(&original)?
            .into_iter()
            .map(|record| record.content)
            .chain(std::iter::once(self.private.metadata.clone()))
            .collect::<Vec<_>>();
        private.protected_values = self
            .private
            .protected_values
            .iter()
            .filter(|secret| values.iter().any(|value| contains_value(value, secret)))
            .cloned()
            .collect();
        let public = self.share_public_value(protected, presentation);
        PrivacyEnvelope::seal_layer(
            self.policy_digest.clone(),
            digest_json(&public)?,
            public,
            &private,
            recipient,
            Vec::new(),
        )
    }
}

fn contains_value(value: &Value, secret: &str) -> bool {
    match value {
        Value::String(value) => value.contains(secret),
        Value::Array(values) => values.iter().any(|value| contains_value(value, secret)),
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| key.contains(secret) || contains_value(value, secret)),
        _ => false,
    }
}

pub(crate) fn public_inspection_text(
    mut public: Value,
    log: &str,
    view: &str,
    policy_digest: &str,
) -> Result<String> {
    let report: ProjectionReport = serde_json::from_value(public["report"].clone())?;
    report.validate(policy_digest)?;
    // Only the verified generated binding is omitted from content scanning; decision paths remain.
    if let Some(details) = public["report"]["details"].as_object_mut() {
        details.remove("policy_digest");
    }
    if let Some(metadata) = public["metadata"].as_object_mut() {
        metadata.remove("session");
    }
    for (name, text) in [("log", log), ("view", view)] {
        let records = storage::parse_envelopes(text)?
            .into_iter()
            .map(|record| json!({"source":record.source,"content":record.content}))
            .collect::<Vec<_>>();
        public["session"][name] = json!(records);
    }
    Ok(serde_json::to_string(&public)?)
}

/// Read one immutable local snapshot and apply the device's repository policy and secret rules.
pub fn project_at(
    repo: &super::repo::Repo,
    commit: &str,
    branch: Option<&str>,
) -> Result<SessionProjection> {
    ensure!(
        matches!(commit.len(), 40 | 64) && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "privacy projection requires an immutable commit ID"
    );
    let repo = repo.clone().local_objects_only();
    let (metadata, log, view) = read_snapshot(&repo, commit)?;
    project_frozen(Some(&repo), branch, &log, &view, &metadata)
}

/// Process captured session bytes under the selected repository's local policy. Unclaimed
/// sessions use the default exclusions and have no authorized workspace or external roots.
pub fn project_frozen(
    repo: Option<&super::repo::Repo>,
    branch: Option<&str>,
    log: &str,
    view: &str,
    metadata: &Meta,
) -> Result<SessionProjection> {
    project_frozen_with_sources(repo, branch, log, view, metadata, &[])
}

/// Authenticated exclusions narrow the same local policy used by offline projection.
pub fn project_frozen_with_sources(
    repo: Option<&super::repo::Repo>,
    branch: Option<&str>,
    log: &str,
    view: &str,
    metadata: &Meta,
    additional: &[super::privacy::mandatory::MandatoryPolicy],
) -> Result<SessionProjection> {
    let input = FrozenInput {
        branch: branch.map(str::to_owned),
        log: log.into(),
        view: view.into(),
        metadata: metadata.clone(),
        additional: additional.to_vec(),
    };
    if let Ok(text) = serde_json::to_string(&input) {
        let outcome = super::privacy::service::transform(
            repo.map(|repo| repo.root()),
            &text,
            super::privacy::projector::Mode::ProjectPublication,
        );
        if outcome.status == super::privacy::projector::Status::Complete
            && let Ok(projection) = serde_json::from_str::<SessionProjection>(&outcome.content)
            && projection.source_log.as_str() == log
            && projection.private.validate().is_ok()
            && storage::snapshot_files(&projection.log, &projection.view).is_ok()
            && projection
                .report
                .validate(&projection.policy_digest)
                .is_ok()
        {
            return Ok(projection);
        }
    }
    // Default projection has no filesystem policy dependencies; original records remain in the envelope.
    let policy = PrivacyPolicy::default();
    project_session(
        &policy,
        &mut PathAliasStore::default(),
        branch,
        log,
        view,
        metadata,
        &Redactor::new(Default::default()),
    )
}

fn project_frozen_local(
    repo: Option<&super::repo::Repo>,
    branch: Option<&str>,
    log: &str,
    view: &str,
    metadata: &Meta,
    additional: &[super::privacy::mandatory::MandatoryPolicy],
) -> Result<SessionProjection> {
    let mut policy = repo.map_or_else(PrivacyPolicy::load_default, PrivacyPolicy::load)?;
    policy.mandatory.extend_from_slice(additional);
    policy.validate()?;
    if branch.is_none() {
        // Detached content has no branch authority and must retain every narrowing rule.
        for restriction in policy.branches.values() {
            policy.exclude.extend(restriction.exclude.iter().cloned());
        }
    }
    let redactor = Redactor::try_this_machine()?;
    if let Some(repo) = repo {
        let redactor = redactor.with_repository(repo.root())?;
        let snapshot = PublicationSnapshot::capture(repo, log, view, metadata)?;
        PathAliasStore::transact(repo, |aliases| {
            snapshot.project(&policy, aliases, branch, &redactor)
        })
    } else {
        project_session(
            &policy,
            &mut PathAliasStore::default(),
            branch,
            log,
            view,
            metadata,
            &redactor,
        )
    }
}

#[derive(Clone)]
struct FileCall {
    id: String,
    allowed: bool,
    ambiguous: bool,
}

struct Projector<'a> {
    policy: &'a PrivacyPolicy,
    aliases: &'a mut PathAliasStore,
    branch: Option<&'a str>,
    redactor: &'a Redactor,
    replacements: Vec<Regex>,
    paths: Regex,
    report: ProjectionReport,
    record: Option<usize>,
    decisions: BTreeMap<DecisionKey, usize>,
    reporting: bool,
    used_aliases: BTreeSet<String>,
    dependencies: ProjectionDependencies,
    /// OpenCode emits parts separately from their parent message.  Keep the native role
    /// association so a reasoning part cannot create a synthetic user turn.
    message_roles: BTreeMap<(String, String, String), String>,
}

/// Freeze and project a complete LOG/VIEW pair. VIEW membership is preserved by transforming
/// each LOG event once, then selecting those exact public bytes for the VIEW.
pub fn project_session(
    policy: &PrivacyPolicy,
    aliases: &mut PathAliasStore,
    branch: Option<&str>,
    log: &str,
    view: &str,
    metadata: &Meta,
    redactor: &Redactor,
) -> Result<SessionProjection> {
    check_input(log, view)?;
    PrivateLayer::check_session_size(log.len(), view.len())?;
    policy.validate()?;
    meta::validate(metadata)?;
    storage::snapshot_files(log, view)?;
    aliases.reserve_aliases(log)?;
    let records = storage::parse_envelopes(log)?;
    let mut projector = Projector::new(policy, aliases, branch, redactor, records.len())?;
    for record in &records {
        projector.remember_native_role(record);
    }
    // Native IDs are scoped by runtime and source session. Ambiguous reuse cannot authorize
    // either output, even when one of the calls names an allowed file.
    let mut calls: BTreeMap<(String, String, String), FileCall> = BTreeMap::new();
    let mut contexts = BTreeMap::<(String, String), String>::new();
    let mut cwd_at = Vec::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        projector.record = Some(index);
        let group = (record.source.clone(), record.session_id.clone());
        if let Some(cwd) = record_cwd(record) {
            contexts.insert(group.clone(), cwd.to_owned());
        }
        let cwd = contexts.get(&group).cloned();
        for (id, name, input) in tool_calls(record) {
            let key = (group.0.clone(), group.1.clone(), id.to_owned());
            if let Some(existing) = calls.get_mut(&key) {
                existing.ambiguous = true;
                existing.allowed = false;
            } else {
                let allowed = projector.allow_call(&name, &input, cwd.as_deref())?;
                calls.insert(
                    key,
                    FileCall {
                        id: format!("call-{index}-{}", calls.len()),
                        allowed,
                        ambiguous: false,
                    },
                );
            }
        }
        cwd_at.push(cwd);
    }
    let mut mapped = BTreeMap::new();
    let mut projected_evidence = BTreeMap::new();
    let mut public_log = String::new();
    for (index, record) in records.iter().enumerate() {
        projector.record = Some(index);
        let content = match evidence_reference(record) {
            Some(reference) => {
                if let Some(source) = reference
                    .filter(|reference| reference.version == 1)
                    .and_then(|reference| projected_evidence.get(&reference.key()))
                {
                    json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":format!("Recovered historical session evidence (policy-projected).\n{source}")}]}})
                } else {
                    json!({"type":"user","message":{"role":"user","content":[projector.omit("recovered evidence source is unavailable")]}})
                }
            }
            None => projector.record(record, cwd_at[index].as_deref(), &calls)?,
        };
        projected_evidence.insert(
            EvidenceReference::from_record(record).key(),
            content.clone(),
        );
        let envelope = Envelope {
            source: "claude-code".into(),
            session_id: PUBLIC_SESSION.into(),
            object_hash: transcript::object_hash(&content),
            content,
        };
        let line = storage::envelope_line(&envelope);
        let original = storage::envelope_line(record);
        if let Some(previous) = mapped.insert(original, line.clone()) {
            ensure!(
                previous == line,
                "repeated event has ambiguous privacy provenance"
            );
        }
        public_log.push_str(&line);
    }
    let mut public_view = String::new();
    for line in view.split_inclusive('\n') {
        public_view.push_str(mapped.get(line).context("VIEW is outside the frozen LOG")?);
    }
    storage::snapshot_files(&public_log, &public_view)?;
    projector.record = None;
    let public_metadata =
        privacy_metadata::project_meta(policy, projector.aliases, metadata, branch)?.value;
    if let Some(alias) = public_metadata["workspace"].as_str() {
        projector.used_aliases.insert(alias.into());
    }
    let public_metadata = projector.rewrite(&public_metadata)?;
    projector.finish_report()?;
    let mut private = PrivateLayer::new(
        log,
        view,
        serde_json::to_value(metadata)?,
        projector
            .aliases
            .private_mappings()
            .into_iter()
            .filter(|(alias, _)| projector.used_aliases.contains(alias))
            .collect(),
    )?;
    private.protected_values = redactor.publication_values_matching(|secret| {
        contains_value(&private.metadata, secret)
            || records
                .iter()
                .any(|record| contains_value(&record.content, secret))
    })?;
    private.validate()?;
    projector.dependencies.aliases = private.path_aliases.clone();
    projector.dependencies.reserved_through = projector.aliases.reservation_boundary();
    Ok(SessionProjection {
        source_log: Zeroizing::new(log.to_owned()),
        log: public_log,
        view: public_view,
        metadata: public_metadata,
        report: projector.report,
        policy_digest: policy.digest()?,
        private,
        dependencies: projector.dependencies,
    })
}

fn evidence_reference(record: &Envelope) -> Option<Option<EvidenceReference>> {
    if let Some(reference) = record.content.get(EVIDENCE_FIELD) {
        return Some(serde_json::from_value(reference.clone()).ok());
    }
    // Legacy evidence quotes retain their native record in the text. A malformed or unmatched
    // quote remains evidence and must not fall through to ordinary message publication.
    let content = if record.content["type"] == "response_item" {
        &record.content["payload"]["content"]
    } else {
        &record.content["message"]["content"]
    };
    let texts = match content {
        Value::String(text) => vec![text.as_str()],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect(),
        _ => Vec::new(),
    };
    let evidence = texts.into_iter().find_map(|text| {
        text.split_once(EVIDENCE_PREFIX)
            .map(|(_, evidence)| evidence)
    })?;
    Some((|| {
        let evidence: Value = serde_json::from_str(evidence).ok()?;
        let content = evidence.get("record")?;
        Some(EvidenceReference {
            version: 1,
            source: evidence.get("runtime")?.as_str()?.into(),
            session: evidence.get("session")?.as_str()?.into(),
            object_hash: transcript::object_hash(content),
        })
    })())
}

impl<'a> Projector<'a> {
    fn new(
        policy: &'a PrivacyPolicy,
        aliases: &'a mut PathAliasStore,
        branch: Option<&'a str>,
        redactor: &'a Redactor,
        records: usize,
    ) -> Result<Self> {
        Ok(Self {
            policy,
            aliases,
            branch,
            redactor,
            replacements: policy
                .replacements
                .iter()
                .map(|rule| {
                    Regex::new(
                        if rule.regex {
                            rule.pattern.clone()
                        } else {
                            regex::escape(&rule.pattern)
                        }
                        .as_str(),
                    )
                    .context("invalid privacy replacement expression")
                })
                .collect::<Result<_>>()?,
            paths: Regex::new(
                r#"(?:file://[^\s<>\"'`]+|[A-Za-z]:[\\/][^\s<>\"'`]+|\\\\[^\s<>\"'`]+|/[^\s<>\"'`]+)"#,
            )?,
            report: ProjectionReport {
                records,
                ..Default::default()
            },
            record: None,
            decisions: BTreeMap::new(),
            reporting: true,
            used_aliases: BTreeSet::new(),
            dependencies: ProjectionDependencies::default(),
            message_roles: BTreeMap::new(),
        })
    }
}

impl Projector<'_> {
    fn note(&mut self, rule: String, action: ProjectionAction, count: usize) {
        if self.reporting && count > 0 {
            *self
                .decisions
                .entry((self.record, None, rule, action))
                .or_default() += count;
        }
    }

    fn finish_report(&mut self) -> Result<()> {
        let pending = std::mem::take(&mut self.decisions);
        let mut decisions = Vec::with_capacity(pending.len());
        self.reporting = false;
        for ((record, path, rule, action), matches) in pending {
            // Report aliases undergo the same rewrites as content; policy patterns stay local.
            let path = path
                .map(|path| {
                    self.rewrite(&json!(path)).and_then(|value| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .context("report path must remain text")
                    })
                })
                .transpose()?;
            decisions.push(ProjectionDecision {
                record,
                path,
                rule,
                action,
                matches,
            });
        }
        self.reporting = true;
        self.report.details = Some(ProjectionDetails {
            policy_version: self.policy.version,
            policy_digest: self.policy.digest()?,
            decisions,
        });
        self.report.validate(&self.policy.digest()?)?;
        Ok(())
    }

    fn file(&mut self, path: &Path) -> Result<super::privacy_paths::PathProjection> {
        let (projection, decision) =
            self.aliases
                .project_file_with_decision(self.policy, path, self.branch)?;
        let fingerprint = digest_json(&serde_json::to_value(&decision)?)?;
        if self.reporting {
            if let Some(alias) = &projection.logical_path {
                self.used_aliases.insert(alias.clone());
            }
            self.decisions
                .entry((
                    self.record,
                    projection.logical_path.clone(),
                    decision.rule.clone(),
                    if decision.action == CandidateAction::Allowed {
                        ProjectionAction::AllowPath
                    } else {
                        ProjectionAction::ExcludeSource
                    },
                ))
                .or_insert(1);
        }
        if let Some(previous) = self
            .dependencies
            .paths
            .insert(path.to_string_lossy().into_owned(), fingerprint.clone())
        {
            self.dependencies.unstable |= previous != fingerprint;
        }
        Ok(projection)
    }
    fn omit(&mut self, reason: &'static str) -> Value {
        self.report.omissions.push(ProjectionNotice {
            record: self.record.expect("omission belongs to a source record"),
            reason: reason.into(),
        });
        json!({"type":"text", "text": OMITTED})
    }

    fn allow_call(&mut self, name: &str, input: &Value, cwd: Option<&str>) -> Result<bool> {
        if !is_file_tool(name) {
            return Ok(false);
        }
        let Some(input) = input.as_object() else {
            return Ok(false);
        };
        if input.keys().any(|key| {
            !matches!(
                key.as_str(),
                "file_path"
                    | "path"
                    | "offset"
                    | "limit"
                    | "old_string"
                    | "new_string"
                    | "replace_all"
                    | "content"
            )
        }) {
            return Ok(false);
        }
        let paths = [input.get("file_path"), input.get("path")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if paths.len() != 1 {
            return Ok(false);
        }
        let Some(path) = paths[0].as_str() else {
            return Ok(false);
        };
        let path = if absolute_path(path) {
            path.to_owned()
        } else if let Some(cwd) = cwd {
            format!("{cwd}/{path}")
        } else {
            return Ok(false);
        };
        let path = decode_file_uri(&path)?.replace('\\', "/");
        // A foreign-platform absolute path is historical evidence, not a file under this device's cwd.
        if !Path::new(&path).is_absolute() {
            return Ok(false);
        }
        Ok(self.file(Path::new(&path))?.action == CandidateAction::Allowed)
    }

    fn remember_native_role(&mut self, record: &Envelope) {
        let value = &record.content;
        let (id, role) = match record.source.as_str() {
            "opencode" if value["kind"] == "message" => {
                (value["id"].as_str(), value["data"]["role"].as_str())
            }
            "openclaw" if value["type"] == "message" => {
                (value["id"].as_str(), value["message"]["role"].as_str())
            }
            _ => (None, None),
        };
        if let (Some(id), Some(role)) = (id, role)
            && matches!(role, "user" | "assistant")
        {
            self.message_roles.insert(
                (record.source.clone(), record.session_id.clone(), id.into()),
                role.into(),
            );
        }
    }

    fn native_message(&self, record: &Envelope) -> Option<(String, Vec<Value>)> {
        let value = &record.content;
        match record.source.as_str() {
            "claude-code" | "cursor" if message_role(record).is_some() => {
                let blocks = native_content_blocks(&value["message"]["content"]);
                let role = assistant_for_reasoning(message_role(record)?.into(), &blocks);
                Some((role, blocks))
            }
            "claude-desktop"
                if matches!(
                    value["message"]["role"].as_str(),
                    Some("user" | "assistant")
                ) =>
            {
                let blocks = native_content_blocks(&value["message"]["content"]);
                Some((
                    assistant_for_reasoning(value["message"]["role"].as_str()?.into(), &blocks),
                    blocks,
                ))
            }
            "codex" if value["type"] == "response_item" => {
                let payload = &value["payload"];
                match payload["type"].as_str() {
                    Some("message") => {
                        let blocks = native_content_blocks(&payload["content"]);
                        Some((
                            assistant_for_reasoning(
                                payload["role"].as_str().unwrap_or("user").into(),
                                &blocks,
                            ),
                            blocks,
                        ))
                    }
                    Some("reasoning") => Some((
                        "assistant".into(),
                        vec![reasoning_block(&codex_reasoning_text(payload))],
                    )),
                    Some("function_call") => Some((
                        "assistant".into(),
                        vec![json!({
                            "type":"tool_use", "id":payload["call_id"],
                            "name":payload["name"], "input":decode_arguments(payload)
                        })],
                    )),
                    Some("function_call_output") => Some((
                        "user".into(),
                        vec![json!({
                            "type":"tool_result", "tool_use_id":payload["call_id"],
                            "content":payload["output"], "is_error":false
                        })],
                    )),
                    _ => None,
                }
            }
            "codex"
                if value["type"] == "event_msg"
                    && value["payload"]["type"] == "agent_reasoning" =>
            {
                Some((
                    "assistant".into(),
                    vec![reasoning_block(&codex_reasoning_text(&value["payload"]))],
                ))
            }
            "opencode" if value["kind"] == "part" => {
                let data = &value["data"];
                let role = value["message_id"]
                    .as_str()
                    .and_then(|id| {
                        self.message_roles.get(&(
                            record.source.clone(),
                            record.session_id.clone(),
                            id.into(),
                        ))
                    })
                    .cloned()
                    .unwrap_or_else(|| "assistant".into());
                match data["type"].as_str() {
                    Some("reasoning") => Some((
                        "assistant".into(),
                        vec![reasoning_block(&reasoning_block_text(data))],
                    )),
                    Some("text") => Some((
                        role,
                        vec![
                            json!({"type":"text", "text":data["text"].as_str().unwrap_or_default()}),
                        ],
                    )),
                    _ => None,
                }
            }
            "opencode" if value["kind"] == "message" => {
                let role = value["data"]["role"].as_str()?.to_owned();
                let blocks = native_content_blocks(&value["data"]["content"]);
                (!blocks.is_empty()).then_some((role, blocks))
            }
            "hermes" if value["type"] == "hermes_message" => {
                let data = &value["data"];
                if data["role"] == "tool" {
                    return Some((
                        "user".into(),
                        vec![json!({
                            "type":"tool_result",
                            "tool_use_id":data["tool_call_id"].as_str().unwrap_or_default(),
                            "content":data["content"].clone(),
                            "is_error":data["effect_disposition"] == "error"
                        })],
                    ));
                }
                let reasoning = if hermes_has_reasoning(data) {
                    hermes_reasoning_texts(data)
                } else {
                    Vec::new()
                };
                let mut blocks: Vec<Value> = reasoning
                    .into_iter()
                    .map(|text| reasoning_block(&text))
                    .collect();
                if blocks.is_empty() && hermes_has_reasoning_fields(data) {
                    blocks.push(reasoning_block(""));
                }
                blocks.extend(native_content_blocks(&data["content"]));
                blocks.extend(hermes_tool_blocks(data));
                let role = match data["role"].as_str() {
                    Some("assistant") => "assistant",
                    Some("user") if !blocks.iter().any(is_reasoning_block) => "user",
                    Some("tool") => "user",
                    _ => "assistant",
                };
                (!blocks.is_empty()).then_some((role.into(), blocks))
            }
            "openclaw" if matches!(value["type"].as_str(), Some("message" | "custom_message")) => {
                let body = if value["type"] == "message" {
                    &value["message"]
                } else {
                    value
                };
                let role = body["role"].as_str()?;
                if role == "toolResult" {
                    let id = body["toolCallId"].as_str().unwrap_or_default();
                    return Some((
                        "user".into(),
                        vec![json!({
                            "type":"tool_result", "tool_use_id":id,
                            "content":body["content"].clone(), "is_error":body["isError"] == true
                        })],
                    ));
                }
                let blocks = native_content_blocks(&body["content"]);
                if blocks.is_empty() || !matches!(role, "user" | "assistant") {
                    None
                } else if blocks.iter().any(is_reasoning_block) && role != "assistant" {
                    Some(("assistant".into(), blocks))
                } else {
                    Some((role.into(), blocks))
                }
            }
            "workbuddy" if value["type"] == "message" => {
                let role = value["role"].as_str()?.to_owned();
                let mut reasoning = Vec::new();
                for key in ["reasoning", "reasoning_content"] {
                    if let Some(value) = value.get(key) {
                        collect_reasoning_value(value, &mut reasoning);
                    }
                }
                let mut blocks: Vec<Value> = reasoning
                    .into_iter()
                    .map(|text| reasoning_block(&text))
                    .collect();
                blocks.extend(native_content_blocks(&value["content"]));
                if blocks.is_empty() || !matches!(role.as_str(), "user" | "assistant") {
                    None
                } else if blocks.iter().any(is_reasoning_block) && role != "assistant" {
                    Some(("assistant".into(), blocks))
                } else {
                    Some((role, blocks))
                }
            }
            "workbuddy" if value["type"] == "function_call" => Some((
                "assistant".into(),
                vec![json!({
                    "type":"tool_use",
                    "id":value["callId"].as_str().unwrap_or_default(),
                    "name":value["name"].as_str().unwrap_or("tool"),
                    "input":native_tool_input(&value["arguments"])
                })],
            )),
            "workbuddy" if value["type"] == "function_call_result" => Some((
                "user".into(),
                vec![json!({
                    "type":"tool_result",
                    "tool_use_id":value["callId"].as_str().unwrap_or_default(),
                    "content":value["output"].clone(),
                    "is_error":value["isError"] == true || matches!(value["status"].as_str(), Some("failed" | "error"))
                })],
            )),
            "workbuddy" if value["type"] == "reasoning" => Some((
                "assistant".into(),
                vec![reasoning_block(&{
                    let text = reasoning_block_text(value);
                    if text.is_empty() {
                        native_message_text(&value["content"])
                    } else {
                        text
                    }
                })],
            )),
            _ => None,
        }
    }

    fn record(
        &mut self,
        record: &Envelope,
        cwd: Option<&str>,
        calls: &BTreeMap<(String, String, String), FileCall>,
    ) -> Result<Value> {
        let value = &record.content;
        let message = self
            .native_message(record)
            .or_else(|| match record.source.as_str() {
                _ if value["type"] == "user"
                    && value["agit"] == "merge_summary"
                    && value["message"]["role"] == "user"
                    && value["message"]["content"].is_string() =>
                {
                    Some((
                        "user".into(),
                        vec![json!({
                            "type": "text",
                            "text": value["message"]["content"].clone(),
                        })],
                    ))
                }
                _ => None,
            });
        let Some((role, blocks)) = message.filter(|(role, blocks)| {
            matches!(role.as_str(), "user" | "assistant") && !blocks.is_empty()
        }) else {
            return Ok(with_native_classification(
                json!({"type":"user", "message":{"role":"user", "content":[self.omit("unsupported native record")]}}),
                value,
                true,
            ));
        };
        let codex_internal = record.source == "codex" && codex_user_message_is_internal(value);
        let mut output = Vec::new();
        for (block_index, block) in blocks.into_iter().enumerate() {
            let projected = match block["type"].as_str() {
                Some("text" | "input_text" | "output_text") => match block["text"].as_str() {
                    Some(text) => json!({"type":"text", "text":self.rewrite(&json!(text))?}),
                    None => self.omit("unsupported message content"),
                },
                Some("thinking" | "reasoning" | "redacted_thinking") => {
                    if role != "assistant" {
                        self.omit("reasoning is not owned by an assistant message")
                    } else {
                        let text = reasoning_block_text(&block);
                        let text = self.rewrite(&json!(text))?;
                        json!({"type":"thinking", "thinking":text})
                    }
                }
                Some("tool_use" | "tool_result") => {
                    let is_call = block["type"] == "tool_use";
                    let id = block[if is_call { "id" } else { "tool_use_id" }]
                        .as_str()
                        .unwrap_or("");
                    let name = block["name"].as_str().unwrap_or("");
                    let call = (!id.is_empty())
                        .then(|| {
                            calls.get(&(
                                record.source.clone(),
                                record.session_id.clone(),
                                id.into(),
                            ))
                        })
                        .flatten();
                    if is_call {
                        // IDs establish result provenance; each file input needs its own authorization.
                        let allowed = self.allow_call(name, &block["input"], cwd)?
                            && call.is_none_or(|call| call.allowed && !call.ambiguous);
                        if !allowed {
                            self.omit("tool source is excluded, ambiguous, or unknown")
                        } else {
                            let id = call
                                .filter(|call| !call.ambiguous)
                                .map(|call| call.id.clone())
                                .unwrap_or_else(|| {
                                    format!(
                                        "call-{}-block-{block_index}",
                                        self.record.expect("tool call belongs to a source record")
                                    )
                                });
                            let mut input = block["input"].clone();
                            if is_file_tool(name) {
                                for field in ["path", "file_path"] {
                                    if let Some(path) = input[field].as_str() {
                                        let path = if absolute_path(path) {
                                            path.to_owned()
                                        } else {
                                            format!("{}/{path}", cwd.unwrap_or_default())
                                        };
                                        input[field] = json!(self.path_alias(&path)?);
                                    }
                                }
                            }
                            let input = self.without_attachments(&input, ContentContext::Json);
                            json!({
                                "type":"tool_use",
                                "id":id,
                                "name":self.rewrite(&json!(name))?,
                                "input":self.rewrite(&input)?
                            })
                        }
                    } else {
                        if call.is_none_or(|call| !call.allowed || call.ambiguous) {
                            self.omit("tool source is excluded, ambiguous, or unknown")
                        } else {
                            let result_id = call
                                .filter(|call| call.allowed && !call.ambiguous)
                                .map(|call| call.id.clone())
                                .expect("validated tool result has a public call identity");
                            json!({
                                "type":"tool_result",
                                "tool_use_id":result_id,
                                "content":self.result_text(&block["content"])?,
                                "is_error":block["is_error"].as_bool().unwrap_or(false)
                            })
                        }
                    }
                }
                _ if is_attachment(&block, ContentContext::MessageBlock) => {
                    self.omit("attachment body is excluded from public content")
                }
                _ => self.omit("unsupported message content"),
            };
            output.push(projected);
        }
        Ok(with_native_classification(
            json!({"type":role, "message":{"role":role, "content":output}}),
            value,
            codex_internal,
        ))
    }

    fn without_attachments(&mut self, value: &Value, context: ContentContext) -> Value {
        // Attachment boundaries apply before generic JSON traversal, including nested payloads.
        if is_attachment(value, context) {
            self.omit("attachment body is excluded from public content");
            return json!(OMITTED);
        }
        match value {
            Value::Array(values) => Value::Array(
                values
                    .iter()
                    .map(|value| self.without_attachments(value, ContentContext::Json))
                    .collect(),
            ),
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        let value = if matches!(key.as_str(), "attachment" | "attachments") {
                            self.omit("attachment body is excluded from public content");
                            json!(OMITTED)
                        } else {
                            self.without_attachments(value, ContentContext::Json)
                        };
                        (key.clone(), value)
                    })
                    .collect(),
            ),
            _ => value.clone(),
        }
    }

    fn public_text(&mut self, value: &Value, context: ContentContext) -> Result<String> {
        let admitted = self.without_attachments(value, context);
        // Scrub string leaves before JSON escaping can hide a sensitive match.
        match self.rewrite(&admitted)? {
            Value::String(text) => Ok(text),
            rewritten => Ok(serde_json::to_string(&rewritten)?),
        }
    }

    fn result_text(&mut self, value: &Value) -> Result<String> {
        match value {
            Value::Array(blocks) if !blocks.is_empty() && blocks.iter().all(is_message_block) => {
                blocks
                    .iter()
                    .map(|block| {
                        if matches!(
                            block["type"].as_str(),
                            Some("text" | "input_text" | "output_text")
                        ) && block["text"].is_string()
                        {
                            self.public_text(&block["text"], ContentContext::Json)
                        } else {
                            self.public_text(block, ContentContext::MessageBlock)
                        }
                    })
                    .collect::<Result<Vec<_>>>()
                    .map(|blocks| blocks.join("\n"))
            }
            _ => self.public_text(value, ContentContext::Json),
        }
    }

    fn path_alias(&mut self, path: &str) -> Result<String> {
        let Ok(normalized) = normalize_path(path) else {
            self.note(
                "path.normalization".into(),
                ProjectionAction::ExcludeSource,
                1,
            );
            return Ok("<private-path>".into());
        };
        let alias = if Path::new(normalized.as_str()).is_absolute() {
            let candidate = decode_file_uri(path)?.replace('\\', "/");
            self.file(Path::new(&candidate))?
                .logical_path
                .context("path projection has no alias")?
        } else {
            let alias = self.aliases.alias_for(normalized.as_str(), &[])?;
            if self.reporting {
                self.decisions
                    .entry((
                        self.record,
                        Some(alias.clone()),
                        "path.foreign_root".into(),
                        ProjectionAction::ExcludeSource,
                    ))
                    .or_insert(1);
            }
            alias
        };
        self.used_aliases.insert(alias.clone());
        Ok(alias)
    }

    fn rewrite_text(&mut self, text: &str) -> Result<String> {
        let mut output = self.rewrite_paths(text)?;
        for (index, (rule, regex)) in self
            .policy
            .replacements
            .iter()
            .zip(&self.replacements)
            .enumerate()
        {
            let count = regex.find_iter(&output).count();
            if self.reporting {
                self.report.replacements += count;
                if count > 0 {
                    *self
                        .decisions
                        .entry((
                            self.record,
                            None,
                            format!("replacements[{index}]"),
                            ProjectionAction::RewriteText,
                        ))
                        .or_default() += count;
                }
            }
            output = if rule.regex {
                regex
                    .replace_all(&output, rule.replacement.as_str())
                    .into_owned()
            } else {
                regex
                    .replace_all(&output, regex::NoExpand(&rule.replacement))
                    .into_owned()
            };
        }
        self.rewrite_paths(&output)
    }

    fn rewrite_paths(&mut self, text: &str) -> Result<String> {
        let matches = self
            .paths
            .find_iter(text)
            .map(|m| (m.start(), m.end()))
            .collect::<Vec<_>>();
        let mut output = String::new();
        let mut start = 0;
        for (a, b) in matches {
            // A URL or an existing logical path is not a local absolute path.
            if a > 0
                && (text[..a].ends_with([':', '>'])
                    || text.as_bytes()[a - 1].is_ascii_alphanumeric())
            {
                continue;
            }
            output.push_str(&text[start..a]);
            output.push_str(&self.path_alias(&text[a..b])?);
            start = b;
        }
        output.push_str(&text[start..]);
        Ok(output)
    }

    fn rewrite(&mut self, value: &Value) -> Result<Value> {
        let rewritten = self.rewrite_inner(value)?;
        let checked = self.redactor.try_scrub_json(&rewritten)?;
        if self.reporting {
            self.report.secret_matches += checked.secrets;
            self.note(
                "secret_scan".into(),
                ProjectionAction::MaskSecret,
                checked.secrets,
            );
        }
        Ok(checked.value)
    }

    fn rewrite_inner(&mut self, value: &Value) -> Result<Value> {
        Ok(match value {
            Value::String(text) => Value::String(self.rewrite_text(text)?),
            Value::Array(values) => Value::Array(
                values
                    .iter()
                    .map(|v| self.rewrite_inner(v))
                    .collect::<Result<_>>()?,
            ),
            Value::Object(values) => {
                let mut output = serde_json::Map::new();
                for (key, value) in values {
                    ensure!(
                        output
                            .insert(self.rewrite_text(key)?, self.rewrite_inner(value)?)
                            .is_none(),
                        "privacy rewriting collides JSON field names"
                    );
                }
                Value::Object(output)
            }
            other => other.clone(),
        })
    }
}

fn reasoning_block(text: &str) -> Value {
    json!({"type":"thinking", "thinking":text})
}

fn reasoning_block_text(block: &Value) -> String {
    let direct = block["thinking"]
        .as_str()
        .or_else(|| block["text"].as_str())
        .or_else(|| block["content"].as_str())
        .or_else(|| block["reasoning"].as_str())
        .or_else(|| block["reasoning_content"].as_str())
        .unwrap_or_default()
        .to_owned();
    if direct.is_empty() {
        native_message_text(&block["content"])
    } else {
        direct
    }
}

fn is_reasoning_block(block: &Value) -> bool {
    block["type"] == "thinking"
}

fn assistant_for_reasoning(role: String, blocks: &[Value]) -> String {
    if role != "assistant" && blocks.iter().any(is_reasoning_block) {
        "assistant".into()
    } else {
        role
    }
}

fn native_message_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(values) => values
            .iter()
            .map(native_message_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(fields) => ["text", "thinking", "content", "output"]
            .iter()
            .filter_map(|key| fields.get(*key))
            .map(native_message_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn native_tool_input(value: &Value) -> Value {
    value
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_else(|| value.clone())
}

fn native_content_blocks(value: &Value) -> Vec<Value> {
    match value {
        Value::String(text) => vec![json!({"type":"text", "text":text})],
        Value::Array(values) => values.iter().map(native_content_block).collect(),
        Value::Object(_) if value.get("type").is_some() => vec![native_content_block(value)],
        _ => Vec::new(),
    }
}

fn native_content_block(block: &Value) -> Value {
    let Some(kind) = block["type"].as_str() else {
        return block.clone();
    };
    match kind {
        "thinking" | "reasoning" | "redacted_thinking" => {
            reasoning_block(&reasoning_block_text(block))
        }
        "toolCall" => json!({
            "type":"tool_use",
            "id":block["id"].as_str().unwrap_or_default(),
            "name":block["name"].as_str().unwrap_or("tool"),
            "input":native_tool_input(
                block
                    .get("arguments")
                    .or_else(|| block.get("input"))
                    .unwrap_or(&Value::Null)
            )
        }),
        "toolResult" => json!({
            "type":"tool_result",
            "tool_use_id":block["toolCallId"].as_str().unwrap_or_default(),
            "content":block["content"].clone(),
            "is_error":block["isError"] == true
        }),
        _ => block.clone(),
    }
}

fn collect_reasoning_value(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::String(text) if !text.is_empty() => output.push(text.clone()),
        Value::Array(values) => {
            for value in values {
                collect_reasoning_value(value, output);
            }
        }
        Value::Object(fields) => {
            for key in [
                "text",
                "thinking",
                "content",
                "summary",
                "reasoning",
                "reasoning_content",
            ] {
                if let Some(value) = fields.get(key) {
                    collect_reasoning_value(value, output);
                }
            }
        }
        _ => {}
    }
}

fn codex_reasoning_text(payload: &Value) -> String {
    let mut values = Vec::new();
    // Summary is the preferred readable representation.  Content is a valid fallback when
    // providers emit an empty summary alongside readable blocks; encrypted fields are ignored.
    for key in [
        "summary",
        "content",
        "text",
        "thinking",
        "message",
        "reasoning",
        "reasoning_content",
    ] {
        if let Some(value) = payload.get(key) {
            collect_reasoning_value(value, &mut values);
        }
    }
    let mut unique = Vec::with_capacity(values.len());
    for value in values {
        if !unique.contains(&value) {
            unique.push(value);
        }
    }
    unique.join("\n")
}

fn hermes_reasoning_texts(data: &Value) -> Vec<String> {
    let mut values = Vec::new();
    for key in [
        "reasoning",
        "reasoning_content",
        "reasoning_details",
        "codex_reasoning_items",
    ] {
        if let Some(value) = data.get(key) {
            collect_reasoning_value(value, &mut values);
        }
    }
    let mut unique = Vec::with_capacity(values.len());
    for value in values {
        if !unique.contains(&value) {
            unique.push(value);
        }
    }
    unique
}

fn hermes_tool_blocks(data: &Value) -> Vec<Value> {
    let calls = match &data["tool_calls"] {
        Value::String(text) => serde_json::from_str::<Value>(text).ok(),
        value => Some(value.clone()),
    };
    calls
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .map(|call| {
            json!({
                "type":"tool_use",
                "id":call["id"].as_str().unwrap_or_default(),
                "name":call["function"]["name"].as_str().unwrap_or("tool"),
                "input":native_tool_input(&call["function"]["arguments"])
            })
        })
        .collect()
}

fn hermes_has_reasoning(data: &Value) -> bool {
    data["active"] != 0
        && [
            "reasoning",
            "reasoning_content",
            "reasoning_details",
            "codex_reasoning_items",
        ]
        .iter()
        .any(|key| !data[*key].is_null())
}

fn hermes_has_reasoning_fields(data: &Value) -> bool {
    [
        "reasoning",
        "reasoning_content",
        "reasoning_details",
        "codex_reasoning_items",
    ]
    .iter()
    .any(|key| !data[*key].is_null())
}

fn absolute_path(path: &str) -> bool {
    normalize_path(path).is_ok_and(|path| {
        path.as_str().starts_with('/') || path.as_str().as_bytes().get(1..3) == Some(b":/")
    })
}

fn record_cwd(record: &Envelope) -> Option<&str> {
    let value = &record.content;
    match record.source.as_str() {
        "codex"
            if matches!(
                value["type"].as_str(),
                Some("session_meta" | "turn_context")
            ) =>
        {
            value["payload"]["cwd"].as_str()
        }
        "claude-code" | "claude-desktop" | "cursor" => value["cwd"].as_str(),
        "workbuddy" => value["cwd"].as_str(),
        "openclaw" if value["type"] == "session" => value["cwd"].as_str(),
        _ => None,
    }
}

fn decode_arguments(payload: &Value) -> Value {
    payload["arguments"]
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null)
}

fn message_role(record: &Envelope) -> Option<&str> {
    let value = &record.content;
    let role = match record.source.as_str() {
        "cursor" if value.get("type").is_none() => value["role"].as_str(),
        "claude-code" | "claude-desktop" | "cursor"
            if matches!(value["type"].as_str(), Some("user" | "assistant")) =>
        {
            Some(value["message"]["role"].as_str().unwrap_or("user"))
        }
        _ => None,
    };
    role.filter(|role| matches!(*role, "user" | "assistant"))
}

/// Keep the native parser's existing classification markers on public message records.
///
/// A public record still uses the native message shape so existing readers can consume it. The
/// marker is part of that shape: dropping it turns runtime-generated user-shaped records into
/// prompts, which changes turn boundaries. Unknown records receive the existing `isMeta` marker;
/// known records retain only the fixed markers that affect Claude's event classification.
fn with_native_classification(mut output: Value, source: &Value, internal: bool) -> Value {
    let fields = output
        .as_object_mut()
        .expect("projected native message is an object");
    let native_internal = internal
        || source["isMeta"] == true
        || (source["type"] == "hermes_message" && source["data"]["active"] == 0)
        || (source["kind"] == "part" && source["data"]["synthetic"] == true);
    let native_compact = source["isCompactSummary"] == true
        || (source["type"] == "hermes_message" && source["data"]["_compressed_summary"] == 1);
    if native_internal {
        fields.insert("isMeta".into(), json!(true));
    } else if native_compact {
        fields.insert("isCompactSummary".into(), json!(true));
    } else if source["promptSource"] == "system" {
        fields.insert("promptSource".into(), json!("system"));
    }
    output
}

fn codex_user_message_is_internal(value: &Value) -> bool {
    if value["type"] != "response_item"
        || value["payload"]["type"] != "message"
        || value["payload"]["role"] != "user"
    {
        return false;
    }
    let Some(blocks) = value["payload"]["content"].as_array() else {
        return false;
    };
    let texts = blocks
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>();
    !texts.is_empty()
        && texts.iter().all(|text| {
            crate::adapter::codex::classify_message("user", (*text).to_owned()).0
                == crate::adapter::EventKind::Other
        })
}

fn tool_calls(record: &Envelope) -> Vec<(String, String, Value)> {
    let value = &record.content;
    match record.source.as_str() {
        "claude-code" | "claude-desktop" | "cursor"
            if message_role(record) == Some("assistant") =>
        {
            value["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "tool_use")
                .filter_map(|block| {
                    Some((
                        block["id"].as_str().filter(|id| !id.is_empty())?.into(),
                        block["name"].as_str()?.into(),
                        block["input"].clone(),
                    ))
                })
                .collect()
        }
        "claude-desktop" if value["message"]["role"] == "assistant" => value["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "tool_use")
            .filter_map(|block| {
                Some((
                    block["id"].as_str().filter(|id| !id.is_empty())?.into(),
                    block["name"].as_str()?.into(),
                    block["input"].clone(),
                ))
            })
            .collect(),
        "codex"
            if value["type"] == "response_item" && value["payload"]["type"] == "function_call" =>
        {
            let payload = &value["payload"];
            match (payload["call_id"].as_str(), payload["name"].as_str()) {
                (Some(id), Some(name)) if !id.is_empty() => {
                    vec![(id.into(), name.into(), decode_arguments(payload))]
                }
                _ => vec![],
            }
        }
        "openclaw"
            if matches!(value["type"].as_str(), Some("message" | "custom_message"))
                && (if value["type"] == "message" {
                    value["message"]["role"] == "assistant"
                } else {
                    value["role"] == "assistant"
                }) =>
        {
            let body = if value["type"] == "message" {
                &value["message"]
            } else {
                value
            };
            body["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "toolCall" || block["type"] == "tool_use")
                .filter_map(|block| {
                    Some((
                        block["id"].as_str().filter(|id| !id.is_empty())?.into(),
                        block["name"].as_str()?.into(),
                        native_tool_input(
                            block
                                .get("arguments")
                                .or_else(|| block.get("input"))
                                .unwrap_or(&Value::Null),
                        ),
                    ))
                })
                .collect()
        }
        "workbuddy" if value["type"] == "message" && value["role"] == "assistant" => {
            value["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "toolCall" || block["type"] == "tool_use")
                .filter_map(|block| {
                    Some((
                        block["id"].as_str().filter(|id| !id.is_empty())?.into(),
                        block["name"].as_str()?.into(),
                        native_tool_input(
                            block
                                .get("arguments")
                                .or_else(|| block.get("input"))
                                .unwrap_or(&Value::Null),
                        ),
                    ))
                })
                .collect()
        }
        "workbuddy" if value["type"] == "function_call" => {
            match (value["callId"].as_str(), value["name"].as_str()) {
                (Some(id), Some(name)) if !id.is_empty() => {
                    vec![(
                        id.into(),
                        name.into(),
                        native_tool_input(&value["arguments"]),
                    )]
                }
                _ => vec![],
            }
        }
        "hermes" if value["type"] == "hermes_message" && value["data"]["role"] == "assistant" => {
            let calls = match &value["data"]["tool_calls"] {
                Value::String(text) => serde_json::from_str::<Value>(text).ok(),
                value => Some(value.clone()),
            };
            calls
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|call| {
                    Some((
                        call["id"].as_str().filter(|id| !id.is_empty())?.into(),
                        call["function"]["name"].as_str()?.into(),
                        native_tool_input(&call["function"]["arguments"]),
                    ))
                })
                .collect()
        }
        _ => vec![],
    }
}

fn is_file_tool(name: &str) -> bool {
    matches!(
        name,
        "Read" | "read_file" | "Edit" | "edit_file" | "Write" | "write_file"
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContentContext {
    Json,
    MessageBlock,
}

fn is_message_block(value: &Value) -> bool {
    // Additional data fields make a text-shaped object ambiguous; keep its JSON structure.
    (matches!(
        value["type"].as_str(),
        Some("text" | "input_text" | "output_text")
    ) && value["text"].is_string()
        && value.as_object().is_some_and(|fields| fields.len() == 2))
        || is_attachment(value, ContentContext::Json)
}

fn is_attachment(value: &Value, context: ContentContext) -> bool {
    let kind = value.get("type").and_then(Value::as_str);
    if matches!(kind, Some("attachment" | "skill_listing")) {
        return true;
    }
    let media = matches!(
        kind,
        Some(
            "image"
                | "input_image"
                | "output_image"
                | "document"
                | "file"
                | "input_file"
                | "audio"
                | "input_audio"
                | "output_audio"
                | "video"
                | "resource"
                | "resource_link"
                | "blob"
                | "base64"
        )
    );
    // MIME labels and file kinds also occur in HTTP responses and directory listings.
    // Outside message blocks, require a payload carrier before discarding the object.
    let payload = ["data", "blob", "file_data", "file_id"]
        .iter()
        .any(|field| value[*field].is_string())
        || ["image_url", "video_url", "file_url"]
            .iter()
            .any(|field| value[*field].is_string() || value[*field]["url"].is_string())
        || ["audio", "input_audio"]
            .iter()
            .any(|field| value[*field]["data"].is_string())
        || (matches!(kind, Some("document" | "file" | "resource")) && value["content"].is_string())
        || (matches!(kind, Some("document" | "resource")) && value["text"].is_string())
        || value.get("source").is_some_and(|source| {
            matches!(source["type"].as_str(), Some("base64" | "url" | "text"))
                && ["data", "url", "text"]
                    .iter()
                    .any(|field| source[*field].is_string())
        })
        || value
            .get("resource")
            .is_some_and(|resource| resource["text"].is_string() || resource["blob"].is_string())
        || value
            .get("file")
            .is_some_and(|file| file["file_data"].is_string() || file["file_id"].is_string())
        || value
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|url| url.starts_with("data:"));
    let mime = value
        .get("mimeType")
        .or_else(|| value.get("mime_type"))
        .or_else(|| value.get("media_type"))
        .and_then(Value::as_str);
    let binary = value["blob"].is_string()
        || value["file_data"].is_string()
        || (value["data"].is_string()
            && mime.is_some_and(|mime| {
                mime.starts_with("image/")
                    || mime.starts_with("audio/")
                    || mime.starts_with("video/")
                    || matches!(mime, "application/pdf" | "application/octet-stream")
            }));
    media && (context == ContentContext::MessageBlock || payload)
        || (mime.is_some() && binary)
        || (value["encoding"] == "base64" && value["data"].is_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{meta, privacy::PrivacyPolicy, privacy_envelope::ViewingRecipient};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use crypto_box::SecretKey;
    use serde_json::json;

    #[test]
    fn complete_snapshot_projects_paths_and_recovers_original_layer() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src/main.rs");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "fn main() {}\n").unwrap();
        let session_id = format!("agit-{}", "a".repeat(40));
        let raw = [
            json!({"type":"user","cwd":temp.path(),"message":{"role":"user","content":"read /secret/customer.txt"}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"Read","input":{"file_path":"src/main.rs"}}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"allowed file body MARKER"}]}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"call-2","name":"Read","input":{"file_path":temp.path().join("private.txt")}}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-2","content":"excluded file body"}]}}),
        ]
        .into_iter()
        .map(|value| format!("{value}\n"))
        .collect::<String>();
        let log = transcript::wrap_lines(&raw, "claude-code", &session_id);
        let metadata = meta::Meta::new(
            session_id,
            "claude-code".into(),
            temp.path().to_string_lossy().into_owned(),
        );
        let policy = PrivacyPolicy {
            workspace: Some(temp.path().to_path_buf()),
            include: vec!["src/**".into()],
            replacements: vec![
                super::super::privacy::ReplacementRule {
                    pattern: "MARKER".into(),
                    replacement: "synthetic-private-value".into(),
                    regex: false,
                },
                super::super::privacy::ReplacementRule {
                    pattern: "main.rs".into(),
                    replacement: "visible.rs".into(),
                    regex: false,
                },
            ],
            ..PrivacyPolicy::default()
        };
        let mut aliases = PathAliasStore::default();
        let redactor = Redactor::new(Default::default());
        let view = log.split_inclusive('\n').next_back().unwrap();
        let projection = project_session(
            &policy,
            &mut aliases,
            None,
            &log,
            view,
            &metadata,
            &redactor,
        )
        .unwrap();
        assert_eq!(projection.report.records, 5);
        assert!(!projection.log().contains(source.to_string_lossy().as_ref()));
        assert!(projection.log().contains("<workspace>/src/visible.rs"));
        assert!(projection.log().contains(OMITTED));
        assert!(projection.log().contains("allowed file body"));
        assert!(!projection.log().contains("excluded file body"));
        assert!(projection.log().contains("synthetic-private-value"));
        assert!(!projection.log().contains("customer.txt"));
        assert_eq!(
            projection.view(),
            projection.log().split_inclusive('\n').next_back().unwrap()
        );

        let key = SecretKey::from([21; 32]);
        let recipient = ViewingRecipient::from_base64(
            "viewer".into(),
            &STANDARD.encode(key.public_key().as_bytes()),
        )
        .unwrap();
        let envelope = projection.seal(&recipient).unwrap();
        let layer = envelope.open_layer(&key).unwrap();
        let (private_log, private_view) = layer.session_bytes().unwrap();
        assert_eq!(private_log.as_str(), log);
        assert_eq!(private_view.as_str(), view);
        assert!(layer.path_aliases.len() <= aliases.len());
        assert!(
            layer
                .path_aliases
                .values()
                .any(|path| path.ends_with("src/main.rs"))
        );
        assert!(transcript::display::parse(projection.log()).is_ok());
        let details = projection.report.details.as_ref().unwrap();
        assert_eq!(details.policy_digest, policy.digest().unwrap());
        assert!(
            details
                .decisions
                .iter()
                .any(|decision| decision.record == Some(1)
                    && decision.path.as_deref() == Some("<workspace>/src/visible.rs")
                    && decision.rule == "repository.include[0]"
                    && decision.action == ProjectionAction::AllowPath)
        );
        assert!(
            details
                .decisions
                .iter()
                .any(|decision| decision.record == Some(3)
                    && decision.rule == "repository.include"
                    && decision.action == ProjectionAction::ExcludeSource)
        );
        assert!(
            details
                .decisions
                .iter()
                .any(|decision| decision.record == Some(2)
                    && decision.rule == "replacements[0]"
                    && decision.matches == 1)
        );
        let encoded = serde_json::to_string(&projection.report).unwrap();
        assert!(
            !projection
                .inspection_text()
                .unwrap()
                .contains(&policy.digest().unwrap())
        );
        for private in [
            "MARKER",
            "main.rs",
            "synthetic-private-value",
            "customer.txt",
            "private.txt",
            temp.path().to_str().unwrap(),
        ] {
            assert!(!encoded.contains(private), "report exposes {private}");
        }
        let mut legacy = serde_json::to_value(&projection.report).unwrap();
        legacy.as_object_mut().unwrap().remove("details");
        let parsed: ProjectionReport = serde_json::from_value(legacy.clone()).unwrap();
        parsed.validate(&policy.digest().unwrap()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), legacy);
        let mut wrong_binding = projection.report.clone();
        wrong_binding.details.as_mut().unwrap().policy_digest = "sha256:wrong".into();
        assert!(wrong_binding.validate(&policy.digest().unwrap()).is_err());
        let mut wrong_public = projection.public_value();
        wrong_public["report"] = serde_json::to_value(wrong_binding).unwrap();
        assert!(
            public_inspection_text(
                wrong_public,
                projection.log(),
                projection.view(),
                &policy.digest().unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn native_reasoning_keeps_one_canonical_assistant_block() {
        let policy = PrivacyPolicy::default();
        let mut aliases = PathAliasStore::default();
        let redactor = Redactor::new(Default::default());
        let values = [
            (
                "claude-code",
                json!({"type":"assistant","message":{"role":"assistant","content":[
                    {"type":"thinking","thinking":"claude","signature":"opaque"},
                    {"type":"text","text":"reply"}
                ]}}),
            ),
            (
                "codex",
                json!({"type":"response_item","payload":{"type":"reasoning","summary":[{"text":"codex"}],"encrypted_content":"opaque"}}),
            ),
            (
                "codex",
                json!({"type":"event_msg","payload":{"type":"agent_reasoning","text":"event"}}),
            ),
            (
                "opencode",
                json!({"kind":"part","message_id":"m","data":{"type":"reasoning","text":"open"}}),
            ),
            (
                "hermes",
                json!({"type":"hermes_message","data":{"role":"assistant","active":1,"content":"reply","reasoning":"one","reasoning_content":"two"}}),
            ),
            (
                "openclaw",
                json!({"type":"message","id":"m","message":{"role":"assistant","content":[{"type":"thinking","thinking":"claw"},{"type":"text","text":"reply"}]}}),
            ),
            (
                "workbuddy",
                json!({"type":"message","role":"assistant","content":[{"type":"reasoning","text":"buddy"},{"type":"text","text":"reply"}]}),
            ),
        ];
        let mut projector =
            Projector::new(&policy, &mut aliases, None, &redactor, values.len()).unwrap();
        for (source, value) in values {
            let record = Envelope {
                source: source.into(),
                session_id: "session".into(),
                object_hash: String::new(),
                content: value,
            };
            projector.remember_native_role(&record);
            let (_, blocks) = projector
                .native_message(&record)
                .unwrap_or_else(|| panic!("missing native message for {source}"));
            assert!(blocks.iter().any(|block| block["type"] == "thinking"));
            assert!(blocks.iter().all(|block| {
                block["type"] != "thinking"
                    || block.as_object().is_some_and(|fields| fields.len() == 2)
                        && block["thinking"].is_string()
            }));
        }
        let ordinary = json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":r#"{"thinking":"ordinary"}"#} ]}});
        let record = Envelope {
            source: "claude-code".into(),
            session_id: "session".into(),
            object_hash: String::new(),
            content: ordinary,
        };
        let (_, blocks) = projector.native_message(&record).unwrap();
        assert_eq!(blocks[0]["type"], "text");
    }

    #[test]
    fn projected_reasoning_is_masked_without_publishing_native_fields() {
        let session = format!("agit-{}", "a".repeat(40));
        let raw = json!({
            "type":"assistant",
            "message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"PRIVATE_REASONING","signature":"opaque"},
                {"type":"text","text":"PUBLIC_REPLY"}
            ]}
        });
        let log = transcript::wrap_lines(&format!("{raw}\n"), "claude-code", &session);
        let metadata = meta::Meta::new(session, "claude-code".into(), String::new());
        let policy = PrivacyPolicy {
            replacements: vec![super::super::privacy::ReplacementRule {
                pattern: "PRIVATE_REASONING".into(),
                replacement: "MASKED_REASONING".into(),
                regex: false,
            }],
            ..PrivacyPolicy::default()
        };
        let mut aliases = PathAliasStore::default();
        let redactor = Redactor::new(Default::default());
        let projection = project_session(
            &policy,
            &mut aliases,
            None,
            &log,
            &log,
            &metadata,
            &redactor,
        )
        .unwrap();
        let records = storage::parse_envelopes(projection.log()).unwrap();
        let blocks = records[0].content["message"]["content"].as_array().unwrap();
        assert_eq!(
            blocks[0],
            json!({"type":"thinking","thinking":"MASKED_REASONING"})
        );
        assert_eq!(blocks[1], json!({"type":"text","text":"PUBLIC_REPLY"}));
        assert!(!projection.log().contains("signature"));
        assert!(!projection.log().contains("PRIVATE_REASONING"));
    }

    #[test]
    fn native_tool_results_filter_attachments_after_projection() {
        let policy = PrivacyPolicy::default();
        let mut aliases = PathAliasStore::default();
        let redactor = Redactor::new(Default::default());
        let mut projector = Projector::new(&policy, &mut aliases, None, &redactor, 1).unwrap();
        projector.record = Some(0);
        let record = Envelope {
            source: "openclaw".into(),
            session_id: "session".into(),
            object_hash: String::new(),
            content: json!({
                "type":"message",
                "message":{
                    "role":"toolResult",
                    "toolCallId":"call",
                    "content":{"type":"document","text":"PRIVATE_ATTACHMENT"}
                }
            }),
        };
        let calls = BTreeMap::from([(
            ("openclaw".into(), "session".into(), "call".into()),
            FileCall {
                id: "call".into(),
                allowed: true,
                ambiguous: false,
            },
        )]);
        let projected = projector.record(&record, None, &calls).unwrap();
        assert_eq!(projected["message"]["content"][0]["content"], OMITTED);
        assert!(!projected.to_string().contains("PRIVATE_ATTACHMENT"));
    }

    #[test]
    fn native_tool_results_keep_structured_json_content() {
        let policy = PrivacyPolicy::default();
        let mut aliases = PathAliasStore::default();
        let redactor = Redactor::new(Default::default());
        let mut projector = Projector::new(&policy, &mut aliases, None, &redactor, 1).unwrap();
        projector.record = Some(0);
        let record = Envelope {
            source: "workbuddy".into(),
            session_id: "session".into(),
            object_hash: String::new(),
            content: json!({
                "type":"function_call_result",
                "callId":"call",
                "output":{"stdout":"RESULT","exit_code":0},
                "isError":false
            }),
        };
        let calls = BTreeMap::from([(
            ("workbuddy".into(), "session".into(), "call".into()),
            FileCall {
                id: "call".into(),
                allowed: true,
                ambiguous: false,
            },
        )]);
        let projected = projector.record(&record, None, &calls).unwrap();
        let content = projected["message"]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(content).unwrap(),
            json!({"stdout":"RESULT","exit_code":0})
        );
    }

    #[test]
    fn openclaw_custom_messages_keep_thinking_classified() {
        let policy = PrivacyPolicy::default();
        let mut aliases = PathAliasStore::default();
        let redactor = Redactor::new(Default::default());
        let mut projector = Projector::new(&policy, &mut aliases, None, &redactor, 1).unwrap();
        projector.record = Some(0);
        let record = Envelope {
            source: "openclaw".into(),
            session_id: "session".into(),
            object_hash: String::new(),
            content: json!({
                "type":"custom_message",
                "role":"assistant",
                "content":[
                    {"type":"thinking","thinking":"reasoning","signature":"opaque"},
                    {"type":"text","text":"reply"}
                ]
            }),
        };
        let projected = projector.record(&record, None, &BTreeMap::new()).unwrap();
        assert_eq!(projected["type"], "assistant");
        assert_eq!(
            projected["message"]["content"][0],
            json!({"type":"thinking","thinking":"reasoning"})
        );
        assert_eq!(
            projected["message"]["content"][1],
            json!({"type":"text","text":"reply"})
        );
        assert!(!projected.to_string().contains("signature"));
    }

    #[test]
    fn hermes_and_opencode_native_markers_do_not_open_turns() {
        let cases = [
            (
                "hermes",
                vec![
                    json!({"type":"hermes_message","data":{"session_id":"session","role":"user","content":"stale","active":0}}),
                    json!({"type":"hermes_message","data":{"session_id":"session","role":"user","content":"summary","active":1,"_compressed_summary":1}}),
                ],
                vec!["isMeta", "isCompactSummary"],
            ),
            (
                "opencode",
                vec![
                    json!({"kind":"message","id":"m","data":{"role":"user"}}),
                    json!({"kind":"part","message_id":"m","data":{"type":"text","text":"injected","synthetic":true}}),
                ],
                vec!["isMeta"],
            ),
        ];
        for (runtime, values, markers) in cases {
            let session = format!("agit-{}", "a".repeat(40));
            let raw = values
                .into_iter()
                .map(|value| format!("{value}\n"))
                .collect::<String>();
            let log = transcript::wrap_lines(&raw, runtime, &session);
            let projection = project_session(
                &PrivacyPolicy::default(),
                &mut PathAliasStore::default(),
                None,
                &log,
                &log,
                &Meta::new(session, runtime.into(), String::new()),
                &Redactor::new(Default::default()),
            )
            .unwrap();
            let records = storage::parse_envelopes(projection.log()).unwrap();
            assert_eq!(
                records.len(),
                markers.len() + usize::from(runtime == "opencode")
            );
            for marker in markers {
                assert!(
                    records.iter().any(|record| record.content[marker] == true),
                    "missing {marker} marker for {runtime}"
                );
            }
            let public = transcript::display::parse(projection.log()).unwrap();
            assert!(crate::domain::turn::groups_of(&public).is_empty());
        }
    }

    #[test]
    fn hermes_inactive_records_keep_public_event_kinds_and_tool_pairing() {
        let session = format!("agit-{}", "b".repeat(40));
        let raw = [
            json!({
                "type":"hermes_message",
                "data":{
                    "session_id":"native",
                    "role":"assistant",
                    "content":"stale assistant",
                    "active":0,
                    "tool_calls":"[{\"id\":\"inactive\",\"type\":\"function\",\"function\":{\"name\":\"Bash\",\"arguments\":\"{}\"}}]"
                }
            }),
            json!({
                "type":"hermes_message",
                "data":{
                    "session_id":"native",
                    "role":"tool",
                    "tool_call_id":"inactive",
                    "content":"stale tool output",
                    "active":0
                }
            }),
            json!({
                "type":"hermes_message",
                "data":{
                    "session_id":"native",
                    "role":"assistant",
                    "content":"compressed context",
                    "active":1,
                    "_compressed_summary":1
                }
            }),
        ]
        .into_iter()
        .map(|value| format!("{value}\n"))
        .collect::<String>();
        let log = transcript::wrap_lines(&raw, "hermes", &session);
        let projection = project_session(
            &PrivacyPolicy::default(),
            &mut PathAliasStore::default(),
            None,
            &log,
            &log,
            &Meta::new(session, "hermes".into(), String::new()),
            &Redactor::new(Default::default()),
        )
        .unwrap();
        let public = transcript::display::parse(projection.log()).unwrap();
        assert_eq!(
            public
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                crate::adapter::EventKind::Other,
                crate::adapter::EventKind::Other,
                crate::adapter::EventKind::CompactSummary
            ]
        );
        let counts = public.counts();
        assert_eq!(counts.prompts, 0);
        assert_eq!(counts.replies, 0);
        assert_eq!(counts.tools, 0);
        assert_eq!(counts.outputs, 0);
        assert_eq!(counts.compactions, 1);
        assert_eq!(counts.dropped, 2);
        let native = transcript::unwrap_strict(projection.log()).unwrap();
        assert!(
            crate::adapter::get("claude-code")
                .unwrap()
                .open_tool_calls(&native)
                .is_empty()
        );
    }

    #[test]
    fn projected_internal_records_do_not_open_false_turns() {
        let session = format!("agit-{}", "d".repeat(40));
        let raw = [
            json!({"type":"user","message":{"role":"user","content":"Run pwd"}}),
            json!({"type":"progress","data":{"message":"Working"}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done"}]}}),
        ]
        .into_iter()
        .map(|value| format!("{value}\n"))
        .collect::<String>();
        let log = transcript::wrap_lines(&raw, "claude-code", &session);
        let metadata = Meta::new(session, "claude-code".into(), String::new());
        let original = transcript::display::parse(&log).unwrap();
        let projection = project_session(
            &PrivacyPolicy::default(),
            &mut PathAliasStore::default(),
            None,
            &log,
            &log,
            &metadata,
            &Redactor::new(Default::default()),
        )
        .unwrap();
        let public = transcript::display::parse(projection.log()).unwrap();

        assert_eq!(crate::domain::turn::groups_of(&original).len(), 1);
        assert_eq!(crate::domain::turn::groups_of(&public).len(), 1);
        assert_eq!(
            public
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                crate::adapter::EventKind::UserPrompt,
                crate::adapter::EventKind::Other,
                crate::adapter::EventKind::AssistantReply,
            ]
        );
    }

    #[test]
    fn projected_compact_summaries_keep_their_non_prompt_classification() {
        let session = format!("agit-{}", "f".repeat(40));
        let raw = [
            json!({"type":"user","message":{"role":"user","content":"Continue the work"}}),
            json!({"type":"user","isCompactSummary":true,"message":{"role":"user","content":"Summary: previous context"}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Continuing"}]}}),
        ]
        .into_iter()
        .map(|value| format!("{value}\n"))
        .collect::<String>();
        let log = transcript::wrap_lines(&raw, "claude-code", &session);
        let metadata = Meta::new(session, "claude-code".into(), String::new());
        let projection = project_session(
            &PrivacyPolicy::default(),
            &mut PathAliasStore::default(),
            None,
            &log,
            &log,
            &metadata,
            &Redactor::new(Default::default()),
        )
        .unwrap();
        let public = transcript::display::parse(projection.log()).unwrap();

        assert_eq!(crate::domain::turn::groups_of(&public).len(), 1);
        assert_eq!(
            public
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                crate::adapter::EventKind::UserPrompt,
                crate::adapter::EventKind::CompactSummary,
                crate::adapter::EventKind::AssistantReply,
            ]
        );
    }

    #[test]
    fn projected_codex_environment_context_does_not_open_a_turn() {
        let session = format!("agit-{}", "a".repeat(40));
        let raw = [
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/workspace</cwd>\n</environment_context>"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"inspect the project"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done"}]}}),
        ]
        .into_iter()
        .map(|value| format!("{value}\n"))
        .collect::<String>();
        let log = transcript::wrap_lines(&raw, "codex", &session);
        let metadata = Meta::new(session, "codex".into(), String::new());
        let original = transcript::display::parse(&log).unwrap();
        let projection = project_session(
            &PrivacyPolicy::default(),
            &mut PathAliasStore::default(),
            None,
            &log,
            &log,
            &metadata,
            &Redactor::new(Default::default()),
        )
        .unwrap();
        let public = transcript::display::parse(projection.log()).unwrap();

        assert_eq!(crate::domain::turn::groups_of(&original).len(), 1);
        assert_eq!(crate::domain::turn::groups_of(&public).len(), 1);
        assert_eq!(
            public
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                crate::adapter::EventKind::Other,
                crate::adapter::EventKind::UserPrompt,
                crate::adapter::EventKind::AssistantReply,
            ]
        );
    }

    #[test]
    fn projected_internal_records_preserve_turns_text_tool_pairs_and_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src/main.rs");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "fn main() {}\n").unwrap();
        let secret = "PRIVATE_MARKER";
        let session = format!("agit-{}", "e".repeat(40));
        let prompt = format!("first prompt {secret} {}", source.display());
        let result = format!("tool result {secret} {}", source.display());
        let raw = [
            json!({"type":"user","cwd":temp.path(),"message":{"role":"user","content":prompt}}),
            json!({"type":"progress","data":{"message":format!("internal progress {secret} {}", source.display())}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"native-call","name":"Read","input":{"file_path":"src/main.rs"}}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"native-call","content":result}]}}),
            json!({"type":"user","isMeta":true,"message":{"role":"user","content":format!("internal note {secret} {}", source.display())}}),
            json!({"type":"user","cwd":temp.path(),"message":{"role":"user","content":format!("second prompt {secret} {}", source.display())}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done"}]}}),
        ]
        .into_iter()
        .map(|value| format!("{value}\n"))
        .collect::<String>();
        let log = transcript::wrap_lines(&raw, "claude-code", &session);
        let metadata = Meta::new(
            session,
            "claude-code".into(),
            temp.path().display().to_string(),
        );
        let policy = PrivacyPolicy {
            workspace: Some(temp.path().into()),
            include: vec!["src/**".into()],
            replacements: vec![super::super::privacy::ReplacementRule {
                pattern: secret.into(),
                replacement: "MASKED".into(),
                regex: false,
            }],
            ..Default::default()
        };
        let projection = project_session(
            &policy,
            &mut PathAliasStore::default(),
            None,
            &log,
            &log,
            &metadata,
            &Redactor::new(Default::default()),
        )
        .unwrap();
        let public = transcript::display::parse(projection.log()).unwrap();
        let public_prompts = public
            .events
            .iter()
            .filter(|event| event.kind == crate::adapter::EventKind::UserPrompt)
            .count();
        assert_eq!(crate::domain::turn::groups_of(&public).len(), 2);
        assert_eq!(public_prompts, 2);
        assert!(public.events.iter().any(|event| {
            event.kind == crate::adapter::EventKind::Other
                && event
                    .text
                    .as_deref()
                    .is_some_and(|text| text.contains(OMITTED))
        }));
        assert!(!projection.log().contains("internal progress"));
        assert!(public.events.iter().any(|event| {
            event.kind == crate::adapter::EventKind::Other
                && event
                    .text
                    .as_deref()
                    .is_some_and(|text| text.contains("internal note"))
        }));
        assert!(projection.log().contains("MASKED"));
        assert!(projection.log().contains("<workspace>/src/main.rs"));
        assert!(!projection.log().contains(secret));
        assert!(!projection.log().contains(temp.path().to_str().unwrap()));

        let records = storage::parse_envelopes(projection.log()).unwrap();
        let tool_id = records[2].content["message"]["content"][0]["id"]
            .as_str()
            .unwrap();
        let result_id = records[3].content["message"]["content"][0]["tool_use_id"]
            .as_str()
            .unwrap();
        assert_eq!(tool_id, result_id);

        let key = SecretKey::from([24; 32]);
        let recipient = ViewingRecipient::from_base64(
            "viewer".into(),
            &STANDARD.encode(key.public_key().as_bytes()),
        )
        .unwrap();
        let layer = projection
            .seal(&recipient)
            .unwrap()
            .open_layer(&key)
            .unwrap();
        let (private_log, private_view) = layer.session_bytes().unwrap();
        assert_eq!(private_log.as_str(), log);
        assert_eq!(private_view.as_str(), log);
    }

    #[test]
    fn cursor_roles_preserve_checked_text_and_encrypted_originals() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("src/main.rs");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let session = format!("agit-{}", "a".repeat(40));
        let raw = [
            json!({"role":"user","message":{"content":[{"type":"text","text":format!("MARKER Read {}", path.display())}]}}),
            json!({"role":"assistant","message":{"content":[{"type":"text","text":"Checked reply"},{"type":"tool_use","name":"Read","input":{"path":path}}]}}),
            json!({"role":"assistant","message":{"content":[{"type":"tool_use","id":"shell-call","name":"Bash","input":{"command":format!("cat {}", path.display())}}]}}),
            json!({"role":"user","message":{"content":[{"type":"tool_result","tool_use_id":"shell-call","content":"BASH_RESULT MARKER"}]}}),
            json!({"role":"assistant","message":{"content":[{"type":"tool_use","name":"Shell","input":{"command":"PRIVATE_COMMAND"}}]}}),
            json!({"role":"assistant","message":{"content":[{"type":"tool_use","id":"","name":"Read","input":{"path":path}}]}}),
            json!({"role":"user","message":{"content":[{"type":"tool_result","content":"UNPAIRED_RESULT"}]}}),
            json!({"type":"turn_ended","role":"assistant","message":{"content":"PRIVATE_CONTROL_RECORD"}}),
        ].into_iter().map(|value| format!("{value}\n")).collect::<String>();
        let log = transcript::wrap_lines(&raw, "cursor", &session);
        let metadata = Meta::new(session, "cursor".into(), temp.path().display().to_string());
        let policy = PrivacyPolicy {
            workspace: Some(temp.path().to_path_buf()),
            replacements: vec![super::super::privacy::ReplacementRule {
                pattern: "MARKER".into(),
                replacement: "Public label".into(),
                regex: false,
            }],
            ..Default::default()
        };
        let projection = project_session(
            &policy,
            &mut PathAliasStore::default(),
            None,
            &log,
            &log,
            &metadata,
            &Redactor::new(Default::default()),
        )
        .unwrap();
        assert!(projection.log().contains("Public label"));
        assert!(projection.log().contains("Checked reply"));
        assert!(!projection.log().contains("BASH_RESULT"));
        assert!(!projection.log().contains("call-2-"));
        assert!(!projection.log().contains("shell-call"));
        assert!(projection.log().contains("<workspace>/src/main.rs"));
        assert!(!projection.log().contains("MARKER"));
        assert!(!projection.log().contains("PRIVATE_COMMAND"));
        assert!(!projection.log().contains("PRIVATE_CONTROL_RECORD"));
        assert!(!projection.log().contains("UNPAIRED_RESULT"));
        assert!(!projection.log().contains(temp.path().to_str().unwrap()));
        let records = storage::parse_envelopes(projection.log()).unwrap();
        assert_eq!(records[1].content["message"]["role"], "assistant");
        assert_eq!(
            records[1].content["message"]["content"][1]["type"],
            "tool_use"
        );
        let key = SecretKey::from([21; 32]);
        let recipient = ViewingRecipient::from_base64(
            "viewer".into(),
            &STANDARD.encode(key.public_key().as_bytes()),
        )
        .unwrap();
        let layer = projection
            .seal(&recipient)
            .unwrap()
            .open_layer(&key)
            .unwrap();
        let (private_log, private_view) = layer.session_bytes().unwrap();
        assert_eq!(private_log.as_str(), log);
        assert_eq!(private_view.as_str(), log);
    }

    #[test]
    fn mandatory_file_exclusions_cover_shell_and_unknown_tool_results() {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("src/budget.txt");
        let allowed = temp.path().join("src/overview.txt");
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        let private_body = "The Elm project hiring budget is 480000 for next quarter.";
        std::fs::write(&private, private_body).unwrap();
        std::fs::write(&allowed, "Public overview").unwrap();
        let mut raw = String::new();
        for (id, name, input, output) in [
            ("read", "read_file", json!({"path":private}), private_body),
            (
                "shell",
                "exec_command",
                json!({"cmd":format!("cat {}", private.display())}),
                private_body,
            ),
            (
                "mcp",
                "mcp__files__read_file",
                json!({"path":private}),
                private_body,
            ),
            (
                "allowed",
                "read_file",
                json!({"path":allowed}),
                "Public overview",
            ),
        ] {
            for payload in [
                json!({"type":"function_call","call_id":id,"name":name,"arguments":input.to_string()}),
                json!({"type":"function_call_output","call_id":id,"output":output}),
            ] {
                raw.push_str(&format!(
                    "{}\n",
                    json!({"type":"response_item","payload":payload})
                ));
            }
        }
        let session = format!("agit-{}", "a".repeat(40));
        let log = transcript::wrap_lines(&raw, "codex", &session);
        let policy = PrivacyPolicy {
            workspace: Some(temp.path().into()),
            mandatory: vec![super::super::privacy::mandatory::MandatoryPolicy {
                version: 1,
                id: "organization".into(),
                revision: "r1".into(),
                exclude: vec!["src/budget.txt".into()],
                memory_exclude: vec![],
            }],
            ..Default::default()
        };
        assert_eq!(
            policy.evaluate_file(&private, None).action,
            CandidateAction::Excluded
        );
        assert_eq!(
            policy.evaluate_file(&allowed, None).action,
            CandidateAction::Allowed
        );
        let projection = project_session(
            &policy,
            &mut PathAliasStore::default(),
            None,
            &log,
            &log,
            &Meta::new(session, "codex".into(), temp.path().display().to_string()),
            &Redactor::new(Default::default()),
        )
        .unwrap();
        let public = projection.public_value().to_string();
        assert!(!public.contains(private_body));
        assert!(public.contains("Public overview"));
        let records = storage::parse_envelopes(projection.log()).unwrap();
        for record in &records[..6] {
            assert_eq!(record.content["message"]["content"][0]["text"], OMITTED);
        }
        assert_eq!(
            records[6].content["message"]["content"][0]["type"],
            "tool_use"
        );
        assert_eq!(
            records[7].content["message"]["content"][0]["type"],
            "tool_result"
        );
        let key = SecretKey::from([27; 32]);
        let recipient = ViewingRecipient::from_base64(
            "viewer".into(),
            &STANDARD.encode(key.public_key().as_bytes()),
        )
        .unwrap();
        let layer = projection
            .seal(&recipient)
            .unwrap()
            .open_layer(&key)
            .unwrap();
        let (private_log, private_view) = layer.session_bytes().unwrap();
        assert_eq!(private_log.as_str(), log);
        assert_eq!(private_view.as_str(), log);
    }

    #[test]
    fn reused_call_ids_and_unknown_shell_outputs_cannot_borrow_file_authorization() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src/main.rs");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        let session = format!("agit-{}", "a".repeat(40));
        let call = |name: &str| json!({"type":"response_item", "payload":{"type":"function_call", "name":name, "call_id":"reused", "arguments":json!({"file_path":source}).to_string()}});
        let raw = [
            call("Read"), call("shell"),
            json!({"type":"response_item", "payload":{"type":"function_call_output", "call_id":"reused", "output":"ambiguous body"}}),
            json!({"type":"future", "opaque":"unknown body"}),
        ].into_iter().map(|v| format!("{v}\n")).collect::<String>();
        let log = transcript::wrap_lines(&raw, "codex", &session);
        let policy = PrivacyPolicy {
            workspace: Some(temp.path().into()),
            ..Default::default()
        };
        let projection = project_session(
            &policy,
            &mut PathAliasStore::default(),
            None,
            &log,
            &log,
            &Meta::new(session, "codex".into(), temp.path().display().to_string()),
            &Redactor::new(Default::default()),
        )
        .unwrap();
        assert!(!projection.log().contains("ambiguous body"));
        assert!(!projection.log().contains("unknown body"));
        assert_eq!(projection.report.omissions.len(), 4);
    }

    #[test]
    fn file_inputs_require_authorization_with_or_without_call_ids() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/main.rs"), "allowed").unwrap();
        let session = format!("agit-{}", "a".repeat(40));
        let mut blocks = Vec::new();
        for name in ["Write", "Edit"] {
            for (index, id) in [Some("valid"), None, Some("")].into_iter().enumerate() {
                for (path, body) in [
                    ("private.txt", "EXCLUDED_BODY"),
                    ("src/main.rs", "ALLOWED_BODY"),
                ] {
                    let input = if name == "Write" {
                        json!({"file_path":path,"content":body})
                    } else {
                        json!({"file_path":path,"old_string":body,"new_string":body})
                    };
                    let mut block = json!({"type":"tool_use","name":name,"input":input});
                    if let Some(id) = id {
                        block["id"] = json!(if id.is_empty() {
                            String::new()
                        } else {
                            format!("{name}-{path}-{index}")
                        });
                    }
                    blocks.push(block);
                }
            }
        }
        let raw = format!(
            "{}\n",
            json!({"type":"assistant","cwd":temp.path(),"message":{"role":"assistant","content":blocks}})
        );
        let log = transcript::wrap_lines(&raw, "claude-code", &session);
        let projection = project_session(
            &PrivacyPolicy {
                workspace: Some(temp.path().into()),
                ..Default::default()
            },
            &mut PathAliasStore::default(),
            None,
            &log,
            &log,
            &Meta::new(
                session,
                "claude-code".into(),
                temp.path().display().to_string(),
            ),
            &Redactor::new(Default::default()),
        )
        .unwrap();
        assert!(!projection.log().contains("EXCLUDED_BODY"));
        let records = storage::parse_envelopes(projection.log()).unwrap();
        let projected = records[0].content["message"]["content"].as_array().unwrap();
        for (source, projected) in blocks.iter().zip(projected) {
            if source["input"]["file_path"] == "private.txt" {
                assert_eq!(projected["text"], OMITTED);
            } else {
                assert_eq!(projected["type"], "tool_use");
                assert_eq!(projected["input"]["file_path"], "<workspace>/src/main.rs");
                assert!(projected.to_string().contains("ALLOWED_BODY"));
            }
        }
        assert_eq!(projection.report.omissions.len(), 6);
    }

    #[test]
    fn json_tool_results_retain_metadata_and_array_structure() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        let path = temp.path().join("src/main.rs");
        let results = [
            json!({"status":200,"mime_type":"application/json","body":{"message":"RETAIN_ME","secret":"SECRET_VALUE","path":path},"metadata":{"mimeType":"text/plain","media_type":"application/json","data":"RETAIN_DATA"}}),
            json!([
                {"type":"file","name":"main.rs","path":path,"secret":"SECRET_VALUE"},
                {"type":"directory","name":"src"}
            ]),
            json!(["SECRET_VALUE",7,true,null,{"type":"text","text":"RETAIN_ROW","row":1},[path,"SECRET_VALUE"]]),
            json!([]),
        ];
        let (projection, _) = project_tool_results(&results, temp.path());
        let expected = [
            json!({"status":200,"mime_type":"application/json","body":{"message":"RETAIN_ME","secret":"MASKED","path":"<workspace>/src/main.rs"},"metadata":{"mimeType":"text/plain","media_type":"application/json","data":"RETAIN_DATA"}}),
            json!([
                {"type":"file","name":"main.rs","path":"<workspace>/src/main.rs","secret":"MASKED"},
                {"type":"directory","name":"src"}
            ]),
            json!(["MASKED",7,true,null,{"type":"text","text":"RETAIN_ROW","row":1},["<workspace>/src/main.rs","MASKED"]]),
            json!([]),
        ];
        let records = storage::parse_envelopes(projection.log()).unwrap();
        assert_eq!(records.len(), expected.len() + 1);
        for (record, expected) in records.iter().skip(1).zip(expected) {
            let content = record.content["message"]["content"][0]["content"]
                .as_str()
                .unwrap();
            assert_eq!(serde_json::from_str::<Value>(content).unwrap(), expected);
        }
        assert!(projection.report.omissions.is_empty());
    }

    #[test]
    fn explicit_content_blocks_omit_attachment_payloads_and_recover_originals() {
        let temp = tempfile::tempdir().unwrap();
        let results = [json!([
            {"type":"text","text":"BEFORE SECRET_VALUE"},
            {"type":"file","file":{"file_data":"PRIVATE_FILE_BODY"}},
            {"type":"document","source":{"type":"text","data":"PRIVATE_DOCUMENT_BODY"}},
            {"mimeType":"image/png","data":"PRIVATE_IMAGE_BODY"},
            {"type":"output_text","text":"AFTER"}
        ])];
        let (projection, log) = project_tool_results(&results, temp.path());
        let records = storage::parse_envelopes(projection.log()).unwrap();
        assert_eq!(
            records[1].content["message"]["content"][0]["content"],
            format!("BEFORE MASKED\n{OMITTED}\n{OMITTED}\n{OMITTED}\nAFTER")
        );
        assert_eq!(projection.report.omissions.len(), 3);
        for notice in &projection.report.omissions {
            assert_eq!(notice.record, 1);
            assert_eq!(
                notice.reason,
                "attachment body is excluded from public content"
            );
        }
        let key = SecretKey::from([23; 32]);
        let recipient = ViewingRecipient::from_base64(
            "viewer".into(),
            &STANDARD.encode(key.public_key().as_bytes()),
        )
        .unwrap();
        let private = projection
            .seal(&recipient)
            .unwrap()
            .open_layer(&key)
            .unwrap();
        let (private_log, private_view) = private.session_bytes().unwrap();
        assert_eq!(private_log.as_str(), log);
        assert_eq!(private_view.as_str(), log);
    }

    fn project_tool_results(results: &[Value], workspace: &Path) -> (SessionProjection, String) {
        let session = format!("agit-{}", "c".repeat(40));
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        let call = json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"file","name":"Read","input":{"file_path":workspace.join("src/main.rs")}}]}});
        let raw = std::iter::once(call).chain(results.iter().map(|result| {
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"file","content":result}]}})
        })).map(|record| format!("{record}\n")).collect::<String>();
        let log = transcript::wrap_lines(&raw, "claude-code", &session);
        let policy = PrivacyPolicy {
            workspace: Some(workspace.into()),
            replacements: vec![super::super::privacy::ReplacementRule {
                pattern: "SECRET_VALUE".into(),
                replacement: "MASKED".into(),
                regex: false,
            }],
            ..Default::default()
        };
        let projection = project_session(
            &policy,
            &mut PathAliasStore::default(),
            None,
            &log,
            &log,
            &Meta::new(
                session,
                "claude-code".into(),
                workspace.display().to_string(),
            ),
            &Redactor::new(Default::default()),
        )
        .unwrap();
        (projection, log)
    }

    #[test]
    fn native_control_records_remain_private_in_minimal_metadata_mode() {
        let temp = tempfile::tempdir().unwrap();
        let session = format!("agit-{}", "b".repeat(40));
        let private = [
            "private-planning-branch",
            "https://private.example/organization/project.git",
            "Private launch instructions for the Elm project.",
            "Private developer instructions for this turn.",
            "Private future control payload.",
        ];
        let raw = [
            json!({"type":"session_meta","payload":{
                "cwd":temp.path(),"git":{"branch":private[0],"repository_url":private[1]},
                "base_instructions":{"text":private[2]}
            }}),
            json!({"type":"turn_context","payload":{"cwd":temp.path(),"developer_instructions":private[3]}}),
            json!({"type":"future_control","payload":{"nested":{"value":private[4]}}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Public question"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Public answer"}]}}),
        ].into_iter().map(|record| format!("{record}\n")).collect::<String>();
        let log = transcript::wrap_lines(&raw, "codex", &session);
        let metadata = Meta::new(session, "codex".into(), temp.path().display().to_string());
        let policy = PrivacyPolicy {
            workspace: Some(temp.path().into()),
            ..Default::default()
        };
        assert_eq!(
            policy.metadata,
            super::super::privacy::MetadataMode::Minimal
        );
        let projection = project_session(
            &policy,
            &mut PathAliasStore::default(),
            None,
            &log,
            &log,
            &metadata,
            &Redactor::new(Default::default()),
        )
        .unwrap();
        let public = projection.public_value().to_string();
        for value in private {
            assert!(
                !public.contains(value),
                "private control value escaped: {value}"
            );
        }
        assert!(projection.log().contains("Public question"));
        assert!(projection.log().contains("Public answer"));
        let records = storage::parse_envelopes(projection.log()).unwrap();
        for record in &records[..3] {
            assert_eq!(record.content["message"]["content"][0]["text"], OMITTED);
            assert_eq!(record.content["isMeta"], true);
        }
        assert_eq!(projection.report.omissions.len(), 3);
        assert!(transcript::display::parse(projection.log()).is_ok());

        let key = SecretKey::from([22; 32]);
        let recipient = ViewingRecipient::from_base64(
            "viewer".into(),
            &STANDARD.encode(key.public_key().as_bytes()),
        )
        .unwrap();
        let layer = projection
            .seal(&recipient)
            .unwrap()
            .open_layer(&key)
            .unwrap();
        assert_eq!(layer.session_bytes().unwrap().0.as_str(), log);
    }

    #[test]
    fn evidence_references_and_legacy_runtime_quotes_keep_source_restrictions() {
        let session = format!("agit-{}", "a".repeat(40));
        let raw = json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"missing-call","output":"PRIVATE_QUOTED_OUTPUT"}});
        let original = transcript::wrap_lines(&format!("{raw}\n"), "codex", &session);
        let record = storage::parse_envelopes(&original).unwrap().remove(0);
        let text = format!(
            "{EVIDENCE_PREFIX}{}",
            json!({"runtime":record.source,"session":record.session_id,"record":record.content})
        );
        let quote = |reference: Value| {
            transcript::wrap_lines(
                &format!(
                    "{}\n",
                    json!({"type":"user","message":{"role":"user","content":"PRIVATE_FORGED_QUOTE"},EVIDENCE_FIELD:reference})
                ),
                "claude-code",
                &session,
            )
        };
        let mut log = original;
        let quoted = quote(serde_json::to_value(EvidenceReference::from_record(&record)).unwrap());
        log.push_str(&quoted);
        let nested = storage::parse_envelopes(&quoted).unwrap().remove(0);
        log.push_str(&quote(
            serde_json::to_value(EvidenceReference::from_record(&nested)).unwrap(),
        ));
        let mut unsupported = EvidenceReference::from_record(&record);
        unsupported.version = 2;
        log.push_str(&quote(serde_json::to_value(unsupported).unwrap()));
        log.push_str(&quote(Value::Null));
        let legacy = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}})
            ),
            "codex",
            &session,
        );
        log.push_str(&legacy);
        let policy = PrivacyPolicy::default();
        let metadata = Meta::new(session, "codex".into(), String::new());
        let project = |log: &str| {
            project_session(
                &policy,
                &mut PathAliasStore::default(),
                None,
                log,
                log,
                &metadata,
                &Redactor::new(Default::default()),
            )
            .unwrap()
        };
        let projected = project(&log);
        assert!(!projected.log().contains("PRIVATE_QUOTED_OUTPUT"));
        assert!(!projected.log().contains("PRIVATE_FORGED_QUOTE"));
        assert!(projected.log().contains("policy-projected"));
        assert!(
            projected
                .report()
                .omissions
                .iter()
                .any(|notice| notice.reason == "recovered evidence source is unavailable")
        );
        let detached = project(&legacy);
        assert!(!detached.log().contains("PRIVATE_QUOTED_OUTPUT"));
        assert_eq!(
            detached.report().omissions[0].reason,
            "recovered evidence source is unavailable"
        );
    }
}
