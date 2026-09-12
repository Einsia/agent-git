//! Mutable native rows are compared by identity before activity is counted.

use crate::adapter::{EventKind, native_snapshot::Limits};
use crate::domain::{link::Link, metadata_facts::JsonFacts, repo::Repo, storage};
use anyhow::{Context, ensure};
use std::collections::{BTreeMap, BTreeSet};

type Key = (String, String);

struct Row {
    facts: JsonFacts,
    line: usize,
}

struct Rows {
    entries: BTreeMap<Key, Row>,
    lines: BTreeMap<usize, Key>,
    text: String,
}

fn field<'a>(facts: &'a JsonFacts, key: &str) -> Option<&'a JsonFacts> {
    match facts {
        JsonFacts::Object(fields) => fields.get(key),
        _ => None,
    }
}

fn string(facts: &JsonFacts) -> Option<&str> {
    match facts {
        JsonFacts::String(value) => Some(value),
        _ => None,
    }
}

fn text_field<'a>(facts: &'a JsonFacts, key: &str) -> Option<&'a str> {
    field(facts, key).and_then(string)
}

fn data(row: &Row) -> Option<&JsonFacts> {
    field(&row.facts, "data")
}

fn message_key(id: &str) -> Key {
    ("message".into(), id.into())
}

fn stable_identity(left: &Row, right: &Row) -> bool {
    ["kind", "id", "session_id", "time_created", "message_id"]
        .iter()
        .all(|key| field(&left.facts, key) == field(&right.facts, key))
}

fn stable_occurrence(left: &Row, right: &Row) -> bool {
    let keys: &[&str] = if text_field(&left.facts, "kind") == Some("message") {
        &["role", "parentID"]
    } else {
        &["type", "tool", "callID"]
    };
    stable_identity(left, right)
        && keys.iter().all(|key| {
            data(left).and_then(|value| field(value, key))
                == data(right).and_then(|value| field(value, key))
        })
}

impl Rows {
    fn read(text: &str, session: &str, envelopes: bool, limits: Limits) -> crate::Result<Self> {
        ensure!(
            text.len() <= limits.bytes,
            "OpenCode comparison exceeds its byte limit"
        );
        let mut result = Self {
            entries: BTreeMap::new(),
            lines: BTreeMap::new(),
            text: String::new(),
        };
        let mut meta_seen = false;
        let mut nodes = limits.working_bytes / 128;
        for (line, raw) in text.lines().enumerate() {
            ensure!(
                line < limits.records && raw.len() <= storage::MAX_EVENT_BYTES,
                "OpenCode comparison exceeds its record limit"
            );
            ensure!(
                !raw.trim().is_empty(),
                "OpenCode evidence contains an empty record"
            );
            let mut facts = JsonFacts::parse_with_node_budget(raw, &mut nodes)
                .context("OpenCode evidence contains ambiguous or over-budget JSON")?;
            let mut value: serde_json::Value = serde_json::from_str(raw)?;
            if envelopes {
                ensure!(
                    text_field(&facts, "_source") == Some("opencode"),
                    "the saved LOG contains another runtime"
                );
                facts = field(&facts, "content")
                    .context("the saved LOG has no content")?
                    .clone();
                value = value["content"].take();
            }
            let kind =
                text_field(&facts, "kind").context("OpenCode evidence has no record kind")?;
            let id = text_field(&facts, "id")
                .filter(|id| !id.is_empty())
                .context("OpenCode evidence has no native identity")?;
            let created = field(&facts, "time_created")
                .context("OpenCode evidence has no creation identity")?;
            ensure!(
                matches!(created, JsonFacts::Atom(value) if value.parse::<i64>().is_ok()),
                "OpenCode creation identity is not an integer"
            );
            match kind {
                "opencode.meta" => {
                    ensure!(
                        id == session,
                        "OpenCode metadata identifies another native session"
                    );
                    meta_seen = true;
                }
                "message" | "part" => {
                    ensure!(
                        text_field(&facts, "session_id") == Some(session),
                        "OpenCode record identifies another native session"
                    );
                    ensure!(
                        matches!(field(&facts, "data"), Some(JsonFacts::Object(_))),
                        "OpenCode record data is not an object"
                    );
                    if kind == "part" {
                        ensure!(
                            text_field(&facts, "message_id").is_some_and(|id| !id.is_empty()),
                            "OpenCode part has no host identity"
                        );
                    }
                }
                _ => anyhow::bail!("OpenCode evidence contains an unsupported record kind"),
            }
            let key = (kind.to_owned(), id.to_owned());
            let row = Row { facts, line };
            if let Some(previous) = result.entries.get(&key) {
                ensure!(
                    envelopes && stable_occurrence(previous, &row),
                    "OpenCode evidence repeats or changes a native identity"
                );
            }
            result.lines.insert(line, key.clone());
            result.entries.insert(key, row);
            result.text.push_str(&serde_json::to_string(&value)?);
            result.text.push('\n');
        }
        ensure!(
            meta_seen || text.is_empty(),
            "OpenCode evidence has no session metadata"
        );
        Ok(result)
    }

