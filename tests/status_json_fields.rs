use std::{fs, path::PathBuf, process::Command};

struct Lab {
    _temp: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    cwd: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let store = temp.path().join("agit");
        let cwd = temp.path().join("work");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        Self {
            _temp: temp,
            home,
            store,
            cwd,
        }
    }

    fn output(&self, args: &[&str]) -> std::process::Output {
        let output = Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(&self.cwd)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("AGIT_YES", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn run(&self, args: &[&str]) -> serde_json::Value {
        serde_json::from_slice(&self.output(args).stdout).unwrap()
    }
}

fn native_sessions(lab: &Lab, id: &str) {
    let cwd = lab.cwd.canonicalize().unwrap();
    let claude = lab
        .home
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&cwd));
    fs::create_dir_all(&claude).unwrap();
    fs::write(claude.join(format!("{id}.jsonl")), format!("{}\n", serde_json::json!({
        "type":"user", "sessionId":id, "cwd":cwd, "uuid":"prompt",
        "timestamp":"2026-09-08T00:00:00Z", "message":{"role":"user","content":"Continue this conversation"},
    }))).unwrap();
    let codex = lab.home.join(".codex/sessions/2026/09/08");
    fs::create_dir_all(&codex).unwrap();
    fs::write(
        codex.join(format!("rollout-2026-09-08T00-00-00-{id}.jsonl")),
        format!(
            "{}\n",
            serde_json::json!({
                "type":"session_meta", "timestamp":"2026-09-08T00:00:00Z", "payload":{
                    "id":id, "cwd":cwd, "timestamp":"2026-09-08T00:00:00Z",
                },
            })
        ),
    )
    .unwrap();
}

#[test]
fn explicit_discovery_works_before_first_adoption_and_distinguishes_runtimes() {
    let lab = Lab::new();
    let id = "cccccccc-0000-4000-8000-000000000003";
    native_sessions(&lab, id);
    let text = lab.output(&["status", "--check-missing"]);
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("cccccccc-000"), "{text}");
    assert!(!lab.store.exists());
    let first = lab.run(&["status", "--check-missing", "--json"]);
    let items = first["result"]["value"]["unadopted"]["sessions"]
        .as_array()
        .unwrap();
    assert!(
        items
            .iter()
            .any(|item| item["runtime"] == "claude-code" && item["session_id"] == id),
        "{first}"
    );
    assert!(
        items
            .iter()
            .any(|item| item["runtime"] == "codex" && item["session_id"] == id),
        "{first}"
    );
    assert!(!lab.store.exists());

    let links = lab.store.join("store/claude-code");
    fs::create_dir_all(&links).unwrap();
    fs::write(
        links.join(format!("{id}.json")),
        serde_json::json!({"cwd":lab.cwd}).to_string(),
    )
    .unwrap();
    let next = lab.run(&["status", "--check-missing", "--json"]);
    let items = next["result"]["value"]["unadopted"]["sessions"]
        .as_array()
        .unwrap();
    assert!(
        !items
            .iter()
            .any(|item| item["runtime"] == "claude-code" && item["session_id"] == id)
    );
    assert!(
        items
            .iter()
            .any(|item| item["runtime"] == "codex" && item["session_id"] == id),
        "{next}"
    );
}

#[test]
fn empty_status_has_typed_absence_and_does_not_initialize_storage() {
    let lab = Lab::new();
    let document = lab.run(&["--json", "status"]);
    assert_eq!(document["result"]["format"], "json");
    let status = &document["result"]["value"];
    assert_eq!(status["schema_version"], 1);
    assert!(status["selection"]["repo"].is_null());
    assert!(status["store_path"].is_null());
    assert_eq!(status["sessions"]["total"], 0);
    assert_eq!(status["sessions"]["items"], serde_json::json!([]));
    assert_eq!(status["unadopted"]["checked"], false);
    assert!(status["unadopted"]["sessions"].is_null());
    assert!(!lab.store.exists());
}

#[test]
fn session_pages_preserve_runtime_identity_namespaces_and_supersession() {
    let lab = Lab::new();
    lab.run(&["--json", "init", "demo"]);
    for (runtime, id, superseded) in [
        ("claude-code", "aaaaaaaa-0000-4000-8000-000000000001", None),
        ("codex", "aaaaaaaa-0000-4000-8000-000000000001", None),
        (
            "claude-code",
            "bbbbbbbb-0000-4000-8000-000000000002",
            Some("codex/replacement"),
        ),
    ] {
        let dir = lab.store.join("store").join(runtime);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("{id}.json")),
            serde_json::json!({
                "owner": "organization", "agent": "demo", "branch": "topic/work",
                "cwd": lab.cwd, "superseded_by": superseded,
            })
            .to_string(),
        )
        .unwrap();
    }
    let first = lab.run(&["status", "--json", "--limit", "1"]);
    let first = &first["result"]["value"];
    assert_eq!(first["bound_repo"], "local/demo");
    assert!(first["selection"]["repo"].is_null());
    assert_eq!(first["sessions"]["total"], 3);
    assert_eq!(first["sessions"]["next_offset"], 1);
    assert_eq!(
        first["sessions"]["items"][0]["target"],
        "organization/demo@topic/work"
    );
    assert_eq!(
        first["sessions"]["items"][0]["session_id"],
        "aaaaaaaa-0000-4000-8000-000000000001"
    );
    let last = lab.run(&["status", "--json", "--limit", "2", "--offset", "1"]);
    let last = &last["result"]["value"]["sessions"];
    assert!(last["next_offset"].is_null());
    assert_eq!(last["items"][0]["runtime"], "codex");
    assert_eq!(last["items"][1]["active"], false);
    assert_eq!(last["items"][1]["superseded_by"], "codex/replacement");
}
