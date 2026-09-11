//! Read-only proofs that archival history preserves the materialized session.
//!
//! Endpoint equality cannot prove that an intervening commit leaves a session untouched.
//! Archive edges preserve the inherited snapshot and append evidence to its LOG. A materialized
//! watermark may also cross file edges that preserve the session's storage and identity.

use crate::Result;
use crate::domain::metadata_facts::JsonFacts;
use crate::domain::{meta, refs, repo::Repo, storage};
use anyhow::{Context, ensure};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const MAX_CHAIN_EDGES: usize = 256;
const MAX_OBJECT_BYTES: usize = storage::MAX_EVENT_BYTES;
const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_COMMIT_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 1024 * 1024 * 1024;
const MAX_TREE_ENTRIES: usize = 2 * storage::MAX_SEQUENCE_EVENTS;
const MAX_PATH_BYTES: usize = 4096;
const MAX_DEPTH: usize = 128;

/// Proof and publication cannot select different repositories or object stores through ambient Git.
pub(crate) const GIT_ROUTING_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_REPLACE_REF_BASE",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
];

/// A successfully verified immutable parent-to-archive edge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveEdgeFacts {
    parent: String,
    archive: String,
    session: String,
    appended_records: usize,
}

impl ArchiveEdgeFacts {
    pub fn parent(&self) -> &str {
        &self.parent
    }

    pub fn archive(&self) -> &str {
        &self.archive
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// Counts appended occurrences, including repeated unchanged envelopes.
    pub fn appended_records(&self) -> usize {
        self.appended_records
    }
}

/// A complete chain proof; an empty chain still validates the source snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveChainFacts {
    source: String,
    candidate: String,
    session: String,
    edges: Vec<ArchiveEdgeFacts>,
}

impl ArchiveChainFacts {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn candidate(&self) -> &str {
        &self.candidate
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// Edges are ordered from the source toward the candidate.
    pub fn edges(&self) -> &[ArchiveEdgeFacts] {
        &self.edges
    }
}

/// Proves an exact parent edge without resolving refs, updating local state, or fetching objects.
pub fn verify_edge(repo: &Repo, parent: &str, candidate: &str) -> Result<ArchiveEdgeFacts> {
    validate_endpoints(parent, candidate)?;
    let mut reader = ObjectReader::new(repo)?;
    let child = Snapshot::read(&mut reader, candidate)?;
    ensure!(
        child.parents.as_slice() == [parent],
        "archive candidate must have exactly the expected parent"
    );
    let old = Snapshot::read(&mut reader, parent)?;
    verify_snapshots(&old, &child)
}

/// A landing keeps the retained ordinary merge tree intact except for appended LOG evidence.
/// Parent identities come from the commit object, independently of traversal grafts or cutoffs.
pub fn verify_merge_landing(
    repo: &Repo,
    target: &str,
    source: &str,
    ordinary_tree: &str,
    candidate: &str,
    candidate_tree: &str,
) -> Result<String> {
    for oid in [source, ordinary_tree, candidate, candidate_tree] {
        validate_endpoints(target, oid)?;
    }
    let mut reader = ObjectReader::new(repo)?;
    let commit = reader.read(candidate, "commit", MAX_COMMIT_BYTES)?;
    let (tree, parents) = commit_headers(&commit, candidate.len())?;
    ensure!(
        tree == candidate_tree && parents.as_slice() == [target, source],
        "merge landing differs from its retained tree or ordered parents"
    );
    for parent in &parents {
        let bytes = reader.read(parent, "commit", MAX_COMMIT_BYTES)?;
        commit_headers(&bytes, parent.len())?;
    }
    let ordinary =
        Snapshot::read_tree(&mut reader, ordinary_tree, ordinary_tree, Vec::new(), false)?;
    let landed = Snapshot::read_tree(&mut reader, candidate, &tree, parents, false)?;
    ensure!(
        ordinary.metadata.kind == meta::Kind::Merge
            && landed.metadata.kind == meta::Kind::Merge
            && regular_entry(&ordinary.entries, meta::FILE)?
                == regular_entry(&landed.entries, meta::FILE)?,
        "merge landing changes the ordinary merge metadata"
    );
    ensure!(
        landed.log.starts_with(&ordinary.log),
        "merge landing changes the ordinary LOG prefix"
    );
    verify_log_extension(&ordinary, &landed)?;
    let mut suffix = String::new();
    for id in &landed.log[ordinary.log.len()..] {
        let bytes = read_regular(
            &mut reader,
            &landed.entries,
            &meta::event_path(id)?,
            MAX_OBJECT_BYTES,
        )?;
        let text = std::str::from_utf8(&bytes)?;
        ensure!(
            suffix
                .len()
                .checked_add(text.len())
                .is_some_and(|size| size <= storage::MAX_MATERIALIZED_BYTES),
            "merge landing suffix exceeds its byte limit"
        );
        suffix.push_str(text);
    }
    Ok(suffix)
}

/// A frozen merge source is verified by object identity, independently of later ref movement.
/// Session sources retain their storage checks; file-line sources require every local blob.
pub fn verify_frozen_source(repo: &Repo, head: &str) -> Result<()> {
    validate_endpoints(head, head)?;
    let mut reader = ObjectReader::new(repo)?;
    let commit = reader.read(head, "commit", MAX_COMMIT_BYTES)?;
    let (tree, _) = commit_headers(&commit, head.len())?;
    let mut entries = BTreeMap::new();
    read_tree(&mut reader, &tree, b"", 0, &mut entries)?;
    let bytes = read_regular(&mut reader, &entries, meta::FILE, MAX_METADATA_BYTES)?;
    let metadata = meta::parse_strict(std::str::from_utf8(&bytes)?, head)?;
    if metadata.is_session_line() {
        Snapshot::read_with_layout(&mut reader, head, true)?;
    } else {
        ensure!(
            metadata.is_file_line(),
            "merge source has no supported line identity"
        );
        for entry in entries.values() {
            if !entry.is_tree()
                && entry.mode != 0o160000
                && reader.validated_blobs.insert(entry.oid.clone())
            {
                reader.read(&entry.oid, "blob", MAX_OBJECT_BYTES)?;
            }
        }
    }
    Ok(())
}

/// The public file result contains no session evidence and names only the original merge parents.
/// Its retained tree fixes shared content independently of the session branch's native cursor.
pub fn verify_file_merge_landing(
    repo: &Repo,
    target: &str,
    source: &str,
    candidate: &str,
    expected_tree: &str,
) -> Result<()> {
    for oid in [source, candidate, expected_tree] {
        validate_endpoints(target, oid)?;
    }
    let mut reader = ObjectReader::new(repo)?;
    let commit = reader.read(candidate, "commit", MAX_COMMIT_BYTES)?;
    let (tree, parents) = commit_headers(&commit, candidate.len())?;
    ensure!(
        tree == expected_tree && parents.as_slice() == [target, source],
        "file merge candidate differs from its retained tree or ordered parents"
    );
    for parent in parents {
        let bytes = reader.read(&parent, "commit", MAX_COMMIT_BYTES)?;
        commit_headers(&bytes, parent.len())?;
    }
    let mut entries = BTreeMap::new();
    read_tree(&mut reader, &tree, b"", 0, &mut entries)?;
    let bytes = read_regular(&mut reader, &entries, meta::FILE, MAX_METADATA_BYTES)?;
    let metadata = meta::parse_strict(std::str::from_utf8(&bytes)?, candidate)?;
    ensure!(
        metadata.is_file_line() && metadata.kind == meta::Kind::Merge,
        "file merge candidate must retain its file-line declaration"
    );
    for (path, entry) in &entries {
        let path = std::str::from_utf8(path).context("file merge path is not Unicode")?;
        ensure!(
            path == meta::FILE || !meta::is_storage_path(path),
            "file merge candidate contains session evidence"
        );
        if !entry.is_tree()
            && entry.mode != 0o160000
            && reader.validated_blobs.insert(entry.oid.clone())
        {
            reader.read(&entry.oid, "blob", MAX_OBJECT_BYTES)?;
        }
    }
    Ok(())
}

/// An exploration with no new records may acknowledge disposition without inventing an Archive.
pub fn verify_empty_file_exploration(repo: &Repo, seed: &str, candidate: &str) -> Result<()> {
    validate_endpoints(seed, candidate)?;
    let mut reader = ObjectReader::new(repo)?;
    let parent = reader.read(seed, "commit", MAX_COMMIT_BYTES)?;
    let (expected_tree, _) = commit_headers(&parent, seed.len())?;
    let child = reader.read(candidate, "commit", MAX_COMMIT_BYTES)?;
    let (tree, parents) = commit_headers(&child, candidate.len())?;
    ensure!(
        tree == expected_tree && parents.as_slice() == [seed],
        "empty file exploration must retain its exact seed tree and parent"
    );
    Snapshot::read(&mut reader, candidate)?;
    Ok(())
}

/// A file merge's evidence branch starts empty and inherits only the frozen shared files.
/// Raw parents keep the evidence branch out of the target's ancestry and publication closure.
pub fn verify_file_evidence_seed(
    repo: &Repo,
    target: &str,
    candidate: &str,
    candidate_tree: &str,
    metadata_text: &str,
) -> Result<()> {
    validate_endpoints(target, candidate)?;
    validate_endpoints(target, candidate_tree)?;
    let mut reader = ObjectReader::new(repo)?;
    let commit = reader.read(candidate, "commit", MAX_COMMIT_BYTES)?;
    let (tree, parents) = commit_headers(&commit, candidate.len())?;
    ensure!(
        tree == candidate_tree && parents.as_slice() == [target],
        "file exploration seed has different parents or tree"
    );
    let seed = Snapshot::read(&mut reader, candidate)?;
    let actual_meta = read_regular(&mut reader, &seed.entries, meta::FILE, MAX_METADATA_BYTES)?;
    ensure!(
        JsonFacts::parse(std::str::from_utf8(&actual_meta)?)? == JsonFacts::parse(metadata_text)?
            && seed.metadata.kind == meta::Kind::File
            && seed.metadata.turn.is_none()
            && seed.log.is_empty(),
        "file exploration seed changes its identity or contains conversation"
    );
    let target_commit = reader.read(target, "commit", MAX_COMMIT_BYTES)?;
    let (target_tree, _) = commit_headers(&target_commit, target.len())?;
    let mut old = BTreeMap::new();
    read_tree(&mut reader, &target_tree, b"", 0, &mut old)?;
    let bytes = read_regular(&mut reader, &old, meta::FILE, MAX_METADATA_BYTES)?;
    let metadata = meta::parse_strict(std::str::from_utf8(&bytes)?, target)?;
    ensure!(
        metadata.is_file_line(),
        "file exploration target is not a file line"
    );
    if metadata.layout == meta::LayoutVersion::V0 {
        ensure!(
            !old.keys().any(|path| {
                path == meta::LOG_FILE.as_bytes()
                    || path == meta::VIEW_FILE.as_bytes()
                    || path == meta::EVENTS_DIR.as_bytes()
                    || path.starts_with(b"events/")
            }),
            "file exploration seed collides with legacy shared paths"
        );
    }
    let inherited = |path: &[u8], entry: &Entry| {
        !entry.is_tree()
            && path != meta::ATTRS_FILE.as_bytes()
            && !std::str::from_utf8(path)
                .is_ok_and(|path| meta::is_storage_path_for(metadata.layout, path))
    };
    let shared = old
        .iter()
        .filter(|(path, entry)| inherited(path, entry))
        .collect::<BTreeMap<_, _>>();
    let actual_shared = seed
        .entries
        .iter()
        .filter(|(path, entry)| {
            !entry.is_tree()
                && ![
                    meta::FILE,
                    meta::LOG_FILE,
                    meta::VIEW_FILE,
                    meta::ATTRS_FILE,
                ]
                .iter()
                .any(|allowed| path.as_slice() == allowed.as_bytes())
        })
        .collect::<BTreeMap<_, _>>();
    ensure!(
        shared == actual_shared,
        "file exploration seed changes shared files or adds evidence"
    );
    let attributes = if old.contains_key(meta::ATTRS_FILE.as_bytes()) {
        Some(read_regular(
            &mut reader,
            &old,
            meta::ATTRS_FILE,
            MAX_OBJECT_BYTES,
        )?)
    } else {
        None
    };
    let expected_attributes = storage::attributes_text_strict(
        attributes.as_deref().map(std::str::from_utf8).transpose()?,
    )?;
    ensure!(
        read_regular(
            &mut reader,
            &seed.entries,
            meta::ATTRS_FILE,
            MAX_OBJECT_BYTES
        )? == expected_attributes.as_bytes(),
        "file exploration seed changes shared Git attributes"
    );
    Ok(())
}

/// Turn coordinates and LOG bytes come from the same complete immutable first-parent chain.
/// Native reads and later graph metadata changes cannot reinterpret a retained selection.
pub(crate) struct FrozenSourceLog {
    head: String,
    chain: refs::Chain,
    logs: BTreeMap<String, String>,
}

impl FrozenSourceLog {
    pub fn log_at(&self, head: &str) -> Result<&str> {
        ensure!(
            head == self.head,
            "source LOG proof belongs to a different head"
        );
        Ok(&self.logs[&self.head])
    }