    fn at_line(&self, line: usize) -> Option<(&Key, &Row)> {
        let key = self.lines.get(&line)?;
        let row = self.entries.get(key)?;
        (row.line == line).then_some((key, row))
    }

    fn host<'a>(&'a self, key: &'a Key, row: &'a Row) -> Option<&'a str> {
        match key.0.as_str() {
            "message" => Some(&key.1),
            "part" => text_field(&row.facts, "message_id"),
            _ => None,
        }
    }
}

pub(super) fn inspect(repo: &Repo, claim: &Link, tip: &str, log: &str) -> crate::Result<String> {
    inspect_with_limits(repo, claim, tip, log, Limits::default())
}

pub(super) fn inspect_with_limits(
    repo: &Repo,
    claim: &Link,
    tip: &str,
    log: &str,
    limits: Limits,
) -> crate::Result<String> {
    let adapter = crate::adapter::get("opencode")?;
    let source = adapter.lookup_native_readonly(&claim.session_id, limits)?;
    let snapshot = adapter.snapshot_native_readonly(&source, limits)?;
    compare_snapshot(repo, claim, tip, log, &snapshot.bytes, limits, false)
}

/// Snapshot acquisition is separate from native identity and revision comparison.
pub(super) fn compare_snapshot(
    repo: &Repo,
    claim: &Link,
    tip: &str,
    log: &str,
    bytes: &[u8],
    limits: Limits,
    status: bool,
) -> crate::Result<String> {
    let live = std::str::from_utf8(bytes).context("OpenCode snapshot is not UTF-8")?;
    let materialized = claim.baseline_bytes.is_some()
        || claim.baseline_hash.is_some()
        || claim.materialized_from.is_some();
    if materialized {
        let boundary = super::materialized_boundary(claim, tip, bytes)
            .context("the reminted OpenCode baseline cannot be reconstructed from changed bytes")?;
        return compare(
            &Rows::read(&live[..boundary], &claim.session_id, false, limits)?,
            &Rows::read(live, &claim.session_id, false, limits)?,
            None,
        );
    }
    let old = Rows::read(log, &claim.session_id, true, limits)?;
    let current = Rows::read(live, &claim.session_id, false, limits)?;
    // Hydration supplies only string comparisons. Original numeric tokens remain authoritative.
    let projection = if current.entries.iter().any(|(key, row)| {
        old.entries
            .get(key)
            .is_some_and(|prior| prior.facts != row.facts)
    }) {
        let reports = crate::domain::secret_filter::RepositoryDictionary::open(repo.root())?
            .hydrate_batch_readonly_with_limits(
                &[log, live],
                limits.working_bytes,
                status.then_some(crate::domain::secret_filter::ReadonlyDictionaryLimits::STATUS),
            )?;
        let mut reports = reports.into_iter();
        let left = reports.next().context("missing saved hydration result")??;
        let right = reports
            .next()
            .context("missing native hydration result")??;
        ensure!(
            left.unresolved == 0 && right.unresolved == 0,
            "OpenCode comparison requires unavailable repository secret mappings"
        );
        Some((
            Rows::read(&left.text, &claim.session_id, true, limits)?,
            Rows::read(&right.text, &claim.session_id, false, limits)?,
        ))
    } else {
        None
    };
    compare(&old, &current, projection.as_ref())
}

