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
    path::{Path, PathBuf},
};

const MAX_COMMIT: usize = 1024 * 1024;
#[cfg(test)]
const LEGACY_COMMIT_SUFFIX: &str = "author AgentGit Privacy <privacy@agentgit.local> 0 +0000\ncommitter AgentGit Privacy <privacy@agentgit.local> 0 +0000\n\nPrivacy-projected snapshot\n";

mod accepted;
#[cfg(test)]
mod accepted_tests;
#[cfg(test)]
mod commit_tests;
mod delivery;
pub(crate) mod modes;
mod published;
#[cfg(test)]
mod receipt_tests;
pub mod sessions;
mod state;

#[derive(Debug, thiserror::Error)]
#[error(
    "accepted published commit {published} on branch {branch} is unavailable in this checkout's retained object cache"
)]
pub(crate) struct MissingAcceptedObjects {
    pub branch: String,
    pub published: String,
}

/// The accepted ledger is local bookkeeping that only agit writes, so no user command repairs
/// it; deleting it or publishing again could republish commits under new public ids. Its
/// refusal therefore asks for a report instead of naming a recovery.
#[derive(Debug, thiserror::Error)]
#[error(
    "this checkout's record of what it already published to the Hub is missing or does not match its commits, so agit cannot tell which commits are already public. Your local commits are unchanged. This is most likely a bug in agit, not something you did, and running the command again will not fix it. Keep this checkout as it is and report the problem with this message and the output of `agit status`"
)]
pub(crate) struct InconsistentAcceptedMapping;

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
    /// Boundary commits mapped under their own ids because the destination's advertisement
    /// reaches them; only these may leave their ancestry out of a candidate.
    held_boundary: BTreeSet<String>,
    /// [`super::privacy_receipt::tag_listing`] of the source's tags, read before `source` was
    /// frozen.
    source_tags: String,
    session_manifest: super::session_privacy::SessionPublicationManifest,
    #[cfg(test)]
    preparations: usize,
    _lock: fs::File,
}

