//! Explicit, read-only proposals for attaching one selected native transcript.
//!
//! Record equality and retained materialization authority are evidence for an explicit choice.
//! Neither claims that the native transcript has an observed Git parent.

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::{
    link::Link,
    native_archive,
    repo::{GitRecordBudgetExceeded, Repo},
    secret_filter::{HydrationBudgetExceeded, HydrationReport, KeyStore, RepositoryDictionary},
    storage, transcript, turn,
};
use crate::{Result, adapter};
use anyhow::{Context, ensure};

const EMPTY_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub references: usize,
    pub reference_record_bytes: usize,
    pub candidate_reference_label_bytes: usize,
    pub visited_commits: usize,
    pub native_bytes: usize,
    pub snapshot_bytes: usize,
    pub total_input_bytes: usize,
    pub hydrated_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            references: 128,
            reference_record_bytes: 4096,
            candidate_reference_label_bytes: 8 * 1024 * 1024,
            visited_commits: 512,
            native_bytes: 64 * 1024 * 1024,
            snapshot_bytes: 16 * 1024 * 1024,
            total_input_bytes: 128 * 1024 * 1024,
            hydrated_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Evidence {
    ExactNativeRecords,
    VerifiedMaterializedSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub commit: String,
    /// These frozen references reach the candidate; they do not identify its first-parent line.
    pub reachable_from: Vec<String>,
    pub completed_turns: usize,
    pub records: usize,
    pub evidence: Evidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    ReferenceScan,
    CommitWalk,
    ScanBudget,
    NativeRecords,
    StoredEvidence,
    SecretMapping,
    Materialization,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Unavailable {
    pub commit: Option<String>,
    pub reason: UnavailableReason,
}

#[derive(Default, Debug)]
pub struct Proposals {
    pub candidates: Vec<Candidate>,
    pub unavailable: Vec<Unavailable>,
    /// Semantic similarity requires its own validated normalizer and never authorizes raw loss.
    pub semantic_discovery_available: bool,
}

impl Proposals {
    fn unavailable(&mut self, commit: Option<String>, reason: UnavailableReason) {
        let issue = Unavailable { commit, reason };
        if !self.unavailable.contains(&issue) {
            self.unavailable.push(issue);
        }
    }
}

/// The caller supplies the selected repository, full native Link identity and its one byte read.
/// No reference, transcript, claim, dictionary or checkout is changed or selected here.
pub fn discover(
    repo: &Repo,
    slug: &str,
    selected: &Link,
    native: &[u8],
    limits: Limits,
) -> Result<Proposals> {
    validate_selection(repo, slug, selected)?;
    let dictionary = RepositoryDictionary::open(repo.root())?;
    discover_with_dictionary(repo, slug, selected, native, limits, &dictionary)
}

/// Recheck a selected immutable proposal without resolving its former display references again.
pub(crate) fn candidate_still_matches(
    repo: &Repo,
    slug: &str,
    selected: &Link,
    native: &[u8],
    expected: &Candidate,
) -> Result<bool> {
    let dictionary = RepositoryDictionary::open(repo.root())?;
    let result = inspect_with_dictionary(
        repo,
        slug,
        selected,
        native,
        Limits::default(),
        &dictionary,
        Some(expected),
    )?;
    Ok(result
        .candidates
        .iter()
        .any(|candidate| candidate == expected))
}

fn validate_selection(repo: &Repo, slug: &str, selected: &Link) -> Result<()> {
    ensure!(
        Repo::open(repo.root()).is_some(),
        "lineage discovery requires an explicit existing absolute repository"
    );
    let (owner, name) = slug
        .split_once('/')
        .context("lineage discovery requires an explicit owner/repository")?;
    crate::domain::repo::valid_name(owner)?;
    crate::domain::repo::valid_name(name)?;
    ensure!(
        !selected.session_id.is_empty(),
        "lineage discovery requires a selected native session"
    );
    adapter::get(&selected.source)?;
    Ok(())
}

fn discover_with_dictionary<K: KeyStore>(
    repo: &Repo,
    slug: &str,
    selected: &Link,
    native: &[u8],
    limits: Limits,
    dictionary: &RepositoryDictionary<K>,
) -> Result<Proposals> {
    inspect_with_dictionary(repo, slug, selected, native, limits, dictionary, None)
}

fn inspect_with_dictionary<K: KeyStore>(
    repo: &Repo,
    slug: &str,
    selected: &Link,
    native: &[u8],
    limits: Limits,
    dictionary: &RepositoryDictionary<K>,
    frozen: Option<&Candidate>,
) -> Result<Proposals> {
    validate_selection(repo, slug, selected)?;
    let mut proposals = Proposals::default();
    if native.len() > limits.native_bytes || native.len() > limits.total_input_bytes {
        proposals.unavailable(None, UnavailableReason::ScanBudget);
        return Ok(proposals);
    }
    let native_records = match native_archive::capture(native, 0, EMPTY_HASH) {
        Ok(captured) if captured.unconsumed.is_empty() => captured.record_count,
        _ => {
            proposals.unavailable(None, UnavailableReason::NativeRecords);
            return Ok(proposals);
        }
    };
    let candidates = if let Some(expected) = frozen {
        ensure!(
            valid_oid(&expected.commit),
            "the selected candidate is not an immutable commit"
        );
        if expected.reachable_from.len() > limits.references
            || expected
                .reachable_from
                .iter()
                .any(|label| label.len() > limits.reference_record_bytes)
            || expected
                .reachable_from
                .iter()
                .try_fold(0usize, |bytes, label| bytes.checked_add(label.len()))
                .is_none_or(|bytes| bytes > limits.candidate_reference_label_bytes)
        {
            proposals.unavailable(None, UnavailableReason::ScanBudget);
            return Ok(proposals);
        }
        let (status, commit, _) = repo.git_status_local(&[
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", expected.commit),
        ])?;
        ensure!(
            status == Some(0) && commit == expected.commit,
            "the selected candidate is no longer readable"
        );
        BTreeMap::from([(
            expected.commit.clone(),
            expected.reachable_from.iter().cloned().collect(),
        )])
    } else {
        discover_commits(repo, limits, &mut proposals)
    };
    let retained_materialization = has_materialization(selected);
    if retained_materialization
        && !selected
            .materialized_from
            .as_ref()
            .is_some_and(|source| candidates.contains_key(source))
    {
        proposals.unavailable(None, UnavailableReason::Materialization);
    }
    let mut remaining = limits.total_input_bytes - native.len();
    let mut snapshots = Vec::new();
    for (commit, labels) in candidates {
        if retained_materialization
            && selected.materialized_from.as_deref() != Some(commit.as_str())
        {
            continue;
        }
        let snapshot = match storage::metadata_local(repo.root(), &commit) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                proposals.unavailable(Some(commit), storage_failure(&error));
                continue;
            }
        };
        if snapshot.is_file_line() || snapshot.session.is_empty() {
            continue;
        }
        if remaining < 2 || limits.snapshot_bytes == 0 {
            proposals.unavailable(None, UnavailableReason::ScanBudget);
            break;
        }
        let cap = limits.snapshot_bytes.min(remaining / 2);
        remaining -= cap * 2;
        let (log, view) = match storage::materialize_pair_local(repo.root(), &commit, cap, cap) {
            Ok(pair) => pair,
            Err(error) => {
                proposals.unavailable(Some(commit), storage_failure(&error));
                continue;
            }
        };
        remaining += cap * 2 - log.len() - view.len();
        match Snapshot::from_log(commit.clone(), labels, &log, selected) {
            Ok(snapshot) => snapshots.push(snapshot),
            Err(reason) => proposals.unavailable(Some(commit), reason),
        }
    }
    let live = std::str::from_utf8(native).context("validated native transcript is not UTF-8")?;
    let inputs: Vec<_> = std::iter::once(live)
        .chain(snapshots.iter().map(|snapshot| snapshot.raw.as_str()))
        .collect();
    let reports = match dictionary.hydrate_batch_readonly_bounded(&inputs, limits.hydrated_bytes) {
        Ok(reports) => reports,
        Err(error) => {
            proposals.unavailable(None, hydration_failure(&error));
            return Ok(proposals);
        }
    };
    let mut reports = reports.into_iter();
    let live = match reports
        .next()
        .context("hydration omitted its native input")?
    {
        Ok(live) => live,
        Err(error) => {
            proposals.unavailable(None, hydration_failure(&error));
            return Ok(proposals);
        }
    };
    if live.unresolved != 0 {
        proposals.unavailable(None, UnavailableReason::SecretMapping);
        return Ok(proposals);
    }
    let live_hashes = transcript::live_hashes(&live.text);
    if live_hashes.len() != native_records {
        proposals.unavailable(None, UnavailableReason::NativeRecords);
        return Ok(proposals);
    }
    for (snapshot, stored) in snapshots.into_iter().zip(reports) {
        let stored = match stored {
            Ok(stored) => stored,
            Err(error) => {
                proposals.unavailable(Some(snapshot.commit), hydration_failure(&error));
                continue;
            }
        };
        match classify(
            &snapshot,
            &stored,
            selected,
            slug,
            native,
            &live.text,
            &live_hashes,
        ) {
            Ok(Some(candidate)) => proposals.candidates.push(candidate),
            Ok(None) => {}
            Err(reason) => proposals.unavailable(Some(snapshot.commit), reason),
        }
    }
    proposals.candidates.sort_by(|left, right| {
        right
            .completed_turns
            .cmp(&left.completed_turns)
            .then_with(|| left.reachable_from.cmp(&right.reachable_from))
            .then_with(|| left.commit.cmp(&right.commit))
    });
    Ok(proposals)
}