    pub fn turn_lines(&self, ordinal: u32) -> Result<(u32, Vec<usize>)> {
        let (turn, sha) = refs::turn_in(&self.chain, ordinal)?;
        let index = self
            .chain
            .entries
            .iter()
            .position(|entry| entry.sha == sha)
            .context("selected turn is absent from the frozen source chain")?;
        let this = self
            .logs
            .get(&sha)
            .context("selected source turn has no valid LOG")?;
        let previous = match index.checked_sub(1).map(|i| &self.chain.entries[i]) {
            None => "",
            Some(parent) => {
                let metadata = parent
                    .meta
                    .as_ref()
                    .context("selected source turn's first parent has no metadata")?;
                if metadata.is_file_line() || metadata.session.is_empty() {
                    ""
                } else {
                    self.logs
                        .get(&parent.sha)
                        .context("selected source turn's first parent has no valid LOG")?
                }
            }
        };
        ensure!(
            this.starts_with(previous),
            "selected source turn rewrites its first parent's LOG"
        );
        ensure!(
            self.log_at(&self.head)?.starts_with(this),
            "source head rewrites LOG after the selected turn"
        );
        Ok((
            turn,
            (previous.lines().count()..this.lines().count()).collect(),
        ))
    }
}

/// Walk stored parent headers to a real root; traversal flags never establish ancestry.
pub(crate) fn freeze_source_log(repo: &Repo, head: &str) -> Result<FrozenSourceLog> {
    freeze_source_log_with_limit(repo, head, MAX_CHAIN_EDGES)
}

fn freeze_source_log_with_limit(
    repo: &Repo,
    head: &str,
    max_commits: usize,
) -> Result<FrozenSourceLog> {
    validate_endpoints(head, head)?;
    let mut reader = ObjectReader::new(repo)?;
    let mut history = Vec::new();
    let mut visited = HashSet::new();
    let mut current = head.to_owned();
    loop {
        ensure!(
            history.len() < max_commits,
            "archive source exceeds its history limit"
        );
        ensure!(
            visited.insert(current.clone()),
            "archive source repeats a parent"
        );
        let bytes = reader.read(&current, "commit", MAX_COMMIT_BYTES)?;
        let (tree, parents) = commit_headers(&bytes, head.len())?;
        let next = parents.first().cloned();
        history.push((current, tree));
        let Some(parent) = next else { break };
        current = parent;
    }
    history.reverse();
    let mut entries = Vec::with_capacity(history.len());
    let mut logs = BTreeMap::new();
    let mut retained_bytes = 0usize;
    for (sha, tree) in history {
        let mut files = BTreeMap::new();
        read_tree(&mut reader, &tree, b"", 0, &mut files)?;
        let metadata = if files.contains_key(meta::FILE.as_bytes()) {
            let bytes = read_regular(&mut reader, &files, meta::FILE, MAX_METADATA_BYTES)?;
            Some(meta::parse_strict(std::str::from_utf8(&bytes)?, &sha)?)
        } else {
            None
        };
        if let Some(metadata) = &metadata
            && metadata.is_session_line()
            && !metadata.session.is_empty()
        {
            let log = match metadata.layout {
                meta::LayoutVersion::V0 => {
                    let bytes = read_regular(
                        &mut reader,
                        &files,
                        meta::LEGACY_LOG_FILE,
                        storage::MAX_MATERIALIZED_BYTES,
                    )?;
                    storage::canonical_v0(std::str::from_utf8(&bytes)?)?
                }
                meta::LayoutVersion::V1 => {
                    let ids = read_sequence(&mut reader, &files, meta::LOG_FILE)?;
                    let mut log = String::new();
                    for id in ids {
                        let bytes = read_regular(
                            &mut reader,
                            &files,
                            &meta::event_path(&id)?,
                            MAX_OBJECT_BYTES,
                        )?;
                        let text = std::str::from_utf8(&bytes)?;
                        ensure!(
                            storage::event_id(text)? == id,
                            "source event content address differs"
                        );
                        ensure!(
                            log.len()
                                .checked_add(text.len())
                                .is_some_and(|size| size <= storage::MAX_MATERIALIZED_BYTES),
                            "source LOG exceeds its byte limit"
                        );
                        log.push_str(text);
                    }
                    log
                }
            };
            ensure!(
                log.len() <= storage::MAX_MATERIALIZED_BYTES,
                "source LOG exceeds its byte limit"
            );
            retained_bytes = retained_bytes
                .checked_add(log.len())
                .context("source LOG budget overflow")?;
            ensure!(
                retained_bytes <= MAX_TOTAL_BYTES,
                "source LOG proof exceeds its aggregate byte limit"
            );
            logs.insert(sha.clone(), log);
        }
        entries.push(refs::ChainEntry {
            sha,
            meta: metadata,
        });
    }
    ensure!(
        logs.contains_key(head),
        "archive source head has no claimed session LOG"
    );
    let declared = entries.iter().any(|entry| entry.meta.is_some());
    Ok(FrozenSourceLog {
        head: head.to_owned(),
        chain: refs::Chain { entries, declared },
        logs,
    })
}

/// Proves every actual edge from source to candidate, refusing unprovable or excessive history.
pub fn verify_chain(repo: &Repo, source: &str, candidate: &str) -> Result<ArchiveChainFacts> {
    verify_chain_with_limit(repo, source, candidate, MAX_CHAIN_EDGES)
}

fn verify_chain_with_limit(
    repo: &Repo,
    source: &str,
    candidate: &str,
    max_edges: usize,
) -> Result<ArchiveChainFacts> {
    let mut edges = Vec::new();
    let session = walk_chain(repo, source, candidate, max_edges, false, |old, child| {
        edges.push(verify_snapshots(old, child)?);
        Ok(())
    })?;
    edges.reverse();
    Ok(ArchiveChainFacts {
        source: source.to_owned(),
        candidate: candidate.to_owned(),
        session,
        edges,
    })
}

/// Only immutable archive and file edges may advance an unchanged materialized runtime.
/// File edges retain their shared-file semantics; they cannot change session storage or identity.
pub fn verify_materialized_chain(repo: &Repo, source: &str, candidate: &str) -> Result<()> {
    verify_materialized_chain_with_limit(repo, source, candidate, MAX_CHAIN_EDGES)
}

/// Late archive evidence may follow ordinary turns without borrowing their materialized context.
/// Every stored parent edge must preserve the claimed session and its accumulated LOG evidence.
/// View surgery, merges and layout transitions require separate admission and are refused here.
pub fn verify_archive_append_target(repo: &Repo, accepted: &str, candidate: &str) -> Result<()> {
    verify_archive_append_target_with_limit(repo, accepted, candidate, MAX_CHAIN_EDGES)
}

fn verify_archive_append_target_with_limit(
    repo: &Repo,
    accepted: &str,
    candidate: &str,
    max_edges: usize,
) -> Result<()> {
    walk_chain(repo, accepted, candidate, max_edges, false, |old, child| {
        ensure!(
            old.metadata.session == child.metadata.session,
            "archive destination changes the claimed logical session"
        );
        match child.metadata.kind {
            meta::Kind::Turn => verify_retained_evidence(old, child),
            meta::Kind::Archive => verify_snapshots(old, child).map(|_| ()),
            meta::Kind::File => verify_file_snapshots(old, child),
            _ => anyhow::bail!("archive destination crosses an unsupported history edge"),
        }
    })?;
    Ok(())
}

fn verify_retained_evidence(parent: &Snapshot, child: &Snapshot) -> Result<()> {
    ensure!(
        child.log.starts_with(&parent.log),
        "archive destination changes an inherited LOG occurrence"
    );
    for (path, old) in &parent.entries {
        if path != meta::EVENTS_DIR.as_bytes() && !path.starts_with(b"events/") {
            continue;
        }
        let new = child
            .entries
            .get(path)
            .context("archive destination removes an inherited event path")?;
        ensure!(
            old.mode == new.mode && (old.is_tree() || old.oid == new.oid),
            "archive destination changes an inherited event object"
        );
    }
    Ok(())
}

fn verify_materialized_chain_with_limit(
    repo: &Repo,
    source: &str,
    candidate: &str,
    max_edges: usize,
) -> Result<()> {
    walk_chain(
        repo,
        source,
        candidate,
        max_edges,
        true,
        |old, child| match child.metadata.kind {
            meta::Kind::Archive => verify_snapshots(old, child).map(|_| ()),
            meta::Kind::File => verify_file_snapshots(old, child),
            _ => anyhow::bail!("materialized history crosses a context-changing commit"),
        },
    )?;
    Ok(())
}

fn walk_chain(
    repo: &Repo,
    source: &str,
    candidate: &str,
    max_edges: usize,
    allow_legacy: bool,
    mut verify: impl FnMut(&Snapshot, &Snapshot) -> Result<()>,
) -> Result<String> {
    validate_endpoints(source, candidate)?;
    let mut reader = ObjectReader::new(repo)?;
    let mut child = Snapshot::read_with_layout(&mut reader, candidate, allow_legacy)?;
    let session = child.metadata.session.clone();
    let mut edges = 0;
    while child.oid != source {
        ensure!(edges < max_edges, "archive chain exceeds its edge limit");
        ensure!(
            child.parents.len() == 1,
            "materialized history requires an exact single parent"
        );
        let old = Snapshot::read_with_layout(&mut reader, &child.parents[0], allow_legacy)?;
        verify(&old, &child)?;
        child = old;
        edges += 1;
    }
    Ok(session)
}

fn validate_endpoints(source: &str, candidate: &str) -> Result<()> {
    ensure!(
        valid_oid(source),
        "archive source must be a full immutable object id"
    );
    ensure!(
        valid_oid(candidate) && candidate.len() == source.len(),
        "archive candidate must use the source object-id format"
    );
    Ok(())
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    mode: u32,
    oid: String,
}

impl Entry {
    fn is_tree(&self) -> bool {
        self.mode == 0o40000
    }
}

struct Snapshot {
    oid: String,
    parents: Vec<String>,
    metadata: meta::Meta,
    metadata_facts: JsonFacts,
    entries: BTreeMap<Vec<u8>, Entry>,
    log: Vec<String>,
}

impl Snapshot {
    fn read(reader: &mut ObjectReader, oid: &str) -> Result<Self> {
        Self::read_with_layout(reader, oid, false)
    }

