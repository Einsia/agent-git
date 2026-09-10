//! Lands an already activated session merge without exposing a public lifecycle entry point.
//!
//! The retained ordinary tree fixes VIEW and shared content before native evidence extends LOG.
//! Pending replay uses immutable receipt objects and exact transaction bytes, never new native
//! input or changed worktree content. Transaction retirement follows durable Landed publication.

use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Read;

use super::{current_head, read_native, require_destination_routing};
use crate::Result;
use crate::commands::{merge, plumbing};
use crate::domain::link::{self, ArchiveLinkSnapshot, Link};
use crate::domain::merge_archive::{
    ArchiveJournal, ArchiveJournalGuard, ArchivePhase, ArchivePublicationKind, ExplorationBinding,
    PreparedArchivePublication, RetainedMergeLanding, checked_landing_transaction,
};
use crate::domain::secret_filter::{KeyStore, Matcher, RepositoryDictionary};
use crate::domain::{
    archive_history, mergetx, meta, native_archive, repo::Repo, storage, store::Store, transcript,
};

/// Selection is explicit and already bound by durable preparation; this core does not launch.
pub struct LandingRequest<'a> {
    pub repo: &'a Repo,
    pub source_repo: &'a Repo,
    pub store: &'a Store,
    pub binding: &'a ExplorationBinding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LandingOutcome {
    pub commit: String,
    pub archived_records: usize,
}

pub fn land(request: LandingRequest<'_>, global: &Matcher) -> Result<LandingOutcome> {
    require_destination_routing(request.repo, &request.binding.role.branch)?;
    let dictionary = RepositoryDictionary::open(request.repo.root())?;
    land_with(request, &dictionary, global, read_native, |_| Ok(()))
}

