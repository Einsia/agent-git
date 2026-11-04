//! `agit scan` reports entropy-only values only when asked, and then only those in what push
//! publishes: a session's records as publication keeps them, files and commit messages. A record
//! publication leaves out is never read, and neither are the ids a record is chained by.

use agit::domain::{meta, repo::Repo, storage, transcript};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::{fs, process::Command};

const TARGET: &str = "alice/app@talk";
/// Only in a record publication leaves out.
const DROPPED: &str = "8e1d4b6a-2c9f-4a7e-b3d5-6f0c1a9e2b48";
/// In a published tool result.
const PUBLISHED: &str = "3f2a9c1e-5b7d-4e8f-a6c2-9d1b0e7f4a35";
/// The first record's own uuid, quoted again in a later tool result.
const QUOTED: &str = "c4b2e8f1-7a3d-4c6e-9f1b-2d8a5e0c7b93";
/// The session id every record but one carries.
const SESSION: &str = "5a7c9e1b-3d5f-4a6c-8e0b-2c4d6f8a0b1d";
/// A pasted value in the one record whose `sessionId` is not an id.
const NOT_AN_ID: &str = "Zq8Rk2Lm9Vx4Tb7Nw3Hj6Pc1";

/// An Agent repo whose `talk` session holds one runtime context record and three conversation
/// records; returns `AGIT_HOME`.
fn fixture(tmp: &Path) -> PathBuf {
    let home = tmp.join("agit");
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    for (key, value) in [
        ("commit.gpgsign", "false"),
        ("user.name", "agit"),
        ("user.email", "agit@localhost"),
    ] {
        repo.git(&["config", key, value]).unwrap();
    }
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();

    let records = [
        json!({"type":"user","uuid":QUOTED,"parentUuid":null,"sessionId":SESSION,
            "message":{"role":"user","content":"start the job"}}),
        json!({"type":"attachment","uuid":"0b9d7f5e-3c1a-4e8b-9d6f-4a2c0e8b6d4f","parentUuid":QUOTED,"sessionId":SESSION,
            "attachment":{"type":"instructions","path":"/work/CLAUDE.md","content":format!("attached note {DROPPED}")}}),
        json!({"type":"assistant","uuid":"e6f4d2b0-8a6c-4e2f-b0d8-6c4a2e0f8d6b","parentUuid":"0b9d7f5e-3c1a-4e8b-9d6f-4a2c0e8b6d4f","sessionId":SESSION,
            "requestId":"req_011CUnJ3pQn5xyZ8AbCdEfGh",
            "message":{"id":"msg_01XFDUDYJgAACzvnptvVoYEL","role":"assistant","content":[
                {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"jobs start"}}]}}),
        json!({"type":"user","uuid":"9a7b5c3d-1e0f-4a2b-8c6d-4e2f0a8b6c4d","parentUuid":"e6f4d2b0-8a6c-4e2f-b0d8-6c4a2e0f8d6b","sessionId":NOT_AN_ID,
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1",
                "content":format!("job {PUBLISHED} started after {QUOTED}")}]}}),
    ];
    let session = meta::Meta::new(
        format!("agit-{}", "c".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let text: String = records.iter().map(|record| format!("{record}\n")).collect();
    let log = transcript::wrap_lines(&text, "claude-code", &session.session);
    repo.git(&["checkout", "-B", "talk"]).unwrap();
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &session).unwrap();
    repo.add_all().unwrap();
    repo.commit("talk 4").unwrap();
    repo.git(&["checkout", "main"]).unwrap();
    home
}

fn scan(home: &Path, flags: &[&str]) -> (String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .args(["scan", TARGET, "--secrets"])
        .args(flags)
        .current_dir(home.parent().unwrap())
        .env("AGIT_HOME", home)
        .env("CI", "1")
        .env("AGIT_TUI", "0")
        .env(
            "AGIT_SECRETS_KEYSTORE",
            if cfg!(unix) { "file" } else { "os" },
        )
        .env_remove("AGIT_SESSION");
    for (name, _) in agit::infra::runtime_session::ENV_SESSIONS {
        command.env_remove(name);
    }
    let out = command.output().unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    assert!(out.status.success(), "{flags:?}\n{stdout}\n{stderr}");
    (stdout, stderr)
}

fn preview(value: &str) -> String {
    agit::domain::secrets::redact(value)
}

/// The report inside the JSON envelope, or the document itself when it is not wrapped.
fn report(stdout: &str) -> Value {
    let document: Value = serde_json::from_str(stdout).unwrap();
    document
        .pointer("/result/value")
        .cloned()
        .unwrap_or(document)
}

