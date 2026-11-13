//! What readers of an encrypted session were shown, kept for its turns once it continues as plain.
//!
//! An encrypted snapshot published a public projection of each record next to the envelope, under
//! the rules of the client that published it. Those rules could hide more than plain capture does
//! today (an older detector, legacy tokens, another omission setting), so a plain snapshot that
//! carries those records publishes them as they were projected, and only later records as they
//! are now.

use super::*;
use crate::domain::{storage, transcript::Envelope};
use std::{cell::OnceCell, collections::HashMap, rc::Rc};

const ENVELOPE: &str = "privacy/envelope.json";

/// The most commits that touched an envelope one merge's history may hold for a push to check.
const MAX_ENVELOPE_COMMITS: usize = 4096;

/// How many paths one tree listing asks for.
const LS_TREE_BATCH: usize = 64;

/// The most commits one probe of the lines between two snapshots lists; a longer answer counts as a
/// line that may bypass the plain snapshot.
const MAX_LINE_COMMITS: usize = 4096;

/// How many commits of a first-parent line are listed at a time.
const FIRST_PARENT_PAGE: usize = 1024;

/// The public lines an encrypted snapshot published.
pub(super) enum Published {
    /// Keyed by the local record each line projects: this device holds the snapshot's source, and
    /// a record is found wherever a later LOG carries it.
    ByRecord(HashMap<String, String>),
    /// By position: this device holds the snapshot only as published, and a LOG continuing it
    /// starts with the records it projected. `originals` are the snapshot's own records, by
    /// [`record_key`], when this device holds them decrypted.
    ByPosition {
        lines: Vec<String>,
        originals: Option<Vec<String>>,
    },
}

/// What a push says when records an encrypted snapshot published can't be told apart from new ones.
const UNPROVEN: &str = "a conversation merged into this one was published encrypted, and this computer can't tell which of its turns were already published; nothing was sent. Continue from its latest plain turn without merging the older encrypted one back in, or unlock that conversation's originals here first with `agit privacy unlock`";

/// What a push says when a later LOG doesn't continue the records an encrypted snapshot published.
const MISALIGNED: &str = "an earlier encrypted snapshot's published view doesn't line up with its turns, so it can't continue as plain; nothing was sent";

impl Published {
    fn projected(source: &Repo, source_oid: &str, retained: &Repo, public: &str) -> Result<Self> {
        let local = log_lines(source, source_oid)?;
        let projected = log_lines(retained, public)?;
        ensure!(local.len() == projected.len(), MISALIGNED);
        let mut by_record = HashMap::with_capacity(local.len());
        for (line, projected) in local.iter().zip(projected) {
            by_record
                .entry(record_key(&storage::parse_legacy_envelope_line(line)?))
                .or_insert(projected);
        }
        Ok(Self::ByRecord(by_record))
    }

    fn held(repo: &Repo, public: &str) -> Result<Self> {
        Ok(Self::ByPosition {
            lines: log_lines(repo, public)?,
            originals: held_originals(repo, public)?,
        })
    }

    /// Refuses a later LOG that doesn't start with the records this snapshot published, when they
    /// are matched by position: by their own records where this device holds them, otherwise by
    /// each record's runtime, type and role.
    pub(super) fn align(&self, later: &[Envelope]) -> Result<()> {
        let Self::ByPosition { lines, originals } = self else {
            return Ok(());
        };
        ensure!(later.len() >= lines.len(), MISALIGNED);
        if let Some(originals) = originals {
            ensure!(originals.len() == lines.len(), MISALIGNED);
        }
        // Each record is proven against the line published at its place: it is the original
        // published there (where this computer holds the originals), it is that line itself (a
        // continuation of the projection), or the line is the placeholder for a record left out
        // and today's rule leaves this record out too.
        for (index, (line, record)) in lines.iter().zip(later).enumerate() {
            let published = storage::parse_legacy_envelope_line(line)?;
            ensure!(
                originals
                    .as_ref()
                    .is_some_and(|originals| originals[index] == record_key(record))
                    || (published.source == record.source && published.content == record.content)
                    || (published.content == left_out_placeholder()
                        && crate::domain::publication_records::published(
                            &record.source,
                            &record.content,
                        )
                        .is_none()),
                MISALIGNED
            );
        }
        Ok(())
    }

    /// The line published for the record `key`, matched by the record itself; `None` when this
    /// snapshot published no such record. `Err` when its lines can only be matched by position.
    fn by_key(&self, key: &str) -> Result<Option<&String>> {
        match self {
            Self::ByRecord(by_record) => Ok(by_record.get(key)),
            Self::ByPosition {
                lines,
                originals: Some(originals),
            } => Ok(originals
                .iter()
                .position(|original| original == key)
                .and_then(|index| lines.get(index))),
            Self::ByPosition { .. } => anyhow::bail!(UNPROVEN),
        }
    }

    /// The record readers were shown for `record`, the one at `index` in a later LOG, bound to
    /// that record's session; `None` when it was not published encrypted.
    fn earlier(&self, index: usize, record: &Envelope) -> Result<Option<Envelope>> {
        let published = match self {
            Self::ByRecord(by_record) => by_record.get(&record_key(record)),
            Self::ByPosition { lines, .. } => lines.get(index),
        };
        let Some(published) = published else {
            return Ok(None);
        };
        let mut envelope = storage::parse_legacy_envelope_line(published)?;
        envelope.session_id = record.session_id.clone();
        Ok(Some(envelope))
    }
}