    fn read_with_layout(reader: &mut ObjectReader, oid: &str, allow_legacy: bool) -> Result<Self> {
        let commit = reader.read(oid, "commit", MAX_COMMIT_BYTES)?;
        let (tree, parents) = commit_headers(&commit, oid.len())?;
        Self::read_tree(reader, oid, &tree, parents, allow_legacy)
    }

    fn read_tree(
        reader: &mut ObjectReader,
        oid: &str,
        tree: &str,
        parents: Vec<String>,
        allow_legacy: bool,
    ) -> Result<Self> {
        let mut entries = BTreeMap::new();
        read_tree(reader, tree, b"", 0, &mut entries)?;
        let metadata_bytes = read_regular(reader, &entries, meta::FILE, MAX_METADATA_BYTES)?;
        let metadata_text =
            std::str::from_utf8(&metadata_bytes).context("archive metadata is not UTF-8")?;
        let metadata = meta::parse_strict(metadata_text, oid)?;
        ensure!(
            (metadata.layout == meta::LayoutVersion::V1 || allow_legacy)
                && metadata.is_session_line()
                && !metadata.session.is_empty(),
            "archive history requires a claimed session in storage layout v1"
        );
        let mut metadata_facts = JsonFacts::parse(metadata_text)?;
        let JsonFacts::Object(fields) = &mut metadata_facts else {
            anyhow::bail!("archive metadata must be a JSON object");
        };
        fields.remove("kind");
        let log = if metadata.layout == meta::LayoutVersion::V1 {
            read_sequence(reader, &entries, meta::LOG_FILE)?
        } else {
            // Legacy VIEW-only markers remain valid: file edges compare the exact payload blobs
            // instead of imposing content-addressed sequence semantics on their native envelopes.
            for path in [meta::LEGACY_LOG_FILE, meta::LEGACY_VIEW_FILE] {
                let bytes = read_regular(reader, &entries, path, storage::MAX_MATERIALIZED_BYTES)?;
                std::str::from_utf8(&bytes).context("legacy materialized evidence is not UTF-8")?;
            }
            Vec::new()
        };
        let view = if metadata.layout == meta::LayoutVersion::V1 {
            read_sequence(reader, &entries, meta::VIEW_FILE)?
        } else {
            Vec::new()
        };
        let log_set: BTreeSet<&str> = log.iter().map(String::as_str).collect();
        ensure!(
            view.iter().all(|id| log_set.contains(id.as_str())),
            "archive VIEW references an event outside LOG"
        );
        for id in log_set {
            let path = meta::event_path(id)?;
            let entry = regular_entry(&entries, &path)?;
            let key = (entry.oid.clone(), id.to_owned());
            if reader.validated_events.contains(&key) {
                continue;
            }
            let bytes = reader.read(&entry.oid, "blob", MAX_OBJECT_BYTES)?;
            let text = std::str::from_utf8(&bytes).context("archive event is not UTF-8")?;
            ensure!(
                storage::event_id(text)? == id,
                "archive event content address differs"
            );
            reader.validated_events.insert(key);
            reader.validated_blobs.insert(entry.oid.clone());
        }
        for entry in entries.values() {
            if !entry.is_tree()
                && entry.mode != 0o160000
                && !reader.validated_blobs.contains(&entry.oid)
            {
                reader.read(&entry.oid, "blob", MAX_OBJECT_BYTES)?;
                reader.validated_blobs.insert(entry.oid.clone());
            }
        }
        Ok(Self {
            oid: oid.to_owned(),
            parents,
            metadata,
            metadata_facts,
            entries,
            log,
        })
    }
}

fn verify_snapshots(parent: &Snapshot, child: &Snapshot) -> Result<ArchiveEdgeFacts> {
    ensure!(
        parent.metadata.layout == meta::LayoutVersion::V1
            && child.metadata.layout == meta::LayoutVersion::V1,
        "archive edges require storage layout v1"
    );
    ensure!(
        child.parents.as_slice() == [parent.oid.as_str()],
        "archive edge does not have the expected single parent"
    );
    ensure!(
        child.metadata.kind == meta::Kind::Archive,
        "candidate is not an archive"
    );
    ensure!(
        child.metadata_facts == parent.metadata_facts,
        "archive changes metadata beyond its kind"
    );
    ensure!(
        child.log.len() > parent.log.len() && child.log.starts_with(&parent.log),
        "archive LOG must retain its exact prefix and append evidence"
    );
    verify_log_extension(parent, child)?;
    Ok(ArchiveEdgeFacts {
        parent: parent.oid.clone(),
        archive: child.oid.clone(),
        session: child.metadata.session.clone(),
        appended_records: child.log.len() - parent.log.len(),
    })
}

fn verify_log_extension(parent: &Snapshot, child: &Snapshot) -> Result<()> {
    let suffix = &child.log[parent.log.len()..];
    let mut additions = BTreeSet::new();
    for id in suffix {
        let path = meta::event_path(id)?.into_bytes();
        if !parent.entries.contains_key(&path) {
            let mut end = path.len();
            loop {
                additions.insert(path[..end].to_vec());
                let Some(index) = path[..end].iter().rposition(|byte| *byte == b'/') else {
                    break;
                };
                end = index;
            }
        }
    }
    for (path, old) in &parent.entries {
        let new = child
            .entries
            .get(path)
            .context("archive removes an inherited tree entry")?;
        ensure!(
            old.mode == new.mode,
            "archive changes an inherited file mode"
        );
        if old.is_tree() || path == meta::FILE.as_bytes() || path == meta::LOG_FILE.as_bytes() {
            continue;
        }
        ensure!(
            old.oid == new.oid,
            "archive changes inherited content outside metadata and LOG"
        );
    }
    for path in child.entries.keys() {
        ensure!(
            parent.entries.contains_key(path) || additions.contains(path),
            "archive adds a tree entry not required by its appended evidence"
        );
    }
    Ok(())
}

fn verify_file_snapshots(parent: &Snapshot, child: &Snapshot) -> Result<()> {
    let (JsonFacts::Object(before), JsonFacts::Object(after)) =
        (&parent.metadata_facts, &child.metadata_facts)
    else {
        anyhow::bail!("materialized session metadata must be an object");
    };
    ensure!(
        before
            .iter()
            .filter(|(name, _)| name.as_str() != "milestone")
            .eq(after
                .iter()
                .filter(|(name, _)| name.as_str() != "milestone")),
        "file edge changes materialized session metadata"
    );
    let payload = |path: &[u8]| {
        [
            meta::LOG_FILE,
            meta::VIEW_FILE,
            meta::LEGACY_LOG_FILE,
            meta::LEGACY_VIEW_FILE,
            meta::EVENTS_DIR,
        ]
        .iter()
        .any(|name| path == name.as_bytes())
            || path.starts_with(b"events/")
    };
    ensure!(
        parent
            .entries
            .iter()
            .filter(|(path, _)| payload(path))
            .eq(child.entries.iter().filter(|(path, _)| payload(path))),
        "file edge changes materialized session storage"
    );
    Ok(())
}

fn regular_entry<'a>(entries: &'a BTreeMap<Vec<u8>, Entry>, path: &str) -> Result<&'a Entry> {
    let entry = entries
        .get(path.as_bytes())
        .context("archive storage entry is missing")?;
    ensure!(
        entry.mode == 0o100644,
        "archive storage requires canonical regular-file modes"
    );
    Ok(entry)
}

