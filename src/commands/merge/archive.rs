//! Publishes a landed merge runtime's complete trailing records as LOG-only history.
//!
//! Prepared Git and Link identities remain authoritative across retries. The journal cursor
//! advances only after checkout-aware publication; native growth cannot rebuild a pending commit.
//! Inherited Git routing overrides are refused before locks or effects, so immutable proofs and
//! existing publication plumbing cannot operate on different repositories or object stores.
//! Git's effective worktree must also name the explicit destination, including linked worktrees.
//! Archive tail publication requires Git's NUL-delimited worktree listing capability; an
//! unavailable capability refuses publication before locks or writes rather than guessing holders.

pub mod abort;
pub mod activation;
pub mod completion;
pub(super) mod dispatch;
pub mod landing;
pub(super) mod launch;
pub mod preparation;

use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};
use std::io::Read;

use crate::Result;
use crate::commands::plumbing;
use crate::domain::link::{self, ArchiveLinkSnapshot, Link};
use crate::domain::merge_archive::{
    ArchiveJournal, ArchiveJournalGuard, ArchivePhase, ArchivePublicationKind, ExplorationBinding,
    MergeArchiveRole, PreparedArchivePublication, RuntimeLinkKey,
};
use crate::domain::metadata_facts::JsonFacts;
use crate::domain::secret_filter::{KeyStore, Matcher, RepositoryDictionary};
use crate::domain::store::Store;
use crate::domain::{
    archive_history, mergetx, meta, native_archive, repo::Repo, storage, transcript,
};

/// Routing is explicit; neither the runtime environment nor cwd chooses an archive destination.
pub struct TailDestination<'a> {
    pub repo: &'a Repo,
    pub store: &'a Store,
    pub role: &'a MergeArchiveRole,
    pub native: &'a RuntimeLinkKey,
}

#[derive(Debug, Eq, PartialEq)]
pub enum TailOutcome {
    Noop { pending_bytes: usize },
    Published { commit: String, records: u64 },
}

pub(crate) fn settlement_destination(primary: &Repo, branch: &str) -> Result<Repo> {
    dispatch::destination(primary, branch)
}

/// The caller has already selected a landed archive role; this core performs no lifecycle dispatch.
pub fn settle_tail(destination: TailDestination<'_>, global: &Matcher) -> Result<TailOutcome> {
    require_destination_routing(destination.repo, &destination.role.branch)?;
    let dictionary = RepositoryDictionary::open(destination.repo.root())?;
    settle_tail_with(destination, &dictionary, global, read_native, |_| Ok(()))
}

/// Final capture consumes any retained candidate before inspecting the child's last complete suffix.
pub(crate) fn settle_final_tail(
    destination: TailDestination<'_>,
    binding: &ExplorationBinding,
    global: &Matcher,
) -> Result<TailOutcome> {
    require_destination_routing(destination.repo, &destination.role.branch)?;
    let dictionary = RepositoryDictionary::open(destination.repo.root())?;
    settle_tail_mode(
        destination,
        &dictionary,
        global,
        read_native,
        |_| Ok(()),
        Some(binding),
    )
}

fn require_destination_routing(repo: &Repo, branch: &str) -> Result<()> {
    for name in archive_history::GIT_ROUTING_ENV {
        ensure!(
            std::env::var_os(name).is_none(),
            "archive publication refuses inherited Git routing override {name}"
        );
    }
    let worktrees = repo
        .git_bytes_result(&["worktree", "list", "--porcelain", "-z"])
        .context("archive tail cannot read NUL-delimited worktrees; upgrade Git if worktree list --porcelain -z is unsupported")?;
    let holders = branch_holders(&worktrees, branch)?;
    let output = repo.git_bytes_result(&["rev-parse", "--show-toplevel"])?;
    let root = output
        .strip_suffix(b"\n")
        .context("Git worktree path is unterminated")?;
    #[cfg(windows)]
    let root = root.strip_suffix(b"\r").unwrap_or(root);
    let reported = worktree_path(root)?;
    ensure!(
        reported.canonicalize()? == repo.root().canonicalize()?,
        "archive Git worktree differs from its explicit destination"
    );
    for holder in holders {
        ensure!(
            holder.canonicalize()? == repo.root().canonicalize()?,
            "archive target is checked out in a different worktree"
        );
    }
    Ok(())
}

fn worktree_path(bytes: &[u8]) -> Result<std::path::PathBuf> {
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
    };
    #[cfg(windows)]
    let path = std::path::PathBuf::from(
        std::str::from_utf8(bytes).context("Git worktree path is not Unicode")?,
    );
    ensure!(path.is_absolute(), "Git worktree path is not absolute");
    Ok(path)
}

fn branch_holders(bytes: &[u8], branch: &str) -> Result<Vec<std::path::PathBuf>> {
    ensure!(
        bytes.ends_with(b"\0\0"),
        "Git worktree listing is not terminated"
    );
    let target = format!("refs/heads/{branch}");
    let mut holders = Vec::new();
    let mut record = Vec::new();
    for field in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        if !field.is_empty() {
            record.push(field);
            continue;
        }
        let path = record
            .first()
            .and_then(|field: &&[u8]| field.strip_prefix(b"worktree "))
            .context("Git worktree record has no leading path")?;
        let path = worktree_path(path)?;
        let mut seen = std::collections::HashSet::new();
        let mut holder = None;
        for field in &record[1..] {
            let mut parts = field.splitn(2, |byte| *byte == b' ');
            let key = parts.next().context("Git worktree field is missing")?;
            let value = parts.next();
            ensure!(seen.insert(key), "Git worktree record repeats a field");
            match key {
                b"HEAD" => ensure!(
                    value.is_some_and(|value| !value.is_empty()),
                    "Git worktree HEAD is missing"
                ),
                b"branch" => {
                    let value = value.context("Git worktree branch is missing")?;
                    ensure!(
                        value.starts_with(b"refs/heads/") && value.len() > b"refs/heads/".len(),
                        "Git worktree branch is invalid"
                    );
                    holder = Some(value);
                }
                b"bare" | b"detached" => ensure!(value.is_none(), "Git worktree flag has a value"),
                b"locked" | b"prunable" => {}
                _ => anyhow::bail!("Git worktree record contains an unknown field"),
            }
        }
        let bare = seen.contains(b"bare".as_slice());
        let detached = seen.contains(b"detached".as_slice());
        ensure!(
            if bare {
                !seen.contains(b"HEAD".as_slice()) && holder.is_none() && !detached
            } else {
                seen.contains(b"HEAD".as_slice()) && (holder.is_some() != detached)
            },
            "Git worktree record has ambiguous branch ownership"
        );
        if holder == Some(target.as_bytes()) {
            holders.push(path);
        }
        record.clear();
    }
    Ok(holders)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Checkpoint {
    Prepared,
    BeforeCas,
    AfterCas,
}

fn read_native(link: &Link) -> Result<Vec<u8>> {
    let path = link
        .resolve()
        .context("archive native transcript is unavailable")?;
    let mut bytes = Vec::new();
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "archive native carrier is not a regular file"
    );
    file.take(storage::MAX_MATERIALIZED_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= storage::MAX_MATERIALIZED_BYTES,
        "archive native snapshot exceeds its byte limit"
    );
    Ok(bytes)
}

