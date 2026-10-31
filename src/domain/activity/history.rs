//! Candidate contributions: the selected history, plain Git attribution and accepted publications.

use super::extract::{Extractor, Meta, parse_meta};
use super::{Key, Reason, Selection, invalid, reported};
use crate::domain::meta;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

pub(super) const FACTS_LIMIT: usize = 256;
pub(super) const MAX_COVERAGE: usize = 1 << 18;
const MAX_WALK: usize = 1 << 20;
const MAX_PLAIN: usize = 1 << 18;
const WALK_TIME: Duration = Duration::from_secs(15);
const HEADER_FORMAT: &str = "--format=%H%x00%P%x00%ct%x00%an%x00%ae";
/// Session metadata for later save times is read together with the current one; a history whose
/// commits all have distinct save times would otherwise start one read per commit.
const PREFETCH: usize = 256;

/// One `(commit, path)` read.
pub enum Blob {
    Missing,
    /// Larger than the requested maximum; no body was read.
    Large,
    Body(Vec<u8>),
}

/// Git access under the caller's process supervision. Reads never fetch.
pub trait Store {
    /// Branch tips as full ref names and commit IDs.
    fn refs(&mut self) -> Result<Vec<(String, String)>>;
    /// `rev-list` arguments that start from every branch tip.
    fn branches(&self) -> &'static [&'static str];
    /// Stream `rev-list` output lines until `visit` returns false.
    fn rev_list(
        &mut self,
        args: &[&str],
        visit: &mut dyn FnMut(&str) -> Result<bool>,
    ) -> Result<()>;
    /// Blob IDs and sizes at `(commit, path)`; `None` when the path is absent. Resolving a path
    /// walks its trees, so callers that also need the body read it by ID with [`Store::blobs`].
    fn locate(&mut self, objects: &[(&str, &str)]) -> Result<Vec<Option<(String, u64)>>>;
    /// Blob bodies at `(commit, path)`, in request order.
    fn read(&mut self, objects: &[(&str, &str)], maximum: usize) -> Result<Vec<Blob>>;
    /// Blob bodies by object ID, in request order.
    fn blobs(&mut self, ids: &[&str], maximum: usize) -> Result<Vec<Blob>>;
}