fn read_regular(
    reader: &mut ObjectReader,
    entries: &BTreeMap<Vec<u8>, Entry>,
    path: &str,
    cap: usize,
) -> Result<Vec<u8>> {
    let entry = regular_entry(entries, path)?;
    let bytes = reader.read(&entry.oid, "blob", cap)?;
    reader.validated_blobs.insert(entry.oid.clone());
    Ok(bytes)
}

fn read_sequence(
    reader: &mut ObjectReader,
    entries: &BTreeMap<Vec<u8>, Entry>,
    path: &str,
) -> Result<Vec<String>> {
    let bytes = read_regular(reader, entries, path, storage::MAX_SEQUENCE_EVENTS * 41)?;
    storage::parse_sequence(std::str::from_utf8(&bytes).context("archive sequence is not UTF-8")?)
}

fn commit_headers(bytes: &[u8], oid_len: usize) -> Result<(String, Vec<String>)> {
    let end = bytes
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .context("archive commit has no header boundary")?;
    let mut tree = None;
    let mut parents = Vec::new();
    for line in bytes[..end].split(|byte| *byte == b'\n') {
        if let Some(value) = line.strip_prefix(b"tree ") {
            ensure!(tree.is_none(), "archive commit repeats its tree header");
            tree = Some(parse_oid_bytes(value, oid_len)?);
        } else if let Some(value) = line.strip_prefix(b"parent ") {
            parents.push(parse_oid_bytes(value, oid_len)?);
        } else {
            ensure!(
                line.starts_with(b" ") || line.contains(&b' '),
                "archive commit contains an invalid header"
            );
        }
    }
    Ok((tree.context("archive commit omits its tree")?, parents))
}

fn parse_oid_bytes(bytes: &[u8], oid_len: usize) -> Result<String> {
    let oid = std::str::from_utf8(bytes).context("archive object id is not UTF-8")?;
    ensure!(
        oid.len() == oid_len && valid_oid(oid),
        "archive object id is malformed"
    );
    Ok(oid.to_owned())
}

fn read_tree(
    reader: &mut ObjectReader,
    oid: &str,
    prefix: &[u8],
    depth: usize,
    entries: &mut BTreeMap<Vec<u8>, Entry>,
) -> Result<()> {
    ensure!(depth < MAX_DEPTH, "archive tree exceeds its depth limit");
    let bytes = reader.read(oid, "tree", MAX_OBJECT_BYTES)?;
    let mut remaining = bytes.as_slice();
    let mut previous = None;
    while !remaining.is_empty() {
        ensure!(
            reader.tree_entries < MAX_TREE_ENTRIES,
            "archive tree exceeds its entry limit"
        );
        reader.tree_entries += 1;
        let space = remaining
            .iter()
            .position(|byte| *byte == b' ')
            .context("archive tree entry omits its mode boundary")?;
        let mode = match &remaining[..space] {
            b"40000" => 0o40000,
            b"100644" => 0o100644,
            b"100755" => 0o100755,
            b"120000" => 0o120000,
            b"160000" => 0o160000,
            _ => anyhow::bail!("archive tree entry has a noncanonical mode"),
        };
        remaining = &remaining[space + 1..];
        let end = remaining
            .iter()
            .position(|byte| *byte == 0)
            .context("archive tree entry omits its name boundary")?;
        let name = &remaining[..end];
        ensure!(
            !name.is_empty() && !name.contains(&b'/') && name != b"." && name != b"..",
            "archive tree entry has an invalid name"
        );
        let mut key = name.to_vec();
        key.push(if mode == 0o40000 { b'/' } else { 0 });
        ensure!(
            previous.as_ref().is_none_or(|old| old < &key),
            "archive tree entries are not ordered uniquely"
        );
        previous = Some(key);
        let mut path = prefix.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(name);
        ensure!(
            path.len() <= MAX_PATH_BYTES,
            "archive path exceeds its length limit"
        );
        remaining = &remaining[end + 1..];
        let binary_len = oid.len() / 2;
        ensure!(
            remaining.len() >= binary_len,
            "archive tree ends inside an object id"
        );
        let entry_oid = hex::encode(&remaining[..binary_len]);
        remaining = &remaining[binary_len..];
        ensure!(
            entries
                .insert(
                    path.clone(),
                    Entry {
                        mode,
                        oid: entry_oid.clone()
                    }
                )
                .is_none(),
            "archive tree repeats a path"
        );
        if mode == 0o40000 {
            read_tree(reader, &entry_oid, &path, depth + 1, entries)?;
        }
    }
    Ok(())
}

