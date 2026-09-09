//! Turn activity uses frozen stored records, including evidence hidden from the current VIEW.

use agit::domain::{meta, repo::Repo, storage, transcript};
use serde_json::json;
use std::{fs, path::Path, process::Command};

fn raw(source: &str, records: &[serde_json::Value], session: &str) -> String {
    let text: String = records.iter().map(|r| format!("{r}\n")).collect();
    transcript::wrap_lines(&text, source, session)
}

fn claude(session: &str) -> String {
    raw(
        "claude-desktop",
        &[
            json!({"type":"user","message":{"role":"user","content":"FIRST"}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[
                {"type":"tool_use","id":"a","name":"Bash","input":{}},
                {"type":"tool_use","id":"b","name":"Bash","input":{}}
            ]}}),
            json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"a","content":"A"},
                {"type":"tool_result","tool_use_id":"b","content":"B"}
            ]}}),
            json!({"type":"assistant","message":{"role":"assistant","content":"done"}}),
        ],
        session,
    )
}

fn codex(session: &str) -> String {
    raw(
        "codex",
        &[
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"SECOND"}]}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"a","arguments":"{}"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"a","output":"A"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"b","input":"patch"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"b","output":"B"}}),
            json!({"type":"event_msg","payload":{"type":"patch_apply_end","changes":{"a.txt":{"type":"update"}}}}),
            json!({"type":"event_msg","payload":{"type":"task_complete"}}),
        ],
        session,
    )
}

fn snapshot(repo: &Repo, log: &str, view: &str, turn: u32, kind: meta::Kind, subject: &str) {
    let mut m = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "codex".into(),
        "/fixture".into(),
    );
    m.kind = kind;
    m.turn = Some(turn);
    meta::write(repo.root(), &m).unwrap();
    storage::write_snapshot(repo.root(), log, view).unwrap();
    repo.add_all().unwrap();
    assert!(repo.commit(subject).unwrap());
}