/// Each record of a LOG as it is published, as a line, or `None` when it is left out. `kept` is
/// each record under today's rules ([`crate::domain::publication_records::published_sequence`]).
/// A record an encrypted snapshot below already published is published as it was projected then,
/// and only as much of that as today's rules keep: never wider than either. A record today's rules
/// leave out stays out, and so does one whose earlier projection they leave out.
/// `others` are the encrypted snapshots below other parents of a merge, whose records are found
/// by the records themselves.
pub(super) fn splice(
    records: &[Envelope],
    kept: Vec<Option<serde_json::Value>>,
    projected: Option<&Published>,
    others: &[&Published],
) -> Result<Vec<Option<String>>> {
    use crate::domain::transcript::object_hash;
    let line = |source: &str, session: &str, content: serde_json::Value| {
        storage::envelope_line(&Envelope {
            source: source.into(),
            session_id: session.into(),
            object_hash: object_hash(&content),
            content,
        })
    };
    if projected.is_none() && others.is_empty() {
        return Ok(records
            .iter()
            .zip(kept)
            .map(|(record, kept)| {
                kept.map(|content| line(&record.source, &record.session_id, content))
            })
            .collect());
    };
    if let Some(projected) = projected {
        projected.align(records)?;
    }
    // Lines matched by position prove the records up to their own end, each as the line at its
    // place; only the records past it are looked for in the other snapshots.
    let prefix = match projected {
        Some(Published::ByPosition { lines, .. }) => lines.len(),
        _ => 0,
    };
    let mut earlier = Vec::with_capacity(records.len());
    for (index, (record, kept)) in records.iter().zip(&kept).enumerate() {
        if kept.is_none() {
            earlier.push(None);
            continue;
        }
        // Every parent's projection that published this record: if they disagree on it, which one
        // readers saw can't be told, so the push is refused rather than one of them picked.
        let mut found = match projected {
            Some(projected) => projected.earlier(index, record)?,
            None => None,
        };
        let key = record_key(record);
        for other in others.iter().filter(|_| index >= prefix) {
            if let Some(line) = other.by_key(&key)? {
                let mut envelope = storage::parse_legacy_envelope_line(line)?;
                envelope.session_id = record.session_id.clone();
                match &found {
                    Some(seen) => ensure!(
                        seen.source == envelope.source && seen.content == envelope.content,
                        UNPROVEN
                    ),
                    None => found = Some(envelope),
                }
            }
        }
        earlier.push(found);
    }
    // Earlier projections pass through today's rules too, in the LOG's order, since an older
    // client may have published injected elements today's rules strip.
    let narrowed =
        crate::domain::publication_records::published_sequence(records.iter().zip(&earlier).map(
            |(record, earlier)| match earlier {
                Some(earlier) => (earlier.source.as_str(), &earlier.content),
                None => (record.source.as_str(), &record.content),
            },
        ));
    Ok(records
        .iter()
        .zip(kept)
        .zip(earlier)
        .zip(narrowed)
        .map(
            |(((record, kept), earlier), narrowed)| match (kept, earlier) {
                (None, _) => None,
                (Some(_), Some(earlier)) => {
                    narrowed.map(|content| line(&earlier.source, &record.session_id, content))
                }
                (Some(content), None) => Some(line(&record.source, &record.session_id, content)),
            },
        )
        .collect())
}

/// The record an encrypted projection published in place of one it left out.
fn left_out_placeholder() -> serde_json::Value {
    serde_json::json!({"type":"user","isMeta":true,"message":{"role":"user","content":[
        {"type":"text","text":crate::domain::privacy::omission::Omission::NativeRecord.text()}
    ]}})
}

/// The records of the encrypted snapshot `public`, by [`record_key`], when this computer holds
/// them: unlocked earlier, or opened in memory through the access to originals it already has, as
/// resume opens them. Nothing is written. `None` without access.
fn held_originals(repo: &Repo, public: &str) -> Result<Option<Vec<String>>> {
    let keys = |log: &str| {
        record_lines(log)
            .iter()
            .map(|line| storage::parse_legacy_envelope_line(line).map(|e| record_key(&e)))
            .collect::<Result<Vec<_>>>()
    };
    if let Ok(Some(recovered)) =
        crate::domain::privacy_recovery::RecoveredSnapshot::load(repo, public)
    {
        return keys(&recovered.log).map(Some);
    }
    let Some(bytes) = crate::domain::privacy_recovery::committed_envelope(repo, public)? else {
        return Ok(None);
    };
    let envelope = crate::domain::privacy_envelope::PrivacyEnvelope::parse(&bytes)?;
    let (_, layer) = crate::commands::privacy::unlock::open_session_original(
        repo, public, &bytes, &envelope, true,
    )?;
    match layer {
        Some(layer) => {
            let (log, _) = layer.session_bytes()?;
            keys(&log).map(Some)
        }
        None => Ok(None),
    }
}

/// A record by its runtime and content: a fork or a resume carries it under another session.
fn record_key(record: &Envelope) -> String {
    format!(
        "{}\n{}",
        record.source,
        crate::domain::transcript::object_hash(&record.content)
    )
}

fn log_lines(repo: &Repo, commit: &str) -> Result<Vec<String>> {
    let limit = storage::MAX_MATERIALIZED_BYTES;
    let (log, _) = storage::materialize_pair_local(repo.root(), commit, limit, limit)?;
    Ok(record_lines(&log))
}

/// A LOG's records, one complete line each.
pub(super) fn record_lines(text: &str) -> Vec<String> {
    text.split_inclusive('\n')
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut line = line.to_owned();
            if !line.ends_with('\n') {
                line.push('\n');
            }
            line
        })
        .collect()
}

/// The encrypted snapshot a commit's records were last published under, read only when a plain
/// snapshot needs it.
pub(super) struct Nearest {
    source: Repo,
    kind: Kind,
    published: OnceCell<Published>,
}

enum Kind {
    Projected {
        source_oid: String,
        retained: Repo,
        public: String,
    },
    Held(String),
    /// A snapshot after the switch, as the Hub holds it: plain, with no originals.
    Plain(String),
}

impl Nearest {
    /// The local commit of a snapshot this device published encrypted, whose tree it holds.
    pub(super) fn local_source(&self) -> Option<&str> {
        match &self.kind {
            Kind::Projected { source_oid, .. } => Some(source_oid),
            Kind::Held(_) | Kind::Plain(_) => None,
        }
    }

    /// The local commit this snapshot is.
    fn oid(&self) -> &str {
        match &self.kind {
            Kind::Projected { source_oid, .. } => source_oid,
            Kind::Held(oid) | Kind::Plain(oid) => oid,
        }
    }

    pub(super) fn published(&self) -> Result<&Published> {
        if let Some(published) = self.published.get() {
            return Ok(published);
        }
        let published = match &self.kind {
            Kind::Projected {
                source_oid,
                retained,
                public,
            } => Published::projected(&self.source, source_oid, retained, public)?,
            Kind::Held(public) => Published::held(&self.source, public)?,
            Kind::Plain(public) => Published::ByPosition {
                lines: log_lines(&self.source, public)?,
                originals: None,
            },
        };
        Ok(self.published.get_or_init(|| published))
    }
}

/// Finds, for each snapshot a branch's publication adds, the nearest snapshot on its first-parent
/// line that was published encrypted, or the nearest above it the destination holds as it is.
pub(super) struct Finder<'a> {
    source: &'a Repo,
    parent: &'a Path,
    ledger: &'a accepted::Ledger,
    /// The commits the destination holds under their own ids.
    held: &'a BTreeSet<String>,
    branch: &'a str,
    found: BTreeMap<String, Option<Rc<Nearest>>>,
    merges: MergesBelow<'a>,
}