struct ObjectReader {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    bytes: usize,
    tree_entries: usize,
    validated_events: HashSet<(String, String)>,
    validated_blobs: HashSet<String>,
}

impl ObjectReader {
    fn new(repo: &Repo) -> Result<Self> {
        let mut command = Command::new("git");
        command
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(repo.root())
            .args(["cat-file", "--batch"])
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for key in GIT_ROUTING_ENV {
            command.env_remove(key);
        }
        let mut child = command
            .spawn()
            .context("cannot start local archive object reader")?;
        let input = child
            .stdin
            .take()
            .context("archive object reader has no input")?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .context("archive object reader has no output")?,
        );
        Ok(Self {
            child,
            input,
            output,
            bytes: 0,
            tree_entries: 0,
            validated_events: HashSet::new(),
            validated_blobs: HashSet::new(),
        })
    }

    fn read(&mut self, oid: &str, kind: &str, cap: usize) -> Result<Vec<u8>> {
        ensure!(
            valid_oid(oid),
            "archive read requires an immutable object id"
        );
        writeln!(self.input, "{oid}").context("cannot request archive object")?;
        self.input
            .flush()
            .context("cannot flush archive object request")?;
        let mut header = Vec::new();
        self.output
            .by_ref()
            .take(256)
            .read_until(b'\n', &mut header)
            .context("cannot read archive object header")?;
        ensure!(
            header.last() == Some(&b'\n'),
            "archive object header is missing or excessive"
        );
        let text = std::str::from_utf8(&header).context("archive object header is not UTF-8")?;
        let fields: Vec<_> = text.trim_end_matches('\n').split(' ').collect();
        ensure!(
            fields.len() == 3 && fields[0] == oid && fields[1] == kind,
            "archive object is missing or has the wrong type"
        );
        let size: usize = fields[2]
            .parse()
            .context("archive object size is invalid")?;
        ensure!(size <= cap, "archive object exceeds its byte limit");
        self.bytes = self
            .bytes
            .checked_add(size)
            .context("archive read budget overflow")?;
        ensure!(
            self.bytes <= MAX_TOTAL_BYTES,
            "archive proof exceeds its aggregate byte limit"
        );
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .context("cannot allocate bounded archive object")?;
        bytes.resize(size, 0);
        self.output
            .read_exact(&mut bytes)
            .context("archive object is truncated or corrupt")?;
        let mut separator = [0];
        self.output
            .read_exact(&mut separator)
            .context("archive object has no separator")?;
        ensure!(
            separator == *b"\n",
            "archive object has an invalid separator"
        );
        Ok(bytes)
    }
}

