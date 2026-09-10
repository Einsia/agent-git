//! Immutable, bounded VIEW selections with LOG-backed event coordinates.

use crate::Result;
use crate::domain::meta::{self, LayoutVersion};
use crate::domain::refs::{self, RefSpec, Tail};
use crate::domain::repo::{ObjectBody, Repo};
use crate::domain::storage;
use anyhow::Context;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

pub(super) const MAX_INPUT_BYTES: usize = 1024 * 1024;
pub(super) const MAX_EVENTS: usize = 4096;
const MAX_COMMITS: usize = 256;
const MAX_TARGETS: usize = 16;
const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct Scope {
    pub sha: String,
    pub branch: Option<String>,
    pub events: Vec<Event>,
    pub unlocated_events: usize,
    pub missing_events: usize,
}

#[derive(Debug)]
pub(super) struct Event {
    pub locator: String,
    pub envelope: Value,
}

/// Resolve every target before reading its transcript. A symbolic ref never reaches a blob read.
pub(super) fn collect(repo: &Repo, targets: &[RefSpec]) -> Result<Vec<Scope>> {
    anyhow::ensure!(
        !targets.is_empty(),
        "sensitive review needs a session target"
    );
    anyhow::ensure!(
        targets.len() <= MAX_TARGETS,
        "sensitive review exceeds the {MAX_TARGETS}-target limit"
    );
    let mut frozen = Vec::with_capacity(targets.len());
    for target in targets {
        if let refs::Base::Name(name) | refs::Base::SessionBranch(name) = &target.base {
            anyhow::ensure!(
                name.len() <= 1024,
                "sensitive review reference exceeds the name limit"
            );
        }
        anyhow::ensure!(
            !matches!(target.tail, Tail::Path(_)),
            "sensitive review does not support file-path targets"
        );
        let mut base = target.clone();
        base.tail = Tail::None;
        frozen.push((refs::resolve(repo, &base)?, target.tail.clone()));
    }

    let mut reader = Reader::new(repo);
    let mut budget = ReviewBudget::default();
    let mut scopes = Vec::with_capacity(frozen.len());
    for (base, tail) in frozen {
        let mut chain = read_chain(&mut reader, &base.sha)?;
        let (index, turns, event_index) = select(&chain, &tail)?;
        chain.entries.truncate(index + 1);
        let selected = chain.entries.last().context("empty selected history")?;
        let snapshot = selected.meta.as_ref().context("missing session metadata")?;
        anyhow::ensure!(
            !snapshot.is_file_line(),
            "sensitive review requires a session line, not a file line"
        );
        let sha = selected.sha.clone();
        let layout = snapshot.layout;
        let (log, coordinates) = locate_log(&mut reader, &chain)?;
        let view = reader.sequence(&sha, layout, true)?;

        // Select coordinates before intersecting with VIEW. Identical envelopes have counted
        // occurrences; their content hash alone cannot distinguish another session's evidence.
        let mut wanted: HashMap<&str, VecDeque<usize>> = HashMap::new();
        let mut logged: HashMap<&str, usize> = HashMap::new();
        for (index, id) in log.iter().enumerate() {
            *logged.entry(id).or_default() += 1;
            let include = match turns {
                None => true,
                Some((a, b)) => coordinates[index].is_some_and(|(turn, event)| {
                    (a..=b).contains(&turn) && event_index.is_none_or(|wanted| wanted == event)
                }),
            };
            if include {
                wanted.entry(id).or_default().push_back(index);
            }
        }
        if let Some(event) = event_index {
            anyhow::ensure!(
                wanted.values().any(|positions| !positions.is_empty()),
                "selected turn has no event {event}"
            );
        }
        let mut scope = Scope {
            sha,
            branch: matches!(tail, Tail::None).then_some(base.branch).flatten(),
            events: Vec::new(),
            unlocated_events: 0,
            missing_events: 0,
        };
        for id in view {
            // Scope filters cannot make orphan or excess VIEW occurrences valid LOG evidence.
            let remaining = logged.get_mut(id.as_str());
            match remaining {
                Some(count) if *count > 0 => *count -= 1,
                _ => {
                    scope.missing_events += 1;
                    budget.add(reader.event(&id)?.len())?;
                    continue;
                }
            }
            let position = wanted.get_mut(id.as_str()).and_then(VecDeque::pop_front);
            let Some(position) = position else {
                continue;
            };
            let canonical = reader.event(&id)?;
            budget.add(canonical.len())?;
            let envelope: Value = serde_json::from_str(canonical)?;
            let marker = envelope["content"]["subtype"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("agit:"));
            let Some((turn, event)) = coordinates[position].filter(|_| !marker) else {
                scope.unlocated_events += 1;
                continue;
            };
            scope.events.push(Event {
                locator: format!("@#{turn}.{event}"),
                envelope,
            });
        }
        scopes.push(scope);
    }
    Ok(scopes)
}

#[derive(Default)]
struct ReviewBudget {
    bytes: usize,
    events: usize,
}

impl ReviewBudget {
    fn add(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .context("review byte overflow")?;
        self.events += 1;
        anyhow::ensure!(
            self.bytes <= MAX_INPUT_BYTES,
            "sensitive review exceeds the {MAX_INPUT_BYTES}-byte input limit"
        );
        anyhow::ensure!(
            self.events <= MAX_EVENTS,
            "sensitive review exceeds the {MAX_EVENTS}-event input limit"
        );
        Ok(())
    }
}