/// Whether a commit is a merge or has one below it, as `rev-list --merges -n 1 <commit>` says.
/// A commit the run read answers by its own parent lines and its parents' answers, kept for the
/// run, so a long line is walked once rather than once for every snapshot on it; any other commit
/// is asked of Git once. Parent lines show every merge `rev-list` would, and a merge Git's commit
/// graph left out as well, which only asks for the check that follows. A repository with a
/// shallow file asks Git for every commit: Git's history stops at the commits it names, and their
/// parent lines don't.
pub(super) struct MergesBelow<'a> {
    /// Each commit the run read, by its own parent lines.
    parents: &'a BTreeMap<String, Vec<String>>,
    shallow: bool,
    known: HashMap<String, bool>,
}

impl<'a> MergesBelow<'a> {
    pub(super) fn new(source: &Repo, parents: &'a BTreeMap<String, Vec<String>>) -> Result<Self> {
        Ok(Self {
            parents,
            shallow: source.git_path_local("shallow")?.exists(),
            known: HashMap::new(),
        })
    }

    pub(super) fn of(&mut self, source: &Repo, oid: &str) -> Result<bool> {
        let by_git = |commit: &str| -> Result<bool> {
            Ok(!source
                .git(&["rev-list", "--merges", "-n", "1", commit])?
                .trim()
                .is_empty())
        };
        if self.shallow {
            return by_git(oid);
        }
        let mut stack = vec![oid.to_owned()];
        while let Some(commit) = stack.last().cloned() {
            if self.known.contains_key(&commit) {
                stack.pop();
                continue;
            }
            let answer = match self.parents.get(&commit) {
                None => Some(by_git(&commit)?),
                Some(parents) if parents.len() > 1 => Some(true),
                Some(parents) => {
                    let pending = parents
                        .iter()
                        .filter(|parent| !self.known.contains_key(*parent))
                        .cloned()
                        .collect::<Vec<_>>();
                    if pending.is_empty() {
                        Some(parents.iter().any(|parent| self.known[parent]))
                    } else {
                        stack.extend(pending);
                        None
                    }
                }
            };
            if let Some(answer) = answer {
                self.known.insert(commit, answer);
                stack.pop();
            }
        }
        Ok(self.known[oid])
    }
}

impl<'a> Finder<'a> {
    pub(super) fn new(
        source: &'a Repo,
        parent: &'a Path,
        ledger: &'a accepted::Ledger,
        held: &'a BTreeSet<String>,
        branch: &'a str,
        parents: &'a BTreeMap<String, Vec<String>>,
    ) -> Result<Self> {
        Ok(Self {
            source,
            parent,
            ledger,
            held,
            branch,
            found: BTreeMap::new(),
            merges: MergesBelow::new(source, parents)?,
        })
    }

    /// The nearest encrypted snapshot below `oid`, a snapshot this publication adds, which is never
    /// encrypted itself. Snapshots are asked parent first, so one whose first parent was asked
    /// reuses its answer.
    pub(super) fn below_new(
        &mut self,
        oid: &str,
        first_parent: Option<&str>,
    ) -> Result<Option<Rc<Nearest>>> {
        let found = match first_parent {
            Some(parent) => match self.found.get(parent) {
                Some(found) => found.clone(),
                None => self.below(parent)?,
            },
            None => None,
        };
        self.found.insert(oid.to_owned(), found.clone());
        Ok(found)
    }

    /// The encrypted snapshots below `oid` other than `first`, the one on its first-parent line,
    /// whose records a merge in its history may carry. Empty when no merge is below `oid`.
    pub(super) fn others(
        &mut self,
        oid: &str,
        first: Option<&Nearest>,
    ) -> Result<Vec<Rc<Nearest>>> {
        if !self.merges.of(self.source, oid)? {
            return Ok(Vec::new());
        }
        let mut candidates = BTreeSet::new();
        let published: BTreeSet<&str> = self
            .ledger
            .every_accepted()
            .filter(|entry| encrypted(entry))
            .map(|entry| entry.source.as_str())
            .collect();
        for source in published {
            if self.reaches(source, oid)? {
                candidates.insert(source.to_owned());
            }
        }
        let marked = envelope_history(self.source, oid, MAX_ENVELOPE_COMMITS)?;
        if !marked.is_empty() {
            candidates.extend(enveloped_commits(self.source, &marked)?);
        }
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let plain = first.filter(|first| matches!(first.kind, Kind::Plain(_)));
        let mut found = Vec::new();
        for commit in &candidates {
            if first.is_some_and(|first| first.oid() == commit) {
                continue;
            }
            let nearest = self
                .own(commit)?
                .context("an encrypted snapshot below a merge is unavailable; nothing was sent")?;
            // Every one is asked, not only the newest of each line: a later snapshot may have left
            // out a record an earlier one published. One this computer holds only as published,
            // without its originals, can't prove what of a merge's records are its originals. It
            // is left out only when every line from `oid` down to it passes through the plain
            // snapshot the destination holds, which vouches for its own records only; anywhere
            // else the push is refused.
            if matches!(
                nearest.published()?,
                Published::ByPosition {
                    originals: None,
                    ..
                }
            ) {
                match plain {
                    Some(plain)
                        if vouched(self.source, commit, plain.oid(), oid, MAX_LINE_COMMITS)? =>
                    {
                        continue;
                    }
                    _ => anyhow::bail!(UNPROVEN),
                }
            }
            found.push(Rc::new(nearest));
        }
        Ok(found)
    }

    /// `oid` itself, when it was published encrypted: accepted from this device as a projection
    /// (by its source or by its public commit), or held as the published snapshot.
    fn own(&self, oid: &str) -> Result<Option<Nearest>> {
        // The commit itself as this device's source, or as the public commit it published: a
        // history that took the Hub's copy holds the latter, and the originals are still here.
        // An identity mapping of the public commit (a later publication that carried it as a
        // parent) says nothing about its originals; the encrypted mapping that published it does.
        let entry = match self.ledger.inherited_immutable(self.branch, oid)? {
            Some(entry) if encrypted(entry) => Some(entry),
            _ => self
                .ledger
                .every_accepted()
                .find(|entry| entry.public == oid && encrypted(entry)),
        };
        if let Some(entry) = entry
            && encrypted(entry)
        {
            return Ok(Some(Nearest {
                source: self.source.clone(),
                kind: Kind::Projected {
                    source_oid: entry.source.clone(),
                    retained: entry.object_source(self.source, self.parent, self.branch)?,
                    public: entry.public.clone(),
                },
                published: OnceCell::new(),
            }));
        }
        if enveloped_commits(self.source, &[oid.to_owned()])?.contains(oid) {
            return Ok(Some(Nearest {
                source: self.source.clone(),
                kind: Kind::Held(oid.to_owned()),
                published: OnceCell::new(),
            }));
        }
        Ok(None)
    }

