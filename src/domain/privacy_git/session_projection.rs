//! Plain session snapshots projected one record at a time, continuing along a chain of commits.
//!
//! [`conversation_commit`] reads a snapshot's whole transcript, several times over, for every
//! commit a publication introduces. A projector instead keeps, for the commit it projected last,
//! the record rule's state and one compact entry per event. A commit whose only parent is that
//! commit, and whose LOG extends that commit's LOG, is projected from the appended records alone;
//! any other commit starts again from an empty state and reads its whole LOG, still one record at
//! a time. Either way its result is the one [`conversation_commit`] gives, which debug builds
//! check on every commit.
//!
//! A projector lives for one publication run, in memory. It takes no settings and keeps nothing
//! across runs, so a second publication starts from nothing; if it ever kept state across runs or
//! took a setting, the projection's privacy review would need to cover that state.
//!
//! While it continues, two things a snapshot needs are taken from the commit before it rather than
//! read again in full, each only after it is shown to be what a full read would give:
//! - which records its public parent already published, when that parent is the commit this
//!   projector wrote for the source parent and its LOG is the blob this projector produced;
//! - its public tree, built on the public tree written for its parent with only what the child
//!   changed. Every chain of trees built that way ends with its last tree built the full way and
//!   compared, and a difference refuses the run.

use super::*;
use crate::commands::plumbing::{BlobWriter, Content};
use crate::domain::publication_records::PublishedSequence;
use crate::domain::storage;
use crate::domain::transcript::Envelope;
use std::collections::{BTreeSet, HashMap, HashSet};

/// One record as it is published: its public event's path, the blob holding its public line when
/// that line differs from the recorded one, and whether the line holds a placeholder the privacy
/// check put where it left something out.
#[derive(Clone)]
struct Public {
    path: String,
    blob: Option<gix::ObjectId>,
    left_out: bool,
}

impl Public {
    fn id(&self) -> &str {
        &self.path[self.path.len() - meta::EVENT_ID_HEX_LEN..]
    }
}

/// The first occurrence of a recorded event in the LOG: its stored size, and how it is published.
struct Entry {
    size: usize,
    public: Option<Public>,
}

/// The plain session snapshots one publication run projects.
#[derive(Default)]
pub(super) struct Projector {
    /// The commit the state below covers.
    at: Option<String>,
    /// That commit's LOG.
    log: Vec<String>,
    /// The bytes its LOG and its distinct events take, as materializing it would count them.
    log_bytes: usize,
    unique_bytes: usize,
    /// The record rule after every LOG occurrence, duplicates included.
    sequence: PublishedSequence,
    /// This device's delivery records spent on the tool-step contexts read so far, as
    /// [`conversation_commit`] spends them over the whole LOG: each vouches once along the chain,
    /// and a fresh start spends them anew.
    deliveries: crate::rc::step_delivery::Conversations,
    /// Each recorded event's first occurrence.
    entries: HashMap<String, Entry>,
    /// Each LOG occurrence as it is published, in LOG order.
    occurrences: Vec<Option<Public>>,
    /// How many occurrences are published other than as recorded.
    rewritten: usize,
    /// The event objects the commit's tree holds, named by its LOG or not.
    held: BTreeSet<String>,
    /// The public ids of the commit's LOG: what a child continuing it finds its public parent
    /// published, when that public parent is [`Self::written`].
    public_ids: HashSet<String>,
    /// The event paths the commit publishes.
    published_paths: HashSet<String>,
    /// What this projector produced for the commit.
    written: Option<Written>,
    /// The commit's public tree, when it was built on its parent's rather than the full way.
    unchecked: Option<Unchecked>,
}

/// The commit holding the LOG a projector produced for a snapshot.
struct Written {
    /// The conversation commit written for it, or the snapshot itself when it publishes every
    /// record as recorded.
    commit: String,
    /// Whether `commit` is a conversation commit this projector wrote.
    conversation: bool,
    /// The blob id of that LOG's bytes.
    log: gix::ObjectId,
}

/// A public tree built on its parent's, with the LOG and VIEW it was given, so that it can be
/// built the full way and compared.
struct Unchecked {
    tree: String,
    log: Vec<u8>,
    view: Vec<u8>,
}