fn read_chain(reader: &mut Reader<'_>, sha: &str) -> Result<refs::Chain> {
    let mut shas = Vec::new();
    reader.repo.git_stream_split(
        &["rev-list", "--first-parent", "--max-count=257", sha],
        b'\n',
        |record| {
            if record.is_empty() {
                return Ok(());
            }
            anyhow::ensure!(
                shas.len() < MAX_COMMITS,
                "sensitive review exceeds the {MAX_COMMITS}-commit history limit"
            );
            let oid = std::str::from_utf8(record).context("non-UTF-8 history id")?;
            anyhow::ensure!(meta::is_event_id(oid), "invalid history commit id");
            shas.push(oid.to_owned());
            Ok(())
        },
    )?;
    anyhow::ensure!(!shas.is_empty(), "selected commit has no history");
    reader.verify_chain(&shas)?;
    shas.reverse();
    let specs = shas
        .iter()
        .map(|sha| format!("{sha}:{}", meta::FILE))
        .collect();
    let oids = reader.load(specs)?;
    let mut entries = Vec::with_capacity(shas.len());
    for (sha, oid) in shas.into_iter().zip(oids) {
        let text =
            std::str::from_utf8(reader.blob(&oid)?).context("session metadata is not UTF-8")?;
        let snapshot = meta::parse_strict(text, &sha)
            .map_err(|_| anyhow::anyhow!("invalid session metadata at {sha}"))?;
        entries.push(refs::ChainEntry {
            sha,
            meta: Some(snapshot),
        });
    }
    Ok(refs::Chain {
        entries,
        declared: true,
    })
}

pub(crate) struct FrozenReviewLog {
    pub log: String,
    chain: refs::Chain,
    coordinates: Coordinates,
}

impl FrozenReviewLog {
    pub fn turn_lines(&self, ordinal: u32) -> Result<(u32, Vec<usize>)> {
        let (turn, _) = selected_turn(&self.chain, ordinal)?;
        let lines = self
            .coordinates
            .iter()
            .enumerate()
            .filter_map(|(index, coordinate)| {
                coordinate
                    .is_some_and(|(selected, _)| selected == turn)
                    .then_some(index)
            })
            .collect();
        Ok((turn, lines))
    }
}

/// A guarded remedy consumes this proof without resolving the mutable traversal again.
pub(crate) fn freeze_review_log(repo: &Repo, sha: &str) -> Result<FrozenReviewLog> {
    let mut reader = Reader::new(repo);
    let chain = read_chain(&mut reader, sha)?;
    let (ids, coordinates) = locate_log(&mut reader, &chain)?;
    let mut log = String::new();
    for id in ids {
        log.push_str(reader.event(&id)?);
        anyhow::ensure!(
            log.len() <= MAX_INPUT_BYTES,
            "guarded review LOG exceeds the byte limit"
        );
    }
    Ok(FrozenReviewLog {
        log,
        chain,
        coordinates,
    })
}

type Selection = (usize, Option<(u32, u32)>, Option<u32>);

fn selected_turn(chain: &refs::Chain, ordinal: u32) -> Result<(u32, String)> {
    let (turn, sha) = refs::turn_in(chain, ordinal)?;
    anyhow::ensure!(
        turn > 0 && turn != refs::LAST_TURN,
        "selected turn ordinal cannot form an event locator"
    );
    let (_, canonical) = refs::turn_in(chain, turn)?;
    anyhow::ensure!(
        sha == canonical,
        "selected turn has duplicate settlements and cannot form an unambiguous event locator"
    );
    let snapshot = chain
        .entries
        .iter()
        .find(|entry| entry.sha == sha)
        .and_then(|entry| entry.meta.as_ref())
        .context("missing selected turn metadata")?;
    anyhow::ensure!(
        !snapshot.is_file_line() && !snapshot.session.is_empty(),
        "selected turn requires a claimed session line"
    );
    Ok((turn, sha))
}

fn select(chain: &refs::Chain, tail: &Tail) -> Result<Selection> {
    let tip = chain.entries.len() - 1;
    let position = |sha: &str| {
        chain
            .entries
            .iter()
            .position(|entry| entry.sha == sha)
            .context("selected turn is outside the frozen history")
    };
    match tail {
        Tail::None => Ok((tip, None, None)),
        Tail::Tilde(n) => Ok((
            tip.checked_sub(*n as usize)
                .context("selected ancestor is outside history")?,
            None,
            None,
        )),
        Tail::Turn(n) | Tail::Event { turn: n, .. } => {
            let (turn, sha) = selected_turn(chain, *n)?;
            let event = match tail {
                Tail::Event { index, .. } => {
                    anyhow::ensure!(*index > 0, "event numbers start at 1");
                    Some(*index)
                }
                _ => None,
            };
            Ok((position(&sha)?, Some((turn, turn)), event))
        }
        Tail::Range { a, b } => {
            let (a, start) = selected_turn(chain, *a)?;
            let (b, sha) = selected_turn(chain, *b)?;
            anyhow::ensure!(
                a > 0 && b != refs::LAST_TURN && a <= b,
                "sensitive review turn range must contain ascending addressable ordinals"
            );
            anyhow::ensure!(
                position(&start)? <= position(&sha)?,
                "sensitive review turn range disagrees with first-parent history order"
            );
            anyhow::ensure!(
                u64::from(b) - u64::from(a) < MAX_COMMITS as u64,
                "sensitive review turn range exceeds the bounded history"
            );
            let start_index = position(&start)?;
            let end_index = position(&sha)?;
            for ordinal in a..=b {
                let (_, member) = selected_turn(chain, ordinal)?;
                anyhow::ensure!(
                    (start_index..=end_index).contains(&position(&member)?),
                    "sensitive review turn range omits a requested turn outside its history interval"
                );
            }
            Ok((position(&sha)?, Some((a, b)), None))
        }
        Tail::Path(_) => anyhow::bail!("sensitive review does not support file-path targets"),
    }
}