    /// The nearest encrypted snapshot at or below `oid`, a commit this publication did not walk,
    /// found on its first-parent line in the source history. A snapshot above an envelope that the
    /// destination holds as it is, without one, is a plain one published since: its lines are what
    /// a LOG continuing it starts with.
    fn below(&mut self, oid: &str) -> Result<Option<Rc<Nearest>>> {
        let mapped = self.ledger.accepted(self.branch).any(encrypted);
        let mut marked = self.marked(oid)?;
        if marked.is_none() && !mapped {
            return Ok(None);
        }
        let mut line = FirstParentLine::new(self.source, oid, FIRST_PARENT_PAGE);
        loop {
            let Some(commit) = line.next()? else {
                return Ok(None);
            };
            let commit = commit.as_str();
            if mapped
                && let Some(entry) = self.ledger.inherited_immutable(self.branch, commit)?
                && encrypted(entry)
            {
                return Ok(Some(Rc::new(self.own(commit)?.context(
                    "an encrypted snapshot's accepted mapping is unavailable",
                )?)));
            }
            if marked.is_some()
                && self.held.contains(commit)
                && !enveloped_commits(self.source, &[commit.to_owned()])?.contains(commit)
            {
                return Ok(Some(Rc::new(Nearest {
                    source: self.source.clone(),
                    kind: Kind::Plain(commit.to_owned()),
                    published: OnceCell::new(),
                })));
            }
            if marked.as_deref() == Some(commit) {
                if let Some(nearest) = self.own(commit)? {
                    return Ok(Some(Rc::new(nearest)));
                }
                // This commit is where the envelope was left out; look below it.
                marked = match self.source.git_opt(&[
                    "rev-parse",
                    "--verify",
                    "-q",
                    &format!("{commit}^1"),
                ]) {
                    Some(parent) => self.marked(&parent)?,
                    None => None,
                };
                if marked.is_none() && !mapped {
                    return Ok(None);
                }
            }
        }
    }

    /// Whether `commit` is `oid` or one of its ancestors; see [`reaches`].
    fn reaches(&self, commit: &str, oid: &str) -> Result<bool> {
        reaches(self.source, commit, oid)
    }

    /// The newest commit at or below `oid` on its first-parent line that adds, changes or leaves
    /// out an envelope.
    fn marked(&self, oid: &str) -> Result<Option<String>> {
        let found =
            self.source
                .git(&["rev-list", "--first-parent", "-n", "1", oid, "--", ENVELOPE])?;
        Ok(Some(found.trim().to_owned()).filter(|found| !found.is_empty()))
    }
}

/// A file the file line held, as it goes back: its mode and blob.
pub(super) type Restored = (String, String);

/// The files of `oid`'s tree a session changed while it was published encrypted, which no upload
/// ever carried: each as it was where that session left its file line, or `None` where the file
/// line had no such path. `encrypted` are the local commits of the encrypted snapshots below
/// `oid` (its first-parent line and its merges). A path whose blob in `oid` differs from the
/// encrypted snapshot's changed after it and is new; the session's own files are its records.
/// `bases` keeps each encrypted commit's file-line base for the publication, so a session pushing
/// many turns walks its line once.
pub(super) fn held_back_files(
    source: &Repo,
    oid: &str,
    encrypted: &[String],
    bases: &mut HashMap<String, Option<String>>,
) -> Result<Vec<(String, Option<Restored>)>> {
    let mut held = BTreeMap::<String, Option<(String, String)>>::new();
    for commit in encrypted {
        let base = match bases.get(commit) {
            Some(base) => base.clone(),
            None => {
                let base = file_line_base(source, commit)?;
                bases.insert(commit.clone(), base.clone());
                base
            }
        };
        // What the encrypted session changed: each path with its mode and blob at the base, if
        // any, and its blob in the encrypted snapshot, if any.
        let mut changed = Vec::<(String, Option<(String, String)>, Option<String>)>::new();
        match &base {
            Some(base) => {
                let raw = source.git_bytes_result(&[
                    "diff-tree",
                    "-r",
                    "-z",
                    "--no-renames",
                    base,
                    commit,
                ])?;
                let mut fields = raw
                    .split(|byte| *byte == 0)
                    .filter(|field| !field.is_empty());
                while let (Some(header), Some(path)) = (fields.next(), fields.next()) {
                    let header = std::str::from_utf8(header)?;
                    let parts: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
                    ensure!(parts.len() >= 4, "unreadable tree change");
                    let blob =
                        |oid: &str| (!oid.bytes().all(|byte| byte == b'0')).then(|| oid.to_owned());
                    changed.push((
                        String::from_utf8(path.to_vec())?,
                        blob(parts[2]).map(|blob| (parts[0].to_owned(), blob)),
                        blob(parts[3]),
                    ));
                }
            }
            None => {
                let raw = source.git_bytes_result(&["ls-tree", "-r", "-z", commit])?;
                for entry in raw
                    .split(|byte| *byte == 0)
                    .filter(|entry| !entry.is_empty())
                {
                    let entry = std::str::from_utf8(entry)?;
                    let (header, path) = entry.split_once('\t').context("unreadable tree entry")?;
                    let blob = header.split(' ').nth(2).context("unreadable tree entry")?;
                    changed.push((path.to_owned(), None, Some(blob.to_owned())));
                }
            }
        }
        let changed: Vec<_> = changed
            .into_iter()
            .filter(|(path, _, _)| !session_owned(path) && !held.contains_key(path))
            .collect();
        if changed.is_empty() {
            continue;
        }
        let now = blobs_at(
            source,
            oid,
            changed.iter().map(|(path, _, _)| path.as_str()),
            LS_TREE_BATCH,
        )?;
        for (path, before, during) in changed {
            if now.get(&path) != during.as_ref() {
                continue;
            }
            held.insert(path, before);
        }
    }
    Ok(held.into_iter().collect())
}

