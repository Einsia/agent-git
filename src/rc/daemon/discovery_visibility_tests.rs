use super::*;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Discovery must filter native children before its result limit, while explicit inventories
/// retain them. Isolated processes keep runtime homes and executable discovery independent.
#[test]
fn user_session_discovery_preserves_parents_across_native_stores() {
    const CASE: &str = "AGIT_DISCOVERY_VISIBILITY_CASE";
    let Ok(case) = std::env::var(CASE) else {
        for case in [
            "codex",
            "codex-old-index",
            "codex-files",
            "codex-incompatible-index",
            "claude",
            "opencode",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let bin = root.join("bin");
            std::fs::create_dir(&bin).unwrap();
            for name in ["codex", "claude", "opencode"] {
                let executable = bin.join(name);
                std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
                std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "rc::daemon::sessions::discovery_visibility_tests::user_session_discovery_preserves_parents_across_native_stores", "--nocapture"])
                .env(CASE, case)
                .env("HOME", root)
                .env("AGIT_HOME", root.join("agit"))
                .env("CODEX_HOME", root.join("codex"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("PATH", &bin)
                .output().unwrap();
            assert!(output.status.success(), "{case}: {output:?}");
        }
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let project = root.join("project");
    std::fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let (runtime, expected) = match case.as_str() {
        "claude" => ("claude-code", claude_fixture(&project)),
        "opencode" => ("opencode", opencode_fixture(&root, &project)),
        _ => ("codex", codex_fixture(&root, &project, &case)),
    };
    let adapter = crate::adapter::get(runtime).unwrap();
    let refs = adapter.session_choices_for(&project).unwrap();
    assert_eq!(
        ids(refs.iter().map(|session| session.id.as_str())),
        expected
    );
    let all = adapter.sessions_for(&project).unwrap();
    assert!(
        all.len() > refs.len(),
        "explicit inventory must retain internal sessions"
    );
    assert!(adapter.all_sessions().unwrap().len() >= all.len());
    for session in &refs {
        if runtime == "opencode" {
            assert!(
                !session.path.exists(),
                "listing must not materialize native history"
            );
        }
    }
    let internal = all
        .iter()
        .find(|session| !expected.contains(&session.id))
        .unwrap();
    assert!(
        adapter.resolve(&internal.id, Some(&project)).is_some(),
        "explicit internal lookup remains available"
    );

    let snapshot = LocalSessionSnapshot {
        roots: policy::CanonicalRoots::from_untrusted([project]),
        supervised: Default::default(),
    };
    for purpose in [LocalSessionScan::Listing, LocalSessionScan::Locate] {
        let local = snapshot.clone().scan(purpose);
        if matches!(purpose, LocalSessionScan::Listing) {
            assert_eq!(
                ids(local
                    .iter()
                    .map(|session| session.runtime_session_id.as_str())),
                expected
            );
        } else {
            assert!(
                local
                    .iter()
                    .any(|session| !expected.contains(&session.runtime_session_id)),
                "explicit RC lookup retains internal sessions"
            );
        }
        assert!(local.iter().all(|session| session.runtime == runtime));
    }
}

fn ids<'a>(values: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    values.into_iter().map(str::to_owned).collect()
}

fn codex_fixture(root: &Path, project: &Path, case: &str) -> BTreeSet<String> {
    let home = root.join("codex");
    let rollouts = home.join("sessions/2026/09/14");
    std::fs::create_dir_all(&rollouts).unwrap();
    let database = Connection::open(home.join("state_5.sqlite")).unwrap();
    database
        .execute_batch(
            "CREATE TABLE threads (
        id TEXT, rollout_path TEXT, cwd TEXT, first_user_message TEXT,
        thread_source TEXT, updated_at_ms INTEGER, archived INTEGER, source TEXT
    )",
        )
        .unwrap();
    let mut rows = vec![
        ("user".to_owned(), Some("user"), Some(json!("vscode")), true),
        ("legacy".to_owned(), None, None, true),
        (
            "future".to_owned(),
            Some("future_user_origin"),
            Some(json!("cli")),
            true,
        ),
        ("guardian".to_owned(), Some("guardian_review"), None, false),
        (
            "source-only".to_owned(),
            None,
            Some(json!({"subagent":{"other":"guardian"}})),
            false,
        ),
        (
            "source-overrides-user".to_owned(),
            Some("user"),
            Some(json!({"subagent":{"other":"guardian"}})),
            false,
        ),
        (
            "string-source".to_owned(),
            None,
            Some(json!("subagent")),
            false,
        ),
    ];
    for index in 0..=PER_PROJECT_LIMIT {
        rows.push((
            format!("child-{index}"),
            Some("subagent"),
            Some(json!({"subagent":{"thread_spawn":{"parent_thread_id":"user","depth":1}}})),
            false,
        ));
    }
    let mut expected = BTreeSet::new();
    for (_label, thread_source, source, visible) in rows {
        let id = uuid::Uuid::new_v4().to_string();
        let path = rollouts.join(format!("rollout-2026-09-14T00-00-00-{id}.jsonl"));
        let mut payload = json!({"id":id,"cwd":project});
        if let Some(kind) = thread_source {
            payload["thread_source"] = json!(kind);
        }
        if let Some(source) = &source {
            payload["source"] = source.clone();
        }
        std::fs::write(
            &path,
            format!("{}\n", json!({"type":"session_meta","payload":payload})),
        )
        .unwrap();
        if visible {
            expected.insert(id.clone());
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
                .unwrap();
        }
        database
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, ?3, 'preview', ?4, ?5, 0, ?6)",
                rusqlite::params![
                    id,
                    path.to_str(),
                    project.to_str(),
                    thread_source,
                    if visible { 1 } else { 1000 },
                    source.map(|value| value.to_string())
                ],
            )
            .unwrap();
    }
    if case == "codex-old-index" {
        database
            .execute_batch("ALTER TABLE threads DROP COLUMN source")
            .unwrap();
    }
    if case == "codex-incompatible-index" {
        database
            .execute_batch("ALTER TABLE threads DROP COLUMN thread_source")
            .unwrap();
    }
    drop(database);
    if case == "codex-files" {
        std::fs::remove_file(home.join("state_5.sqlite")).unwrap();
    }
    expected
}