/// Hub facts that projected Git headers cannot carry.
pub trait Attribution {
    /// Logical and public identities related to the given session IDs.
    fn aliases(&mut self, sessions: &[String]) -> Result<Vec<Alias>>;
    /// The author's account and accepted publications in the requested window.
    fn facts(&mut self, request: &FactsRequest) -> Result<Facts>;
    /// Projected commits among these that lack accepted attribution.
    fn missing(&mut self, commits: &[String]) -> Result<Reason>;
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    pub logical: String,
    pub public: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub id: String,
    pub username: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FactsRequest {
    pub author: String,
    /// The query window; inventory counts cover all of it.
    pub since: String,
    pub before: String,
    /// Inclusive lower bound on acceptance time for the listed publications.
    pub from: String,
    /// Logical or public session IDs; empty selects every session.
    pub sessions: Vec<String>,
    pub limit: usize,
    /// Also count accepted publications in the window that no account can claim.
    pub inventory: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Facts {
    pub account: Option<Account>,
    /// The author's accepted publications ordered by acceptance time, then commit.
    pub publications: Vec<Publication>,
    /// Every publication accepted before this instant is listed; `None` when all are. One
    /// acceptance time is never split, so a batch publication orders as a whole.
    pub horizon: Option<String>,
    #[serde(default)]
    pub unmapped: Reason,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    pub commit: String,
    /// Logical session ID.
    pub session: String,
    pub public_session: String,
    pub source_commit: String,
    pub account_id: String,
    pub username: Option<String>,
    pub published_at: String,
}

/// The local route reaches Hub attribution through one request shape.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AttributionRequest {
    Aliases(Vec<String>),
    Facts(FactsRequest),
    Missing(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AttributionBody {
    Aliases(Vec<Alias>),
    Facts(Facts),
    Missing(Reason),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttributionReply {
    pub schema_version: u32,
    pub repository_id: String,
    pub body: AttributionBody,
}

pub(super) struct Scope {
    /// Walk roots; `None` starts from every branch.
    roots: Option<Vec<String>>,
    /// Logical and public IDs of the selected sessions; empty selects every session.
    pub sessions: BTreeSet<String>,
}

impl Scope {
    fn selects(&self, ids: &[&str]) -> bool {
        self.sessions.is_empty() || ids.iter().any(|id| self.sessions.contains(*id))
    }

    /// Selected IDs the Hub can know; publications and aliases always use exact session IDs.
    pub(super) fn exact(&self) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|id| meta::is_bare_id(id))
            .cloned()
            .collect()
    }
}

fn branch(name: &str) -> Option<&str> {
    name.strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/remotes/origin/"))
}

/// A branch name selects its tips' sessions; a session ID selects every branch whose tip saves
/// that session under either identity. Encrypted sessions publish under a public ID, so both
/// identities of each selected session are kept.
pub(super) fn scope(
    store: &mut impl Store,
    attribution: &mut impl Attribution,
    selection: &Selection,
) -> Result<Scope> {
    let Some(selected) = selection.session.as_deref() else {
        return Ok(Scope {
            roots: None,
            sessions: BTreeSet::new(),
        });
    };
    let bare = meta::is_bare_id(selected);
    let refs: Vec<_> = store
        .refs()?
        .into_iter()
        .filter(|(name, _)| bare || branch(name) == Some(selected))
        .collect();
    let mut tips: Vec<&str> = refs.iter().map(|(_, oid)| oid.as_str()).collect();
    tips.sort_unstable();
    tips.dedup();
    let objects: Vec<_> = tips.iter().map(|tip| (*tip, meta::FILE)).collect();
    let mut tip_sessions = HashMap::new();
    for (tip, blob) in tips
        .iter()
        .zip(store.read(&objects, crate::domain::meta::MAX_BYTES)?)
    {
        if let Blob::Body(raw) = blob
            && let Ok(metadata) = parse_meta(&raw, tip)
        {
            tip_sessions.insert(*tip, metadata.session);
        }
    }
    let mut sessions: BTreeSet<String> = if bare {
        BTreeSet::from([selected.to_owned()])
    } else {
        tip_sessions.values().cloned().collect()
    };
    let ids: Vec<_> = sessions
        .iter()
        .filter(|id| meta::is_bare_id(id))
        .cloned()
        .collect();
    if !ids.is_empty() {
        for alias in attribution.aliases(&ids)? {
            sessions.insert(alias.logical);
            sessions.insert(alias.public);
        }
    }
    let mut roots: Vec<String> = refs
        .iter()
        .filter(|(_, oid)| {
            tip_sessions
                .get(oid.as_str())
                .is_some_and(|session| sessions.contains(session))
        })
        .map(|(_, oid)| oid.clone())
        .collect();
    roots.sort_unstable();
    roots.dedup();
    if roots.is_empty() {
        return Err(invalid(
            "session does not match an existing branch or session ID",
        ));
    }
    Ok(Scope {
        roots: Some(roots),
        sessions,
    })
}

/// Acceptance times bound keyset pagination; a reply outside its request would skip work.
pub(super) fn validate(
    facts: &Facts,
    floor: DateTime<Utc>,
    horizon: Option<DateTime<Utc>>,
    before: DateTime<Utc>,
) -> Result<Vec<(DateTime<Utc>, Publication)>> {
    ensure!(
        horizon.is_none_or(|horizon| horizon > floor && horizon <= before),
        "activity attribution horizon does not advance"
    );
    let mut previous: Option<(DateTime<Utc>, &str)> = None;
    let mut result = Vec::with_capacity(facts.publications.len());
    for publication in &facts.publications {
        let at = reported(&publication.published_at)?;
        ensure!(
            at >= floor && at < before && horizon.is_none_or(|horizon| at < horizon),
            "activity attribution is outside the requested window"
        );
        ensure!(
            previous.is_none_or(|previous| previous < (at, publication.commit.as_str())),
            "activity attribution is not ordered"
        );
        ensure!(
            facts
                .account
                .as_ref()
                .is_some_and(|account| account.id == publication.account_id),
            "activity attribution names another account"
        );
        previous = Some((at, &publication.commit));
        result.push((at, publication.clone()));
    }
    Ok(result)
}

/// Plain commits match by Git author name or email, by the resolved account's username, or by
/// the account recorded in session metadata.
pub(super) struct Who<'a> {
    author: &'a str,
    account: Option<&'a Account>,
    username: Option<String>,
}

impl<'a> Who<'a> {
    pub(super) fn new(author: &'a str, account: Option<&'a Account>) -> Self {
        Self {
            author,
            account,
            username: account.map(|account| account.username.to_lowercase()),
        }
    }

    fn signs(&self, name: &str, email: &str) -> bool {
        let name = name.to_lowercase();
        name == self.author
            || email.to_lowercase() == self.author
            || self.username.as_deref() == Some(name.as_str())
    }

