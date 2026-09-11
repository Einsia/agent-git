//! Private merge exploration authority and its durable publication journal.
//!
//! A static runtime role names a journal; only the journal advances the native cursor.
//! Callers hold branch guards and sorted participating Link guards before this guard,
//! then acquire merge transaction control. Journal I/O never acquires those outer locks.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Read;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::domain::native_archive::Frontier;
use crate::domain::repo::{Repo, common_git_dir};

pub const DIRECTORY: &str = "AGIT_MERGE_ARCHIVES";
pub const VERSION: u32 = 1;
const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RECOVERY_BYTES: u64 = MAX_JOURNAL_BYTES * 2 + 1024;
const MAX_LINK_BYTES: usize = 64 * 1024;
const MAX_CLAIMS: usize = 128;
const MAX_ENTRIES: usize = 8192;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLinkKey {
    pub runtime: String,
    pub session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeArchiveRole {
    pub generation: String,
    pub slug: String,
    pub branch: String,
    pub origin_head: String,
    pub logical_session: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenMergeSource {
    pub reference: String,
    pub slug: String,
    pub branch: Option<String>,
    pub head: String,
    pub base: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplorationBinding {
    pub role: MergeArchiveRole,
    pub native: RuntimeLinkKey,
    pub installed: Frontier,
    pub source: FrozenMergeSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_target: Option<FrozenFileTarget>,
}

/// File merges publish shared files on this ref and exploration only on the role's session ref.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenFileTarget {
    pub branch: String,
    pub head: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveJournal {
    pub version: u32,
    pub binding: ExplorationBinding,
    pub phase: ArchivePhase,
    pub consumed: Frontier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode: Option<crate::domain::native_archive::opencode::State>,
    pub accepted_commit: Option<String>,
    pub publication: Option<PreparedArchivePublication>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<RetainedMergeLanding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort: Option<RetainedAbort>,
    pub previous_claims: Vec<PreviousClaim>,
    pub activation: Option<PreparedActivation>,
    pub detach: Option<PreparedDetach>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArchivePhase {
    Preparing,
    Open,
    Landed { merge_commit: String },
    Aborting,
    Aborted,
    Detached,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedArchivePublication {
    pub kind: ArchivePublicationKind,
    pub link_json: String,
    pub expected_old: String,
    pub candidate: String,
    pub candidate_tree: String,
    pub prior_frontier: Frontier,
    pub next_frontier: Frontier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_opencode: Option<crate::domain::native_archive::opencode::State>,
    pub appended_records: u64,
    pub protected_suffix_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArchivePublicationKind {
    MergeLanding { source_head: String },
    Tail,
}

/// Cleanup and replay use the frozen transaction and ordinary merge result after Git publication.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedMergeLanding {
    pub transaction_json: String,
    pub ordinary_tree: String,
    pub worktree_tree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_evidence: Option<String>,
}

/// Cancellation freezes the exact authority before restoring claims or retiring the transaction.
/// An unpublished candidate remains evidence even when its publication is explicitly cancelled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedAbort {
    pub expected_head: String,
    pub transaction_json: String,
    pub successor_json: Option<String>,
    pub activation: Option<PreparedActivation>,
    pub cancelled_publication: Option<PreparedArchivePublication>,
}

pub(crate) fn checked_landing_transaction(
    json: &str,
    binding: &ExplorationBinding,
) -> Result<crate::domain::mergetx::Tx> {
    let tx = checked_abort_transaction(json, binding, false)?;
    ensure!(
        tx.has_summary(),
        "archive landing has no complete merge summary"
    );
    Ok(tx)
}

pub(crate) fn checked_abort_transaction(
    json: &str,
    binding: &ExplorationBinding,
    allow_unbound: bool,
) -> Result<crate::domain::mergetx::Tx> {
    use crate::domain::mergetx::checked_activation_image;
    binding.role.validate(binding.role.origin_head.len())?;
    binding.validate_file_target(binding.role.origin_head.len())?;
    let tx = checked_activation_image(json)?;
    ensure!(
        tx.mode == Some(binding.mode())
            && (tx.exploration.as_ref() == Some(binding)
                || (allow_unbound && tx.exploration.is_none()))
            && tx.generation.as_ref() == Some(&binding.role.generation)
            && tx.target == binding.target_branch()
            && tx.target_head == binding.target_head()
            && tx.source == binding.source.reference
            && tx.source_repo.as_ref() == Some(&binding.source.slug)
            && tx.source_branch == binding.source.branch
            && tx.source_head == binding.source.head
            && (if tx.base.is_empty() {
                binding.source.base.is_none()
            } else {
                binding.source.base.as_ref() == Some(&tx.base)
            }),
        "archive disposition differs from its frozen transaction"
    );
    Ok(tx)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedActivation {
    pub successor_json: String,
    pub transaction_original_json: String,
    pub transaction_bound_json: String,
}

impl PreparedActivation {
    pub(crate) fn validate(&self, binding: &ExplorationBinding) -> Result<()> {
        use crate::domain::mergetx::checked_activation_image;
        use crate::domain::metadata_facts::JsonFacts;

        let original = checked_abort_transaction(&self.transaction_original_json, binding, true)?;
        let bound = checked_activation_image(&self.transaction_bound_json)?;
        ensure!(
            original.exploration.is_none() && bound.exploration.as_ref() == Some(binding),
            "archive activation differs from the frozen merge transaction"
        );
        let JsonFacts::Object(mut before) = JsonFacts::parse(&self.transaction_original_json)?
        else {
            anyhow::bail!("archive activation transaction must be an object");
        };
        let JsonFacts::Object(mut after) = JsonFacts::parse(&self.transaction_bound_json)? else {
            anyhow::bail!("archive activation transaction must be an object");
        };
        before.remove("exploration");
        after.remove("exploration");
        ensure!(
            before == after,
            "archive activation changes transaction facts beyond its binding"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviousClaim {
    pub native: RuntimeLinkKey,
    pub original_json: String,
    pub retired_json: String,
}

/// The destination candidate and both Link images exist before any detachment publication.
/// Destination-side admission and Git endpoint proof remain the command layer's responsibility.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedDetach {
    pub admission_id: String,
    pub destination_slug: String,
    pub destination_branch: String,
    pub expected_destination: Option<String>,
    pub candidate_destination: String,
    pub candidate_tree: String,
    pub old_link_json: String,
    pub new_link_json: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalSummary {
    pub generation: String,
    pub branch: String,
    pub phase: ArchivePhase,
    pub publication_pending: bool,
    pub detach_pending: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    version: u32,
    previous: Option<ArchiveJournal>,
    next: ArchiveJournal,
}

fn checked_generation(generation: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(generation).context("invalid archive generation")?;
    ensure!(
        parsed.get_version() == Some(uuid::Version::SortRand)
            && parsed.hyphenated().to_string() == generation,
        "archive generation must be a canonical transaction UUID"
    );
    Ok(())
}

fn checked_slug(slug: &str) -> Result<()> {
    let (owner, name) = slug
        .split_once('/')
        .context("invalid archive repository identity")?;
    for component in [owner, name] {
        ensure!(
            component == component.trim(),
            "archive identity has surrounding whitespace"
        );
        crate::domain::repo::valid_name(component)?;
        ensure!(
            component.len() <= 255,
            "archive identity component is too long"
        );
    }
    Ok(())
}

fn checked_branch(branch: &str) -> Result<()> {
    ensure!(branch.len() <= 1024, "archive branch is too long");
    ensure!(
        !branch.is_empty()
            && branch == branch.trim()
            && !branch.starts_with(['-', '/'])
            && branch != "HEAD"
            && branch != "@"
            && branch != "main"
            && !branch.ends_with(['/', '.'])
            && !branch.contains("..")
            && !branch.contains("//")
            && !branch.contains("@{")
            && !branch.contains(['~', '^', ':', '?', '*', '[', '\\', '\0'])
            && !branch.chars().any(|c| c.is_control() || c == ' ')
            && branch
                .split('/')
                .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock")),
        "invalid archive session branch"
    );
    crate::domain::repo::valid_branch_name(branch)
}

fn checked_hex(value: &str, width: usize) -> bool {
    value.len() == width
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn checked_oid(oid: &str, width: usize) -> Result<()> {
    ensure!(
        checked_hex(oid, width) && oid.bytes().any(|b| b != b'0'),
        "invalid archive object ID"
    );
    Ok(())
}

fn checked_frontier(frontier: &Frontier) -> Result<()> {
    ensure!(
        checked_hex(&frontier.sha256, 64),
        "invalid archive frontier digest"
    );
    if frontier.bytes == 0 {
        ensure!(
            frontier.sha256 == "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "empty archive frontier has a nonempty digest"
        );
    }
    Ok(())
}

impl RuntimeLinkKey {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            crate::adapter::RUNTIMES.contains(&self.runtime.as_str()),
            "unsupported archive runtime"
        );
        ensure!(
            !self.session_id.is_empty()
                && self.session_id.len() <= 255
                && self
                    .session_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "invalid archive native Link key"
        );
        Ok(())
    }
}

impl MergeArchiveRole {
    pub fn validate(&self, object_id_width: usize) -> Result<()> {
        ensure!(
            matches!(object_id_width, 40 | 64),
            "unsupported Git object format"
        );
        checked_generation(&self.generation)?;
        checked_slug(&self.slug)?;
        checked_branch(&self.branch)?;
        checked_oid(&self.origin_head, object_id_width)?;
        ensure!(
            crate::domain::meta::is_bare_id(&self.logical_session),
            "invalid archive logical session"
        );
        Ok(())
    }
}

impl ExplorationBinding {
    pub fn target_branch(&self) -> &str {
        self.file_target
            .as_ref()
            .map_or(&self.role.branch, |target| &target.branch)
    }

    pub fn target_head(&self) -> &str {
        self.file_target
            .as_ref()
            .map_or(&self.role.origin_head, |target| &target.head)
    }

    pub fn mode(&self) -> crate::domain::mergetx::Mode {
        if self.file_target.is_some() {
            crate::domain::mergetx::Mode::FileAgent
        } else {
            crate::domain::mergetx::Mode::SessionAgent
        }
    }

    fn validate_file_target(&self, width: usize) -> Result<()> {
        if let Some(target) = &self.file_target {
            if target.branch != "main" {
                checked_branch(&target.branch)?;
            }
            checked_oid(&target.head, width)?;
            ensure!(
                target.branch != self.role.branch
                    && target.head != self.role.origin_head
                    && self.role.branch == format!("merge-exploration/{}", self.role.generation),
                "file merge target and exploration session must have distinct frozen identities"
            );
        }
        Ok(())
    }
}

impl FrozenMergeSource {
    pub(crate) fn validate(&self, object_id_width: usize) -> Result<()> {
        checked_slug(&self.slug)?;
        ensure!(
            !self.reference.is_empty()
                && self.reference.len() <= 4096
                && !self.reference.chars().any(char::is_control),
            "invalid frozen merge source reference"
        );
        if let Some(branch) = &self.branch {
            // Frozen source names include file lines and historical selectors, never write refs.
            ensure!(
                !branch.is_empty()
                    && branch.len() <= 4096
                    && branch == branch.trim()
                    && !branch.chars().any(char::is_control),
                "invalid frozen merge source name"
            );
        }
        checked_oid(&self.head, object_id_width)?;
        if let Some(base) = &self.base {
            checked_oid(base, object_id_width)?;
        }
        Ok(())
    }
}

impl ArchiveJournal {
    /// Validation proves private state shape; the caller separately proves referenced Git objects.
    pub fn validate(&self, object_id_width: usize) -> Result<()> {
        ensure!(
            matches!(object_id_width, 40 | 64),
            "unsupported Git object format"
        );
        ensure!(
            self.version == VERSION,
            "unsupported merge archive journal version"
        );
        let binding = &self.binding;
        let role = &binding.role;
        checked_generation(&role.generation)?;
        checked_slug(&role.slug)?;
        checked_branch(&role.branch)?;
        checked_oid(&role.origin_head, object_id_width)?;
        ensure!(
            crate::domain::meta::is_bare_id(&role.logical_session),
            "invalid archive logical session"
        );
        binding.native.validate()?;
        checked_frontier(&binding.installed)?;
        checked_frontier(&self.consumed)?;
        ensure!(
            self.consumed.bytes >= binding.installed.bytes,
            "archive cursor precedes installation"
        );
        if self.consumed.bytes == binding.installed.bytes {
            ensure!(
                self.consumed == binding.installed,
                "archive installation digest changed"
            );
        }
        if let Some(state) = &self.opencode {
            ensure!(
                binding.native.runtime == "opencode",
                "non-OpenCode archive retains observation state"
            );
            state.validate(&binding.native.session_id)?;
        }
        binding.source.validate(object_id_width)?;
        binding.validate_file_target(object_id_width)?;
        if let Some(commit) = &self.accepted_commit {
            checked_oid(commit, object_id_width)?;
        }
        ensure!(
            self.previous_claims.len() <= MAX_CLAIMS,
            "too many archive previous claims"
        );
        let mut keys = BTreeSet::new();
        for previous in &self.previous_claims {
            previous.native.validate()?;
            ensure!(
                previous.native != binding.native && keys.insert(&previous.native),
                "duplicate archive participating Link"
            );
            checked_link_image(&previous.original_json)?;
            checked_link_image(&previous.retired_json)?;
            ensure!(
                previous.original_json != previous.retired_json,
                "archive retirement has identical Link images"
            );
        }
        if let Some(activation) = &self.activation {
            checked_link_image(&activation.successor_json)?;
            activation.validate(&self.binding)?;
        }
        if let Some(landing) = &self.landing {
            checked_landing_transaction(&landing.transaction_json, binding)?;
            checked_oid(&landing.ordinary_tree, object_id_width)?;
            ensure!(
                landing.file_commit.is_some() == binding.file_target.is_some()
                    && landing.file_evidence.is_some() == binding.file_target.is_some(),
                "file merge landing must retain both publication candidates"
            );
            if let Some(commit) = &landing.file_commit {
                checked_oid(commit, object_id_width)?;
                let evidence = landing.file_evidence.as_ref().unwrap();
                checked_oid(evidence, object_id_width)?;
                ensure!(
                    evidence != commit
                        && evidence != &role.origin_head
                        && self.publication.as_ref().is_none_or(|publication| {
                            publication.kind == ArchivePublicationKind::Tail
                                || &publication.candidate == evidence
                        }),
                    "file landing evidence differs from its retained candidate"
                );
                ensure!(
                    commit != binding.target_head()
                        && commit != &role.origin_head
                        && self.accepted_commit.as_ref() != Some(commit)
                        && self
                            .publication
                            .as_ref()
                            .is_none_or(|publication| &publication.candidate != commit),
                    "file merge result must remain separate from exploration history"
                );
                if let ArchivePhase::Landed { merge_commit } = &self.phase {
                    ensure!(
                        merge_commit == commit,
                        "file merge disposition differs from its retained candidate"
                    );
                }
            }
            if let Some(tree) = &landing.worktree_tree {
                checked_oid(tree, object_id_width)?;
            }
            ensure!(
                match self.phase {
                    ArchivePhase::Open => self.publication.as_ref().is_some_and(|publication| {
                        matches!(
                            publication.kind,
                            ArchivePublicationKind::MergeLanding { .. }
                        )
                    }),
                    ArchivePhase::Landed { .. } | ArchivePhase::Detached => true,
                    ArchivePhase::Aborting | ArchivePhase::Aborted => self
                        .abort
                        .as_ref()
                        .is_some_and(|abort| abort.cancelled_publication.is_some()),
                    _ => false,
                },
                "retained merge landing contradicts its disposition"
            );
        }
        match &self.phase {
            ArchivePhase::Preparing
            | ArchivePhase::Open
            | ArchivePhase::Aborting
            | ArchivePhase::Aborted => {
                ensure!(
                    self.accepted_commit.is_none() && self.consumed == binding.installed,
                    "unlanded archive has a publication cursor"
                );
            }
            ArchivePhase::Landed { merge_commit } => {
                checked_oid(merge_commit, object_id_width)?;
                ensure!(
                    self.accepted_commit.is_some(),
                    "landed archive has no accepted commit"
                );
                ensure!(
                    binding.file_target.is_none() || self.landing.is_some(),
                    "file merge disposition has no retained candidates"
                );
            }
            ArchivePhase::Detached => {}
        }
        ensure!(
            match self.phase {
                ArchivePhase::Preparing => self.activation.is_some(),
                ArchivePhase::Aborting => true,
                _ => self.activation.is_none(),
            },
            "archive activation contradicts its phase"
        );
        if !matches!(
            self.phase,
            ArchivePhase::Preparing
                | ArchivePhase::Open
                | ArchivePhase::Aborting
                | ArchivePhase::Aborted
        ) {
            ensure!(
                self.previous_claims.is_empty(),
                "completed archive retains claim restoration intent"
            );
        }
        if let Some(publication) = &self.publication {
            checked_link_image(&publication.link_json)?;
            ensure!(
                self.detach.is_none(),
                "archive publication and detachment overlap"
            );
            for oid in [
                &publication.expected_old,
                &publication.candidate,
                &publication.candidate_tree,
            ] {
                checked_oid(oid, object_id_width)?;
            }
            ensure!(
                publication.expected_old != publication.candidate,
                "archive publication does not advance Git"
            );
            checked_frontier(&publication.prior_frontier)?;
            checked_frontier(&publication.next_frontier)?;
            ensure!(
                publication.next_opencode.is_some() == self.opencode.is_some(),
                "archive publication changes its observation cursor family"
            );
            if let (Some(prior), Some(next)) = (&self.opencode, &publication.next_opencode) {
                prior.validate_successor(next, publication.appended_records)?;
            }
            if publication.appended_records == 0 {
                ensure!(
                    publication.next_opencode == self.opencode,
                    "empty archive publication changes observation state"
                );
            }
            ensure!(
                publication.prior_frontier == self.consumed,
                "archive publication does not start at the consumed cursor"
            );
            ensure!(
                publication.next_frontier.bytes >= publication.prior_frontier.bytes,
                "archive publication moves its cursor backward"
            );
            ensure!(
                checked_hex(&publication.protected_suffix_sha256, 64),
                "invalid protected archive suffix digest"
            );
            if publication.appended_records == 0 {
                ensure!(
                    publication.prior_frontier == publication.next_frontier,
                    "empty archive publication advances its cursor"
                );
            } else {
                ensure!(
                    publication.next_frontier.bytes > publication.prior_frontier.bytes,
                    "archive records do not advance the cursor"
                );
            }
            match (&self.phase, &publication.kind) {
                (ArchivePhase::Open, ArchivePublicationKind::MergeLanding { source_head }) => {
                    ensure!(
                        source_head == &binding.source.head
                            && publication.expected_old == role.origin_head,
                        "archive landing differs from its frozen merge selection"
                    );
                    ensure!(
                        binding.file_target.is_none() || self.landing.is_some(),
                        "file merge publication has no retained file result"
                    );
                }
                (ArchivePhase::Landed { .. }, ArchivePublicationKind::Tail) => {
                    ensure!(
                        publication.appended_records > 0,
                        "empty archive tail publication"
                    );
                }
                _ => anyhow::bail!("archive publication kind contradicts its phase"),
            }
        }
        ensure!(
            !matches!(self.phase, ArchivePhase::Aborting | ArchivePhase::Aborted)
                || self.abort.is_some(),
            "archive cancellation has no retained authority"
        );
        if let Some(abort) = &self.abort {
            ensure!(
                matches!(
                    self.phase,
                    ArchivePhase::Aborting | ArchivePhase::Aborted | ArchivePhase::Detached
                ) && self.publication.is_none()
                    && abort.expected_head == role.origin_head,
                "retained cancellation contradicts its disposition or expected head"
            );
            checked_abort_transaction(
                &abort.transaction_json,
                binding,
                abort.activation.is_some(),
            )?;
            ensure!(
                self.activation.as_ref()
                    == if self.phase == ArchivePhase::Aborting {
                        abort.activation.as_ref()
                    } else {
                        None
                    },
                "cancellation activation evidence differs from its retained disposition"
            );
            if let Some(activation) = &abort.activation {
                activation.validate(binding)?;
                ensure!(
                    abort.cancelled_publication.is_none()
                        && (abort.transaction_json == activation.transaction_original_json
                            || abort.transaction_json == activation.transaction_bound_json)
                        && abort
                            .successor_json
                            .as_ref()
                            .is_none_or(|json| { json == &activation.successor_json }),
                    "cancellation differs from retained activation endpoints"
                );
            } else {
                ensure!(
                    abort.successor_json.is_some(),
                    "activated cancellation has no successor Link image"
                );
            }
            if let Some(json) = &abort.successor_json {
                let selected = crate::domain::link::parse_archive_link_image(
                    &binding.native.runtime,
                    &binding.native.session_id,
                    json.clone(),
                )?;
                ensure!(
                    selected.link.is_archive_for(
                        role,
                        &binding.native.runtime,
                        &binding.native.session_id
                    ) && selected.link.baseline_bytes == Some(binding.installed.bytes)
                        && selected.link.baseline_hash.as_ref() == Some(&binding.installed.sha256),
                    "cancelled successor differs from its retained archive authority"
                );
            }
            if let Some(publication) = &abort.cancelled_publication {
                ensure!(
                    self.landing.as_ref().is_some_and(|landing| {
                        landing.transaction_json == abort.transaction_json
                    }) && abort.successor_json.as_ref() == Some(&publication.link_json)
                        && publication.expected_old == abort.expected_head,
                    "cancelled publication differs from its exact landing endpoints"
                );
                let mut pending = self.clone();
                pending.phase = ArchivePhase::Open;
                pending.abort = None;
                pending.publication = Some(publication.clone());
                pending.validate(object_id_width)?;
            } else {
                ensure!(
                    self.landing.is_none(),
                    "cancelled landing receipt is missing"
                );
            }
        }
        if let Some(detach) = &self.detach {
            ensure!(
                matches!(
                    self.phase,
                    ArchivePhase::Landed { .. } | ArchivePhase::Aborted
                ),
                "archive detachment requires a completed merge disposition"
            );
            checked_slug(&detach.destination_slug)?;
            checked_generation(&detach.admission_id)?;
            checked_branch(&detach.destination_branch)?;
            ensure!(
                detach.destination_slug != role.slug || detach.destination_branch != role.branch,
                "archive detachment reuses its original line"
            );
            if let Some(expected) = &detach.expected_destination {
                checked_oid(expected, object_id_width)?;
            }
            checked_oid(&detach.candidate_destination, object_id_width)?;
            checked_oid(&detach.candidate_tree, object_id_width)?;
            ensure!(
                detach.expected_destination.as_ref() != Some(&detach.candidate_destination),
                "detachment destination does not advance"
            );
            checked_link_image(&detach.old_link_json)?;
            checked_link_image(&detach.new_link_json)?;
            ensure!(
                detach.old_link_json != detach.new_link_json,
                "detachment has identical Link images"
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() as u64 <= MAX_JOURNAL_BYTES,
            "archive journal exceeds its storage budget"
        );
        Ok(())
    }
}

// Authority fields are validated without projecting away unrelated local metadata.
#[derive(Deserialize)]
struct LinkImage {
    cwd: Option<String>,
    agent: Option<String>,
    owner: Option<String>,
    branch: Option<String>,
    baseline_bytes: Option<u64>,
    baseline_hash: Option<String>,
    materialized_from: Option<String>,
    superseded_by: Option<String>,
    #[serde(default)]
    naming_ignored: bool,
    merge_archive: Option<MergeArchiveRole>,
}

pub(crate) fn checked_link_image(text: &str) -> Result<()> {
    ensure!(
        text.len() <= MAX_LINK_BYTES,
        "archive Link image exceeds its storage budget"
    );
    ensure!(
        text.trim_start().starts_with('{'),
        "archive Link image must be a JSON object"
    );
    let image: LinkImage = serde_json::from_str(text).context("invalid archive Link image")?;
    let LinkImage {
        cwd,
        agent,
        owner,
        branch,
        baseline_bytes,
        baseline_hash,
        materialized_from,
        superseded_by,
        naming_ignored,
        merge_archive,
    } = image;
    drop((
        cwd,
        agent,
        owner,
        branch,
        baseline_bytes,
        baseline_hash,
        materialized_from,
        superseded_by,
        naming_ignored,
        merge_archive,
    ));
    Ok(())
}

fn checked_transition(previous: &ArchiveJournal, next: &ArchiveJournal) -> Result<()> {
    ensure!(
        previous.binding == next.binding && previous.version == next.version,
        "archive binding is immutable"
    );
    if previous == next {
        return Ok(());
    }
    let phase_ok = match (&previous.phase, &next.phase) {
        (ArchivePhase::Preparing, ArchivePhase::Open | ArchivePhase::Aborting)
        | (
            ArchivePhase::Open,
            ArchivePhase::Open | ArchivePhase::Aborting | ArchivePhase::Landed { .. },
        )
        | (ArchivePhase::Aborting, ArchivePhase::Aborted)
        | (ArchivePhase::Aborted, ArchivePhase::Aborted | ArchivePhase::Detached)
        | (ArchivePhase::Landed { .. }, ArchivePhase::Detached) => true,
        (
            ArchivePhase::Landed { merge_commit: old },
            ArchivePhase::Landed { merge_commit: new },
        ) => old == new,
        _ => false,
    };
    ensure!(phase_ok, "invalid archive phase transition");
    if matches!(next.phase, ArchivePhase::Open | ArchivePhase::Aborting) {
        ensure!(
            previous.previous_claims == next.previous_claims,
            "archive claim restoration receipt changed"
        );
    }
    let cancelled_publication = if matches!(next.phase, ArchivePhase::Aborting) {
        ensure!(
            previous.abort.is_none()
                && next.abort.is_some()
                && previous.activation == next.activation
                && next.abort.as_ref().unwrap().activation == previous.activation
                && next.abort.as_ref().unwrap().cancelled_publication == previous.publication,
            "archive abort must retain pending publication and activation evidence"
        );
        previous.publication.as_ref()
    } else {
        None
    };
    if let Some(abort) = &previous.abort {
        ensure!(
            next.abort.as_ref() == Some(abort)
                && (matches!(next.phase, ArchivePhase::Detached)
                    || (previous.previous_claims == next.previous_claims
                        && next.activation.is_none())),
            "retained cancellation authority cannot be replaced or discarded"
        );
    } else if next.abort.is_some() {
        ensure!(
            matches!(previous.phase, ArchivePhase::Preparing | ArchivePhase::Open)
                && next.phase == ArchivePhase::Aborting,
            "cancellation authority must precede claim restoration"
        );
    }
    let completed_publication = previous.publication.as_ref().filter(|publication| {
        next.publication.is_none()
            && next.accepted_commit.as_ref() == Some(&publication.candidate)
            && next.consumed == publication.next_frontier
            && next.opencode == publication.next_opencode
    });
    if previous.publication.is_some() && next.publication.is_none() {
        ensure!(
            completed_publication.is_some() || cancelled_publication.is_some(),
            "archive publication intent cleared without completion"
        );
    }
    if previous.consumed != next.consumed
        || previous.opencode != next.opencode
        || previous.accepted_commit != next.accepted_commit
    {
        ensure!(
            completed_publication.is_some(),
            "archive cursor advanced without its prepared publication"
        );
    }
    if let (ArchivePhase::Open, ArchivePhase::Landed { merge_commit }) =
        (&previous.phase, &next.phase)
    {
        ensure!(
            completed_publication.is_some_and(|publication| {
                previous
                    .landing
                    .as_ref()
                    .and_then(|landing| landing.file_commit.as_ref())
                    .unwrap_or(&publication.candidate)
                    == merge_commit
            }),
            "archive landing has no matching prepared candidate"
        );
    }
    if let (Some(old), Some(new)) = (&previous.publication, &next.publication) {
        ensure!(
            old == new,
            "archive publication receipt cannot be rewritten"
        );
    }
    if previous.landing.is_some() {
        ensure!(
            previous.landing == next.landing,
            "retained merge landing cannot be rewritten or discarded"
        );
    } else if next.landing.is_some() {
        ensure!(
            previous.phase == ArchivePhase::Open
                && previous.publication.is_none()
                && next.phase == ArchivePhase::Open,
            "merge landing evidence must precede Git publication"
        );
    }
    if let (Some(old), Some(new)) = (&previous.detach, &next.detach) {
        ensure!(old == new, "archive detachment receipt cannot be rewritten");
    }
    if previous.detach.is_some() && next.detach.is_none() {
        ensure!(
            matches!(next.phase, ArchivePhase::Detached),
            "archive detachment intent cleared without completion"
        );
    }
    if matches!(next.phase, ArchivePhase::Detached) {
        ensure!(
            previous.detach.is_some() && next.detach.is_none(),
            "archive detachment has no prepared intent"
        );
    }
    Ok(())
}

fn object_id_width(repo_root: &Path) -> Result<usize> {
    match Repo::at(repo_root)
        .git(&["rev-parse", "--show-object-format"])?
        .as_str()
    {
        "sha1" => Ok(40),
        "sha256" => Ok(64),
        _ => anyhow::bail!("unsupported merge archive Git object format"),
    }
}

fn namespace(repo_root: &Path) -> Result<PathBuf> {
    ensure!(
        repo_root.is_absolute(),
        "archive repository root must be absolute"
    );
    let common = common_git_dir(repo_root);
    checked_carrier(&common, true, false)?
        .context("archive repository has no common Git directory")?;
    Ok(common.join(DIRECTORY))
}

fn checked_carrier(
    path: &Path,
    directory: bool,
    private: bool,
) -> Result<Option<std::fs::Metadata>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot inspect archive state carrier"),
    };
    ensure!(
        !metadata.file_type().is_symlink()
            && if directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            },
        "archive state carrier is not an ordinary file or directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let forbidden = if private { 0o077 } else { 0o022 };
        ensure!(
            metadata.mode() & forbidden == 0 && metadata.uid() == unsafe { libc::geteuid() },
            "archive state carrier is not controlled by its owner"
        );
        validate_unix_ancestors(path)?;
    }
    #[cfg(windows)]
    crate::infra::windows_security::validate_path(path, directory, private)?;
    Ok(Some(metadata))
}

#[cfg(unix)]
fn validate_unix_ancestors(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let absolute = std::path::absolute(path)?;
    let canonical = std::fs::canonicalize(path)?;
    let current_user = unsafe { libc::geteuid() };
    let mut visited = BTreeSet::new();
    for ancestor in absolute
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .chain(canonical.parent().into_iter().flat_map(Path::ancestors))
    {
        if !visited.insert(ancestor) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(ancestor)?;
        ensure!(
            (metadata.uid() == current_user || metadata.uid() == 0)
                && (metadata.is_dir() || metadata.file_type().is_symlink()),
            "archive authority has an uncontrolled ancestor"
        );
        // A sticky ancestor protects owned children even when other users may create siblings.
        ensure!(
            metadata.file_type().is_symlink()
                || metadata.mode() & 0o022 == 0
                || metadata.mode() & 0o1000 != 0,
            "archive authority ancestor permits replacement by another user"
        );
    }
    Ok(())
}

pub(crate) fn authority_directory_exists(path: &Path) -> Result<bool> {
    Ok(checked_carrier(path, true, false)?.is_some())
}

fn open_file(path: &Path, write: bool, create: bool) -> Result<File> {
    open_carrier(path, write, create, true)
}

fn open_carrier(path: &Path, write: bool, create: bool, private: bool) -> Result<File> {
    checked_carrier(path, false, private)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(windows)]
    let file = if private && create {
        ensure!(
            write,
            "creating archive control state requires write access"
        );
        crate::infra::windows_security::open_private_control(path)
    } else {
        options.open(path)
    }
    .context("cannot open archive state carrier")?;
    #[cfg(not(windows))]
    let file = options
        .open(path)
        .context("cannot open archive state carrier")?;
    ensure!(
        file.metadata()?.is_file(),
        "archive state carrier changed type"
    );
    checked_carrier(path, false, private)?.context("archive state carrier disappeared")?;
    Ok(file)
}

/// Ordinary authority can be readable, but its path and ancestry remain owner-controlled.
pub(crate) fn read_transition_bytes(path: &Path, budget: u64) -> Result<Option<Vec<u8>>> {
    read_carrier_bytes(path, budget, false)
}

fn read_bytes(path: &Path, budget: u64) -> Result<Option<Vec<u8>>> {
    read_carrier_bytes(path, budget, true)
}

/// Retained private evidence must reject readable carriers before consuming their contents.
pub(crate) fn read_private_bytes(path: &Path, budget: u64) -> Result<Option<Vec<u8>>> {
    read_bytes(path, budget)
}

fn read_carrier_bytes(path: &Path, budget: u64, private: bool) -> Result<Option<Vec<u8>>> {
    let Some(metadata) = checked_carrier(path, false, private)? else {
        return Ok(None);
    };
    ensure!(
        metadata.len() <= budget,
        "archive state carrier exceeds its storage budget"
    );
    let mut bytes = Vec::new();
    open_carrier(path, false, false, private)?
        .take(budget + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= budget,
        "archive state carrier grew beyond its storage budget"
    );
    Ok(Some(bytes))
}

struct JournalPair {
    main: Option<ArchiveJournal>,
    recovery: Option<Recovery>,
}

impl JournalPair {
    fn settled(self) -> Result<Option<ArchiveJournal>> {
        ensure!(
            match (&self.main, &self.recovery) {
                (None, None) => true,
                (Some(main), Some(recovery)) => main == &recovery.next,
                _ => false,
            },
            "archive journal has pending recovery; replay its retained intent before continuing"
        );
        Ok(self.main)
    }
}

fn read_pair(directory: &Path, generation: &str, width: usize) -> Result<JournalPair> {
    let main = read_bytes(
        &directory.join(format!("{generation}.json")),
        MAX_JOURNAL_BYTES,
    )?
    .map(|bytes| serde_json::from_slice::<ArchiveJournal>(&bytes))
    .transpose()?;
    let recovery = read_bytes(
        &directory.join(format!("{generation}.recovery")),
        MAX_RECOVERY_BYTES,
    )?
    .map(|bytes| serde_json::from_slice::<Recovery>(&bytes))
    .transpose()?;
    if let Some(journal) = &main {
        journal.validate(width)?;
        ensure!(
            journal.binding.role.generation == generation,
            "archive filename differs from its generation"
        );
    }
    if let Some(recovery) = &recovery {
        ensure!(
            recovery.version == VERSION,
            "unsupported archive recovery version"
        );
        recovery.next.validate(width)?;
        ensure!(
            recovery.next.binding.role.generation == generation,
            "archive recovery generation differs from its filename"
        );
        if let Some(previous) = &recovery.previous {
            previous.validate(width)?;
            checked_transition(previous, &recovery.next)?;
        } else {
            ensure!(
                matches!(recovery.next.phase, ArchivePhase::Preparing),
                "archive creation recovery is not preparing"
            );
        }
        ensure!(
            match &main {
                Some(main) => main == &recovery.next || recovery.previous.as_ref() == Some(main),
                None => recovery.previous.is_none(),
            },
            "archive recovery has no matching primary state; repair the retained operation before continuing"
        );
    } else {
        ensure!(
            main.is_none(),
            "archive primary state has no durability receipt"
        );
    }
    Ok(JournalPair { main, recovery })
}

/// Read existing authority without creating a directory or lock carrier.
pub fn read(repo_root: &Path, generation: &str) -> Result<Option<ArchiveJournal>> {
    inspect(repo_root, generation, false)
}

/// Transaction control excludes activation writers while ordinary progress checks retained intent.
/// This check takes no journal lock, preserving the branch, Link, journal, transaction lock order.
pub(crate) fn has_retained_intent(common_git_dir: &Path, generation: &str) -> Result<bool> {
    if checked_generation(generation).is_err() {
        return Ok(false);
    }
    checked_carrier(common_git_dir, true, false)?
        .context("archive transaction has no common Git directory")?;
    let directory = common_git_dir.join(DIRECTORY);
    if checked_carrier(&directory, true, true)?.is_none() {
        return Ok(false);
    }
    for extension in ["json", "recovery"] {
        if checked_carrier(
            &directory.join(format!("{generation}.{extension}")),
            false,
            true,
        )?
        .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Transaction control keeps the journal stable while progress admission checks activation state.
/// Preparing retains exact transaction endpoints, so progress waits until durable Open publication.
pub(crate) fn require_open_progress(
    common_git_dir: &Path,
    binding: &ExplorationBinding,
) -> Result<()> {
    checked_generation(&binding.role.generation)?;
    checked_carrier(common_git_dir, true, false)?
        .context("archive transaction has no common Git directory")?;
    let directory = common_git_dir.join(DIRECTORY);
    checked_carrier(&directory, true, true)?
        .context("archive transaction has no journal directory")?;
    let journal = read_pair(
        &directory,
        &binding.role.generation,
        binding.role.origin_head.len(),
    )?
    .settled()?
    .context("archive transaction has no durable journal")?;
    ensure!(
        journal.binding == *binding
            && journal.phase == ArchivePhase::Open
            && journal.publication.is_none(),
        "archive transaction progress requires its matching Open journal without pending publication"
    );
    Ok(())
}

/// Transaction control serializes this settled disposition check with all journal writers.
pub(crate) fn require_landed_transaction(
    common_git_dir: &Path,
    binding: &ExplorationBinding,
    transaction_json: &str,
    merge_commit: &str,
) -> Result<()> {
    checked_generation(&binding.role.generation)?;
    checked_carrier(common_git_dir, true, false)?.context("merge common directory is missing")?;
    let directory = common_git_dir.join(DIRECTORY);
    checked_carrier(&directory, true, true)?.context("archive journal directory is missing")?;
    let journal = read_pair(
        &directory,
        &binding.role.generation,
        binding.role.origin_head.len(),
    )?
    .settled()?
    .context("archive landing has no settled journal")?;
    ensure!(
        journal.binding == *binding
            && journal.phase
                == (ArchivePhase::Landed {
                    merge_commit: merge_commit.to_owned()
                })
            && journal
                .landing
                .as_ref()
                .map(|landing| landing.transaction_json.as_str())
                == Some(transaction_json),
        "transaction completion differs from the retained merge landing"
    );
    Ok(())
}

pub(crate) fn require_aborted_transaction(
    common_git_dir: &Path,
    binding: &ExplorationBinding,
    transaction_json: &str,
) -> Result<()> {
    checked_generation(&binding.role.generation)?;
    checked_carrier(common_git_dir, true, false)?.context("merge common directory is missing")?;
    let directory = common_git_dir.join(DIRECTORY);
    checked_carrier(&directory, true, true)?.context("archive journal directory is missing")?;
    let journal = read_pair(
        &directory,
        &binding.role.generation,
        binding.role.origin_head.len(),
    )?
    .settled()?
    .context("archive cancellation has no settled journal")?;
    ensure!(
        journal.binding == *binding
            && journal.phase == ArchivePhase::Aborted
            && journal
                .abort
                .as_ref()
                .is_some_and(|abort| { abort.transaction_json == transaction_json }),
        "transaction completion differs from the retained archive cancellation"
    );
    Ok(())
}

/// A retained endpoint proves removal when persistence fails after its namespace transition.
pub(crate) fn durable_retire_transition_bytes(
    path: &Path,
    retired: &Path,
    expected: &[u8],
) -> Result<()> {
    retire_transition_with(path, retired, expected, &mut |_| Ok(()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetirementPoint {
    CarrierVisible,
    CarrierDurable,
    Removed,
}

fn retire_transition_with(
    path: &Path,
    retired: &Path,
    expected: &[u8],
    checkpoint: &mut impl FnMut(RetirementPoint) -> Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .context("transaction has no parent directory")?;
    ensure!(
        retired.parent() == Some(parent) && retired != path,
        "transaction retirement must remain in its original directory"
    );
    checked_carrier(parent, true, false)?.context("transaction directory is missing")?;
    let current = read_transition_bytes(path, expected.len() as u64)?;
    let completed = read_transition_bytes(retired, expected.len() as u64)?;
    match (current, completed) {
        (Some(current), None) if current == expected => {
            checked_carrier(path, false, true)?.context("retired transaction must be private")?;
            #[cfg(unix)]
            {
                std::fs::hard_link(path, retired)?;
                checkpoint(RetirementPoint::CarrierVisible)?;
                finish_retirement_unix(path, retired, expected, checkpoint)?;
            }
            #[cfg(windows)]
            {
                move_write_through(path, retired, false)?;
                checkpoint(RetirementPoint::CarrierVisible)?;
                checkpoint(RetirementPoint::CarrierDurable)?;
                checkpoint(RetirementPoint::Removed)?;
            }
        }
        (None, Some(completed)) if completed == expected => {
            checked_carrier(retired, false, true)?.context("retired transaction is not private")?;
            #[cfg(windows)]
            durable_publish_bytes(retired, expected, true)?;
        }
        #[cfg(unix)]
        (Some(current), Some(completed)) if current == expected && completed == expected => {
            finish_retirement_unix(path, retired, expected, checkpoint)?;
        }
        _ => anyhow::bail!("transaction retirement is outside its exact retained endpoints"),
    }
    checked_carrier(retired, false, true)?.context("completion carrier is not private")?;
    ensure!(
        read_transition_bytes(retired, expected.len() as u64)?.as_deref() == Some(expected),
        "transaction completion carrier changed during retirement"
    );
    #[cfg(unix)]
    File::open(parent)?
        .sync_all()
        .context("transaction retirement directory persistence failed")?;
    Ok(())
}

#[cfg(unix)]
fn finish_retirement_unix(
    path: &Path,
    retired: &Path,
    expected: &[u8],
    checkpoint: &mut impl FnMut(RetirementPoint) -> Result<()>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let current =
        checked_carrier(path, false, true)?.context("retiring transaction disappeared")?;
    let completed =
        checked_carrier(retired, false, true)?.context("completion carrier disappeared")?;
    ensure!(
        current.dev() == completed.dev() && current.ino() == completed.ino(),
        "transaction completion carrier is not the retained original file"
    );
    File::open(path.parent().context("transaction has no directory")?)?.sync_all()?;
    checkpoint(RetirementPoint::CarrierDurable)?;
    ensure!(
        read_transition_bytes(path, expected.len() as u64)?.as_deref() == Some(expected),
        "transaction changed before its retirement"
    );
    let current =
        checked_carrier(path, false, true)?.context("retiring transaction disappeared")?;
    let completed =
        checked_carrier(retired, false, true)?.context("completion carrier disappeared")?;
    ensure!(
        current.dev() == completed.dev() && current.ino() == completed.ino(),
        "transaction endpoint changed before retirement"
    );
    std::fs::remove_file(path)?;
    checkpoint(RetirementPoint::Removed)?;
    Ok(())
}

/// Preparation selects retained participants without publishing journal recovery out of lock order.
pub(crate) fn read_preparation_intent(
    repo_root: &Path,
    generation: &str,
) -> Result<Option<ArchiveJournal>> {
    inspect(repo_root, generation, true)
}

fn inspect(repo_root: &Path, generation: &str, pending: bool) -> Result<Option<ArchiveJournal>> {
    checked_generation(generation)?;
    let directory = namespace(repo_root)?;
    if checked_carrier(&directory, true, true)?.is_none() {
        return Ok(None);
    }
    let control = directory.join(format!("{generation}.control"));
    if checked_carrier(&control, false, true)?.is_none() {
        ensure!(
            checked_carrier(&directory.join(format!("{generation}.json")), false, true)?.is_none()
                && checked_carrier(
                    &directory.join(format!("{generation}.recovery")),
                    false,
                    true
                )?
                .is_none(),
            "archive journal has no control carrier"
        );
        return Ok(None);
    }
    let file = open_file(&control, false, false)?;
    fs2::FileExt::lock_shared(&file).context("cannot lock archive journal for inspection")?;
    let pair = read_pair(&directory, generation, object_id_width(repo_root)?)?;
    if pending {
        Ok(pair.recovery.map(|recovery| recovery.next).or(pair.main))
    } else {
        pair.settled()
    }
}

/// Enumerate every recognized journal strictly before selecting the requested branch.
pub fn probe_for_branch(repo_root: &Path, slug: &str, branch: &str) -> Result<Vec<JournalSummary>> {
    checked_slug(slug)?;
    checked_branch(branch)?;
    let directory = namespace(repo_root)?;
    if checked_carrier(&directory, true, true)?.is_none() {
        return Ok(Vec::new());
    }
    let mut generations = BTreeSet::new();
    for (count, entry) in std::fs::read_dir(&directory)?.enumerate() {
        ensure!(
            count < MAX_ENTRIES,
            "archive journal namespace exceeds its inspection budget"
        );
        let entry = entry?;
        let filename = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("archive namespace has a non-Unicode entry"))?;
        checked_carrier(&entry.path(), false, true)?
            .context("archive namespace entry disappeared")?;
        if let Some(generation) = filename.strip_prefix(".pending-") {
            checked_generation(generation)?;
            continue;
        }
        let generation = [".json", ".recovery", ".control"]
            .iter()
            .find_map(|suffix| filename.strip_suffix(suffix))
            .context("unsupported archive namespace entry")?;
        checked_generation(generation)?;
        generations.insert(generation.to_owned());
    }
    let mut result = Vec::new();
    for generation in generations {
        if let Some(journal) = read(repo_root, &generation)?
            && journal.binding.role.slug == slug
            && journal.binding.role.branch == branch
        {
            result.push(JournalSummary {
                generation,
                branch: branch.to_owned(),
                phase: journal.phase,
                publication_pending: journal.publication.is_some(),
                detach_pending: journal.detach.is_some(),
            });
        }
    }
    Ok(result)
}

/// The control inode is stable for the lifetime of the generation, including tombstones.
pub struct ArchiveJournalGuard {
    directory: PathBuf,
    generation: String,
    width: usize,
    _file: File,
}

impl ArchiveJournalGuard {
    pub fn acquire(repo_root: &Path, generation: &str) -> Result<Self> {
        checked_generation(generation)?;
        let directory = namespace(repo_root)?;
        create_namespace(&directory)?;
        let file = open_file(&directory.join(format!("{generation}.control")), true, true)?;
        fs2::FileExt::lock_exclusive(&file).context("cannot lock archive journal")?;
        Ok(Self {
            directory,
            generation: generation.to_owned(),
            width: object_id_width(repo_root)?,
            _file: file,
        })
    }

    pub fn read(&self) -> Result<Option<ArchiveJournal>> {
        read_pair(&self.directory, &self.generation, self.width)?.settled()
    }

    /// Replay only the retained journal successor while holding its control lock.
    /// Git and Link effects remain the caller's responsibility after inspecting this authority.
    pub fn recover_pending(&self) -> Result<Option<ArchiveJournal>> {
        let pair = read_pair(&self.directory, &self.generation, self.width)?;
        let Some(recovery) = pair.recovery else {
            return Ok(None);
        };
        if pair.main.as_ref() != Some(&recovery.next) {
            durable_publish_bytes(
                &self.directory.join(format!("{}.json", self.generation)),
                &serde_json::to_vec(&recovery.next)?,
                pair.main.is_some(),
            )?;
        }
        self.read()
    }

    pub fn create(&self, journal: &ArchiveJournal) -> Result<()> {
        journal.validate(self.width)?;
        ensure!(
            journal.binding.role.generation == self.generation
                && matches!(journal.phase, ArchivePhase::Preparing),
            "archive creation binding is invalid"
        );
        let path = self.directory.join(format!("{}.json", self.generation));
        let recovery_path = self.directory.join(format!("{}.recovery", self.generation));
        let receipt = Recovery {
            version: VERSION,
            previous: None,
            next: journal.clone(),
        };
        if let Some(bytes) = read_bytes(&recovery_path, MAX_RECOVERY_BYTES)? {
            let existing: Recovery = serde_json::from_slice(&bytes)?;
            ensure!(
                existing == receipt,
                "archive generation already has different creation evidence"
            );
            if let Some(existing) = read_bytes(&path, MAX_JOURNAL_BYTES)? {
                let existing: ArchiveJournal = serde_json::from_slice(&existing)?;
                ensure!(existing == *journal, "archive generation already exists");
                return durable_publish_bytes(&path, &serde_json::to_vec(journal)?, true);
            }
            durable_publish_bytes(&recovery_path, &serde_json::to_vec(&receipt)?, true)?;
        } else {
            ensure!(
                checked_carrier(&path, false, true)?.is_none(),
                "archive generation already exists without recovery evidence"
            );
            durable_publish_bytes(&recovery_path, &serde_json::to_vec(&receipt)?, false)?;
        }
        durable_publish_bytes(&path, &serde_json::to_vec(journal)?, false)
    }

    pub fn replace(&self, expected: &ArchiveJournal, next: &ArchiveJournal) -> Result<()> {
        expected.validate(self.width)?;
        next.validate(self.width)?;
        checked_transition(expected, next)?;
        ensure!(
            expected.binding.role.generation == self.generation,
            "archive replacement selects another generation"
        );
        ensure!(
            self.read()?.as_ref() == Some(expected),
            "archive journal changed before replacement"
        );
        let path = self.directory.join(format!("{}.json", self.generation));
        // Stabilize the observed namespace before overwriting its prior recovery receipt.
        durable_publish_bytes(&path, &serde_json::to_vec(expected)?, true)?;
        if expected == next {
            return Ok(());
        }
        let receipt = Recovery {
            version: VERSION,
            previous: Some(expected.clone()),
            next: next.clone(),
        };
        durable_publish_bytes(
            &self.directory.join(format!("{}.recovery", self.generation)),
            &serde_json::to_vec(&receipt)?,
            true,
        )?;
        durable_publish_bytes(&path, &serde_json::to_vec(next)?, true)
    }
}

fn create_namespace(directory: &Path) -> Result<()> {
    checked_carrier(
        directory
            .parent()
            .context("archive namespace has no parent")?,
        true,
        false,
    )?
    .context("archive namespace parent is missing")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("cannot create archive namespace"),
        }
        checked_carrier(directory, true, true)?.context("archive namespace disappeared")?;
        File::open(
            directory
                .parent()
                .context("archive namespace has no parent")?,
        )?
        .sync_all()?;
    }
    #[cfg(windows)]
    {
        if checked_carrier(directory, true, true)?.is_none() {
            let pending =
                directory.with_file_name(format!(".archive-directory-{}", uuid::Uuid::new_v4()));
            crate::infra::windows_security::private_directory(&pending)?;
            match move_write_through(&pending, directory, false) {
                Ok(()) => {}
                Err(error) => {
                    checked_carrier(directory, true, true)?
                        .context("cannot publish archive namespace")?;
                    return Err(error).context("archive namespace publication is uncertain; the private temporary directory is retained");
                }
            }
        }
        checked_carrier(directory, true, true)?.context("archive namespace disappeared")?;
    }
    Ok(())
}

/// The caller retains exact old/new authority before a replacement can become visible.
/// Failed persistence retains carriers for inspection and never attempts a speculative rollback.
pub(crate) fn durable_publish_bytes(path: &Path, bytes: &[u8], replace: bool) -> Result<()> {
    durable_publish_with_existing(path, bytes, replace, true)
}

/// Existing ordinary authority may be readable while only its owner can change it.
/// The published successor is private, including when its parent directory permits public reads.
pub(crate) fn durable_publish_transition_bytes(
    path: &Path,
    bytes: &[u8],
    replace: bool,
) -> Result<()> {
    durable_publish_with_existing(path, bytes, replace, false)
}

fn durable_publish_with_existing(
    path: &Path,
    bytes: &[u8],
    replace: bool,
    private_existing: bool,
) -> Result<()> {
    let parent = path
        .parent()
        .context("archive publication has no parent directory")?;
    checked_carrier(parent, true, false)?
        .context("archive publication directory does not exist")?;
    let existing = checked_carrier(path, false, private_existing)?;
    ensure!(
        replace || existing.is_none(),
        "archive publication would overwrite existing state"
    );
    let pending = parent.join(format!(".pending-{}", uuid::Uuid::now_v7()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let mut file = options.open(&pending)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(windows)]
    crate::infra::windows_security::write_private_file(&pending, bytes)?;
    checked_carrier(&pending, false, true)?.context("archive temporary publication disappeared")?;
    #[cfg(unix)]
    {
        if replace {
            std::fs::rename(&pending, path)?;
        } else {
            std::fs::hard_link(&pending, path)?;
        }
        File::open(parent)?
            .sync_all()
            .context("archive namespace persistence failed; publication evidence is retained")?;
        if !replace {
            std::fs::remove_file(&pending)?;
            File::open(parent)?.sync_all()?;
        }
    }
    #[cfg(windows)]
    move_write_through(&pending, path, replace)?;
    Ok(())
}

#[cfg(windows)]
fn move_write_through(from: &Path, to: &Path, replace: bool) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let wide = |path: &Path| -> Result<Vec<u16>> {
        let mut path: Vec<_> = path.as_os_str().encode_wide().collect();
        ensure!(!path.contains(&0), "archive path contains a NUL");
        path.push(0);
        Ok(path)
    };
    let from = wide(from)?;
    let to = wide(to)?;
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } == 0 {
        return Err(std::io::Error::last_os_error()).context(
            "archive write-through publication failed; retained state requires reinspection",
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_activation(
    binding: &ExplorationBinding,
    successor_json: String,
) -> PreparedActivation {
    let mut tx = crate::domain::mergetx::Tx {
        mode: Some(binding.mode()),
        exploration: None,
        generation: Some(binding.role.generation.clone()),
        target: binding.target_branch().to_owned(),
        source: binding.source.reference.clone(),
        source_repo: Some(binding.source.slug.clone()),
        source_branch: binding.source.branch.clone(),
        base: binding.source.base.clone().unwrap_or_default(),
        target_head: binding.target_head().to_owned(),
        source_head: binding.source.head.clone(),
        picked: Vec::new(),
        summary: None,
    };
    let transaction_original_json = serde_json::to_string(&tx).unwrap();
    tx.exploration = Some(binding.clone());
    PreparedActivation {
        successor_json,
        transaction_original_json,
        transaction_bound_json: serde_json::to_string(&tx).unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    mod private_control {
        use super::*;
        use crate::infra::windows_security as security;
        use std::io::{Read, Seek, Write};
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW,
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };

        fn facts(file: &File) -> ([u32; 3], Vec<u16>) {
            let mut information = BY_HANDLE_FILE_INFORMATION::default();
            assert_ne!(
                unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) },
                0
            );
            let selected = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
            let mut descriptor = std::ptr::null_mut();
            assert_eq!(
                unsafe {
                    GetSecurityInfo(
                        file.as_raw_handle(),
                        SE_FILE_OBJECT,
                        selected,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut descriptor,
                    )
                },
                0
            );
            let _descriptor = security::LocalAllocation(descriptor);
            let mut text = std::ptr::null_mut();
            let mut length = 0;
            assert_ne!(
                unsafe {
                    ConvertSecurityDescriptorToStringSecurityDescriptorW(
                        descriptor,
                        1,
                        selected,
                        &mut text,
                        &mut length,
                    )
                },
                0
            );
            let _text = security::LocalAllocation(text.cast());
            (
                [
                    information.dwVolumeSerialNumber,
                    information.nFileIndexHigh,
                    information.nFileIndexLow,
                ],
                unsafe { std::slice::from_raw_parts(text, length as usize) }.to_vec(),
            )
        }

        fn control_bytes(mut file: &File) -> Vec<u8> {
            file.rewind().unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            bytes
        }

        #[test]
        fn acquire_and_reacquire_keep_private_control_identity_and_bytes() {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("private");
            security::private_directory(&root).unwrap();
            let repo = Repo::init(&root.join("repo")).unwrap();
            let generation = uuid::Uuid::now_v7().to_string();
            let guard = ArchiveJournalGuard::acquire(repo.root(), &generation).unwrap();
            let path = guard.directory.join(format!("{generation}.control"));
            security::validate_path(&path, false, true).unwrap();
            let before = facts(&guard._file);
            let bytes = b"synthetic retained control bytes";
            let mut writer = &guard._file;
            writer.write_all(bytes).unwrap();
            writer.sync_all().unwrap();
            let contender = open_file(&path, true, true).unwrap();
            assert_eq!(facts(&contender), before);
            assert!(fs2::FileExt::try_lock_exclusive(&contender).is_err());
            assert!(std::fs::remove_file(&path).is_err());
            assert_eq!(control_bytes(&guard._file), bytes);
            drop(contender);
            drop(guard);
            let reopened = ArchiveJournalGuard::acquire(repo.root(), &generation).unwrap();
            assert_eq!(facts(&reopened._file), before);
            assert_eq!(control_bytes(&reopened._file), bytes);
            security::validate_path(&path, false, true).unwrap();
        }

        #[test]
        fn unsafe_existing_control_is_refused_without_replacement_or_acl_repair() {
            use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
            use windows_sys::Win32::Storage::FileSystem::{
                CREATE_NEW, CreateFileW, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
                FILE_SHARE_WRITE,
            };

            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("private");
            security::private_directory(&root).unwrap();
            let repo = Repo::init(&root.join("repo")).unwrap();
            let directory = namespace(repo.root()).unwrap();
            create_namespace(&directory).unwrap();
            let generation = uuid::Uuid::now_v7().to_string();
            let path = directory.join(format!("{generation}.control"));
            let sid = security::current_sid().unwrap();
            let sddl = security::wide(format!("O:{sid}D:P(A;;GA;;;{sid})(A;;GR;;;WD)")).unwrap();
            let mut raw_descriptor = std::ptr::null_mut();
            assert_ne!(
                unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        sddl.as_ptr(),
                        1,
                        &mut raw_descriptor,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            let descriptor = security::LocalAllocation(raw_descriptor);
            let attributes = security::attributes(&descriptor);
            let name = security::wide(&path).unwrap();
            let handle = security::Handle::new(unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    &attributes,
                    CREATE_NEW,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    std::ptr::null_mut(),
                )
            })
            .unwrap();
            let raw = handle.0;
            std::mem::forget(handle);
            let mut file = unsafe { File::from_raw_handle(raw) };
            let bytes = b"synthetic unselected control bytes";
            file.write_all(bytes).unwrap();
            file.sync_all().unwrap();
            let before = facts(&file);
            assert!(security::validate_path(&path, false, true).is_err());
            assert!(security::open_private_control(&path).is_err());
            let error = ArchiveJournalGuard::acquire(repo.root(), &generation)
                .err()
                .unwrap();
            assert!(
                error
                    .to_string()
                    .contains("grants access to another Windows user")
            );
            assert_eq!(facts(&file), before);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert!(security::validate_path(&path, false, true).is_err());
            assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        }
    }

    #[test]
    fn transaction_retirement_replays_each_namespace_boundary_without_clobbering() {
        let bytes = b"{\"transaction\":\"retained\"}\n";
        for boundary in [
            RetirementPoint::CarrierVisible,
            RetirementPoint::CarrierDurable,
            RetirementPoint::Removed,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let original = directory.path().join("tx.json");
            let retired = directory.path().join("complete.json");
            durable_publish_transition_bytes(&original, bytes, false).unwrap();
            assert!(
                retire_transition_with(&original, &retired, bytes, &mut |point| {
                    if point == boundary {
                        anyhow::bail!("injected retirement stop")
                    }
                    Ok(())
                })
                .is_err()
            );
            assert_eq!(std::fs::read(&retired).unwrap(), bytes);
            durable_retire_transition_bytes(&original, &retired, bytes).unwrap();
            assert!(!original.exists());
            assert_eq!(std::fs::read(&retired).unwrap(), bytes);
            durable_retire_transition_bytes(&original, &retired, bytes).unwrap();
        }
    }

    #[test]
    fn transaction_retirement_rejects_conflicting_carriers_and_replacement_authority() {
        let bytes = b"{\"transaction\":\"retained\"}\n";
        for conflicting in [
            bytes.as_slice(),
            b"{\"transaction\":\"another\"}\n".as_slice(),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let original = directory.path().join("tx.json");
            let retired = directory.path().join("complete.json");
            durable_publish_transition_bytes(&original, bytes, false).unwrap();
            durable_publish_transition_bytes(&retired, conflicting, false).unwrap();
            assert!(durable_retire_transition_bytes(&original, &retired, bytes).is_err());
            assert_eq!(std::fs::read(&original).unwrap(), bytes);
            assert_eq!(std::fs::read(&retired).unwrap(), conflicting);
        }
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("tx.json");
        let retired = directory.path().join("complete.json");
        assert!(durable_retire_transition_bytes(&original, &retired, bytes).is_err());
        durable_publish_transition_bytes(&original, b"{\"other\":true}\n", false).unwrap();
        assert!(durable_retire_transition_bytes(&original, &retired, bytes).is_err());
        assert_eq!(std::fs::read(&original).unwrap(), b"{\"other\":true}\n");
        assert!(!retired.exists());
    }

    #[cfg(unix)]
    #[test]
    fn transaction_retirement_rejects_redirected_or_nonprivate_completion_evidence() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let bytes = b"{\"transaction\":\"retained\"}\n";
        for redirected in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let original = directory.path().join("tx.json");
            let retired = directory.path().join("complete.json");
            let unrelated = directory.path().join("unrelated.json");
            durable_publish_transition_bytes(&original, bytes, false).unwrap();
            if redirected {
                durable_publish_transition_bytes(&unrelated, bytes, false).unwrap();
                symlink(&unrelated, &retired).unwrap();
            } else {
                durable_retire_transition_bytes(&original, &retired, bytes).unwrap();
                std::fs::set_permissions(&retired, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            assert!(durable_retire_transition_bytes(&original, &retired, bytes).is_err());
            assert_eq!(std::fs::read(&retired).unwrap(), bytes);
            assert_eq!(original.exists(), redirected);
        }
    }

    /// A retained successor remains authoritative until its exact journal publication completes.
    #[test]
    fn partial_journal_publication_requires_exact_replay() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        let preparing = preparing();
        let generation = &preparing.binding.role.generation;
        let guard = ArchiveJournalGuard::acquire(repo.root(), generation).unwrap();
        let primary = guard.directory.join(format!("{generation}.json"));
        let recovery_path = guard.directory.join(format!("{generation}.recovery"));
        let creation = Recovery {
            version: VERSION,
            previous: None,
            next: preparing.clone(),
        };
        durable_publish_bytes(
            &recovery_path,
            &serde_json::to_vec(&creation).unwrap(),
            false,
        )
        .unwrap();
        assert!(guard.read().is_err());
        assert_eq!(guard.recover_pending().unwrap(), Some(preparing.clone()));

        let mut open = preparing.clone();
        open.phase = ArchivePhase::Open;
        open.activation = None;
        let recovery = Recovery {
            version: VERSION,
            previous: Some(preparing.clone()),
            next: open.clone(),
        };
        durable_publish_bytes(
            &recovery_path,
            &serde_json::to_vec(&recovery).unwrap(),
            true,
        )
        .unwrap();
        let before = (
            std::fs::read(&primary).unwrap(),
            std::fs::read(&recovery_path).unwrap(),
        );
        drop(guard);
        assert!(read(repo.root(), generation).is_err());
        assert!(probe_for_branch(repo.root(), "alice/photo", "work").is_err());
        let guard = ArchiveJournalGuard::acquire(repo.root(), generation).unwrap();
        let mut different = preparing.clone();
        different.phase = ArchivePhase::Aborting;
        assert!(guard.replace(&preparing, &different).is_err());
        assert_eq!(std::fs::read(&primary).unwrap(), before.0);
        assert_eq!(std::fs::read(&recovery_path).unwrap(), before.1);
        assert_eq!(guard.recover_pending().unwrap(), Some(open.clone()));
        assert_eq!(std::fs::read(&recovery_path).unwrap(), before.1);
        assert_eq!(guard.read().unwrap(), Some(open.clone()));
        let mut aborted = open.clone();
        aborted.phase = ArchivePhase::Aborting;
        assert_refused_without_write(&guard, &open, &aborted);
    }

    fn preparing() -> ArchiveJournal {
        let installed = Frontier {
            bytes: 0,
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        };
        let mut journal = ArchiveJournal {
            version: VERSION,
            binding: ExplorationBinding {
                file_target: None,
                role: MergeArchiveRole {
                    generation: uuid::Uuid::now_v7().to_string(),
                    slug: "alice/photo".into(),
                    branch: "work".into(),
                    origin_head: "a".repeat(40),
                    logical_session: format!("agit-{}", "f".repeat(40)),
                },
                native: RuntimeLinkKey {
                    runtime: "codex".into(),
                    session_id: "ARCHIVE".into(),
                },
                installed: installed.clone(),
                source: FrozenMergeSource {
                    reference: "alice/photo@source".into(),
                    slug: "alice/photo".into(),
                    branch: Some("source".into()),
                    head: "b".repeat(40),
                    base: None,
                },
            },
            phase: ArchivePhase::Preparing,
            consumed: installed,
            opencode: None,
            accepted_commit: None,
            publication: None,
            landing: None,
            abort: None,
            previous_claims: Vec::new(),
            activation: None,
            detach: None,
        };
        journal.activation = Some(test_activation(&journal.binding, "{}".into()));
        journal
    }

    fn file_preparing() -> ArchiveJournal {
        let mut journal = preparing();
        journal.binding.role.branch =
            format!("merge-exploration/{}", journal.binding.role.generation);
        journal.binding.file_target = Some(FrozenFileTarget {
            branch: "main".into(),
            head: "3".repeat(40),
        });
        journal.activation = Some(test_activation(&journal.binding, "{}".into()));
        journal
    }

    #[test]
    fn file_target_authority_is_separate_from_the_native_evidence_role() {
        let journal = file_preparing();
        journal.validate(40).unwrap();
        let activation = journal.activation.as_ref().unwrap();
        for (json, unbound) in [
            (&activation.transaction_original_json, true),
            (&activation.transaction_bound_json, false),
        ] {
            let tx = checked_abort_transaction(json, &journal.binding, unbound).unwrap();
            assert_eq!(tx.mode, Some(crate::domain::mergetx::Mode::FileAgent));
            assert_eq!(tx.target, "main");
            assert_ne!(tx.target, journal.binding.role.branch);
            assert_ne!(tx.target_head, journal.binding.role.origin_head);
            for (key, value) in [
                ("mode", serde_json::json!("session_agent")),
                ("target", serde_json::json!(journal.binding.role.branch)),
                (
                    "target_head",
                    serde_json::json!(journal.binding.role.origin_head),
                ),
                (
                    "generation",
                    serde_json::json!(uuid::Uuid::now_v7().to_string()),
                ),
                ("source_head", serde_json::json!("4".repeat(40))),
            ] {
                let mut changed: serde_json::Value = serde_json::from_str(json).unwrap();
                changed[key] = value;
                assert!(
                    checked_abort_transaction(&changed.to_string(), &journal.binding, unbound)
                        .is_err()
                );
            }
        }
        let ordinary = preparing();
        let serialized = serde_json::to_string(&ordinary).unwrap();
        assert!(!serialized.contains("file_target"));
        assert_eq!(
            serde_json::from_str::<ArchiveJournal>(&serialized).unwrap(),
            ordinary
        );
        for branch in ["main", "work", "merge-exploration/wrong-generation"] {
            let mut changed = journal.clone();
            changed.binding.role.branch = branch.into();
            assert!(changed.validate(40).is_err());
        }
        let mut changed = journal.clone();
        changed.binding.file_target.as_mut().unwrap().head =
            journal.binding.role.origin_head.clone();
        assert!(changed.validate(40).is_err());
    }

    #[test]
    fn file_landing_retains_both_candidates_before_consuming_native_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        let preparing = file_preparing();
        let guard =
            ArchiveJournalGuard::acquire(repo.root(), &preparing.binding.role.generation).unwrap();
        guard.create(&preparing).unwrap();
        let mut open = preparing.clone();
        open.phase = ArchivePhase::Open;
        open.activation = None;
        guard.replace(&preparing, &open).unwrap();
        let mut tx = crate::domain::mergetx::checked_activation_image(
            &preparing
                .activation
                .as_ref()
                .unwrap()
                .transaction_bound_json,
        )
        .unwrap();
        tx.set_summary("Keep shared instructions".into());
        let file_commit = "c".repeat(40);
        let evidence_commit = "d".repeat(40);
        let publication = PreparedArchivePublication {
            kind: ArchivePublicationKind::MergeLanding {
                source_head: open.binding.source.head.clone(),
            },
            link_json: "{}".into(),
            expected_old: open.binding.role.origin_head.clone(),
            candidate: evidence_commit.clone(),
            candidate_tree: "e".repeat(40),
            prior_frontier: open.consumed.clone(),
            next_frontier: Frontier {
                bytes: 8,
                sha256: "1".repeat(64),
            },
            next_opencode: None,
            appended_records: 1,
            protected_suffix_sha256: "2".repeat(64),
        };
        let mut pending = open.clone();
        pending.publication = Some(publication.clone());
        pending.landing = Some(RetainedMergeLanding {
            transaction_json: serde_json::to_string(&tx).unwrap(),
            ordinary_tree: "f".repeat(40),
            worktree_tree: None,
            file_commit: Some(file_commit.clone()),
            file_evidence: Some(evidence_commit.clone()),
        });
        guard.replace(&open, &pending).unwrap();
        for missing in [true, false] {
            let mut invalid = pending.clone();
            invalid.landing.as_mut().unwrap().file_commit = if missing {
                None
            } else {
                Some(evidence_commit.clone())
            };
            assert_refused_without_write(&guard, &pending, &invalid);
        }
        for evidence in [None, Some(file_commit.clone()), Some("8".repeat(40))] {
            let mut invalid = pending.clone();
            invalid.landing.as_mut().unwrap().file_evidence = evidence;
            assert_refused_without_write(&guard, &pending, &invalid);
        }
        let mut landed = pending.clone();
        landed.phase = ArchivePhase::Landed {
            merge_commit: file_commit.clone(),
        };
        landed.publication = None;
        landed.accepted_commit = Some(evidence_commit.clone());
        landed.consumed = publication.next_frontier.clone();
        let mut wrong = landed.clone();
        wrong.phase = ArchivePhase::Landed {
            merge_commit: evidence_commit.clone(),
        };
        assert_refused_without_write(&guard, &pending, &wrong);
        wrong = landed.clone();
        wrong.accepted_commit = Some(file_commit.clone());
        assert_refused_without_write(&guard, &pending, &wrong);
        guard.replace(&pending, &landed).unwrap();
        require_landed_transaction(
            &common_git_dir(repo.root()),
            &landed.binding,
            &landed.landing.as_ref().unwrap().transaction_json,
            &file_commit,
        )
        .unwrap();
        assert!(
            require_landed_transaction(
                &common_git_dir(repo.root()),
                &landed.binding,
                &landed.landing.as_ref().unwrap().transaction_json,
                &evidence_commit
            )
            .is_err()
        );
        let mut tail = landed.clone();
        let mut next_publication = publication;
        next_publication.kind = ArchivePublicationKind::Tail;
        next_publication.expected_old = evidence_commit;
        next_publication.candidate = "9".repeat(40);
        next_publication.prior_frontier = landed.consumed.clone();
        next_publication.next_frontier = Frontier {
            bytes: 12,
            sha256: "3".repeat(64),
        };
        tail.publication = Some(next_publication.clone());
        guard.replace(&landed, &tail).unwrap();
        let mut complete = tail.clone();
        complete.publication = None;
        complete.accepted_commit = Some(next_publication.candidate);
        complete.consumed = next_publication.next_frontier;
        guard.replace(&tail, &complete).unwrap();
        assert_eq!(
            complete.phase,
            ArchivePhase::Landed {
                merge_commit: file_commit
            }
        );
    }

    #[test]
    fn preparing_file_cancellation_retains_the_main_transaction_and_evidence_role() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        let preparing = file_preparing();
        let guard =
            ArchiveJournalGuard::acquire(repo.root(), &preparing.binding.role.generation).unwrap();
        guard.create(&preparing).unwrap();
        let transaction_json = preparing
            .activation
            .as_ref()
            .unwrap()
            .transaction_original_json
            .clone();
        let mut cancelling = preparing.clone();
        cancelling.phase = ArchivePhase::Aborting;
        cancelling.abort = Some(RetainedAbort {
            expected_head: preparing.binding.role.origin_head.clone(),
            transaction_json: transaction_json.clone(),
            successor_json: None,
            activation: preparing.activation.clone(),
            cancelled_publication: None,
        });
        guard.replace(&preparing, &cancelling).unwrap();
        let mut completed = cancelling.clone();
        completed.phase = ArchivePhase::Aborted;
        completed.activation = None;
        guard.replace(&cancelling, &completed).unwrap();
        require_aborted_transaction(
            &common_git_dir(repo.root()),
            &completed.binding,
            &transaction_json,
        )
        .unwrap();
        let mut changed = completed.clone();
        changed.binding.file_target.as_mut().unwrap().head = "5".repeat(40);
        assert_refused_without_write(&guard, &completed, &changed);
    }

    fn assert_refused_without_write(
        guard: &ArchiveJournalGuard,
        expected: &ArchiveJournal,
        next: &ArchiveJournal,
    ) {
        let primary = guard.directory.join(format!("{}.json", guard.generation));
        let recovery = guard
            .directory
            .join(format!("{}.recovery", guard.generation));
        let before = (
            std::fs::read(&primary).unwrap(),
            std::fs::read(&recovery).unwrap(),
        );
        assert!(guard.replace(expected, next).is_err());
        assert_eq!(guard.read().unwrap().as_ref(), Some(expected));
        assert_eq!(std::fs::read(primary).unwrap(), before.0);
        assert_eq!(std::fs::read(recovery).unwrap(), before.1);
    }

    /// The journal uses the same generation format as the transaction that admits the runtime.
    #[test]
    fn archive_generation_accepts_transaction_identity() {
        let journal = preparing();
        journal.validate(40).unwrap();
        journal.binding.role.validate(40).unwrap();
        assert_eq!(
            uuid::Uuid::parse_str(&journal.binding.role.generation)
                .unwrap()
                .get_version(),
            Some(uuid::Version::SortRand)
        );
        for generation in [uuid::Uuid::new_v4().to_string(), "not-a-generation".into()] {
            let mut other = journal.clone();
            other.binding.role.generation = generation;
            assert!(other.validate(40).is_err());
        }
        let mut other = journal.clone();
        other.binding.role.generation = uuid::Uuid::now_v7().to_string();
        assert_ne!(
            other.binding.role.generation,
            journal.binding.role.generation
        );
        assert!(checked_transition(&journal, &other).is_err());
    }

    /// Exact restoration images retain extensions through durable journal replacement.
    #[test]
    fn journal_preserves_unrelated_link_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        let mut journal = preparing();
        let original = "{\"owner\":\"alice\",\"agent\":\"photo\",\"branch\":\"work\",\"extension\":{\"retained\":true}}\n";
        let retired = "{\"owner\":\"alice\",\"agent\":\"photo\",\"branch\":\"work\",\"superseded_by\":\"codex/ARCHIVE\",\"extension\":{\"retained\":true}}\n";
        journal.previous_claims.push(PreviousClaim {
            native: RuntimeLinkKey {
                runtime: "codex".into(),
                session_id: "ORDINARY".into(),
            },
            original_json: original.into(),
            retired_json: retired.into(),
        });
        journal.activation.as_mut().unwrap().successor_json =
            "{\"extension\":{\"nested\":[true,null,\"kept\"]}}\n".into();
        let guard =
            ArchiveJournalGuard::acquire(repo.root(), &journal.binding.role.generation).unwrap();
        guard.create(&journal).unwrap();
        assert_eq!(guard.read().unwrap().unwrap(), journal);
        let mut open = journal.clone();
        open.phase = ArchivePhase::Open;
        open.activation = None;
        guard.replace(&journal, &open).unwrap();
        let restored = guard.read().unwrap().unwrap();
        assert_eq!(restored.previous_claims[0].original_json, original);
        assert_eq!(restored.previous_claims[0].retired_json, retired);
        assert!(checked_link_image("{\"branch\":false,\"extension\":true}").is_err());
        assert!(checked_link_image("{\"branch\":\"work\",\"branch\":\"other\"}").is_err());
        assert!(checked_link_image("[]").is_err());
    }

    /// Prepared publication and detachment receipts survive until their exact completion transition.
    #[test]
    fn pending_intent_cannot_disappear_without_completion() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        let journal = preparing();
        let guard =
            ArchiveJournalGuard::acquire(repo.root(), &journal.binding.role.generation).unwrap();
        guard.create(&journal).unwrap();
        let mut current = journal.clone();
        current.phase = ArchivePhase::Open;
        current.activation = None;
        guard.replace(&journal, &current).unwrap();
        for (index, kind) in [
            ArchivePublicationKind::MergeLanding {
                source_head: journal.binding.source.head.clone(),
            },
            ArchivePublicationKind::Tail,
        ]
        .into_iter()
        .enumerate()
        {
            let candidate = if index == 0 { "c" } else { "d" }.repeat(40);
            let publication = PreparedArchivePublication {
                kind,
                link_json: "{}".into(),
                expected_old: current
                    .accepted_commit
                    .clone()
                    .unwrap_or_else(|| current.binding.role.origin_head.clone()),
                candidate: candidate.clone(),
                candidate_tree: "e".repeat(40),
                prior_frontier: current.consumed.clone(),
                next_opencode: None,
                next_frontier: Frontier {
                    bytes: current.consumed.bytes + 4,
                    sha256: "1".repeat(64),
                },
                appended_records: 1,
                protected_suffix_sha256: "2".repeat(64),
            };
            let mut pending = current.clone();
            pending.publication = Some(publication.clone());
            guard.replace(&current, &pending).unwrap();
            let mut dropped = pending.clone();
            dropped.publication = None;
            assert_refused_without_write(&guard, &pending, &dropped);
            let mut completed = pending.clone();
            completed.publication = None;
            completed.accepted_commit = Some(candidate.clone());
            completed.consumed = publication.next_frontier;
            if index == 0 {
                completed.phase = ArchivePhase::Landed {
                    merge_commit: candidate,
                };
            }
            guard.replace(&pending, &completed).unwrap();
            current = completed;
        }
        let mut pending = current.clone();
        pending.detach = Some(PreparedDetach {
            admission_id: uuid::Uuid::now_v7().to_string(),
            destination_slug: "alice/photo".into(),
            destination_branch: "detached".into(),
            expected_destination: None,
            candidate_destination: "e".repeat(40),
            candidate_tree: "f".repeat(40),
            old_link_json: "{}".into(),
            new_link_json: "{\"branch\":\"detached\"}".into(),
        });
        guard.replace(&current, &pending).unwrap();
        let mut dropped = pending.clone();
        dropped.detach = None;
        assert_refused_without_write(&guard, &pending, &dropped);
        dropped.phase = ArchivePhase::Detached;
        guard.replace(&pending, &dropped).unwrap();
        assert_eq!(guard.read().unwrap().unwrap(), dropped);
    }
}