fn settle_tail_with<K: KeyStore>(
    destination: TailDestination<'_>,
    dictionary: &RepositoryDictionary<K>,
    global: &Matcher,
    read_native: impl FnOnce(&Link) -> Result<Vec<u8>>,
    checkpoint: impl FnMut(Checkpoint) -> Result<()>,
) -> Result<TailOutcome> {
    settle_tail_mode(
        destination,
        dictionary,
        global,
        read_native,
        checkpoint,
        None,
    )
}

fn settle_tail_mode<K: KeyStore>(
    destination: TailDestination<'_>,
    dictionary: &RepositoryDictionary<K>,
    global: &Matcher,
    read_native: impl FnOnce(&Link) -> Result<Vec<u8>>,
    mut checkpoint: impl FnMut(Checkpoint) -> Result<()>,
    final_binding: Option<&ExplorationBinding>,
) -> Result<TailOutcome> {
    require_destination_routing(destination.repo, &destination.role.branch)?;
    let TailDestination {
        repo,
        store,
        role,
        native,
    } = destination;
    role.validate(role.origin_head.len())?;
    native.validate()?;
    let _branch = link::lock_branch(store, &role.slug, &role.branch)?;
    let _link = link::lock(store, &native.runtime, &native.session_id)?;
    let guard = ArchiveJournalGuard::acquire(repo.root(), &role.generation)?;
    let control = mergetx::ControlGuard::acquire(repo.root())?;
    require_destination_routing(repo, &role.branch)?;
    guard.recover_pending()?;
    let mut journal = guard.read()?.context("archive journal is missing")?;
    ensure!(
        journal.binding.role == *role && journal.binding.native == *native,
        "archive journal does not match the selected runtime role"
    );
    ensure!(
        final_binding.is_none_or(|binding| journal.binding == *binding),
        "archive final capture differs from its launched binding"
    );
    ensure!(
        matches!(journal.phase, ArchivePhase::Landed { .. }) && journal.detach.is_none(),
        "archive tail requires a landed, attached runtime"
    );
    ensure!(
        !control.read()?.is_some_and(|tx| tx.target == role.branch),
        "archive target has an active merge transaction"
    );
    let selected = link::read_archive_link_snapshot(store, &native.runtime, &native.session_id)?
        .context("archive runtime Link is missing")?;
    ensure!(
        selected
            .link
            .is_archive_for(role, &native.runtime, &native.session_id),
        "archive runtime Link no longer owns the selected role"
    );
    ensure!(
        selected.link.baseline_bytes == Some(journal.binding.installed.bytes)
            && selected.link.baseline_hash.as_ref() == Some(&journal.binding.installed.sha256),
        "archive runtime Link has a different installation frontier"
    );
    plumbing::recover_interrupted_checkout(repo)?;
    if journal.publication.is_some() {
        let outcome = publish_prepared(repo, &guard, &journal, &selected, &mut checkpoint)?;
        if final_binding.is_none() {
            return Ok(outcome);
        }
        // The same locks cover recovery and the final snapshot, so hooks cannot interleave cursors.
        journal = guard
            .read()?
            .context("archive journal disappeared after tail recovery")?;
    }
    let head = current_head(repo, &role.branch)?;
    let accepted = journal
        .accepted_commit
        .as_deref()
        .context("archive has no accepted publication")?;
    archive_history::verify_archive_append_target(repo, accepted, &head)?;
    let snapshot = read_native(&selected.link)?;
    ensure!(
        snapshot.len() <= storage::MAX_MATERIALIZED_BYTES,
        "archive native snapshot exceeds its byte limit"
    );
    let captured = capture_selected(&snapshot, &journal)?;
    let pending_bytes = captured.pending_bytes;
    if captured.record_count == 0 {
        ensure!(
            final_binding.is_none() || pending_bytes == 0,
            "archive final capture retains an incomplete native record"
        );
        return Ok(TailOutcome::Noop { pending_bytes });
    }
    let protected = dictionary.protect_jsonl(&captured.records, global)?;
    ensure!(
        protected.text.len() <= storage::MAX_MATERIALIZED_BYTES,
        "protected archive records exceed their byte limit"
    );
    let checked = native_archive::capture(
        protected.text.as_bytes(),
        0,
        &hex::encode(Sha256::digest([])),
    )?;
    ensure!(
        checked.record_count == captured.record_count && checked.unconsumed.is_empty(),
        "archive protection changed the complete record boundary"
    );
    let envelopes = transcript::wrap_lines(&protected.text, &native.runtime, &role.logical_session);
    let (candidate, tree) = build_candidate(repo, &head, role, &envelopes, captured.record_count)?;
    let mut pending = journal.clone();
    pending.publication = Some(PreparedArchivePublication {
        kind: ArchivePublicationKind::Tail,
        link_json: selected.json.clone(),
        expected_old: head,
        candidate,
        candidate_tree: tree,
        prior_frontier: journal.consumed.clone(),
        next_frontier: captured.frontier,
        next_opencode: captured.opencode,
        appended_records: u64::try_from(captured.record_count)?,
        protected_suffix_sha256: hex::encode(Sha256::digest(envelopes.as_bytes())),
    });
    guard.replace(&journal, &pending)?;
    checkpoint(Checkpoint::Prepared)?;
    let outcome = publish_prepared(repo, &guard, &pending, &selected, &mut checkpoint)?;
    ensure!(
        final_binding.is_none() || pending_bytes == 0,
        "archive final capture retains an incomplete native record"
    );
    Ok(outcome)
}

struct SelectedCapture {
    records: String,
    record_count: usize,
    frontier: native_archive::Frontier,
    opencode: Option<native_archive::opencode::State>,
    pending_bytes: usize,
}

fn capture_selected(snapshot: &[u8], journal: &ArchiveJournal) -> Result<SelectedCapture> {
    if journal.binding.native.runtime == "opencode" {
        let state = journal.opencode.as_ref().context(
            "OpenCode archive has no installed observation state; abort or detach this exploration",
        )?;
        let observed = state.observe(snapshot, &journal.consumed)?;
        return Ok(SelectedCapture {
            records: observed.records,
            record_count: observed.record_count,
            frontier: observed.frontier,
            opencode: Some(observed.state),
            pending_bytes: observed.pending_bytes,
        });
    }
    let installed = &journal.binding.installed;
    native_archive::capture(
        &snapshot[..usize::try_from(installed.bytes)?.min(snapshot.len())],
        installed.bytes,
        &installed.sha256,
    )?;
    let captured =
        native_archive::capture(snapshot, journal.consumed.bytes, &journal.consumed.sha256)?;
    Ok(SelectedCapture {
        records: captured.records.to_owned(),
        record_count: captured.record_count,
        frontier: captured.frontier,
        opencode: None,
        pending_bytes: captured.unconsumed.len(),
    })
}

fn current_head(repo: &Repo, branch: &str) -> Result<String> {
    repo.git(&[
        "rev-parse",
        "--verify",
        &format!("refs/heads/{branch}^{{commit}}"),
    ])
}

