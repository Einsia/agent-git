//! Public Git history is built in an object store with no source-object alternates.
//!
//! Private cache keys bind original snapshots, projected bytes, policy, recipients and parents.
//! Encrypted snapshots publish generated trees; registered plain and accepted objects keep their bytes.

use super::{
    meta::{self, Meta},
    privacy::PrivacyPolicy,
    privacy_envelope::{PrivacyEnvelope, digest_json},
    privacy_paths::PathAliasStore,
    privacy_publication::{ProjectionReport, PublicationSnapshot},
    redact::Redactor,
    repo::{
        Repo,
        publication::{PublicationPlan, commit_parents},
    },
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Seek, Write},
    path::Path,
};

const MAX_COMMIT: usize = 1024 * 1024;
const COMMIT_SUFFIX: &str = "author AgentGit Privacy <privacy@agentgit.local> 0 +0000\ncommitter AgentGit Privacy <privacy@agentgit.local> 0 +0000\n\nPrivacy-projected snapshot\n";

mod accepted;
#[cfg(test)]
mod accepted_tests;
mod published;
#[cfg(test)]
mod receipt_tests;
pub mod sessions;
mod state;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    version: u32,
    sessions: BTreeMap<String, String>,
    commits: BTreeMap<String, String>,
}

pub struct ProjectedHistory {
    repo: Repo,
    source: PublicationPlan,
    branches: Vec<String>,
    plan: PublicationPlan,
    policy_digest: String,
    additional_policies: Vec<super::privacy::mandatory::MandatoryPolicy>,
    reports: Vec<ProjectionReport>,
    accepted_ledger: accepted::Ledger,
    candidates: BTreeMap<String, accepted::Candidate>,
    session_manifest: super::session_privacy::SessionPublicationManifest,
    #[cfg(test)]
    preparations: usize,
    _lock: fs::File,
}

impl ProjectedHistory {
    pub fn repo(&self) -> &Repo {
        &self.repo
    }
    pub fn plan(&self) -> &PublicationPlan {
        &self.plan
    }
    pub fn reports(&self) -> &[ProjectionReport] {
        &self.reports
    }

    pub fn policy_digest(&self) -> &str {
        &self.policy_digest
    }

    pub fn session_manifest(&self) -> &super::session_privacy::SessionPublicationManifest {
        &self.session_manifest
    }

    pub(crate) fn prepare_acceptance(&mut self) -> Result<()> {
        self.accepted_ledger.prepare(&self.candidates)
    }

    pub(crate) fn confirm_acceptance(
        &mut self,
        report: &crate::hub::git::PublicationReport,
    ) -> Result<()> {
        self.accepted_ledger.confirm(&self.candidates, report)
    }

    /// Digest the public projections in publication order without incorporating private layers.
    pub fn public_digest(&self) -> Result<String> {
        let manifest = &self.session_manifest;
        digest_json(&json!({"manifest":manifest,"snapshots":self.session_preview()?}))
    }

    /// Save the actual public contents reviewed by the CLI; the private envelope stays ciphertext.
    pub fn write_preview(
        &self,
        destination: &serde_json::Value,
        source: &Repo,
        captured: &crate::hub::git::CapturedPublication,
    ) -> Result<std::path::PathBuf> {
        self.verify_source(source)?;
        let policy = effective_policy(source, &self.additional_policies)?;
        let snapshots = self.session_preview()?;
        let files = self.plain_file_preview(captured)?;
        let path = self.repo.root().join(".git/publication-preview.json");
        let public_digest = self.public_digest()?;
        fs::write(
            &path,
            serde_json::to_vec_pretty(
                &json!({"destination":destination,"policy":policy,"plan":self.plan,"public_digest":public_digest,"snapshots":snapshots,"files":files,"session_manifest":self.session_manifest}),
            )?,
        )?;
        Ok(path)
    }

    pub fn verify_source(&self, source: &Repo) -> Result<()> {
        self.source.verify(source, &self.branches)?;
        ensure!(
            effective_policy(source, &self.additional_policies)?.digest()? == self.policy_digest,
            "privacy policy changed after preview"
        );
        self.plan.verify(&self.repo, &self.branches)
    }
}

/// A retained receipt selects an immutable generated snapshot, independently of current refs.
pub(crate) fn receipt_session_id(
    source: &Repo,
    receipt: &super::privacy_receipt::PublicationReceipt,
) -> Result<String> {
    let (repo, _lock) = receipt_cache(source, receipt)?;
    let commit = read(
        &repo,
        &["cat-file", "commit", &receipt.published],
        MAX_COMMIT,
    )?;
    let parents = commit_parents(&commit)?;
    let snapshot = published::reuse(&repo, &repo, &receipt.published, &parents)?
        .context("retained publication is not a generated snapshot")?;
    ensure!(
        Some(snapshot.policy_digest.as_str()) == receipt.policy_digest.as_deref()
            && meta::is_bare_id(&snapshot.session_id),
        "retained publication policy or session identity is invalid"
    );
    Ok(snapshot.session_id)
}