/// A session's own files, which its records decide.
fn session_owned(path: &str) -> bool {
    [meta::LOG_FILE, meta::VIEW_FILE, ".gitattributes"].contains(&path)
        || ["events/", "session/", "privacy/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

/// Where the session of `commit` left its file line: the nearest commit on its first-parent line
/// that is a file-line snapshot. `None` when it never had one. A snapshot whose metadata can't be
/// read refuses.
fn file_line_base(source: &Repo, commit: &str) -> Result<Option<String>> {
    let mut line = FirstParentLine::new(source, commit, FIRST_PARENT_PAGE);
    while let Some(candidate) = line.next()? {
        let metadata = storage::metadata_local(source.root(), &candidate).with_context(|| {
            format!("{candidate} has no readable metadata, so what it shared can't be told; nothing was sent")
        })?;
        if metadata.is_file_line() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// The blob at each of `paths` in `oid` that has one, listed `batch` paths at a time so no command
/// line outgrows what the platform allows.
fn blobs_at<'p>(
    source: &Repo,
    oid: &str,
    paths: impl Iterator<Item = &'p str>,
    batch: usize,
) -> Result<BTreeMap<String, String>> {
    let paths: Vec<&str> = paths.collect();
    let mut blobs = BTreeMap::new();
    for chunk in paths.chunks(batch) {
        let mut args = vec!["ls-tree", "-z", oid, "--"];
        args.extend(chunk);
        let raw = source.git_bytes_result(&args)?;
        for entry in raw
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let entry = std::str::from_utf8(entry)?;
            let (header, path) = entry.split_once('\t').context("unreadable tree entry")?;
            let blob = header.split(' ').nth(2).context("unreadable tree entry")?;
            blobs.insert(path.to_owned(), blob.to_owned());
        }
    }
    Ok(blobs)
}

/// Whether `commit` is `oid` or one of its ancestors, asked of Git by exit code: a commit this
/// checkout doesn't hold can't be either, and an answer Git can't give refuses the push.
fn reaches(source: &Repo, commit: &str, oid: &str) -> Result<bool> {
    if commit == oid {
        return Ok(true);
    }
    if source
        .git_status_local(&["cat-file", "-e", &format!("{commit}^{{commit}}")])?
        .0
        != Some(0)
    {
        return Ok(false);
    }
    // Git may answer "no" while reporting that it couldn't read part of the history: only a clean
    // answer counts.
    match source.git_status_local(&["merge-base", "--is-ancestor", commit, oid])? {
        (Some(0), _, stderr) if stderr.is_empty() => Ok(true),
        (Some(1), _, stderr) if stderr.is_empty() => Ok(false),
        _ => anyhow::bail!(UNPROVEN),
    }
}

/// Whether the plain snapshot `plain` vouches for the encrypted snapshot `commit` below `oid`:
/// `commit` is below `plain`, `plain` is `oid` or below it, and every line from `oid` down to
/// `commit` passes through `plain`.
fn vouched(source: &Repo, commit: &str, plain: &str, oid: &str, cap: usize) -> Result<bool> {
    Ok(reaches(source, commit, plain)?
        && reaches(source, plain, oid)?
        && only_through(source, commit, plain, oid, cap)?)
}

/// Whether every line of history from `oid` down to `commit`, which is below `plain`, passes
/// through `plain`. A line leaves the commits above `plain` by one parent edge, to `plain` or to a
/// commit below it; it passes by `plain` exactly when that commit is another one that is `commit`
/// or reaches it. A listing past `cap`, or an answer Git can't give cleanly, counts as a line that
/// passes by.
fn only_through(source: &Repo, commit: &str, plain: &str, oid: &str, cap: usize) -> Result<bool> {
    let limit = format!("--max-count={}", cap + 1);
    let not_below_plain = format!("^{plain}");
    let (Some(0), stdout, stderr) =
        source.git_status_local(&["rev-list", &limit, "--parents", oid, &not_below_plain])?
    else {
        return Ok(false);
    };
    let lines: Vec<Vec<&str>> = stdout
        .lines()
        .map(|line| line.split(' ').collect())
        .collect();
    if !stderr.is_empty() || lines.len() > cap {
        return Ok(false);
    }
    // The commits above `plain`; every parent outside them is `plain` or below it.
    let above: BTreeSet<&str> = lines
        .iter()
        .filter_map(|line| line.first().copied())
        .collect();
    let below: BTreeSet<&str> = lines
        .iter()
        .flat_map(|line| line.iter().skip(1).copied())
        .filter(|parent| !above.contains(parent) && *parent != plain)
        .collect();
    for parent in below {
        if reaches(source, commit, parent)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Every commit in the history of `oid` that adds, changes or leaves out an envelope, through any
/// parent; refused past `cap`, so the list is never longer than one push can check.
fn envelope_history(source: &Repo, oid: &str, cap: usize) -> Result<Vec<String>> {
    let limit = format!("--max-count={}", cap + 1);
    let marked: Vec<String> = source
        // Full history: by default a path query skips a parent the merge's tree doesn't take.
        .git(&["rev-list", &limit, "--full-history", oid, "--", ENVELOPE])?
        .lines()
        .map(str::to_owned)
        .collect();
    ensure!(
        marked.len() <= cap,
        "this conversation's history holds more encrypted snapshots than one push can check; nothing was sent"
    );
    Ok(marked)
}

/// The first-parent line of a commit, newest first, listed `page` commits at a time.
struct FirstParentLine<'a> {
    source: &'a Repo,
    oid: &'a str,
    page: usize,
    skipped: usize,
    listed: Vec<String>,
    ended: bool,
}

impl<'a> FirstParentLine<'a> {
    fn new(source: &'a Repo, oid: &'a str, page: usize) -> Self {
        Self {
            source,
            oid,
            page,
            skipped: 0,
            listed: Vec::new(),
            ended: false,
        }
    }

    fn next(&mut self) -> Result<Option<String>> {
        if self.listed.is_empty() && !self.ended {
            self.listed = self
                .source
                .git(&[
                    "rev-list",
                    "--first-parent",
                    &format!("--max-count={}", self.page),
                    &format!("--skip={}", self.skipped),
                    self.oid,
                ])?
                .lines()
                .rev()
                .map(str::to_owned)
                .collect();
            self.skipped += self.listed.len();
            self.ended = self.listed.len() < self.page;
        }
        Ok(self.listed.pop())
    }
}

/// An accepted mapping this device published encrypted: a projection of its source.
pub(super) fn encrypted(entry: &accepted::Mapping) -> bool {
    !entry.plain && !entry.delivered && entry.source != entry.public
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(source: &str, content: serde_json::Value) -> Envelope {
        Envelope {
            source: source.into(),
            session_id: format!("agit-{}", "a".repeat(40)),
            object_hash: crate::domain::transcript::object_hash(&content),
            content,
        }
    }

    /// A record an encrypted snapshot published keeps its earlier projection, but only as much of
    /// it as today's rules keep: an injected reminder an older client published is stripped, and
    /// the path it aliased stays aliased. A record today's rules leave out stays out, whatever was
    /// projected for it.
    #[test]
    fn an_earlier_projection_is_published_no_wider_than_todays_rules() {
        const CANARY: &str = "reminder-canary-k3";
        const RAW: &str = "/Users/alice/secret-project";
        let reminder = format!("<system-reminder>\n{CANARY}\n</system-reminder>");
        let user_turn = |path: &str| {
            json!({"type":"user","message":{"role":"user","content":[
                {"type":"text","text":reminder},
                {"type":"text","text":format!("read {path}/plan.txt")},
            ]}})
        };
        let original = envelope("claude-code", user_turn(RAW));
        let meta = envelope(
            "claude-code",
            json!({"type":"user","isMeta":true,"message":{"role":"user","content":"context"}}),
        );
        // An older client projected the turn with the path aliased and the reminder left in.
        let projected = envelope("claude-code", user_turn("<workspace>"));
        let published = Published::ByRecord(HashMap::from([
            (record_key(&original), storage::envelope_line(&projected)),
            (record_key(&meta), storage::envelope_line(&original)),
        ]));
        let records = [original, meta];
        let kept = crate::domain::publication_records::published_sequence(
            records.iter().map(|r| (r.source.as_str(), &r.content)),
        );
        let lines = splice(&records, kept, Some(&published), &[]).unwrap();
        let turn = lines[0].as_deref().expect("the turn is published");
        assert!(turn.contains("<workspace>/plan.txt"), "{turn}");
        assert!(!turn.contains(CANARY) && !turn.contains(RAW), "{turn}");
        assert_eq!(
            lines[1], None,
            "today's rules leave the runtime's own record out"
        );

        // A merge brings in records another parent's snapshot published: they are found by the
        // records themselves and held to that projection too. One whose lines can only be matched
        // by position can't say which records it published, so nothing is published.
        let kept = crate::domain::publication_records::published_sequence(
            records.iter().map(|r| (r.source.as_str(), &r.content)),
        );
        let merged = splice(&records, kept.clone(), None, &[&published]).unwrap();
        assert_eq!(merged, lines);
        let unproven = Published::ByPosition {
            lines: vec![storage::envelope_line(&projected)],
            originals: None,
        };
        assert!(splice(&records, kept.clone(), None, &[&unproven]).is_err());
        // Two parents that published the same record differently are refused, whichever comes
        // first; two that agree publish it once as they showed it.
        let other_view = Published::ByRecord(HashMap::from([(
            record_key(&records[0]),
            storage::envelope_line(&envelope("claude-code", user_turn("<home>"))),
        )]));
        assert!(splice(&records, kept.clone(), Some(&published), &[&other_view]).is_err());
        assert!(splice(&records, kept.clone(), None, &[&other_view, &published]).is_err());
        assert_eq!(
            splice(&records, kept, Some(&published), &[&published]).unwrap(),
            lines
        );
    }

    /// Lines matched by position are used only when the later LOG starts with the records they
    /// projected: by the records themselves where this device holds them, otherwise by each
    /// record's runtime, type and role. A LOG with a record removed before them is refused.
    #[test]
    fn lines_matched_by_position_need_the_later_log_to_start_with_their_records() {
        let user = |text: &str| {
            envelope(
                "claude-code",
                json!({"type":"user","message":{"role":"user","content":text}}),
            )
        };
        let reply = |text: &str| {
            envelope(
                "claude-code",
                json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":text}]}}),
            )
        };
        let held = [user("first"), reply("noted"), user("second")];
        let lines: Vec<String> = held.iter().map(storage::envelope_line).collect();
        let by_shape = Published::ByPosition {
            lines: lines.clone(),
            originals: None,
        };
        let mut continued = held.to_vec();
        continued.push(user("third"));
        by_shape.align(&continued).unwrap();
        let removed = [reply("noted"), user("second"), user("third")];
        assert!(by_shape.align(&removed).is_err());
        assert!(
            by_shape.align(&held[..2]).is_err(),
            "shorter than what was published"
        );
        let mut codex = continued.clone();
        codex[0].source = "codex".into();
        assert!(by_shape.align(&codex).is_err(), "another runtime");

        let by_record = Published::ByPosition {
            lines,
            originals: Some(held.iter().map(record_key).collect()),
        };
        by_record.align(&continued).unwrap();
        let mut edited = continued.clone();
        edited[1] = reply("changed");
        assert!(
            by_record.align(&edited).is_err(),
            "a record other than the one published"
        );
        assert!(splice(&removed, vec![None, None, None], Some(&by_shape), &[]).is_err());
    }
}

#[cfg(test)]
mod continuation_tests {
    use super::*;
    use serde_json::json;

    fn envelope(content: serde_json::Value) -> Envelope {
        Envelope {
            source: "claude-code".into(),
            session_id: format!("agit-{}", "b".repeat(40)),
            object_hash: crate::domain::transcript::object_hash(&content),
            content,
        }
    }
    fn user(text: &str) -> Envelope {
        envelope(json!({"type":"user","message":{"role":"user","content":text}}))
    }
    fn reply(text: &str) -> Envelope {
        envelope(
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":text}]}}),
        )
    }
    fn kept(records: &[Envelope]) -> Vec<Option<serde_json::Value>> {
        crate::domain::publication_records::published_sequence(
            records.iter().map(|r| (r.source.as_str(), &r.content)),
        )
    }

    /// A continuation proven by its originals publishes, at every place the snapshot published,
    /// the line published then, byte for byte: never the original projected again. Only the
    /// records after those places go through today's rule.
    #[test]
    fn proven_places_publish_the_line_published_then() {
        let originals = [user("read /Users/alice/secret/a.txt"), reply("noted")];
        let projected = [user("read <workspace>/a.txt"), reply("noted")];
        let published = Published::ByPosition {
            lines: projected.iter().map(storage::envelope_line).collect(),
            originals: Some(originals.iter().map(record_key).collect()),
        };
        let mut later = originals.to_vec();
        later.push(user("then /Users/alice/secret/b.txt"));
        let lines = splice(&later, kept(&later), Some(&published), &[]).unwrap();
        for (line, old) in lines.iter().zip(&projected) {
            assert_eq!(line.as_deref(), Some(storage::envelope_line(old).as_str()));
        }
        assert_eq!(
            lines[2].as_deref(),
            Some(storage::envelope_line(&later[2]).as_str()),
            "a record after the published ones goes through today's rule"
        );
    }

    /// The line published then still passes through today's rule, so a proven place is never wider
    /// than either: what an older client published and today's rule strips goes, and the rest of
    /// the line stays as it was published.
    #[test]
    fn a_proven_place_publishes_the_old_line_minus_what_today_strips() {
        let reminder = "<system-reminder>\nreminder-canary\n</system-reminder>";
        let turn = |path: &str, injected: bool| {
            let mut blocks = Vec::new();
            if injected {
                blocks.push(json!({"type":"text","text":reminder}));
            }
            blocks.push(json!({"type":"text","text":format!("read {path}/a.txt")}));
            envelope(json!({"type":"user","message":{"role":"user","content":blocks}}))
        };
        let original = turn("/Users/alice/secret", true);
        let published = Published::ByPosition {
            lines: vec![storage::envelope_line(&turn("<workspace>", true))],
            originals: Some(vec![record_key(&original)]),
        };
        let later = [original];
        let lines = splice(&later, kept(&later), Some(&published), &[]).unwrap();
        assert_eq!(
            lines[0].as_deref(),
            Some(storage::envelope_line(&turn("<workspace>", false)).as_str())
        );
    }

    /// A continuation of the projection itself is proven on a computer that holds the originals
    /// too: holding them takes no proof away.
    #[test]
    fn a_continuation_of_the_projection_is_proven_with_the_originals_held() {
        let originals = [user("read /Users/alice/secret/a.txt"), reply("noted")];
        let projected = [user("read <workspace>/a.txt"), reply("noted")];
        let published = Published::ByPosition {
            lines: projected.iter().map(storage::envelope_line).collect(),
            originals: Some(originals.iter().map(record_key).collect()),
        };
        let mut later = projected.to_vec();
        later.push(user("next"));
        let lines = splice(&later, kept(&later), Some(&published), &[]).unwrap();
        for (line, old) in lines.iter().zip(&projected) {
            assert_eq!(line.as_deref(), Some(storage::envelope_line(old).as_str()));
        }
        let mut shifted = projected[1..].to_vec();
        shifted.push(user("next"));
        assert!(splice(&shifted, kept(&shifted), Some(&published), &[]).is_err());
    }

    /// Without the originals, a place the snapshot filled with a placeholder is proven by a record
    /// today's rule leaves out too, and nothing is published for it. A record today's rule keeps
    /// can't stand where only a placeholder was published.
    #[test]
    fn a_placeholder_is_proven_only_by_a_record_left_out_today() {
        let placeholder = envelope(left_out_placeholder());
        let published = Published::ByPosition {
            lines: [placeholder, user("hi")]
                .iter()
                .map(storage::envelope_line)
                .collect(),
            originals: None,
        };
        let attachment =
            envelope(json!({"type":"attachment","attachment":{"type":"skill_listing"}}));
        let later = [attachment, user("hi")];
        let lines = splice(&later, kept(&later), Some(&published), &[]).unwrap();
        assert_eq!(lines[0], None);
        assert!(lines[1].is_some());
        let shifted = [user("kept"), user("hi")];
        assert!(splice(&shifted, kept(&shifted), Some(&published), &[]).is_err());
    }
}

#[cfg(test)]
mod bound_tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// Paths are listed a batch at a time, and every path the tree holds comes back whatever the
    /// batch size; one it doesn't hold has no blob.
    #[test]
    fn tree_paths_are_listed_in_bounded_batches() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        let names: Vec<String> = (0..5).map(|index| format!("memory/{index}.md")).collect();
        std::fs::create_dir(temp.path().join("memory")).unwrap();
        for (index, name) in names.iter().enumerate() {
            std::fs::write(temp.path().join(name), format!("note {index}")).unwrap();
        }
        git(temp.path(), &["add", "-A"]);
        git(temp.path(), &["commit", "-q", "-m", "files"]);
        let tip = git(temp.path(), &["rev-parse", "HEAD"]);
        let mut asked: Vec<&str> = names.iter().map(String::as_str).collect();
        asked.insert(2, "skills/absent.md");
        let source = Repo::at(temp.path()).local_objects_only();
        for batch in [1, 2, 6, 64] {
            let blobs = blobs_at(&source, &tip, asked.iter().copied(), batch).unwrap();
            assert_eq!(
                blobs.keys().cloned().collect::<Vec<_>>(),
                names,
                "batch {batch}"
            );
            for name in &names {
                assert_eq!(
                    blobs[name],
                    git(temp.path(), &["rev-parse", &format!("{tip}:{name}")]),
                    "batch {batch}"
                );
            }
        }
    }

    /// A snapshot is vouched for by a plain one only when every line down to it passes through
    /// that plain one: a line beside it, which can carry the snapshot's own records past the plain
    /// one's, means no, and so does a listing past its cap.
    #[test]
    fn a_line_beside_the_plain_snapshot_is_not_vouched_for() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q"]);
        let commit = |message: &str| {
            git(root, &["commit", "-q", "--allow-empty", "-m", message]);
            git(root, &["rev-parse", "HEAD"])
        };
        let encrypted = commit("encrypted");
        let plain = commit("plain over it");
        let above = commit("a turn after the plain one");
        // A line from the encrypted snapshot that doesn't pass through the plain one.
        git(root, &["checkout", "-q", "-b", "beside", &encrypted]);
        let beside = commit("continued from the encrypted snapshot");
        git(root, &["checkout", "-q", "-"]);
        git(root, &["merge", "-q", "--no-ff", "-m", "merge", &beside]);
        let merged = git(root, &["rev-parse", "HEAD"]);
        let source = Repo::at(root).local_objects_only();
        assert!(vouched(&source, &encrypted, &plain, &above, 64).unwrap());
        assert!(!vouched(&source, &encrypted, &plain, &merged, 64).unwrap());
        // A listing that outgrows its cap can't vouch either, nor a plain one not above it.
        assert!(!vouched(&source, &encrypted, &plain, &above, 0).unwrap());
        assert!(!vouched(&source, &beside, &plain, &merged, 64).unwrap());
    }

    /// Each parent edge out of the commits above the plain snapshot is judged: a merge whose other
    /// parent is the encrypted snapshot itself, a commit beside the plain one above it, or one below
    /// the plain one that reaches it isn't vouched for, while one whose other parent is below the encrypted snapshot is, as is a
    /// straight line. Nor is one whose plain snapshot is above it, a listing past its cap, or one
    /// Git can't list.
    #[test]
    fn a_parent_edge_that_passes_by_the_plain_snapshot_is_not_vouched_for() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q"]);
        let commit = |message: &str| {
            git(root, &["commit", "-q", "--allow-empty", "-m", message]);
            git(root, &["rev-parse", "HEAD"])
        };
        let merge = |parents: &[&str]| {
            let mut args = vec!["commit-tree", "-m", "merge"];
            for parent in parents {
                args.extend(["-p", parent]);
            }
            let tree = format!("{}^{{tree}}", parents[0]);
            args.insert(1, &tree);
            git(root, &args)
        };
        let older = commit("below the encrypted snapshot");
        let encrypted = commit("encrypted");
        let between = commit("a turn between the two");
        let plain = commit("plain over it");
        let line = commit("a turn after the plain one");
        git(root, &["checkout", "-q", "-b", "beside", &encrypted]);
        let beside = commit("continued from the encrypted snapshot");
        let source = Repo::at(root).local_objects_only();
        let vouches = |oid: &str| vouched(&source, &encrypted, &plain, oid, 64).unwrap();

        assert!(
            !vouches(&merge(&[&plain, &encrypted])),
            "the encrypted snapshot as a parent"
        );
        assert!(
            !vouches(&merge(&[&plain, &beside])),
            "a commit beside the plain one"
        );
        assert!(
            !vouches(&merge(&[&plain, &between])),
            "a parent below the plain snapshot that reaches the encrypted one"
        );
        assert!(
            vouches(&merge(&[&plain, &older])),
            "a parent below the encrypted snapshot"
        );
        assert!(vouches(&line), "a straight line");
        assert!(!vouched(&source, &older, &plain, &encrypted, 64).unwrap());
        assert!(!vouched(&source, &encrypted, &plain, &line, 0).unwrap());
        assert!(
            !only_through(
                &source,
                &encrypted,
                &plain,
                "0123456789abcdef0123456789abcdef01234567",
                64
            )
            .unwrap()
        );
    }

    /// The first-parent line is read page by page, and every commit comes once, newest first,
    /// whatever the page size.
    #[test]
    fn a_first_parent_line_is_read_in_bounded_pages() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        for index in 0..10 {
            git(
                temp.path(),
                &[
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    &format!("turn {index}"),
                ],
            );
        }
        let tip = git(temp.path(), &["rev-parse", "HEAD"]);
        let all: Vec<String> = git(temp.path(), &["rev-list", "--first-parent", &tip])
            .lines()
            .map(str::to_owned)
            .collect();
        let source = Repo::at(temp.path()).local_objects_only();
        for page in [1, 3, 10, 11] {
            let mut line = FirstParentLine::new(&source, &tip, page);
            let mut read = Vec::new();
            while let Some(commit) = line.next().unwrap() {
                read.push(commit);
                assert!(read.len() <= all.len(), "page {page}: {read:?}");
            }
            assert_eq!(read, all, "page {page}");
        }
    }

    /// An ancestry probe Git can't answer refuses; a commit this checkout doesn't hold is no
    /// ancestor.
    #[test]
    fn an_ancestry_probe_git_cannot_answer_refuses() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(
            temp.path(),
            &["commit", "-q", "--allow-empty", "-m", "first"],
        );
        let first = git(temp.path(), &["rev-parse", "HEAD"]);
        git(
            temp.path(),
            &["commit", "-q", "--allow-empty", "-m", "second"],
        );
        let tip = git(temp.path(), &["rev-parse", "HEAD"]);
        git(temp.path(), &["checkout", "-q", "--orphan", "other"]);
        git(
            temp.path(),
            &["commit", "-q", "--allow-empty", "-m", "other"],
        );
        let other = git(temp.path(), &["rev-parse", "HEAD"]);
        let source = Repo::at(temp.path()).local_objects_only();
        assert!(reaches(&source, &first, &tip).unwrap());
        assert!(!reaches(&source, &other, &tip).unwrap());
        assert!(!reaches(&source, &"0".repeat(40), &tip).unwrap());
        std::fs::remove_file(
            temp.path()
                .join(".git/objects")
                .join(&first[..2])
                .join(&first[2..]),
        )
        .unwrap();
        assert!(reaches(&source, &other, &tip).is_err());
    }

    /// A history with more envelope commits than the cap is refused rather than read in part.
    #[test]
    fn more_envelope_commits_than_the_cap_refuse() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        std::fs::create_dir_all(temp.path().join("privacy")).unwrap();
        for index in 0..3 {
            std::fs::write(
                temp.path().join(ENVELOPE),
                format!("{{\"envelope\":{index}}}"),
            )
            .unwrap();
            git(temp.path(), &["add", "-A"]);
            git(
                temp.path(),
                &["commit", "-q", "-m", &format!("snapshot {index}")],
            );
        }
        let tip = git(temp.path(), &["rev-parse", "HEAD"]);
        let source = Repo::at(temp.path()).local_objects_only();
        assert_eq!(envelope_history(&source, &tip, 3).unwrap().len(), 3);
        let refused = envelope_history(&source, &tip, 2).unwrap_err().to_string();
        assert!(refused.contains("nothing was sent"), "{refused}");
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// A merge below a snapshot counts whether the run read the commits down to it or not: a
    /// commit the run didn't read is asked of Git. In a repository with a shallow file Git's own
    /// answer stands for every commit, since its history stops where that file says though the
    /// parent lines go on. Taking a commit the run didn't read as having no merge below, or
    /// following parent lines past a shallow boundary, answers otherwise here than `rev-list` does.
    #[test]
    fn a_merge_below_counts_whether_the_run_read_down_to_it_or_not() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q"]);
        let tree = git(root, &["mktree"]);
        let commit = |parents: &[&str], message: &str| {
            let mut args = vec!["commit-tree", tree.as_str(), "-m", message];
            for parent in parents {
                args.extend(["-p", parent]);
            }
            git(root, &args)
        };
        let base = commit(&[], "base");
        let left = commit(&[&base], "left");
        let right = commit(&[&base], "right");
        let merge = commit(&[&left, &right], "merge");
        let after = commit(&[&merge], "after");
        let tip = commit(&[&after], "tip");
        let source = Repo::at(root).local_objects_only();

        // The run read the tip alone: the merge below it is asked of Git.
        let read = super::super::commit_parent_map(&source, &[tip.clone()]).unwrap();
        assert!(
            MergesBelow::new(&source, &read)
                .unwrap()
                .of(&source, &tip)
                .unwrap()
        );

        // The run read the whole line: parent lines answer, for the tip and for a line with none.
        let line = [&base, &left, &right, &merge, &after, &tip].map(|oid| oid.to_string());
        let read = super::super::commit_parent_map(&source, &line).unwrap();
        let mut merges = MergesBelow::new(&source, &read).unwrap();
        assert!(merges.of(&source, &tip).unwrap());
        assert!(!merges.of(&source, &left).unwrap());

        // Git's history stops at `after`, so it finds no merge below the tip.
        std::fs::write(root.join(".git/shallow"), format!("{after}\n")).unwrap();
        assert!(
            !MergesBelow::new(&source, &read)
                .unwrap()
                .of(&source, &tip)
                .unwrap()
        );
    }
}
