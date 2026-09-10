//! The installed agent guide must support adoption and explicit follow-up commands without
//! inheriting a session target or starting a nested runtime.

use agit::domain::{meta, repo::Repo, storage};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SESSION: &str = "11111111-0000-4000-8000-000000000001";
const TARGET: &str = "me/guide@review";

struct Lab {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    agit_home: PathBuf,
    codex_home: PathBuf,
    work: PathBuf,
    hub: String,
}

impl Lab {
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agit"));
        cmd.args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.agit_home)
            .env("CODEX_HOME", &self.codex_home)
            .env("AGIT_HUB_URL", &self.hub)
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.join("empty-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .current_dir(&self.work);
        #[cfg(windows)]
        {
            for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
                if let Some(value) = std::env::var_os(name) {
                    cmd.env(name, value);
                }
            }
            cmd.env("USERPROFILE", &self.home);
        }
        cmd
    }
}

fn document(output: Output) -> serde_json::Value {
    assert!(output.status.success(), "{output:?}");
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["ok"], true, "{document}");
    assert_eq!(document["exit_code"], 0, "{document}");
    document
}

fn native_turn(path: &Path, marker: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for record in [
        serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{"type":"input_text","text":marker}]}}),
        serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "content":[{"type":"output_text","text":"The requested phase is complete."}]}}),
        serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete"}}),
    ] {
        writeln!(file, "{record}").unwrap();
    }
}

#[test]
fn installed_skill_supports_explicit_adopt_commit_search_and_prepare() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let lab = Lab {
        home: tmp.path().join("home"),
        agit_home: tmp.path().join("agit"),
        codex_home: tmp.path().join("codex"),
        work: tmp.path().join("work"),
        _tmp: tmp,
        hub,
    };
    std::fs::create_dir_all(&lab.home).unwrap();
    std::fs::create_dir_all(&lab.work).unwrap();
    let repo = Repo::init(&lab.agit_home.join("repos/me/guide")).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    repo.add_all().unwrap();
    repo.commit("shared file line").unwrap();
    agit::infra::credentials::save_at(
        &lab.agit_home.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(&lab.hub).unwrap()
        )),
        &agit::infra::credentials::HubCredential {
            username: "me".into(),
            email: None,
            hub: Some(lab.hub.clone()),
            access_token: "synthetic-test-token".into(),
            refresh_token: "synthetic-test-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let directory = lab.codex_home.join("sessions/2026/09/08");
    std::fs::create_dir_all(&directory).unwrap();
    let source = directory.join(format!("rollout-2026-09-08T00-00-00-{SESSION}.jsonl"));
    std::fs::write(&source, format!("{}\n", serde_json::json!({
        "type":"session_meta", "payload":{"id":SESSION, "cwd":lab.work, "timestamp":"2026-09-08T00:00:00Z"}
    }))).unwrap();
    native_turn(&source, "SYNTHETIC-OPENING-TURN");

    document(
        lab.command(&[
            "setup",
            "--runtime",
            "codex",
            "--skill",
            "--agents-md",
            "--json",
        ])
        .output()
        .unwrap(),
    );
    let installed = lab.codex_home.join("skills/agit");
    assert_eq!(
        std::fs::read_to_string(installed.join("SKILL.md")).unwrap(),
        include_str!("../src/commands/setup_skill.md")
    );
    assert!(
        std::fs::read_to_string(lab.work.join("AGENTS.md"))
            .unwrap()
            .contains(include_str!("../src/commands/setup_agents_section.md").trim())
    );
    let references = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/subskills");
    for entry in std::fs::read_dir(references).unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|s| s.to_str()) == Some("md") {
            assert_eq!(
                std::fs::read(entry.path()).unwrap(),
                std::fs::read(
                    installed
                        .join("references/commands")
                        .join(entry.file_name())
                )
                .unwrap()
            );
        }
    }

    document(
        lab.command(&[
            "import",
            SESSION,
            "--from",
            "codex",
            "--into",
            TARGET,
            "--independent",
            "--json",
        ])
        .output()
        .unwrap(),
    );
    assert!(repo.has_ref("refs/heads/review"));
    let opening = repo.git(&["rev-parse", "refs/heads/review"]).unwrap();
    native_turn(&source, "SYNTHETIC-COMPLETED-PHASE");
    let missing_target = lab.command(&["commit", "--json"]).output().unwrap();
    assert!(!missing_target.status.success());
    assert_eq!(
        repo.git(&["rev-parse", "refs/heads/review"]).unwrap(),
        opening
    );
    document(
        lab.command(&["commit", TARGET, "--milestone", "Verified phase", "--json"])
            .output()
            .unwrap(),
    );
    let settled = repo.git(&["rev-parse", "refs/heads/review"]).unwrap();
    assert_ne!(settled, opening);
    let view = storage::materialize_at(repo.root(), "refs/heads/review", meta::VIEW_FILE).unwrap();
    assert!(view.contains("SYNTHETIC-OPENING-TURN"));
    assert!(view.contains("SYNTHETIC-COMPLETED-PHASE"));

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut bytes = [0; 4096];
            let size = stream.read(&mut bytes).unwrap();
            assert_ne!(size, 0);
            request.extend_from_slice(&bytes[..size]);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(
            request.starts_with("GET /api/search/sessions?"),
            "{request}"
        );
        assert!(
            request.contains("me%2Fguide") && request.contains("completed"),
            "{request}"
        );
        let body = serde_json::json!({"type":"sessions", "total":1, "page":1, "per":10,
            "items":[{"session_id":SESSION, "repo":"me/guide", "branch":"review"}]})
        .to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let search = document(
        lab.command(&["search", "completed", "--repo", "me/guide", "--json"])
            .output()
            .unwrap(),
    );
    server.join().unwrap();
    assert_eq!(search["result"]["value"]["hits"][0]["session_id"], SESSION);
    document(lab.command(&["view", TARGET, "--json"]).output().unwrap());
    document(
        lab.command(&["resume", TARGET, "--as", "codex", "--no-launch", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        repo.git(&["rev-parse", "refs/heads/review"]).unwrap(),
        settled
    );
    assert!(lab.agit_home.join("store/codex").is_dir());
}