pub(crate) enum PreviewEntry<'a> {
    Snapshot(&'a serde_json::Value),
    File(&'a serde_json::Value),
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

    /// The source's tag listing this publication may be acknowledged with; see
    /// [`super::privacy_receipt::save_acknowledged_tags`].
    pub(crate) fn source_tags(&self) -> &str {
        &self.source_tags
    }

    pub fn session_manifest(&self) -> &super::session_privacy::SessionPublicationManifest {
        &self.session_manifest
    }

    pub(crate) fn prepare_acceptance(&mut self) -> Result<()> {
        self.accepted_ledger
            .prepare(&self.candidates, &self.held_boundary)
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
        self.write_preview_with(destination, source, captured, |_| Ok(()))
    }

    pub(crate) fn write_preview_with(
        &self,
        destination: &serde_json::Value,
        source: &Repo,
        captured: &crate::hub::git::CapturedPublication,
        mut emit: impl FnMut(PreviewEntry<'_>) -> Result<()>,
    ) -> Result<std::path::PathBuf> {
        self.verify_source(source)?;
        let policy = effective_policy(source, &self.additional_policies)?;
        let snapshots = self.session_preview()?;
        for snapshot in &snapshots {
            emit(PreviewEntry::Snapshot(snapshot))?;
        }
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
            emit(PreviewEntry::File(&file))?;
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

/// Destination refs publications to `identity` leave out because the Hub refused them as
/// withdrawn (see `accepted::Ledger::withheld`).
pub(crate) fn withheld_refs(
    source: &Repo,
    identity: &crate::hub::identity::RemoteIdentity,
) -> Result<BTreeSet<String>> {
    Ok(accepted::Ledger::open(source, identity)?.withheld().clone())
}

/// Record refs the Hub refused as withdrawn, so later publications to `identity` leave them out.
pub(crate) fn withhold_refs(
    source: &Repo,
    identity: &crate::hub::identity::RemoteIdentity,
    refs: &[String],
) -> Result<()> {
    accepted::Ledger::open(source, identity)?.withhold(refs)
}

/// Let publications to `identity` plan these refs again, as when the person names one explicitly.
pub(crate) fn release_refs(
    source: &Repo,
    identity: &crate::hub::identity::RemoteIdentity,
    refs: &[String],
) -> Result<()> {
    accepted::Ledger::open(source, identity)?.release(refs)
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

/// The store holding the generated history a receipt names, with its shared lock.
pub(crate) fn receipt_store(
    source: &Repo,
    receipt: &super::privacy_receipt::PublicationReceipt,
) -> Result<(Repo, fs::File)> {
    receipt_cache(source, receipt)
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
            mapping.object_source(source, &parent, branch.unwrap_or("selected history"))?,
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

/// Packs, and loose objects, past which a publication store is compacted: the thresholds of Git's
/// own automatic maintenance (`gc.autoPackLimit`, `gc.auto`).
const COMPACT_PACKS: u64 = 50;
const COMPACT_LOOSE: u64 = 6700;

/// The hidden entry that compacts one publication store in the background.
pub const COMPACT: &str = "__privacy-compact-v1";

fn store_counts(repo: &Repo) -> Result<impl Fn(&str) -> u64> {
    let counts = repo.git(&["count-objects", "-v"])?;
    Ok(move |name: &str| {
        counts
            .lines()
            .find_map(|line| line.strip_prefix(name)?.trim().parse::<u64>().ok())
            .unwrap_or(0)
    })
}

fn needs_compaction(repo: &Repo) -> Result<bool> {
    let field = store_counts(repo)?;
    Ok(field("packs:") > COMPACT_PACKS || field("count:") > COMPACT_LOOSE)
}

/// Start compacting the store at `root` in a detached process once it needs it.
///
/// A repack takes as long as the store is large, and a publication that waited for it would hold
/// every caller for that long. The publication never depends on the result, so a store that cannot
/// be compacted, or a process that cannot start, still publishes and is compacted at a later push.
pub(crate) fn schedule_compaction(root: &Path) {
    if !needs_compaction(&Repo::at(root)).unwrap_or(false) {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut command = std::process::Command::new(executable);
    command
        .arg(COMPACT)
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// Run [`COMPACT`]: `None` when `args` name another command.
pub fn compaction_entry(args: &[std::ffi::OsString]) -> Option<i32> {
    if args.get(1)?.to_str()? != COMPACT {
        return None;
    }
    let [_, _, root] = args else {
        return Some(4);
    };
    let root = Path::new(root);
    // Only a store under a publication directory: the argument comes from this binary, but the
    // entry must not become a way to repack an arbitrary repository.
    let is_store = root.is_absolute()
        && root.join(".git").is_dir()
        && root
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "privacy-publication");
    if !is_store || crate::infra::git_runtime::initialize().is_err() {
        return Some(4);
    }
    let compacted = (|| -> Result<()> {
        let lock = crate::infra::config::state_file_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".git/agit-compact.lock"))?;
        // Another compaction of this store is already running; one is enough.
        if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
            return Ok(());
        }
        compact_store(root)?;
        Ok(())
    })();
    Some(if compacted.is_ok() { 0 } else { 4 })
}

/// Compact the publication store at `root` once it holds many packs or loose objects, and say
/// whether it did. Every publication imports into the store and nothing else packs it, so it would
/// keep each import's pack and each snapshot's loose objects for good. Unreachable objects stay
/// (`-k`): what pending candidates and outbox receipts name is not always reachable from the
/// store's refs, and a publication running alongside may still write objects no ref names yet.
/// Because nothing is pruned, a repack needs no publication lock: Git's own repack leaves every
/// object either in a pack or loose. Only the store's own objects are packed (`-l`): were it ever
/// to gain an alternate, packing everything would copy the source's private objects into the
/// publication store.
pub(crate) fn compact_store(root: &Path) -> Result<bool> {
    let repo = Repo::at(root);
    if !needs_compaction(&repo)? {
        return Ok(false);
    }
    let field = store_counts(&repo)?;
    // The new pack is written before anything it replaces is removed, so compaction needs about
    // the store's size free; starting without it would fill the disk and then fail.
    let needed = (field("size:") + field("size-pack:")) * 1024;
    let available = fs2::available_space(root)?;
    ensure!(
        available >= needed,
        "compacting it needs about {} MiB free on its disk and {} MiB are free",
        needed >> 20,
        available >> 20
    );
    repo.git(&["repack", "-a", "-d", "-k", "-l", "-q"])?;
    Ok(true)
}

/// The publication stores kept for the repository whose common Git directory is `common`, each
/// with the bytes it occupies.
pub(crate) fn publication_stores(common: &Path) -> Vec<(PathBuf, u64)> {
    let Ok(entries) = fs::read_dir(common.join("agit/privacy-publication")) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.join(".git").is_dir())
        .map(|path| {
            let bytes = directory_bytes(&path);
            (path, bytes)
        })
        .collect()
}

fn directory_bytes(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => directory_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |meta| meta.len()),
            _ => 0,
        })
        .sum()
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

/// The largest `privacy` directory [`enveloped_commits`] reads; a larger one is refused.
const MAX_PRIVACY_TREE: u64 = 1024 * 1024;

/// Commits whose tree has an entry at `privacy/envelope.json`, whatever its type, read from local
/// objects through at most two batch processes. The `privacy` directory is read rather than the
/// whole path resolved: Git versions differ in whether a submodule at that path is reported or
/// reported missing, and a missing report would let the entry through as absent. A commit that is
/// not available locally is an error, never a commit without an envelope.
pub(crate) fn enveloped_commits(repo: &Repo, commits: &[String]) -> Result<BTreeSet<String>> {
    let mut specs = Vec::with_capacity(commits.len() * 2);
    for commit in commits {
        ensure!(
            valid_oid(commit),
            "privacy envelope lookup needs commit ids"
        );
        specs.push(commit.clone());
        specs.push(format!("{commit}:privacy"));
    }
    let mut answers = Vec::with_capacity(specs.len());
    repo.git_cat_file_batch_check_with_policy(
        specs,
        super::repo::ReadPolicy::LocalOnly,
        |oid, kind, size| {
            answers.push((oid.to_owned(), kind.to_owned(), size));
            Ok(())
        },
    )?;
    ensure!(
        answers.len() == commits.len() * 2,
        "privacy envelope lookup is incomplete"
    );
    // A `privacy` that is absent, a file or a submodule has nothing below it.
    let mut directories = BTreeMap::<String, Vec<&String>>::new();
    for (commit, answer) in commits.iter().zip(answers.chunks(2)) {
        ensure!(
            answer[0].1 == "commit",
            "privacy source object is unavailable"
        );
        let (tree, kind, size) = &answer[1];
        if kind == "tree" {
            ensure!(
                *size <= MAX_PRIVACY_TREE,
                "privacy directory exceeds the size limit"
            );
            directories.entry(tree.clone()).or_default().push(commit);
        }
    }
    let trees: Vec<String> = directories.keys().cloned().collect();
    let oid_bytes = commits.first().map_or(0, |commit| commit.len() / 2);
    let mut found = BTreeSet::new();
    let mut answered = 0;
    // The sizes are checked above; a cap here would start another check process.
    repo.git_cat_file_batch_with_policy(
        trees.clone(),
        usize::MAX,
        super::repo::ReadPolicy::LocalOnly,
        |_, kind, body| {
            let tree = trees
                .get(answered)
                .context("privacy envelope lookup returned extra answers")?;
            answered += 1;
            let super::repo::ObjectBody::Read(bytes) = body else {
                anyhow::bail!("privacy source object is unavailable");
            };
            ensure!(kind == "tree", "privacy source object is unavailable");
            if tree_has_entry(bytes, b"envelope.json", oid_bytes)? {
                found.extend(directories[tree].iter().map(|commit| (*commit).clone()));
            }
            Ok(())
        },
    )?;
    ensure!(
        answered == trees.len(),
        "privacy envelope lookup is incomplete"
    );
    Ok(found)
}

/// Whether a raw tree object has an entry called `name`, of any mode.
fn tree_has_entry(mut tree: &[u8], name: &[u8], oid_bytes: usize) -> Result<bool> {
    while !tree.is_empty() {
        let space = tree.iter().position(|byte| *byte == b' ');
        let nul = tree.iter().position(|byte| *byte == 0);
        let (Some(space), Some(nul)) = (space, nul) else {
            anyhow::bail!("privacy directory is malformed");
        };
        ensure!(
            space < nul && tree.len() >= nul + 1 + oid_bytes,
            "privacy directory is malformed"
        );
        if &tree[space + 1..nul] == name {
            return Ok(true);
        }
        tree = &tree[nul + 1 + oid_bytes..];
    }
    Ok(false)
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
    // Objects are written one tree depth at a time, each depth in one Git process: a tree names
    // its entries by object ID, so a depth can be written only once the one below it has been.
    // One process per object makes a snapshot cost as many processes as it has files and
    // directories, and a publication pays that for every snapshot it projects, reused or not.
    struct Node {
        files: Vec<(String, usize)>,
        directories: Vec<(String, usize)>,
        height: usize,
    }
    fn flatten(tree: Tree, blobs: &mut Vec<Vec<u8>>, nodes: &mut Vec<Node>) -> usize {
        let files = tree
            .files
            .into_iter()
            .map(|(name, bytes)| {
                blobs.push(bytes);
                (name, blobs.len() - 1)
            })
            .collect();
        let directories: Vec<(String, usize)> = tree
            .directories
            .into_iter()
            .map(|(name, subtree)| (name, flatten(subtree, blobs, nodes)))
            .collect();
        let height = directories
            .iter()
            .map(|(_, child)| nodes[*child].height + 1)
            .max()
            .unwrap_or(0);
        nodes.push(Node {
            files,
            directories,
            height,
        });
        nodes.len() - 1
    }
    let mut blobs = Vec::new();
    let mut nodes = Vec::new();
    let root = flatten(tree, &mut blobs, &mut nodes);
    let blob_ids = write_objects(repo, "blob", &blobs)?;
    let mut tree_ids: Vec<Option<String>> = vec![None; nodes.len()];
    for height in 0..=nodes[root].height {
        let level = (0..nodes.len())
            .filter(|index| nodes[*index].height == height)
            .collect::<Vec<_>>();
        let mut contents = Vec::with_capacity(level.len());
        for index in &level {
            let node = &nodes[*index];
            let mut entries = Vec::new();
            for (name, blob) in &node.files {
                entries.push((name.as_bytes().to_vec(), "100644", &blob_ids[*blob]));
            }
            for (name, child) in &node.directories {
                let mut key = name.as_bytes().to_vec();
                // Git orders a directory as though its name ended in a slash.
                key.push(b'/');
                let id = tree_ids[*child]
                    .as_ref()
                    .context("projected subtree was not written")?;
                entries.push((key, "40000", id));
            }
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut bytes = Vec::new();
            for (key, mode, id) in entries {
                let name = if mode == "40000" {
                    &key[..key.len() - 1]
                } else {
                    &key[..]
                };
                bytes.extend_from_slice(mode.as_bytes());
                bytes.push(b' ');
                bytes.extend_from_slice(name);
                bytes.push(0);
                bytes.extend_from_slice(&hex::decode(id)?);
            }
            contents.push(bytes);
        }
        for (index, id) in level
            .into_iter()
            .zip(write_objects(repo, "tree", &contents)?)
        {
            tree_ids[index] = Some(id);
        }
    }
    tree_ids[root]
        .take()
        .context("projected tree was not written")
}

/// Write `contents` as objects of `kind` in one Git process, returning their IDs in order.
fn write_objects(repo: &Repo, kind: &str, contents: &[Vec<u8>]) -> Result<Vec<String>> {
    if contents.is_empty() {
        return Ok(Vec::new());
    }
    let directory = tempfile::tempdir()?;
    let mut paths = String::new();
    for (index, bytes) in contents.iter().enumerate() {
        let path = directory.path().join(index.to_string());
        std::fs::write(&path, bytes)?;
        let path = path
            .to_str()
            .filter(|path| !path.contains(['\n', '\r']))
            .context("projected object path cannot be listed")?;
        paths.push_str(path);
        paths.push('\n');
    }
    // Content is written as given: path attributes must not filter what the projection decided.
    let ids = input(
        repo,
        &[
            "hash-object",
            "-t",
            kind,
            "-w",
            "--no-filters",
            "--stdin-paths",
        ],
        paths.as_bytes(),
    )?;
    let ids = ids.lines().map(str::to_owned).collect::<Vec<_>>();
    ensure!(
        ids.len() == contents.len() && ids.iter().all(|id| valid_oid(id)),
        "Git returned an invalid projected object ID"
    );
    Ok(ids)
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

#[cfg(test)]
mod store_tests {
    use super::*;

    /// Runs Git the way agit does for the store, so the fixture and the compaction share one Git.
    fn git(root: &Path, args: &[&str]) -> String {
        let identity = [
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ];
        Repo::at(root)
            .git(&[&identity[..], args].concat())
            .unwrap_or_else(|error| panic!("git {args:?}: {error:#}"))
    }

    fn packs(root: &Path) -> usize {
        fs::read_dir(root.join(".git/objects/pack"))
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .count()
    }

    /// Projected trees keep the object IDs `git mktree` gives the same entries, in both object
    /// formats: cached snapshots and published commits are identified by them. A writer that
    /// sorted directories by bare name (`a` before `a-b`), or wrote a directory mode as `040000`,
    /// produces another tree for the same files and the cache rejects its own candidates.
    #[test]
    fn projected_trees_match_mktree() {
        for format in ["sha1", "sha256"] {
            let root = tempfile::tempdir().unwrap();
            git(
                root.path(),
                &["init", "-q", &format!("--object-format={format}")],
            );
            let repo = Repo::at(root.path());
            let files: BTreeMap<String, Vec<u8>> = [
                "a-b",
                "a.b",
                "a/x",
                "a/y/z",
                "a0",
                "b/c/d/e",
                "privacy/envelope.json",
                "ab",
            ]
            .into_iter()
            .map(|path| (path.to_owned(), format!("{path}\n").into_bytes()))
            .collect();
            fn mktree(repo: &Repo, entries: &[(&str, &str, String)]) -> String {
                let mut records = String::new();
                for (mode, name, id) in entries {
                    let kind = if *mode == "040000" { "tree" } else { "blob" };
                    records.push_str(&format!("{mode} {kind} {id}\t{name}\0"));
                }
                input(repo, &["mktree", "-z"], records.as_bytes()).unwrap()
            }
            let blob = |path: &str| {
                input(
                    &repo,
                    &["hash-object", "-w", "--stdin"],
                    format!("{path}\n").as_bytes(),
                )
                .unwrap()
            };
            let y = mktree(&repo, &[("100644", "z", blob("a/y/z"))]);
            let a = mktree(&repo, &[("100644", "x", blob("a/x")), ("040000", "y", y)]);
            let d = mktree(&repo, &[("100644", "e", blob("b/c/d/e"))]);
            let c = mktree(&repo, &[("040000", "d", d)]);
            let b = mktree(&repo, &[("040000", "c", c)]);
            let privacy = mktree(
                &repo,
                &[("100644", "envelope.json", blob("privacy/envelope.json"))],
            );
            let expected = mktree(
                &repo,
                &[
                    ("100644", "a-b", blob("a-b")),
                    ("100644", "a.b", blob("a.b")),
                    ("040000", "a", a),
                    ("100644", "a0", blob("a0")),
                    ("100644", "ab", blob("ab")),
                    ("040000", "b", b),
                    ("040000", "privacy", privacy),
                ],
            );
            assert_eq!(write_tree(&repo, files).unwrap(), expected, "{format}");
        }
    }

    /// A store past Git's own maintenance thresholds is compacted into one pack without losing an
    /// object that no ref reaches any more, since pending candidates and receipts can name such
    /// objects; below the thresholds it is left alone. A compaction that pruned (`gc`) or dropped
    /// unreachable packed objects (`repack -a -d` without `-k`) loses the orphaned blob.
    #[test]
    fn a_crowded_store_is_compacted_without_losing_unreachable_objects() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "core.logAllRefUpdates", "false"]);
        // `git commit` past the pack threshold would start Git's own background gc, which deletes
        // packs while this test reads them. agit writes a store only with plumbing commands.
        git(root, &["config", "gc.auto", "0"]);
        git(root, &["config", "maintenance.auto", "false"]);
        git(root, &["switch", "-q", "-c", "candidate"]);
        fs::write(
            root.join("candidate.txt"),
            "only a pending candidate names this\n",
        )
        .unwrap();
        git(root, &["add", "candidate.txt"]);
        git(root, &["commit", "-qm", "candidate"]);
        let orphan = git(root, &["rev-parse", "HEAD:candidate.txt"]);
        git(root, &["repack", "-q"]);
        git(root, &["switch", "-q", "--orphan", "main"]);
        git(root, &["branch", "-q", "-D", "candidate"]);
        for index in 0..=COMPACT_PACKS {
            fs::write(root.join("turn.txt"), format!("turn {index}\n")).unwrap();
            git(root, &["add", "turn.txt"]);
            git(root, &["commit", "-qm", "turn"]);
            git(root, &["repack", "-q"]);
        }
        assert!(packs(root) as u64 > COMPACT_PACKS);

        assert!(compact_store(root).unwrap());
        assert_eq!(packs(root), 1);
        git(root, &["cat-file", "-e", &orphan]);
        assert!(!compact_store(root).unwrap());
    }
}