type Coordinates = Vec<Option<(u32, u32)>>;

fn locate_log(reader: &mut Reader<'_>, chain: &refs::Chain) -> Result<(Vec<String>, Coordinates)> {
    let mut previous = Vec::new();
    let mut coordinates = Vec::new();
    let mut settled = HashSet::new();
    for entry in &chain.entries {
        let snapshot = entry
            .meta
            .as_ref()
            .context("missing historical session metadata")?;
        if snapshot.is_file_line() || snapshot.session.is_empty() {
            anyhow::ensure!(
                previous.is_empty(),
                "session LOG disappears inside selected history"
            );
            continue;
        }
        let log = reader.sequence(&entry.sha, snapshot.layout, false)?;
        anyhow::ensure!(
            log.starts_with(&previous),
            "commit {} rewrites its first parent's LOG instead of appending",
            entry.sha
        );
        let turn = if snapshot.kind == meta::Kind::Turn {
            snapshot
                .turn
                .filter(|turn| *turn > 0 && *turn != refs::LAST_TURN && settled.insert(*turn))
        } else {
            None
        };
        coordinates.extend(
            (0..log.len() - previous.len())
                .map(|offset| turn.map(|turn| (turn, offset as u32 + 1))),
        );
        previous = log;
    }
    Ok((previous, coordinates))
}

struct Reader<'a> {
    repo: &'a Repo,
    blobs: HashMap<String, Vec<u8>>,
    sequences: HashMap<(String, bool), Vec<String>>,
    events: HashMap<String, String>,
    verified: HashSet<(String, String)>,
    history_bytes: usize,
    canonical_bytes: usize,
}

impl<'a> Reader<'a> {
    fn new(repo: &'a Repo) -> Self {
        Self {
            repo,
            blobs: HashMap::new(),
            sequences: HashMap::new(),
            events: HashMap::new(),
            verified: HashSet::new(),
            history_bytes: 0,
            canonical_bytes: 0,
        }
    }

    /// Locators require the immutable parent edges and a real root, not a clipped traversal.
    /// Shallow boundaries and grafts can otherwise change coordinates without changing the tip.
    fn verify_chain(&mut self, shas: &[String]) -> Result<()> {
        let mut index = 0;
        self.repo
            .git_cat_file_batch_check(shas.to_vec(), |oid, kind, size| {
                anyhow::ensure!(
                    shas.get(index).is_some_and(|sha| sha == oid) && kind == "commit",
                    "sensitive review history needs immutable commit objects"
                );
                index += 1;
                anyhow::ensure!(
                    size <= MAX_INPUT_BYTES as u64,
                    "sensitive review commit exceeds the byte limit"
                );
                self.history_bytes = self
                    .history_bytes
                    .checked_add(size as usize)
                    .context("sensitive review history byte overflow")?;
                anyhow::ensure!(
                    self.history_bytes <= MAX_HISTORY_BYTES,
                    "sensitive review exceeds the {MAX_HISTORY_BYTES}-byte history limit"
                );
                Ok(())
            })?;
        anyhow::ensure!(index == shas.len(), "missing review commit headers");
        let mut index = 0;
        self.repo.git_cat_file_batch(
            shas.to_vec(),
            MAX_INPUT_BYTES,
            |oid, kind, body| {
                anyhow::ensure!(
                    shas.get(index).is_some_and(|sha| sha == oid) && kind == "commit",
                    "sensitive review history commit changed"
                );
                let ObjectBody::Read(bytes) = body else {
                    anyhow::bail!("sensitive review commit exceeds the byte limit");
                };
                let end = bytes
                    .windows(2)
                    .position(|pair| pair == b"\n\n")
                    .context("invalid sensitive review commit headers")?;
                let mut first_parent = None;
                for line in bytes[..end].split(|byte| *byte == b'\n') {
                    if let Some(parent) = line.strip_prefix(b"parent ") {
                        anyhow::ensure!(
                            std::str::from_utf8(parent)
                                .ok()
                                .is_some_and(meta::is_event_id),
                            "invalid sensitive review commit parent"
                        );
                        first_parent.get_or_insert(parent);
                    }
                }
                anyhow::ensure!(
                    first_parent == shas.get(index + 1).map(|sha| sha.as_bytes()),
                    "sensitive review history is incomplete or rewritten; full immutable first-parent ancestry is required"
                );
                index += 1;
                Ok(())
            },
        )?;
        anyhow::ensure!(index == shas.len(), "missing review commit bodies");
        Ok(())
    }