fn build_candidate(
    repo: &Repo,
    head: &str,
    role: &MergeArchiveRole,
    envelopes: &str,
    count: usize,
) -> Result<(String, String)> {
    let raw = plumbing::regular_blob_text_at(repo, head, meta::FILE)?
        .context("archive parent metadata is missing")?;
    let metadata = meta::parse_strict(&raw, head)?;
    ensure!(
        metadata.session == role.logical_session,
        "archive parent has a different logical session"
    );
    let mut facts = JsonFacts::parse(&raw)?;
    let JsonFacts::Object(fields) = &mut facts else {
        anyhow::bail!("archive metadata must be an object")
    };
    fields.insert("kind".into(), JsonFacts::String("archive".into()));
    let mut next_metadata = String::new();
    encode_facts(&facts, &mut next_metadata)?;
    next_metadata.push('\n');
    let mut files = storage::snapshot_files(envelopes, "")?;
    files.remove(meta::VIEW_FILE);
    let suffix = files
        .remove(meta::LOG_FILE)
        .context("archive event sequence is missing")?;
    ensure!(
        storage::parse_sequence(std::str::from_utf8(&suffix)?)?.len() == count,
        "archive wrapping changed the record count"
    );
    let mut log = plumbing::regular_blob_text_at(repo, head, meta::LOG_FILE)?
        .context("archive parent LOG is missing")?;
    log.push_str(std::str::from_utf8(&suffix)?);
    storage::parse_sequence(&log)?;
    files.insert(meta::LOG_FILE.into(), log.into_bytes());
    files.insert(meta::FILE.into(), next_metadata.into_bytes());
    let tree = plumbing::tree_apply_owned(
        repo,
        head,
        files
            .into_iter()
            .map(|(path, bytes)| (path, Some(bytes)))
            .collect(),
    )?;
    let candidate = plumbing::commit_tree(repo, &tree, &[head], "Archive merge exploration")?;
    let proof = archive_history::verify_edge(repo, head, &candidate)?;
    ensure!(
        proof.appended_records() == count,
        "archive candidate changes the captured record count"
    );
    Ok((candidate, tree))
}

fn encode_facts(value: &JsonFacts, out: &mut String) -> Result<()> {
    value.write_json(out)
}