fn claude_fixture(project: &Path) -> BTreeSet<String> {
    let directory = crate::adapter::claude_code::projects_dir()
        .unwrap()
        .join(crate::adapter::claude_code::slug_for(project));
    std::fs::create_dir_all(directory.join("user/subagents")).unwrap();
    for (name, metadata) in [
        ("user", json!({"isSidechain":false})),
        ("legacy", json!({})),
        ("sidechain", json!({"isSidechain":true})),
        ("agent-legacy", json!({})),
    ] {
        let record = json!({"type":"user","message":{"role":"user","content":"fixture"},"sessionId":name,"parentUuid":"previous-message"});
        let mut record = record.as_object().unwrap().clone();
        record.extend(metadata.as_object().unwrap().clone());
        std::fs::write(
            directory.join(format!("{name}.jsonl")),
            format!(
                "{{\"type\":\"file-history-snapshot\"}}\n{}\n",
                Value::Object(record)
            ),
        )
        .unwrap();
    }
    std::fs::write(
        directory.join("user/subagents/agent-nested.jsonl"),
        "{\"isSidechain\":true}\n",
    )
    .unwrap();
    ids(["user", "legacy"])
}

fn opencode_fixture(root: &Path, project: &Path) -> BTreeSet<String> {
    let directory = root.join("data/opencode");
    std::fs::create_dir_all(&directory).unwrap();
    let database = Connection::open(directory.join("opencode.db")).unwrap();
    database.execute_batch("CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT);
        CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT,
            directory TEXT, time_created INTEGER, time_updated INTEGER, version TEXT);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);").unwrap();
    database
        .execute(
            "INSERT INTO project VALUES ('project', ?1)",
            [project.to_str()],
        )
        .unwrap();
    for (id, parent, cwd) in [
        ("ses_root", None, project.to_path_buf()),
        ("ses_empty_parent", Some(""), project.to_path_buf()),
        ("ses_worktree", None, project.join("subdirectory")),
        ("ses_child", Some("ses_root"), project.to_path_buf()),
        (
            "ses_child_worktree",
            Some("ses_root"),
            project.join("subdirectory"),
        ),
    ] {
        database
            .execute(
                "INSERT INTO session VALUES (?1, 'project', ?2, ?3, 1, 2, 'fixture')",
                rusqlite::params![id, parent, cwd.to_str()],
            )
            .unwrap();
    }
    ids(["ses_root", "ses_empty_parent", "ses_worktree"])
}