    /// Check every immutable path even if its body is cached: another tree may omit its CAS file.
    fn load(&mut self, specs: Vec<String>) -> Result<Vec<String>> {
        anyhow::ensure!(
            specs.len() <= MAX_EVENTS,
            "too many sensitive review blob requests"
        );
        let mut all = Vec::with_capacity(specs.len());
        let mut pending = Vec::new();
        let mut seen = HashSet::new();
        let mut index = 0;
        self.repo
            .git_cat_file_batch_check(specs.clone(), |oid, kind, size| {
                let spec = specs.get(index).context("unexpected review blob header")?;
                index += 1;
                anyhow::ensure!(
                    kind == "blob",
                    "sensitive review needs a readable blob at {spec}"
                );
                anyhow::ensure!(
                    size <= MAX_INPUT_BYTES as u64,
                    "sensitive review blob exceeds the {MAX_INPUT_BYTES}-byte limit at {spec}"
                );
                if !self.blobs.contains_key(oid) && seen.insert(oid.to_owned()) {
                    self.history_bytes = self
                        .history_bytes
                        .checked_add(size as usize)
                        .context("sensitive review history byte overflow")?;
                    anyhow::ensure!(
                        self.history_bytes <= MAX_HISTORY_BYTES,
                        "sensitive review exceeds the {MAX_HISTORY_BYTES}-byte history limit"
                    );
                    pending.push(oid.to_owned());
                }
                all.push(oid.to_owned());
                Ok(())
            })?;
        anyhow::ensure!(
            index == specs.len(),
            "missing sensitive review blob headers"
        );
        self.repo
            .git_cat_file_batch(pending, MAX_INPUT_BYTES, |oid, kind, body| {
                anyhow::ensure!(kind == "blob", "sensitive review object is not a blob");
                let ObjectBody::Read(bytes) = body else {
                    anyhow::bail!("sensitive review blob exceeds the byte limit");
                };
                self.blobs.insert(oid.to_owned(), bytes.to_vec());
                Ok(())
            })?;
        Ok(all)
    }

    fn blob(&self, oid: &str) -> Result<&[u8]> {
        self.blobs
            .get(oid)
            .map(Vec::as_slice)
            .context("missing review blob body")
    }

    fn event(&self, id: &str) -> Result<&str> {
        self.events
            .get(id)
            .map(String::as_str)
            .context("missing validated review envelope")
    }

    fn remember(&mut self, id: String, canonical: String) -> Result<()> {
        if let Some(existing) = self.events.get(&id) {
            anyhow::ensure!(
                existing == &canonical,
                "event id names different full envelopes"
            );
            return Ok(());
        }
        self.canonical_bytes = self
            .canonical_bytes
            .checked_add(canonical.len())
            .context("canonical review byte overflow")?;
        anyhow::ensure!(
            self.canonical_bytes <= MAX_HISTORY_BYTES,
            "canonical review history exceeds the {MAX_HISTORY_BYTES}-byte limit"
        );
        self.events.insert(id, canonical);
        Ok(())
    }

    fn sequence(&mut self, sha: &str, layout: LayoutVersion, view: bool) -> Result<Vec<String>> {
        let path = match (layout, view) {
            (LayoutVersion::V0, false) => meta::LEGACY_LOG_FILE,
            (LayoutVersion::V0, true) => meta::LEGACY_VIEW_FILE,
            (LayoutVersion::V1, false) => meta::LOG_FILE,
            (LayoutVersion::V1, true) => meta::VIEW_FILE,
        };
        let oid = self.load(vec![format!("{sha}:{path}")])?.remove(0);
        let key = (oid.clone(), layout == LayoutVersion::V1);
        let ids = if let Some(ids) = self.sequences.get(&key) {
            ids.clone()
        } else {
            let text =
                std::str::from_utf8(self.blob(&oid)?).context("review sequence is not UTF-8")?;
            anyhow::ensure!(
                text.bytes().filter(|byte| *byte == b'\n').count() <= MAX_EVENTS,
                "sensitive review sequence exceeds the {MAX_EVENTS}-event limit"
            );
            let mut canonical = Vec::new();
            let ids = match layout {
                LayoutVersion::V1 => storage::parse_sequence(text).map_err(|_| {
                    anyhow::anyhow!("invalid review event sequence at {sha}:{path}")
                })?,
                LayoutVersion::V0 => {
                    let mut ids = Vec::new();
                    let mut bytes = 0usize;
                    for line in text.split_inclusive('\n') {
                        super::json::parse(line).map_err(|_| {
                            anyhow::anyhow!("invalid legacy review envelope at {sha}:{path}")
                        })?;
                        let envelope = storage::parse_legacy_envelope_line(line).map_err(|_| {
                            anyhow::anyhow!("invalid legacy review envelope at {sha}:{path}")
                        })?;
                        let line = storage::envelope_line(&envelope);
                        bytes = bytes
                            .checked_add(line.len())
                            .context("review sequence byte overflow")?;
                        anyhow::ensure!(
                            bytes <= MAX_INPUT_BYTES,
                            "canonical review sequence exceeds the byte limit"
                        );
                        let id = storage::event_id(&line)?;
                        ids.push(id.clone());
                        canonical.push((id, line));
                    }
                    ids
                }
            };
            anyhow::ensure!(
                ids.len() <= MAX_EVENTS,
                "sensitive review sequence exceeds the event limit"
            );
            for (id, line) in canonical {
                self.remember(id, line)?;
            }
            self.sequences.insert(key, ids.clone());
            ids
        };

        if layout == LayoutVersion::V1 {
            let unique: Vec<&str> = ids
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let specs = unique
                .iter()
                .map(|id| Ok(format!("{sha}:{}", meta::event_path(id)?)))
                .collect::<Result<Vec<_>>>()?;
            let oids = self.load(specs)?;
            for (id, oid) in unique.into_iter().zip(oids) {
                if self.verified.contains(&(id.to_owned(), oid.clone())) {
                    continue;
                }
                let text =
                    std::str::from_utf8(self.blob(&oid)?).context("review event is not UTF-8")?;
                storage::parse_envelope_line(text)
                    .map_err(|_| anyhow::anyhow!("invalid historical CAS review envelope"))?;
                anyhow::ensure!(
                    storage::event_id(text)? == id,
                    "historical CAS event id mismatch"
                );
                let canonical = text.to_owned();
                self.remember(id.to_owned(), canonical)?;
                self.verified.insert((id.to_owned(), oid));
            }
        }
        let mut bytes = 0usize;
        for id in &ids {
            bytes = bytes
                .checked_add(self.event(id)?.len())
                .context("review sequence byte overflow")?;
            anyhow::ensure!(
                bytes <= MAX_INPUT_BYTES,
                "materialized review sequence exceeds the {MAX_INPUT_BYTES}-byte limit"
            );
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transcript::{self, Envelope};

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: Repo,
        layout: LayoutVersion,
    }

    impl Fixture {
        fn new(layout: LayoutVersion) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repo::init(&dir.path().join("review")).unwrap();
            repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
            meta::ensure_session_dir(repo.root()).unwrap();
            meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
            repo.add_all().unwrap();
            assert!(repo.commit("initialize synthetic file line").unwrap());
            repo.git(&["switch", "-c", "session"]).unwrap();
            Self {
                _dir: dir,
                repo,
                layout,
            }
        }

        fn commit(&self, log: &str, view: &str, kind: meta::Kind, turn: Option<u32>) -> String {
            let mut snapshot = meta::Meta::new(claim('a'), "codex".into(), "/synthetic".into());
            snapshot.layout = self.layout;
            snapshot.kind = kind;
            snapshot.turn = turn;
            meta::write(self.repo.root(), &snapshot).unwrap();
            match self.layout {
                LayoutVersion::V0 => {
                    std::fs::write(self.repo.root().join(meta::LEGACY_LOG_FILE), log).unwrap();
                    std::fs::write(self.repo.root().join(meta::LEGACY_VIEW_FILE), view).unwrap();
                }
                LayoutVersion::V1 => storage::write_snapshot(self.repo.root(), log, view).unwrap(),
            }
            self.repo.add_all().unwrap();
            assert!(
                self.repo
                    .commit("record synthetic review snapshot")
                    .unwrap()
            );
            self.repo.git(&["rev-parse", "HEAD"]).unwrap()
        }

        fn collect(&self, target: &str) -> Result<Vec<Scope>> {
            collect(&self.repo, &[refs::parse(target)?])
        }
    }

