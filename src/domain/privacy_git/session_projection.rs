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
    /// already published.
    pub(super) fn project(
        &mut self,
        source: &Repo,
        oid: &str,
        parents: &[String],
        published: &Published<'_>,
        rechecked: &mut Rechecked,
    ) -> Result<Conversation> {
        let Some((log, view)) = storage::local_v1_sequences(source.root(), oid)? else {
            // A v0 snapshot is rebuilt in the v1 layout as a whole.
            *self = Self::default();
            return conversation_commit(
                source,
                oid,
                parents,
                published,
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
        let carried = match previous.as_deref() {
            Some(parent)
                if matches!(parents, [only] if only == parent) && log.starts_with(&self.log) =>
            {
                held_after(
                    source,
                    std::mem::take(&mut self.held),
                    parent,
                    oid,
                    crate::domain::storage::MAX_SEQUENCE_EVENTS,
                )?
            }
            _ => None,
        };
        let continues = carried.is_some();
        if !continues {
            *self = Self::default();
        }
        let start = self.log.len();
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
        let prefix = published.prefix.as_ref().filter(|_| start == 0);
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
                    published.ids.contains(&public_id)
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
                published,
                Carried::default(),
                rechecked,
            );
        }
        self.sequence = sequence;
        self.deliveries = deliveries;
        self.entries = entries;
        self.occurrences = occurrences;
        self.rewritten = rewritten;
        self.log_bytes = log_bytes;
        self.unique_bytes = unique_bytes;
        self.held = match carried {
            Some(held) => held,
            None => held_events(source, oid)?,
        };
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
        // Only what changes is written: every event object the tree holds that is not published
        // is removed, events that changed are added, and LOG and VIEW are replaced. Every other
        // event keeps its object.
        let files = self
            .occurrences
            .iter()
            .flatten()
            .map(|public| public.path.as_str())
            .collect::<HashSet<_>>();
        let mut edits: Vec<(String, Option<Content>)> = self
            .held
            .iter()
            .filter(|path| !files.contains(path.as_str()))
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
        edits.push((
            meta::LOG_FILE.to_owned(),
            Some(Content::Bytes(
                storage::sequence_text(&public_log)?.into_bytes(),
            )),
        ));
        edits.push((
            meta::VIEW_FILE.to_owned(),
            Some(Content::Bytes(
                storage::sequence_text(&public_view)?.into_bytes(),
            )),
        ));
        let tree = crate::commands::plumbing::tree_apply_content(source, oid, edits)?;
        let original = read(source, &["cat-file", "commit", oid], MAX_COMMIT)?;
        Ok(Conversation {
            commit: Some(write_object(
                source,
                "commit",
                &commit_body(&tree, parents, &original)?,
            )?),
            left_out,
        })
    }
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

/// The event objects `oid`'s tree holds, from those its parent's tree holds and the difference
/// between the two trees under `events/`: what a full listing of `oid` returns, without listing
/// the events the two trees share. `None` when the difference removes or changes an object the
/// parent's tree holds, or adds one past `cap`, the most a session may hold: only added objects
/// leave what was read from the parent valid, and past the cap the full listing counts the
/// child's own tree. Each path is bounded while it is read, as the full listing bounds it.
fn held_after(
    source: &Repo,
    mut held: BTreeSet<String>,
    parent: &str,
    oid: &str,
    cap: usize,
) -> Result<Option<BTreeSet<String>>> {
    let mut status: Option<Vec<u8>> = None;
    let mut kept = true;
    source.git_stream_split_local_bounded(
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--name-status",
            parent,
            oid,
            "--",
            meta::EVENTS_DIR,
        ],
        0,
        MAX_EVENT_PATH_BYTES,
        |field| {
            let Some(change) = status.take() else {
                status = Some(field.to_vec());
                return Ok(());
            };
            if !kept {
                return Ok(());
            }
            let path = String::from_utf8(field.to_vec()).context("event path is not UTF-8")?;
            match change.as_slice() {
                b"D" | b"M" | b"T" => kept = false,
                // The parent's set counts the child's tree only while nothing is removed, and a
                // removal can come later in the difference: the full listing decides.
                b"A" if held.len() >= cap && !held.contains(&path) => kept = false,
                b"A" => {
                    held.insert(path);
                }
                _ => anyhow::bail!("unexpected change to a session tree's events"),
            }
            Ok(())
        },
    )?;
    ensure!(status.is_none(), "a session tree diff ended mid-entry");
    Ok(kept.then_some(held))
}

#[cfg(test)]
#[path = "session_projection_tests.rs"]
mod tests;