    fn owns(&self, account_id: Option<&str>) -> bool {
        account_id.is_some_and(|id| {
            id.to_lowercase() == self.author || self.account.is_some_and(|account| account.id == id)
        })
    }
}

pub(super) struct Header {
    commit: String,
    parent: Option<String>,
    at: DateTime<Utc>,
    name: String,
    email: String,
}

fn header(line: &str) -> Result<Header> {
    let mut fields = line.split('\0');
    let mut next = || fields.next().context("invalid activity history record");
    let (commit, parents, time, name, email) = (next()?, next()?, next()?, next()?, next()?);
    ensure!(
        commit.len() >= 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid activity history record"
    );
    Ok(Header {
        commit: commit.into(),
        parent: parents.split_whitespace().next().map(str::to_owned),
        at: DateTime::from_timestamp(time.parse()?, 0).context("invalid commit time")?,
        name: name.into(),
        email: email.into(),
    })
}

pub(super) struct Walked {
    /// Plain commits saved from the resume position on, and before the bound if one is given, of
    /// every author.
    plain: Vec<Header>,
    /// Listed publications reachable from the scope, with their first parents.
    published: HashMap<String, Option<String>>,
    /// Source commits of listed publications that are present as originals.
    originals: HashSet<String>,
    /// Every projected commit in scope, collected for the first page's attribution coverage.
    pub projected: Vec<String>,
    pub author_seen: bool,
    pub truncated: bool,
}

/// Accepted publication IDs and envelope paths identify projections independently of Git authors.
/// Envelope paths are located in batches; session metadata is read only for commits a page reaches.
pub(super) fn walk(
    store: &mut impl Store,
    scope: &Scope,
    publications: &[(DateTime<Utc>, Publication)],
    who: &Who,
    floor: DateTime<Utc>,
    (since, before): (DateTime<Utc>, Option<DateTime<Utc>>),
    coverage: bool,
) -> Result<Walked> {
    let wanted: HashSet<&str> = publications
        .iter()
        .map(|(_, p)| p.commit.as_str())
        .collect();
    let sources: HashSet<&str> = publications
        .iter()
        .map(|(_, p)| p.source_commit.as_str())
        .collect();
    let mut walked = Walked {
        plain: Vec::new(),
        published: HashMap::new(),
        originals: HashSet::new(),
        projected: Vec::new(),
        author_seen: false,
        truncated: false,
    };
    let mut args = vec!["rev-list", HEADER_FORMAT];
    match &scope.roots {
        Some(roots) => args.extend(roots.iter().map(String::as_str)),
        None => args.extend(store.branches()),
    }
    args.push("--");
    let started = Instant::now();
    let mut seen = 0;
    let mut unclassified = Vec::new();
    store.rev_list(&args, &mut |line| {
        if line.is_empty() || line.starts_with("commit ") {
            return Ok(true);
        }
        seen += 1;
        if seen > MAX_WALK || walked.plain.len() >= MAX_PLAIN || started.elapsed() > WALK_TIME {
            walked.truncated = true;
            return Ok(false);
        }
        let header = header(line)?;
        if wanted.contains(header.commit.as_str()) {
            walked
                .published
                .insert(header.commit.clone(), header.parent);
            if coverage && walked.projected.len() <= MAX_COVERAGE {
                walked.projected.push(header.commit);
            }
            return Ok(true);
        }
        unclassified.push((
            header.commit.clone(),
            who.signs(&header.name, &header.email),
        ));
        if header.at >= floor.max(since) && before.is_none_or(|before| header.at < before) {
            walked.plain.push(header);
        }
        Ok(true)
    })?;
    let mut plain = HashSet::new();
    for batch in unclassified.chunks(PREFETCH) {
        if started.elapsed() > WALK_TIME {
            walked.truncated = true;
            break;
        }
        let paths: Vec<_> = batch
            .iter()
            .map(|(commit, _)| (commit.as_str(), "privacy/envelope.json"))
            .collect();
        let envelopes = store.locate(&paths)?;
        ensure!(
            envelopes.len() == batch.len(),
            "activity projection inventory is incomplete"
        );
        for ((commit, signed), envelope) in batch.iter().zip(envelopes) {
            if envelope.is_some() {
                if coverage && walked.projected.len() <= MAX_COVERAGE {
                    walked.projected.push(commit.clone());
                }
            } else {
                walked.author_seen |= signed;
                if sources.contains(commit.as_str()) {
                    walked.originals.insert(commit.clone());
                }
                plain.insert(commit.as_str());
            }
        }
    }
    walked
        .plain
        .retain(|header| plain.contains(header.commit.as_str()));
    Ok(walked)
}

pub(super) enum Entry {
    Plain(Header),
    Published {
        at: DateTime<Utc>,
        parent: Option<String>,
        publication: Publication,
    },
}

impl Entry {
    fn commit(&self) -> &str {
        match self {
            Self::Plain(header) => &header.commit,
            Self::Published { publication, .. } => &publication.commit,
        }
    }
}

pub(super) type Group = (DateTime<Utc>, Vec<Entry>);

/// Entries grouped by save time, oldest first; a group is complete before any of it is ordered.
pub(super) fn groups(
    walked: Walked,
    publications: Vec<(DateTime<Utc>, Publication)>,
) -> Vec<Group> {
    let mut groups: BTreeMap<_, Vec<Entry>> = BTreeMap::new();
    for header in walked.plain {
        groups
            .entry(header.at)
            .or_default()
            .push(Entry::Plain(header));
    }
    for (at, publication) in publications {
        // Unreachable publications are outside the selected branches.
        let Some(parent) = walked.published.get(&publication.commit) else {
            continue;
        };
        // A present original is the same contribution with its own committer time.
        if walked.originals.contains(&publication.source_commit) {
            continue;
        }
        groups.entry(at).or_default().push(Entry::Published {
            at,
            parent: parent.clone(),
            publication,
        });
    }
    groups.into_iter().collect()
}

/// A contribution, or a commit that could be one but cannot be read. Both take a place in
/// contribution order so that a continuation reports each unreadable commit once.
pub(super) enum Item {
    Read(Candidate),
    Skipped { key: Key, reason: &'static str },
}

impl Item {
    pub(super) fn key(&self) -> &Key {
        match self {
            Self::Read(candidate) => &candidate.key,
            Self::Skipped { key, .. } => key,
        }
    }
}

pub(super) struct Candidate {
    pub key: Key,
    pub parent: Option<String>,
    pub meta: Meta,
    pub published: bool,
    pub account_id: Option<String>,
    pub name: String,
    pub email: String,
}

/// Resolve the first of `upcoming` into the author's contributions, in contribution order.
pub(super) fn candidates<S: Store>(
    extractor: &mut Extractor<'_, S>,
    upcoming: &mut [Group],
    scope: &Scope,
    who: &Who,
) -> Result<Vec<Item>> {
    let current = upcoming[0].1.len();
    if upcoming[0]
        .1
        .iter()
        .any(|e| !extractor.has_meta(e.commit()))
    {
        let commits: Vec<&str> = upcoming
            .iter()
            .flat_map(|(_, entries)| entries.iter().map(Entry::commit))
            .take(PREFETCH.max(current))
            .collect();
        extractor.metas_ahead(&commits, current)?;
    }
    let entries = std::mem::take(&mut upcoming[0].1);
    let mut result = Vec::new();
    for entry in entries {
        let metadata = extractor.meta(entry.commit());
        match entry {
            Entry::Plain(header) => {
                let signed = who.signs(&header.name, &header.email);
                let metadata = match metadata {
                    Ok(Some(metadata)) => metadata,
                    Ok(None) => continue,
                    Err(_) => {
                        if signed {
                            result.push(Item::Skipped {
                                key: Key {
                                    at: header.at,
                                    session: String::new(),
                                    turn: None,
                                    commit: header.commit,
                                },
                                reason: "unreadable_commit",
                            });
                        }
                        continue;
                    }
                };
                if metadata.session.is_empty()
                    || !metadata.contribution
                    || !scope.selects(&[&metadata.session])
                    || !(signed || who.owns(metadata.account_id.as_deref()))
                {
                    continue;
                }
                result.push(Item::Read(Candidate {
                    key: Key {
                        at: header.at,
                        session: metadata.session.clone(),
                        turn: metadata.turn,
                        commit: header.commit,
                    },
                    parent: header.parent,
                    published: false,
                    account_id: metadata.account_id.clone(),
                    name: header.name,
                    email: header.email,
                    meta: metadata,
                }));
            }
            Entry::Published {
                at,
                parent,
                publication,
            } => {
                let reason = match metadata {
                    Ok(Some(metadata)) if metadata.session == publication.public_session => {
                        Ok(metadata)
                    }
                    Ok(Some(_)) => Err("publication_mismatch"),
                    Ok(None) | Err(_) => Err("unreadable_commit"),
                };
                let metadata = match reason {
                    Ok(metadata) => metadata,
                    Err(reason) => {
                        result.push(Item::Skipped {
                            key: Key {
                                at,
                                session: publication.session,
                                turn: None,
                                commit: publication.commit,
                            },
                            reason,
                        });
                        continue;
                    }
                };
                if !metadata.contribution
                    || !scope.selects(&[&publication.session, &publication.public_session])
                {
                    continue;
                }
                result.push(Item::Read(Candidate {
                    key: Key {
                        at,
                        session: publication.session,
                        turn: metadata.turn,
                        commit: publication.commit,
                    },
                    parent,
                    published: true,
                    account_id: Some(publication.account_id),
                    name: publication.username.unwrap_or_default(),
                    email: String::new(),
                    meta: metadata,
                }));
            }
        }
    }
    result.sort_by(|a, b| a.key().cmp(b.key()));
    Ok(result)
}