    fn claim(character: char) -> String {
        format!("agit-{}", character.to_string().repeat(40))
    }

    fn event(text: &str, source: &str, session: char) -> String {
        let content =
            serde_json::json!({"type": "user", "message": {"role": "user", "content": text}});
        storage::envelope_line(&Envelope {
            source: source.into(),
            session_id: claim(session),
            object_hash: transcript::object_hash(&content),
            content,
        })
    }

    fn locators(scope: &Scope) -> Vec<&str> {
        scope
            .events
            .iter()
            .map(|event| event.locator.as_str())
            .collect()
    }

    #[test]
    fn selected_duplicates_intersect_view_by_full_envelope_and_multiplicity() {
        for layout in [LayoutVersion::V0, LayoutVersion::V1] {
            let fixture = Fixture::new(layout);
            let duplicate = event("same content", "codex", 'a');
            let foreign = event("same content", "claude-code", 'b');
            let removed = event("removed event", "codex", 'a');
            let log = format!("{duplicate}{duplicate}{foreign}{removed}");
            let view = format!("{duplicate}{foreign}");
            let sha = fixture.commit(&log, &view, meta::Kind::Turn, Some(1));

            let all = fixture.collect("session").unwrap();
            assert_eq!(all[0].sha, sha);
            assert_eq!(all[0].branch.as_deref(), Some("session"));
            assert_eq!(locators(&all[0]), ["@#1.1", "@#1.3"]);
            assert_eq!(all[0].unlocated_events + all[0].missing_events, 0);

            let second = fixture.collect("session#1.2").unwrap();
            assert_eq!(locators(&second[0]), ["@#1.2"]);
            assert_eq!(second[0].events[0].envelope["_source"], "codex");
            assert_eq!(second[0].events[0].envelope["_session_id"], claim('a'));
            assert!(second[0].branch.is_none());
            assert!(fixture.collect("session#1.4").unwrap()[0].events.is_empty());

            let turn = fixture.collect("session#1").unwrap();
            assert_eq!(locators(&turn[0]), ["@#1.1", "@#1.3"]);
            assert!(fixture.collect("session#1.5").is_err());
        }
    }

    #[test]
    fn selectors_use_the_historical_view_and_validated_first_parent_turn_ranges() {
        for layout in [LayoutVersion::V0, LayoutVersion::V1] {
            let fixture = Fixture::new(layout);
            let first = event("first", "codex", 'a');
            let second = event("second", "codex", 'a');
            let third = event("third", "codex", 'a');
            let first_sha = fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
            let log = format!("{first}{second}");
            fixture.commit(&log, &log, meta::Kind::Turn, Some(2));
            let log = format!("{log}{third}");
            fixture.commit(&log, &second, meta::Kind::Turn, Some(3));

            let first_scope = fixture.collect("session#1").unwrap();
            assert_eq!(first_scope[0].sha, first_sha);
            assert_eq!(locators(&first_scope[0]), ["@#1.1"]);
            let range = fixture.collect("session#1..#2").unwrap();
            assert_eq!(locators(&range[0]), ["@#1.1", "@#2.1"]);
            assert!(range[0].branch.is_none());
            assert!(fixture.collect("session#-1").unwrap()[0].events.is_empty());
            assert_eq!(locators(&fixture.collect("session").unwrap()[0]), ["@#2.1"]);
            assert!(fixture.collect("session#3..#1").is_err());
        }
    }