fn same_facts(
    left: &JsonFacts,
    right: &JsonFacts,
    projected: Option<(&JsonFacts, &JsonFacts)>,
) -> bool {
    match (left, right) {
        (JsonFacts::String(_), JsonFacts::String(_)) => {
            projected.map_or(left == right, |(a, b)| a == b)
        }
        (JsonFacts::Object(a), JsonFacts::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(key, value)| {
                    b.get(key).is_some_and(|other| {
                        same_facts(
                            value,
                            other,
                            projected.and_then(|(p, q)| Some((field(p, key)?, field(q, key)?))),
                        )
                    })
                })
        }
        (JsonFacts::Array(a), JsonFacts::Array(b)) => {
            a.len() == b.len()
                && a.iter().zip(b).enumerate().all(|(i, (a, b))| {
                    same_facts(
                        a,
                        b,
                        projected.and_then(|(p, q)| match (p, q) {
                            (JsonFacts::Array(p), JsonFacts::Array(q)) => {
                                Some((p.get(i)?, q.get(i)?))
                            }
                            _ => None,
                        }),
                    )
                })
        }
        _ => left == right,
    }
}

struct Semantics {
    prompts: BTreeSet<String>,
    prompt_rows: BTreeMap<Key, String>,
    tools: BTreeMap<Key, Key>,
    known: BTreeSet<Key>,
    turns: BTreeMap<String, Option<String>>,
    compactions: Vec<BTreeSet<Key>>,
}

impl Semantics {
    fn read(rows: &Rows) -> crate::Result<Self> {
        let parsed = crate::adapter::opencode::parse_with_compaction_evidence(&rows.text)?;
        let mut result = Self {
            prompts: BTreeSet::new(),
            prompt_rows: BTreeMap::new(),
            tools: BTreeMap::new(),
            known: BTreeSet::new(),
            turns: BTreeMap::new(),
            compactions: vec![],
        };
        for event in &parsed.session.events {
            let Some((key, row)) = event.line.and_then(|line| rows.at_line(line)) else {
                continue;
            };
            if event.kind != EventKind::Other {
                result.known.insert(key.clone());
            }
            if event.kind == EventKind::UserPrompt
                && let Some(host) = rows.host(key, row)
            {
                result.prompts.insert(host.into());
                result.prompt_rows.insert(key.clone(), host.into());
            }
            if event.kind == EventKind::ToolUse
                && let Some(host) = rows.host(key, row)
            {
                if rows
                    .entries
                    .get(&message_key(host))
                    .and_then(data)
                    .and_then(|header| text_field(header, "role"))
                    != Some("assistant")
                {
                    result.known.remove(key);
                    continue;
                }
                let call = data(row)
                    .and_then(|data| text_field(data, "callID"))
                    .filter(|id| !id.is_empty())
                    .map(|id| format!("call:{id}"))
                    .unwrap_or_else(|| format!("part:{}", key.1));
                result.tools.insert(key.clone(), (host.into(), call));
            }
        }
        for evidence in parsed.compactions.values() {
            let mut dependencies = BTreeSet::new();
            for line in evidence
                .boundary_lines
                .iter()
                .chain(&evidence.summary_lines)
            {
                if let Some((key, row)) = rows.at_line(*line) {
                    dependencies.insert(key.clone());
                    if let Some(host) = rows.host(key, row) {
                        dependencies.insert(message_key(host));
                    }
                }
            }
            result.known.extend(dependencies.iter().cloned());
            result.compactions.push(dependencies);
        }
        for (key, _) in rows.entries.iter().filter(|(key, _)| key.0 == "message") {
            result.resolve_turn(rows, &key.1);
        }
        Ok(result)
    }

    fn resolve_turn(&mut self, rows: &Rows, start: &str) {
        let mut path = BTreeSet::new();
        let mut cursor = start.to_owned();
        let found = loop {
            if let Some(known) = self.turns.get(&cursor) {
                break known.clone();
            }
            if !path.insert(cursor.clone()) {
                break None;
            }
            if self.prompts.contains(&cursor) {
                break Some(cursor);
            }
            let Some(header) = rows.entries.get(&message_key(&cursor)).and_then(data) else {
                break None;
            };
            if text_field(header, "role") != Some("assistant") {
                break None;
            }
            let Some(parent) = text_field(header, "parentID").filter(|id| !id.is_empty()) else {
                break None;
            };
            cursor = parent.into();
        };
        for id in path {
            self.turns.insert(id, found.clone());
        }
    }