fn publish_prepared(
    repo: &Repo,
    guard: &ArchiveJournalGuard,
    journal: &ArchiveJournal,
    selected: &ArchiveLinkSnapshot,
    checkpoint: &mut impl FnMut(Checkpoint) -> Result<()>,
) -> Result<TailOutcome> {
    let publication = journal
        .publication
        .as_ref()
        .context("archive publication is missing")?;
    ensure!(
        publication.kind == ArchivePublicationKind::Tail && publication.link_json == selected.json,
        "archive publication has different runtime authority"
    );
    let role = &journal.binding.role;
    archive_history::verify_archive_append_target(
        repo,
        journal
            .accepted_commit
            .as_deref()
            .context("archive accepted publication is missing")?,
        &publication.expected_old,
    )?;
    let proof =
        archive_history::verify_edge(repo, &publication.expected_old, &publication.candidate)?;
    ensure!(
        proof.session() == role.logical_session
            && u64::try_from(proof.appended_records())? == publication.appended_records,
        "archive publication does not match its retained identity or record count"
    );
    ensure!(
        repo.git(&[
            "rev-parse",
            "--verify",
            &format!("{}^{{tree}}", publication.candidate)
        ])? == publication.candidate_tree,
        "archive candidate tree differs from its publication receipt"
    );
    let log = storage::materialize_at(repo.root(), &publication.candidate, meta::LOG_FILE)?;
    let old_count = log
        .lines()
        .count()
        .checked_sub(proof.appended_records())
        .context("archive suffix lies outside its LOG")?;
    let offset: usize = log
        .split_inclusive('\n')
        .take(old_count)
        .map(str::len)
        .sum();
    let suffix = &log[offset..];
    ensure!(
        hex::encode(Sha256::digest(suffix.as_bytes())) == publication.protected_suffix_sha256,
        "archive retained evidence digest differs"
    );
    for line in suffix.split_inclusive('\n') {
        let envelope = storage::parse_envelope_line(line)?;
        ensure!(
            envelope.source == journal.binding.native.runtime
                && envelope.session_id == role.logical_session,
            "archive retained evidence belongs to another runtime or logical session"
        );
    }
    let head = current_head(repo, &role.branch)?;
    if head == publication.expected_old {
        checkpoint(Checkpoint::BeforeCas)?;
        require_destination_routing(repo, &role.branch)?;
        plumbing::update_branch_cas_and_refresh(
            repo,
            &role.branch,
            &publication.candidate,
            &publication.expected_old,
            false,
        )?;
        checkpoint(Checkpoint::AfterCas)?;
    } else {
        ensure!(
            head == publication.candidate,
            "archive target moved outside the retained publication endpoints"
        );
    }
    ensure!(
        current_head(repo, &role.branch)? == publication.candidate,
        "archive target moved before cursor publication"
    );
    let mut completed = journal.clone();
    completed.accepted_commit = Some(publication.candidate.clone());
    completed.consumed = publication.next_frontier.clone();
    completed.opencode = publication.next_opencode.clone();
    completed.publication = None;
    guard.replace(journal, &completed)?;
    Ok(TailOutcome::Published {
        commit: publication.candidate.clone(),
        records: publication.appended_records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::merge_archive::{ExplorationBinding, FrozenMergeSource};
    use crate::domain::native_archive::Frontier;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use zeroize::Zeroizing;

    const SESSION: &str = "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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
        store: Store,
        role: MergeArchiveRole,
        native: RuntimeLinkKey,
        native_path: PathBuf,
        dictionary: RepositoryDictionary<MemoryKeys>,
        original: String,
        installed: Frontier,
        landing: String,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(&directory.path().join("repo")).unwrap();
            repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
            let original = transcript::wrap_lines(
                "{\"type\":\"user\",\"text\":\"inherited\"}\n",
                "codex",
                SESSION,
            );
            let mut metadata = meta::Meta::new(SESSION.into(), "codex".into(), "/work".into());
            metadata.turn = Some(1);
            meta::write(repo.root(), &metadata).unwrap();
            storage::write_snapshot(repo.root(), &original, &original).unwrap();
            std::fs::write(repo.root().join("AGENTS.md"), "Shared instructions\n").unwrap();
            repo.add_all().unwrap();
            repo.commit("archive source fixture").unwrap();
            let origin = repo.git(&["rev-parse", "HEAD"]).unwrap();
            let tree = repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
            let source =
                plumbing::commit_tree(&repo, &tree, &[&origin], "merge source fixture").unwrap();
            metadata.kind = meta::Kind::Merge;
            let text = meta::to_text(&metadata).unwrap();
            let text = format!(
                "{},\"future\":{{\"number\":9007199254740993.0,\"escaped\":\"a\\nb\"}}}}\n",
                text.trim().strip_suffix('}').unwrap()
            );
            let tree = plumbing::tree_apply_owned(
                &repo,
                &origin,
                vec![(meta::FILE.into(), Some(text.into_bytes()))],
            )
            .unwrap();
            let landing =
                plumbing::commit_tree(&repo, &tree, &[&origin, &source], "merge landing fixture")
                    .unwrap();
            repo.git(&["update-ref", "refs/heads/work", &landing])
                .unwrap();
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/work"])
                .unwrap();
            repo.git(&["read-tree", "--reset", "-u", &landing]).unwrap();
            let role = MergeArchiveRole {
                generation: uuid::Uuid::now_v7().to_string(),
                slug: "alice/photo".into(),
                branch: "work".into(),
                origin_head: origin.clone(),
                logical_session: SESSION.into(),
            };
            let native = RuntimeLinkKey {
                runtime: "codex".into(),
                session_id: "ARCHIVE_NATIVE".into(),
            };
            let baseline = b"{\"type\":\"session_meta\",\"id\":\"ARCHIVE_NATIVE\"}\n";
            let installed = Frontier {
                bytes: baseline.len() as u64,
                sha256: hex::encode(Sha256::digest(baseline)),
            };
            let native_path = directory.path().join("native.jsonl");
            std::fs::write(&native_path, baseline).unwrap();
            let store = Store::at(directory.path().join("store"));
            let mut link = Link::new(&native.runtime, &native.session_id, Some(directory.path()));
            link.owner = Some("alice".into());
            link.agent = Some("photo".into());
            link.branch = Some("work".into());
            link.baseline_bytes = Some(installed.bytes);
            link.baseline_hash = Some(installed.sha256.clone());
            link.materialized_from = Some(origin.clone());
            link.merge_archive = Some(role.clone());
            let image = link.to_json().unwrap();
            {
                let _lock = link::lock(&store, &native.runtime, &native.session_id).unwrap();
                link::publish_archive_transition_locked(
                    &store,
                    &native.runtime,
                    &native.session_id,
                    None,
                    &image,
                )
                .unwrap();
            }
            let mut preparing = ArchiveJournal {
                version: crate::domain::merge_archive::VERSION,
                binding: ExplorationBinding {
                    role: role.clone(),
                    native: native.clone(),
                    installed: installed.clone(),
                    source: FrozenMergeSource {
                        reference: "alice/photo@source".into(),
                        slug: "alice/photo".into(),
                        branch: Some("source".into()),
                        head: source.clone(),
                        base: Some(origin.clone()),
                    },
                },
                phase: ArchivePhase::Preparing,
                consumed: installed.clone(),
                opencode: None,
                accepted_commit: None,
                publication: None,
                landing: None,
                abort: None,
                previous_claims: vec![],
                activation: None,
                detach: None,
            };
            preparing.activation = Some(crate::domain::merge_archive::test_activation(
                &preparing.binding,
                image.clone(),
            ));
            {
                let guard = ArchiveJournalGuard::acquire(repo.root(), &role.generation).unwrap();
                guard.create(&preparing).unwrap();
                let mut open = preparing.clone();
                open.phase = ArchivePhase::Open;
                open.activation = None;
                guard.replace(&preparing, &open).unwrap();
                let mut pending = open.clone();
                pending.publication = Some(PreparedArchivePublication {
                    kind: ArchivePublicationKind::MergeLanding {
                        source_head: source,
                    },
                    link_json: image,
                    expected_old: origin,
                    candidate: landing.clone(),
                    candidate_tree: tree,
                    prior_frontier: installed.clone(),
                    next_frontier: installed.clone(),
                    next_opencode: None,
                    appended_records: 0,
                    protected_suffix_sha256: hex::encode(Sha256::digest([])),
                });
                guard.replace(&open, &pending).unwrap();
                let mut landed = pending.clone();
                landed.phase = ArchivePhase::Landed {
                    merge_commit: landing.clone(),
                };
                landed.publication = None;
                landed.accepted_commit = Some(landing.clone());
                guard.replace(&pending, &landed).unwrap();
            }
            let dictionary = RepositoryDictionary::new(
                repo.git_path("fixture-dictionary.json").unwrap(),
                MemoryKeys::default(),
            );
            Self {
                _directory: directory,
                repo,
                store,
                role,
                native,
                native_path,
                dictionary,
                original,
                installed,
                landing,
            }
        }

        fn destination(&self) -> TailDestination<'_> {
            TailDestination {
                repo: &self.repo,
                store: &self.store,
                role: &self.role,
                native: &self.native,
            }
        }
        fn append(&self, suffix: &[u8]) {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.native_path)
                .unwrap()
                .write_all(suffix)
                .unwrap();
        }
        fn settle(&self) -> Result<TailOutcome> {
            self.settle_at(&Matcher::empty(), |_| Ok(()))
        }
        fn settle_at(
            &self,
            global: &Matcher,
            checkpoint: impl FnMut(Checkpoint) -> Result<()>,
        ) -> Result<TailOutcome> {
            settle_tail_with(
                self.destination(),
                &self.dictionary,
                global,
                |_| Ok(std::fs::read(&self.native_path)?),
                checkpoint,
            )
        }
        fn journal(&self) -> ArchiveJournal {
            crate::domain::merge_archive::read(self.repo.root(), &self.role.generation)
                .unwrap()
                .unwrap()
        }
        fn link_image(&self) -> Vec<u8> {
            std::fs::read(link::link_path(
                &self.store,
                &self.native.runtime,
                &self.native.session_id,
            ))
            .unwrap()
        }
        fn ordinary_turn(&self) -> String {
            let head = current_head(&self.repo, "work").unwrap();
            let old_log = storage::materialize_at(self.repo.root(), &head, meta::LOG_FILE).unwrap();
            let new = transcript::wrap_lines(
                "{\"type\":\"user\",\"text\":\"ordinary continuation\"}\n",
                "claude-code",
                SESSION,
            );
            let mut metadata = meta::read_at_ref_result(&self.repo, &head)
                .unwrap()
                .unwrap();
            metadata.kind = meta::Kind::Turn;
            metadata.runtime = "claude-code".into();
            metadata.turn = Some(2);
            let tree = plumbing::session_snapshot_tree(
                &self.repo,
                &head,
                &format!("{old_log}{new}"),
                &new,
                &meta::to_text(&metadata).unwrap(),
            )
            .unwrap();
            let next =
                plumbing::commit_tree(&self.repo, &tree, &[&head], "ordinary continuation fixture")
                    .unwrap();
            plumbing::update_branch_cas_and_refresh(&self.repo, "work", &next, &head, false)
                .unwrap();
            next
        }
    }

    #[test]
    fn tail_publishes_only_log_and_preserves_native_link_and_exact_metadata() {
        let fixture = Fixture::new();
        let inherited_view = fixture
            .repo
            .show_raw(&fixture.landing, meta::VIEW_FILE)
            .unwrap();
        let raw_metadata = fixture.repo.show_raw(&fixture.landing, meta::FILE).unwrap();
        let image = fixture.link_image();
        let line = b"{\"type\":\"event_msg\",\"payload\":{\"text\":\"late answer\"}}\n";
        fixture.append(line);
        fixture.append(line);
        fixture.append(b"{\"unfinished\":");
        let native = std::fs::read(&fixture.native_path).unwrap();
        let TailOutcome::Published { commit, records } = fixture.settle().unwrap() else {
            panic!("expected publication")
        };
        assert_eq!(records, 2);
        let proof = archive_history::verify_edge(&fixture.repo, &fixture.landing, &commit).unwrap();
        assert_eq!(proof.appended_records(), 2);
        assert_eq!(
            fixture.repo.show_raw(&commit, meta::VIEW_FILE).unwrap(),
            inherited_view
        );
        assert_eq!(
            fixture.repo.show_raw(&commit, "AGENTS.md").unwrap(),
            "Shared instructions\n"
        );
        let raw = fixture.repo.show_raw(&commit, meta::FILE).unwrap();
        let JsonFacts::Object(mut old) = JsonFacts::parse(&raw_metadata).unwrap() else {
            unreachable!()
        };
        let JsonFacts::Object(mut new) = JsonFacts::parse(&raw).unwrap() else {
            unreachable!()
        };
        old.remove("kind");
        new.remove("kind");
        assert_eq!(old, new);
        assert!(raw.contains("9007199254740993.0"));
        assert!(!raw.contains(&fixture.native.session_id));
        let log = storage::materialize_at(fixture.repo.root(), &commit, meta::LOG_FILE).unwrap();
        let expected = transcript::wrap_lines(
            std::str::from_utf8(line).unwrap(),
            &fixture.native.runtime,
            SESSION,
        );
        assert_eq!(log, format!("{}{}{}", fixture.original, expected, expected));
        assert_eq!(fixture.link_image(), image);
        assert_eq!(std::fs::read(&fixture.native_path).unwrap(), native);
        assert_eq!(
            fixture.journal().consumed.bytes,
            fixture.installed.bytes + 2 * line.len() as u64
        );
        assert_eq!(
            fixture.settle().unwrap(),
            TailOutcome::Noop {
                pending_bytes: b"{\"unfinished\":".len()
            }
        );
        assert_eq!(current_head(&fixture.repo, "work").unwrap(), commit);
    }

    #[test]
    fn late_tail_retains_ordinary_turn_context_and_runtime() {
        let fixture = Fixture::new();
        fixture.append(b"{\"event\":\"first archive\"}\n");
        fixture.settle().unwrap();
        let ordinary = fixture.ordinary_turn();
        let view = fixture.repo.show_raw(&ordinary, meta::VIEW_FILE).unwrap();
        fixture.append(b"{\"event\":\"later archive\"}\n");
        let TailOutcome::Published { commit, records: 1 } = fixture.settle().unwrap() else {
            panic!("expected late publication")
        };
        archive_history::verify_edge(&fixture.repo, &ordinary, &commit).unwrap();
        let metadata = meta::read_at_ref_result(&fixture.repo, &commit)
            .unwrap()
            .unwrap();
        assert_eq!(metadata.runtime, "claude-code");
        assert_eq!(metadata.turn, Some(2));
        assert_eq!(
            fixture.repo.show_raw(&commit, meta::VIEW_FILE).unwrap(),
            view
        );
        assert_eq!(
            fixture.journal().accepted_commit.as_deref(),
            Some(commit.as_str())
        );
    }

    #[test]
    fn malformed_completed_records_and_rewritten_prefixes_never_prepare_a_publication() {
        for suffix in [
            b"{broken}\n".as_slice(),
            b"[]\n",
            b"{\"x\":1,\"x\":2}\n",
            b"\xff\n",
        ] {
            let fixture = Fixture::new();
            let journal = fixture.journal();
            fixture.append(suffix);
            assert!(fixture.settle().is_err());
            assert_eq!(fixture.journal(), journal);
            assert_eq!(
                current_head(&fixture.repo, "work").unwrap(),
                fixture.landing
            );
        }
        let fixture = Fixture::new();
        fixture.append(b"{\"x\":1}\n");
        fixture.settle().unwrap();
        let journal = fixture.journal();
        let mut bytes = std::fs::read(&fixture.native_path).unwrap();
        bytes[fixture.installed.bytes as usize + 2] = b'y';
        std::fs::write(&fixture.native_path, &bytes).unwrap();
        assert!(fixture.settle().is_err());
        assert_eq!(fixture.journal(), journal);
        bytes[2] = b'z';
        std::fs::write(&fixture.native_path, bytes).unwrap();
        assert!(fixture.settle().is_err());
        assert_eq!(fixture.journal(), journal);
    }

    #[test]
    fn prepared_retry_reuses_exact_candidate_before_and_after_ref_publication() {
        for stop in [Checkpoint::Prepared, Checkpoint::AfterCas] {
            let fixture = Fixture::new();
            fixture.append(b"{\"event\":\"captured\"}\n");
            assert!(
                fixture
                    .settle_at(&Matcher::empty(), |at| if at == stop {
                        anyhow::bail!("injected interruption")
                    } else {
                        Ok(())
                    })
                    .is_err()
            );
            let pending = fixture.journal();
            let publication = pending.publication.as_ref().unwrap();
            assert_eq!(pending.consumed, fixture.installed);
            assert_eq!(
                current_head(&fixture.repo, "work").unwrap(),
                if stop == Checkpoint::Prepared {
                    fixture.landing.clone()
                } else {
                    publication.candidate.clone()
                }
            );
            std::fs::remove_file(&fixture.native_path).unwrap();
            let outcome = fixture
                .settle_at(&Matcher::for_test(&[("new-rule", "captured")]), |_| Ok(()))
                .unwrap();
            assert_eq!(
                outcome,
                TailOutcome::Published {
                    commit: publication.candidate.clone(),
                    records: 1
                }
            );
            assert_eq!(fixture.journal().consumed, publication.next_frontier);
            assert!(fixture.journal().publication.is_none());
        }
    }

    #[test]
    fn final_capture_recovers_pending_tail_and_consumes_later_complete_records() {
        for stop in [Checkpoint::Prepared, Checkpoint::AfterCas] {
            let fixture = Fixture::new();
            fixture.append(b"{\"event\":\"retained\"}\n");
            assert!(
                fixture
                    .settle_at(&Matcher::empty(), |at| {
                        if at == stop {
                            anyhow::bail!("injected publication interruption");
                        }
                        Ok(())
                    })
                    .is_err()
            );
            let retained = fixture.journal().publication.unwrap().candidate;
            fixture.append(b"{\"event\":\"last child record\"}\n");
            let binding = fixture.journal().binding;
            let finish = || {
                settle_tail_mode(
                    fixture.destination(),
                    &fixture.dictionary,
                    &Matcher::empty(),
                    |_| Ok(std::fs::read(&fixture.native_path)?),
                    |_| Ok(()),
                    Some(&binding),
                )
            };
            let TailOutcome::Published { commit, records: 1 } = finish().unwrap() else {
                panic!("the final suffix was not captured after pending publication");
            };
            assert_eq!(
                fixture
                    .repo
                    .git(&["rev-parse", &format!("{commit}^1")])
                    .unwrap()
                    .trim(),
                retained
            );
            let log =
                storage::materialize_at(fixture.repo.root(), &commit, meta::LOG_FILE).unwrap();
            assert_eq!(log.matches("retained").count(), 1);
            assert_eq!(log.matches("last child record").count(), 1);
            assert_eq!(
                fixture.journal().consumed.bytes,
                std::fs::metadata(&fixture.native_path).unwrap().len()
            );
            assert_eq!(finish().unwrap(), TailOutcome::Noop { pending_bytes: 0 });
            assert_eq!(current_head(&fixture.repo, "work").unwrap(), commit);
        }
    }

    #[test]
    fn final_capture_rejects_changed_binding_before_native_read_or_publication() {
        let fixture = Fixture::new();
        fixture.append(b"{\"event\":\"last record\"}\n");
        let before = fixture.journal();
        let mut binding = before.binding.clone();
        binding.source.reference = "alice/elsewhere@source".into();
        let error = settle_tail_mode(
            fixture.destination(),
            &fixture.dictionary,
            &Matcher::empty(),
            |_| panic!("binding refusal must precede native capture"),
            |_| Ok(()),
            Some(&binding),
        )
        .unwrap_err();
        assert!(error.to_string().contains("launched binding"));
        assert_eq!(fixture.journal(), before);
        assert_eq!(
            current_head(&fixture.repo, "work").unwrap(),
            fixture.landing
        );
    }

    #[test]
    fn final_capture_keeps_partial_record_without_acknowledging_completion() {
        let fixture = Fixture::new();
        fixture.append(b"{\"event\":\"complete\"}\n{\"event\":\"partial\"}");
        let binding = fixture.journal().binding;
        let finish = || {
            settle_tail_mode(
                fixture.destination(),
                &fixture.dictionary,
                &Matcher::empty(),
                |_| Ok(std::fs::read(&fixture.native_path)?),
                |_| Ok(()),
                Some(&binding),
            )
        };
        assert!(
            finish()
                .unwrap_err()
                .to_string()
                .contains("incomplete native record")
        );
        let captured = current_head(&fixture.repo, "work").unwrap();
        let log = storage::materialize_at(fixture.repo.root(), &captured, meta::LOG_FILE).unwrap();
        assert_eq!(log.matches("complete").count(), 1);
        assert!(!log.contains("partial"));
        assert!(finish().is_err());
        assert_eq!(current_head(&fixture.repo, "work").unwrap(), captured);
        fixture.append(b"\n");
        assert!(matches!(
            finish().unwrap(),
            TailOutcome::Published { records: 1, .. }
        ));
        let head = current_head(&fixture.repo, "work").unwrap();
        let log = storage::materialize_at(fixture.repo.root(), &head, meta::LOG_FILE).unwrap();
        assert_eq!(log.matches("complete").count(), 1);
        assert_eq!(log.matches("partial").count(), 1);
    }

    #[test]
    fn native_growth_after_preparation_waits_for_a_distinct_tail() {
        let fixture = Fixture::new();
        fixture.append(b"{\"event\":\"captured\"}\n");
        assert!(
            fixture
                .settle_at(&Matcher::empty(), |at| {
                    if at == Checkpoint::Prepared {
                        anyhow::bail!("prepared stop")
                    }
                    Ok(())
                })
                .is_err()
        );
        let first = fixture.journal().publication.unwrap();
        fixture.append(b"{\"event\":\"arrived later\"}\n");
        assert_eq!(
            fixture.settle().unwrap(),
            TailOutcome::Published {
                commit: first.candidate.clone(),
                records: 1
            }
        );
        assert_eq!(fixture.journal().consumed, first.next_frontier);
        let TailOutcome::Published { commit, records: 1 } = fixture.settle().unwrap() else {
            panic!("expected another tail")
        };
        assert_ne!(commit, first.candidate);
        archive_history::verify_edge(&fixture.repo, &first.candidate, &commit).unwrap();
        let log = storage::materialize_at(fixture.repo.root(), &commit, meta::LOG_FILE).unwrap();
        assert_eq!(log.matches("captured").count(), 1);
        assert_eq!(log.matches("arrived later").count(), 1);
    }

    #[test]
    fn revoked_link_authority_and_active_transaction_cannot_capture_native_bytes() {
        for field in ["superseded_by", "branch", "baseline_hash"] {
            let fixture = Fixture::new();
            let journal = fixture.journal();
            let path = link::link_path(
                &fixture.store,
                &fixture.native.runtime,
                &fixture.native.session_id,
            );
            let mut image: serde_json::Value =
                serde_json::from_slice(&fixture.link_image()).unwrap();
            image[field] = serde_json::json!(if field == "baseline_hash" {
                "0".repeat(64)
            } else {
                "other".into()
            });
            std::fs::write(path, serde_json::to_vec(&image).unwrap()).unwrap();
            let result = settle_tail_with(
                fixture.destination(),
                &fixture.dictionary,
                &Matcher::empty(),
                |_| panic!("revoked authority read native bytes"),
                |_| Ok(()),
            );
            assert!(result.is_err());
            assert_eq!(fixture.journal(), journal);
            assert_eq!(
                current_head(&fixture.repo, "work").unwrap(),
                fixture.landing
            );
        }
        let fixture = Fixture::new();
        let journal = fixture.journal();
        mergetx::create(
            fixture.repo.root(),
            &mergetx::Tx {
                mode: None,
                exploration: None,
                generation: Some(uuid::Uuid::now_v7().to_string()),
                target: "work".into(),
                source: "alice/photo@source".into(),
                source_repo: Some("alice/photo".into()),
                source_branch: Some("source".into()),
                base: fixture.role.origin_head.clone(),
                target_head: fixture.landing.clone(),
                source_head: fixture.role.origin_head.clone(),
                picked: vec![],
                summary: None,
            },
        )
        .unwrap();
        assert!(
            settle_tail_with(
                fixture.destination(),
                &fixture.dictionary,
                &Matcher::empty(),
                |_| panic!("active transaction read native bytes"),
                |_| Ok(())
            )
            .is_err()
        );
        assert_eq!(fixture.journal(), journal);
        assert!(mergetx::read(fixture.repo.root()).unwrap().is_some());
    }

    #[test]
    fn competing_ref_or_changed_link_retains_pending_authority_and_frontier() {
        for change in ["ref", "link"] {
            let fixture = Fixture::new();
            fixture.append(b"{\"event\":\"pending\"}\n");
            assert!(
                fixture
                    .settle_at(&Matcher::empty(), |at| if at == Checkpoint::Prepared {
                        anyhow::bail!("prepared stop")
                    } else {
                        Ok(())
                    })
                    .is_err()
            );
            let pending = fixture.journal();
            if change == "ref" {
                fixture.ordinary_turn();
            } else {
                let path = link::link_path(
                    &fixture.store,
                    &fixture.native.runtime,
                    &fixture.native.session_id,
                );
                let image = String::from_utf8(fixture.link_image()).unwrap();
                std::fs::write(path, format!("{} ", image)).unwrap();
            }
            let head = current_head(&fixture.repo, "work").unwrap();
            assert!(fixture.settle().is_err());
            assert_eq!(fixture.journal(), pending);
            assert_eq!(current_head(&fixture.repo, "work").unwrap(), head);
        }
    }

    #[test]
    fn checkout_refusal_does_not_consume_prepared_native_bytes() {
        let fixture = Fixture::new();
        fixture.append(b"{\"event\":\"pending\"}\n");
        let log_path = fixture.repo.root().join(meta::LOG_FILE);
        let original = std::fs::read(&log_path).unwrap();
        assert!(
            fixture
                .settle_at(&Matcher::empty(), |at| {
                    if at == Checkpoint::BeforeCas {
                        std::fs::write(&log_path, b"uncommitted user bytes\n")?;
                    }
                    Ok(())
                })
                .is_err()
        );
        let pending = fixture.journal();
        assert!(pending.publication.is_some());
        assert_eq!(pending.consumed, fixture.installed);
        assert_eq!(
            current_head(&fixture.repo, "work").unwrap(),
            fixture.landing
        );
        assert_eq!(
            std::fs::read(&log_path).unwrap(),
            b"uncommitted user bytes\n"
        );
        std::fs::write(log_path, original).unwrap();
        fixture.settle().unwrap();
        assert!(fixture.journal().publication.is_none());
    }

    #[test]
    fn projection_precedes_public_objects_and_noop_preserves_frontier() {
        let fixture = Fixture::new();
        let journal = fixture.journal();
        assert_eq!(
            fixture.settle().unwrap(),
            TailOutcome::Noop { pending_bytes: 0 }
        );
        assert_eq!(fixture.journal(), journal);
        fixture.append(b"{\"text\":\"blue horse battery\"}\n");
        let TailOutcome::Published { commit, .. } = fixture
            .settle_at(
                &Matcher::for_test(&[("secret", "blue horse battery")]),
                |_| Ok(()),
            )
            .unwrap()
        else {
            panic!("expected publication")
        };
        let log = storage::materialize_at(fixture.repo.root(), &commit, meta::LOG_FILE).unwrap();
        assert!(!log.contains("blue horse battery"));
        assert!(log.contains("AGIT_SECRET_V1"));
        assert!(
            fixture
                .dictionary
                .hydrate_jsonl(&log)
                .unwrap()
                .text
                .contains("blue horse battery")
        );
        assert!(
            String::from_utf8(std::fs::read(&fixture.native_path).unwrap())
                .unwrap()
                .contains("blue horse battery")
        );
    }

    #[test]
    fn inherited_git_routing_refuses_before_any_repository_or_authority_effect() {
        const CHILD: &str = "AGIT_ARCHIVE_ROUTING_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let input: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let repo = Repo::at(input["repo"].as_str().unwrap());
            let store = Store::at(input["store"].as_str().unwrap());
            let role: MergeArchiveRole = serde_json::from_value(input["role"].clone()).unwrap();
            let native: RuntimeLinkKey = serde_json::from_value(input["native"].clone()).unwrap();
            let dictionary = RepositoryDictionary::new(
                repo.root().join("unused-dictionary.json"),
                MemoryKeys::default(),
            );
            let result = settle_tail_with(
                TailDestination {
                    repo: &repo,
                    store: &store,
                    role: &role,
                    native: &native,
                },
                &dictionary,
                &Matcher::empty(),
                |_| panic!("routing override reached native capture"),
                |_| panic!("routing override reached publication"),
            );
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("inherited Git routing override")
            );
            return;
        }
        let fixture = Fixture::new();
        let decoy = Fixture::new();
        std::fs::write(
            decoy.repo.root().join("decoy.txt"),
            "Different destination\n",
        )
        .unwrap();
        decoy.repo.add_all().unwrap();
        decoy.repo.commit("decoy routing fixture").unwrap();
        let real_head = current_head(&fixture.repo, "work").unwrap();
        let decoy_head = current_head(&decoy.repo, "work").unwrap();
        assert_ne!(real_head, decoy_head);
        let redirected = std::process::Command::new("git")
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(fixture.repo.root())
            .args(["rev-parse", "HEAD"])
            .env("GIT_DIR", decoy.repo.root().join(".git"))
            .output()
            .unwrap();
        assert!(redirected.status.success());
        assert_eq!(
            String::from_utf8(redirected.stdout).unwrap().trim(),
            decoy_head
        );
        let input = fixture._directory.path().join("routing-child.json");
        std::fs::write(&input, serde_json::to_vec(&serde_json::json!({"repo":fixture.repo.root(),"store":fixture.store.root(),"role":fixture.role,"native":fixture.native})).unwrap()).unwrap();
        let state = |f: &Fixture| {
            let dir = f
                .repo
                .root()
                .join(".git")
                .join(crate::domain::merge_archive::DIRECTORY);
            (
                f.repo.git(&["show-ref"]).unwrap(),
                f.link_image(),
                std::fs::read(dir.join(format!("{}.json", f.role.generation))).unwrap(),
                std::fs::read(dir.join(format!("{}.recovery", f.role.generation))).unwrap(),
                std::fs::read(f.repo.git_path("index").unwrap()).unwrap(),
                std::fs::read(f.repo.git_path("config").unwrap()).unwrap(),
            )
        };
        let before = (state(&fixture), state(&decoy));
        for name in archive_history::GIT_ROUTING_ENV {
            let output = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", "commands::merge::archive::tests::inherited_git_routing_refuses_before_any_repository_or_authority_effect", "--nocapture"]).env(CHILD, &input).env(name, decoy.repo.root().join(".git")).output().unwrap();
            assert!(
                output.status.success(),
                "child failed for {name}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            assert_eq!((state(&fixture), state(&decoy)), before);
        }
    }

    #[test]
    fn configured_worktree_redirect_refuses_without_mutating_either_checkout() {
        let fixture = Fixture::new();
        let decoy = Fixture::new();
        fixture
            .repo
            .git(&[
                "config",
                "core.worktree",
                decoy.repo.root().to_str().unwrap(),
            ])
            .unwrap();
        assert_eq!(
            PathBuf::from(fixture.repo.git(&["rev-parse", "--show-toplevel"]).unwrap())
                .canonicalize()
                .unwrap(),
            decoy.repo.root().canonicalize().unwrap()
        );
        let state = |f: &Fixture| {
            let dir = f
                .repo
                .root()
                .join(".git")
                .join(crate::domain::merge_archive::DIRECTORY);
            (
                f.repo.git(&["show-ref"]).unwrap(),
                f.link_image(),
                std::fs::read(dir.join(format!("{}.json", f.role.generation))).unwrap(),
                std::fs::read(dir.join(format!("{}.recovery", f.role.generation))).unwrap(),
                std::fs::read(f.repo.root().join(meta::LOG_FILE)).unwrap(),
                std::fs::read(f.repo.root().join(meta::FILE)).unwrap(),
            )
        };
        let before = (state(&fixture), state(&decoy));
        let error = settle_tail_with(
            fixture.destination(),
            &fixture.dictionary,
            &Matcher::empty(),
            |_| panic!("worktree redirect reached native capture"),
            |_| panic!("worktree redirect reached publication"),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("worktree differs from its explicit destination")
        );
        assert_eq!((state(&fixture), state(&decoy)), before);
    }

    #[test]
    fn linked_branch_holder_is_required_for_checkout_aware_publication() {
        assert_linked_holder_publication(std::path::Path::new("linked branch"), false);
    }

    #[cfg(unix)]
    #[test]
    fn unusual_holder_paths_cannot_hide_a_checked_out_target() {
        assert_linked_holder_publication(std::path::Path::new("linked\n\nbranch"), true);
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_worktree_records_preserve_native_path_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let path = b"/tmp/raw-\xff\n\nparent/linked";
        let mut listing = b"worktree ".to_vec();
        listing.extend_from_slice(path);
        listing.extend_from_slice(b"\0HEAD aaaaaaaa\0branch refs/heads/work\0\0");
        assert_eq!(
            branch_holders(&listing, "work").unwrap(),
            vec![PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()))]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_unicode_holder_paths_support_checkout_aware_publication() {
        use std::os::unix::ffi::OsStringExt;
        let raw_parent = PathBuf::from(std::ffi::OsString::from_vec(b"raw-\xff".to_vec()));
        assert_linked_holder_publication(&raw_parent.join("linked"), true);
    }

    fn assert_linked_holder_publication(relative: &std::path::Path, legacy_misses: bool) {
        let fixture = Fixture::new();
        let linked_path = fixture._directory.path().join(relative);
        std::fs::create_dir_all(linked_path.parent().unwrap()).unwrap();
        fixture
            .repo
            .git(&["symbolic-ref", "HEAD", "refs/heads/main"])
            .unwrap();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(fixture.repo.root())
            .args(["worktree", "add"])
            .arg(&linked_path)
            .arg("work")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let linked = Repo::at(&linked_path);
        let listing = fixture
            .repo
            .git_bytes_result(&["worktree", "list", "--porcelain", "-z"])
            .unwrap();
        assert_eq!(
            branch_holders(&listing, "work")
                .unwrap()
                .into_iter()
                .map(|path| path.canonicalize().unwrap())
                .collect::<Vec<_>>(),
            vec![linked_path.canonicalize().unwrap()]
        );
        if legacy_misses {
            assert!(
                !fixture
                    .repo
                    .worktrees()
                    .unwrap()
                    .iter()
                    .any(|holder| holder.branch.as_deref() == Some("work")
                        && holder.path == linked_path.canonicalize().unwrap())
            );
        }
        let primary_state = (
            std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap(),
            std::fs::read(fixture.repo.root().join(meta::FILE)).unwrap(),
            std::fs::read(fixture.repo.root().join(meta::LOG_FILE)).unwrap(),
        );
        let before = fixture.journal();
        assert!(
            fixture
                .settle()
                .unwrap_err()
                .to_string()
                .contains("different worktree")
        );
        assert_eq!(fixture.journal(), before);
        fixture.append(b"{\"event\":\"linked tail\"}\n");
        let TailOutcome::Published { commit, records: 1 } = settle_tail_with(
            TailDestination {
                repo: &linked,
                store: &fixture.store,
                role: &fixture.role,
                native: &fixture.native,
            },
            &fixture.dictionary,
            &Matcher::empty(),
            |_| Ok(std::fs::read(&fixture.native_path)?),
            |_| Ok(()),
        )
        .unwrap() else {
            panic!("expected linked publication")
        };
        assert_eq!(current_head(&fixture.repo, "work").unwrap(), commit);
        assert_eq!(linked.git(&["status", "--porcelain"]).unwrap(), "");
        assert_eq!(
            std::fs::read(linked.root().join(meta::VIEW_FILE)).unwrap(),
            fixture
                .repo
                .show_raw(&fixture.landing, meta::VIEW_FILE)
                .unwrap()
                .into_bytes()
        );
        assert_eq!(
            fixture.repo.git(&["rev-parse", "HEAD"]).unwrap(),
            fixture.role.origin_head
        );
        assert_eq!(
            (
                std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap(),
                std::fs::read(fixture.repo.root().join(meta::FILE)).unwrap(),
                std::fs::read(fixture.repo.root().join(meta::LOG_FILE)).unwrap()
            ),
            primary_state
        );
    }

    #[test]
    fn worktree_record_framing_refuses_ambiguous_ownership() {
        let path = std::env::temp_dir().join("holder");
        let prefix = format!("worktree {}\0HEAD aaaaaaaa\0", path.display());
        let valid = format!("{prefix}branch refs/heads/work\0\0");
        assert_eq!(
            branch_holders(valid.as_bytes(), "work").unwrap(),
            vec![path]
        );
        for bytes in [
            Vec::new(),
            valid.as_bytes()[..valid.len() - 1].to_vec(),
            format!("{prefix}branch refs/heads/work\0branch refs/heads/other\0\0").into_bytes(),
            format!("{prefix}branch refs/heads/work\0detached\0\0").into_bytes(),
            format!("{prefix}\0").into_bytes(),
            format!("branch refs/heads/work\0{prefix}\0").into_bytes(),
            format!("{valid}\0").into_bytes(),
        ] {
            assert!(branch_holders(&bytes, "work").is_err());
        }
        assert_eq!(
            branch_holders(format!("{valid}{valid}").as_bytes(), "work")
                .unwrap()
                .len(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_worktree_listing_refuses_before_locks_or_writes() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "AGIT_ARCHIVE_CAPABILITY_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let input: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let repo = Repo::at(input["repo"].as_str().unwrap());
            let store = Store::at(input["store"].as_str().unwrap());
            let role: MergeArchiveRole = serde_json::from_value(input["role"].clone()).unwrap();
            let native: RuntimeLinkKey = serde_json::from_value(input["native"].clone()).unwrap();
            let error = settle_tail(
                TailDestination {
                    repo: &repo,
                    store: &store,
                    role: &role,
                    native: &native,
                },
                &Matcher::empty(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("upgrade Git"));
            assert!(format!("{error:#}").contains("unknown switch"));
            return;
        }
        let fixture = Fixture::new();
        let input = fixture._directory.path().join("capability-child.json");
        std::fs::write(&input, serde_json::to_vec(&serde_json::json!({"repo":fixture.repo.root(),"store":fixture.store.root(),"role":fixture.role,"native":fixture.native})).unwrap()).unwrap();
        let bin = fixture._directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let git = bin.join("git");
        std::fs::write(
            &git,
            b"#!/bin/sh\nprintf '%s\\n' 'unknown switch: z' >&2\nexit 129\n",
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).unwrap();
        fn files(path: &std::path::Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
            let mut result = std::collections::BTreeMap::new();
            for entry in std::fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    result.extend(files(&entry.path()));
                } else {
                    result.insert(entry.path(), std::fs::read(entry.path()).unwrap());
                }
            }
            result
        }
        let before = files(fixture._directory.path());
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "commands::merge::archive::tests::unsupported_worktree_listing_refuses_before_locks_or_writes", "--nocapture"])
            .env(CHILD, &input)
            .env("PATH", &bin)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        assert_eq!(files(fixture._directory.path()), before);
    }

    #[test]
    fn routing_or_holder_change_before_cas_retains_the_prepared_frontier() {
        for change in ["worktree", "holder"] {
            let fixture = Fixture::new();
            fixture.append(b"{\"event\":\"pending\"}\n");
            let alternate = fixture._directory.path().join("alternate");
            let image = fixture.link_image();
            assert!(
                fixture
                    .settle_at(&Matcher::empty(), |at| {
                        if at == Checkpoint::BeforeCas {
                            if change == "worktree" {
                                std::fs::create_dir(&alternate)?;
                                fixture.repo.git(&[
                                    "config",
                                    "core.worktree",
                                    alternate.to_str().unwrap(),
                                ])?;
                            } else {
                                fixture
                                    .repo
                                    .git(&["symbolic-ref", "HEAD", "refs/heads/main"])?;
                                fixture.repo.add_worktree(&alternate, "work")?;
                            }
                        }
                        Ok(())
                    })
                    .is_err()
            );
            let pending = fixture.journal();
            assert!(pending.publication.is_some());
            assert_eq!(pending.consumed, fixture.installed);
            assert_eq!(fixture.link_image(), image);
            assert_eq!(
                current_head(&fixture.repo, "work").unwrap(),
                fixture.landing
            );
            if change == "worktree" {
                fixture
                    .repo
                    .git(&["config", "--unset", "core.worktree"])
                    .unwrap();
            } else {
                fixture.repo.remove_worktree(&alternate).unwrap();
                fixture
                    .repo
                    .git(&["symbolic-ref", "HEAD", "refs/heads/work"])
                    .unwrap();
            }
            fixture.settle().unwrap();
            assert_eq!(
                fixture.journal().accepted_commit,
                Some(pending.publication.unwrap().candidate)
            );
        }
    }
}