    #[test]
    fn shallow_boundaries_cannot_assign_inherited_events_to_the_visible_turn() {
        for layout in [LayoutVersion::V0, LayoutVersion::V1] {
            let fixture = Fixture::new(layout);
            let first = event("first", "codex", 'a');
            fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
            let log = format!("{first}{}", event("second", "codex", 'a'));
            let head = fixture.commit(&log, &log, meta::Kind::Turn, Some(2));
            let shallow = fixture.repo.git_path("shallow").unwrap();
            std::fs::write(&shallow, format!("{head}\n")).unwrap();
            assert_eq!(
                fixture
                    .repo
                    .git(&["rev-list", "--parents", "HEAD"])
                    .unwrap(),
                head
            );
            let error = fixture.collect("session").unwrap_err().to_string();
            assert!(error.contains("immutable first-parent ancestry"), "{error}");
            std::fs::remove_file(shallow).unwrap();
            let scope = fixture.collect("session").unwrap();
            assert_eq!(scope[0].sha, head);
            assert_eq!(locators(&scope[0]), ["@#1.1", "@#2.1"]);
        }
    }

    #[test]
    fn grafted_roots_and_skipped_parents_cannot_issue_event_coordinates() {
        let fixture = Fixture::new(LayoutVersion::V1);
        let root = fixture.repo.git(&["rev-parse", "HEAD"]).unwrap();
        let first = event("first", "codex", 'a');
        fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
        let log = format!("{first}{}", event("second", "codex", 'a'));
        let head = fixture.commit(&log, &log, meta::Kind::Turn, Some(2));
        let grafts = fixture.repo.git_path("info/grafts").unwrap();
        std::fs::create_dir_all(grafts.parent().unwrap()).unwrap();
        for graft in [format!("{head}\n"), format!("{head} {root}\n")] {
            std::fs::write(&grafts, graft).unwrap();
            let error = fixture.collect("session").unwrap_err().to_string();
            assert!(error.contains("immutable first-parent ancestry"), "{error}");
        }
        std::fs::remove_file(grafts).unwrap();
        let scope = fixture.collect("session").unwrap();
        assert_eq!(scope[0].sha, head);
        assert_eq!(locators(&scope[0]), ["@#1.1", "@#2.1"]);
    }

    #[test]
    fn immutable_topology_checks_bound_commit_bodies_before_reading_them() {
        let fixture = Fixture::new(LayoutVersion::V1);
        let first = event("first", "codex", 'a');
        let head = fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
        let mut commit = fixture
            .repo
            .git_bytes_result(&["cat-file", "commit", &head])
            .unwrap();
        commit.extend(std::iter::repeat_n(b'x', MAX_INPUT_BYTES));
        let path = fixture._dir.path().join("oversized-commit");
        std::fs::write(&path, commit).unwrap();
        let oversized = fixture
            .repo
            .git(&["hash-object", "-w", "-t", "commit", path.to_str().unwrap()])
            .unwrap();
        fixture
            .repo
            .git(&["update-ref", "refs/heads/session", &oversized])
            .unwrap();
        let error = fixture.collect("session").unwrap_err().to_string();
        assert!(error.contains("commit exceeds the byte limit"), "{error}");
    }