/// Replay uses the retained candidate before opening a source repository or a secret vault.
/// A fresh Open exploration returns None so its caller can prepare the frozen source inputs.
pub(super) fn replay(
    repo: &Repo,
    store: &Store,
    binding: &ExplorationBinding,
) -> Result<Option<LandingOutcome>> {
    require_destination_routing(repo, &binding.role.branch)?;
    let _branch = link::lock_branch(store, &binding.role.slug, &binding.role.branch)?;
    let _link = link::lock(store, &binding.native.runtime, &binding.native.session_id)?;
    let guard = ArchiveJournalGuard::acquire(repo.root(), &binding.role.generation)?;
    let control = mergetx::ControlGuard::acquire(repo.root())?;
    require_destination_routing(repo, &binding.role.branch)?;
    guard.recover_pending()?;
    let journal = guard.read()?.context("merge replay journal is missing")?;
    ensure!(
        journal.binding == *binding && journal.activation.is_none() && journal.detach.is_none(),
        "merge replay differs from its attached activated journal"
    );
    ensure!(
        matches!(
            journal.phase,
            ArchivePhase::Open | ArchivePhase::Landed { .. }
        ),
        "merge continuation requires an Open or Landed archive disposition"
    );
    let request = LandingRequest {
        repo,
        source_repo: repo,
        store,
        binding,
    };
    let selected = selected_link(&request)?;
    if journal.phase == ArchivePhase::Open && journal.publication.is_none() {
        ensure!(
            journal.landing.is_none(),
            "merge landing has incomplete retained intent"
        );
        let current = control
            .read_activation_snapshot()?
            .context("merge transaction is missing")?;
        checked_landing_transaction(&current.json, binding)?;
        return Ok(None);
    }
    plumbing::recover_interrupted_checkout(repo)?;
    if matches!(journal.phase, ArchivePhase::Landed { .. }) {
        return complete(&request, &control, &journal, &mut |_| Ok(())).map(Some);
    }
    publish(&request, &guard, &control, &journal, &selected, &mut |_| {
        Ok(())
    })
    .map(Some)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Checkpoint {
    Prepared,
    BeforeCas,
    AfterCas,
    Landed,
    BeforeRetire,
    Retired,
}

fn require_selection_graph(repo: &Repo) -> Result<()> {
    let (status, _, _) = repo.git_status_local(&["config", "--get", "extensions.partialclone"])?;
    ensure!(
        status == Some(1),
        "archive merge selection requires a complete local repository without partial-clone routing"
    );
    let (status, values, _) = repo.git_status_local(&[
        "config",
        "--bool",
        "--get-regexp",
        "^remote\\..*\\.promisor$",
    ])?;
    ensure!(
        status == Some(1)
            || (status == Some(0) && values.lines().all(|line| line.ends_with(" false"))),
        "archive merge selection refuses promisor remotes; obtain complete local history before landing"
    );
    ensure!(
        repo.git(&["rev-parse", "--is-shallow-repository"])? == "false",
        "archive merge selection requires complete local history; deepen the repository before landing"
    );
    match std::fs::symlink_metadata(repo.git_path("info/grafts")?) {
        Ok(metadata) => ensure!(
            metadata.is_file() && metadata.len() == 0,
            "archive merge selection refuses local grafts; remove the history override before landing"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn target_is_checked_out(repo: &Repo, branch: &str) -> Result<bool> {
    let (status, output, _) = repo.git_status_local(&["symbolic-ref", "--quiet", "HEAD"])?;
    match status {
        Some(0) => Ok(output.trim_end_matches(['\r', '\n']) == format!("refs/heads/{branch}")),
        Some(1) => Ok(false),
        _ => anyhow::bail!("cannot determine the merge destination checkout"),
    }
}

fn require_retained_worktree(
    request: &LandingRequest<'_>,
    landing: &RetainedMergeLanding,
) -> Result<()> {
    ensure!(
        target_is_checked_out(request.repo, &request.binding.role.branch)?
            == landing.worktree_tree.is_some(),
        "merge destination checkout changed after candidate preparation"
    );
    if let Some(tree) = &landing.worktree_tree {
        ensure!(
            plumbing::tree_overlay_worktree(
                request.repo,
                tree,
                &shared_paths(request.repo, tree)?
            )? == *tree,
            "shared worktree content changed after merge preparation; restore the retained shared result before retrying"
        );
    }
    Ok(())
}

const MAX_SHARED_PATH_BYTES: u64 = 4 * 1024 * 1024;

fn shared_paths(repo: &Repo, tree: &str) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();
    for args in [
        vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-only",
            "--no-renames",
            "-z",
            tree,
            "--",
        ],
        vec!["ls-files", "--others", "--exclude-standard", "-z", "--"],
    ] {
        let mut command = std::process::Command::new("git");
        command
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(repo.root())
            .args(args)
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = command.spawn()?;
        let mut bytes = Vec::new();
        let read = child
            .stdout
            .take()
            .context("shared path reader has no output")?
            .take(MAX_SHARED_PATH_BYTES + 1)
            .read_to_end(&mut bytes);
        if read.is_err() || bytes.len() as u64 > MAX_SHARED_PATH_BYTES {
            let _ = child.kill();
        }
        let status = child.wait()?;
        read?;
        ensure!(
            bytes.len() as u64 <= MAX_SHARED_PATH_BYTES,
            "shared merge paths exceed their byte limit"
        );
        ensure!(status.success(), "cannot enumerate shared merge paths");
        add_shared_paths(&bytes, &mut paths)?;
    }
    for path in &paths {
        match std::fs::symlink_metadata(repo.root().join(path)) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink() || repo.root().join(path).exists(),
                "shared merge overlay cannot preserve a dangling symlink"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(paths.into_iter().collect())
}

fn add_shared_paths(bytes: &[u8], paths: &mut BTreeSet<String>) -> Result<()> {
    ensure!(
        bytes.len() as u64 <= MAX_SHARED_PATH_BYTES,
        "shared merge paths exceed their byte limit"
    );
    ensure!(
        bytes.is_empty() || bytes.last() == Some(&0),
        "shared merge path listing is not terminated"
    );
    for raw in bytes.split(|byte| *byte == 0).filter(|raw| !raw.is_empty()) {
        ensure!(
            raw.len() <= 4096,
            "shared merge path exceeds the immutable reader path limit"
        );
        let path = std::str::from_utf8(raw).context("shared merge path is not Unicode")?;
        ensure!(
            !std::path::Path::new(path).is_absolute()
                && path
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != ".."),
            "shared merge path is not repository relative"
        );
        if !merge::storage_exclusions(meta::LayoutVersion::V1)
            .iter()
            .any(|excluded| {
                path == *excluded
                    || path
                        .strip_prefix(excluded)
                        .is_some_and(|tail| tail.starts_with('/'))
            })
        {
            paths.insert(path.to_owned());
        }
        ensure!(
            paths.len() <= storage::MAX_SEQUENCE_EVENTS,
            "shared merge path count exceeds its limit"
        );
    }
    Ok(())
}

fn selected_link(request: &LandingRequest<'_>) -> Result<ArchiveLinkSnapshot> {
    let binding = request.binding;
    let selected = link::read_archive_link_snapshot(
        request.store,
        &binding.native.runtime,
        &binding.native.session_id,
    )?
    .context("merge landing runtime Link is missing")?;
    ensure!(
        selected.link.is_archive_for(
            &binding.role,
            &binding.native.runtime,
            &binding.native.session_id
        ) && selected.link.baseline_bytes == Some(binding.installed.bytes)
            && selected.link.baseline_hash.as_ref() == Some(&binding.installed.sha256),
        "merge landing runtime Link differs from its retained authority"
    );
    Ok(selected)
}

fn require_transaction(control: &mergetx::ControlGuard, json: &str) -> Result<()> {
    ensure!(
        control
            .read_activation_snapshot()?
            .as_ref()
            .map(|snapshot| snapshot.json.as_str())
            == Some(json),
        "merge landing transaction differs from its retained exact image"
    );
    Ok(())
}

fn land_with<K: KeyStore>(
    request: LandingRequest<'_>,
    dictionary: &RepositoryDictionary<K>,
    global: &Matcher,
    read_native: impl FnOnce(&Link) -> Result<Vec<u8>>,
    mut checkpoint: impl FnMut(Checkpoint) -> Result<()>,
) -> Result<LandingOutcome> {
    let binding = request.binding;
    let role = &binding.role;
    role.validate(role.origin_head.len())?;
    binding.native.validate()?;
    require_destination_routing(request.repo, &role.branch)?;
    let _branch = link::lock_branch(request.store, &role.slug, &role.branch)?;
    let _link = link::lock(
        request.store,
        &binding.native.runtime,
        &binding.native.session_id,
    )?;
    let guard = ArchiveJournalGuard::acquire(request.repo.root(), &role.generation)?;
    let control = mergetx::ControlGuard::acquire(request.repo.root())?;
    require_destination_routing(request.repo, &role.branch)?;
    guard.recover_pending()?;
    let journal = guard.read()?.context("merge landing journal is missing")?;
    ensure!(
        journal.binding == *binding && journal.activation.is_none() && journal.detach.is_none(),
        "merge landing differs from its attached activated journal"
    );
    let selected = selected_link(&request)?;
    plumbing::recover_interrupted_checkout(request.repo)?;
    if matches!(journal.phase, ArchivePhase::Landed { .. }) {
        return complete(&request, &control, &journal, &mut checkpoint);
    }
    ensure!(
        journal.phase == ArchivePhase::Open,
        "merge landing requires an Open exploration"
    );
    if journal.publication.is_some() {
        return publish(
            &request,
            &guard,
            &control,
            &journal,
            &selected,
            &mut checkpoint,
        );
    }
    ensure!(
        journal.landing.is_none(),
        "merge landing has incomplete retained intent"
    );
    let snapshot = control
        .read_activation_snapshot()?
        .context("merge landing transaction is missing")?;
    let tx = checked_landing_transaction(&snapshot.json, binding)?;
    ensure!(
        current_head(request.repo, &role.branch)? == role.origin_head,
        "merge landing target moved from its frozen head"
    );
    require_selection_graph(request.repo)?;
    require_selection_graph(request.source_repo)?;
    archive_history::verify_archive_append_target(
        request.repo,
        &role.origin_head,
        &role.origin_head,
    )?;
    archive_history::verify_frozen_source(request.source_repo, &binding.source.head)?;
    let source_log = archive_history::freeze_source_log(request.source_repo, &binding.source.head)?;

    let native = read_native(&selected.link)?;
    ensure!(
        native.len() <= storage::MAX_MATERIALIZED_BYTES,
        "merge native snapshot exceeds its byte limit"
    );
    let capture = super::capture_selected(&native, &journal)?;
    let prepared = merge::prepare_session_merge(
        request.repo,
        request.source_repo,
        &tx,
        &binding.source.head,
        Some(&source_log),
    )?
    .context("ordinary merge selection cannot be landed")?;
    let protected = if capture.record_count == 0 {
        String::new()
    } else {
        dictionary.protect_jsonl(&capture.records, global)?.text
    };
    ensure!(
        protected.len() <= storage::MAX_MATERIALIZED_BYTES,
        "protected merge evidence exceeds its byte limit"
    );
    let checked =
        native_archive::capture(protected.as_bytes(), 0, &hex::encode(Sha256::digest([])))?;
    ensure!(
        checked.record_count == capture.record_count && checked.unconsumed.is_empty(),
        "merge protection changed complete native record boundaries"
    );
    let envelopes =
        transcript::wrap_lines(&protected, &binding.native.runtime, &role.logical_session);
    plumbing::import_commit_graph(request.repo, request.source_repo, &binding.source.head)?;
    let on_target = target_is_checked_out(request.repo, &role.branch)?;
    let shared = if on_target {
        shared_paths(request.repo, &role.origin_head)?
    } else {
        Vec::new()
    };
    let base = plumbing::tree_overlay_worktree(request.repo, &role.origin_head, &shared)?;
    let (ordinary_tree, message) = merge::land_session_merge(request.repo, &tx, prepared, &base)?;
    let tree = append_log(request.repo, &ordinary_tree, &envelopes)?;
    let candidate = plumbing::commit_tree(
        request.repo,
        &tree,
        &[&role.origin_head, &binding.source.head],
        &message,
    )?;
    let mut pending = journal.clone();
    pending.landing = Some(RetainedMergeLanding {
        transaction_json: snapshot.json,
        ordinary_tree,
        worktree_tree: on_target.then_some(base),
    });
    pending.publication = Some(PreparedArchivePublication {
        kind: ArchivePublicationKind::MergeLanding {
            source_head: binding.source.head.clone(),
        },
        link_json: selected.json.clone(),
        expected_old: role.origin_head.clone(),
        candidate,
        candidate_tree: tree,
        prior_frontier: journal.consumed.clone(),
        next_frontier: capture.frontier,
        next_opencode: capture.opencode,
        appended_records: u64::try_from(capture.record_count)?,
        protected_suffix_sha256: hex::encode(Sha256::digest(envelopes.as_bytes())),
    });
    verify_publication(request.repo, &pending)?;
    require_transaction(
        &control,
        &pending.landing.as_ref().unwrap().transaction_json,
    )?;
    ensure!(
        selected_link(&request)?.json == selected.json,
        "merge Link changed before prepared publication"
    );
    require_destination_routing(request.repo, &role.branch)?;
    guard.replace(&journal, &pending)?;
    checkpoint(Checkpoint::Prepared)?;
    publish(
        &request,
        &guard,
        &control,
        &pending,
        &selected,
        &mut checkpoint,
    )
}

fn append_log(repo: &Repo, ordinary_tree: &str, envelopes: &str) -> Result<String> {
    if envelopes.is_empty() {
        return Ok(ordinary_tree.to_owned());
    }
    let mut files = storage::snapshot_files(envelopes, "")?;
    files.remove(meta::VIEW_FILE);
    let suffix = files
        .remove(meta::LOG_FILE)
        .context("merge suffix sequence is missing")?;
    let mut log = plumbing::regular_blob_text_at(repo, ordinary_tree, meta::LOG_FILE)?
        .context("ordinary merge LOG is missing")?;
    log.push_str(std::str::from_utf8(&suffix)?);
    storage::parse_sequence(&log)?;
    files.insert(meta::LOG_FILE.into(), log.into_bytes());
    plumbing::tree_apply_owned(
        repo,
        ordinary_tree,
        files
            .into_iter()
            .map(|(path, bytes)| (path, Some(bytes)))
            .collect(),
    )
}

fn verify_publication(repo: &Repo, journal: &ArchiveJournal) -> Result<usize> {
    let publication = journal
        .publication
        .as_ref()
        .context("merge publication is missing")?;
    let landing = journal
        .landing
        .as_ref()
        .context("retained ordinary merge result is missing")?;
    ensure!(
        publication.kind
            == (ArchivePublicationKind::MergeLanding {
                source_head: journal.binding.source.head.clone()
            }),
        "retained publication is not this merge landing"
    );
    let suffix = archive_history::verify_merge_landing(
        repo,
        &publication.expected_old,
        &journal.binding.source.head,
        &landing.ordinary_tree,
        &publication.candidate,
        &publication.candidate_tree,
    )?;
    ensure!(
        hex::encode(Sha256::digest(suffix.as_bytes())) == publication.protected_suffix_sha256,
        "merge landing evidence digest differs from its retained receipt"
    );
    let count = verify_suffix(&suffix, &journal.binding)?;
    ensure!(
        u64::try_from(count)? == publication.appended_records,
        "merge landing evidence count differs from its retained receipt"
    );
    Ok(count)
}

fn verify_suffix(suffix: &str, binding: &ExplorationBinding) -> Result<usize> {
    for line in suffix.split_inclusive('\n') {
        let envelope = storage::parse_envelope_line(line)?;
        ensure!(
            envelope.source == binding.native.runtime
                && envelope.session_id == binding.role.logical_session,
            "merge landing evidence belongs to another runtime or logical session"
        );
    }
    Ok(suffix.lines().count())
}

fn publish(
    request: &LandingRequest<'_>,
    guard: &ArchiveJournalGuard,
    control: &mergetx::ControlGuard,
    journal: &ArchiveJournal,
    selected: &ArchiveLinkSnapshot,
    checkpoint: &mut impl FnMut(Checkpoint) -> Result<()>,
) -> Result<LandingOutcome> {
    let publication = journal
        .publication
        .as_ref()
        .context("merge publication is missing")?;
    let landing = journal
        .landing
        .as_ref()
        .context("merge landing receipt is missing")?;
    ensure!(
        publication.link_json == selected.json,
        "merge publication runtime authority changed"
    );
    verify_publication(request.repo, journal)?;
    require_transaction(control, &landing.transaction_json)?;
    let head = current_head(request.repo, &request.binding.role.branch)?;
    if head == publication.expected_old {
        checkpoint(Checkpoint::BeforeCas)?;
        require_destination_routing(request.repo, &request.binding.role.branch)?;
        require_transaction(control, &landing.transaction_json)?;
        require_retained_worktree(request, landing)?;
        ensure!(
            selected_link(request)?.json == publication.link_json,
            "merge Link changed before ref publication"
        );
        plumbing::update_branch_cas_and_refresh(
            request.repo,
            &request.binding.role.branch,
            &publication.candidate,
            &publication.expected_old,
            landing.worktree_tree.is_some(),
        )?;
        checkpoint(Checkpoint::AfterCas)?;
    } else {
        ensure!(
            head == publication.candidate,
            "merge target moved outside its retained publication endpoints"
        );
    }
    finish_visible(request, guard, control, journal, checkpoint)
}

fn finish_visible(
    request: &LandingRequest<'_>,
    guard: &ArchiveJournalGuard,
    control: &mergetx::ControlGuard,
    journal: &ArchiveJournal,
    checkpoint: &mut impl FnMut(Checkpoint) -> Result<()>,
) -> Result<LandingOutcome> {
    let publication = journal
        .publication
        .as_ref()
        .context("merge publication is missing")?;
    let landing = journal
        .landing
        .as_ref()
        .context("merge landing receipt is missing")?;
    require_transaction(control, &landing.transaction_json)?;
    ensure!(
        selected_link(request)?.json == publication.link_json,
        "merge runtime authority changed before durable landing publication"
    );
    ensure!(
        current_head(request.repo, &request.binding.role.branch)? == publication.candidate,
        "merge target moved before durable landing publication"
    );
    let mut landed = journal.clone();
    landed.phase = ArchivePhase::Landed {
        merge_commit: publication.candidate.clone(),
    };
    landed.consumed = publication.next_frontier.clone();
    landed.opencode = publication.next_opencode.clone();
    landed.accepted_commit = Some(publication.candidate.clone());
    landed.previous_claims.clear();
    landed.publication = None;
    guard.replace(journal, &landed)?;
    checkpoint(Checkpoint::Landed)?;
    complete(request, control, &landed, checkpoint)
}

fn complete(
    request: &LandingRequest<'_>,
    control: &mergetx::ControlGuard,
    journal: &ArchiveJournal,
    checkpoint: &mut impl FnMut(Checkpoint) -> Result<()>,
) -> Result<LandingOutcome> {
    let ArchivePhase::Landed { merge_commit } = &journal.phase else {
        anyhow::bail!("merge completion requires durable Landed disposition")
    };
    let landing = journal
        .landing
        .as_ref()
        .context("landed merge has no retained transaction")?;
    let tree = request
        .repo
        .git(&["rev-parse", "--verify", &format!("{merge_commit}^{{tree}}")])?;
    let suffix = archive_history::verify_merge_landing(
        request.repo,
        &request.binding.role.origin_head,
        &request.binding.source.head,
        &landing.ordinary_tree,
        merge_commit,
        &tree,
    )?;
    let records = verify_suffix(&suffix, request.binding)?;
    archive_history::verify_archive_append_target(
        request.repo,
        merge_commit,
        &current_head(request.repo, &request.binding.role.branch)?,
    )?;
    checkpoint(Checkpoint::BeforeRetire)?;
    require_destination_routing(request.repo, &request.binding.role.branch)?;
    selected_link(request)?;
    control.complete_archive_landing(request.binding, &landing.transaction_json, merge_commit)?;
    checkpoint(Checkpoint::Retired)?;
    Ok(LandingOutcome {
        commit: merge_commit.clone(),
        archived_records: records,
    })
}

/// Cancellation may finish visible immutable intent while retaining the caller's lock order.
/// This replay uses imported objects only; it never resolves or rereads the source repository.
pub(super) fn complete_visible(
    repo: &Repo,
    store: &Store,
    binding: &ExplorationBinding,
    guard: &ArchiveJournalGuard,
    control: &mergetx::ControlGuard,
    journal: &ArchiveJournal,
) -> Result<LandingOutcome> {
    let request = LandingRequest {
        repo,
        source_repo: repo,
        store,
        binding,
    };
    let selected = selected_link(&request)?;
    if matches!(journal.phase, ArchivePhase::Landed { .. }) {
        plumbing::recover_interrupted_checkout(repo)?;
        return complete(&request, control, journal, &mut |_| Ok(()));
    }
    let head = current_head(repo, &binding.role.branch)?;
    ensure!(
        journal.phase == ArchivePhase::Open
            && journal
                .publication
                .as_ref()
                .is_some_and(|publication| head == publication.candidate),
        "cancellation cannot publish an unlanded merge candidate"
    );
    ensure!(
        journal
            .publication
            .as_ref()
            .is_some_and(|publication| publication.link_json == selected.json),
        "visible merge publication runtime authority changed"
    );
    verify_publication(repo, journal)?;
    require_transaction(
        control,
        &journal
            .landing
            .as_ref()
            .context("merge landing receipt is missing")?
            .transaction_json,
    )?;
    plumbing::recover_interrupted_checkout(repo)?;
    finish_visible(&request, guard, control, journal, &mut |_| Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::merge_archive::{self, FrozenMergeSource, MergeArchiveRole, RuntimeLinkKey};
    use crate::domain::native_archive::Frontier;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use zeroize::Zeroizing;

    const SESSION: &str = "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BASELINE: &[u8] = b"{\"type\":\"session_meta\",\"id\":\"INSTALLED\"}\n";
    const EXPLORATION: &[u8] = b"{\"type\":\"assistant\",\"text\":\"private exploration\"}\n";

    #[derive(Default)]
    struct MemoryKeys(Mutex<HashMap<String, Vec<u8>>>);

    impl KeyStore for MemoryKeys {
        fn get(&self, id: &str) -> Result<Zeroizing<Vec<u8>>> {
            self.0
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(Zeroizing::new)
                .context("missing fixture key")
        }
        fn set(&self, id: &str, key: &[u8]) -> Result<()> {
            self.0.lock().unwrap().insert(id.into(), key.to_vec());
            Ok(())
        }
        fn delete(&self, id: &str) -> Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        repo: Repo,
        source: Repo,
        store: Store,
        binding: ExplorationBinding,
        native: PathBuf,
        dictionary: RepositoryDictionary<MemoryKeys>,
        original_log: String,
        source_event: String,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_native("codex", BASELINE)
        }

        fn with_native(runtime: &str, baseline: &[u8]) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(&directory.path().join("target")).unwrap();
            let source = Repo::init(&directory.path().join("source")).unwrap();
            let mut metadata = meta::Meta::new(SESSION.into(), "codex".into(), "/work".into());
            metadata.turn = Some(1);
            meta::write(repo.root(), &metadata).unwrap();
            let original_log = transcript::wrap_lines(
                "{\"type\":\"user\",\"text\":\"inherited\"}\n",
                "codex",
                SESSION,
            );
            storage::write_snapshot(repo.root(), &original_log, &original_log).unwrap();
            std::fs::write(repo.root().join("AGENTS.md"), "Shared instructions\n").unwrap();
            repo.add_all().unwrap();
            repo.commit("landing target fixture").unwrap();
            let origin = repo.git(&["rev-parse", "HEAD"]).unwrap();
            repo.git(&["update-ref", "refs/heads/work", &origin])
                .unwrap();
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/work"])
                .unwrap();
            plumbing::import_commit_graph(&source, &repo, &origin).unwrap();
            let source_event = transcript::wrap_lines(
                "{\"type\":\"user\",\"text\":\"selected source event\"}\n",
                "codex",
                SESSION,
            );
            metadata.turn = Some(2);
            let log = format!("{original_log}{source_event}");
            let mut files = storage::snapshot_files(&log, &log).unwrap();
            files.insert(
                meta::FILE.into(),
                meta::to_text(&metadata).unwrap().into_bytes(),
            );
            let tree = plumbing::tree_apply_owned(
                &source,
                &origin,
                files
                    .into_iter()
                    .map(|(path, bytes)| (path, Some(bytes)))
                    .collect(),
            )
            .unwrap();
            let source_head =
                plumbing::commit_tree(&source, &tree, &[&origin], "landing source fixture")
                    .unwrap();
            source
                .git(&["update-ref", "refs/heads/source", &source_head])
                .unwrap();
            let native = directory.path().join("native.jsonl");
            std::fs::write(&native, baseline).unwrap();
            let binding = ExplorationBinding {
                role: MergeArchiveRole {
                    generation: uuid::Uuid::now_v7().to_string(),
                    slug: "alice/target".into(),
                    branch: "work".into(),
                    origin_head: origin.clone(),
                    logical_session: SESSION.into(),
                },
                native: RuntimeLinkKey {
                    runtime: runtime.into(),
                    session_id: "INSTALLED".into(),
                },
                installed: Frontier {
                    bytes: baseline.len() as u64,
                    sha256: hex::encode(Sha256::digest(baseline)),
                },
                source: FrozenMergeSource {
                    reference: "alice/source@source".into(),
                    slug: "alice/source".into(),
                    branch: Some("source".into()),
                    head: source_head,
                    base: Some(origin.clone()),
                },
            };
            let mut installed = Link::new(runtime, "INSTALLED", Some(directory.path()));
            installed.owner = Some("alice".into());
            installed.agent = Some("target".into());
            installed.branch = Some("work".into());
            installed.materialized_from = Some(origin);
            installed.baseline_bytes = Some(binding.installed.bytes);
            installed.baseline_hash = Some(binding.installed.sha256.clone());
            let store = Store::at(directory.path().join("store"));
            let mut tx = mergetx::checked_activation_image(
                &merge_archive::test_activation(&binding, "{}".into()).transaction_original_json,
            )
            .unwrap();
            tx.picked = vec![format!("{}#2.1", binding.source.reference)];
            tx.summary = Some("Retain the selected source decision".into());
            let json = format!(
                "{},\"future\":9007199254740993.0000000000000001}}\n",
                serde_json::to_string(&tx)
                    .unwrap()
                    .strip_suffix('}')
                    .unwrap()
            );
            mergetx::create(repo.root(), &tx).unwrap();
            let tx_path = crate::domain::repo::common_git_dir(repo.root()).join(mergetx::LOCK_FILE);
            merge_archive::durable_publish_transition_bytes(&tx_path, json.as_bytes(), true)
                .unwrap();
            super::super::activation::activate_with(
                super::super::activation::ActivationRequest {
                    repo: &repo,
                    source_repo: &source,
                    store: &store,
                    binding: &binding,
                    installed: &installed,
                    transaction_json: &json,
                    previous_claims: &[],
                },
                |_| Ok(baseline.to_vec()),
                |_| Ok(()),
            )
            .unwrap();
            let dictionary = RepositoryDictionary::new(
                repo.git_path("fixture-dictionary.json").unwrap(),
                MemoryKeys::default(),
            );
            Self {
                _directory: directory,
                repo,
                source,
                store,
                binding,
                native,
                dictionary,
                original_log,
                source_event,
            }
        }

        fn request(&self) -> LandingRequest<'_> {
            LandingRequest {
                repo: &self.repo,
                source_repo: &self.source,
                store: &self.store,
                binding: &self.binding,
            }
        }
        fn run(&self) -> Result<LandingOutcome> {
            self.at(|_| Ok(()))
        }
        fn at(&self, checkpoint: impl FnMut(Checkpoint) -> Result<()>) -> Result<LandingOutcome> {
            land_with(
                self.request(),
                &self.dictionary,
                &Matcher::empty(),
                |_| Ok(std::fs::read(&self.native)?),
                checkpoint,
            )
        }
        fn append(&self, bytes: &[u8]) {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.native)
                .unwrap()
                .write_all(bytes)
                .unwrap();
        }
        fn tx_path(&self) -> PathBuf {
            crate::domain::repo::common_git_dir(self.repo.root()).join(mergetx::LOCK_FILE)
        }
        fn retired_path(&self) -> PathBuf {
            self.tx_path()
                .with_extension(format!("landed-{}.json", self.binding.role.generation))
        }
        fn journal(&self) -> ArchiveJournal {
            merge_archive::read(self.repo.root(), &self.binding.role.generation)
                .unwrap()
                .unwrap()
        }
        fn head(&self) -> String {
            current_head(&self.repo, "work").unwrap()
        }
        fn stop(&self, stage: Checkpoint) {
            assert!(
                self.at(|point| {
                    if point == stage {
                        anyhow::bail!("injected landing stop")
                    }
                    Ok(())
                })
                .is_err()
            );
        }
    }

    #[test]
    fn opencode_landing_and_final_tail_share_the_terminal_frontier() {
        use super::super::{TailDestination, TailOutcome, settle_tail_mode};
        for aborted in [false, true] {
            let baseline = b"{\"id\":\"INSTALLED\",\"kind\":\"opencode.meta\"}\n";
            let fixture = Fixture::with_native("opencode", baseline);
            let completed = concat!(
                "{\"id\":\"done\",\"kind\":\"message\",\"session_id\":\"INSTALLED\",\"data\":{\"role\":\"assistant\",\"finish\":\"stop\",\"time\":{\"created\":1,\"completed\":2}}}\n",
                "{\"id\":\"text\",\"kind\":\"part\",\"session_id\":\"INSTALLED\",\"message_id\":\"done\",\"data\":{\"type\":\"text\",\"text\":\"stable exploration\"}}\n"
            );
            let mut message = serde_json::json!({"id":"active", "kind":"message", "session_id":"INSTALLED", "data":{"role":"assistant", "time":{"created":3}, "finish":"tool-calls"}});
            let running_tool = "{\"id\":\"tool\",\"kind\":\"part\",\"session_id\":\"INSTALLED\",\"message_id\":\"active\",\"data\":{\"type\":\"tool\",\"state\":{\"status\":\"running\",\"output\":\"partial tool output\"}}}\n";
            fixture.append(completed.as_bytes());
            let initial_tool = if aborted {
                running_tool.to_owned()
            } else {
                running_tool.replace("running", "completed")
            };
            fixture.append(format!("{message}\n{initial_tool}").as_bytes());
            let link = selected_link(&fixture.request()).unwrap().json;
            let landed = fixture.run().unwrap();
            assert_eq!(landed.archived_records, 2);
            let cursor = fixture.journal().consumed;
            assert_eq!(cursor.bytes, (baseline.len() + completed.len()) as u64);
            let view =
                storage::materialize_at(fixture.repo.root(), &landed.commit, meta::VIEW_FILE)
                    .unwrap();
            let log = storage::materialize_at(fixture.repo.root(), &landed.commit, meta::LOG_FILE)
                .unwrap();
            assert!(log.contains("stable exploration"));
            assert!(!log.contains("partial tool output"));
            assert!(!view.contains("stable exploration"));
            let settle = || {
                settle_tail_mode(
                    TailDestination {
                        repo: &fixture.repo,
                        store: &fixture.store,
                        role: &fixture.binding.role,
                        native: &fixture.binding.native,
                    },
                    &fixture.dictionary,
                    &Matcher::empty(),
                    |_| Ok(std::fs::read(&fixture.native)?),
                    |_| Ok(()),
                    Some(&fixture.binding),
                )
            };
            let assert_held = || {
                let before = std::fs::read(&fixture.native).unwrap();
                let error = settle().unwrap_err();
                assert!(format!("{error:#}").contains("incomplete native record"));
                assert_eq!(fixture.head(), landed.commit);
                assert_eq!(fixture.journal().consumed, cursor);
                assert_eq!(selected_link(&fixture.request()).unwrap().json, link);
                assert_eq!(std::fs::read(&fixture.native).unwrap(), before);
            };
            assert_held();
            let terminal_tool = running_tool
                .replace("running", if aborted { "error" } else { "completed" })
                .replace("partial tool output", "final tool output");
            if aborted {
                message["data"].as_object_mut().unwrap().remove("finish");
                message["data"]["error"] = serde_json::json!({"name":"MessageAbortedError"});
            }
            let write_native = |message: &serde_json::Value, patch: &str| {
                let bytes = [
                    baseline.as_slice(),
                    completed.as_bytes(),
                    format!("{message}\n{terminal_tool}{patch}").as_bytes(),
                ]
                .concat();
                std::fs::write(&fixture.native, &bytes).unwrap();
                bytes
            };
            write_native(&message, "");
            assert_held();
            let patch = "{\"id\":\"patch\",\"kind\":\"part\",\"session_id\":\"INSTALLED\",\"message_id\":\"active\",\"data\":{\"type\":\"patch\",\"hash\":\"synthetic-patch\",\"files\":[\"synthetic.txt\"]}}\n";
            write_native(&message, patch);
            assert_held();
            message["data"]["time"]["completed"] = 4.into();
            let native = write_native(&message, patch);
            let TailOutcome::Published { commit, records: 3 } = settle().unwrap() else {
                panic!("terminal OpenCode tail was not archived");
            };
            let log =
                storage::materialize_at(fixture.repo.root(), &commit, meta::LOG_FILE).unwrap();
            assert_eq!(log.matches("stable exploration").count(), 1);
            assert_eq!(log.matches("final tool output").count(), 1);
            assert_eq!(log.matches("synthetic-patch").count(), 1);
            assert!(!log.contains("partial tool output"));
            assert_eq!(
                storage::materialize_at(fixture.repo.root(), &commit, meta::VIEW_FILE).unwrap(),
                view
            );
            assert_eq!(fixture.journal().consumed.bytes, native.len() as u64);
            assert_eq!(std::fs::read(&fixture.native).unwrap(), native);
            assert_eq!(selected_link(&fixture.request()).unwrap().json, link);
            assert_eq!(settle().unwrap(), TailOutcome::Noop { pending_bytes: 0 });
            std::fs::write(
                &fixture.native,
                String::from_utf8(native.clone())
                    .unwrap()
                    .replace("\"role\":\"assistant\"", "\"role\":\"user\""),
            )
            .unwrap();
            assert!(
                format!("{:#}", settle().unwrap_err()).contains("identity was reused or changed")
            );
            assert_eq!(fixture.head(), commit);
        }
    }

    #[test]
    fn opencode_summary_revisions_survive_git_cas_stops_without_rebuilding_observations() {
        use super::super::{
            Checkpoint as TailCheckpoint, TailDestination, TailOutcome, settle_tail_mode,
        };
        for (stop, ref_lock) in [
            (TailCheckpoint::Prepared, false),
            (TailCheckpoint::BeforeCas, false),
            (TailCheckpoint::AfterCas, false),
            (TailCheckpoint::BeforeCas, true),
        ] {
            let baseline = b"{\"id\":\"INSTALLED\",\"kind\":\"opencode.meta\"}\n";
            let fixture = Fixture::with_native("opencode", baseline);
            let secret = "SYNTHETIC-ARCHIVE-PRIVATE-RECORDED-SECRET";
            let matcher = Matcher::for_test(&[("explicit", secret)]);
            let user = serde_json::json!({"id":"user","kind":"message","session_id":"INSTALLED","data":{"role":"user","summary":{"diffs":[]}}});
            let done = serde_json::json!({"id":"done","kind":"message","session_id":"INSTALLED","data":{"role":"assistant","time":{"completed":2}}});
            fixture.append(format!("{user}\n{done}\n").as_bytes());
            let landed = fixture.run().unwrap();
            let old_log =
                storage::materialize_at(fixture.repo.root(), &landed.commit, meta::LOG_FILE)
                    .unwrap();
            let view =
                storage::materialize_at(fixture.repo.root(), &landed.commit, meta::VIEW_FILE)
                    .unwrap();
            let before = fixture.journal();
            let link = selected_link(&fixture.request()).unwrap().json;
            let write_revision = |value: &str| {
                let mut changed = user.clone();
                changed["data"]["numeric"] = "native-number-token".into();
                changed["data"]["summary"]["diffs"] =
                    serde_json::json!([{"file":"owned.txt","after":value,"private":secret}]);
                let next = serde_json::json!({"id":"next","kind":"message","session_id":"INSTALLED","data":{"role":"assistant","time":{"completed":3}}});
                std::fs::write(
                    &fixture.native,
                    [
                        baseline.as_slice(),
                        format!("{changed}\n{done}\n{next}\n")
                            .replace("\"native-number-token\"", "1e0")
                            .as_bytes(),
                    ]
                    .concat(),
                )
                .unwrap();
            };
            write_revision("first summary");
            assert!(
                std::fs::read_to_string(&fixture.native)
                    .unwrap()
                    .contains("\"numeric\":1e0")
            );
            let result = settle_tail_mode(
                TailDestination {
                    repo: &fixture.repo,
                    store: &fixture.store,
                    role: &fixture.binding.role,
                    native: &fixture.binding.native,
                },
                &fixture.dictionary,
                &matcher,
                |_| Ok(std::fs::read(&fixture.native)?),
                |point| {
                    if point == stop {
                        if ref_lock {
                            std::fs::write(
                                fixture.repo.root().join(".git/refs/heads/work.lock"),
                                b"synthetic ref lock",
                            )?;
                        } else {
                            anyhow::bail!("injected observation publication stop");
                        }
                    }
                    Ok(())
                },
                None,
            );
            assert!(result.is_err());
            if ref_lock {
                assert_eq!(fixture.head(), landed.commit);
                std::fs::remove_file(fixture.repo.root().join(".git/refs/heads/work.lock"))
                    .unwrap();
            }
            let pending = fixture.journal();
            assert_eq!(pending.opencode, before.opencode);
            assert_eq!(pending.consumed, before.consumed);
            let publication = pending.publication.as_ref().unwrap();
            assert_eq!(publication.appended_records, 2);
            write_revision("later summary");
            let replay = settle_tail_mode(
                TailDestination {
                    repo: &fixture.repo,
                    store: &fixture.store,
                    role: &fixture.binding.role,
                    native: &fixture.binding.native,
                },
                &fixture.dictionary,
                &matcher,
                |_| panic!("retained observation replay must not read native"),
                |_| Ok(()),
                None,
            )
            .unwrap();
            assert_eq!(
                replay,
                TailOutcome::Published {
                    commit: publication.candidate.clone(),
                    records: 2
                }
            );
            let replayed = fixture.journal();
            assert_eq!(replayed.opencode, publication.next_opencode);
            assert_eq!(replayed.consumed, publication.next_frontier);
            let replay_log = storage::materialize_at(
                fixture.repo.root(),
                &publication.candidate,
                meta::LOG_FILE,
            )
            .unwrap();
            assert!(replay_log.starts_with(&old_log));
            assert!(replay_log.contains("first summary"));
            assert!(replay_log.contains("\"numeric\":1.0"));
            assert!(!replay_log.contains("\"numeric\":1e0"));
            assert!(!replay_log.contains(secret));
            assert!(!replay_log.contains("later summary"));
            let settle = || {
                settle_tail_mode(
                    TailDestination {
                        repo: &fixture.repo,
                        store: &fixture.store,
                        role: &fixture.binding.role,
                        native: &fixture.binding.native,
                    },
                    &fixture.dictionary,
                    &matcher,
                    |_| Ok(std::fs::read(&fixture.native)?),
                    |_| Ok(()),
                    Some(&fixture.binding),
                )
            };
            let TailOutcome::Published { commit, records: 1 } = settle().unwrap() else {
                panic!("new summary observation was not retained");
            };
            let log =
                storage::materialize_at(fixture.repo.root(), &commit, meta::LOG_FILE).unwrap();
            assert!(log.starts_with(&replay_log));
            assert_eq!(log.matches("first summary").count(), 1);
            assert_eq!(log.matches("later summary").count(), 1);
            assert_eq!(log.matches("\"numeric\":1.0").count(), 2);
            assert!(!log.contains("\"numeric\":1e0"));
            assert!(!log.contains(secret));
            assert_eq!(log.matches("opencode.archive.revision").count(), 2);
            assert_eq!(
                storage::materialize_at(fixture.repo.root(), &commit, meta::VIEW_FILE).unwrap(),
                view
            );
            assert_eq!(selected_link(&fixture.request()).unwrap().json, link);
            assert_eq!(settle().unwrap(), TailOutcome::Noop { pending_bytes: 0 });
            let accepted = fixture.journal();
            std::fs::write(&fixture.native, baseline).unwrap();
            assert!(format!("{:#}", settle().unwrap_err()).contains("observed rows were deleted"));
            assert_eq!(fixture.head(), commit);
            assert_eq!(fixture.journal(), accepted);
            assert_eq!(selected_link(&fixture.request()).unwrap().json, link);
        }
    }

    #[test]
    fn landing_preserves_ordinary_view_and_shared_tree_with_exact_exploration_occurrences() {
        let fixture = Fixture::new();
        fixture.append(EXPLORATION);
        fixture.append(EXPLORATION);
        fixture.append(b"{\"unfinished\":");
        std::fs::write(
            fixture.repo.root().join("AGENTS.md"),
            "Reconciled shared instructions\n",
        )
        .unwrap();
        std::fs::create_dir(fixture.repo.root().join("memory")).unwrap();
        std::fs::write(
            fixture.repo.root().join("memory/new.md"),
            "New shared memory\n",
        )
        .unwrap();
        let native = std::fs::read(&fixture.native).unwrap();
        let link = selected_link(&fixture.request()).unwrap().json;
        let tx = std::fs::read(fixture.tx_path()).unwrap();
        let outcome = fixture.run().unwrap();
        assert_eq!(outcome.archived_records, 2);
        let journal = fixture.journal();
        let ordinary = plumbing::commit_tree(
            &fixture.repo,
            &journal.landing.as_ref().unwrap().ordinary_tree,
            &[&fixture.binding.role.origin_head],
            "ordinary merge projection fixture",
        )
        .unwrap();
        let view =
            storage::materialize_at(fixture.repo.root(), &outcome.commit, meta::VIEW_FILE).unwrap();
        assert_eq!(
            view,
            storage::materialize_at(fixture.repo.root(), &ordinary, meta::VIEW_FILE).unwrap()
        );
        assert!(view.starts_with(&fixture.original_log));
        assert!(view.contains(&fixture.source_event));
        assert!(!view.contains("private exploration"));
        let log =
            storage::materialize_at(fixture.repo.root(), &outcome.commit, meta::LOG_FILE).unwrap();
        assert_eq!(log.matches("private exploration").count(), 2);
        assert!(!log.contains("unfinished"));
        assert_eq!(
            journal.consumed.bytes,
            (BASELINE.len() + EXPLORATION.len() * 2) as u64
        );
        assert_eq!(std::fs::read(&fixture.native).unwrap(), native);
        assert_eq!(selected_link(&fixture.request()).unwrap().json, link);
        assert_eq!(std::fs::read(fixture.retired_path()).unwrap(), tx);
        assert!(
            journal
                .landing
                .as_ref()
                .unwrap()
                .transaction_json
                .contains("9007199254740993.0000000000000001")
        );
        assert!(!fixture.tx_path().exists());
        assert_eq!(
            fixture
                .repo
                .git_bytes(&["show", &format!("{}:AGENTS.md", outcome.commit)])
                .unwrap(),
            b"Reconciled shared instructions\n"
        );
        assert_eq!(
            fixture
                .repo
                .git_bytes(&["show", &format!("{}:memory/new.md", outcome.commit)])
                .unwrap(),
            b"New shared memory\n"
        );
        assert_eq!(fixture.run().unwrap(), outcome);
    }

    #[test]
    fn empty_capture_lands_frozen_source_even_after_its_branch_advances() {
        let fixture = Fixture::new();
        let dictionary_path = fixture.repo.git_path("fixture-dictionary.json").unwrap();
        std::fs::write(&dictionary_path, "unreadable fixture dictionary").unwrap();
        let tree = fixture
            .source
            .git(&[
                "rev-parse",
                &format!("{}^{{tree}}", fixture.binding.source.head),
            ])
            .unwrap();
        let next = plumbing::commit_tree(
            &fixture.source,
            &tree,
            &[&fixture.binding.source.head],
            "later source movement",
        )
        .unwrap();
        fixture
            .source
            .git(&["update-ref", "refs/heads/source", &next])
            .unwrap();
        let result = fixture.run().unwrap();
        assert_eq!(
            std::fs::read_to_string(dictionary_path).unwrap(),
            "unreadable fixture dictionary"
        );
        assert_eq!(result.archived_records, 0);
        let journal = fixture.journal();
        assert_eq!(journal.consumed, fixture.binding.installed);
        assert_eq!(
            fixture
                .repo
                .git(&["rev-parse", &format!("{}^{{tree}}", result.commit)])
                .unwrap(),
            journal.landing.unwrap().ordinary_tree
        );
        let commit = fixture
            .repo
            .git(&["cat-file", "commit", &result.commit])
            .unwrap();
        let parents: Vec<_> = commit
            .lines()
            .filter_map(|line| line.strip_prefix("parent "))
            .collect();
        assert_eq!(
            parents,
            [
                fixture.binding.role.origin_head.as_str(),
                fixture.binding.source.head.as_str()
            ]
        );
        assert_ne!(parents[1], next);
    }

    #[test]
    fn every_publication_stop_replays_exact_candidate_without_native_or_worktree_rebuild() {
        for stop in [
            Checkpoint::Prepared,
            Checkpoint::BeforeCas,
            Checkpoint::AfterCas,
            Checkpoint::Landed,
            Checkpoint::BeforeRetire,
            Checkpoint::Retired,
        ] {
            let fixture = Fixture::new();
            fixture.append(EXPLORATION);
            fixture.stop(stop);
            let journal = fixture.journal();
            let candidate = journal
                .publication
                .as_ref()
                .map(|p| p.candidate.clone())
                .or_else(|| journal.accepted_commit.clone())
                .unwrap();
            let frontier = journal
                .publication
                .as_ref()
                .map(|p| p.next_frontier.clone())
                .unwrap_or(journal.consumed);
            fixture.append(b"{\"type\":\"assistant\",\"text\":\"later native evidence\"}\n");
            std::fs::write(
                fixture.repo.root().join("AGENTS.md"),
                "Later worktree change\n",
            )
            .unwrap();
            if matches!(stop, Checkpoint::Prepared | Checkpoint::BeforeCas) {
                assert!(
                    land_with(
                        fixture.request(),
                        &fixture.dictionary,
                        &Matcher::empty(),
                        |_| panic!("pending retry must not read native"),
                        |_| Ok(())
                    )
                    .is_err()
                );
                assert_eq!(fixture.head(), fixture.binding.role.origin_head);
                assert_eq!(
                    std::fs::read_to_string(fixture.repo.root().join("AGENTS.md")).unwrap(),
                    "Later worktree change\n"
                );
                std::fs::write(
                    fixture.repo.root().join("AGENTS.md"),
                    "Shared instructions\n",
                )
                .unwrap();
            }
            let result = land_with(
                fixture.request(),
                &fixture.dictionary,
                &Matcher::empty(),
                |_| panic!("retry must not read native"),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(result.commit, candidate, "{stop:?}");
            assert_eq!(result.archived_records, 1);
            assert_eq!(fixture.journal().consumed, frontier);
            assert!(
                !storage::materialize_at(fixture.repo.root(), &candidate, meta::LOG_FILE)
                    .unwrap()
                    .contains("later native evidence")
            );
            std::fs::remove_file(&fixture.native).unwrap();
            assert_eq!(fixture.run().unwrap(), result);
        }
    }

    #[test]
    fn cancellation_retains_an_unpublished_candidate_without_changing_git_or_native_bytes() {
        use super::super::abort::{AbortOutcome, AbortRequest, abort};
        for stop in [Checkpoint::Prepared, Checkpoint::BeforeCas] {
            let fixture = Fixture::new();
            fixture.append(EXPLORATION);
            fixture.stop(stop);
            let pending = fixture.journal();
            fixture
                .source
                .git(&["update-ref", "-d", "refs/heads/source"])
                .unwrap();
            let native = std::fs::read(&fixture.native).unwrap();
            let index = std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap();
            let worktree = std::fs::read(fixture.repo.root().join("AGENTS.md")).unwrap();
            let request = || AbortRequest {
                repo: &fixture.repo,
                store: &fixture.store,
                binding: &fixture.binding,
            };
            assert_eq!(abort(request()).unwrap(), AbortOutcome::Aborted);
            assert_eq!(abort(request()).unwrap(), AbortOutcome::Aborted);
            let aborted = fixture.journal();
            assert_eq!(
                aborted.abort.as_ref().unwrap().cancelled_publication,
                pending.publication
            );
            assert_eq!(aborted.landing, pending.landing);
            assert!(aborted.publication.is_none());
            assert_eq!(fixture.head(), fixture.binding.role.origin_head);
            assert_eq!(
                std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap(),
                index
            );
            assert_eq!(
                std::fs::read(fixture.repo.root().join("AGENTS.md")).unwrap(),
                worktree
            );
            assert_eq!(std::fs::read(&fixture.native).unwrap(), native);
            assert_eq!(
                fixture.repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap(),
                fixture
                    .repo
                    .git(&[
                        "rev-parse",
                        &format!("{}^{{tree}}", fixture.binding.role.origin_head)
                    ])
                    .unwrap()
            );
            assert!(
                mergetx::ControlGuard::acquire(fixture.repo.root())
                    .unwrap()
                    .write(
                        &mergetx::checked_activation_image(
                            &aborted.abort.unwrap().transaction_json
                        )
                        .unwrap()
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn cancellation_after_visible_cas_finishes_landing_and_never_rolls_back() {
        use super::super::abort::{AbortOutcome, AbortRequest, abort};
        for stop in [
            Checkpoint::AfterCas,
            Checkpoint::Landed,
            Checkpoint::BeforeRetire,
            Checkpoint::Retired,
        ] {
            let fixture = Fixture::new();
            fixture.append(EXPLORATION);
            fixture.stop(stop);
            let candidate = fixture.head();
            fixture
                .source
                .git(&["update-ref", "-d", "refs/heads/source"])
                .unwrap();
            std::fs::remove_file(&fixture.native).unwrap();
            let request = || AbortRequest {
                repo: &fixture.repo,
                store: &fixture.store,
                binding: &fixture.binding,
            };
            let expected = AbortOutcome::AlreadyLanded(LandingOutcome {
                commit: candidate.clone(),
                archived_records: 1,
            });
            assert_eq!(abort(request()).unwrap(), expected);
            assert_eq!(abort(request()).unwrap(), expected);
            assert_eq!(fixture.head(), candidate);
            assert_eq!(
                fixture.journal().phase,
                ArchivePhase::Landed {
                    merge_commit: candidate
                }
            );
            assert!(fixture.journal().abort.is_none());
            assert!(!fixture.tx_path().exists());
            assert!(fixture.retired_path().exists());
            assert!(!fixture.native.exists());
        }
    }

    #[test]
    fn cancellation_preserves_visible_candidate_when_transaction_or_link_changes() {
        use super::super::abort::{AbortRequest, abort};
        for changed_link in [false, true] {
            let fixture = Fixture::new();
            fixture.append(EXPLORATION);
            fixture.stop(Checkpoint::AfterCas);
            let path = if changed_link {
                link::link_path(&fixture.store, "codex", "INSTALLED")
            } else {
                fixture.tx_path()
            };
            let original = std::fs::read_to_string(&path).unwrap();
            std::fs::write(&path, format!("{original} \n")).unwrap();
            let before = std::fs::read(&path).unwrap();
            let journal = fixture.journal();
            let head = fixture.head();
            assert!(
                abort(AbortRequest {
                    repo: &fixture.repo,
                    store: &fixture.store,
                    binding: &fixture.binding
                })
                .is_err()
            );
            assert_eq!(std::fs::read(path).unwrap(), before);
            assert_eq!(fixture.journal(), journal);
            assert_eq!(fixture.head(), head);
        }
    }

    #[test]
    fn visible_completion_never_republishes_a_candidate_after_the_head_returns_to_expected_old() {
        let fixture = Fixture::new();
        fixture.stop(Checkpoint::AfterCas);
        let _branch =
            link::lock_branch(&fixture.store, &fixture.binding.role.slug, "work").unwrap();
        let _link = link::lock(&fixture.store, "codex", "INSTALLED").unwrap();
        let guard =
            ArchiveJournalGuard::acquire(fixture.repo.root(), &fixture.binding.role.generation)
                .unwrap();
        let control = mergetx::ControlGuard::acquire(fixture.repo.root()).unwrap();
        let journal = guard.read().unwrap().unwrap();
        fixture
            .repo
            .git(&[
                "update-ref",
                "refs/heads/work",
                &fixture.binding.role.origin_head,
            ])
            .unwrap();
        assert!(
            finish_visible(&fixture.request(), &guard, &control, &journal, &mut |_| Ok(
                ()
            ))
            .is_err()
        );
        assert_eq!(fixture.head(), fixture.binding.role.origin_head);
        assert_eq!(guard.read().unwrap(), Some(journal));
        assert!(fixture.tx_path().exists());
    }

    #[test]
    fn an_unheld_target_never_collects_the_other_checkout_shared_files() {
        let fixture = Fixture::new();
        fixture
            .repo
            .git(&["symbolic-ref", "HEAD", "refs/heads/main"])
            .unwrap();
        std::fs::write(
            fixture.repo.root().join("AGENTS.md"),
            "Another branch workspace\n",
        )
        .unwrap();
        let index = std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap();
        let outcome = fixture.run().unwrap();
        assert_eq!(
            fixture
                .repo
                .git_bytes(&["show", &format!("{}:AGENTS.md", outcome.commit)])
                .unwrap(),
            b"Shared instructions\n"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.repo.root().join("AGENTS.md")).unwrap(),
            "Another branch workspace\n"
        );
        assert_eq!(
            std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap(),
            index
        );
        assert!(fixture.journal().landing.unwrap().worktree_tree.is_none());
        assert_eq!(
            fixture.repo.git(&["symbolic-ref", "HEAD"]).unwrap(),
            "refs/heads/main"
        );
    }

    #[cfg(unix)]
    #[test]
    fn newline_shared_paths_are_frozen_and_pending_edits_are_not_overwritten() {
        let fixture = Fixture::new();
        let name = "memory/new\n\nshared.md";
        std::fs::create_dir(fixture.repo.root().join("memory")).unwrap();
        std::fs::write(fixture.repo.root().join(name), "Captured shared bytes\n").unwrap();
        fixture.stop(Checkpoint::Prepared);
        let candidate = fixture.journal().publication.unwrap().candidate;
        std::fs::write(fixture.repo.root().join(name), "Later shared bytes\n").unwrap();
        assert!(fixture.run().is_err());
        assert_eq!(
            std::fs::read_to_string(fixture.repo.root().join(name)).unwrap(),
            "Later shared bytes\n"
        );
        assert_eq!(fixture.head(), fixture.binding.role.origin_head);
        std::fs::write(fixture.repo.root().join(name), "Captured shared bytes\n").unwrap();
        assert_eq!(fixture.run().unwrap().commit, candidate);
        assert_eq!(
            fixture
                .repo
                .git_bytes(&["show", &format!("{candidate}:{name}")])
                .unwrap(),
            b"Captured shared bytes\n"
        );
    }

    #[test]
    fn shared_path_protocol_and_limits_refuse_truncation_or_unrepresentable_paths() {
        let mut paths = BTreeSet::new();
        add_shared_paths(b"memory/a\n\nb.md\0events/aa/skip\0", &mut paths).unwrap();
        assert_eq!(paths, BTreeSet::from(["memory/a\n\nb.md".into()]));
        let mut long_path = vec![b'x'; 4097];
        long_path.push(0);
        for bytes in [
            b"memory/a".to_vec(),
            b"../elsewhere\0".to_vec(),
            b"memory/\xff\0".to_vec(),
            long_path,
        ] {
            assert!(add_shared_paths(&bytes, &mut BTreeSet::new()).is_err());
        }
        assert!(
            add_shared_paths(
                &vec![0; MAX_SHARED_PATH_BYTES as usize + 1],
                &mut BTreeSet::new()
            )
            .is_err()
        );
    }

    #[test]
    fn protected_exploration_keeps_record_count_and_original_native_bytes() {
        let fixture = Fixture::new();
        fixture.append(b"{\"type\":\"assistant\",\"text\":\"secret violet river\"}\n");
        let bytes = std::fs::read(&fixture.native).unwrap();
        let outcome = land_with(
            fixture.request(),
            &fixture.dictionary,
            &Matcher::for_test(&[("private", "secret violet river")]),
            |_| Ok(std::fs::read(&fixture.native)?),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(outcome.archived_records, 1);
        let log =
            storage::materialize_at(fixture.repo.root(), &outcome.commit, meta::LOG_FILE).unwrap();
        assert!(!log.contains("secret violet river"));
        assert_eq!(std::fs::read(&fixture.native).unwrap(), bytes);
        let hydrated = fixture.dictionary.hydrate_jsonl(&log).unwrap();
        assert!(hydrated.text.contains("secret violet river"));
    }

    #[test]
    fn checkout_routing_changed_at_cas_cannot_publish_or_retire() {
        let fixture = Fixture::new();
        let foreign = tempfile::tempdir().unwrap();
        let tx = std::fs::read(fixture.tx_path()).unwrap();
        assert!(
            fixture
                .at(|point| {
                    if point == Checkpoint::BeforeCas {
                        fixture.repo.git(&[
                            "config",
                            "core.worktree",
                            foreign.path().to_str().unwrap(),
                        ])?;
                    }
                    Ok(())
                })
                .is_err()
        );
        fixture
            .repo
            .git(&["config", "--unset", "core.worktree"])
            .unwrap();
        assert_eq!(fixture.head(), fixture.binding.role.origin_head);
        assert_eq!(std::fs::read(fixture.tx_path()).unwrap(), tx);
        assert!(fixture.journal().publication.is_some());
        assert_eq!(std::fs::read_dir(foreign.path()).unwrap().count(), 0);
    }

    #[test]
    fn pending_landing_freezes_generic_progress_and_changed_transaction_refuses_cas() {
        let fixture = Fixture::new();
        fixture.append(EXPLORATION);
        fixture.stop(Checkpoint::Prepared);
        let control = mergetx::ControlGuard::acquire(fixture.repo.root()).unwrap();
        let mut tx = control.read().unwrap().unwrap();
        tx.set_summary("changed progress".into());
        assert!(control.write(&tx).is_err());
        drop(control);
        let changed = serde_json::to_vec(&tx).unwrap();
        std::fs::write(fixture.tx_path(), &changed).unwrap();
        assert!(fixture.run().is_err());
        assert_eq!(fixture.head(), fixture.binding.role.origin_head);
        assert_eq!(std::fs::read(fixture.tx_path()).unwrap(), changed);
        assert!(fixture.journal().publication.is_some());
    }

    #[test]
    fn landed_cleanup_preserves_replacement_generation_and_never_infers_success_from_absence() {
        let fixture = Fixture::new();
        fixture.stop(Checkpoint::Landed);
        let expected = std::fs::read(fixture.tx_path()).unwrap();
        let mut tx: mergetx::Tx = serde_json::from_slice(&expected).unwrap();
        tx.generation = Some(uuid::Uuid::now_v7().to_string());
        tx.exploration = None;
        let replacement = serde_json::to_vec(&tx).unwrap();
        std::fs::write(fixture.tx_path(), &replacement).unwrap();
        assert!(fixture.run().is_err());
        assert_eq!(std::fs::read(fixture.tx_path()).unwrap(), replacement);
        std::fs::remove_file(fixture.tx_path()).unwrap();
        assert!(fixture.run().is_err());
        crate::domain::merge_archive::durable_publish_transition_bytes(
            &fixture.tx_path(),
            &expected,
            false,
        )
        .unwrap();
        fixture.run().unwrap();
        assert!(fixture.retired_path().exists());
    }

    #[test]
    fn invalid_native_baseline_or_complete_record_leaves_open_authority_and_ref_unchanged() {
        for bytes in [
            b"{\"invalid\":true}\n".as_slice(),
            [BASELINE, b"not-json\n"].concat().as_slice(),
        ] {
            let fixture = Fixture::new();
            let tx = std::fs::read(fixture.tx_path()).unwrap();
            std::fs::write(&fixture.native, bytes).unwrap();
            assert!(fixture.run().is_err());
            assert_eq!(fixture.head(), fixture.binding.role.origin_head);
            assert_eq!(std::fs::read(fixture.tx_path()).unwrap(), tx);
            assert_eq!(fixture.journal().phase, ArchivePhase::Open);
            assert!(fixture.journal().publication.is_none());
        }
    }

    #[test]
    fn ordinary_tree_proof_rejects_view_metadata_event_and_parent_tampering() {
        let fixture = Fixture::new();
        fixture.append(EXPLORATION);
        fixture.stop(Checkpoint::Prepared);
        let journal = fixture.journal();
        let publication = journal.publication.as_ref().unwrap();
        let landing = journal.landing.as_ref().unwrap();
        let inherited = storage::event_id(&fixture.original_log).unwrap();
        for edit in [
            (meta::VIEW_FILE.to_owned(), b"".to_vec()),
            (meta::FILE.to_owned(), b"{}\n".to_vec()),
            (meta::event_path(&inherited).unwrap(), b"{}\n".to_vec()),
            ("AGENTS.md".to_owned(), b"tampered shared text".to_vec()),
        ] {
            let tree = plumbing::tree_apply_owned(
                &fixture.repo,
                &publication.candidate_tree,
                vec![(edit.0, Some(edit.1))],
            )
            .unwrap();
            let candidate = plumbing::commit_tree(
                &fixture.repo,
                &tree,
                &[&publication.expected_old, &fixture.binding.source.head],
                "invalid landing candidate",
            )
            .unwrap();
            assert!(
                archive_history::verify_merge_landing(
                    &fixture.repo,
                    &publication.expected_old,
                    &fixture.binding.source.head,
                    &landing.ordinary_tree,
                    &candidate,
                    &tree
                )
                .is_err()
            );
        }
        let wrong = plumbing::commit_tree(
            &fixture.repo,
            &publication.candidate_tree,
            &[&fixture.binding.source.head, &publication.expected_old],
            "wrong parent order",
        )
        .unwrap();
        assert!(
            archive_history::verify_merge_landing(
                &fixture.repo,
                &publication.expected_old,
                &fixture.binding.source.head,
                &landing.ordinary_tree,
                &wrong,
                &publication.candidate_tree
            )
            .is_err()
        );
    }

    #[test]
    fn before_cas_target_or_link_changes_preserve_prepared_evidence() {
        for change_link in [false, true] {
            let fixture = Fixture::new();
            fixture.append(EXPLORATION);
            let mut replacement = None;
            assert!(
                fixture
                    .at(|point| {
                        if point == Checkpoint::BeforeCas {
                            if change_link {
                                let selected = selected_link(&fixture.request())?;
                                let mut image = selected.link;
                                image.naming_ignored = true;
                                let bytes = image.to_json()?;
                                std::fs::write(
                                    link::link_path(&fixture.store, "codex", "INSTALLED"),
                                    bytes,
                                )?;
                            } else {
                                let tree = fixture.repo.git(&["rev-parse", "HEAD^{tree}"])?;
                                let head = plumbing::commit_tree(
                                    &fixture.repo,
                                    &tree,
                                    &[&fixture.binding.role.origin_head],
                                    "competing head",
                                )?;
                                fixture
                                    .repo
                                    .git(&["update-ref", "refs/heads/work", &head])?;
                                replacement = Some(head);
                            }
                        }
                        Ok(())
                    })
                    .is_err()
            );
            assert_eq!(
                fixture.head(),
                replacement.unwrap_or_else(|| fixture.binding.role.origin_head.clone())
            );
            assert_eq!(fixture.journal().consumed, fixture.binding.installed);
            assert!(fixture.journal().publication.is_some());
        }
    }

    #[test]
    fn graph_overrides_refuse_only_first_construction_and_pending_replay_uses_objects() {
        for target in [false, true] {
            for shallow in [false, true] {
                let fixture = Fixture::new();
                let repo = if target {
                    &fixture.repo
                } else {
                    &fixture.source
                };
                let head = if target {
                    &fixture.binding.role.origin_head
                } else {
                    &fixture.binding.source.head
                };
                let path = repo
                    .git_path(if shallow { "shallow" } else { "info/grafts" })
                    .unwrap();
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, format!("{head}\n")).unwrap();
                assert!(fixture.run().is_err());
                assert_eq!(fixture.head(), fixture.binding.role.origin_head);
                assert!(fixture.journal().publication.is_none());
                std::fs::remove_file(&path).unwrap();
                fixture.stop(Checkpoint::Prepared);
                std::fs::write(&path, format!("{head}\n")).unwrap();
                fixture.run().unwrap();
            }
        }
    }

    /// Native capture cannot change the meaning of a pick while immutable heads stay equal.
    #[test]
    fn native_read_graph_changes_keep_frozen_source_coordinates() {
        for same_repo in [false, true] {
            for shallow in [false, true] {
                let fixture = Fixture::new();
                let source = if same_repo {
                    plumbing::import_commit_graph(
                        &fixture.repo,
                        &fixture.source,
                        &fixture.binding.source.head,
                    )
                    .unwrap();
                    &fixture.repo
                } else {
                    &fixture.source
                };
                let path = source
                    .git_path(if shallow { "shallow" } else { "info/grafts" })
                    .unwrap();
                let native = std::fs::read(&fixture.native).unwrap();
                let original_commit = source
                    .git_bytes(&["cat-file", "commit", &fixture.binding.source.head])
                    .unwrap();
                let link = selected_link(&fixture.request()).unwrap().json;
                let transaction = std::fs::read(fixture.tx_path()).unwrap();
                let source_refs = source.git(&["show-ref"]).unwrap();
                let outcome = land_with(
                    LandingRequest {
                        source_repo: source,
                        ..fixture.request()
                    },
                    &fixture.dictionary,
                    &Matcher::empty(),
                    |_| {
                        std::fs::create_dir_all(path.parent().unwrap())?;
                        std::fs::write(&path, format!("{}\n", fixture.binding.source.head))?;
                        // The injected graph rewrite reproduces the mutable traversal's cutoff.
                        assert_eq!(
                            source
                                .git(&[
                                    "rev-list",
                                    "--parents",
                                    "-n",
                                    "1",
                                    &fixture.binding.source.head
                                ])?
                                .split_whitespace()
                                .count(),
                            1
                        );
                        Ok(native.clone())
                    },
                    |_| Ok(()),
                )
                .unwrap();
                assert_eq!(
                    source
                        .git_bytes(&["cat-file", "commit", &fixture.binding.source.head])
                        .unwrap(),
                    original_commit
                );
                assert_eq!(std::fs::read(&fixture.native).unwrap(), native);
                assert_eq!(selected_link(&fixture.request()).unwrap().json, link);
                assert_eq!(std::fs::read(fixture.retired_path()).unwrap(), transaction);
                let view =
                    storage::materialize_at(fixture.repo.root(), &outcome.commit, meta::VIEW_FILE)
                        .unwrap();
                assert_eq!(
                    view.matches(fixture.original_log.as_str()).count(),
                    1,
                    "{view}"
                );
                assert_eq!(
                    view.matches(fixture.source_event.as_str()).count(),
                    1,
                    "{view}"
                );
                assert_eq!(fixture.head(), outcome.commit);
                assert!(matches!(
                    fixture.journal().phase,
                    ArchivePhase::Landed { .. }
                ));
                if !same_repo {
                    assert_eq!(source.git(&["show-ref"]).unwrap(), source_refs);
                }
                std::fs::remove_file(path).unwrap();
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn promisor_effective_config_helper() {
        let Some(mode) = std::env::var_os("AGIT_LANDING_PROMISOR_MODE") else {
            return;
        };
        let fixture = Fixture::new();
        if mode == "local" {
            fixture
                .source
                .git(&["config", "remote.probe.url", "probe::fixture"])
                .unwrap();
            fixture
                .source
                .git(&["config", "remote.probe.promisor", "true"])
                .unwrap();
        }
        let event_path =
            meta::event_path(&storage::event_id(&fixture.source_event).unwrap()).unwrap();
        let oid = fixture
            .source
            .git(&[
                "rev-parse",
                &format!("{}:{event_path}", fixture.binding.source.head),
            ])
            .unwrap();
        let path = fixture
            .source
            .git_path(&format!("objects/{}/{}", &oid[..2], &oid[2..]))
            .unwrap();
        assert!(path.is_file());
        std::fs::remove_file(path).unwrap();
        let marker = PathBuf::from(std::env::var_os("AGIT_LANDING_PROMISOR_MARKER").unwrap());
        if marker.exists() {
            std::fs::remove_file(&marker).unwrap();
        }
        let control = std::process::Command::new("git")
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(fixture.source.root())
            .args(["cat-file", "-p", &oid])
            .env("GIT_NO_LAZY_FETCH", "0")
            .env("GIT_ALLOW_PROTOCOL", "probe")
            .output()
            .unwrap();
        assert!(!control.status.success());
        assert!(
            marker.is_file(),
            "ordinary missing-object read must reach the isolated remote helper"
        );
        std::fs::remove_file(&marker).unwrap();
        let tx = std::fs::read(fixture.tx_path()).unwrap();
        let error = fixture.run().unwrap_err();
        assert!(format!("{error:#}").contains("promisor"));
        assert!(!marker.exists());
        assert_eq!(fixture.head(), fixture.binding.role.origin_head);
        assert_eq!(std::fs::read(fixture.tx_path()).unwrap(), tx);
        assert!(fixture.journal().publication.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn local_and_global_included_promisor_config_refuse_before_any_remote_helper() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("git-remote-probe");
        std::fs::write(
            &helper,
            "#!/bin/sh\nprintf called > \"$AGIT_LANDING_PROMISOR_MARKER\"\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let included = directory.path().join("included.gitconfig");
        std::fs::write(
            &included,
            "[remote \"probe\"]\nurl = probe::fixture\npromisor = true\n",
        )
        .unwrap();
        let global = directory.path().join("global.gitconfig");
        let empty = directory.path().join("empty.gitconfig");
        std::fs::write(&empty, "").unwrap();
        let status = std::process::Command::new("git")
            .args(["config", "--file"])
            .arg(&global)
            .arg("include.path")
            .arg(&included)
            .status()
            .unwrap();
        assert!(status.success());
        let marker = directory.path().join("remote-helper-called");
        let mut paths = vec![directory.path().to_path_buf()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let paths = std::env::join_paths(paths).unwrap();
        for mode in ["local", "global"] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "commands::merge::archive::landing::tests::promisor_effective_config_helper",
                    "--nocapture",
                ])
                .env("AGIT_LANDING_PROMISOR_MODE", mode)
                .env("AGIT_LANDING_PROMISOR_MARKER", &marker)
                .env("PATH", &paths)
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if mode == "global" { &global } else { &empty },
                )
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{mode}: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(!marker.exists());
        }
    }
}