/// Changes to a tree by path: the entry each path takes, or none to take it away.
type Edits = Vec<(String, Option<Content>)>;

/// An event object a child's tree adds to its parent's, with its mode and object.
struct Added {
    path: String,
    kind: gix::object::tree::EntryKind,
    id: gix::ObjectId,
}

/// A record kept for publication whose public line waits for today's protection.
struct Waiting {
    /// Its place in [`Projector::occurrences`].
    occurrence: usize,
    /// Its recorded event id, when this is that event's first occurrence.
    first: Option<String>,
    source: String,
    session_id: String,
    line: String,
}

impl Projector {
    /// `oid` as plain publication carries it: the same [`Conversation`] that
    /// [`conversation_commit`] returns for `oid`, its `parents` and what their public commits
    /// already published, which `published` reads. `written_parent` is the commit this run wrote
    /// the LOG of the snapshot's one public parent in, when the run wrote it and the parent shows
    /// itself plain.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn project<'a>(
        &mut self,
        objects: &Objects,
        source: &Repo,
        oid: &str,
        parents: &[String],
        written_parent: Option<&str>,
        published: &dyn Fn() -> Result<Published<'a>>,
        rechecked: &mut Rechecked,
    ) -> Result<Conversation> {
        let Some((log, view)) = storage::local_v1_sequences(source.root(), oid)? else {
            // A v0 snapshot is rebuilt in the v1 layout as a whole.
            self.finish(source)?;
            *self = Self::default();
            return conversation_commit(
                source,
                oid,
                parents,
                &published()?,
                Carried::default(),
                rechecked,
            );
        };
        let previous = self.at.take();
        // What the state holds was read from the parent's event objects, so it carries over only
        // while the child's tree holds each of them unchanged: an event object the child removed
        // or replaced is read again, as a fresh start reads every event, and refused there if it
        // is missing or malformed. Otherwise a child that kept its parent's LOG while dropping an
        // event would be projected from the parent's copy of it.
        let parent = previous
            .as_deref()
            .filter(|parent| matches!(parents, [only] if only == parent))
            .filter(|_| log.starts_with(&self.log));
        let added = match parent {
            Some(parent) => held_after(
                source,
                &self.held,
                parent,
                oid,
                crate::domain::storage::MAX_SEQUENCE_EVENTS,
            )?,
            None => None,
        };
        let continues = added.is_some();
        // A child continuing a parent whose records were not all published as recorded, and
        // whose public tree this projector wrote, has its public tree built on the parent's.
        let outside = match (parent, &self.written) {
            (Some(parent), Some(written))
                if continues && written.conversation && self.rewritten > 0 =>
            {
                changes_outside_events(source, parent, oid)?
            }
            _ => None,
        };
        // The chain of trees built on their parents' ends here unless this one is too.
        if outside.is_none() {
            self.check(source, previous.as_deref())?;
        }
        if !continues {
            *self = Self::default();
        }
        let start = self.log.len();
        // The records the public parent published are the ones this projector produced for it,
        // when the parent's LOG is the blob it wrote: read from that LOG, they would be the same.
        let carried_ids = start > 0
            && match (written_parent, &self.written) {
                (Some(commit), Some(written)) if continues && commit == written.commit => objects
                    .blob_id(source, &format!("{commit}:{}", meta::LOG_FILE))?
                    .is_some_and(|id| {
                        gix::ObjectId::from_hex(id.as_bytes()).is_ok_and(|id| id == written.log)
                    }),
                _ => false,
            };
        let read;
        let published = if carried_ids {
            #[cfg(debug_assertions)]
            ensure!(
                published()?.ids == self.public_ids,
                "the records carried as published differ from the public parent's LOG"
            );
            None
        } else {
            read = published()?;
            Some(&read)
        };
        let known = std::mem::take(&mut self.public_ids);
        let mut published_paths = std::mem::take(&mut self.published_paths);
        let appended = log[start..].iter().map(String::as_str).collect::<Vec<_>>();
        let mut fresh = Vec::new();
        let mut seen = HashSet::new();
        for id in &appended {
            if !self.entries.contains_key(*id) && seen.insert(*id) {
                fresh.push(*id);
            }
        }
        let fresh_sizes = storage::local_event_sizes(source.root(), oid, &fresh)?;
        let fresh_sizes = fresh
            .iter()
            .copied()
            .zip(fresh_sizes)
            .collect::<HashMap<_, _>>();
        let size = |id: &str| {
            self.entries
                .get(id)
                .map(|entry| entry.size)
                .or_else(|| fresh_sizes.get(id).copied())
                .context("an event's size was not read")
        };
        let sizes = appended
            .iter()
            .map(|id| size(id))
            .collect::<Result<Vec<_>>>()?;
        let unique_bytes = self
            .unique_bytes
            .checked_add(fresh_sizes.values().sum())
            .context("unique event byte count overflow")?;
        let log_bytes = self
            .log_bytes
            .checked_add(sizes.iter().sum())
            .context("materialized size overflow")?;
        let mut view_bytes = 0usize;
        for id in &view {
            view_bytes = view_bytes
                .checked_add(size(id)?)
                .context("materialized size overflow")?;
        }
        storage::check_materialized_bounds(unique_bytes, log_bytes, view_bytes)?;

        // The records this snapshot shares with the source parent whose LOG its own extends go
        // out as that parent's public commit published them. A projector that continues from
        // that parent holds those lines already; one that starts here takes them from the public
        // parent, and the whole-LOG projection decides when one applies.
        let prefix = published
            .and_then(|published| published.prefix.as_ref())
            .filter(|_| start == 0);
        let writer = BlobWriter::open(source)?;
        let mut sequence = std::mem::take(&mut self.sequence);
        let mut deliveries = std::mem::take(&mut self.deliveries);
        let mut entries = std::mem::take(&mut self.entries);
        let mut occurrences = std::mem::take(&mut self.occurrences);
        let mut rewritten = self.rewritten;
        let mut waiting: Vec<Waiting> = Vec::new();
        let mut contents: Vec<serde_json::Value> = Vec::new();
        let mut batched = 0usize;
        let mut shared = 0usize;
        let mut prefix_holds = true;
        let mut settle = |occurrences: &mut Vec<Option<Public>>,
                          entries: &mut HashMap<String, Entry>,
                          rewritten: &mut usize,
                          waiting: &mut Vec<Waiting>,
                          contents: &mut Vec<serde_json::Value>|
         -> Result<()> {
            // Protection replaces in place what it changes, and leaves the rest as it was.
            rechecked.contents(source, contents)?;
            for (item, content) in waiting.drain(..).zip(contents.drain(..)) {
                let line = storage::envelope_line(&Envelope {
                    source: item.source,
                    session_id: item.session_id,
                    object_hash: crate::domain::transcript::object_hash(&content),
                    content,
                });
                let public = public_of(&writer, &line, &item.line)?;
                if public.blob.is_some() {
                    *rewritten += 1;
                }
                if let Some(id) = item.first
                    && let Some(entry) = entries.get_mut(&id)
                {
                    entry.public = Some(public.clone());
                }
                occurrences[item.occurrence] = Some(public);
            }
            Ok(())
        };
        storage::for_each_local_event(source.root(), oid, &appended, &sizes, |id, line| {
            let envelope = storage::parse_legacy_envelope_line(line)?;
            let first = !entries.contains_key(id);
            if first {
                entries.insert(
                    id.to_owned(),
                    Entry {
                        size: line.len(),
                        public: None,
                    },
                );
            }
            let position = occurrences.len();
            // A tool step's context this device's records vouch for goes out as the typed message
            // it was, as in `conversation_commit`.
            let typed = (envelope.source == "claude-code")
                .then(|| deliveries.as_typed(&envelope.content))
                .flatten();
            let recorded = typed.as_ref().unwrap_or(&envelope.content);
            let Some(content) = sequence.next(&envelope.source, recorded) else {
                rewritten += 1;
                occurrences.push(None);
                return Ok(());
            };
            let candidate = Envelope {
                source: envelope.source.clone(),
                session_id: envelope.session_id.clone(),
                object_hash: crate::domain::transcript::object_hash(&content),
                content,
            };
            let mut public_line = storage::envelope_line(&candidate);
            let public_id = storage::canonical_event_id(&public_line);
            let decided =
                if let Some(prefix) = prefix.filter(|prefix| position < prefix.source_lines) {
                    match prefix.public.get(shared) {
                        Some(published_id) => {
                            if *published_id != public_id {
                                public_line = prefix.line(published_id)?;
                            }
                            shared += 1;
                            true
                        }
                        None => {
                            prefix_holds = false;
                            true
                        }
                    }
                } else {
                    match published {
                        Some(published) => published.ids.contains(&public_id),
                        None => known.contains(&public_id),
                    }
                };
            if decided {
                let public = public_of(&writer, &public_line, line)?;
                if public.blob.is_some() {
                    rewritten += 1;
                }
                if first && let Some(entry) = entries.get_mut(id) {
                    entry.public = Some(public.clone());
                }
                occurrences.push(Some(public));
                return Ok(());
            }
            batched += public_line.len();
            occurrences.push(None);
            waiting.push(Waiting {
                occurrence: position,
                first: first.then(|| id.to_owned()),
                source: candidate.source,
                session_id: candidate.session_id,
                line: line.to_owned(),
            });
            contents.push(candidate.content);
            if batched >= super::PROTECTION_BATCH_BYTES {
                settle(
                    &mut occurrences,
                    &mut entries,
                    &mut rewritten,
                    &mut waiting,
                    &mut contents,
                )?;
                batched = 0;
            }
            Ok(())
        })?;
        settle(
            &mut occurrences,
            &mut entries,
            &mut rewritten,
            &mut waiting,
            &mut contents,
        )?;
        // The whole-LOG projection uses a public parent's lines only while every record it keeps
        // in the shared part has one; otherwise none of them, so neither does this.
        if prefix.is_some_and(|prefix| !prefix_holds || shared != prefix.public.len()) {
            *self = Self::default();
            return conversation_commit(
                source,
                oid,
                parents,
                published.context("a prefix comes from what was read")?,
                Carried::default(),
                rechecked,
            );
        }
        let mut public_ids = known;
        let mut new_paths = Vec::new();
        for public in occurrences[start..].iter().flatten() {
            public_ids.insert(public.id().to_owned());
            if published_paths.insert(public.path.clone()) {
                new_paths.push(public.path.clone());
            }
        }
        self.sequence = sequence;
        self.deliveries = deliveries;
        self.entries = entries;
        self.occurrences = occurrences;
        self.rewritten = rewritten;
        self.log_bytes = log_bytes;
        self.unique_bytes = unique_bytes;
        match &added {
            Some(added) => self
                .held
                .extend(added.iter().map(|added| added.path.clone())),
            None => self.held = held_events(source, oid)?,
        }
        self.public_ids = public_ids;
        self.published_paths = published_paths;
        self.log = log;
        self.at = Some(oid.to_owned());

        // As in `conversation_commit`, only an event's first occurrence names it and counts it.
        let firsts = || {
            self.entries
                .values()
                .filter_map(|entry| entry.public.as_ref())
        };
        let named = firsts()
            .map(|public| public.path.as_str())
            .collect::<HashSet<_>>();
        let left_out = firsts()
            .filter(|public| public.left_out)
            .map(|public| public.path.clone())
            .collect::<BTreeSet<_>>();
        let changed =
            self.rewritten > 0 || self.held.iter().any(|path| !named.contains(path.as_str()));
        if !changed {
            self.written = Some(Written {
                commit: oid.to_owned(),
                conversation: false,
                log: writer.id(storage::sequence_text(&self.log)?.as_bytes())?,
            });
            return Ok(Conversation {
                commit: None,
                left_out,
            });
        }

        let public_log = self
            .occurrences
            .iter()
            .flatten()
            .map(|public| public.id().to_owned())
            .collect::<Vec<_>>();
        let mut public_view = Vec::new();
        for id in &view {
            let entry = self
                .entries
                .get(id)
                .context("a session VIEW holds an event its LOG does not")?;
            if let Some(public) = &entry.public {
                public_view.push(public.id().to_owned());
            }
        }
        let log_text = storage::sequence_text(&public_log)?.into_bytes();
        let view_text = storage::sequence_text(&public_view)?.into_bytes();
        let log_id = writer.id(&log_text)?;
        let tree = match (outside, added, self.written.take()) {
            (Some(outside), Some(added), Some(written)) => {
                let edits = self.edits_on_parent(
                    objects,
                    source,
                    oid,
                    outside,
                    &added,
                    &new_paths,
                    (log_text.clone(), view_text.clone()),
                )?;
                let tree =
                    crate::commands::plumbing::tree_apply_content(source, &written.commit, edits)?;
                self.unchecked = Some(Unchecked {
                    tree: tree.clone(),
                    log: log_text,
                    view: view_text,
                });
                tree
            }
            _ => {
                let edits = self.full_edits(log_text, view_text)?;
                crate::commands::plumbing::tree_apply_content(source, oid, edits)?
            }
        };
        let original = objects.commit(source, oid)?;
        let commit = write_object(source, "commit", &commit_body(&tree, parents, &original)?)?;
        self.written = Some(Written {
            commit: commit.clone(),
            conversation: true,
            log: log_id,
        });
        Ok(Conversation {
            commit: Some(commit),
            left_out,
        })
    }

    /// The edits that make the public tree of the commit this projector holds out of that
    /// commit's own tree: every event object the tree holds that is not published is removed,
    /// events that changed are added, and LOG and VIEW are replaced. Every other path keeps its
    /// entry.
    fn full_edits(&self, log: Vec<u8>, view: Vec<u8>) -> Result<Edits> {
        let mut edits: Edits = self
            .held
            .iter()
            .filter(|path| !self.published_paths.contains(path.as_str()))
            .map(|path| (path.clone(), None))
            .collect();
        let mut added = HashSet::new();
        for public in self.occurrences.iter().flatten() {
            if !self.held.contains(&public.path) && added.insert(public.path.as_str()) {
                let blob = public
                    .blob
                    .context("a published event is missing from its snapshot's tree")?;
                edits.push((public.path.clone(), Some(Content::Blob(blob))));
            }
        }
        edits.push((meta::LOG_FILE.to_owned(), Some(Content::Bytes(log))));
        edits.push((meta::VIEW_FILE.to_owned(), Some(Content::Bytes(view))));
        Ok(edits)
    }

    /// The edits that make the public tree of `oid`, the commit this projector holds, out of the
    /// public tree written for its parent, which the full edits made out of the parent's tree:
    /// - every path outside `events/` the child changed takes the child's entry, or leaves;
    /// - each event object the child added (`added`) and each event path it publishes that the
    ///   parent did not (`new_paths`) is judged as the full edits judge it: held and published,
    ///   it keeps the child's entry; published only, it takes its public object; held only, it is
    ///   not there;
    /// - LOG and VIEW are replaced.
    ///
    /// Every other event path is held and published as it was for the parent, so the full edits
    /// leave it as they left it there.
    #[allow(clippy::too_many_arguments)]
    fn edits_on_parent(
        &self,
        objects: &Objects,
        source: &Repo,
        oid: &str,
        outside: Edits,
        added: &[Added],
        new_paths: &[String],
        (log, view): (Vec<u8>, Vec<u8>),
    ) -> Result<Edits> {
        let mut edits = outside
            .into_iter()
            .filter(|(path, _)| path != meta::LOG_FILE && path != meta::VIEW_FILE)
            .collect::<Vec<_>>();
        let by_path = added
            .iter()
            .map(|added| (added.path.as_str(), added))
            .collect::<HashMap<_, _>>();
        for added in added {
            if self.published_paths.contains(&added.path) {
                edits.push((
                    added.path.clone(),
                    Some(Content::Entry(added.kind, added.id)),
                ));
            }
        }
        let blobs = self
            .occurrences
            .iter()
            .flatten()
            .map(|public| (public.path.as_str(), public.blob))
            .collect::<HashMap<_, _>>();
        for path in new_paths {
            if by_path.contains_key(path.as_str()) {
                continue;
            }
            let content = if self.held.contains(path) {
                let (kind, id) = tree_entry(objects, source, oid, path)?;
                Content::Entry(kind, id)
            } else {
                Content::Blob(
                    blobs
                        .get(path.as_str())
                        .copied()
                        .flatten()
                        .context("a published event is missing from its snapshot's tree")?,
                )
            };
            edits.push((path.clone(), Some(content)));
        }
        edits.push((meta::LOG_FILE.to_owned(), Some(Content::Bytes(log))));
        edits.push((meta::VIEW_FILE.to_owned(), Some(Content::Bytes(view))));
        Ok(edits)
    }

    /// Builds the last public tree built on its parent's the full way, from the commit `at` it is
    /// for, and refuses the run when the two differ.
    fn check(&mut self, source: &Repo, at: Option<&str>) -> Result<()> {
        let Some(unchecked) = self.unchecked.take() else {
            return Ok(());
        };
        let at = at.context("a public tree has no snapshot")?;
        let edits = self.full_edits(unchecked.log, unchecked.view)?;
        let tree = crate::commands::plumbing::tree_apply_content(source, at, edits)?;
        ensure!(
            tree == unchecked.tree,
            "the public tree check found a difference, so nothing was published"
        );
        Ok(())
    }

    /// Ends the chain of public trees this projector built on their parents', checking its last
    /// one. Call it before the projector's state is dropped, and when a branch's last snapshot
    /// is projected.
    pub(super) fn finish(&mut self, source: &Repo) -> Result<()> {
        let at = self.at.clone();
        self.check(source, at.as_deref())
    }
}

