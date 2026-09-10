use agit::domain::{link, meta, repo::Repo, storage, store::Store, transcript};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SID: &str = "ses_pending_fixture";

fn isolated(program: impl AsRef<std::ffi::OsStr>, root: &Path) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    for key in [
        "PATH",
        "SystemRoot",
        "WINDIR",
        "TEMP",
        "TMP",
        "ComSpec",
        "PATHEXT",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("AGIT_HOME", root.join("agit"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("AGIT_SESSION", "alice/notes@topic")
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("empty-config"))
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("NO_COLOR", "1")
        .env("AGIT_TUI", "0")
        .current_dir(root);
    command
}

fn success(output: Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn message(id: &str, created: i64, data: Value) -> Value {
    json!({"id":id,"kind":"message","session_id":SID,"time_created":created,"data":data})
}

fn part(id: &str, host: &str, created: i64, data: Value) -> Value {
    json!({"id":id,"kind":"part","session_id":SID,"message_id":host,"time_created":created,"data":data})
}

fn baseline() -> Vec<Value> {
    vec![
        message("user", 10, json!({"role":"user"})),
        part(
            "prompt",
            "user",
            11,
            json!({"type":"text","text":"saved prompt"}),
        ),
        message(
            "assistant",
            20,
            json!({"role":"assistant","parentID":"user"}),
        ),
        part(
            "reply",
            "assistant",
            21,
            json!({"type":"text","text":"saved answer"}),
        ),
        part(
            "tool",
            "assistant",
            22,
            json!({"type":"tool","tool":"bash","callID":"call-original","state":{"status":"running","input":{}}}),
        ),
    ]
}

fn canonical(rows: &[Value]) -> String {
    let meta = json!({"directory":"/fixture","id":SID,"kind":"opencode.meta","parent_id":null,"project_id":"project","time_created":1,"version":"fixture"});
    let mut records = vec![(1, 0, SID.to_owned(), meta.to_string())];
    for row in rows {
        let id = row["id"].as_str().unwrap();
        let created = row["time_created"].as_i64().unwrap();
        let encoded = if row["kind"] == "message" {
            format!(
                "{{\"id\":{},\"kind\":\"message\",\"session_id\":{},\"time_created\":{created},\"data\":{}}}",
                json!(id),
                json!(SID),
                row["data"]
            )
        } else {
            format!(
                "{{\"id\":{},\"kind\":\"part\",\"message_id\":{},\"session_id\":{},\"time_created\":{created},\"data\":{}}}",
                json!(id),
                row["message_id"],
                json!(SID),
                row["data"]
            )
        };
        records.push((created, u8::from(row["kind"] == "part"), id.into(), encoded));
    }
    records.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    records
        .into_iter()
        .map(|(_, _, _, text)| format!("{text}\n"))
        .collect()
}

struct Lab {
    temp: tempfile::TempDir,
    repo: Repo,
    database: PathBuf,
    connection: Connection,
    claim: link::Link,
    original: String,
}

impl Lab {
    fn new(rows: &[Value]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("empty-config"), "").unwrap();
        fs::create_dir(root.join("empty-hooks")).unwrap();
        let repo = Repo::at(root.join("agit/repos/alice/notes"));
        fs::create_dir_all(repo.root()).unwrap();
        success(
            isolated("git", root)
                .arg("-C")
                .arg(repo.root())
                .args(["init", "-b", "topic"])
                .output()
                .unwrap(),
        );
        let database = root.join("data/opencode/opencode.db");
        fs::create_dir_all(database.parent().unwrap()).unwrap();
        let connection = Connection::open(&database).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE session(id TEXT PRIMARY KEY,project_id TEXT,parent_id TEXT,directory TEXT,time_created INTEGER,time_updated INTEGER,version TEXT);
            CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,time_updated INTEGER,data TEXT);
            CREATE TABLE part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,time_created INTEGER,time_updated INTEGER,data TEXT);").unwrap();
        connection
            .execute(
                "INSERT INTO session VALUES (?1,'project',NULL,'/fixture',1,1,'fixture')",
                [SID],
            )
            .unwrap();
        let mut claim = link::Link::new("opencode", SID, None);
        claim.owner = Some("alice".into());
        claim.agent = Some("notes".into());
        claim.branch = Some("topic".into());
        let lab = Self {
            temp,
            repo,
            database,
            connection,
            claim,
            original: canonical(rows),
        };
        lab.replace_rows(rows);
        lab.git(&["config", "commit.gpgsign", "false"]);
        lab.git(&[
            "config",
            "core.hooksPath",
            lab.temp.path().join("empty-hooks").to_str().unwrap(),
        ]);
        lab.save_log(&lab.original);
        lab.save_claim(&lab.claim);
        lab
    }

    fn git(&self, args: &[&str]) -> String {
        success(
            isolated("git", self.temp.path())
                .arg("-C")
                .arg(self.repo.root())
                .args(args)
                .output()
                .unwrap(),
        )
    }

    fn save_log(&self, raw: &str) {
        let session = format!("agit-{}", "a".repeat(40));
        let log = transcript::wrap_lines(raw, "opencode", &session);
        storage::write_snapshot(self.repo.root(), &log, &log).unwrap();
        let mut metadata = meta::Meta::new(session, "opencode".into(), "/fixture".into());
        metadata.turn = Some(1);
        meta::write(self.repo.root(), &metadata).unwrap();
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", "native evidence"]);
    }

    fn save_claim(&self, claim: &link::Link) {
        link::write(&Store::at(self.temp.path().join("agit/store")), claim).unwrap();
    }

    fn replace_rows(&self, rows: &[Value]) {
        self.connection
            .execute_batch("DELETE FROM part; DELETE FROM message;")
            .unwrap();
        for row in rows {
            if row["kind"] == "message" {
                self.connection
                    .execute(
                        "INSERT INTO message VALUES (?1,?2,?3,1,?4)",
                        rusqlite::params![
                            row["id"].as_str().unwrap(),
                            SID,
                            row["time_created"].as_i64().unwrap(),
                            row["data"].to_string()
                        ],
                    )
                    .unwrap();
            } else {
                self.connection
                    .execute(
                        "INSERT INTO part VALUES (?1,?2,?3,?4,1,?5)",
                        rusqlite::params![
                            row["id"].as_str().unwrap(),
                            SID,
                            row["message_id"].as_str().unwrap(),
                            row["time_created"].as_i64().unwrap(),
                            row["data"].to_string()
                        ],
                    )
                    .unwrap();
            }
        }
    }

    fn application_rows(&self) -> Vec<Vec<String>> {
        let mut result = vec![];
        for query in [
            "SELECT id,project_id,coalesce(parent_id,''),directory,cast(time_created AS TEXT),cast(time_updated AS TEXT),version FROM session ORDER BY id",
            "SELECT id,session_id,cast(time_created AS TEXT),cast(time_updated AS TEXT),data FROM message ORDER BY id",
            "SELECT id,session_id,message_id,cast(time_created AS TEXT),cast(time_updated AS TEXT),data FROM part ORDER BY id",
        ] {
            let mut statement = self.connection.prepare(query).unwrap();
            let width = statement.column_count();
            result.extend(
                statement
                    .query_map([], |row| {
                        (0..width)
                            .map(|i| row.get(i))
                            .collect::<rusqlite::Result<Vec<String>>>()
                    })
                    .unwrap()
                    .map(Result::unwrap),
            );
        }
        result
    }

    fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        walkdir::WalkDir::new(self.temp.path())
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            // SQLite may update WAL reader-coordination bytes without changing application rows.
            .filter(|entry| entry.path() != self.database.with_extension("db-shm"))
            .map(|entry| {
                (
                    entry
                        .path()
                        .strip_prefix(self.temp.path())
                        .unwrap()
                        .to_owned(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    fn inspect(&self, code: i32) -> String {
        self.inspect_args(code, &["diff"])
    }

    fn inspect_args(&self, code: i32, args: &[&str]) -> String {
        let before = self.files();
        let rows = self.application_rows();
        let output = isolated(env!("CARGO_BIN_EXE_agit"), self.temp.path())
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        assert_eq!(
            self.application_rows(),
            rows,
            "inspection changed native application rows"
        );
        assert_eq!(
            self.files(),
            before,
            "inspection changed cache, claims, Git state, or native data"
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

#[test]
fn both_json_versions_preserve_the_native_update_summary() {
    let mut rows = baseline();
    let lab = Lab::new(&rows);
    rows[3]["data"]["text"] = json!("streamed update");
    lab.replace_rows(&rows);
    for version in ["1", "2"] {
        let text = lab.inspect_args(0, &["--json", "--json-version", version, "diff"]);
        let document: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(document["schema_version"], version.parse::<u32>().unwrap());
        assert_eq!(document["result"]["format"], "text");
        assert_eq!(document["exit_code"], 0);
        assert!(
            document["result"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line
                    .as_str()
                    .is_some_and(|line| line.contains("0 added, 1 updated, 0 missing"))),
            "{text}"
        );
    }
}

#[test]
fn readonly_snapshot_distinguishes_unchanged_time_noise_and_same_turn_additions() {
    let rows = baseline();
    let lab = Lab::new(&rows);
    assert!(lab.inspect(0).contains("no unsettled content"));
    lab.connection
        .execute_batch("UPDATE message SET time_updated=123; UPDATE part SET time_updated=456;")
        .unwrap();
    assert!(lab.inspect(0).contains("no unsettled content"));
    let mut more = rows;
    more.push(part(
        "extra-prompt",
        "user",
        12,
        json!({"type":"text","text":"another part of the same user message"}),
    ));
    more.push(part(
        "late-answer",
        "assistant",
        23,
        json!({"type":"text","text":"continuation"}),
    ));
    lab.replace_rows(&more);
    let text = lab.inspect(0);
    assert!(text.contains("2 added, 0 updated, 0 missing"), "{text}");
    assert!(
        text.contains("1 user turns with pending activity (0 newly started), 0 new ToolUse"),
        "{text}"
    );
}

#[test]
fn tool_completion_and_streaming_text_are_updates_without_new_calls_or_turns() {
    let mut rows = baseline();
    let lab = Lab::new(&rows);
    rows[3]["data"]["text"] = json!("saved answer with streamed continuation");
    rows[4]["data"]["state"] =
        json!({"status":"completed","input":{},"output":"done","metadata":{"exit":0}});
    lab.replace_rows(&rows);
    let text = lab.inspect(0);
    assert!(text.contains("0 added, 2 updated, 0 missing"), "{text}");
    assert!(
        text.contains("1 user turns with pending activity (0 newly started), 0 new ToolUse"),
        "{text}"
    );
}

#[test]
fn native_parent_chains_attribute_activity_without_recounting_prompt_parts() {
    let mut rows = baseline();
    let lab = Lab::new(&rows);
    rows.extend([
        message("next-user", 30, json!({"role":"user"})),
        part(
            "next-a",
            "next-user",
            31,
            json!({"type":"text","text":"first part"}),
        ),
        part(
            "next-b",
            "next-user",
            32,
            json!({"type":"text","text":"second part"}),
        ),
        message(
            "next-assistant",
            40,
            json!({"role":"assistant","parentID":"next-user"}),
        ),
        message(
            "next-step",
            50,
            json!({"role":"assistant","parentID":"next-assistant"}),
        ),
        part(
            "next-tool",
            "next-step",
            51,
            json!({"type":"tool","tool":"bash","callID":"next-call","state":{"status":"running"}}),
        ),
    ]);
    lab.replace_rows(&rows);
    let text = lab.inspect(0);
    assert!(
        text.contains("1 user turns with pending activity (1 newly started), 1 new ToolUse"),
        "{text}"
    );
    assert!(!text.contains("(2 newly started)"), "{text}");
}

#[test]
fn missing_and_cyclic_parent_edges_keep_activity_visible_without_guessing_a_turn() {
    for parents in [["absent", "absent"], ["second", "first"]] {
        let mut rows = baseline();
        let lab = Lab::new(&rows);
        rows.extend([
            message(
                "first",
                30,
                json!({"role":"assistant","parentID":parents[0]}),
            ),
            message(
                "second",
                40,
                json!({"role":"assistant","parentID":parents[1]}),
            ),
            part(
                "unattributed",
                "first",
                41,
                json!({"type":"text","text":"work with no proven user host"}),
            ),
        ]);
        lab.replace_rows(&rows);
        let text = lab.inspect(0);
        assert!(text.contains("3 added, 0 updated, 0 missing"), "{text}");
        assert!(
            text.contains("0 user turns with pending activity"),
            "{text}"
        );
        assert!(text.contains("lower bounds"), "{text}");
        assert!(!text.contains("no unsettled"), "{text}");
    }
}

#[test]
fn compaction_updates_follow_summary_dependencies_on_an_unchanged_boundary() {
    let mut rows = baseline();
    rows.extend([
        message("boundary", 30, json!({"role":"user"})),
        part(
            "compact",
            "boundary",
            31,
            json!({"type":"compaction","auto":true}),
        ),
        message(
            "summary",
            40,
            json!({"role":"assistant","mode":"compaction","parentID":"boundary"}),
        ),
    ]);
    let lab = Lab::new(&rows);
    rows.push(part(
        "summary-text",
        "summary",
        41,
        json!({"type":"text","text":"summary text"}),
    ));
    lab.replace_rows(&rows);
    let text = lab.inspect(0);
    assert!(text.contains("1 added, 0 updated, 0 missing"), "{text}");
    assert!(
        text.contains("(0 newly started), 0 new ToolUse calls, 1 changed compactions"),
        "{text}"
    );
    lab.save_log(&canonical(&rows));
    rows.last_mut().unwrap()["data"]["text"] = json!("updated summary text");
    lab.replace_rows(&rows);
    let text = lab.inspect(0);
    assert!(text.contains("0 added, 1 updated, 0 missing"), "{text}");
    assert!(text.contains("1 changed compactions"), "{text}");
}

#[test]
fn missing_saved_rows_and_header_only_updates_never_report_a_clean_snapshot() {
    let mut rows = baseline();
    let lab = Lab::new(&rows);
    rows.remove(3);
    rows[2]["data"]["finish"] = json!("stop");
    lab.replace_rows(&rows);
    let text = lab.inspect(0);
    assert!(text.contains("0 added, 1 updated, 1 missing"), "{text}");
    assert!(text.contains("lower bounds"), "{text}");
    assert!(!text.contains("no unsettled"), "{text}");
}

#[test]
fn materialized_claim_requires_the_actual_reminted_prefix_and_original_tip() {
    let mut rows = baseline();
    let lab = Lab::new(&rows);
    let mut saved = baseline();
    for row in &mut saved {
        row["id"] = json!(format!("saved-{}", row["id"].as_str().unwrap()));
        if let Some(host) = row["message_id"].as_str() {
            row["message_id"] = json!(format!("saved-{host}"));
        }
        if let Some(parent) = row["data"]["parentID"].as_str() {
            row["data"]["parentID"] = json!(format!("saved-{parent}"));
        }
    }
    lab.save_log(&canonical(&saved).replace(SID, "ses_saved_original"));
    let mut claim = lab.claim.clone();
    claim.materialized_from = Some(lab.git(&["rev-parse", "topic"]).trim().into());
    claim.baseline_bytes = Some(lab.original.len() as u64);
    claim.baseline_hash = Some(hex::encode(Sha256::digest(lab.original.as_bytes())));
    lab.save_claim(&claim);
    assert!(lab.inspect(0).contains("no unsettled content"));
    rows.push(part(
        "new",
        "assistant",
        30,
        json!({"type":"text","text":"new content"}),
    ));
    lab.replace_rows(&rows);
    assert!(lab.inspect(0).contains("1 added, 0 updated, 0 missing"));
    rows[3]["data"]["text"] = json!("changed reminted history");
    lab.replace_rows(&rows);
    let text = lab.inspect(4);
    assert!(text.contains("cannot be reconstructed"), "{text}");
    lab.replace_rows(&baseline());
    claim.materialized_from = Some("b".repeat(40));
    lab.save_claim(&claim);
    assert!(lab.inspect(4).contains("selected branch has advanced"));
}

#[test]
fn new_tool_parts_with_saved_call_identity_do_not_invent_another_call() {
    let mut rows = baseline();
    let lab = Lab::new(&rows);
    rows.push(part("receipt", "assistant", 30, json!({"type":"tool","tool":"bash","callID":"call-original","state":{"status":"completed"}})));
    lab.replace_rows(&rows);
    let text = lab.inspect(0);
    assert!(text.contains("1 added, 0 updated, 0 missing"), "{text}");
    assert!(text.contains("0 new ToolUse calls"), "{text}");
}

#[test]
fn exact_numeric_changes_and_unmodeled_parts_remain_visible() {
    let mut rows = baseline();
    rows[4]["data"]["future"] = json!(1.0);
    let lab = Lab::new(&rows);
    let raw = rows[4]["data"]
        .to_string()
        .replace("\"future\":1.0", "\"future\":1.00000000000000001");
    assert_ne!(raw, rows[4]["data"].to_string());
    lab.connection
        .execute("UPDATE part SET data=?1 WHERE id='tool'", [&raw])
        .unwrap();
    let text = lab.inspect(0);
    assert!(text.contains("0 added, 1 updated, 0 missing"), "{text}");
    rows.push(part(
        "future",
        "assistant",
        30,
        json!({"type":"future-activity","payload":"unmodeled"}),
    ));
    lab.replace_rows(&rows);
    let text = lab.inspect(0);
    assert!(text.contains("1 added, 0 updated, 0 missing"), "{text}");
    assert!(text.contains("lower bounds"), "{text}");
}

#[test]
fn changed_parent_binding_does_not_assign_updates_to_a_guessed_user_turn() {
    let mut rows = baseline();
    rows.extend([
        message("other-user", 30, json!({"role":"user"})),
        part(
            "other-prompt",
            "other-user",
            31,
            json!({"type":"text","text":"another request"}),
        ),
    ]);
    let lab = Lab::new(&rows);
    rows[2]["data"]["parentID"] = json!("other-user");
    rows[3]["data"]["text"] = json!("updated after parent reassignment");
    lab.replace_rows(&rows);
    let text = lab.inspect(0);
    assert!(text.contains("0 added, 2 updated, 0 missing"), "{text}");
    assert!(
        text.contains("0 user turns with pending activity"),
        "{text}"
    );
    assert!(text.contains("lower bounds"), "{text}");
}

#[test]
fn native_identity_changes_and_ambiguous_json_are_refused() {
    for sql in [
        "UPDATE part SET message_id='user' WHERE id='tool'",
        "UPDATE message SET time_created=99 WHERE id='assistant'",
        "UPDATE part SET data='{\"type\":\"text\",\"type\":\"tool\"}' WHERE id='tool'",
    ] {
        let lab = Lab::new(&baseline());
        lab.connection.execute_batch(sql).unwrap();
        let text = lab.inspect(4);
        assert!(text.contains("unavailable"), "{text}");
        assert!(!text.contains("no unsettled"), "{text}");
    }
}

#[test]
fn repeated_saved_occurrences_keep_latest_data_but_cannot_rebind_identity() {
    let rows = baseline();
    let lab = Lab::new(&rows);
    let tool = canonical(&[rows[4].clone()])
        .lines()
        .last()
        .unwrap()
        .to_owned();
    let old = tool.replace("running", "pending");
    lab.save_log(&format!("{}\n{old}\n{tool}\n", lab.original.trim_end()));
    assert!(lab.inspect(0).contains("no unsettled content"));
    let rebound = tool.replace("\"time_created\":22", "\"time_created\":23");
    lab.save_log(&format!("{}{rebound}\n", lab.original));
    assert!(lab.inspect(4).contains("identity"));
}

#[test]
fn repeated_message_occurrences_cannot_hide_parent_or_role_identity_changes() {
    for changed in [
        json!({"role":"assistant","parentID":"another-user"}),
        json!({"role":"user","parentID":"user"}),
    ] {
        let rows = baseline();
        let lab = Lab::new(&rows);
        let original = canonical(&[rows[2].clone()])
            .lines()
            .last()
            .unwrap()
            .to_owned();
        let changed = canonical(&[message("assistant", 20, changed)])
            .lines()
            .last()
            .unwrap()
            .to_owned();
        lab.save_log(&format!("{}{changed}\n{original}\n", lab.original));
        let text = lab.inspect(4);
        assert!(
            text.contains("repeats or changes a native identity"),
            "{text}"
        );
    }
}

#[test]
fn first_prompt_parts_on_an_existing_empty_user_header_start_one_observed_turn() {
    let mut rows = baseline();
    rows.push(message("empty-user", 30, json!({"role":"user"})));
    let lab = Lab::new(&rows);
    rows.extend([
        part(
            "first",
            "empty-user",
            31,
            json!({"type":"text","text":"first part"}),
        ),
        part(
            "second",
            "empty-user",
            32,
            json!({"type":"text","text":"second part"}),
        ),
    ]);
    lab.replace_rows(&rows);
    let text = lab.inspect(0);
    assert!(text.contains("2 added, 0 updated, 0 missing"), "{text}");
    assert!(
        text.contains("1 user turns with pending activity (1 newly started)"),
        "{text}"
    );
}

#[test]
fn role_reassignment_cannot_invent_a_new_user_turn_from_an_added_text_part() {
    let mut rows = baseline();
    let lab = Lab::new(&rows);
    rows[2]["data"]["role"] = json!("user");
    rows.push(part(
        "invented-prompt",
        "assistant",
        30,
        json!({"type":"text","text":"the host was previously an assistant"}),
    ));
    lab.replace_rows(&rows);
    let text = lab.inspect(0);
    assert!(text.contains("1 added, 1 updated, 0 missing"), "{text}");
    assert!(
        text.contains("0 user turns with pending activity (0 newly started)"),
        "{text}"
    );
    assert!(text.contains("lower bounds"), "{text}");
}