pub(crate) fn receipt_ancestors(
    source: &Repo,
    receipt: &super::privacy_receipt::PublicationReceipt,
) -> Result<BTreeSet<String>> {
    let (repo, _lock) = receipt_cache(source, receipt)?;
    super::repo::publication::raw_ancestors(&repo, &receipt.published)
}

/// Only authenticated, accepted mappings may translate private points for Hub bookkeeping.
/// Prepared candidates and ambiguous cross-branch projections cannot authorize disclosure.
pub(crate) fn accepted_session_identity(
    source: &Repo,
    branch: Option<&str>,
    commit: &str,
    identity: &crate::hub::identity::RemoteIdentity,
) -> Result<Option<(String, String)>> {
    let ledger = accepted::Ledger::open(source, identity)?;
    Ok(ledger
        .selected(branch, commit)?
        .map(|mapping| (mapping.public_session.clone(), mapping.public.clone())))
}

pub(crate) fn publication_envelope(
    source: &Repo,
    branch: Option<&str>,
    commit: &str,
    identity: &crate::hub::identity::RemoteIdentity,
) -> Result<(String, Option<Vec<u8>>)> {
    super::session_privacy::validate_oid(commit)?;
    let parent = source.common_dir()?.join("agit/privacy-publication");
    let lock = crate::infra::config::state_file_options()
        .read(true)
        .open(parent.join("lock"));
    let _lock = match lock {
        Ok(lock) => {
            fs2::FileExt::try_lock_shared(&lock).context("publication state is busy; retry")?;
            Some(lock)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let ledger = accepted::Ledger::open(source, identity)?;
    let mapping = ledger.selected(branch, commit)?;
    let (repo, public) = if let Some(mapping) = mapping {
        (
            Repo::at(parent.join(&mapping.store)).local_objects_only(),
            mapping.public.as_str(),
        )
    } else {
        (source.clone().local_objects_only(), commit)
    };
    let entry = repo.git_bytes_result(&[
        "ls-tree",
        "-z",
        "--full-name",
        public,
        "--",
        "privacy/envelope.json",
    ])?;
    if entry.is_empty() {
        return Ok((public.to_owned(), None));
    }
    ensure!(
        entry.starts_with(b"100644 blob "),
        "privacy envelope must be a regular file"
    );
    let raw = read(&repo, &["cat-file", "blob", &format!("{public}:privacy/envelope.json")], super::privacy_envelope::MAX_ENVELOPE_BYTES)
        .context("this snapshot has no verified publication envelope; publish or fetch its accepted snapshot first")?;
    Ok((public.to_owned(), Some(raw)))
}

fn receipt_cache(
    source: &Repo,
    receipt: &super::privacy_receipt::PublicationReceipt,
) -> Result<(Repo, fs::File)> {
    receipt.validate()?;
    ensure!(
        receipt.mode.is_encrypted(),
        "ordinary publication does not use generated history"
    );
    let parent = source.common_dir()?.join("agit/privacy-publication");
    let lock = crate::infra::config::state_file_options()
        .read(true)
        .open(parent.join("lock"))?;
    fs2::FileExt::try_lock_shared(&lock)
        .context("privacy publication cache is being updated; retry")?;
    let ledger = accepted::Ledger::open(source, &receipt.destination)?;
    let store = ledger
        .receipt_store(receipt)?
        .context("retained session publication has no authenticated mapping")?;
    let root = parent.join(store);
    ensure!(
        root.join(".git/config").is_file() && !root.join(".git/objects/info/alternates").exists(),
        "retained privacy publication storage is unavailable or not isolated"
    );
    let repo = Repo::at(root).local_objects_only();
    if let Some(snapshot) = &receipt.session_snapshot {
        let bytes = read(
            &repo,
            &[
                "cat-file",
                "blob",
                &format!("{}:privacy/envelope.json", receipt.published),
            ],
            super::privacy_envelope::MAX_ENVELOPE_BYTES,
        )?;
        let envelope = PrivacyEnvelope::parse(&bytes)?;
        ensure!(
            super::privacy_envelope::digest_bytes(&bytes) == snapshot.envelope_digest
                && envelope.session_binding.as_ref() == Some(&snapshot.binding)
                && Some(&envelope.policy_digest) == receipt.policy_digest.as_ref(),
            "retained session envelope differs from its accepted receipt"
        );
    }
    Ok((repo, lock))
}

fn effective_policy(
    source: &Repo,
    additional: &[super::privacy::mandatory::MandatoryPolicy],
) -> Result<PrivacyPolicy> {
    let mut policy = PrivacyPolicy::load(source).unwrap_or_default();
    policy.mandatory.extend_from_slice(additional);
    policy.validate()?;
    Ok(policy)
}

fn commit_body(tree: &str, parents: &[String]) -> String {
    let mut body = format!("tree {tree}\n");
    for parent in parents {
        body.push_str(&format!("parent {parent}\n"));
    }
    body.push_str(COMMIT_SUFFIX);
    body
}

fn initialize_with_format(root: &Path, oid_length: usize) -> Result<()> {
    ensure!(
        matches!(oid_length, 40 | 64),
        "unsupported publication object format"
    );
    if root.exists() {
        ensure!(
            root.join(".git/config").is_file()
                && !root.join(".git/objects/info/alternates").exists(),
            "privacy publication storage is not isolated"
        );
        let expected = if oid_length == 64 { "sha256" } else { "sha1" };
        ensure!(
            Repo::at(root)
                .local_objects_only()
                .git(&["rev-parse", "--show-object-format"])?
                == expected,
            "privacy publication storage uses another object format"
        );
        return Ok(());
    }
    let parent = root
        .parent()
        .context("privacy publication storage has no parent")?;
    let staging = tempfile::tempdir_in(parent)?;
    for directory in [
        "objects/info",
        "objects/pack",
        "refs/heads",
        "refs/tags",
        "hooks",
    ] {
        fs::create_dir_all(staging.path().join(".git").join(directory))?;
    }
    let format = if oid_length == 64 { "1" } else { "0" };
    let mut config = format!(
        "[core]\nrepositoryformatversion = {format}\nbare = false\nlogallrefupdates = false\nhooksPath =\n"
    );
    if oid_length == 64 {
        config.push_str("[extensions]\nobjectFormat = sha256\n");
    }
    fs::write(staging.path().join(".git/config"), config)?;
    fs::write(staging.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    fs::rename(staging.path(), root)?;
    Ok(())
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn read(repo: &Repo, args: &[&str], limit: usize) -> Result<Vec<u8>> {
    let output = repo.inspection_output(args, limit)?;
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "privacy source object is unavailable"
    );
    Ok(output.stdout)
}

fn input(repo: &Repo, args: &[&str], bytes: &[u8]) -> Result<String> {
    let mut file = tempfile::tempfile()?;
    file.write_all(bytes)?;
    file.rewind()?;
    repo.git_with_stdin_file(args, file)
}

fn write_object(repo: &Repo, kind: &str, bytes: &[u8]) -> Result<String> {
    let oid = input(repo, &["hash-object", "-t", kind, "-w", "--stdin"], bytes)?;
    ensure!(
        valid_oid(&oid),
        "Git returned an invalid projected object ID"
    );
    Ok(oid)
}

#[derive(Default)]
struct Tree {
    files: BTreeMap<String, Vec<u8>>,
    directories: BTreeMap<String, Tree>,
}

fn write_tree(repo: &Repo, files: BTreeMap<String, Vec<u8>>) -> Result<String> {
    let mut tree = Tree::default();
    for (path, bytes) in files {
        let mut parts = path.split('/').peekable();
        let mut node = &mut tree;
        while let Some(part) = parts.next() {
            ensure!(
                !matches!(part, "" | "." | ".." | ".git") && !part.chars().any(char::is_control),
                "invalid projected tree path"
            );
            if parts.peek().is_some() {
                node = node.directories.entry(part.into()).or_default();
            } else {
                node.files.insert(part.into(), bytes);
                break;
            }
        }
    }
    fn emit(repo: &Repo, tree: Tree) -> Result<String> {
        let mut records = Vec::new();
        for (name, bytes) in tree.files {
            records.extend(
                format!(
                    "100644 blob {}\t{name}\0",
                    write_object(repo, "blob", &bytes)?
                )
                .as_bytes(),
            );
        }
        for (name, subtree) in tree.directories {
            records.extend(format!("040000 tree {}\t{name}\0", emit(repo, subtree)?).as_bytes());
        }
        input(repo, &["mktree", "-z"], &records)
    }
    emit(repo, tree)
}

fn ancestors(head: &str, parents: &BTreeMap<String, Vec<String>>) -> Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    let mut pending = vec![head.to_owned()];
    while let Some(oid) = pending.pop() {
        if found.insert(oid.clone()) {
            pending.extend(
                parents
                    .get(&oid)
                    .context("source parent is outside the frozen plan")?
                    .iter()
                    .cloned(),
            );
        }
    }
    Ok(found)
}

fn parent_first(parents: &BTreeMap<String, Vec<String>>) -> Result<Vec<String>> {
    let mut pending = parents.keys().cloned().collect::<BTreeSet<_>>();
    let mut done = BTreeSet::new();
    let mut order = Vec::new();
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .filter(|oid| parents[*oid].iter().all(|parent| done.contains(parent)))
            .cloned()
            .collect::<Vec<_>>();
        ensure!(
            !ready.is_empty(),
            "source history has an unavailable or cyclic parent"
        );
        for oid in ready {
            pending.remove(&oid);
            done.insert(oid.clone());
            order.push(oid);
        }
    }
    Ok(order)
}