impl Drop for ObjectReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transcript;

    const SESSION: &str = "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct Fixture {
        _directory: tempfile::TempDir,
        repo: Repo,
        original: String,
        metadata: meta::Meta,
        source: String,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(directory.path()).unwrap();
            repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
            let mut metadata = meta::Meta::new(SESSION.into(), "codex".into(), "/work".into());
            metadata.turn = Some(1);
            let original = native("inherited");
            meta::write(repo.root(), &metadata).unwrap();
            storage::write_snapshot(repo.root(), &original, &original).unwrap();
            std::fs::write(repo.root().join("AGENTS.md"), "Shared instructions\n").unwrap();
            repo.add_all().unwrap();
            repo.commit("source fixture").unwrap();
            let source = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().into();
            Self {
                _directory: directory,
                repo,
                original,
                metadata,
                source,
            }
        }

        fn stage_archive(&self, log: &str, view: &str) {
            let mut metadata = self.metadata.clone();
            metadata.kind = meta::Kind::Archive;
            meta::write(self.repo.root(), &metadata).unwrap();
            storage::write_snapshot(self.repo.root(), log, view).unwrap();
        }

        fn commit(&self) -> String {
            self.repo.add_all().unwrap();
            self.repo.commit("archive fixture").unwrap();
            self.repo.git(&["rev-parse", "HEAD"]).unwrap().trim().into()
        }

        fn commit_tree(&self, parents: &[&str]) -> String {
            self.repo.add_all().unwrap();
            let tree = self.repo.git(&["write-tree"]).unwrap();
            let mut args = vec!["commit-tree", tree.trim(), "-m", "archive topology fixture"];
            for parent in parents {
                args.extend(["-p", parent]);
            }
            self.repo.git(&args).unwrap().trim().into()
        }
    }

    fn native(text: &str) -> String {
        transcript::wrap_lines(
            &format!("{}\n", serde_json::json!({ "type": "event", "text": text })),
            "codex",
            SESSION,
        )
    }

    #[test]
    fn source_coordinates_use_raw_parents_and_retained_logs_after_graph_changes() {
        for shallow in [false, true] {
            let fixture = Fixture::new();
            let log = format!("{}{}", fixture.original, native("second turn"));
            let head = turn_snapshot(&fixture, &log, &log);
            let proof = freeze_source_log(&fixture.repo, &head).unwrap();
            assert_eq!(proof.turn_lines(2).unwrap(), (2, vec![1]));
            assert_eq!(proof.turn_lines(refs::LAST_TURN).unwrap(), (2, vec![1]));
            assert_eq!(proof.turn_lines(1).unwrap(), (1, vec![0]));
            assert!(proof.turn_lines(3).is_err());
            assert!(proof.log_at(&fixture.source).is_err());
            let path = fixture
                .repo
                .git_path(if shallow { "shallow" } else { "info/grafts" })
                .unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("{head}\n")).unwrap();
            assert_eq!(proof.turn_lines(2).unwrap(), (2, vec![1]));
            assert_eq!(proof.log_at(&head).unwrap(), log);
            let reread = freeze_source_log(&fixture.repo, &head).unwrap();
            assert_eq!(reread.turn_lines(2).unwrap(), (2, vec![1]));
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn source_proof_requires_the_real_root_within_its_budget() {
        let fixture = Fixture::new();
        let log = format!("{}{}", fixture.original, native("second turn"));
        let head = turn_snapshot(&fixture, &log, &log);
        assert!(freeze_source_log_with_limit(&fixture.repo, &head, 1).is_err());
        assert!(freeze_source_log_with_limit(&fixture.repo, &head, 2).is_ok());
        let parent = fixture
            .repo
            .git_path(&format!(
                "objects/{}/{}",
                &fixture.source[..2],
                &fixture.source[2..]
            ))
            .unwrap();
        std::fs::remove_file(parent).unwrap();
        std::fs::write(
            fixture.repo.git_path("shallow").unwrap(),
            format!("{head}\n"),
        )
        .unwrap();
        assert!(freeze_source_log(&fixture.repo, &head).is_err());
    }

    #[test]
    fn source_coordinates_refuse_rewritten_parent_or_head_logs() {
        let fixture = Fixture::new();
        let log = native("rewritten turn");
        let head = turn_snapshot(&fixture, &log, &log);
        let proof = freeze_source_log(&fixture.repo, &head).unwrap();
        assert!(proof.turn_lines(2).is_err());
        assert!(proof.turn_lines(1).is_err());
    }

    #[test]
    fn source_coordinates_canonicalize_legacy_logs_before_v1_selection() {
        let fixture = Fixture::new();
        let mut metadata = fixture.metadata.clone();
        metadata.layout = meta::LayoutVersion::V0;
        metadata.kind = meta::Kind::File;
        meta::write(fixture.repo.root(), &metadata).unwrap();
        std::fs::write(
            fixture.repo.root().join(meta::LEGACY_LOG_FILE),
            format!("  {}", fixture.original),
        )
        .unwrap();
        fixture.commit();
        meta::write(fixture.repo.root(), &fixture.metadata).unwrap();
        let log = format!("{}{}", fixture.original, native("second turn"));
        let head = turn_snapshot(&fixture, &log, &log);
        let proof = freeze_source_log(&fixture.repo, &head).unwrap();
        assert_eq!(proof.turn_lines(2).unwrap(), (2, vec![1]));
        assert_eq!(proof.log_at(&head).unwrap(), log);
    }

    fn file_snapshot(fixture: &Fixture, label: &str) -> String {
        let mut metadata = meta::read(fixture.repo.root()).unwrap();
        metadata.kind = meta::Kind::File;
        metadata.milestone = None;
        meta::write(fixture.repo.root(), &metadata).unwrap();
        std::fs::write(fixture.repo.root().join("AGENTS.md"), label).unwrap();
        fixture.commit()
    }

    fn turn_snapshot(fixture: &Fixture, log: &str, view: &str) -> String {
        let mut metadata = meta::read(fixture.repo.root()).unwrap();
        metadata.kind = meta::Kind::Turn;
        metadata.turn = Some(metadata.turn.unwrap_or(0) + 1);
        meta::write(fixture.repo.root(), &metadata).unwrap();
        storage::write_snapshot(fixture.repo.root(), log, view).unwrap();
        fixture.commit()
    }

    #[test]
    fn archive_target_accepts_turn_context_changes_and_strict_file_archive_edges() {
        let fixture = Fixture::new();
        let new = transcript::wrap_lines(
            "{\"type\":\"user\",\"message\":{\"content\":\"continued\"}}\n",
            "claude-code",
            SESSION,
        );
        let log = format!("{}{}{new}", fixture.original, fixture.original);
        let mut metadata = fixture.metadata.clone();
        metadata.runtime = "claude-code".into();
        metadata.code = Some("a".repeat(40));
        meta::write(fixture.repo.root(), &metadata).unwrap();
        let turn = turn_snapshot(&fixture, &log, &new);
        let file = file_snapshot(&fixture, "Shared memory after continuation\n");
        metadata = meta::read(fixture.repo.root()).unwrap();
        metadata.kind = meta::Kind::Archive;
        meta::write(fixture.repo.root(), &metadata).unwrap();
        storage::write_snapshot(fixture.repo.root(), &format!("{log}{new}"), &new).unwrap();
        let archive = fixture.commit();
        let refs = fixture.repo.git(&["show-ref"]).unwrap();
        let index = std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap();
        let config = std::fs::read(fixture.repo.git_path("config").unwrap()).unwrap();

        for tip in [&fixture.source, &turn, &file, &archive] {
            verify_archive_append_target(&fixture.repo, &fixture.source, tip).unwrap();
        }
        assert!(verify_materialized_chain(&fixture.repo, &fixture.source, &archive).is_err());
        assert!(
            verify_archive_append_target_with_limit(&fixture.repo, &fixture.source, &archive, 2)
                .is_err()
        );
        assert_eq!(fixture.repo.git(&["show-ref"]).unwrap(), refs);
        assert_eq!(
            std::fs::read(fixture.repo.git_path("index").unwrap()).unwrap(),
            index
        );
        assert_eq!(
            std::fs::read(fixture.repo.git_path("config").unwrap()).unwrap(),
            config
        );
    }

    #[test]
    fn archive_target_checks_each_occurrence_and_event_object_before_restoration() {
        for mutation in [
            "occurrence",
            "event-delete",
            "event-rewrite",
            "event-mode",
            "session",
        ] {
            let mut fixture = Fixture::new();
            let log = format!("{}{}", fixture.original, fixture.original);
            storage::write_snapshot(fixture.repo.root(), &log, &fixture.original).unwrap();
            let extra = native("retained object outside VIEW and LOG");
            let extra_path = fixture
                .repo
                .root()
                .join(meta::event_path(&storage::event_id(&extra).unwrap()).unwrap());
            std::fs::create_dir_all(extra_path.parent().unwrap()).unwrap();
            std::fs::write(&extra_path, &extra).unwrap();
            fixture.source = fixture.commit();
            let valid = turn_snapshot(&fixture, &log, &fixture.original);
            verify_archive_append_target(&fixture.repo, &fixture.source, &valid).unwrap();
            match mutation {
                "occurrence" => storage::write_snapshot(
                    fixture.repo.root(),
                    &fixture.original,
                    &fixture.original,
                )
                .unwrap(),
                "event-delete" => std::fs::remove_file(&extra_path).unwrap(),
                "event-rewrite" => std::fs::write(&extra_path, native("different object")).unwrap(),
                "event-mode" => {
                    fixture
                        .repo
                        .git(&[
                            "update-index",
                            "--chmod=+x",
                            extra_path
                                .strip_prefix(fixture.repo.root())
                                .unwrap()
                                .to_str()
                                .unwrap(),
                        ])
                        .unwrap();
                }
                "session" => {
                    let mut metadata = meta::read(fixture.repo.root()).unwrap();
                    metadata.session = format!("agit-{}", "e".repeat(40));
                    meta::write(fixture.repo.root(), &metadata).unwrap();
                }
                _ => unreachable!(),
            }
            let bad = if mutation == "event-mode" {
                let tree = fixture.repo.git(&["write-tree"]).unwrap();
                let bad = fixture
                    .repo
                    .git(&[
                        "commit-tree",
                        &tree,
                        "-p",
                        &valid,
                        "-m",
                        "event mode fixture",
                    ])
                    .unwrap();
                fixture
                    .repo
                    .git(&["update-ref", "refs/heads/main", &bad, &valid])
                    .unwrap();
                bad
            } else {
                fixture.commit()
            };
            fixture.repo.git(&["read-tree", &valid]).unwrap();
            let tree = fixture.repo.git(&["write-tree"]).unwrap();
            let restored = fixture
                .repo
                .git(&[
                    "commit-tree",
                    &tree,
                    "-p",
                    &bad,
                    "-m",
                    "restored endpoint fixture",
                ])
                .unwrap();
            assert!(
                verify_archive_append_target(&fixture.repo, &fixture.source, &bad).is_err(),
                "accepted {mutation}"
            );
            assert!(
                verify_archive_append_target(&fixture.repo, &fixture.source, &restored).is_err(),
                "accepted restored {mutation}"
            );
        }
    }

    #[test]
    fn archive_target_refuses_unknown_edges_and_invalid_sources() {
        for kind in [meta::Kind::View, meta::Kind::Merge] {
            let fixture = Fixture::new();
            let mut metadata = fixture.metadata.clone();
            metadata.kind = kind;
            meta::write(fixture.repo.root(), &metadata).unwrap();
            let tip = fixture.commit();
            assert!(verify_archive_append_target(&fixture.repo, &fixture.source, &tip).is_err());
        }
        let fixture = Fixture::new();
        assert!(verify_archive_append_target(&fixture.repo, "HEAD", &fixture.source).is_err());
        let mut metadata = fixture.metadata.clone();
        metadata.layout = meta::LayoutVersion::V0;
        meta::write(fixture.repo.root(), &metadata).unwrap();
        let legacy = fixture.commit();
        assert!(verify_archive_append_target(&fixture.repo, &fixture.source, &legacy).is_err());
        assert!(verify_archive_append_target(&fixture.repo, &legacy, &legacy).is_err());
        std::fs::write(fixture.repo.root().join(meta::FILE), "{}").unwrap();
        let invalid = fixture.commit();
        assert!(verify_archive_append_target(&fixture.repo, &invalid, &invalid).is_err());
    }

    #[test]
    fn archive_target_uses_actual_parents_despite_grafts_shallow_and_replacements() {
        let fixture = Fixture::new();
        let bad = turn_snapshot(&fixture, "", "");
        let restored = turn_snapshot(&fixture, &fixture.original, &fixture.original);
        let graft = fixture.repo.git_path("info/grafts").unwrap();
        std::fs::create_dir_all(graft.parent().unwrap()).unwrap();
        std::fs::write(&graft, format!("{restored} {}\n", fixture.source)).unwrap();
        assert_eq!(
            fixture
                .repo
                .git(&["rev-list", "--first-parent", &restored])
                .unwrap()
                .lines()
                .count(),
            2
        );
        assert!(verify_archive_append_target(&fixture.repo, &fixture.source, &restored).is_err());
        std::fs::remove_file(graft).unwrap();
        let shallow = fixture.repo.git_path("shallow").unwrap();
        std::fs::write(&shallow, format!("{restored}\n")).unwrap();
        assert_eq!(
            fixture
                .repo
                .git(&["rev-list", "--first-parent", &restored])
                .unwrap(),
            restored
        );
        assert!(verify_archive_append_target(&fixture.repo, &fixture.source, &restored).is_err());
        std::fs::remove_file(shallow).unwrap();
        let tree = fixture
            .repo
            .git(&["rev-parse", &format!("{restored}^{{tree}}")])
            .unwrap();
        let synthetic = fixture
            .repo
            .git(&[
                "commit-tree",
                &tree,
                "-p",
                &fixture.source,
                "-m",
                "alternate parent fixture",
            ])
            .unwrap();
        fixture
            .repo
            .git(&["replace", &restored, &synthetic])
            .unwrap();
        let replaced = Command::new("git")
            .arg("-C")
            .arg(fixture.repo.root())
            .args(["rev-list", "--first-parent", &restored])
            .env_remove("GIT_NO_REPLACE_OBJECTS")
            .output()
            .unwrap();
        assert!(replaced.status.success());
        assert!(
            !String::from_utf8(replaced.stdout)
                .unwrap()
                .lines()
                .any(|oid| oid == bad)
        );
        verify_archive_append_target(&fixture.repo, &fixture.source, &synthetic).unwrap();
        assert!(verify_archive_append_target(&fixture.repo, &fixture.source, &restored).is_err());
    }

    #[test]
    fn materialized_chain_accepts_archive_and_file_interleavings_with_one_budget() {
        let mut fixture = Fixture::new();
        fixture.metadata.milestone = Some("completed phase".into());
        meta::write(fixture.repo.root(), &fixture.metadata).unwrap();
        fixture.source = fixture.commit();
        let first_file = file_snapshot(&fixture, "Collected memory\n");
        fixture.metadata.milestone = None;
        let first_log = format!("{}{}", fixture.original, native("exploration"));
        fixture.stage_archive(&first_log, &fixture.original);
        let archive = fixture.commit();
        let last_file = file_snapshot(&fixture, "Collected later memory\n");
        let last_log = format!("{first_log}{}", native("late response"));
        fixture.stage_archive(&last_log, &fixture.original);
        let tip = fixture.commit();

        verify_materialized_chain(&fixture.repo, &fixture.source, &tip).unwrap();
        verify_materialized_chain(&fixture.repo, &first_file, &last_file).unwrap();
        verify_materialized_chain(&fixture.repo, &archive, &tip).unwrap();
        assert!(verify_chain(&fixture.repo, &fixture.source, &tip).is_err());
        assert!(
            verify_materialized_chain_with_limit(&fixture.repo, &fixture.source, &tip, 3).is_err()
        );
        assert_eq!(
            fixture.repo.show_raw(&tip, "AGENTS.md").unwrap(),
            "Collected later memory\n"
        );
    }

    #[test]
    fn file_edges_cannot_hide_restored_storage_or_metadata_changes() {
        for mutation in ["view", "log", "runtime", "turn", "code", "unknown"] {
            let fixture = Fixture::new();
            let metadata_path = fixture.repo.root().join(meta::FILE);
            let original_metadata = std::fs::read(&metadata_path).unwrap();
            let original_log = std::fs::read(fixture.repo.root().join(meta::LOG_FILE)).unwrap();
            let original_view = std::fs::read(fixture.repo.root().join(meta::VIEW_FILE)).unwrap();
            let mut changed: serde_json::Value =
                serde_json::from_slice(&original_metadata).unwrap();
            changed["kind"] = serde_json::json!("file");
            match mutation {
                "view" => std::fs::write(fixture.repo.root().join(meta::VIEW_FILE), "").unwrap(),
                "log" => storage::write_snapshot(
                    fixture.repo.root(),
                    &format!("{}{}", fixture.original, fixture.original),
                    &fixture.original,
                )
                .unwrap(),
                "runtime" => changed["runtime"] = serde_json::json!("claude-code"),
                "turn" => changed["turn"] = serde_json::json!(2),
                "code" => changed["code"] = serde_json::json!("f".repeat(40)),
                "unknown" => changed["future"] = serde_json::json!({"retained":false}),
                _ => unreachable!(),
            }
            std::fs::write(&metadata_path, serde_json::to_vec(&changed).unwrap()).unwrap();
            fixture.commit();
            std::fs::write(&metadata_path, original_metadata).unwrap();
            std::fs::write(fixture.repo.root().join(meta::LOG_FILE), original_log).unwrap();
            std::fs::write(fixture.repo.root().join(meta::VIEW_FILE), original_view).unwrap();
            let restored = file_snapshot(&fixture, "Collected memory\n");
            assert!(
                verify_materialized_chain(&fixture.repo, &fixture.source, &restored).is_err(),
                "accepted intervening {mutation} mutation"
            );
        }
    }

    #[test]
    fn legacy_file_watermarks_preserve_view_only_evidence_and_check_every_edge() {
        let fixture = Fixture::new();
        let mut metadata = fixture.metadata.clone();
        metadata.layout = meta::LayoutVersion::V0;
        meta::write(fixture.repo.root(), &metadata).unwrap();
        for path in [meta::LOG_FILE, meta::VIEW_FILE] {
            std::fs::remove_file(fixture.repo.root().join(path)).unwrap();
        }
        std::fs::remove_dir_all(fixture.repo.root().join(meta::EVENTS_DIR)).unwrap();
        let view = format!("{}{}", fixture.original, native("legacy VIEW marker"));
        std::fs::write(
            fixture.repo.root().join(meta::LEGACY_LOG_FILE),
            &fixture.original,
        )
        .unwrap();
        std::fs::write(fixture.repo.root().join(meta::LEGACY_VIEW_FILE), &view).unwrap();
        let source = fixture.commit();
        let valid = file_snapshot(&fixture, "Collected legacy memory\n");
        verify_materialized_chain(&fixture.repo, &source, &valid).unwrap();
        assert!(verify_chain(&fixture.repo, &source, &valid).is_err());

        std::fs::write(fixture.repo.root().join(meta::LEGACY_VIEW_FILE), "").unwrap();
        file_snapshot(&fixture, "Transient context\n");
        std::fs::write(fixture.repo.root().join(meta::LEGACY_VIEW_FILE), view).unwrap();
        let restored = file_snapshot(&fixture, "Collected legacy memory\n");
        assert!(verify_materialized_chain(&fixture.repo, &source, &restored).is_err());
    }

    #[test]
    fn materialized_chain_reads_stored_parents_despite_local_graph_boundaries() {
        let fixture = Fixture::new();
        std::fs::write(fixture.repo.root().join(meta::VIEW_FILE), "").unwrap();
        file_snapshot(&fixture, "Transient context\n");
        storage::write_snapshot(fixture.repo.root(), &fixture.original, &fixture.original).unwrap();
        let restored = file_snapshot(&fixture, "Shared instructions\n");
        let graft = fixture.repo.git_path("info/grafts").unwrap();
        std::fs::create_dir_all(graft.parent().unwrap()).unwrap();
        std::fs::write(&graft, format!("{restored} {}\n", fixture.source)).unwrap();
        let graph = fixture
            .repo
            .git(&["rev-list", "--first-parent", &restored])
            .unwrap();
        assert_eq!(graph.lines().count(), 2);
        assert!(verify_materialized_chain(&fixture.repo, &fixture.source, &restored).is_err());
        std::fs::remove_file(graft).unwrap();
        std::fs::write(
            fixture.repo.git_path("shallow").unwrap(),
            format!("{restored}\n"),
        )
        .unwrap();
        let graph = fixture
            .repo
            .git(&["rev-list", "--first-parent", &restored])
            .unwrap();
        assert_eq!(graph, restored);
        assert!(verify_materialized_chain(&fixture.repo, &fixture.source, &restored).is_err());
    }

    /// Repeated occurrences remain evidence even when their content-addressed object is inherited.
    #[test]
    fn archive_chain_retains_occurrences_without_mutating_repository() {
        let fixture = Fixture::new();
        let first_log = format!("{}{}", fixture.original, fixture.original);
        fixture.stage_archive(&first_log, &fixture.original);
        let first = fixture.commit();
        let final_log = format!("{first_log}{}", native("late final answer"));
        fixture.stage_archive(&final_log, &fixture.original);
        let last = fixture.commit();
        let refs = fixture.repo.git(&["show-ref"]).unwrap();
        let index_path = fixture.repo.git_path("index").unwrap();
        let index = std::fs::read(&index_path).unwrap();
        let status = fixture.repo.git(&["status", "--porcelain=v1"]).unwrap();

        let proof = verify_chain(&fixture.repo, &fixture.source, &last).unwrap();
        assert_eq!(proof.source(), fixture.source);
        assert_eq!(proof.candidate(), last);
        assert_eq!(proof.session(), SESSION);
        assert_eq!(proof.edges().len(), 2);
        assert_eq!(proof.edges()[0].parent(), fixture.source);
        assert_eq!(proof.edges()[0].archive(), first);
        assert_eq!(proof.edges()[0].session(), SESSION);
        assert_eq!(proof.edges()[0].appended_records(), 1);
        assert_eq!(
            proof.edges()[1],
            verify_edge(&fixture.repo, &first, &last).unwrap()
        );
        assert!(verify_chain_with_limit(&fixture.repo, &fixture.source, &last, 1).is_err());
        assert_eq!(fixture.repo.git(&["show-ref"]).unwrap(), refs);
        assert_eq!(std::fs::read(index_path).unwrap(), index);
        assert_eq!(
            fixture.repo.git(&["status", "--porcelain=v1"]).unwrap(),
            status
        );
    }

    /// An unchanged endpoint still requires readable storage and a claimed session.
    #[test]
    fn empty_chain_validates_source_and_rejects_mutable_endpoints() {
        let fixture = Fixture::new();
        assert!(
            verify_chain(&fixture.repo, &fixture.source, &fixture.source)
                .unwrap()
                .edges()
                .is_empty()
        );
        assert!(verify_chain(&fixture.repo, "HEAD", "HEAD").is_err());
        assert!(verify_edge(&fixture.repo, &fixture.source, &fixture.source[..8]).is_err());
        std::fs::write(
            fixture.repo.root().join(meta::VIEW_FILE),
            format!("{}\n", "f".repeat(40)),
        )
        .unwrap();
        let bad = fixture.commit();
        assert!(verify_chain(&fixture.repo, &bad, &bad).is_err());
    }

    /// Equal final VIEW bytes cannot hide a change in the traversed history.
    #[test]
    fn intervening_view_change_and_nonarchive_history_refuse() {
        let fixture = Fixture::new();
        let first_log = format!("{}{}", fixture.original, native("exploration"));
        fixture.stage_archive(&first_log, &first_log);
        let changed_view = fixture.commit();
        let final_log = format!("{first_log}{}", native("late answer"));
        fixture.stage_archive(&final_log, &fixture.original);
        let restored_view = fixture.commit();
        assert!(verify_chain(&fixture.repo, &fixture.source, &restored_view).is_err());
        assert!(verify_edge(&fixture.repo, &changed_view, &restored_view).is_err());

        let fixture = Fixture::new();
        fixture.stage_archive(
            &format!("{}{}", fixture.original, native("exploration")),
            &fixture.original,
        );
        meta::write(fixture.repo.root(), &fixture.metadata).unwrap();
        let ordinary = fixture.commit();
        assert!(verify_chain(&fixture.repo, &fixture.source, &ordinary).is_err());
    }

    /// Archive naming cannot authorize shared-file changes, extra objects, or reordered evidence.
    #[test]
    fn archive_shape_rejects_unrelated_tree_and_sequence_changes() {
        for mutation in [
            "shared",
            "mode",
            "extra",
            "rewrite",
            "empty",
            "turn",
            "unknown",
            "file-line",
        ] {
            let fixture = Fixture::new();
            let appended = native("exploration");
            let log = format!("{}{appended}", fixture.original);
            fixture.stage_archive(&log, &fixture.original);
            match mutation {
                "shared" => {
                    std::fs::write(fixture.repo.root().join("AGENTS.md"), "Different\n").unwrap()
                }
                "mode" => {
                    fixture.repo.add_all().unwrap();
                    fixture
                        .repo
                        .git(&["update-index", "--chmod=+x", "AGENTS.md"])
                        .unwrap();
                    let tree = fixture.repo.git(&["write-tree"]).unwrap();
                    let child = fixture
                        .repo
                        .git(&[
                            "commit-tree",
                            tree.trim(),
                            "-p",
                            &fixture.source,
                            "-m",
                            "mode fixture",
                        ])
                        .unwrap();
                    assert!(verify_edge(&fixture.repo, &fixture.source, child.trim()).is_err());
                    continue;
                }
                "extra" => {
                    let envelope = native("unreferenced");
                    let id = storage::event_id(&envelope).unwrap();
                    let path = fixture.repo.root().join(meta::event_path(&id).unwrap());
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, envelope).unwrap();
                }
                "rewrite" => storage::write_snapshot(
                    fixture.repo.root(),
                    &format!("{appended}{}", fixture.original),
                    &fixture.original,
                )
                .unwrap(),
                "empty" => storage::write_snapshot(
                    fixture.repo.root(),
                    &fixture.original,
                    &fixture.original,
                )
                .unwrap(),
                "turn" | "unknown" | "file-line" => {
                    let path = fixture.repo.root().join(meta::FILE);
                    let mut value: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    match mutation {
                        "turn" => value["turn"] = serde_json::json!(2),
                        "unknown" => value["extension"] = serde_json::json!({ "authority": true }),
                        _ => value["line"] = serde_json::json!("file"),
                    }
                    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
                }
                _ => unreachable!(),
            }
            let child = fixture.commit();
            assert!(
                verify_edge(&fixture.repo, &fixture.source, &child).is_err(),
                "accepted mutation: {mutation}"
            );
        }
    }

    /// The proof follows immutable commit parents even when local replacement refs redirect Git.
    #[test]
    fn exact_topology_and_replacement_objects_cannot_hide_changes() {
        let fixture = Fixture::new();
        fixture.stage_archive(
            &format!("{}{}", fixture.original, native("exploration")),
            &fixture.original,
        );
        let valid = fixture.commit_tree(&[&fixture.source]);
        verify_edge(&fixture.repo, &fixture.source, &valid).unwrap();
        let merge = fixture.commit_tree(&[&fixture.source, &valid]);
        assert!(verify_edge(&fixture.repo, &fixture.source, &merge).is_err());
        let orphan = fixture.commit_tree(&[]);
        assert!(verify_edge(&fixture.repo, &fixture.source, &orphan).is_err());

        std::fs::write(fixture.repo.root().join("AGENTS.md"), "Unrelated edit\n").unwrap();
        let invalid = fixture.commit_tree(&[&fixture.source]);
        fixture.repo.git(&["replace", &invalid, &valid]).unwrap();
        let replaced = Command::new("git")
            .arg("-C")
            .arg(fixture.repo.root())
            .args(["show", &format!("{invalid}:AGENTS.md")])
            .env_remove("GIT_NO_REPLACE_OBJECTS")
            .output()
            .unwrap();
        assert!(replaced.status.success());
        assert_eq!(replaced.stdout, b"Shared instructions\n");
        assert!(verify_edge(&fixture.repo, &fixture.source, &invalid).is_err());
        verify_edge(&fixture.repo, &fixture.source, &valid).unwrap();
    }

    /// Missing immutable event objects cannot be repaired by consulting the materialized worktree.
    #[test]
    fn missing_event_object_refuses_despite_intact_worktree_bytes() {
        let fixture = Fixture::new();
        let appended = native("exploration");
        fixture.stage_archive(
            &format!("{}{appended}", fixture.original),
            &fixture.original,
        );
        let child = fixture.commit();
        let id = storage::event_id(&appended).unwrap();
        let path = meta::event_path(&id).unwrap();
        let oid = fixture
            .repo
            .git(&["rev-parse", &format!("{child}:{path}")])
            .unwrap();
        let oid = oid.trim();
        let object = fixture
            .repo
            .git_path(&format!("objects/{}/{}", &oid[..2], &oid[2..]))
            .unwrap();
        std::fs::remove_file(object).unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.repo.root().join(path)).unwrap(),
            appended
        );
        assert!(verify_edge(&fixture.repo, &fixture.source, &child).is_err());
    }

    /// Unknown fields retain exact number tokens and nested duplicate keys cannot erase evidence.
    #[test]
    fn metadata_facts_preserve_extensions_without_float_rounding() {
        assert_eq!(
            JsonFacts::parse("{\"b\":true,\"a\":[null,\"x\"]}").unwrap(),
            JsonFacts::parse(" { \"a\": [ null, \"x\" ], \"b\": true } ").unwrap()
        );
        assert_ne!(
            JsonFacts::parse("{\"extension\":9007199254740992.0}").unwrap(),
            JsonFacts::parse("{\"extension\":9007199254740993.0}").unwrap()
        );
        for text in [
            "{\"x\":1,\"x\":2}",
            "{\"x\":{\"k\":1,\"k\":2}}",
            "[] true",
            "{\"x\":}",
            "[1,]",
        ] {
            assert!(
                JsonFacts::parse(text).is_err(),
                "accepted malformed metadata: {text}"
            );
        }
    }
}