/// The kind and object of the entry at `path` in `oid`'s tree.
fn tree_entry(
    objects: &Objects,
    source: &Repo,
    oid: &str,
    path: &str,
) -> Result<(gix::object::tree::EntryKind, gix::ObjectId)> {
    let (directory, name) = path
        .rsplit_once('/')
        .context("an event path has no directory")?;
    let entry = objects
        .tree(source, &format!("{oid}:{directory}"))?
        .into_iter()
        .flatten()
        .find(|entry| entry.name == name.as_bytes())
        .context("a published event is missing from its snapshot's tree")?;
    Ok((
        entry_kind(entry.mode)?,
        gix::ObjectId::from_hex(entry.id.as_bytes())?,
    ))
}

/// The kind of tree entry a canonical Git mode names.
fn entry_kind(mode: u32) -> Result<gix::object::tree::EntryKind> {
    use gix::object::tree::EntryKind;
    Ok(match mode {
        0o100644 => EntryKind::Blob,
        0o100755 => EntryKind::BlobExecutable,
        0o120000 => EntryKind::Link,
        0o160000 => EntryKind::Commit,
        0o040000 => EntryKind::Tree,
        _ => anyhow::bail!("a tree entry has an unknown mode {mode:o}"),
    })
}

/// How `line`, a public line, is carried for the recorded line `recorded`: its event's path, the
/// blob holding it when it differs from what was recorded, and whether it holds a placeholder the
/// privacy check put where it left something out.
fn public_of(writer: &BlobWriter, line: &str, recorded: &str) -> Result<Public> {
    use crate::domain::privacy::{
        projector::UNREADABLE_TEXT,
        service::{CROWDED_TEXT, OMITTED_TEXT},
    };
    Ok(Public {
        path: meta::event_path(&storage::event_id(line)?)?,
        blob: (line != recorded)
            .then(|| writer.write(line.as_bytes()))
            .transpose()?,
        left_out: line.contains(OMITTED_TEXT)
            || line.contains(CROWDED_TEXT)
            || line.contains(UNREADABLE_TEXT),
    })
}

