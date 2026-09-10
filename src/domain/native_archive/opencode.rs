//! Record observations for a mutable OpenCode database, separate from native byte offsets.
//!
//! Completion admits a first observation; it does not promise that an asynchronous summary or
//! compaction cannot revise that row. Revisions append evidence without replacing LOG or VIEW.

use super::{Frontier, capture, opencode_observable_end};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const MAX_ROWS: usize = 8192;
const MAX_STATE_BYTES: usize = 1024 * 1024;
pub const REVISION_KIND: &str = "opencode.archive.revision";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RowState {
    identity: String,
    sha256: String,
    revision: u64,
    installed: bool,
}

/// Hashes stay private: the public evidence contains protected native data, not secret digests.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    version: u32,
    session: String,
    rows: BTreeMap<String, RowState>,
}

pub struct Observation {
    pub records: String,
    pub record_count: usize,
    pub frontier: Frontier,
    pub state: State,
    pub pending_bytes: usize,
}

struct Row<'a> {
    id: String,
    identity: String,
    raw: &'a str,
    end: usize,
    meta: bool,
    unfinished: bool,
}

fn digest(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn read(snapshot: &[u8]) -> crate::Result<(String, Vec<Row<'_>>, usize)> {
    ensure!(
        snapshot.len() <= crate::domain::storage::MAX_MATERIALIZED_BYTES,
        "OpenCode archive snapshot exceeds its byte limit"
    );
    let complete = capture(snapshot, 0, EMPTY)?;
    ensure!(
        complete.unconsumed.is_empty(),
        "OpenCode archive snapshot ends inside a record"
    );
    ensure!(
        complete.record_count <= MAX_ROWS,
        "OpenCode archive snapshot exceeds its row limit"
    );
    // This validates canonical kinds, unique IDs, message ownership and terminal tool states.
    // Native input cannot supply our revision kind, including nested observation wrappers.
    let (observed_end, unfinished_messages) = opencode_observable_end(snapshot)?;
    let mut rows = Vec::new();
    let mut end = 0;
    let mut session = None;
    for raw in complete.records.split_inclusive('\n') {
        end += raw.len();
        let mut value: serde_json::Value = serde_json::from_str(raw)?;
        let id = value["id"]
            .as_str()
            .context("OpenCode archive row has no identity")?
            .to_owned();
        ensure!(valid_id(&id), "OpenCode archive row identity is invalid");
        let meta = value["kind"] == "opencode.meta";
        let unfinished = unfinished_messages.contains(&id)
            || value["message_id"]
                .as_str()
                .is_some_and(|id| unfinished_messages.contains(id));
        if meta {
            session = Some(id.clone());
        }
        let fields = value
            .as_object_mut()
            .context("OpenCode archive row is not an object")?;
        if let Some(data) = fields.remove("data") {
            let mut identity = serde_json::Map::new();
            for key in [
                "id",
                "sessionID",
                "messageID",
                "parentID",
                "role",
                "type",
                "callID",
            ] {
                if let Some(field) = data.get(key) {
                    identity.insert(key.into(), field.clone());
                }
            }
            fields.insert("data".into(), serde_json::Value::Object(identity));
        }
        rows.push(Row {
            id,
            identity: digest(serde_json::to_vec(&value)?),
            raw,
            end,
            meta,
            unfinished,
        });
    }
    Ok((
        session.context("OpenCode archive snapshot has no session")?,
        rows,
        observed_end,
    ))
}

impl State {
    /// Seed only from the exact installed bytes, before the runtime can mutate their data.
    pub fn installed(snapshot: &[u8], session: &str) -> crate::Result<Self> {
        let (actual, rows, _) = read(snapshot)?;
        ensure!(
            actual == session,
            "OpenCode archive installation has another native session"
        );
        let state = Self {
            version: 1,
            session: actual,
            rows: rows
                .into_iter()
                .map(|row| {
                    (
                        row.id,
                        RowState {
                            identity: row.identity,
                            sha256: digest(row.raw),
                            revision: 0,
                            installed: true,
                        },
                    )
                })
                .collect(),
        };
        state.validate(session)?;
        Ok(state)
    }

    pub fn validate(&self, session: &str) -> crate::Result<()> {
        ensure!(
            self.version == 1 && self.session == session && valid_id(session),
            "OpenCode archive observation has another native session or version"
        );
        ensure!(
            !self.rows.is_empty() && self.rows.len() <= MAX_ROWS,
            "OpenCode archive observation row count is invalid"
        );
        ensure!(
            self.rows
                .get(session)
                .is_some_and(|row| row.installed && row.revision == 0),
            "OpenCode archive observation has no unchanged installed session"
        );
        for (id, row) in &self.rows {
            ensure!(
                valid_id(id),
                "OpenCode archive observation row identity is invalid"
            );
            for hash in [&row.identity, &row.sha256] {
                ensure!(
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                    "OpenCode archive observation digest is invalid"
                );
            }
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_STATE_BYTES,
            "OpenCode archive observation exceeds its private state limit"
        );
        Ok(())
    }

    pub fn validate_successor(&self, next: &Self, records: u64) -> crate::Result<()> {
        self.validate(&self.session)?;
        next.validate(&self.session)?;
        let mut changed = 0_u64;
        for (id, old) in &self.rows {
            let new = next
                .rows
                .get(id)
                .context("OpenCode archive publication removes an observed row")?;
            ensure!(
                old.identity == new.identity && old.installed == new.installed,
                "OpenCode archive publication changes row authority"
            );
            if old.sha256 == new.sha256 {
                ensure!(
                    old.revision == new.revision,
                    "OpenCode archive unchanged row advances its revision"
                );
            } else {
                ensure!(
                    old.revision.checked_add(1) == Some(new.revision),
                    "OpenCode archive publication skips a revision"
                );
                changed += 1;
            }
        }
        for (id, new) in &next.rows {
            if !self.rows.contains_key(id) {
                ensure!(
                    !new.installed && new.revision == 0,
                    "OpenCode archive publication invents installed or revised history"
                );
                changed += 1;
            }
        }
        ensure!(
            changed == records,
            "OpenCode archive publication record count differs from its observations"
        );
        Ok(())
    }

    pub fn observe(&self, snapshot: &[u8], prior: &Frontier) -> crate::Result<Observation> {
        self.validate(&self.session)?;
        let (session, rows, observed_end) = read(snapshot)?;
        ensure!(
            session == self.session,
            "OpenCode archive snapshot has another native session"
        );
        let present: BTreeSet<_> = rows.iter().map(|row| row.id.as_str()).collect();
        ensure!(
            self.rows.keys().all(|id| present.contains(id.as_str())),
            "OpenCode archive observed rows were deleted; native revert requires aborting or detaching this exploration"
        );
        let mut next = self.clone();
        let mut records = String::new();
        let mut count = 0;
        let mut pending_bytes = 0;
        for row in rows {
            let previous = self.rows.get(&row.id);
            if row.unfinished
                || (row.end > observed_end && !previous.is_some_and(|entry| entry.installed))
            {
                pending_bytes += row.raw.len();
            }
            let hash = digest(row.raw);
            if let Some(previous) = previous {
                ensure!(
                    previous.identity == row.identity,
                    "OpenCode archive row identity was reused or changed"
                );
                if previous.sha256 == hash {
                    continue;
                }
                ensure!(
                    !row.meta,
                    "OpenCode archive installed session metadata changed"
                );
                let revision = previous
                    .revision
                    .checked_add(1)
                    .context("OpenCode archive revision overflow")?;
                let record: serde_json::Value = serde_json::from_str(row.raw)?;
                let evidence = serde_json::json!({"kind":REVISION_KIND,"id":row.id,"revision":revision,"record":record});
                records.push_str(&serde_json::to_string(&evidence)?);
                records.push('\n');
                next.rows.insert(
                    row.id,
                    RowState {
                        identity: row.identity,
                        sha256: hash,
                        revision,
                        installed: previous.installed,
                    },
                );
            } else {
                if row.end > observed_end {
                    continue;
                }
                records.push_str(row.raw);
                next.rows.insert(
                    row.id,
                    RowState {
                        identity: row.identity,
                        sha256: hash,
                        revision: 0,
                        installed: false,
                    },
                );
            }
            count += 1;
            ensure!(
                records.len() <= crate::domain::storage::MAX_MATERIALIZED_BYTES,
                "OpenCode archive observation exceeds its byte limit"
            );
        }
        self.validate_successor(&next, count as u64)?;
        // Wrapping a revision adds nesting, so validate the actual outgoing record boundary too.
        let checked = capture(records.as_bytes(), 0, EMPTY)?;
        ensure!(
            checked.record_count == count && checked.unconsumed.is_empty(),
            "OpenCode archive observation has an invalid record boundary"
        );
        let frontier = if count == 0 {
            prior.clone()
        } else {
            let mut hash = Sha256::new();
            hash.update(prior.sha256.as_bytes());
            hash.update(records.as_bytes());
            Frontier {
                bytes: prior
                    .bytes
                    .checked_add(records.len() as u64)
                    .context("OpenCode archive cursor overflow")?,
                sha256: hex::encode(hash.finalize()),
            }
        };
        Ok(Observation {
            records,
            record_count: count,
            frontier,
            state: next,
            pending_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const META: &str = "{\"id\":\"ses_one\",\"kind\":\"opencode.meta\"}\n";
    const USER: &str = "{\"id\":\"user\",\"kind\":\"message\",\"session_id\":\"ses_one\",\"data\":{\"role\":\"user\",\"summary\":{\"diffs\":[]}}}\n";

    fn message(id: &str, completed: serde_json::Value) -> String {
        format!(
            "{}\n",
            serde_json::json!({"id":id,"kind":"message","session_id":"ses_one","data":{"role":"assistant","time":{"completed":completed}}})
        )
    }
    fn tool(message: &str, status: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"id":format!("part_{message}"),"kind":"part","session_id":"ses_one","message_id":message,"data":{"type":"tool","state":{"status":status,"output":"original output"}}})
        )
    }
    fn seed(bytes: &str) -> (State, Frontier) {
        (
            State::installed(bytes.as_bytes(), "ses_one").unwrap(),
            Frontier {
                bytes: bytes.len() as u64,
                sha256: digest(bytes),
            },
        )
    }

    #[test]
    fn completion_admits_observation_without_promising_immutability() {
        let (state, initial) = seed(META);
        let done = format!("{}{}", message("done", 2.into()), tool("done", "completed"));
        for completion in [
            serde_json::Value::Null,
            true.into(),
            "2".into(),
            (-1).into(),
            1.5.into(),
            2.into(),
        ] {
            for status in ["pending", "running", "completed", "error"] {
                let pending = format!(
                    "{}{}",
                    message("pending", completion.clone()),
                    tool("pending", status)
                );
                let later = message("later", 3.into());
                let bytes = format!("{META}{done}{pending}{later}");
                let captured = state.observe(bytes.as_bytes(), &initial).unwrap();
                if completion.as_u64().is_some() && matches!(status, "completed" | "error") {
                    assert_eq!(captured.records, format!("{done}{pending}{later}"));
                    assert_eq!(captured.pending_bytes, 0);
                } else {
                    assert_eq!(captured.records, done);
                    assert_eq!(captured.pending_bytes, pending.len() + later.len());
                }
            }
        }
    }

    #[test]
    fn asynchronous_summary_and_compaction_append_revisions_without_duplicate_turns() {
        use crate::adapter::{Adapter, EventKind};
        let (state, initial) = seed(META);
        let done = format!("{}{}", message("done", 2.into()), tool("done", "completed"));
        let before = format!("{META}{USER}{done}");
        let first = state.observe(before.as_bytes(), &initial).unwrap();
        let updated = before
            .replace(
                "\"diffs\":[]",
                "\"diffs\":[{\"file\":\"owned.txt\",\"before\":\"old\",\"after\":\"new\"}]",
            )
            .replace("original output", "compacted output");
        let appended = message("next", 4.into());
        let second = first
            .state
            .observe(format!("{updated}{appended}").as_bytes(), &first.frontier)
            .unwrap();
        let evidence: Vec<serde_json::Value> = second
            .records
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(evidence.len(), 3);
        assert_eq!(evidence[0]["kind"], REVISION_KIND);
        assert_eq!(evidence[0]["revision"], 1);
        assert_eq!(
            evidence[0]["record"]["data"]["summary"]["diffs"][0]["after"],
            "new"
        );
        assert_eq!(
            evidence[1]["record"]["data"]["state"]["output"],
            "compacted output"
        );
        assert_eq!(evidence[2]["id"], "next");
        assert!(first.records.contains("original output"));
        assert!(!first.records.contains("owned.txt"));
        let parsed = crate::adapter::opencode::OpenCode
            .parse(&second.records)
            .unwrap();
        assert_eq!(
            parsed
                .events
                .iter()
                .filter(|event| event.kind == EventKind::UserPrompt)
                .count(),
            0
        );
        assert_eq!(
            parsed
                .events
                .iter()
                .filter(|event| event.kind == EventKind::Other)
                .count(),
            2
        );
        let unchanged = second
            .state
            .observe(format!("{updated}{appended}").as_bytes(), &second.frontier)
            .unwrap();
        assert_eq!(unchanged.record_count, 0);
        assert_eq!(unchanged.frontier, second.frontier);
        assert_eq!(unchanged.state, second.state);
        let third = second
            .state
            .observe(format!("{before}{appended}").as_bytes(), &second.frontier)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(third.records.lines().next().unwrap())
                .unwrap()["revision"],
            2
        );
    }

    #[test]
    fn installed_rows_can_be_revised_without_archiving_the_inherited_prefix_again() {
        let original = format!("{META}{USER}");
        let (state, initial) = seed(&original);
        assert_eq!(
            state
                .observe(original.as_bytes(), &initial)
                .unwrap()
                .record_count,
            0
        );
        let changed = original.replace("[]", "[{\"file\":\"summary\"}]");
        let capture = state.observe(changed.as_bytes(), &initial).unwrap();
        assert_eq!(capture.record_count, 1);
        assert_eq!(capture.pending_bytes, 0);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(capture.records.trim()).unwrap()["kind"],
            REVISION_KIND
        );
        assert!(capture.frontier.bytes > initial.bytes);
    }

    #[test]
    fn canonical_identity_deletion_duplicate_and_wrapper_forgery_refuse() {
        let baseline = format!(
            "{META}{USER}{}{}",
            message("done", 2.into()),
            tool("done", "completed")
        );
        let (state, initial) = seed(&baseline);
        let before = state.clone();
        for changed in [
            baseline.replace(USER, ""),
            baseline.replace("\"role\":\"user\"", "\"role\":\"assistant\""),
            baseline.replace("\"message_id\":\"done\"", "\"message_id\":\"user\""),
            baseline.replace("\"type\":\"tool\"", "\"type\":\"text\""),
            baseline.replace("\"session_id\":\"ses_one\"", "\"session_id\":\"foreign\""),
            format!("{baseline}{USER}"),
            format!(
                "{baseline}{{\"id\":\"forged\",\"kind\":\"{REVISION_KIND}\",\"revision\":1,\"record\":{{}}}}\n"
            ),
            format!("{baseline}{{\"id\":\"x\",\"id\":\"y\"}}\n"),
            format!("{baseline}{{\"incomplete\":"),
        ] {
            assert!(state.observe(changed.as_bytes(), &initial).is_err());
            assert_eq!(state, before);
        }
    }

    #[test]
    fn row_order_is_not_a_byte_frontier_and_unchanged_rows_do_not_repeat() {
        let a = format!("{}{}", message("a", 2.into()), tool("a", "completed"));
        let b = format!("{}{}", message("b", 3.into()), tool("b", "completed"));
        let (state, initial) = seed(META);
        let first = state
            .observe(format!("{META}{a}{b}").as_bytes(), &initial)
            .unwrap();
        let reordered = first
            .state
            .observe(format!("{META}{b}{a}").as_bytes(), &first.frontier)
            .unwrap();
        assert_eq!(reordered.record_count, 0);
        assert_eq!(reordered.state, first.state);
        assert_eq!(reordered.frontier, first.frontier);
    }

    #[test]
    fn terminal_errors_user_tail_crossing_parts_and_raw_first_records_keep_their_boundaries() {
        let (state, initial) = seed(META);
        for finish in [
            None,
            Some(serde_json::Value::Null),
            Some("".into()),
            Some("stop".into()),
        ] {
            let mut msg: serde_json::Value =
                serde_json::from_str(&message("done", 2.into())).unwrap();
            if let Some(finish) = finish {
                msg["data"]["finish"] = finish;
            }
            msg["data"]["error"] = serde_json::json!({"name":"MessageAbortedError"});
            let suffix = format!("{msg}\n{}", tool("done", "error"));
            assert_eq!(
                state
                    .observe(format!("{META}{suffix}").as_bytes(), &initial)
                    .unwrap()
                    .records,
                suffix
            );
        }
        let held = state
            .observe(format!("{META}{USER}").as_bytes(), &initial)
            .unwrap();
        assert_eq!(held.record_count, 0);
        assert_eq!(held.pending_bytes, USER.len());
        let crossing = format!(
            "{META}{}{}{}",
            message("done", 2.into()),
            message("pending", serde_json::Value::Null),
            tool("done", "completed")
        );
        assert_eq!(
            state
                .observe(crossing.as_bytes(), &initial)
                .unwrap()
                .record_count,
            0
        );
        let raw = " { \"id\":\"p\", \"kind\":\"part\", \"session_id\":\"ses_one\", \"message_id\":\"done\", \"data\":{\"type\":\"future\",\"number\":9007199254740993.00000000001} }\r\n";
        let suffix = format!("{}{raw}", message("done", 2.into()));
        assert_eq!(
            state
                .observe(format!("{META}{suffix}").as_bytes(), &initial)
                .unwrap()
                .records,
            suffix
        );
    }
    #[test]
    fn real_sqlite_snapshot_summary_update_does_not_invalidate_prior_evidence() {
        use crate::adapter::{
            Adapter,
            native_snapshot::{Limits, Source},
        };
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("native.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT, time_created INTEGER, time_updated INTEGER, version TEXT);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
            CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, data TEXT);
            INSERT INTO session VALUES ('ses_one','project',NULL,'/synthetic',0,0,'1.18.13');").unwrap();
        let source = Source {
            runtime: "opencode",
            session_id: "ses_one".into(),
            path: database.clone(),
            database: true,
        };
        let snapshot = || {
            crate::adapter::opencode::OpenCode
                .snapshot_native_readonly(&source, Limits::default())
                .unwrap()
                .bytes
        };
        let installed = snapshot();
        let state = State::installed(&installed, "ses_one").unwrap();
        let cursor = Frontier {
            bytes: installed.len() as u64,
            sha256: digest(&installed),
        };
        connection
            .execute(
                "INSERT INTO message VALUES ('user','ses_one',1,?1)",
                [r#"{"role":"user","summary":{"diffs":[]}}"#],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES ('done','ses_one',2,?1)",
                [r#"{"role":"assistant","time":{"completed":3}}"#],
            )
            .unwrap();
        let before = snapshot();
        let first = state.observe(&before, &cursor).unwrap();
        connection.execute("UPDATE message SET data=?1 WHERE id='user'",[r#"{"role":"user","summary":{"diffs":[{"file":"owned.txt","after":"summary edit"}]}}"#]).unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES ('next','ses_one',4,?1)",
                [r#"{"role":"assistant","time":{"completed":5}}"#],
            )
            .unwrap();
        let database_before = std::fs::read(&database).unwrap();
        let after = snapshot();
        assert!(
            capture(&after, first.frontier.bytes, &digest(&before)).is_err(),
            "a native byte-prefix cursor cannot survive this update"
        );
        let second = first.state.observe(&after, &first.frontier).unwrap();
        assert_eq!(second.record_count, 2);
        assert_eq!(second.pending_bytes, 0);
        assert_eq!(std::fs::read(&database).unwrap(), database_before);
        assert!(first.records.contains("\"diffs\":[]"));
        assert!(second.records.contains("summary edit"));
        assert_eq!(
            first
                .state
                .observe(&after, &first.frontier)
                .unwrap()
                .records,
            second.records
        );
        assert_eq!(
            second
                .state
                .observe(&after, &second.frontier)
                .unwrap()
                .record_count,
            0
        );
    }

    #[test]
    fn persisted_observation_successor_requires_exact_revision_and_count() {
        let (state, initial) = seed(&format!("{META}{USER}"));
        let changed = format!("{META}{}", USER.replace("[]", "[{}]"));
        let observed = state.observe(changed.as_bytes(), &initial).unwrap();
        state.validate_successor(&observed.state, 1).unwrap();
        assert!(state.validate_successor(&observed.state, 0).is_err());
        let mut bad = observed.state.clone();
        bad.rows.get_mut("user").unwrap().revision += 1;
        assert!(state.validate_successor(&bad, 1).is_err());
        bad = observed.state.clone();
        bad.rows.remove("user");
        assert!(state.validate_successor(&bad, 1).is_err());
        bad = observed.state.clone();
        bad.rows.get_mut("user").unwrap().installed = false;
        assert!(state.validate_successor(&bad, 1).is_err());
    }
    #[test]
    fn row_and_private_state_budgets_refuse_instead_of_truncating() {
        let over_rows = format!("{META}{}", (0..MAX_ROWS).map(|index| format!("{{\"id\":\"u{index}\",\"kind\":\"message\",\"session_id\":\"ses_one\",\"data\":{{\"role\":\"user\"}}}}\n")).collect::<String>());
        assert!(
            format!(
                "{:#}",
                State::installed(over_rows.as_bytes(), "ses_one").unwrap_err()
            )
            .contains("row limit")
        );
        let large_state = format!("{META}{}", (0..MAX_ROWS/2).map(|index| format!("{{\"id\":\"u{index}_{}\",\"kind\":\"message\",\"session_id\":\"ses_one\",\"data\":{{\"role\":\"user\"}}}}\n", "x".repeat(230))).collect::<String>());
        assert!(
            format!(
                "{:#}",
                State::installed(large_state.as_bytes(), "ses_one").unwrap_err()
            )
            .contains("private state limit")
        );
    }
    #[test]
    fn installed_identity_does_not_exempt_an_unfinished_assistant_or_tool() {
        let original = format!(
            "{META}{USER}{}{}",
            message("done", 2.into()),
            tool("done", "completed")
        );
        let (state, initial) = seed(&original);
        for changed in [
            original.replace("\"completed\":2", "\"completed\":null"),
            original.replace("\"status\":\"completed\"", "\"status\":\"running\""),
        ] {
            let observed = state.observe(changed.as_bytes(), &initial).unwrap();
            assert_eq!(observed.record_count, 1);
            assert!(observed.pending_bytes > 0);
            let repeated = observed
                .state
                .observe(changed.as_bytes(), &observed.frontier)
                .unwrap();
            assert_eq!(repeated.record_count, 0);
            assert!(repeated.pending_bytes > 0);
        }
    }
}
