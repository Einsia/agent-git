//! Public Git history is built in an object store with no source-object alternates.
//!
//! Private cache keys bind original snapshots, projected bytes, policy, recipients and parents.
//! Initial plain selection normalizes source privacy metadata; accepted objects keep their bytes.

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
#[cfg(test)]
const LEGACY_COMMIT_SUFFIX: &str = "author AgentGit Privacy <privacy@agentgit.local> 0 +0000\ncommitter AgentGit Privacy <privacy@agentgit.local> 0 +0000\n\nPrivacy-projected snapshot\n";

mod accepted;
#[cfg(test)]
mod accepted_tests;
#[cfg(test)]
mod commit_tests;
pub(crate) mod modes;
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
        self.public_digest_of(&self.session_preview()?)
    }

    fn public_digest_of(&self, snapshots: &[serde_json::Value]) -> Result<String> {
        let manifest = &self.session_manifest;
        digest_json(&json!({"manifest":manifest,"snapshots":snapshots}))
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
        let path = self.repo.root().join(".git/publication-preview.json");
        let public_digest = self.public_digest_of(&snapshots)?;
        let summary = serde_json::to_string_pretty(
            &json!({"destination":destination,"policy":policy,"plan":self.plan,"public_digest":public_digest,"snapshots":snapshots,"session_manifest":self.session_manifest}),
        )?;
        // File entries stream after the summary, so the document never has to fit in memory.
        let body = summary
            .strip_suffix("\n}")
            .context("preview summary is not an object")?;
        let mut output = std::io::BufWriter::new(fs::File::create(&path)?);
        write!(output, "{body},\n  \"files\": [")?;
        let mut first = true;
        self.plain_file_preview(captured, |file| {
            output.write_all(if first { b"\n    " } else { b",\n    " })?;
            first = false;
            serde_json::to_writer(&mut output, &file)?;
            Ok(())
        })?;
        output.write_all(b"\n  ]\n}\n")?;
        output
            .into_inner()
            .map_err(|error| error.into_error())?
            .sync_all()?;
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
    if receipt.mode == super::privacy_receipt::PublicationMode::SessionPlain {
        let metadata = super::storage::metadata_local(repo.root(), &receipt.published)?;
        ensure!(
            metadata.is_session_line()
                && Some(&metadata.session) == receipt.projected_session_id.as_ref()
                && metadata.session_privacy.as_ref().is_none_or(
                    |privacy| privacy.scheme == super::session_privacy::SessionScheme::Plain
                ),
            "plain session receipt differs from its accepted metadata"
        );
        return Ok(metadata.session);
    }
    let commit = read(
        &repo,
        &["cat-file", "commit", &receipt.published],
        MAX_COMMIT,
    )?;
    let parents = commit_parents(&commit)?;
    let snapshot = published::reuse(&repo, &repo, &receipt.published, &parents)?;
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
        receipt.mode.uses_generated_history(),
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

/// Identity, dates, encoding and message belong to the source commit. Signatures and merge
/// attestations cannot survive replacing its tree or parents, so only descriptive headers carry over.
fn commit_body(tree: &str, parents: &[String], original: &[u8]) -> Result<Vec<u8>> {
    let boundary = original
        .windows(2)
        .position(|bytes| bytes == b"\n\n")
        .context("publication source commit has no header boundary")?;
    let mut author = None;
    let mut committer = None;
    let mut encoding = None;
    let mut retained_header = false;
    for line in original[..boundary].split(|byte| *byte == b'\n') {
        if line.starts_with(b" ") {
            ensure!(
                !retained_header,
                "publication commit identity or encoding has a continuation"
            );
            continue;
        }
        retained_header = false;
        let (field, value) = if let Some(value) = line.strip_prefix(b"author ") {
            (&mut author, value)
        } else if let Some(value) = line.strip_prefix(b"committer ") {
            (&mut committer, value)
        } else if let Some(value) = line.strip_prefix(b"encoding ") {
            (&mut encoding, value)
        } else {
            continue;
        };
        ensure!(
            !value.is_empty() && !value.contains(&0) && !value.contains(&b'\r'),
            "publication commit identity or encoding is malformed"
        );
        ensure!(
            field.replace(line).is_none(),
            "publication commit has duplicate identity or encoding headers"
        );
        retained_header = true;
    }
    let author = author.context("publication source commit has no author")?;
    let committer = committer.context("publication source commit has no committer")?;
    let mut body = format!("tree {tree}\n").into_bytes();
    for parent in parents {
        body.extend_from_slice(format!("parent {parent}\n").as_bytes());
    }
    for line in [Some(author), Some(committer), encoding]
        .into_iter()
        .flatten()
    {
        body.extend_from_slice(line);
        body.push(b'\n');
    }
    body.push(b'\n');
    body.extend_from_slice(&original[boundary + 2..]);
    Ok(body)
}

/// Review uses published object bytes; non-UTF-8 fields remain lossless as escaped bytes.
fn commit_preview(bytes: &[u8]) -> Result<serde_json::Value> {
    let boundary = bytes
        .windows(2)
        .position(|bytes| bytes == b"\n\n")
        .context("publication preview commit has no header boundary")?;
    let text = |value: &[u8]| match std::str::from_utf8(value) {
        Ok(value) => json!(value),
        Err(_) => json!({"escaped_bytes": value.escape_ascii().to_string()}),
    };
    let mut preview = json!({"encoding": null, "message": text(&bytes[boundary + 2..])});
    for line in bytes[..boundary].split(|byte| *byte == b'\n') {
        for field in ["author", "committer", "encoding"] {
            if let Some(value) = line
                .strip_prefix(field.as_bytes())
                .and_then(|value| value.strip_prefix(b" "))
            {
                preview[field] = text(value);
            }
        }
    }
    Ok(preview)
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

/// Commits whose tree carries `privacy/envelope.json`, answered by one batch lookup.
fn enveloped_commits(repo: &Repo, commits: &[String]) -> Result<BTreeSet<String>> {
    let specs = commits
        .iter()
        .map(|commit| format!("{commit}:privacy/envelope.json"))
        .collect();
    let mut found = BTreeSet::new();
    let mut answered = 0;
    repo.git_cat_file_batch_check_with_policy(
        specs,
        super::repo::ReadPolicy::LocalOnly,
        |_, kind, _| {
            let commit = commits
                .get(answered)
                .context("privacy envelope lookup returned extra answers")?;
            if kind != "missing" {
                found.insert(commit.clone());
            }
            answered += 1;
            Ok(())
        },
    )?;
    ensure!(
        answered == commits.len(),
        "privacy envelope lookup is incomplete"
    );
    Ok(found)
}

/// Raw parent lists of local commits, read through one batch process.
fn commit_parent_map(repo: &Repo, commits: &[String]) -> Result<BTreeMap<String, Vec<String>>> {
    let mut parents = BTreeMap::new();
    repo.git_cat_file_batch_with_policy(
        commits.to_vec(),
        MAX_COMMIT,
        super::repo::ReadPolicy::LocalOnly,
        |oid, kind, body| {
            let super::repo::ObjectBody::Read(bytes) = body else {
                anyhow::bail!("privacy source object is unavailable");
            };
            ensure!(kind == "commit", "privacy source object is unavailable");
            parents.insert(oid.to_owned(), commit_parents(bytes)?);
            Ok(())
        },
    )?;
    ensure!(
        commits.iter().all(|oid| parents.contains_key(oid)),
        "privacy source object is unavailable"
    );
    Ok(parents)
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