/// The event objects `oid`'s tree adds to `held`, those its parent's tree holds, from the
/// difference between the two trees under `events/`: with `held`, what a full listing of `oid`
/// returns, without listing the events the two trees share. `None` when the difference removes or
/// changes an object the parent's tree holds, or adds one past `cap`, the most a session may hold:
/// only added objects leave what was read from the parent valid, and past the cap the full listing
/// counts the child's own tree. Each path is bounded while it is read, as the full listing bounds
/// it.
fn held_after(
    source: &Repo,
    held: &BTreeSet<String>,
    parent: &str,
    oid: &str,
    cap: usize,
) -> Result<Option<Vec<Added>>> {
    let mut added = Vec::new();
    let mut seen = HashSet::new();
    let mut kept = true;
    each_change(
        source,
        parent,
        oid,
        Some(meta::EVENTS_DIR),
        MAX_EVENT_PATH_BYTES,
        |change| {
            if !kept {
                return Ok(());
            }
            match change.status.as_slice() {
                b"D" | b"M" | b"T" => kept = false,
                // The parent's set counts the child's tree only while nothing is removed, and a
                // removal can come later in the difference: the full listing decides.
                b"A" if held.len() + seen.len() >= cap && !held.contains(&change.path) => {
                    kept = false
                }
                b"A" => {
                    if seen.insert(change.path.clone()) {
                        added.push(Added {
                            kind: entry_kind(change.mode)?,
                            id: change.id,
                            path: change.path,
                        });
                    }
                }
                _ => anyhow::bail!("unexpected change to a session tree's events"),
            }
            Ok(())
        },
    )?;
    Ok(kept.then_some(added))
}