fn log(home: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(["log", "me/counts@main", "--oneline"])
        .args(extra)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("AGIT_HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("CI", "1")
        .current_dir(home)
        .output()
        .unwrap()
}

#[test]
fn actual_log_counts_envelopes_and_calls_across_sources_and_filters() {
    let home = tempfile::tempdir().unwrap();
    let repo = Repo::init(&home.path().join("repos/me/counts")).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    let session = format!("agit-{}", "a".repeat(40));
    let first = claude(&session);
    snapshot(&repo, &first, &first, 1, meta::Kind::Turn, "FIRST");
    let second = format!("{first}{}{}", codex(&session), claude(&session));
    snapshot(&repo, &second, &second, 2, meta::Kind::Turn, "SECOND");
    snapshot(&repo, &second, "", 2, meta::Kind::View, "HIDDEN");
    snapshot(&repo, &second, "", 2, meta::Kind::File, "FILES");
    snapshot(&repo, &second, "", 2, meta::Kind::Merge, "MERGED");

    for filters in [vec![], vec!["--kind", "turn"], vec!["--since", "9999w"]] {
        let output = log(home.path(), &filters);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("[turn ] 4 events 2 ToolUse FIRST"), "{text}");
        assert!(
            text.contains("[turn ] 11 events 4 ToolUse SECOND"),
            "{text}"
        );
        for line in text.lines().filter(|l| !l.contains("[turn ]")) {
            assert!(
                !line.contains("events") && !line.contains("ToolUse"),
                "{line}"
            );
        }
    }
    let filtered = log(home.path(), &["--grep", "SECOND", "-n", "1"]);
    assert!(filtered.status.success());
    let text = String::from_utf8(filtered.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(
        text.contains("#  2") && text.contains("11 events 4 ToolUse"),
        "{text}"
    );
}

#[test]
fn missing_log_evidence_is_an_error_and_ordinary_git_counts_are_unknown() {
    let home = tempfile::tempdir().unwrap();
    let repo = Repo::init(&home.path().join("repos/me/counts")).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    fs::write(repo.root().join("README.md"), "plain Git\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("PLAIN").unwrap();
    let output = log(home.path(), &[]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("events ? ToolUse ? PLAIN"));

    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    repo.add_all().unwrap();
    repo.commit("FILE LINE").unwrap();
    let session = format!("agit-{}", "a".repeat(40));
    let mut m = meta::Meta::new(session, "codex".into(), "/fixture".into());
    m.turn = Some(1);
    meta::write(repo.root(), &m).unwrap();
    repo.add_all().unwrap();
    repo.commit("MISSING").unwrap();
    let output = log(home.path(), &["--grep", "MISSING"]);
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("0 events"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot read this branch's history"));
}

fn native_file_calls(source: &str, sibling: bool) -> Vec<serde_json::Value> {
    if source == "opencode" {
        let mut records = vec![
            json!({"kind":"opencode.meta","id":"native","directory":"/fixture"}),
            json!({"kind":"message","id":"message","session_id":"native","time_created":1,"data":{"role":"assistant"}}),
            json!({"kind":"part","id":"edit","message_id":"message","session_id":"native","time_created":2,"data":{
                "type":"tool","tool":"edit","callID":"edit","state":{"status":"completed","input":{"filePath":"/fixture/file"},"output":"done"}
            }}),
        ];
        if sibling {
            records.push(json!({"kind":"part","id":"bash","message_id":"message","session_id":"native","time_created":3,"data":{
                "type":"tool","tool":"bash","callID":"bash","state":{"status":"completed","input":{"command":"true"},"output":"done"}
            }}));
        }
        return records;
    }
    let mut blocks = if source == "cursor" {
        vec![json!({"type":"tool_use","name":"StrReplace","input":{"path":"/fixture/file"}})]
    } else {
        vec![
            json!({"type":"tool_use","id":"read","name":"Read","input":{"file_path":"/fixture/file"}}),
            json!({"type":"tool_use","id":"edit","name":"Edit","input":{"file_path":"/fixture/file"}}),
        ]
    };
    if sibling {
        blocks
            .push(json!({"type":"tool_use","id":"bash","name":"Bash","input":{"command":"true"}}));
    }
    vec![
        json!({"type":"assistant","role":"assistant","message":{"role":"assistant","content":blocks}}),
    ]
}

#[test]
fn log_counts_the_ir_category_without_dropping_sibling_tool_use_events() {
    for source in ["claude-code", "claude-desktop", "cursor", "opencode"] {
        let home = tempfile::tempdir().unwrap();
        let repo = Repo::init(&home.path().join("repos/me/counts")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let session = format!("agit-{}", "a".repeat(40));
        let mut history = String::new();
        for (index, sibling) in [false, true].into_iter().enumerate() {
            let records = native_file_calls(source, sibling);
            let native: String = records.iter().map(|record| format!("{record}\n")).collect();
            let parsed = agit::adapter::get(source).unwrap().parse(&native).unwrap();
            assert!(
                parsed
                    .events
                    .iter()
                    .any(|e| e.kind == agit::adapter::EventKind::FileEdit),
                "{source}"
            );
            assert_eq!(parsed.counts().tools, usize::from(sibling), "{source}");
            history.push_str(&raw(source, &records, &session));
            snapshot(
                &repo,
                &history,
                &history,
                index as u32 + 1,
                meta::Kind::Turn,
                "FILE CALLS",
            );
            let output = log(home.path(), &["-n", "1"]);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(
                text.contains(&format!(
                    "{} events {} ToolUse",
                    records.len(),
                    usize::from(sibling)
                )),
                "{source}: {text}"
            );
        }
    }
}

#[test]
fn native_opencode_import_counts_a_late_part_on_an_earlier_turns_host() {
    for host in ["a1", "a2"] {
        let home = tempfile::tempdir().unwrap();
        let hub = "http://127.0.0.1:9";
        let key = agit::infra::config::hub_host_key(hub).unwrap();
        agit::infra::credentials::save_at(
            &home
                .path()
                .join("agit/credentials")
                .join(format!("{key}.json")),
            &agit::infra::credentials::HubCredential {
                username: "me".into(),
                email: None,
                hub: Some(hub.into()),
                access_token: "synthetic-token".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_token: "synthetic-token".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
        let data = home.path().join("data");
        fs::create_dir_all(data.join("opencode")).unwrap();
        let database = rusqlite::Connection::open(data.join("opencode/opencode.db")).unwrap();
        database.execute_batch(
            "CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);
             CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, directory TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, version TEXT NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);",
        ).unwrap();
        database
            .execute(
                "INSERT INTO project VALUES ('global', ?1)",
                [home.path().to_str().unwrap()],
            )
            .unwrap();
        database.execute("INSERT INTO session VALUES ('ses_counts', 'global', NULL, ?1, 100, 150, '1.18.13')", [home.path().to_str().unwrap()]).unwrap();
        for (id, created, body) in [
            ("u1", 110, json!({"role":"user", "time":{"created":110}})),
            (
                "a1",
                120,
                json!({"role":"assistant", "parentID":"u1", "finish":"stop", "time":{"created":120}}),
            ),
            ("u2", 130, json!({"role":"user", "time":{"created":130}})),
            (
                "a2",
                140,
                json!({"role":"assistant", "parentID":"u2", "finish":"stop", "time":{"created":140}}),
            ),
        ] {
            database
                .execute(
                    "INSERT INTO message VALUES (?1, 'ses_counts', ?2, ?3)",
                    rusqlite::params![id, created, body.to_string()],
                )
                .unwrap();
        }
        for (id, message, created, body) in [
            ("u1text", "u1", 111, json!({"type":"text", "text":"FIRST"})),
            (
                "a1text",
                "a1",
                121,
                json!({"type":"text", "text":"Starting work"}),
            ),
            ("u2text", "u2", 131, json!({"type":"text", "text":"SECOND"})),
            (
                "bash",
                host,
                if host == "a1" { 132 } else { 142 },
                json!({"type":"tool", "tool":"bash", "callID":"late-bash", "state":{"status":"completed", "input":{"command":"true"}, "output":"done"}}),
            ),
            ("a2text", "a2", 143, json!({"type":"text", "text":"Done"})),
        ] {
            database
                .execute(
                    "INSERT INTO part VALUES (?1, ?2, 'ses_counts', ?3, ?4)",
                    rusqlite::params![id, message, created, body.to_string()],
                )
                .unwrap();
        }
        drop(database);
        let run = |args: &[&str]| {
            Command::new(env!("CARGO_BIN_EXE_agit"))
                .args(args)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", home.path())
                .env("AGIT_HOME", home.path().join("agit"))
                .env("AGIT_HUB_URL", hub)
                .env("XDG_DATA_HOME", &data)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("CI", "1")
                .current_dir(home.path())
                .output()
                .unwrap()
        };
        let imported = run(&[
            "import",
            "ses_counts",
            "--from",
            "opencode",
            "--into",
            "me/counts@work",
            "--independent",
        ]);
        assert!(
            imported.status.success(),
            "{}",
            String::from_utf8_lossy(&imported.stderr)
        );
        for filters in [&[][..], &["--grep", "SECOND", "-n", "1"][..]] {
            let mut args = vec!["log", "me/counts@work", "--oneline"];
            args.extend_from_slice(filters);
            let output = run(&args);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.contains("5 events 1 ToolUse SECOND"), "{host}: {text}");
            if filters.is_empty() {
                assert!(text.contains("5 events 0 ToolUse FIRST"), "{host}: {text}");
            }
        }
        if host == "a1" {
            let raw = run(&["show", "me/counts@work#2.3", "--raw"]);
            assert!(raw.status.success());
            let selected: serde_json::Value = serde_json::from_slice(&raw.stdout).unwrap();
            assert_eq!(selected["id"], "bash");
            assert_eq!(selected["message_id"], "a1");
        }
    }
}