/// Every low-confidence finding in a `--json` report: shape, place, line and preview.
fn low_findings(stdout: &str) -> Vec<(String, String, u64, String)> {
    let low = report(stdout)["low_confidence"].clone();
    low["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| {
            let shape = group["shape"].as_str().unwrap().to_owned();
            group["findings"]
                .as_array()
                .unwrap()
                .iter()
                .map(move |finding| {
                    (
                        shape.clone(),
                        finding["at"].as_str().unwrap().to_owned(),
                        finding["line"].as_u64().unwrap(),
                        finding["preview"].as_str().unwrap().to_owned(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn without_the_flag_no_low_confidence_value_is_reported_or_counted() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());

    let (stdout, stderr) = scan(&home, &[]);
    let text = format!("{stdout}{stderr}");
    assert!(!text.contains("low-confidence"), "{text}");
    for value in [DROPPED, PUBLISHED, QUOTED, NOT_AN_ID] {
        assert!(!text.contains(&preview(value)), "{value}: {text}");
    }

    let (stdout, _) = scan(&home, &["--json"]);
    let report = report(&stdout);
    assert_eq!(report["check"], "secrets", "{report}");
    assert!(report.get("low_confidence").is_none(), "{report}");
    for value in [DROPPED, PUBLISHED, QUOTED, NOT_AN_ID] {
        assert!(!stdout.contains(&preview(value)), "{value}: {stdout}");
    }
}

#[test]
fn the_flag_lists_by_shape_only_what_publication_carries() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());

    let (stdout, _) = scan(&home, &["--low-confidence", "--json"]);
    let low = report(&stdout)["low_confidence"].clone();
    assert_eq!(low["count_exact"], true, "{low}");
    assert_eq!(low["files_scope"], "branch_tips", "{low}");
    let listed = |shape: &str| -> Vec<(String, u64, String)> {
        low["groups"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|group| group["shape"] == shape)
            .flat_map(|group| group["findings"].as_array().unwrap().clone())
            .map(|finding| {
                assert_eq!(finding["confidence"], "low", "{finding}");
                (
                    finding["at"].as_str().unwrap().to_owned(),
                    finding["line"].as_u64().unwrap(),
                    finding["preview"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let uuids = listed("uuid");
    let at = |value: &str| -> Vec<(String, u64)> {
        uuids
            .iter()
            .filter(|(_, _, shown)| *shown == preview(value))
            .map(|(at, line, _)| (at.clone(), *line))
            .collect()
    };
    // The tool result in the fourth LOG record is published; the attachment record is not, and
    // the records' own chaining ids are left out where the runtime writes them.
    assert_eq!(at(PUBLISHED), [("talk:LOG".to_owned(), 4)], "{low}");
    assert_eq!(at(QUOTED), [("talk:LOG".to_owned(), 4)], "{low}");
    assert!(at(DROPPED).is_empty(), "{low}");
    assert!(at(SESSION).is_empty(), "{low}");
    assert_eq!(uuids.len(), 2, "{low}");
    assert_eq!(
        listed("base64"),
        [("talk:LOG".to_owned(), 4, preview(NOT_AN_ID))],
        "{low}"
    );
    assert!(!stdout.contains(PUBLISHED) && !stdout.contains(NOT_AN_ID));

    let (stdout, _) = scan(&home, &["--low-confidence"]);
    assert!(
        stdout.contains("3 low-confidence values in what push publishes, by shape"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "\n· files are read as each branch holds them now, not their earlier versions\n  uuid: 2 values\n"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("  uuid: 2 values"), "{stdout}");
    assert!(stdout.contains("  base64 run: 1 value"), "{stdout}");
    assert!(
        stdout.contains(&format!("    talk:LOG:4  {}", preview(PUBLISHED))),
        "{stdout}"
    );
    assert!(!stdout.contains(&preview(DROPPED)), "{stdout}");
}

/// A file staged for the next commit is read as the index holds it, whether it is new or a
/// change the working tree has since undone; a working-tree link is never followed, whether it
/// is the file or a tracked directory above it, since Git publishes the link itself. A pass over
/// working-tree changes alone misses the first two, and one that follows links lists a value
/// from outside the repository.
#[test]
fn the_flag_reads_staged_files_and_never_follows_a_link() {
    const STAGED_NEW: &str = "6b8d0f2a-4c6e-4a8b-9d1f-3e5a7c9b1d2f";
    const STAGED_CHANGE: &str = "1c3e5a7b-9d1f-4b3d-8e5a-7c9b1d3f5a7e";
    const OUTSIDE: &str = "d2f4a6c8-0e2b-4d6f-a8c0-2e4b6d8f0a2c";
    const OUTSIDE_DIRECTORY: &str = "a4c6e8f0-2b4d-4f6a-8c0e-4b6d8f0a2c4e";
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let repo = Repo::open(home.join("repos/alice/app")).unwrap();
    fs::create_dir(repo.root().join("docs")).unwrap();
    fs::write(repo.root().join("docs/guide.md"), "Guide").unwrap();
    repo.git(&["add", "docs/guide.md"]).unwrap();
    repo.git(&["commit", "-q", "-m", "Guide"]).unwrap();
    fs::write(repo.root().join("notes.md"), format!("run {STAGED_NEW}\n")).unwrap();
    fs::write(
        repo.root().join("README.md"),
        format!("run {STAGED_CHANGE}\n"),
    )
    .unwrap();
    repo.git(&["add", "notes.md", "README.md"]).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    #[cfg(unix)]
    {
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, format!("run {OUTSIDE}\n")).unwrap();
        std::os::unix::fs::symlink(&outside, repo.root().join("link.txt")).unwrap();
        // The tracked directory becomes a link to a directory outside holding a same-named file.
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(
            elsewhere.join("guide.md"),
            format!("run {OUTSIDE_DIRECTORY}\n"),
        )
        .unwrap();
        fs::remove_dir_all(repo.root().join("docs")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, repo.root().join("docs")).unwrap();
    }

    let (stdout, _) = scan(&home, &["--low-confidence", "--json"]);
    let found = low_findings(&stdout);
    let at = |value: &str| -> Vec<String> {
        found
            .iter()
            .filter(|(_, _, _, shown)| *shown == preview(value))
            .map(|(_, at, line, _)| format!("{at}:{line}"))
            .collect()
    };
    assert_eq!(at(STAGED_NEW), ["notes.md:1"], "{found:?}");
    assert_eq!(at(STAGED_CHANGE), ["README.md:1"], "{found:?}");
    assert!(at(OUTSIDE).is_empty(), "{found:?}");
    assert!(at(OUTSIDE_DIRECTORY).is_empty(), "{found:?}");
}