/// What `oid`'s tree changes from its parent's outside `events/`, as tree edits that give each
/// changed path the child's entry or take it away; `None` when one changed path lies inside
/// another, as a file that became a directory does, which edits by path can't express.
fn changes_outside_events(source: &Repo, parent: &str, oid: &str) -> Result<Option<Edits>> {
    let mut edits = Vec::new();
    each_change(
        source,
        parent,
        oid,
        None,
        MAX_OUTSIDE_PATH_BYTES,
        |change| {
            if change.path == meta::EVENTS_DIR
                || change
                    .path
                    .strip_prefix(meta::EVENTS_DIR)
                    .is_some_and(|rest| rest.starts_with('/'))
            {
                return Ok(());
            }
            let content = match change.status.as_slice() {
                b"D" => None,
                b"A" | b"M" | b"T" => Some(Content::Entry(entry_kind(change.mode)?, change.id)),
                _ => anyhow::bail!("unexpected change to a session tree"),
            };
            edits.push((change.path, content));
            Ok(())
        },
    )?;
    let paths = edits
        .iter()
        .map(|(path, _)| path.as_str())
        .collect::<HashSet<_>>();
    let nested = paths.iter().any(|path| {
        let mut parent = path.rsplit_once('/').map(|(parent, _)| parent);
        while let Some(directory) = parent {
            if paths.contains(directory) {
                return true;
            }
            parent = directory.rsplit_once('/').map(|(next, _)| next);
        }
        false
    });
    Ok((!nested).then_some(edits))
}