    fn turn<'a>(&'a self, rows: &Rows, key: &Key, row: &Row) -> Option<&'a str> {
        self.turns.get(rows.host(key, row)?)?.as_deref()
    }
}

fn compare(old: &Rows, current: &Rows, projection: Option<&(Rows, Rows)>) -> crate::Result<String> {
    let mut added = BTreeSet::new();
    let mut updated = BTreeSet::new();
    for (key, row) in &current.entries {
        match old.entries.get(key) {
            Some(prior) => {
                ensure!(
                    stable_identity(prior, row),
                    "OpenCode native identity or host changed since the saved snapshot"
                );
                let projected = projection.and_then(|(a, b)| {
                    Some((&a.entries.get(key)?.facts, &b.entries.get(key)?.facts))
                });
                if !same_facts(&prior.facts, &row.facts, projected) {
                    updated.insert(key.clone());
                }
            }
            None => {
                added.insert(key.clone());
            }
        }
    }
    let missing = old
        .entries
        .keys()
        .filter(|key| !current.entries.contains_key(*key))
        .count();
    if added.is_empty() && updated.is_empty() && missing == 0 {
        return Ok("no unsettled content in the verified native snapshot".into());
    }
    let before = Semantics::read(old)?;
    let after = Semantics::read(current)?;
    let changed: BTreeSet<_> = added.union(&updated).cloned().collect();
    let new_turns: BTreeSet<_> = after
        .prompt_rows
        .iter()
        .filter(|(key, host)| {
            !before.prompts.contains(*host)
                && (added.contains(*key) || added.contains(&message_key(host)))
                && old.entries.get(&message_key(host)).is_none_or(|prior| {
                    let header = &current.entries[&message_key(host)];
                    data(prior).and_then(|value| text_field(value, "role")) == Some("user")
                        && data(prior).and_then(|value| field(value, "parentID"))
                            == data(header).and_then(|value| field(value, "parentID"))
                })
        })
        .map(|(_, host)| host)
        .collect();
    let prior_calls: BTreeSet<_> = before.tools.values().collect();
    let new_calls: BTreeSet<_> = after
        .tools
        .iter()
        .filter(|(key, call)| added.contains(*key) && !prior_calls.contains(call))
        .map(|(_, call)| call)
        .collect();
    let mut turns = BTreeSet::new();
    let mut unknown = missing;
    for key in &changed {
        let row = &current.entries[key];
        let turn = after.turn(current, key, row);
        let unchanged_parent = current.host(key, row).is_none_or(|host| {
            let host_key = message_key(host);
            old.entries.get(&host_key).is_none_or(|prior| {
                (before.turn(old, &host_key, prior) == turn
                    || (turn == Some(host)
                        && data(prior).and_then(|header| text_field(header, "role"))
                            == Some("user")))
                    && data(prior).and_then(|value| field(value, "parentID"))
                        == current
                            .entries
                            .get(&host_key)
                            .and_then(data)
                            .and_then(|value| field(value, "parentID"))
            })
        });
        if unchanged_parent && let Some(turn) = turn {
            turns.insert(turn);
        }
        let modeled = after.known.contains(key);
        if !modeled || turn.is_none() || !unchanged_parent {
            unknown += 1;
        }
    }
    let compactions = after
        .compactions
        .iter()
        .filter(|dependencies| !dependencies.is_disjoint(&changed))
        .count();
    let warning = if unknown > 0 {
        format!(
            "; {unknown} native rows have unclassified or unattributed activity, so semantic counts are lower bounds"
        )
    } else {
        String::new()
    };
    Ok(format!(
        "{} added, {} updated, {missing} missing native events; {} user turns with pending activity ({} newly started), {} new ToolUse calls, {compactions} changed compactions{warning}",
        added.len(),
        updated.len(),
        turns.len(),
        new_turns.len(),
        new_calls.len()
    ))
}
