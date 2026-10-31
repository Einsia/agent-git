//! Saved conversation contributions share selection, extraction and pagination across local and
//! Hub readers.
//!
//! Messages are selected by the time their harness recorded them, falling back to save time.
//! Nothing is saved before it is sent, so only saves from `since` on are read; a save after
//! `before` can still carry messages sent inside the window.
//!
//! Contributions are ordered by save time, session, turn and commit. A continuation resumes after
//! the last emitted position, so a save that lands later in that order extends the result instead
//! of invalidating the cursor.

mod extract;
mod history;
#[cfg(feature = "cli")]
pub(crate) mod local;
#[cfg(all(test, feature = "cli"))]
pub(crate) mod tests;

pub use history::{
    Account, Alias, Attribution, AttributionBody, AttributionReply, AttributionRequest, Blob,
    Facts, FactsRequest, Publication, Store,
};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

pub const SCHEMA_VERSION: u32 = 4;
/// The contract a Hub answers in when a client names none, as every Hub that predates
/// [`SCHEMA_HEADER`] does: messages selected by save time, without send times.
pub const SAVE_TIME_SCHEMA: u32 = 3;
/// The request header in which a client names the contract it reads. It cannot be a query field:
/// a Hub refuses a query field it does not know, while it ignores a header.
pub const SCHEMA_HEADER: &str = "x-agentgit-activity-schema";
/// Message chunks in a page when the caller names no limit.
pub const DEFAULT_LIMIT: usize = 50;
/// The most message chunks one page may hold; every surface that takes a limit offers this range.
pub const MAX_LIMIT: usize = 200;
const MAX_PAGE_TEXT: usize = 256 * 1024;
const MAX_PAGE_READ: usize = 128 * 1024 * 1024;
const PAGE_TIME: Duration = Duration::from_secs(20);
const MAX_CURSOR: usize = 4096;
const EXAMPLES: usize = 5;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub author: String,
    pub since: String,
    pub before: String,
    pub session: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    #[serde(default)]
    pub assistant: Assistant,
    #[serde(default, skip_serializing_if = "SelectBy::is_sent")]
    pub select_by: SelectBy,
}

/// Which time `since` and `before` bound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectBy {
    /// Each message's `sent_at`.
    #[default]
    Sent,
    /// Each contribution's `saved_at`, for a reader that chooses commits by when they were saved:
    /// under `Sent`, messages written before their save fall outside a window drawn around it.
    Saved,
}

impl SelectBy {
    fn is_sent(&self) -> bool {
        *self == SelectBy::Sent
    }
}

/// Which assistant messages a page carries; user messages are always included.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[serde(rename_all = "snake_case")]
pub enum Assistant {
    #[default]
    All,
    /// Only the last assistant message before each user message or the end of a contribution:
    /// the answer, without the commentary that precedes it.
    Last,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Selection {
    pub author: String,
    pub since: String,
    pub before: String,
    pub session: Option<String>,
    #[serde(default)]
    pub assistant: Assistant,
    #[serde(default, skip_serializing_if = "SelectBy::is_sent")]
    pub select_by: SelectBy,
}

#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    #[error("{0}")]
    Invalid(String),
}

fn invalid(message: &str) -> anyhow::Error {
    RequestError::Invalid(message.into()).into()
}

fn instant(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|v| v.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc())
        })
        .map_err(|_| invalid("time filters require YYYY-MM-DD (UTC) or RFC3339 with a timezone"))
}