/// The longest path outside `events/` a public tree built on its parent's reads from a
/// difference; a longer one is refused while it is read, before it is held whole.
const MAX_OUTSIDE_PATH_BYTES: usize = 64 * 1024;

/// One entry of a raw tree difference: its status letter, the child's mode and object, and the
/// path.
struct Change {
    status: Vec<u8>,
    mode: u32,
    id: gix::ObjectId,
    path: String,
}

/// Each entry of the raw, recursive difference from `parent`'s tree to `oid`'s, under `under`
/// when it is given, renames shown as a removal and an addition. Each field is bounded by `limit`
/// while it is read.
fn each_change(
    source: &Repo,
    parent: &str,
    oid: &str,
    under: Option<&str>,
    limit: usize,
    mut each: impl FnMut(Change) -> Result<()>,
) -> Result<()> {
    let mut args = vec![
        "diff-tree",
        "-r",
        "-z",
        "--raw",
        "--no-renames",
        parent,
        oid,
    ];
    if let Some(under) = under {
        args.extend(["--", under]);
    }
    let mut header: Option<Vec<u8>> = None;
    source.git_stream_split_local_bounded(&args, 0, limit, |field| {
        let Some(fields) = header.take() else {
            header = Some(field.to_vec());
            return Ok(());
        };
        // `:<old mode> <new mode> <old object> <new object> <status>`
        let fields = std::str::from_utf8(&fields).context("a tree difference is not text")?;
        let parts = fields
            .strip_prefix(':')
            .context("a tree difference entry has no header")?
            .split(' ')
            .collect::<Vec<_>>();
        let [_, mode, _, id, status] = parts.as_slice() else {
            anyhow::bail!("a tree difference entry is malformed");
        };
        each(Change {
            status: status.as_bytes().to_vec(),
            mode: u32::from_str_radix(mode, 8).context("a tree difference mode is malformed")?,
            id: gix::ObjectId::from_hex(id.as_bytes())?,
            path: String::from_utf8(field.to_vec()).context("a tree path is not UTF-8")?,
        })
    })?;
    ensure!(header.is_none(), "a session tree diff ended mid-entry");
    Ok(())
}

#[cfg(test)]
#[path = "session_projection_tests.rs"]
mod tests;
