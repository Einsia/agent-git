//! Lands an activated merge while retaining its exact publication and exploration endpoints.
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
    land_resolved(request, global, &[])
}

pub(super) fn land_resolved(
    request: LandingRequest<'_>,
    global: &Matcher,
    resolved: &[String],
) -> Result<LandingOutcome> {
    require_destination_routing(request.repo, &request.binding.role.branch)?;
    let dictionary = RepositoryDictionary::open(request.repo.root())?;
    land_with_resolved(
        request,
        &dictionary,
        global,
        read_native,
        |_| Ok(()),
        resolved,
    )
}

/// Replay uses the retained candidate before opening a source repository or a secret vault.
/// A fresh Open exploration returns None so its caller can prepare the frozen source inputs.
pub(super) fn replay(
    repo: &Repo,
    store: &Store,
    binding: &ExplorationBinding,
) -> Result<Option<LandingOutcome>> {
    require_destination_routing(repo, &binding.role.branch)?;
    let _branches = super::lock_binding_branches(store, binding)?;
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
        target_is_checked_out(request.repo, request.binding.target_branch())?
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

fn land_with_resolved<K: KeyStore>(
    request: LandingRequest<'_>,
    dictionary: &RepositoryDictionary<K>,
    global: &Matcher,
    read_native: impl FnOnce(&Link) -> Result<Vec<u8>>,
    mut checkpoint: impl FnMut(Checkpoint) -> Result<()>,
    resolved: &[String],
) -> Result<LandingOutcome> {
    let binding = request.binding;
    let role = &binding.role;
    role.validate(role.origin_head.len())?;
    binding.native.validate()?;
    require_destination_routing(request.repo, &role.branch)?;
    let _branches = super::lock_binding_branches(request.store, binding)?;
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
    // A retained candidate owns its resolution; new acknowledgements cannot alter or replay it.
    ensure!(
        resolved.is_empty()
            || (binding.file_target.is_some()
                && journal.phase == ArchivePhase::Open
                && journal.publication.is_none()
                && journal.landing.is_none()),
        "--resolved requires a fresh Open file merge; replay retained publication without --resolved"
    );
    if !resolved.is_empty() {
        ensure!(
            target_is_checked_out(request.repo, binding.target_branch())?,
            "--resolved requires the target branch checkout"
        );
    }
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
    if let Some(target) = &binding.file_target {
        require_destination_routing(request.repo, &target.branch)?;
        super::file_agent::require_seed_binding(request.repo, binding)?;
        ensure!(
            current_head(request.repo, &target.branch)? == target.head,
            "file merge target moved before landing"
        );
        ensure!(
            !target_is_checked_out(request.repo, &role.branch)?,
            "file exploration branch must not be checked out while the merge is open"
        );
    }
    let source_log = if binding.file_target.is_none() {
        Some(archive_history::freeze_source_log(
            request.source_repo,
            &binding.source.head,
        )?)
    } else {
        None
    };

    super::file_agent::verify_fresh_launch_evidence(request.repo, binding, &selected.link)?;
    let native = read_native(&selected.link)?;
    ensure!(
        native.len() <= storage::MAX_MATERIALIZED_BYTES,
        "merge native snapshot exceeds its byte limit"
    );
    let capture = super::capture_selected(&native, &journal)?;
    let prepared = if binding.file_target.is_none() {
        Some(
            merge::prepare_session_merge(
                request.repo,
                request.source_repo,
                &tx,
                &binding.source.head,
                source_log.as_ref(),
            )?
            .context("ordinary merge selection cannot be landed")?,
        )
    } else {
        None
    };
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
    let on_target = target_is_checked_out(request.repo, binding.target_branch())?;
    let shared = if on_target {
        shared_paths(request.repo, binding.target_head())?
    } else {
        Vec::new()
    };
    let base = plumbing::tree_overlay_worktree(request.repo, binding.target_head(), &shared)?;
    let (ordinary_tree, file_commit, candidate, tree) = if binding.file_target.is_some() {
        let (ordinary_tree, message) = merge::merge_tree(
            request.repo,
            request.source_repo,
            &tx,
            &binding.source.head,
            &shared,
            true,
            resolved,
        )?
        .context("file merge selection cannot be landed")?;
        let main = plumbing::commit_tree(
            request.repo,
            &ordinary_tree,
            &[binding.target_head(), &binding.source.head],
            &message,
        )?;
        let (evidence, tree) = if capture.record_count == 0 {
            let tree = request.repo.git(&[
                "rev-parse",
                "--verify",
                &format!("{}^{{tree}}", role.origin_head),
            ])?;
            let commit = plumbing::commit_tree(
                request.repo,
                &tree,
                &[&role.origin_head],
                "Complete file merge without native evidence",
            )?;
            (commit, tree)
        } else {
            super::build_candidate(
                request.repo,
                &role.origin_head,
                role,
                &envelopes,
                capture.record_count,
            )?
        };
        (ordinary_tree, Some(main), evidence, tree)
    } else {
        let (ordinary_tree, message) =
            merge::land_session_merge(request.repo, &tx, prepared.unwrap(), &base)?;
        let tree = append_log(request.repo, &ordinary_tree, &envelopes)?;
        let candidate = plumbing::commit_tree(
            request.repo,
            &tree,
            &[&role.origin_head, &binding.source.head],
            &message,
        )?;
        (ordinary_tree, None, candidate, tree)
    };
    let mut pending = journal.clone();
    pending.landing = Some(RetainedMergeLanding {
        file_evidence: file_commit.as_ref().map(|_| candidate.clone()),
        file_commit,
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
    let suffix = if journal.binding.file_target.is_some() {
        ensure!(
            landing.file_evidence.as_ref() == Some(&publication.candidate),
            "file evidence candidate changed"
        );
        ensure!(
            repo.git(&[
                "rev-parse",
                "--verify",
                &format!("{}^{{tree}}", publication.candidate)
            ])? == publication.candidate_tree,
            "file evidence tree differs from its publication receipt"
        );
        verify_file_result(repo, journal)?
    } else {
        archive_history::verify_merge_landing(
            repo,
            &publication.expected_old,
            &journal.binding.source.head,
            &landing.ordinary_tree,
            &publication.candidate,
            &publication.candidate_tree,
        )?
    };
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

fn verify_file_result(repo: &Repo, journal: &ArchiveJournal) -> Result<String> {
    let binding = &journal.binding;
    let seed = super::file_agent::require_seed_binding(repo, binding)?
        .context("file landing seed is missing")?;
    let landing = journal
        .landing
        .as_ref()
        .context("file landing result is missing")?;
    let main = landing
        .file_commit
        .as_deref()
        .context("file merge candidate is missing")?;
    let evidence = landing
        .file_evidence
        .as_deref()
        .context("file evidence candidate is missing")?;
    archive_history::verify_file_merge_landing(
        repo,
        binding.target_head(),
        &binding.source.head,
        main,
        &landing.ordinary_tree,
    )?;
    let log = storage::materialize_at(repo.root(), evidence, meta::LOG_FILE)?;
    if log.is_empty() {
        let tree = repo.git(&["rev-parse", "--verify", &format!("{evidence}^{{tree}}")])?;
        ensure!(
            tree == seed.tree,
            "empty file evidence checkpoint changes its seed tree"
        );
        archive_history::verify_empty_file_exploration(repo, &seed.role.origin_head, evidence)?;
    } else {
        archive_history::verify_edge(repo, &seed.role.origin_head, evidence)?;
    }
    Ok(log)
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
    if request.binding.file_target.is_some() {
        publish_file_pair(request, control, journal, checkpoint)?;
        return finish_visible(request, guard, control, journal, checkpoint);
    }
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

fn publish_file_pair(
    request: &LandingRequest<'_>,
    control: &mergetx::ControlGuard,
    journal: &ArchiveJournal,
    checkpoint: &mut impl FnMut(Checkpoint) -> Result<()>,
) -> Result<()> {
    let binding = request.binding;
    let target = binding
        .file_target
        .as_ref()
        .context("file merge target is missing")?;
    let publication = journal
        .publication
        .as_ref()
        .context("file evidence publication is missing")?;
    let landing = journal
        .landing
        .as_ref()
        .context("file merge result is missing")?;
    let main = landing
        .file_commit
        .as_deref()
        .context("file merge candidate is missing")?;
    require_destination_routing(request.repo, &target.branch)?;
    require_destination_routing(request.repo, &binding.role.branch)?;
    ensure!(
        !target_is_checked_out(request.repo, &binding.role.branch)?,
        "file exploration branch must not be checked out during dual publication"
    );
    let actual_main = current_head(request.repo, &target.branch)?;
    let actual_evidence = current_head(request.repo, &binding.role.branch)?;
    if actual_main == main && actual_evidence == publication.candidate {
        return Ok(());
    }
    ensure!(
        actual_main == target.head && actual_evidence == publication.expected_old,
        "file merge refs are outside their retained atomic publication endpoints"
    );
    checkpoint(Checkpoint::BeforeCas)?;
    require_destination_routing(request.repo, &target.branch)?;
    require_destination_routing(request.repo, &binding.role.branch)?;
    ensure!(
        !target_is_checked_out(request.repo, &binding.role.branch)?,
        "file exploration branch must not be checked out during dual publication"
    );
    require_retained_worktree(request, landing)?;
    require_transaction(control, &landing.transaction_json)?;
    ensure!(
        selected_link(request)?.json == publication.link_json,
        "file merge Link changed before publication"
    );
    super::file_agent::require_seed_binding(request.repo, binding)?;
    let checkout = if landing.worktree_tree.is_some() {
        Some(plumbing::prepare_checkout_transaction(
            request.repo,
            &target.branch,
            &target.head,
            main,
            true,
        )?)
    } else {
        None
    };
    let input = format!(
        "start\noption no-deref\nupdate refs/heads/{} {} {}\noption no-deref\nupdate refs/heads/{} {} {}\nprepare\ncommit\n",
        target.branch,
        main,
        target.head,
        binding.role.branch,
        publication.candidate,
        publication.expected_old
    );
    plumbing::raw_git(request.repo, &["update-ref", "--stdin"], Some(&input)).context(
        "file merge publication is uncertain; replay the retained candidates before continuing",
    )?;
    checkpoint(Checkpoint::AfterCas)?;
    if let Some(checkout) = checkout {
        plumbing::refresh_prepared_checkout(request.repo, &checkout)
            .map_err(|failure| failure.error)
            .context(
                "file merge refs are published; the retained checkout journal must recover forward",
            )?;
        plumbing::finish_checkout_transaction(checkout)?;
    }
    Ok(())
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
    if let Some(main) = &landing.file_commit {
        ensure!(
            current_head(request.repo, request.binding.target_branch())? == *main,
            "file merge target moved before durable landing publication"
        );
    }
    let mut landed = journal.clone();
    landed.phase = ArchivePhase::Landed {
        merge_commit: landing
            .file_commit
            .clone()
            .unwrap_or_else(|| publication.candidate.clone()),
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
    let suffix = if request.binding.file_target.is_some() {
        ensure!(
            landing.file_commit.as_ref() == Some(merge_commit),
            "file merge completion candidate changed"
        );
        verify_file_result(request.repo, journal)?
    } else {
        archive_history::verify_merge_landing(
            request.repo,
            &request.binding.role.origin_head,
            &request.binding.source.head,
            &landing.ordinary_tree,
            merge_commit,
            &tree,
        )?
    };
    let records = verify_suffix(&suffix, request.binding)?;
    archive_history::verify_archive_append_target(
        request.repo,
        landing.file_evidence.as_deref().unwrap_or(merge_commit),
        &current_head(request.repo, &request.binding.role.branch)?,
    )?;
    checkpoint(Checkpoint::BeforeRetire)?;
    require_destination_routing(request.repo, &request.binding.role.branch)?;
    if request.binding.file_target.is_some() {
        require_destination_routing(request.repo, request.binding.target_branch())?;
        ensure!(
            current_head(request.repo, request.binding.target_branch())? == *merge_commit,
            "file merge target moved before exact transaction completion"
        );
    }
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
    fn land_with<K: KeyStore>(
        request: LandingRequest<'_>,
        dictionary: &RepositoryDictionary<K>,
        global: &Matcher,
        read_native: impl FnOnce(&Link) -> Result<Vec<u8>>,
        checkpoint: impl FnMut(Checkpoint) -> Result<()>,
    ) -> Result<LandingOutcome> {
        land_with_resolved(request, dictionary, global, read_native, checkpoint, &[])
    }

    use super::*;
    use crate::domain::merge_archive::{
        self, FrozenFileTarget, FrozenMergeSource, MergeArchiveRole, RuntimeLinkKey,
    };
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
            Self::with_line_kind(runtime, baseline, false)
        }

        fn file() -> Self {
            Self::with_line_kind("codex", BASELINE, true)
        }

        fn with_line_kind(runtime: &str, baseline: &[u8], file: bool) -> Self {
            Self::with_file_conflict(runtime, baseline, file, false)
        }

        fn with_file_conflict(runtime: &str, baseline: &[u8], file: bool, conflict: bool) -> Self {
            Self::with_file_history(runtime, baseline, file, conflict, false)
        }

        fn with_file_history(
            runtime: &str,
            baseline: &[u8],
            file: bool,
            conflict: bool,
            target_only: bool,
        ) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(&directory.path().join("target")).unwrap();
            let source = Repo::init(&directory.path().join("source")).unwrap();
            let mut metadata = if file {
                meta::Meta::new_file_line()
            } else {
                meta::Meta::new(SESSION.into(), "codex".into(), "/work".into())
            };
            if !file {
                metadata.turn = Some(1);
            }
            meta::write(repo.root(), &metadata).unwrap();
            let original_log = transcript::wrap_lines(
                "{\"type\":\"user\",\"text\":\"inherited\"}\n",
                "codex",
                SESSION,
            );
            if !file {
                storage::write_snapshot(repo.root(), &original_log, &original_log).unwrap();
            }
            std::fs::write(repo.root().join("AGENTS.md"), "Shared instructions\n").unwrap();
            repo.add_all().unwrap();
            repo.commit("landing target fixture").unwrap();
            let fork = repo.git(&["rev-parse", "HEAD"]).unwrap();
            if conflict || target_only {
                std::fs::write(repo.root().join("AGENTS.md"), "Target shared rule\n").unwrap();
                repo.add_all().unwrap();
                repo.commit("target shared conflict").unwrap();
            }
            let origin = repo.git(&["rev-parse", "HEAD"]).unwrap();
            let target_branch = if file { "main" } else { "work" };
            repo.git(&[
                "update-ref",
                &format!("refs/heads/{target_branch}"),
                &origin,
            ])
            .unwrap();
            repo.git(&[
                "symbolic-ref",
                "HEAD",
                &format!("refs/heads/{target_branch}"),
            ])
            .unwrap();
            plumbing::import_commit_graph(&source, &repo, &origin).unwrap();
            let source_event = transcript::wrap_lines(
                "{\"type\":\"user\",\"text\":\"selected source event\"}\n",
                "codex",
                SESSION,
            );
            if !file {
                metadata.turn = Some(2);
            }
            let log = format!("{original_log}{source_event}");
            let mut files = if file {
                std::collections::BTreeMap::from([(
                    "memory/source.md".into(),
                    b"Source shared rule\n".to_vec(),
                )])
            } else {
                storage::snapshot_files(&log, &log).unwrap()
            };
            files.insert(
                meta::FILE.into(),
                meta::to_text(&metadata).unwrap().into_bytes(),
            );
            if conflict {
                files.insert("AGENTS.md".into(), b"Source shared rule\n".to_vec());
            }
            let tree = plumbing::tree_apply_owned(
                &source,
                &fork,
                files
                    .into_iter()
                    .map(|(path, bytes)| (path, Some(bytes)))
                    .collect(),
            )
            .unwrap();
            let source_head =
                plumbing::commit_tree(&source, &tree, &[&fork], "landing source fixture").unwrap();
            source
                .git(&["update-ref", "refs/heads/source", &source_head])
                .unwrap();
            let native = directory.path().join("native.jsonl");
            std::fs::write(&native, baseline).unwrap();
            let mut binding = ExplorationBinding {
                file_target: file.then(|| FrozenFileTarget {
                    branch: "main".into(),
                    head: origin.clone(),
                }),
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
                    base: Some(fork),
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
            if !file {
                tx.picked = vec![format!("{}#2.1", binding.source.reference)];
            }
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
            if file {
                let seed =
                    super::super::file_agent::prepare_seed(super::super::file_agent::SeedRequest {
                        repo: &repo,
                        store: &store,
                        slug: "alice/target",
                        transaction_json: &json,
                        runtime,
                        cwd: directory.path(),
                    })
                    .unwrap();
                binding.role = seed.role;
                installed.branch = Some(binding.role.branch.clone());
                installed.materialized_from = Some(binding.role.origin_head.clone());
            }
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
        fn with_resolutions(
            &self,
            paths: &[String],
            checkpoint: impl FnMut(Checkpoint) -> Result<()>,
        ) -> Result<LandingOutcome> {
            land_with_resolved(
                self.request(),
                &self.dictionary,
                &Matcher::empty(),
                |_| Ok(std::fs::read(&self.native)?),
                checkpoint,
                paths,
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
    fn file_landing_publishes_distinct_main_and_evidence_history() {
        for with_records in [false, true] {
            let f = Fixture::file();
            if with_records {
                f.append(EXPLORATION);
            }
            let result = f.run().unwrap();
            let journal = f.journal();
            let landing = journal.landing.as_ref().unwrap();
            let evidence = landing.file_evidence.as_deref().unwrap();
            assert_eq!(landing.file_commit.as_deref(), Some(result.commit.as_str()));
            assert_eq!(current_head(&f.repo, "main").unwrap(), result.commit);
            assert_eq!(
                current_head(&f.repo, &f.binding.role.branch).unwrap(),
                evidence
            );
            assert_eq!(journal.accepted_commit.as_deref(), Some(evidence));
            assert_ne!(result.commit, evidence);
            assert_eq!(result.archived_records, usize::from(with_records));
            assert!(!f.tx_path().exists());
            assert_eq!(f.repo.git(&["status", "--porcelain"]).unwrap(), "");
            let paths = f.repo.ls_tree_result(&result.commit).unwrap();
            assert!(
                !paths
                    .iter()
                    .any(|path| path != meta::FILE && meta::is_storage_path(path))
            );
            let metadata = meta::read_at_ref_result(&f.repo, &result.commit)
                .unwrap()
                .unwrap();
            assert!(metadata.is_file_line());
            assert_eq!(metadata.kind, meta::Kind::Merge);
            let log = storage::materialize_at(f.repo.root(), evidence, meta::LOG_FILE).unwrap();
            assert_eq!(log.contains("private exploration"), with_records);
            assert_eq!(
                storage::materialize_at(f.repo.root(), evidence, meta::VIEW_FILE).unwrap(),
                ""
            );
            assert_eq!(
                f.repo
                    .git_status_local(&["merge-base", "--is-ancestor", evidence, &result.commit])
                    .unwrap()
                    .0,
                Some(1)
            );
            assert_eq!(
                f.repo
                    .git(&["rev-list", "--parents", "-n", "1", &result.commit])
                    .unwrap(),
                format!(
                    "{} {} {}",
                    result.commit,
                    f.binding.target_head(),
                    f.binding.source.head
                )
            );
            let replayed = replay(&f.repo, &f.store, &f.binding).unwrap().unwrap();
            assert_eq!(replayed, result);
        }
    }

    #[test]
    fn file_native_capture_graph_changes_preserve_frozen_merge_content() {
        for overlay in ["graft", "shallow", "replace"] {
            let f = Fixture::with_file_history("codex", BASELINE, true, false, true);
            f.append(EXPLORATION);
            let target = f.binding.target_head();
            let source = &f.binding.source.head;
            plumbing::import_commit_graph(&f.repo, &f.source, source).unwrap();
            let original_target = f
                .repo
                .git_bytes_result(&["cat-file", "commit", target])
                .unwrap();
            let original_source = f
                .repo
                .git_bytes_result(&["cat-file", "commit", source])
                .unwrap();
            let native = std::fs::read(&f.native).unwrap();
            let link = selected_link(&f.request()).unwrap().json;
            let transaction = std::fs::read(f.tx_path()).unwrap();
            let before = f.repo.git(&["show-ref"]).unwrap();
            let mut captured = false;
            let outcome = land_with(
                LandingRequest {
                    source_repo: &f.repo,
                    ..f.request()
                },
                &f.dictionary,
                &Matcher::empty(),
                |_| {
                    captured = true;
                    assert_eq!(f.repo.git(&["show-ref"])?, before);
                    match overlay {
                        "graft" => std::fs::write(
                            f.repo.git_path("info/grafts")?,
                            format!("{source} {target}\n"),
                        )?,
                        "shallow" => {
                            std::fs::write(f.repo.git_path("shallow")?, format!("{source}\n"))?
                        }
                        "replace" => {
                            let tree = f.repo.git(&["rev-parse", &format!("{source}^{{tree}}")])?;
                            let replacement =
                                plumbing::commit_tree(&f.repo, &tree, &[target], "replacement")?;
                            f.repo.git(&[
                                "update-ref",
                                &format!("refs/replace/{source}"),
                                &replacement,
                            ])?;
                        }
                        _ => unreachable!(),
                    }
                    assert_eq!(current_head(&f.repo, f.binding.target_branch())?, target);
                    assert_eq!(
                        current_head(&f.repo, &f.binding.role.branch)?,
                        f.binding.role.origin_head
                    );
                    Ok(native.clone())
                },
                |_| Ok(()),
            )
            .unwrap();
            assert!(captured);
            assert_eq!(
                f.repo.show_raw(&outcome.commit, "AGENTS.md").as_deref(),
                Some("Target shared rule\n")
            );
            assert_eq!(
                f.repo
                    .show_raw(&outcome.commit, "memory/source.md")
                    .as_deref(),
                Some("Source shared rule\n")
            );
            let raw = f
                .repo
                .git(&["cat-file", "commit", &outcome.commit])
                .unwrap();
            let parents: Vec<_> = raw
                .lines()
                .take_while(|line| !line.is_empty())
                .filter_map(|line| line.strip_prefix("parent "))
                .collect();
            assert_eq!(parents, vec![target, source.as_str()]);
            assert_eq!(
                f.repo
                    .git_bytes_result(&["cat-file", "commit", target])
                    .unwrap(),
                original_target
            );
            assert_eq!(
                f.repo
                    .git_bytes_result(&["cat-file", "commit", source])
                    .unwrap(),
                original_source
            );
            assert_eq!(std::fs::read(&f.native).unwrap(), native);
            assert_eq!(selected_link(&f.request()).unwrap().json, link);
            assert_eq!(std::fs::read(f.retired_path()).unwrap(), transaction);
            assert_eq!(
                current_head(&f.repo, f.binding.target_branch()).unwrap(),
                outcome.commit
            );
            let journal = f.journal();
            let evidence = current_head(&f.repo, &f.binding.role.branch).unwrap();
            assert_eq!(journal.accepted_commit.as_deref(), Some(evidence.as_str()));
            let log = storage::materialize_at(f.repo.root(), &evidence, meta::LOG_FILE).unwrap();
            assert_eq!(log.matches("private exploration").count(), 1);
            assert!(matches!(journal.phase, ArchivePhase::Landed { .. }));
            assert!(!f.tx_path().exists());
        }
    }

    #[test]
    fn file_landing_uses_resolved_shared_conflicts_and_refuses_unresolved_drafts() {
        for contents in [
            None,
            Some("<<<<<<< ours\nstill unresolved\n=======\nother side\n>>>>>>> theirs\n"),
            Some("Reconciled shared rule\n"),
        ] {
            let f = Fixture::with_file_conflict("codex", BASELINE, true, true);
            f.append(EXPLORATION);
            if let Some(contents) = contents {
                std::fs::write(f.repo.root().join("AGENTS.md"), contents).unwrap();
            }
            let refs = f.repo.git(&["show-ref"]).unwrap();
            let tx = std::fs::read(f.tx_path()).unwrap();
            let link = selected_link(&f.request()).unwrap().json;
            let native = std::fs::read(&f.native).unwrap();
            let result = f.run();
            if contents == Some("Reconciled shared rule\n") {
                let result = result.unwrap();
                assert_eq!(
                    f.repo.show_raw(&result.commit, "AGENTS.md").as_deref(),
                    contents
                );
                assert!(!f.tx_path().exists());
                assert_eq!(f.repo.git(&["status", "--porcelain"]).unwrap(), "");
            } else {
                assert!(result.is_err());
                assert_eq!(f.repo.git(&["show-ref"]).unwrap(), refs);
                assert_eq!(std::fs::read(f.tx_path()).unwrap(), tx);
                assert_eq!(selected_link(&f.request()).unwrap().json, link);
                assert_eq!(f.journal().phase, ArchivePhase::Open);
                assert!(f.journal().publication.is_none());
            }
            assert_eq!(std::fs::read(&f.native).unwrap(), native);
        }
    }

    /// Acknowledgements select only frozen conflicts and cannot grant another generation authority.
    #[test]
    fn file_landing_can_explicitly_keep_target_and_refuses_unrelated_acknowledgements() {
        for (paths, contents, valid) in [
            (vec![], None, false),
            (vec!["AGENTS.md"], None, true),
            (vec!["memory/source.md"], None, false),
            (vec!["missing.md"], None, false),
            (vec!["../AGENTS.md"], None, false),
            (vec![meta::FILE], None, false),
            (vec!["AGENTS.md", "AGENTS.md"], None, false),
            (
                vec!["AGENTS.md"],
                Some("<<<<<<< ours\nUnresolved\n=======\nOther\n>>>>>>> theirs\n"),
                false,
            ),
        ] {
            let f = Fixture::with_file_conflict("codex", BASELINE, true, true);
            f.append(EXPLORATION);
            if let Some(contents) = contents {
                std::fs::write(f.repo.root().join("AGENTS.md"), contents).unwrap();
            }
            let refs = f.repo.git(&["show-ref"]).unwrap();
            let tx = std::fs::read(f.tx_path()).unwrap();
            let link = selected_link(&f.request()).unwrap().json;
            let native = std::fs::read(&f.native).unwrap();
            let worktree = std::fs::read(f.repo.root().join("AGENTS.md")).unwrap();
            let paths: Vec<_> = paths.into_iter().map(str::to_owned).collect();
            let outcome = f.with_resolutions(&paths, |_| Ok(()));
            if valid {
                let outcome = outcome.unwrap();
                assert_eq!(
                    f.repo.show_raw(&outcome.commit, "AGENTS.md").as_deref(),
                    Some("Target shared rule\n")
                );
                assert_eq!(
                    f.repo
                        .git(&["show", "-s", "--format=%P", &outcome.commit])
                        .unwrap(),
                    format!("{} {}", f.binding.target_head(), f.binding.source.head)
                );
                let evidence = current_head(&f.repo, &f.binding.role.branch).unwrap();
                assert_eq!(
                    storage::materialize_at(f.repo.root(), &evidence, meta::VIEW_FILE).unwrap(),
                    ""
                );
                assert!(
                    storage::materialize_at(f.repo.root(), &evidence, meta::LOG_FILE)
                        .unwrap()
                        .contains("private exploration")
                );
                assert_eq!(f.repo.git(&["status", "--porcelain"]).unwrap(), "");
                assert_eq!(
                    replay(&f.repo, &f.store, &f.binding).unwrap(),
                    Some(outcome)
                );
            } else {
                assert!(outcome.is_err(), "{paths:?}");
                assert_eq!(f.repo.git(&["show-ref"]).unwrap(), refs);
                assert_eq!(std::fs::read(f.tx_path()).unwrap(), tx);
                assert_eq!(selected_link(&f.request()).unwrap().json, link);
                assert_eq!(f.journal().phase, ArchivePhase::Open);
                assert!(f.journal().publication.is_none());
                assert_eq!(
                    std::fs::read(f.repo.root().join("AGENTS.md")).unwrap(),
                    worktree
                );
            }
            assert_eq!(std::fs::read(&f.native).unwrap(), native);
        }
    }

    #[test]
    fn explicit_conflict_confirmation_requires_a_file_target_checkout() {
        for file in [false, true] {
            let f = Fixture::with_file_conflict("codex", BASELINE, file, file);
            if file {
                f.repo
                    .git(&["checkout", "--detach", f.binding.target_head()])
                    .unwrap();
            }
            let refs = f.repo.git(&["show-ref"]).unwrap();
            let tx = std::fs::read(f.tx_path()).unwrap();
            let link = selected_link(&f.request()).unwrap().json;
            let error = f
                .with_resolutions(&["AGENTS.md".into()], |_| Ok(()))
                .unwrap_err();
            assert!(format!("{error:#}").contains(if file {
                "target branch checkout"
            } else {
                "fresh Open file merge"
            }));
            assert_eq!(f.repo.git(&["show-ref"]).unwrap(), refs);
            assert_eq!(std::fs::read(f.tx_path()).unwrap(), tx);
            assert_eq!(selected_link(&f.request()).unwrap().json, link);
            assert!(f.journal().publication.is_none());
        }
    }

    #[test]
    fn confirmed_file_landing_replay_uses_only_its_retained_candidate() {
        let f = Fixture::with_file_conflict("codex", BASELINE, true, true);
        f.append(EXPLORATION);
        let paths = vec!["AGENTS.md".into()];
        assert!(
            f.with_resolutions(&paths, |point| {
                if point == Checkpoint::Prepared {
                    anyhow::bail!("injected landing stop");
                }
                Ok(())
            })
            .is_err()
        );
        let before = f.journal();
        let candidate = before
            .landing
            .as_ref()
            .unwrap()
            .file_commit
            .clone()
            .unwrap();
        let refs = f.repo.git(&["show-ref"]).unwrap();
        let tx = std::fs::read(f.tx_path()).unwrap();
        let error = f.with_resolutions(&paths, |_| Ok(())).unwrap_err();
        assert!(format!("{error:#}").contains("replay retained publication without --resolved"));
        assert_eq!(f.repo.git(&["show-ref"]).unwrap(), refs);
        assert_eq!(std::fs::read(f.tx_path()).unwrap(), tx);
        assert_eq!(
            serde_json::to_value(f.journal()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        std::fs::remove_file(&f.native).unwrap();
        std::fs::rename(
            f.source.root(),
            f._directory.path().join("source-unavailable"),
        )
        .unwrap();
        let result = replay(&f.repo, &f.store, &f.binding).unwrap().unwrap();
        assert_eq!(result.commit, candidate);
        assert_eq!(
            f.repo.show_raw(&result.commit, "AGENTS.md").as_deref(),
            Some("Target shared rule\n")
        );
    }

    #[test]
    fn explicit_file_resolution_does_not_bypass_generation_or_dual_ref_cas() {
        for fault in ["generation", "target", "evidence"] {
            let f = Fixture::with_file_conflict("codex", BASELINE, true, true);
            f.append(EXPLORATION);
            if fault == "generation" {
                use crate::domain::metadata_facts::JsonFacts;
                let _guard = mergetx::ControlGuard::acquire(f.repo.root()).unwrap();
                let raw = std::fs::read_to_string(f.tx_path()).unwrap();
                let JsonFacts::Object(mut fields) = JsonFacts::parse(&raw).unwrap() else {
                    panic!("fixture transaction must be an object");
                };
                fields.insert(
                    "generation".into(),
                    JsonFacts::String(uuid::Uuid::now_v7().to_string()),
                );
                let mut changed = String::new();
                JsonFacts::Object(fields).write_json(&mut changed).unwrap();
                // Foreign mutation bypasses the ordinary writer, which refuses replacement authority.
                std::fs::write(f.tx_path(), changed).unwrap();
            }
            let before = f.repo.git(&["show-ref"]).unwrap();
            let tx = std::fs::read(f.tx_path()).unwrap();
            let link = selected_link(&f.request()).unwrap().json;
            let paths = vec!["AGENTS.md".into()];
            let result = f.with_resolutions(&paths, |point| {
                if point == Checkpoint::BeforeCas && fault != "generation" {
                    let branch = if fault == "target" {
                        f.binding.target_branch()
                    } else {
                        &f.binding.role.branch
                    };
                    let old = current_head(&f.repo, branch)?;
                    let tree = f.repo.git(&["rev-parse", &format!("{old}^{{tree}}")])?;
                    let moved = plumbing::commit_tree(
                        &f.repo,
                        &tree,
                        &[&old],
                        "Concurrent fixture change",
                    )?;
                    f.repo
                        .git(&["update-ref", &format!("refs/heads/{branch}"), &moved, &old])?;
                }
                Ok(())
            });
            assert!(result.is_err(), "{fault}");
            assert_eq!(std::fs::read(f.tx_path()).unwrap(), tx);
            assert_eq!(selected_link(&f.request()).unwrap().json, link);
            assert_eq!(f.journal().phase, ArchivePhase::Open);
            if fault == "generation" {
                assert_eq!(f.repo.git(&["show-ref"]).unwrap(), before);
                assert!(f.journal().publication.is_none());
            } else {
                let untouched = if fault == "target" {
                    &f.binding.role.branch
                } else {
                    f.binding.target_branch()
                };
                let expected = if fault == "target" {
                    &f.binding.role.origin_head
                } else {
                    f.binding.target_head()
                };
                assert_eq!(current_head(&f.repo, untouched).unwrap(), expected);
                assert!(f.journal().publication.is_some());
            }
        }
    }

    #[test]
    fn empty_file_evidence_requires_the_seed_tree_and_exact_parent() {
        let f = Fixture::file();
        let seed = &f.binding.role.origin_head;
        let tree = f
            .repo
            .git(&["rev-parse", "--verify", &format!("{seed}^{{tree}}")])
            .unwrap();
        let valid =
            plumbing::commit_tree(&f.repo, &tree, &[seed], "empty exploration checkpoint").unwrap();
        archive_history::verify_empty_file_exploration(&f.repo, seed, &valid).unwrap();
        for parents in [
            vec![f.binding.target_head()],
            vec![seed.as_str(), f.binding.target_head()],
        ] {
            let wrong = plumbing::commit_tree(&f.repo, &tree, &parents, "wrong exploration parent")
                .unwrap();
            assert!(archive_history::verify_empty_file_exploration(&f.repo, seed, &wrong).is_err());
        }
        let changed = plumbing::tree_apply_owned(
            &f.repo,
            &tree,
            vec![(
                "AGENTS.md".into(),
                Some(b"changed shared content\n".to_vec()),
            )],
        )
        .unwrap();
        let wrong =
            plumbing::commit_tree(&f.repo, &changed, &[seed], "changed empty exploration tree")
                .unwrap();
        assert!(archive_history::verify_empty_file_exploration(&f.repo, seed, &wrong).is_err());
        assert_eq!(
            current_head(&f.repo, "main").unwrap(),
            f.binding.target_head()
        );
        assert_eq!(
            current_head(&f.repo, &f.binding.role.branch).unwrap(),
            *seed
        );
    }

    #[test]
    fn file_publication_replay_keeps_both_candidates_and_recovers_checkout_forward() {
        for stop in [
            Checkpoint::Prepared,
            Checkpoint::AfterCas,
            Checkpoint::Landed,
            Checkpoint::Retired,
        ] {
            let f = Fixture::file();
            f.append(EXPLORATION);
            f.stop(stop);
            let journal = f.journal();
            let landing = journal.landing.as_ref().unwrap();
            let main = landing.file_commit.clone().unwrap();
            let evidence = landing.file_evidence.clone().unwrap();
            f.append(b"{\"type\":\"assistant\",\"text\":\"later unconsumed evidence\"}\n");
            std::fs::rename(
                f.source.root(),
                f._directory.path().join("source-unavailable"),
            )
            .unwrap();
            let result = replay(&f.repo, &f.store, &f.binding).unwrap().unwrap();
            assert_eq!(result.commit, main);
            assert_eq!(current_head(&f.repo, "main").unwrap(), main);
            assert_eq!(
                current_head(&f.repo, &f.binding.role.branch).unwrap(),
                evidence
            );
            assert_eq!(f.repo.git(&["status", "--porcelain"]).unwrap(), "");
            assert!(
                !storage::materialize_at(f.repo.root(), &evidence, meta::LOG_FILE)
                    .unwrap()
                    .contains("later unconsumed")
            );
            assert_eq!(
                f.journal().consumed.bytes,
                (BASELINE.len() + EXPLORATION.len()) as u64
            );
            assert!(!f.tx_path().exists());
        }
    }

    #[test]
    fn file_evidence_checkout_before_publication_preserves_refs_and_worktree() {
        let f = Fixture::file();
        f.append(EXPLORATION);
        f.repo
            .git(&["checkout", "--detach", f.binding.target_head()])
            .unwrap();
        let transaction = std::fs::read(f.tx_path()).unwrap();
        let link = selected_link(&f.request()).unwrap().json;
        let native = std::fs::read(&f.native).unwrap();
        let mut checkout = None;
        let error = f
            .at(|step| {
                if step == Checkpoint::BeforeCas {
                    f.repo.git(&["checkout", &f.binding.role.branch])?;
                    checkout = Some((
                        std::fs::read(f.repo.git_path("index")?)?,
                        std::fs::read(f.repo.root().join(meta::FILE))?,
                        std::fs::read(f.repo.root().join("AGENTS.md"))?,
                    ));
                }
                Ok(())
            })
            .unwrap_err();
        assert!(
            error.to_string().contains(
                "file exploration branch must not be checked out during dual publication"
            )
        );
        let (index, metadata, shared) = checkout.expect("evidence checkout was not reached");
        assert_eq!(
            std::fs::read(f.repo.git_path("index").unwrap()).unwrap(),
            index
        );
        assert_eq!(
            std::fs::read(f.repo.root().join(meta::FILE)).unwrap(),
            metadata
        );
        assert_eq!(
            std::fs::read(f.repo.root().join("AGENTS.md")).unwrap(),
            shared
        );
        assert_eq!(std::fs::read(f.tx_path()).unwrap(), transaction);
        assert_eq!(selected_link(&f.request()).unwrap().json, link);
        assert_eq!(std::fs::read(&f.native).unwrap(), native);
        assert_eq!(
            current_head(&f.repo, "main").unwrap(),
            f.binding.target_head()
        );
        assert_eq!(
            current_head(&f.repo, &f.binding.role.branch).unwrap(),
            f.binding.role.origin_head
        );
        let pending = f.journal();
        assert_eq!(pending.phase, ArchivePhase::Open);
        assert_eq!(pending.consumed, f.binding.installed);
        let landing = pending.landing.as_ref().unwrap();
        assert!(landing.worktree_tree.is_none());
        let main = landing.file_commit.as_ref().unwrap();
        let evidence = landing.file_evidence.as_ref().unwrap();
        assert_ne!(evidence, &f.binding.role.origin_head);
        f.repo
            .git(&["checkout", "--detach", f.binding.target_head()])
            .unwrap();
        let outcome = replay(&f.repo, &f.store, &f.binding).unwrap().unwrap();
        assert_eq!(&outcome.commit, main);
        assert_eq!(&current_head(&f.repo, "main").unwrap(), main);
        assert_eq!(
            &current_head(&f.repo, &f.binding.role.branch).unwrap(),
            evidence
        );
        assert_eq!(
            f.repo.git(&["rev-parse", "HEAD"]).unwrap(),
            f.binding.target_head()
        );
        assert_eq!(f.repo.git(&["status", "--porcelain"]).unwrap(), "");
        assert!(!f.tx_path().exists());
    }

    #[test]
    fn a_file_main_cas_failure_cannot_publish_only_the_evidence_ref() {
        let f = Fixture::file();
        f.append(EXPLORATION);
        let target = f.binding.target_head();
        let tree = f
            .repo
            .git(&["rev-parse", "--verify", &format!("{target}^{{tree}}")])
            .unwrap();
        let moved =
            plumbing::commit_tree(&f.repo, &tree, &[target], "independent main advance").unwrap();
        assert!(
            f.at(|step| {
                if step == Checkpoint::BeforeCas {
                    plumbing::update_ref_cas(&f.repo, "refs/heads/main", &moved, Some(target))?;
                }
                Ok(())
            })
            .is_err()
        );
        let pending = f.journal();
        assert_eq!(pending.phase, ArchivePhase::Open);
        assert_eq!(pending.consumed, f.binding.installed);
        assert_eq!(
            current_head(&f.repo, &f.binding.role.branch).unwrap(),
            f.binding.role.origin_head
        );
        assert_eq!(current_head(&f.repo, "main").unwrap(), moved);
        plumbing::update_ref_cas(&f.repo, "refs/heads/main", target, Some(&moved)).unwrap();
        let result = replay(&f.repo, &f.store, &f.binding).unwrap().unwrap();
        assert_eq!(
            Some(result.commit),
            pending.landing.as_ref().unwrap().file_commit
        );
    }

    #[test]
    fn a_file_evidence_cas_failure_cannot_publish_only_the_main_ref() {
        let f = Fixture::file();
        f.append(EXPLORATION);
        let origin = &f.binding.role.origin_head;
        let tree = f
            .repo
            .git(&["rev-parse", "--verify", &format!("{origin}^{{tree}}")])
            .unwrap();
        let moved =
            plumbing::commit_tree(&f.repo, &tree, &[origin], "independent evidence advance")
                .unwrap();
        let evidence_ref = format!("refs/heads/{}", f.binding.role.branch);
        assert!(
            f.at(|step| {
                if step == Checkpoint::BeforeCas {
                    plumbing::update_ref_cas(&f.repo, &evidence_ref, &moved, Some(origin))?;
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(
            current_head(&f.repo, "main").unwrap(),
            f.binding.target_head()
        );
        assert_eq!(
            current_head(&f.repo, &f.binding.role.branch).unwrap(),
            moved
        );
        assert_eq!(f.journal().consumed, f.binding.installed);
        plumbing::update_ref_cas(&f.repo, &evidence_ref, origin, Some(&moved)).unwrap();
        let result = replay(&f.repo, &f.store, &f.binding).unwrap().unwrap();
        assert_eq!(current_head(&f.repo, "main").unwrap(), result.commit);
        assert_eq!(f.repo.git(&["status", "--porcelain"]).unwrap(), "");
    }

    #[test]
    fn mixed_file_publication_endpoints_refuse_without_retiring_or_consuming() {
        for main_only in [false, true] {
            let f = Fixture::file();
            f.append(EXPLORATION);
            f.stop(Checkpoint::Prepared);
            let pending = f.journal();
            let landing = pending.landing.as_ref().unwrap();
            let (branch, old, candidate) = if main_only {
                (
                    "main",
                    f.binding.target_head(),
                    landing.file_commit.as_deref().unwrap(),
                )
            } else {
                (
                    f.binding.role.branch.as_str(),
                    f.binding.role.origin_head.as_str(),
                    landing.file_evidence.as_deref().unwrap(),
                )
            };
            let refname = format!("refs/heads/{branch}");
            plumbing::update_ref_cas(&f.repo, &refname, candidate, Some(old)).unwrap();
            let tx = std::fs::read(f.tx_path()).unwrap();
            let link = selected_link(&f.request()).unwrap().json;
            let refs = f.repo.git(&["show-ref"]).unwrap();
            assert!(replay(&f.repo, &f.store, &f.binding).is_err());
            assert_eq!(f.journal(), pending);
            assert_eq!(std::fs::read(f.tx_path()).unwrap(), tx);
            assert_eq!(selected_link(&f.request()).unwrap().json, link);
            assert_eq!(f.repo.git(&["show-ref"]).unwrap(), refs);
            plumbing::update_ref_cas(&f.repo, &refname, old, Some(candidate)).unwrap();
            replay(&f.repo, &f.store, &f.binding).unwrap().unwrap();
        }
    }

    #[test]
    fn file_completion_does_not_retire_a_main_rewound_after_publication() {
        let f = Fixture::file();
        f.append(EXPLORATION);
        f.stop(Checkpoint::Landed);
        let journal = f.journal();
        let main = journal
            .landing
            .as_ref()
            .unwrap()
            .file_commit
            .as_deref()
            .unwrap();
        plumbing::update_ref_cas(
            &f.repo,
            "refs/heads/main",
            f.binding.target_head(),
            Some(main),
        )
        .unwrap();
        let tx = std::fs::read(f.tx_path()).unwrap();
        assert!(replay(&f.repo, &f.store, &f.binding).is_err());
        assert_eq!(std::fs::read(f.tx_path()).unwrap(), tx);
        assert_eq!(f.journal(), journal);
        plumbing::update_ref_cas(
            &f.repo,
            "refs/heads/main",
            main,
            Some(f.binding.target_head()),
        )
        .unwrap();
        assert_eq!(
            replay(&f.repo, &f.store, &f.binding)
                .unwrap()
                .unwrap()
                .commit,
            main
        );
    }

    #[test]
    fn file_cancellation_preserves_native_and_visible_evidence_without_publishing_main() {
        for prepared in [false, true] {
            let f = Fixture::file();
            f.append(EXPLORATION);
            if prepared {
                f.stop(Checkpoint::Prepared);
            }
            let refs = f.repo.git(&["show-ref"]).unwrap();
            let native = std::fs::read(&f.native).unwrap();
            let link = selected_link(&f.request()).unwrap().json;
            let cancel = || {
                super::super::abort::abort(super::super::abort::AbortRequest {
                    repo: &f.repo,
                    store: &f.store,
                    binding: &f.binding,
                })
            };
            assert_eq!(
                cancel().unwrap(),
                super::super::abort::AbortOutcome::Aborted
            );
            assert_eq!(
                cancel().unwrap(),
                super::super::abort::AbortOutcome::Aborted
            );
            assert_eq!(f.journal().phase, ArchivePhase::Aborted);
            assert_eq!(f.journal().consumed, f.binding.installed);
            assert_eq!(f.repo.git(&["show-ref"]).unwrap(), refs);
            assert_eq!(std::fs::read(&f.native).unwrap(), native);
            assert_eq!(selected_link(&f.request()).unwrap().json, link);
            assert!(!f.tx_path().exists());
            assert!(replay(&f.repo, &f.store, &f.binding).is_err());
        }
    }

    #[test]
    fn file_cancellation_does_not_need_a_native_file_after_fresh_launch_retirement() {
        let f = Fixture::with_line_kind("claude-code", b"", true);
        merge_archive::durable_publish_transition_bytes(&f.native, b"", true).unwrap();
        let selected = selected_link(&f.request()).unwrap();
        let refs = f
            .repo
            .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
            .unwrap();
        super::super::file_agent::reserve_fresh_launch_fixture(
            &f.repo,
            &f.binding,
            &selected.link,
            &f.native,
        )
        .unwrap();
        assert!(!f.native.exists());
        let request = || super::super::abort::AbortRequest {
            repo: &f.repo,
            store: &f.store,
            binding: &f.binding,
        };
        assert_eq!(
            super::super::abort::abort(request()).unwrap(),
            super::super::abort::AbortOutcome::Aborted
        );
        assert_eq!(
            super::super::abort::abort(request()).unwrap(),
            super::super::abort::AbortOutcome::Aborted
        );
        assert!(!f.native.exists());
        assert!(!f.tx_path().exists());
        assert_eq!(f.journal().phase, ArchivePhase::Aborted);
        assert_eq!(
            f.repo
                .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
                .unwrap(),
            refs
        );
        assert_eq!(selected_link(&f.request()).unwrap().json, selected.json);
    }

    #[test]
    fn file_cancellation_finishes_a_visible_pair_and_never_rolls_it_back() {
        for checkpoint in [
            Checkpoint::AfterCas,
            Checkpoint::Landed,
            Checkpoint::Retired,
        ] {
            let f = Fixture::file();
            f.append(EXPLORATION);
            f.stop(checkpoint);
            let refs = f.repo.git(&["show-ref"]).unwrap();
            let main = current_head(&f.repo, "main").unwrap();
            let result = super::super::abort::abort(super::super::abort::AbortRequest {
                repo: &f.repo,
                store: &f.store,
                binding: &f.binding,
            })
            .unwrap();
            let super::super::abort::AbortOutcome::AlreadyLanded(outcome) = result else {
                panic!("visible file merge was cancelled");
            };
            assert_eq!(outcome.commit, main);
            assert_eq!(f.repo.git(&["show-ref"]).unwrap(), refs);
            assert_eq!(f.repo.git(&["status", "--porcelain"]).unwrap(), "");
            assert!(!f.tx_path().exists());
        }
    }

    #[test]
    fn file_cancellation_refuses_either_mixed_publication_without_mutation() {
        for main_only in [false, true] {
            let f = Fixture::file();
            f.append(EXPLORATION);
            f.stop(Checkpoint::Prepared);
            let journal = f.journal();
            let landing = journal.landing.as_ref().unwrap();
            let (branch, old, new) = if main_only {
                (
                    "main",
                    f.binding.target_head(),
                    landing.file_commit.as_deref().unwrap(),
                )
            } else {
                (
                    f.binding.role.branch.as_str(),
                    f.binding.role.origin_head.as_str(),
                    landing.file_evidence.as_deref().unwrap(),
                )
            };
            plumbing::update_ref_cas(&f.repo, &format!("refs/heads/{branch}"), new, Some(old))
                .unwrap();
            let refs = f.repo.git(&["show-ref"]).unwrap();
            let tx = std::fs::read(f.tx_path()).unwrap();
            let link = selected_link(&f.request()).unwrap().json;
            assert!(
                super::super::abort::abort(super::super::abort::AbortRequest {
                    repo: &f.repo,
                    store: &f.store,
                    binding: &f.binding,
                })
                .is_err()
            );
            assert_eq!(f.journal(), journal);
            assert_eq!(std::fs::read(f.tx_path()).unwrap(), tx);
            assert_eq!(selected_link(&f.request()).unwrap().json, link);
            assert_eq!(f.repo.git(&["show-ref"]).unwrap(), refs);
        }
    }

    #[test]
    fn file_dispatch_selects_the_main_transaction_not_the_evidence_branch() {
        let f = Fixture::file();
        let tx = mergetx::read(f.repo.root()).unwrap().unwrap();
        assert_eq!(
            super::super::dispatch::select(&f.repo, "alice/target", Some("main"), Some(&tx))
                .unwrap(),
            Some(f.binding.clone())
        );
        assert!(
            super::super::dispatch::select(
                &f.repo,
                "alice/target",
                Some(&f.binding.role.branch),
                Some(&tx)
            )
            .is_err()
        );
        assert!(
            super::super::dispatch::select(&f.repo, "alice/foreign", Some("main"), Some(&tx))
                .is_err()
        );
        assert_eq!(
            super::super::dispatch::destination(&f.repo, "main")
                .unwrap()
                .root(),
            f.repo.root().canonicalize().unwrap()
        );
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