impl Query {
    pub fn selection(&self) -> Result<Selection> {
        let author = self.author.trim().to_lowercase();
        if author.is_empty() || author.len() > 512 || author.chars().any(char::is_control) {
            return Err(invalid(
                "author must be a name or email without control characters (at most 512 bytes)",
            ));
        }
        let (since, before) = (instant(&self.since)?, instant(&self.before)?);
        if since >= before {
            return Err(invalid("since must be earlier than before"));
        }
        if self.limit.is_some_and(|n| !(1..=MAX_LIMIT).contains(&n)) {
            return Err(invalid("limit must be between 1 and 200"));
        }
        if self
            .session
            .as_deref()
            .is_some_and(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(invalid(
                "session must be a branch name or exact AgentGit session ID",
            ));
        }
        Ok(Selection {
            author,
            since: since.to_rfc3339_opts(SecondsFormat::AutoSi, true),
            before: before.to_rfc3339_opts(SecondsFormat::AutoSi, true),
            session: self.session.clone(),
            assistant: self.assistant,
            select_by: self.select_by,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Page {
    pub schema_version: u32,
    pub repo: String,
    pub applied_filters: Selection,
    pub contributions: Vec<Contribution>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    /// Set when any page of this query, including earlier ones, could not read everything.
    pub incomplete: bool,
    pub reasons: BTreeMap<String, Reason>,
    pub warnings: BTreeSet<String>,
}

/// `count` covers the whole query so far; `examples` come from the page that found them.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reason {
    pub count: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
}

impl Reason {
    fn note(&mut self, example: &str) {
        self.count += 1;
        if self.examples.len() < EXAMPLES {
            self.examples.push(example.into());
        }
    }

    /// Merge a count reported in parts.
    pub fn absorb(&mut self, other: Reason) {
        self.count += other.count;
        let room = EXAMPLES.saturating_sub(self.examples.len());
        self.examples.extend(other.examples.into_iter().take(room));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeBasis {
    /// Git committer time of an original or plain commit.
    CommittedAt,
    /// Hub acceptance time of an encrypted publication.
    PublishedAt,
}

/// One commit's newly saved messages; a long commit continues in the next page's first entry.
#[derive(Debug, Deserialize, Serialize)]
pub struct Contribution {
    pub session: String,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    pub saved_at: String,
    pub time_basis: TimeBasis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_account_id: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author_email: String,
    pub messages: Vec<Message>,
}

/// Where a message's `sent_at` comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SentBasis {
    /// The time the harness wrote into the message's record.
    Recorded,
    /// The record has no time; its contribution's `saved_at` stands in.
    Saved,
}

/// A chunk of a long message carries its byte offset; the last chunk is `text_complete`.
#[derive(Debug, Deserialize, Serialize)]
pub struct Message {
    pub role: String,
    pub line: usize,
    pub sent_at: String,
    pub sent_basis: SentBasis,
    pub text: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub text_offset: usize,
    #[serde(default = "complete", skip_serializing_if = "is_complete")]
    pub text_complete: bool,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn complete() -> bool {
    true
}

fn is_complete(value: &bool) -> bool {
    *value
}

/// Contribution order: save time, then session and turn so that one batch keeps conversation
/// order, then commit.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Key {
    at: DateTime<Utc>,
    session: String,
    turn: Option<u32>,
    commit: String,
}

/// Everything ordered before the resume position has been emitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Resume {
    /// Contributions saved at or after this instant.
    From(DateTime<Utc>),
    /// Contributions ordered after this one.
    Past(Key),
    /// This contribution from a record window, message and byte offset.
    Inside {
        key: Key,
        record: usize,
        message: usize,
        text: usize,
    },
}

impl Resume {
    fn admits(&self, key: &Key) -> bool {
        match self {
            Self::From(at) => key.at >= *at,
            Self::Past(past) => key > past,
            Self::Inside { key: inside, .. } => key >= inside,
        }
    }

    /// Inclusive lower bound on save time.
    fn floor(&self) -> DateTime<Utc> {
        match self {
            Self::From(at) => *at,
            Self::Past(key) | Self::Inside { key, .. } => key.at,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    query: String,
    resume: Resume,
    #[serde(default)]
    reasons: BTreeMap<String, u64>,
}

fn decode(raw: &str, binding: &str) -> Result<Cursor> {
    if raw.len() > MAX_CURSOR {
        return Err(invalid("invalid activity cursor"));
    }
    let bytes = hex::decode(raw).map_err(|_| invalid("invalid activity cursor"))?;
    let cursor: Cursor =
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid activity cursor"))?;
    if cursor.query != binding {
        return Err(invalid(
            "activity cursor belongs to another repository, route or filter; restart without it",
        ));
    }
    Ok(cursor)
}

#[derive(Default)]
struct Output {
    contributions: Vec<Contribution>,
    messages: usize,
    bytes: usize,
}

impl Output {
    fn full(&self, limit: usize) -> bool {
        self.messages >= limit || self.bytes >= MAX_PAGE_TEXT
    }

    fn push(&mut self, candidate: &history::Candidate, message: Message) {
        if self
            .contributions
            .last()
            .is_none_or(|last| last.commit != candidate.key.commit)
        {
            let at = candidate
                .key
                .at
                .to_rfc3339_opts(SecondsFormat::AutoSi, true);
            self.contributions.push(Contribution {
                session: candidate.key.session.clone(),
                commit: candidate.key.commit.clone(),
                turn: candidate.key.turn,
                saved_at: at,
                time_basis: if candidate.published {
                    TimeBasis::PublishedAt
                } else {
                    TimeBasis::CommittedAt
                },
                author_account_id: candidate.account_id.clone(),
                author_name: candidate.name.clone(),
                author_email: candidate.email.clone(),
                messages: Vec::new(),
            });
        }
        self.messages += 1;
        self.bytes += message.text.len();
        self.contributions
            .last_mut()
            .unwrap()
            .messages
            .push(message);
    }
}

/// Why a page stopped before the end of the selected history.
enum Stop {
    Full,
    Budget,
    Horizon(DateTime<Utc>),
}

/// Read one page. `identity` names the repository and route so a cursor cannot cross them.
pub fn page(
    store: &mut impl Store,
    attribution: &mut impl Attribution,
    repo: &str,
    identity: &str,
    query: &Query,
) -> Result<Page> {
    let selection = query.selection()?;
    let (since, before) = (instant(&selection.since)?, instant(&selection.before)?);
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    let started = Instant::now();
    let binding = hex::encode(
        &Sha256::digest(serde_json::to_vec(&(SCHEMA_VERSION, identity, &selection))?)[..16],
    );
    let cursor = query
        .cursor
        .as_deref()
        .map(|raw| decode(raw, &binding))
        .transpose()?;
    let first = cursor.is_none();
    let resume = cursor.as_ref().map(|c| c.resume.clone());
    let mut reasons: BTreeMap<String, Reason> = cursor
        .map(|c| c.reasons)
        .unwrap_or_default()
        .into_iter()
        .map(|(name, count)| {
            (
                name,
                Reason {
                    count,
                    examples: Vec::new(),
                },
            )
        })
        .collect();

    let scope = history::scope(store, attribution, &selection)?;
    let floor = resume.as_ref().map_or(since, Resume::floor);
    let facts = attribution.facts(&FactsRequest {
        author: selection.author.clone(),
        since: selection.since.clone(),
        from: floor.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        before: selection.before.clone(),
        sessions: scope.exact(),
        limit: history::FACTS_LIMIT,
        inventory: first,
    })?;
    let horizon = facts.horizon.as_deref().map(reported).transpose()?;
    let publications = history::validate(&facts, floor, horizon, before)?;
    if first && facts.unmapped.count > 0 {
        reasons.insert("unmapped_publication_author".into(), facts.unmapped.clone());
    }
    let who = history::Who::new(&selection.author, facts.account.as_ref());
    let saved_before = (selection.select_by == SelectBy::Saved).then_some(before);
    let walked = history::walk(
        store,
        &scope,
        &publications,
        &who,
        floor,
        (since, saved_before),
        first,
    )?;
    if walked.truncated {
        let truncated = reasons.entry("history_truncated".into()).or_default();
        truncated.count = truncated.count.max(1);
    }
    if first && !walked.projected.is_empty() {
        let missing = if walked.projected.len() > history::MAX_COVERAGE {
            (
                "attribution_unverified",
                Reason {
                    count: walked.projected.len() as u64,
                    examples: Vec::new(),
                },
            )
        } else {
            (
                "missing_publication_metadata",
                attribution.missing(&walked.projected)?,
            )
        };
        if missing.1.count > 0 {
            reasons.insert(missing.0.into(), missing.1);
        }
    }
    let mut warnings = BTreeSet::new();
    if facts.account.is_none() && !walked.author_seen {
        warnings.insert("author_not_found".into());
    }

    let mut groups = history::groups(walked, publications);
    let mut extractor = extract::Extractor::new(store, MAX_PAGE_READ);
    let mut output = Output::default();
    let mut position = resume.clone();
    let mut entered = !matches!(resume, Some(Resume::Inside { .. }));
    let mut stop = None;
    'groups: for index in 0..groups.len() {
        let at = groups[index].0;
        // Publications from the horizon on are not listed yet; a group there would be partial.
        if let Some(horizon) = horizon
            && at >= horizon
        {
            stop = Some(Stop::Horizon(horizon));
            break;
        }
        // Every earlier group is finished, so a page can end here even if nothing matched yet.
        if index > 0 {
            position = Some(Resume::From(at));
            if started.elapsed() > PAGE_TIME {
                stop = Some(Stop::Budget);
                break;
            }
        }
        let candidates =
            match history::candidates(&mut extractor, &mut groups[index..], &scope, &who) {
                Ok(candidates) => candidates,
                Err(error) if error.is::<extract::Exhausted>() && position != resume => {
                    stop = Some(Stop::Budget);
                    break;
                }
                Err(error) => return Err(error),
            };
        for item in candidates {
            if resume.as_ref().is_some_and(|r| !r.admits(item.key())) {
                continue;
            }
            let resumed = match &resume {
                Some(Resume::Inside {
                    key,
                    record,
                    message,
                    text,
                }) if !entered && key == item.key() => Some((*record, *message, *text)),
                _ => None,
            };
            if !entered {
                entered = true;
                if resumed.is_none() || matches!(item, history::Item::Skipped { .. }) {
                    changed(&mut reasons, &resume);
                }
            }
            let candidate = match item {
                history::Item::Read(candidate) => candidate,
                history::Item::Skipped { key, reason } => {
                    reasons.entry(reason.into()).or_default().note(&key.commit);
                    position = Some(Resume::Past(key));
                    continue;
                }
            };
            let (mut record, mut message, mut text) = resumed.unwrap_or_default();
            loop {
                let inside = Resume::Inside {
                    key: candidate.key.clone(),
                    record,
                    message,
                    text,
                };
                let here = if (record, message, text) == (0, 0, 0) {
                    position.clone()
                } else {
                    Some(inside)
                };
                if output.full(limit) {
                    position = here;
                    stop = Some(Stop::Full);
                    break 'groups;
                }
                if started.elapsed() > PAGE_TIME && here != resume {
                    position = here;
                    stop = Some(Stop::Budget);
                    break 'groups;
                }
                let window = match extractor.window(
                    &candidate,
                    record,
                    selection.assistant,
                    (since, before, selection.select_by),
                ) {
                    Ok(window) => window,
                    Err(error) if error.is::<extract::Exhausted>() => {
                        ensure!(
                            here != resume,
                            "activity read budget exhausted before making progress"
                        );
                        position = here;
                        stop = Some(Stop::Budget);
                        break 'groups;
                    }
                    Err(_) => {
                        reasons
                            .entry("unreadable_commit".into())
                            .or_default()
                            .note(&candidate.key.commit);
                        break;
                    }
                };
                if message > window.texts.len() {
                    return Err(invalid("invalid activity message cursor"));
                }
                while let Some(source) = window.texts.get(message) {
                    if text >= source.text.len() || !source.text.is_char_boundary(text) {
                        return Err(invalid("invalid activity text cursor"));
                    }
                    let room = MAX_PAGE_TEXT.saturating_sub(output.bytes);
                    let mut end = source.text.len().min(text + room);
                    while !source.text.is_char_boundary(end) {
                        end -= 1;
                    }
                    if end == text || output.full(limit) {
                        position = Some(Resume::Inside {
                            key: candidate.key.clone(),
                            record,
                            message,
                            text,
                        });
                        stop = Some(Stop::Full);
                        break 'groups;
                    }
                    output.push(
                        &candidate,
                        Message {
                            role: source.role.into(),
                            line: source.line,
                            sent_at: source
                                .sent_at
                                .unwrap_or(candidate.key.at)
                                .to_rfc3339_opts(SecondsFormat::AutoSi, true),
                            sent_basis: if source.sent_at.is_some() {
                                SentBasis::Recorded
                            } else {
                                SentBasis::Saved
                            },
                            text: source.text[text..end].into(),
                            text_offset: text,
                            text_complete: end == source.text.len(),
                        },
                    );
                    if end == source.text.len() {
                        (message, text) = (message + 1, 0);
                    } else {
                        text = end;
                    }
                }
                if window.next_record == window.records {
                    break;
                }
                (record, message, text) = (window.next_record, 0, 0);
            }
            position = Some(Resume::Past(candidate.key.clone()));
        }
    }
    if !entered {
        changed(&mut reasons, &resume);
    }
    if let (None, Some(horizon)) = (&stop, horizon) {
        stop = Some(Stop::Horizon(horizon));
    }
    if let Some(Stop::Horizon(horizon)) = stop {
        position = Some(Resume::From(horizon));
    }
    let has_more = stop.is_some();
    ensure!(
        !has_more || position != resume,
        "activity read budget exhausted before making progress"
    );
    let next_cursor = match position.filter(|_| has_more) {
        Some(resume) => Some(hex::encode(serde_json::to_vec(&Cursor {
            query: binding,
            resume,
            reasons: reasons.iter().map(|(k, v)| (k.clone(), v.count)).collect(),
        })?)),
        None => None,
    };
    if let Some(cursor) = &next_cursor {
        ensure!(
            cursor.len() <= MAX_CURSOR,
            "activity cursor exceeds its size budget"
        );
    }
    Ok(Page {
        schema_version: SCHEMA_VERSION,
        repo: repo.into(),
        applied_filters: selection,
        contributions: output.contributions,
        has_more,
        next_cursor,
        incomplete: !reasons.is_empty(),
        reasons,
        warnings,
    })
}

/// A continuation inside a contribution that is no longer selected cannot finish its message.
fn changed(reasons: &mut BTreeMap<String, Reason>, resume: &Option<Resume>) {
    if let Some(Resume::Inside { key, .. }) = resume {
        reasons
            .entry("changed_during_pagination".into())
            .or_default()
            .note(&key.commit);
    }
}

/// Whether this client reads a Hub reply in `version`: the contract it names, or the save-time
/// contract of a Hub that ignores the name. Any other version may give a field another meaning,
/// so a reply in it means nothing to this client.
pub fn readable(version: u32) -> bool {
    version == SCHEMA_VERSION || version == SAVE_TIME_SCHEMA
}

const UNACKNOWLEDGED: &str = "the Hub did not acknowledge the requested activity contract; upgrade the Hub before relying on these results";

impl Page {
    /// Hub replies must echo a contract this client reads before their contents mean anything.
    pub fn acknowledges(&self, repo: &str, selection: &Selection) -> Result<()> {
        ensure!(
            readable(self.schema_version)
                && self.applied_filters == *selection
                && self.repo == repo,
            UNACKNOWLEDGED
        );
        Ok(())
    }

    /// Read a Hub's page in this client's contract.
    ///
    /// The version is checked before the fields: a page in another contract may spell them
    /// differently, and a parse error would hide that the Hub answered in it. A save-time page
    /// carries no send times, and its Hub bounded the window by save time. Each of its messages
    /// therefore reads as sent at its contribution's `saved_at` with [`SentBasis::Saved`], and
    /// its filters as [`SelectBy::Saved`]; read as recorded send times, or under the default
    /// selection, they would claim a window the Hub did not apply.
    pub fn from_hub(
        mut value: serde_json::Value,
        repo: &str,
        selection: &Selection,
    ) -> Result<Page> {
        let version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u32::try_from(version).ok());
        ensure!(version.is_some_and(readable), UNACKNOWLEDGED);
        if version == Some(SAVE_TIME_SCHEMA) {
            let basis = serde_json::to_value(SentBasis::Saved)?;
            let contributions = value
                .get_mut("contributions")
                .and_then(serde_json::Value::as_array_mut);
            for contribution in contributions.into_iter().flatten() {
                let saved_at = contribution.get("saved_at").cloned().unwrap_or_default();
                let messages = contribution
                    .get_mut("messages")
                    .and_then(serde_json::Value::as_array_mut);
                for message in messages.into_iter().flatten() {
                    if let Some(message) = message.as_object_mut() {
                        message.insert("sent_at".into(), saved_at.clone());
                        message.insert("sent_basis".into(), basis.clone());
                    }
                }
            }
        }
        let mut page: Page =
            serde_json::from_value(value).context("the Hub's activity page could not be read")?;
        page.acknowledges(repo, selection)?;
        if page.schema_version == SAVE_TIME_SCHEMA {
            page.schema_version = SCHEMA_VERSION;
            page.applied_filters.select_by = SelectBy::Saved;
        }
        Ok(page)
    }
}

/// Parse an instant reported by an attribution source.
fn reported(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|v| v.with_timezone(&Utc))
        .with_context(|| format!("invalid activity time {raw}"))
}