    #[test]
    fn frozen_remedy_coordinates_do_not_requery_mutable_git_topology() {
        let fixture = Fixture::new(LayoutVersion::V1);
        let first = event("first", "codex", 'a');
        fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
        let log = format!("{first}{}", event("second", "codex", 'a'));
        let head = fixture.commit(&log, &log, meta::Kind::Turn, Some(2));
        let proof = freeze_review_log(&fixture.repo, &head).unwrap();
        for relative in ["shallow", "info/grafts"] {
            let path = fixture.repo.git_path(relative).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("{head}\n")).unwrap();
            assert_eq!(fixture.repo.git(&["rev-list", "HEAD"]).unwrap(), head);
            assert!(freeze_review_log(&fixture.repo, &head).is_err());
            assert_eq!(proof.turn_lines(2).unwrap(), (2, vec![1]));
            assert_eq!(proof.turn_lines(refs::LAST_TURN).unwrap(), (2, vec![1]));
            assert_eq!(proof.log, log);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn another_sessions_equal_content_does_not_keep_a_removed_event_selected() {
        for layout in [LayoutVersion::V0, LayoutVersion::V1] {
            let fixture = Fixture::new(layout);
            let local = event("identical payload", "codex", 'a');
            let foreign = event("identical payload", "codex", 'b');
            fixture.commit(
                &format!("{local}{foreign}"),
                &foreign,
                meta::Kind::Turn,
                Some(1),
            );
            let result = fixture.collect("session#1.1").unwrap();
            assert!(result[0].events.is_empty());
            assert_eq!(result[0].missing_events, 0);
        }
    }

    #[test]
    fn markers_and_non_turn_additions_make_the_selected_view_incomplete() {
        for layout in [LayoutVersion::V0, LayoutVersion::V1] {
            let fixture = Fixture::new(layout);
            let first = event("first", "codex", 'a');
            fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
            let merged = event("merged evidence", "codex", 'b');
            let marker = crate::commands::merge::marker_envelope(
                "__merge_start__",
                "codex",
                &claim('a'),
                "source#1",
            );
            let log = format!("{first}{marker}{merged}");
            fixture.commit(&log, &log, meta::Kind::Merge, Some(1));
            let scope = fixture.collect("session").unwrap();
            assert_eq!(locators(&scope[0]), ["@#1.1"]);
            assert_eq!(scope[0].unlocated_events, 2);
            assert_eq!(scope[0].missing_events, 0);
        }
    }

    #[test]
    fn historical_missing_cas_cannot_borrow_the_latest_trees_object() {
        let fixture = Fixture::new(LayoutVersion::V1);
        let first = event("first", "codex", 'a');
        let id = storage::event_id(&first).unwrap();
        fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
        let path = meta::event_path(&id).unwrap();
        std::fs::remove_file(fixture.repo.root().join(&path)).unwrap();
        fixture.repo.add_all().unwrap();
        assert!(
            fixture
                .repo
                .commit("omit synthetic historical CAS object")
                .unwrap()
        );
        let second = event("second", "codex", 'a');
        fixture.commit(
            &format!("{first}{second}"),
            &second,
            meta::Kind::Turn,
            Some(2),
        );
        let error = fixture.collect("session").unwrap_err().to_string();
        assert!(error.contains("readable blob"), "{error}");
    }

    #[test]
    fn rewritten_log_prefix_cannot_issue_stale_coordinates() {
        for layout in [LayoutVersion::V0, LayoutVersion::V1] {
            let fixture = Fixture::new(layout);
            let first = event("first", "codex", 'a');
            fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
            let replacement = event("replacement", "codex", 'a');
            fixture.commit(&replacement, &replacement, meta::Kind::Turn, Some(2));
            let error = fixture.collect("session").unwrap_err().to_string();
            assert!(error.contains("rewrites"), "{error}");
        }
    }

    #[test]
    fn byte_cap_rejects_an_oversized_blob_before_json_materialization() {
        let fixture = Fixture::new(LayoutVersion::V0);
        let oversized = "x".repeat(MAX_INPUT_BYTES + 1);
        fixture.commit(&oversized, &oversized, meta::Kind::Turn, Some(1));
        let error = fixture.collect("session").unwrap_err().to_string();
        assert!(error.contains("byte limit"), "{error}");
    }

    #[test]
    fn total_input_budget_covers_duplicate_targets_and_repeated_occurrences() {
        let fixture = Fixture::new(LayoutVersion::V1);
        let large = event(&"x".repeat(MAX_INPUT_BYTES / 2), "codex", 'a');
        fixture.commit(&large, &large, meta::Kind::Turn, Some(1));
        let target = refs::parse("session").unwrap();
        let error = collect(&fixture.repo, &[target.clone(), target])
            .unwrap_err()
            .to_string();
        assert!(error.contains("input limit"), "{error}");

        let mut budget = ReviewBudget::default();
        for _ in 0..MAX_EVENTS {
            budget.add(0).unwrap();
        }
        assert!(budget.add(0).is_err());
    }

    #[test]
    fn absent_metadata_file_lines_and_paths_are_explicit_errors() {
        let fixture = Fixture::new(LayoutVersion::V1);
        assert!(
            fixture
                .collect("session")
                .unwrap_err()
                .to_string()
                .contains("file line")
        );
        assert!(
            fixture
                .collect("session:AGENTS.md")
                .unwrap_err()
                .to_string()
                .contains("file-path")
        );
        let first = event("first", "codex", 'a');
        fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
        std::fs::remove_file(fixture.repo.root().join(meta::FILE)).unwrap();
        fixture.repo.add_all().unwrap();
        assert!(fixture.repo.commit("omit synthetic metadata").unwrap());
        assert!(
            fixture
                .collect("session")
                .unwrap_err()
                .to_string()
                .contains("readable blob")
        );
    }

    #[test]
    fn view_occurrences_without_log_evidence_cannot_report_complete() {
        let fixture = Fixture::new(LayoutVersion::V0);
        let first = event("first", "codex", 'a');
        fixture.commit(
            &first,
            &format!("{first}{first}"),
            meta::Kind::Turn,
            Some(1),
        );
        let scope = fixture.collect("session").unwrap();
        assert_eq!(locators(&scope[0]), ["@#1.1"]);
        assert_eq!(scope[0].missing_events, 1);
    }

    #[test]
    fn narrow_selectors_cannot_hide_orphan_or_excess_view_occurrences() {
        let logged = event("logged evidence", "codex", 'a');
        let orphan = event("orphan evidence", "codex", 'a');
        for extra in [&orphan, &logged] {
            let fixture = Fixture::new(LayoutVersion::V0);
            let view = format!("{logged}{extra}");
            fixture.commit(&logged, &view, meta::Kind::Turn, Some(1));
            for target in ["session#1", "session#1.1", "session#1..#1"] {
                let scope = fixture.collect(target).unwrap();
                assert_eq!(locators(&scope[0]), ["@#1.1"]);
                assert_eq!(scope[0].missing_events, 1, "{target}");
            }
            fixture.commit(&logged, &view, meta::Kind::Turn, Some(2));
            for target in ["session#2", "session#1..#2"] {
                let scope = fixture.collect(target).unwrap();
                assert_eq!(scope[0].missing_events, 1, "{target}");
            }
        }
    }

    #[test]
    fn legacy_nested_duplicate_keys_cannot_hide_earlier_evidence() {
        let benign = event("benign", "codex", 'a');
        let private = "SYNTHETIC-PRIVATE-PAYLOAD";
        let duplicate = benign.replacen(
            "\"content\":\"benign\"",
            &format!("\"content\":\"{private}\",\"content\":\"benign\""),
            1,
        );
        assert!(duplicate.contains(private));
        for log in [&duplicate, &benign] {
            let fixture = Fixture::new(LayoutVersion::V0);
            fixture.commit(log, &duplicate, meta::Kind::Turn, Some(1));
            for target in ["session", "session#1.1"] {
                let error = format!("{:#}", fixture.collect(target).unwrap_err());
                assert!(error.contains("invalid legacy review envelope"), "{target}");
                assert!(!error.contains(private));
            }
        }
    }

    #[test]
    fn corrupt_stored_text_is_not_copied_into_review_errors() {
        let fixture = Fixture::new(LayoutVersion::V1);
        let first = event("first", "codex", 'a');
        fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
        let private = "SYNTHETIC-PRIVATE-PAYLOAD";
        std::fs::write(
            fixture.repo.root().join(meta::LOG_FILE),
            format!("{private}\n"),
        )
        .unwrap();
        fixture.repo.add_all().unwrap();
        assert!(
            fixture
                .repo
                .commit("record invalid synthetic sequence")
                .unwrap()
        );
        let error = format!("{:#}", fixture.collect("session").unwrap_err());
        assert!(error.contains("invalid review event sequence"));
        assert!(!error.contains(private));
    }

    #[test]
    fn reserved_turn_ordinals_never_become_runnable_event_coordinates() {
        let fixture = Fixture::new(LayoutVersion::V1);
        let zero = event("zero ordinal", "codex", 'a');
        fixture.commit(&zero, &zero, meta::Kind::Turn, Some(0));
        let sentinel = event("reserved ordinal", "codex", 'a');
        let log = format!("{zero}{sentinel}");
        fixture.commit(&log, &log, meta::Kind::Turn, Some(refs::LAST_TURN));
        let ordinary = event("ordinary ordinal", "codex", 'a');
        let log = format!("{log}{ordinary}");
        fixture.commit(&log, &log, meta::Kind::Turn, Some(1));
        let scope = fixture.collect("session").unwrap();
        assert_eq!(locators(&scope[0]), ["@#1.1"]);
        assert_eq!(scope[0].unlocated_events, 2);
    }

    #[test]
    fn range_endpoints_cannot_silently_omit_an_earlier_requested_ordinal() {
        let fixture = Fixture::new(LayoutVersion::V1);
        let first = event("settled as two", "codex", 'a');
        fixture.commit(&first, &first, meta::Kind::Turn, Some(2));
        let second = event("settled as one", "codex", 'a');
        let log = format!("{first}{second}");
        fixture.commit(&log, &log, meta::Kind::Turn, Some(1));
        let error = fixture.collect("session#1..#2").unwrap_err().to_string();
        assert!(error.contains("history order"), "{error}");
    }

    #[test]
    fn last_turn_cannot_silently_select_a_duplicate_settlement() {
        let fixture = Fixture::new(LayoutVersion::V1);
        let first = event("first settlement", "codex", 'a');
        fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
        let duplicate = event("duplicate settlement", "codex", 'a');
        fixture.commit(
            &format!("{first}{duplicate}"),
            &duplicate,
            meta::Kind::Turn,
            Some(1),
        );
        for target in ["session#-1", "session#-1.1", "session#1..#-1"] {
            let error = fixture.collect(target).unwrap_err().to_string();
            assert!(error.contains("duplicate settlements"), "{target}: {error}");
        }
        let scope = fixture.collect("session").unwrap();
        assert!(scope[0].events.is_empty());
        assert_eq!(scope[0].unlocated_events, 1);
    }

    #[test]
    fn an_unclaimed_turn_cannot_report_a_complete_empty_selection() {
        let fixture = Fixture::new(LayoutVersion::V1);
        let first = event("unclaimed evidence", "codex", 'a');
        let mut snapshot = meta::Meta::new_session_line("codex".into(), "/synthetic".into());
        snapshot.kind = meta::Kind::Turn;
        snapshot.turn = Some(1);
        meta::write(fixture.repo.root(), &snapshot).unwrap();
        storage::write_snapshot(fixture.repo.root(), &first, &first).unwrap();
        fixture.repo.add_all().unwrap();
        assert!(
            fixture
                .repo
                .commit("record synthetic unclaimed turn")
                .unwrap()
        );
        let error = fixture.collect("session#1").unwrap_err().to_string();
        assert!(error.contains("claimed session"), "{error}");
        let whole = fixture.collect("session").unwrap();
        assert_eq!(whole[0].missing_events, 1);
    }

    #[test]
    fn every_range_member_must_exist_inside_the_selected_history_interval() {
        let fixture = Fixture::new(LayoutVersion::V1);
        let first = event("turn one", "codex", 'a');
        fixture.commit(&first, &first, meta::Kind::Turn, Some(1));
        let third = event("turn three", "codex", 'a');
        let log = format!("{first}{third}");
        fixture.commit(&log, &log, meta::Kind::Turn, Some(3));
        assert!(fixture.collect("session#1..#3").is_err());
        let second = event("turn two", "codex", 'a');
        let log = format!("{log}{second}");
        fixture.commit(&log, &log, meta::Kind::Turn, Some(2));
        let error = fixture.collect("session#1..#3").unwrap_err().to_string();
        assert!(error.contains("history interval"), "{error}");
    }
}