fn hydration_failure(error: &anyhow::Error) -> UnavailableReason {
    if error.downcast_ref::<HydrationBudgetExceeded>().is_some() {
        UnavailableReason::ScanBudget
    } else {
        UnavailableReason::SecretMapping
    }
}

fn storage_failure(error: &anyhow::Error) -> UnavailableReason {
    if error.downcast_ref::<storage::ReadLimitExceeded>().is_some() {
        UnavailableReason::ScanBudget
    } else {
        UnavailableReason::StoredEvidence
    }
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn discover_commits(
    repo: &Repo,
    limits: Limits,
    proposals: &mut Proposals,
) -> BTreeMap<String, BTreeSet<String>> {
    match repo.git_status_local(&["rev-parse", "--is-shallow-repository"]) {
        Ok((Some(0), shallow, _)) if shallow == "false" => {}
        _ => proposals.unavailable(None, UnavailableReason::CommitWalk),
    }
    let mut refs = Vec::new();
    let mut limited = false;
    let result = repo.git_stream_split_local_bounded(
        &[
            "for-each-ref",
            "--format=%(objectname) %(objecttype) %(refname)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
        b'\n',
        limits.reference_record_bytes,
        |record| {
            if refs.len() >= limits.references {
                limited = true;
                anyhow::bail!("reference budget reached");
            }
            let record = std::str::from_utf8(record)?;
            let fields: Vec<_> = record.split(' ').collect();
            ensure!(
                fields.len() == 3 && valid_oid(fields[0]),
                "invalid frozen reference"
            );
            refs.push((
                fields[0].to_owned(),
                fields[1].to_owned(),
                fields[2].to_owned(),
            ));
            Ok(())
        },
    );
    if let Err(error) = result {
        proposals.unavailable(
            None,
            if limited || error.is::<GitRecordBudgetExceeded>() {
                UnavailableReason::ScanBudget
            } else {
                UnavailableReason::ReferenceScan
            },
        );
    }
    let mut heads: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (oid, kind, label) in refs {
        let head = match kind.as_str() {
            "commit" => oid,
            "tag" => match repo.git_status_local(&[
                "rev-parse",
                "--verify",
                &format!("{oid}^{{commit}}"),
            ]) {
                Ok((Some(0), head, _)) if valid_oid(&head) => head,
                _ => {
                    proposals.unavailable(None, UnavailableReason::ReferenceScan);
                    continue;
                }
            },
            _ => continue,
        };
        heads.entry(head).or_default().insert(label);
    }
    let mut visits = 0usize;
    let mut label_bytes = 0usize;
    let mut commits: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (head, labels) in heads {
        let mut budget = false;
        let result = repo.git_stream_split_local(&["rev-list", &head, "--"], b'\n', |record| {
            if visits >= limits.visited_commits {
                budget = true;
                anyhow::bail!("commit budget reached");
            }
            visits += 1;
            let oid = std::str::from_utf8(record)?;
            ensure!(valid_oid(oid), "invalid immutable history object");
            let existing = commits.get(oid);
            let next_bytes = labels
                .iter()
                .filter(|label| !existing.is_some_and(|known| known.contains(*label)))
                .try_fold(label_bytes, |bytes, label| bytes.checked_add(label.len()));
            let Some(next_bytes) =
                next_bytes.filter(|bytes| *bytes <= limits.candidate_reference_label_bytes)
            else {
                budget = true;
                anyhow::bail!("candidate reference label budget reached");
            };
            commits
                .entry(oid.to_owned())
                .or_default()
                .extend(labels.iter().cloned());
            label_bytes = next_bytes;
            Ok(())
        });
        if result.is_err() {
            proposals.unavailable(
                Some(head),
                if budget {
                    UnavailableReason::ScanBudget
                } else {
                    UnavailableReason::CommitWalk
                },
            );
        }
        if budget {
            break;
        }
    }
    commits
}

struct Snapshot {
    commit: String,
    labels: BTreeSet<String>,
    raw: String,
    records: usize,
    same_runtime: bool,
}

impl Snapshot {
    fn from_log(
        commit: String,
        labels: BTreeSet<String>,
        log: &str,
        selected: &Link,
    ) -> std::result::Result<Self, UnavailableReason> {
        let envelopes =
            storage::parse_envelopes(log).map_err(|_| UnavailableReason::StoredEvidence)?;
        let mut raw = String::new();
        for envelope in &envelopes {
            raw.push_str(
                &serde_json::to_string(&envelope.content)
                    .map_err(|_| UnavailableReason::StoredEvidence)?,
            );
            raw.push('\n');
        }
        if raw.len() > log.len() {
            return Err(UnavailableReason::ScanBudget);
        }
        Ok(Self {
            commit,
            labels,
            raw,
            records: envelopes.len(),
            same_runtime: envelopes
                .iter()
                .all(|envelope| envelope.source == selected.source),
        })
    }

    fn candidate(&self, completed_turns: usize, records: usize, evidence: Evidence) -> Candidate {
        Candidate {
            commit: self.commit.clone(),
            reachable_from: self.labels.iter().cloned().collect(),
            completed_turns,
            records,
            evidence,
        }
    }
}

fn classify(
    snapshot: &Snapshot,
    stored: &HydrationReport,
    selected: &Link,
    slug: &str,
    bytes: &[u8],
    live: &str,
    live_hashes: &[String],
) -> std::result::Result<Option<Candidate>, UnavailableReason> {
    if stored.unresolved != 0 {
        return Err(UnavailableReason::SecretMapping);
    }
    let adapter = adapter::get(&selected.source).map_err(|_| UnavailableReason::NativeRecords)?;
    if has_materialization(selected) {
        if selected.materialized_from.as_deref() != Some(&snapshot.commit) {
            return Ok(None);
        }
        let route = selected
            .owner
            .as_deref()
            .zip(selected.agent.as_deref())
            .map(|(owner, name)| format!("{owner}/{name}"));
        if route.as_deref() != Some(slug) || selected.branch.is_none() {
            return Err(UnavailableReason::Materialization);
        }
        let (Some(offset), Some(hash)) =
            (selected.baseline_bytes, selected.baseline_hash.as_deref())
        else {
            return Err(UnavailableReason::Materialization);
        };
        native_archive::capture(bytes, offset, hash)
            .map_err(|_| UnavailableReason::Materialization)?;
        let offset = usize::try_from(offset).map_err(|_| UnavailableReason::Materialization)?;
        let records = bytes[..offset]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        let hydrated_offset =
            prefix_end(live, records).ok_or(UnavailableReason::Materialization)?;
        let parsed = adapter
            .parse(&live[..hydrated_offset])
            .map_err(|_| UnavailableReason::NativeRecords)?;
        let completed = turn::completed_count(&parsed);
        return Ok((completed != 0).then(|| {
            snapshot.candidate(completed, records, Evidence::VerifiedMaterializedSource)
        }));
    }
    if !snapshot.same_runtime {
        return Ok(None);
    }
    let stored_hashes = transcript::live_hashes(&stored.text);
    if stored_hashes.len() != snapshot.records {
        return Err(UnavailableReason::StoredEvidence);
    }
    if stored_hashes.is_empty() || !live_hashes.starts_with(&stored_hashes) {
        return Ok(None);
    }
    let parsed = adapter
        .parse(&stored.text)
        .map_err(|_| UnavailableReason::StoredEvidence)?;
    let completed = turn::completed_count(&parsed);
    Ok((completed != 0)
        .then(|| snapshot.candidate(completed, snapshot.records, Evidence::ExactNativeRecords)))
}

fn has_materialization(selected: &Link) -> bool {
    selected.materialized_from.is_some()
        || selected.baseline_bytes.is_some()
        || selected.baseline_hash.is_some()
}

fn prefix_end(text: &str, records: usize) -> Option<usize> {
    if records == 0 {
        Some(0)
    } else {
        text.match_indices('\n')
            .nth(records - 1)
            .map(|(offset, _)| offset + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::meta;
    use sha2::{Digest, Sha256};
    use zeroize::Zeroizing;

    struct NoKeys;
    impl KeyStore for NoKeys {
        fn get(&self, _: &str) -> Result<Zeroizing<Vec<u8>>> {
            anyhow::bail!("discovery requested an absent key")
        }
        fn set(&self, _: &str, _: &[u8]) -> Result<()> {
            anyhow::bail!("discovery attempted a key write")
        }
        fn delete(&self, _: &str) -> Result<()> {
            anyhow::bail!("discovery attempted a key deletion")
        }
    }

    #[derive(Clone, Default)]
    struct CountingKeys {
        values: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
        reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl KeyStore for CountingKeys {
        fn get(&self, id: &str) -> Result<Zeroizing<Vec<u8>>> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.values
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(Zeroizing::new)
                .context("missing fixture key")
        }
        fn set(&self, id: &str, key: &[u8]) -> Result<()> {
            self.values.lock().unwrap().insert(id.into(), key.into());
            Ok(())
        }
        fn delete(&self, id: &str) -> Result<()> {
            self.values.lock().unwrap().remove(id);
            Ok(())
        }
    }

    struct Fixture {
        directory: tempfile::TempDir,
        repo: Repo,
        selected: Link,
        dictionary: RepositoryDictionary<NoKeys>,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(&directory.path().join("repository")).unwrap();
            let selected = Link::new("claude-code", "native-id", None);
            let dictionary =
                RepositoryDictionary::new(directory.path().join("private-dictionary.json"), NoKeys);
            Self {
                directory,
                repo,
                selected,
                dictionary,
            }
        }
        fn commit(&self, raw: &str) -> String {
            let metadata = meta::Meta::new(
                format!("agit-{}", "a".repeat(40)),
                "claude-code".into(),
                "/explicit".into(),
            );
            meta::write(self.repo.root(), &metadata).unwrap();
            let log = transcript::wrap_lines(raw, "claude-code", &metadata.session);
            storage::write_snapshot(self.repo.root(), &log, &log).unwrap();
            self.repo.add_all().unwrap();
            self.repo.commit("lineage fixture").unwrap();
            self.repo.git(&["rev-parse", "HEAD"]).unwrap()
        }
        fn scan(&self, raw: &[u8], limits: Limits) -> Proposals {
            discover_with_dictionary(
                &self.repo,
                "alice/repo",
                &self.selected,
                raw,
                limits,
                &self.dictionary,
            )
            .unwrap()
        }
    }

    fn turn(text: &str) -> String {
        format!(
            "{}\n{}\n",
            serde_json::json!({"type":"user","message":{"role":"user","content":text}}),
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}})
        )
    }

    #[test]
    fn reference_record_budget_refuses_large_packed_names_without_losing_prior_evidence() {
        let fixture = Fixture::new();
        let raw = turn("bounded packed reference");
        let source = fixture.commit(&raw);
        let label = format!("refs/heads/{}", "z".repeat(200_000));
        let packed = fixture.repo.git_path("packed-refs").unwrap();
        std::fs::write(&packed, format!("{source} {label}\n")).unwrap();
        let before = std::fs::read(&packed).unwrap();
        let index_path = fixture.repo.git_path("index").unwrap();
        let index = std::fs::read(&index_path).unwrap();
        let mut present = false;
        fixture
            .repo
            .git_stream_split(
                &["for-each-ref", "--format=%(refname)", "refs/heads"],
                b'\n',
                |record| {
                    present |= record == label.as_bytes();
                    Ok(())
                },
            )
            .unwrap();
        assert!(present);
        let limited = fixture.scan(raw.as_bytes(), Limits::default());
        assert!(
            limited
                .unavailable
                .iter()
                .any(|item| item.reason == UnavailableReason::ScanBudget)
        );
        assert_eq!(limited.candidates.len(), 1);
        assert_eq!(limited.candidates[0].commit, source);
        assert_eq!(limited.candidates[0].reachable_from, ["refs/heads/main"]);
        let complete = fixture.scan(
            raw.as_bytes(),
            Limits {
                reference_record_bytes: label.len() + 128,
                ..Limits::default()
            },
        );
        assert!(complete.unavailable.is_empty());
        assert!(complete.candidates[0].reachable_from.contains(&label));
        assert_eq!(std::fs::read(&packed).unwrap(), before);
        assert_eq!(std::fs::read(&index_path).unwrap(), index);
        assert!(
            !fixture
                .directory
                .path()
                .join("private-dictionary.json")
                .exists()
        );
    }

    #[test]
    fn propagated_label_budget_keeps_whole_commit_associations_and_shared_aliases() {
        let fixture = Fixture::new();
        let raw = turn("first prefix");
        let first = fixture.commit(&raw);
        let continued = format!("{raw}{}", turn("second prefix"));
        let second = fixture.commit(&continued);
        fixture.repo.git(&["branch", "alias", &second]).unwrap();
        let refs = fixture.repo.git(&["show-ref"]).unwrap();
        let per_commit = "refs/heads/main".len() + "refs/heads/alias".len();
        let limited = fixture.scan(
            continued.as_bytes(),
            Limits {
                candidate_reference_label_bytes: per_commit,
                ..Limits::default()
            },
        );
        assert_eq!(limited.candidates.len(), 1);
        assert_eq!(limited.candidates[0].commit, second);
        assert_eq!(
            limited.candidates[0].reachable_from,
            ["refs/heads/alias", "refs/heads/main"]
        );
        assert!(
            limited
                .unavailable
                .iter()
                .any(|item| item.reason == UnavailableReason::ScanBudget)
        );
        let complete = fixture.scan(
            continued.as_bytes(),
            Limits {
                candidate_reference_label_bytes: per_commit * 2,
                ..Limits::default()
            },
        );
        assert!(complete.unavailable.is_empty());
        assert_eq!(
            complete
                .candidates
                .iter()
                .map(|item| &item.commit)
                .collect::<Vec<_>>(),
            [&second, &first]
        );
        assert!(
            complete
                .candidates
                .iter()
                .all(|item| item.reachable_from == ["refs/heads/alias", "refs/heads/main"])
        );
        let mut oversized = complete.candidates[0].clone();
        oversized.reachable_from = vec!["x".repeat(Limits::default().reference_record_bytes + 1)];
        let replay = inspect_with_dictionary(
            &fixture.repo,
            "alice/repo",
            &fixture.selected,
            continued.as_bytes(),
            Limits::default(),
            &fixture.dictionary,
            Some(&oversized),
        )
        .unwrap();
        assert!(replay.candidates.is_empty());
        assert!(
            replay
                .unavailable
                .iter()
                .any(|item| item.reason == UnavailableReason::ScanBudget)
        );
        assert_eq!(fixture.repo.git(&["show-ref"]).unwrap(), refs);
    }

    /// An accepted immutable candidate survives alias movement, but not altered native evidence.
    #[test]
    fn revalidation_uses_only_the_selected_immutable_candidate() {
        let fixture = Fixture::new();
        let raw = turn("chosen prefix");
        let source = fixture.commit(&raw);
        let candidate = fixture
            .scan(raw.as_bytes(), Limits::default())
            .candidates
            .remove(0);
        let unrelated = fixture.commit(&turn("unrelated current branch"));
        fixture
            .repo
            .git(&["update-ref", "refs/heads/unreadable", &unrelated])
            .unwrap();
        let objects = fixture.repo.git_path("objects").unwrap();
        let missing = objects.join(&unrelated[..2]).join(&unrelated[2..]);
        assert_eq!(
            fixture.repo.git(&["cat-file", "-t", &unrelated]).unwrap(),
            "commit"
        );
        std::fs::remove_file(&missing).unwrap_or_else(|error| {
            panic!("cannot remove unrelated fixture commit {missing:?}: {error}")
        });
        assert!(fixture.repo.git(&["cat-file", "-e", &unrelated]).is_err());
        let inspect = |native: &[u8]| {
            inspect_with_dictionary(
                &fixture.repo,
                "alice/repo",
                &fixture.selected,
                native,
                Limits::default(),
                &fixture.dictionary,
                Some(&candidate),
            )
        };
        assert_eq!(
            inspect(raw.as_bytes()).unwrap().candidates.as_slice(),
            std::slice::from_ref(&candidate)
        );
        assert!(
            inspect(turn("changed selected prefix").as_bytes())
                .unwrap()
                .candidates
                .is_empty()
        );
        let object = objects.join(&source[..2]).join(&source[2..]);
        assert_eq!(
            fixture.repo.git(&["cat-file", "-t", &source]).unwrap(),
            "commit"
        );
        std::fs::remove_file(&object).unwrap_or_else(|error| {
            panic!("cannot remove selected fixture commit {object:?}: {error}")
        });
        assert!(fixture.repo.git(&["cat-file", "-e", &source]).is_err());
        assert!(inspect(raw.as_bytes()).is_err());
    }

    #[test]
    fn exact_proposals_retain_occurrences_freeze_aliases_and_leave_local_state_unchanged() {
        let fixture = Fixture::new();
        let first = turn("same");
        let repeated = format!("{first}{first}");
        let a = fixture.commit(&first);
        let b = fixture.commit(&repeated);
        fixture.repo.git(&["branch", "alias", &b]).unwrap();
        fixture.repo.git(&["tag", "frozen", &b]).unwrap();
        let refs = fixture.repo.git(&["show-ref"]).unwrap();
        let index = std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap();
        let report = fixture.scan(
            format!("{repeated}{}", turn("new")).as_bytes(),
            Limits::default(),
        );
        assert!(report.unavailable.is_empty());
        assert!(!report.semantic_discovery_available);
        assert_eq!(report.candidates.len(), 2);
        assert_eq!(report.candidates[0].commit, b);
        assert_eq!(report.candidates[0].records, 4);
        assert_eq!(report.candidates[0].completed_turns, 2);
        assert_eq!(report.candidates[0].evidence, Evidence::ExactNativeRecords);
        assert!(
            report.candidates[0]
                .reachable_from
                .contains(&"refs/heads/alias".into())
        );
        assert_eq!(report.candidates[1].commit, a);
        assert_eq!(fixture.repo.git(&["show-ref"]).unwrap(), refs);
        assert_eq!(
            std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap(),
            index
        );
        assert!(!fixture.dictionary.exists());
        assert!(
            !fixture
                .directory
                .path()
                .join("private-dictionary.json.lock")
                .exists()
        );
        fixture.repo.git(&["branch", "-f", "alias", &a]).unwrap();
        assert_eq!(report.candidates[0].commit, b);
    }

    #[test]
    fn proposals_share_one_dictionary_snapshot_without_losing_candidate_boundaries() {
        use std::sync::atomic::Ordering;
        let fixture = Fixture::new();
        let path = fixture.directory.path().join("known-dictionary/vault.json");
        let keys = CountingKeys::default();
        let dictionary = RepositoryDictionary::new(path.clone(), keys.clone());
        let secret = "selected private value\nwith an escaped record boundary";
        let raw = turn(secret);
        let stored = dictionary
            .protect_jsonl(
                &raw,
                &crate::domain::secret_filter::Matcher::for_test(&[("known", secret)]),
            )
            .unwrap()
            .text;
        let a = fixture.commit(&stored);
        let b = fixture.commit(&format!("{stored}{stored}"));
        let unknown = turn(
            "{{AGIT_SECRET_V1:00000000-0000-4000-8000-000000000001:sec_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa}}",
        );
        let unavailable = fixture.commit(&unknown);
        let protected: serde_json::Value =
            serde_json::from_str(stored.lines().next().unwrap()).unwrap();
        let placeholder = protected["message"]["content"].as_str().unwrap();
        let collision = serde_json::Value::Object(serde_json::Map::from_iter([
            (secret.to_owned(), serde_json::Value::from(1)),
            (placeholder.to_owned(), serde_json::Value::from(2)),
        ]))
        .to_string();
        let collision_commit = fixture.commit(&format!("{collision}\n"));
        let before = std::fs::read(&path).unwrap();
        let lock = path.parent().unwrap().join("vault.lock");
        std::fs::remove_file(&lock).unwrap();
        let key_values = keys.values.lock().unwrap().clone();
        keys.reads.store(0, Ordering::SeqCst);
        let native = format!("{raw}{raw}{}", turn("next"));
        let report = discover_with_dictionary(
            &fixture.repo,
            "alice/repo",
            &fixture.selected,
            native.as_bytes(),
            Limits::default(),
            &dictionary,
        )
        .unwrap();
        assert_eq!(keys.reads.load(Ordering::SeqCst), 1);
        assert_eq!(
            report
                .candidates
                .iter()
                .map(|candidate| candidate.commit.as_str())
                .collect::<Vec<_>>(),
            [b.as_str(), a.as_str()]
        );
        assert_eq!(report.candidates[0].records, 4);
        assert_eq!(report.candidates[1].records, 2);
        assert!(report.unavailable.contains(&Unavailable {
            commit: Some(unavailable),
            reason: UnavailableReason::SecretMapping
        }));
        assert!(report.unavailable.contains(&Unavailable {
            commit: Some(collision_commit),
            reason: UnavailableReason::SecretMapping,
        }));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(*keys.values.lock().unwrap(), key_values);
        assert!(!lock.exists());
        let mut materialized = fixture.selected.clone();
        materialized.owner = Some("alice".into());
        materialized.agent = Some("repo".into());
        materialized.branch = Some("work".into());
        materialized.materialized_from = Some(a.clone());
        materialized.baseline_bytes = Some(stored.len() as u64);
        materialized.baseline_hash = Some(hex::encode(Sha256::digest(stored.as_bytes())));
        keys.reads.store(0, Ordering::SeqCst);
        let report = discover_with_dictionary(
            &fixture.repo,
            "alice/repo",
            &materialized,
            format!("{stored}{raw}").as_bytes(),
            Limits::default(),
            &dictionary,
        )
        .unwrap();
        assert_eq!(keys.reads.load(Ordering::SeqCst), 1);
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.candidates[0].commit, a);
        assert_eq!(report.candidates[0].records, 2);
        assert_eq!(
            report.candidates[0].evidence,
            Evidence::VerifiedMaterializedSource
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!lock.exists());
    }

    #[test]
    fn exhausted_native_snapshot_and_expansion_budgets_are_distinct_from_corrupt_storage() {
        let fixture = Fixture::new();
        let raw = turn("bounded evidence");
        fixture.commit(&raw);
        for limits in [
            Limits {
                total_input_bytes: raw.len() - 1,
                ..Limits::default()
            },
            Limits {
                total_input_bytes: raw.len(),
                ..Limits::default()
            },
            Limits {
                snapshot_bytes: 1,
                ..Limits::default()
            },
            Limits {
                hydrated_bytes: 1,
                ..Limits::default()
            },
        ] {
            let report = fixture.scan(raw.as_bytes(), limits);
            assert!(report.candidates.is_empty());
            assert!(
                report
                    .unavailable
                    .iter()
                    .any(|issue| issue.reason == UnavailableReason::ScanBudget)
            );
            assert!(
                !report
                    .unavailable
                    .iter()
                    .any(|issue| issue.reason == UnavailableReason::StoredEvidence)
            );
        }
        assert!(storage::metadata_local(fixture.repo.root(), "HEAD").is_err());
        assert!(storage::materialize_pair_local(fixture.repo.root(), "HEAD", 1024, 1024).is_err());
    }

    #[test]
    fn legacy_evidence_is_read_without_migration_and_nested_directories_do_not_select_a_repo() {
        let fixture = Fixture::new();
        let raw = turn("legacy prefix");
        let mut metadata = meta::Meta::new(
            format!("agit-{}", "b".repeat(40)),
            "claude-code".into(),
            "/explicit".into(),
        );
        metadata.layout = meta::LayoutVersion::V0;
        meta::write(fixture.repo.root(), &metadata).unwrap();
        let log = transcript::wrap_lines(&raw, "claude-code", &metadata.session);
        for path in [meta::LEGACY_LOG_FILE, meta::LEGACY_VIEW_FILE] {
            std::fs::write(fixture.repo.root().join(path), &log).unwrap();
        }
        fixture.repo.add_all().unwrap();
        fixture.repo.commit("legacy lineage fixture").unwrap();
        let source = fixture.repo.git(&["rev-parse", "HEAD"]).unwrap();
        let before = fixture
            .repo
            .git(&["status", "--porcelain=v1", "--untracked-files=all"])
            .unwrap();
        let report = fixture.scan(raw.as_bytes(), Limits::default());
        assert!(report.unavailable.is_empty());
        assert_eq!(report.candidates[0].commit, source);
        assert_eq!(report.candidates[0].evidence, Evidence::ExactNativeRecords);
        assert_eq!(
            std::fs::read_to_string(fixture.repo.root().join(meta::LEGACY_LOG_FILE)).unwrap(),
            log
        );
        assert!(!fixture.repo.root().join(meta::LOG_FILE).exists());
        assert_eq!(
            fixture
                .repo
                .git(&["status", "--porcelain=v1", "--untracked-files=all"])
                .unwrap(),
            before
        );
        let nested = fixture.repo.root().join("nested");
        std::fs::create_dir(&nested).unwrap();
        assert!(
            discover_with_dictionary(
                &Repo::at(nested),
                "alice/repo",
                &fixture.selected,
                raw.as_bytes(),
                Limits::default(),
                &fixture.dictionary
            )
            .is_err()
        );
    }

    #[test]
    fn matching_modeled_turns_do_not_hide_different_tool_outputs_or_native_fields() {
        let fixture = Fixture::new();
        let first = turn("same");
        let output = |text: &str| {
            format!(
                "{}\n",
                serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":text}]}})
            )
        };
        fixture.commit(&format!("{first}{}", output("old")));
        let report = fixture.scan(
            format!("{first}{}", output("new")).as_bytes(),
            Limits::default(),
        );
        assert!(report.candidates.is_empty());
        assert!(!report.semantic_discovery_available);
        assert!(report.unavailable.is_empty());
    }

    #[test]
    fn incomplete_corrupt_and_budget_limited_evidence_is_never_an_unknown_start() {
        let fixture = Fixture::new();
        let first = turn("first");
        fixture.commit(&first);
        for raw in [
            format!("{first}{{"),
            format!("{first}bad\n"),
            "{\"a\":1,\"a\":2}\n".into(),
        ] {
            let report = fixture.scan(raw.as_bytes(), Limits::default());
            assert!(report.candidates.is_empty());
            assert!(
                report
                    .unavailable
                    .iter()
                    .any(|issue| issue.reason == UnavailableReason::NativeRecords)
            );
        }
        let report = fixture.scan(
            first.as_bytes(),
            Limits {
                visited_commits: 0,
                ..Limits::default()
            },
        );
        assert!(
            report
                .unavailable
                .iter()
                .any(|issue| issue.reason == UnavailableReason::ScanBudget)
        );
        let event = self::storage::parse_sequence(
            &std::fs::read_to_string(fixture.repo.root().join(meta::LOG_FILE)).unwrap(),
        )
        .unwrap()[0]
            .clone();
        std::fs::remove_file(fixture.repo.root().join(meta::event_path(&event).unwrap())).unwrap();
        fixture.repo.add_all().unwrap();
        fixture.repo.commit("missing event fixture").unwrap();
        let report = fixture.scan(first.as_bytes(), Limits::default());
        assert!(
            report
                .unavailable
                .iter()
                .any(|issue| issue.reason == UnavailableReason::StoredEvidence)
        );
    }

    #[test]
    fn materialization_requires_the_exact_source_namespace_and_unchanged_native_prefix() {
        let mut fixture = Fixture::new();
        let source = fixture.commit(&turn("source"));
        let restored = turn("restored native representation");
        fixture.selected.owner = Some("alice".into());
        fixture.selected.agent = Some("repo".into());
        fixture.selected.branch = Some("work".into());
        fixture.selected.materialized_from = Some(source.clone());
        fixture.selected.baseline_bytes = Some(restored.len() as u64);
        fixture.selected.baseline_hash = Some(hex::encode(Sha256::digest(restored.as_bytes())));
        let native = format!("{restored}{}", turn("continued"));
        let report = fixture.scan(native.as_bytes(), Limits::default());
        assert!(report.unavailable.is_empty());
        assert_eq!(report.candidates[0].commit, source);
        assert_eq!(
            report.candidates[0].evidence,
            Evidence::VerifiedMaterializedSource
        );
        let changed = native.replace("restored", "modified");
        assert!(
            fixture
                .scan(changed.as_bytes(), Limits::default())
                .unavailable
                .iter()
                .any(|issue| issue.reason == UnavailableReason::Materialization)
        );
        fixture.selected.owner = Some("elsewhere".into());
        assert!(
            fixture
                .scan(native.as_bytes(), Limits::default())
                .candidates
                .is_empty()
        );
    }

    #[test]
    fn retained_materialization_spends_its_budget_only_on_the_selected_source() {
        let mut fixture = Fixture::new();
        let raw = turn("retained source");
        let source = fixture.commit(&raw);
        let source_bytes =
            transcript::wrap_lines(&raw, "claude-code", &format!("agit-{}", "a".repeat(40))).len();
        let unrelated = fixture.commit(&turn(&"unrelated payload ".repeat(1024)));
        let ids = storage::parse_sequence(
            &std::fs::read_to_string(fixture.repo.root().join(meta::LOG_FILE)).unwrap(),
        )
        .unwrap();
        let event = fixture
            .repo
            .git(&[
                "rev-parse",
                &format!("{unrelated}:{}", meta::event_path(&ids[0]).unwrap()),
            ])
            .unwrap();
        let budget = Limits {
            total_input_bytes: raw.len() + source_bytes * 2,
            ..Limits::default()
        };
        let ordinary = fixture.scan(raw.as_bytes(), budget);
        assert!(
            ordinary
                .unavailable
                .iter()
                .any(|issue| issue.reason == UnavailableReason::ScanBudget)
        );
        fixture.selected.owner = Some("alice".into());
        fixture.selected.agent = Some("repo".into());
        fixture.selected.branch = Some("work".into());
        fixture.selected.materialized_from = Some(source.clone());
        fixture.selected.baseline_bytes = Some(raw.len() as u64);
        fixture.selected.baseline_hash = Some(hex::encode(Sha256::digest(raw.as_bytes())));
        let report = fixture.scan(raw.as_bytes(), budget);
        assert!(report.unavailable.is_empty());
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.candidates[0].commit, source);
        let objects = fixture.repo.git_path("objects").unwrap();
        std::fs::remove_file(objects.join(&event[..2]).join(&event[2..])).unwrap();
        let report = fixture.scan(raw.as_bytes(), budget);
        assert!(report.unavailable.is_empty());
        assert_eq!(report.candidates[0].commit, source);
        let ordinary = discover_with_dictionary(
            &fixture.repo,
            "alice/repo",
            &Link::new("claude-code", "native-id", None),
            raw.as_bytes(),
            Limits::default(),
            &fixture.dictionary,
        )
        .unwrap();
        assert!(
            ordinary
                .unavailable
                .iter()
                .any(|issue| issue.commit.as_deref() == Some(unrelated.as_str())
                    && issue.reason == UnavailableReason::StoredEvidence)
        );
        fixture.selected.materialized_from = None;
        let incomplete = fixture.scan(raw.as_bytes(), budget);
        assert!(incomplete.candidates.is_empty());
        assert_eq!(
            incomplete.unavailable,
            [Unavailable {
                commit: None,
                reason: UnavailableReason::Materialization
            }]
        );
    }

    #[test]
    fn unknown_placeholders_and_no_user_turns_do_not_produce_an_exact_proposal() {
        let fixture = Fixture::new();
        fixture.commit("{\"type\":\"system\",\"content\":\"metadata\"}\n");
        assert!(
            fixture
                .scan(
                    b"{\"type\":\"system\",\"content\":\"metadata\"}\n",
                    Limits::default()
                )
                .candidates
                .is_empty()
        );
        let token = "{{AGIT_SECRET_V1:00000000-0000-4000-8000-000000000001:sec_00000000000000000000000000000000}}";
        let native = turn(token);
        fixture.commit(&native);
        let report = fixture.scan(native.as_bytes(), Limits::default());
        assert!(
            report
                .unavailable
                .iter()
                .any(|issue| issue.reason == UnavailableReason::SecretMapping)
        );
    }

    #[test]
    fn grafts_cannot_supply_candidates_and_shallow_history_remains_explicitly_incomplete() {
        use crate::domain::repo::ReadPolicy;
        let fixture = Fixture::new();
        let base_raw = turn("real ancestor");
        let base = fixture.commit(&base_raw);
        let head_raw = turn("visible head");
        let head = fixture.commit(&head_raw);
        let injected_raw = turn("unreachable source");
        let temporary = fixture.commit(&injected_raw);
        let tree = fixture
            .repo
            .git(&["rev-parse", &format!("{temporary}^{{tree}}")])
            .unwrap();
        let injected = fixture
            .repo
            .git(&["commit-tree", &tree, "-m", "unreachable fixture"])
            .unwrap();
        fixture
            .repo
            .git(&["update-ref", "refs/heads/main", &head, &temporary])
            .unwrap();
        let expected = format!("{head}\n{base}");
        assert_eq!(fixture.repo.git(&["rev-list", &head]).unwrap(), expected);
        let graft = format!("{head} {injected}\n");
        let inherited = fixture.directory.path().join("inherited-grafts");
        std::fs::write(&inherited, &graft).unwrap();
        let local = fixture.repo.git_path("info/grafts").unwrap();
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, &graft).unwrap();
        let refs = fixture.repo.git(&["show-ref"]).unwrap();
        let index = std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap();
        let grafted = format!("{head}\n{injected}");
        assert_eq!(fixture.repo.git(&["rev-list", &head]).unwrap(), grafted);
        let report = fixture.scan(injected_raw.as_bytes(), Limits::default());
        assert!(report.candidates.is_empty());
        assert!(report.unavailable.is_empty());
        assert_eq!(std::fs::read_to_string(&local).unwrap(), graft);
        std::fs::remove_file(&local).unwrap();
        let probe = |key: &str, path: &std::path::Path, local_only: bool| {
            let mut command = std::process::Command::new("git");
            command
                .arg("--no-replace-objects")
                .arg("-C")
                .arg(fixture.repo.root())
                .args(["rev-list", &head])
                .env(key, path);
            if local_only {
                ReadPolicy::LocalOnly.apply(&mut command);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        assert_eq!(probe("GIT_GRAFT_FILE", &inherited, false), grafted);
        assert_eq!(probe("GIT_GRAFT_FILE", &inherited, true), expected);
        let shallow_override = fixture.directory.path().join("inherited-shallow");
        std::fs::write(&shallow_override, format!("{head}\n")).unwrap();
        assert_eq!(probe("GIT_SHALLOW_FILE", &shallow_override, false), head);
        assert_eq!(probe("GIT_SHALLOW_FILE", &shallow_override, true), expected);
        let shallow = fixture.repo.git_path("shallow").unwrap();
        std::fs::write(&shallow, format!("{head}\n")).unwrap();
        let hidden = fixture.scan(base_raw.as_bytes(), Limits::default());
        assert!(hidden.candidates.is_empty());
        assert!(hidden.unavailable.contains(&Unavailable {
            commit: None,
            reason: UnavailableReason::CommitWalk
        }));
        let visible = fixture.scan(head_raw.as_bytes(), Limits::default());
        assert_eq!(visible.candidates[0].commit, head);
        assert!(visible.unavailable.contains(&Unavailable {
            commit: None,
            reason: UnavailableReason::CommitWalk
        }));
        assert_eq!(
            std::fs::read_to_string(&shallow).unwrap(),
            format!("{head}\n")
        );
        std::fs::remove_file(&shallow).unwrap();
        let complete = fixture.scan(base_raw.as_bytes(), Limits::default());
        assert!(complete.unavailable.is_empty());
        assert_eq!(complete.candidates[0].commit, base);
        assert_eq!(fixture.repo.git(&["show-ref"]).unwrap(), refs);
        assert_eq!(
            std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap(),
            index
        );
        assert!(!fixture.dictionary.exists());
    }

    /// A promisor remote that serves ordinary reads is never contacted by candidate discovery.
    #[test]
    fn missing_promisor_objects_remain_unavailable_without_transport_or_state_changes() {
        use crate::domain::repo::ReadPolicy;
        use std::io::{Read, Write};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        let fixture = Fixture::new();
        let raw = turn("promised evidence");
        let head = fixture.commit(&raw);
        let ids = storage::parse_sequence(
            &std::fs::read_to_string(fixture.repo.root().join(meta::LOG_FILE)).unwrap(),
        )
        .unwrap();
        let object = fixture
            .repo
            .git(&[
                "rev-parse",
                &format!("{head}:{}", meta::event_path(&ids[0]).unwrap()),
            ])
            .unwrap();
        let objects = fixture.repo.git_path("objects").unwrap();
        std::fs::remove_file(objects.join(&object[..2]).join(&object[2..])).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_hits = hits.clone();
        let worker_stop = stop.clone();
        let server = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            while !worker_stop.load(Ordering::SeqCst)
                && started.elapsed() < std::time::Duration::from_secs(20)
            {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        worker_hits.fetch_add(1, Ordering::SeqCst);
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .unwrap();
                        let mut request = [0; 4096];
                        let _ = stream.read(&mut request);
                        let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        for (name, value) in [
            ("core.repositoryformatversion", "1".to_owned()),
            ("extensions.partialclone", "origin".to_owned()),
            ("remote.origin.promisor", "true".into()),
            ("remote.origin.partialclonefilter", "blob:none".into()),
            (
                "remote.origin.url",
                format!("http://127.0.0.1:{port}/repository"),
            ),
        ] {
            fixture.repo.git(&["config", name, &value]).unwrap();
        }
        let control = std::process::Command::new("git")
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(fixture.repo.root())
            .args([
                "-c",
                "http.proxy=",
                "-c",
                "credential.helper=",
                "cat-file",
                "blob",
                &object,
            ])
            .env_remove("GIT_NO_LAZY_FETCH")
            .env_remove("GIT_ALLOW_PROTOCOL")
            .output()
            .unwrap();
        let control_hits = hits.swap(0, Ordering::SeqCst);
        let refs = fixture.repo.git(&["show-ref"]).unwrap();
        let index = std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap();
        let report = fixture.scan(raw.as_bytes(), Limits::default());
        let discovery_hits = hits.load(Ordering::SeqCst);
        let metadata_oid = fixture
            .repo
            .git(&["rev-parse", &format!("{head}:{}", meta::FILE)])
            .unwrap();
        let metadata_object = objects.join(&metadata_oid[..2]).join(&metadata_oid[2..]);
        let metadata_bytes = std::fs::read(&metadata_object).unwrap();
        std::fs::remove_file(&metadata_object).unwrap();
        let metadata_refs = vec![head.clone()];
        let missing_metadata = meta::at_refs_with_policy(
            &fixture.repo,
            &metadata_refs,
            ReadPolicy::LocalOnly,
            64 * 1024,
        );
        let picker_hits = hits.load(Ordering::SeqCst);
        std::fs::write(&metadata_object, metadata_bytes).unwrap();
        let readable_metadata = meta::at_refs_with_policy(
            &fixture.repo,
            &metadata_refs,
            ReadPolicy::LocalOnly,
            64 * 1024,
        );
        let oversized_metadata =
            meta::at_refs_with_policy(&fixture.repo, &metadata_refs, ReadPolicy::LocalOnly, 1);
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        assert!(!control.status.success());
        assert!(
            control_hits > 0,
            "the missing-object positive control must contact its promisor remote"
        );
        assert_eq!(discovery_hits, 0);
        assert_eq!(picker_hits, 0);
        assert!(missing_metadata[0].is_none());
        assert!(readable_metadata[0].is_some());
        assert!(oversized_metadata[0].is_none());
        assert!(report.candidates.is_empty());
        assert!(
            report
                .unavailable
                .iter()
                .any(|issue| issue.reason == UnavailableReason::StoredEvidence)
        );
        assert_eq!(fixture.repo.git(&["show-ref"]).unwrap(), refs);
        assert_eq!(
            std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap(),
            index
        );
        assert!(!fixture.dictionary.exists());
    }
}
