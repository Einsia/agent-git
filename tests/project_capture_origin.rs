//! A branch a project opens for a captured session records, in its own session metadata, the
//! conversation that opened it, so the project's capture rule applies to the branch whatever it is
//! named and on whichever machine it is read. From the CLI entry point this pins that the import a
//! project runs writes that record on the line's first settled turn, also when the capture began
//! before that turn was finished, that a later settlement keeps it, and that a line the user opens
//! (an import of their own, a fork of the capture) has none: such a line is not judged by the
//! opener of another.

use std::process::Command;
use std::{fs, io::Write as _};

const CAPTURED: &str = "aaaaaaaa-0000-4000-8000-000000000041";
const IMPORTED: &str = "bbbbbbbb-0000-4000-8000-000000000042";
const HUB: &str = "http://127.0.0.1:1";

struct Lab {
    _tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    agit_home: std::path::PathBuf,
    work: std::path::PathBuf,
}

impl Lab {
    fn new() -> Lab {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let agit_home = tmp.path().join("agit");
        let work = tmp.path().join("work");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&work).unwrap();
        // The author comes from the credentials; the Hub is unreachable, so nothing leaves.
        let cred = agit::infra::credentials::HubCredential {
            account_id: None,
            username: "me".into(),
            email: None,
            hub: Some(HUB.into()),
            access_token: "fake".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "fake".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let key = agit::infra::config::hub_host_key(HUB).unwrap();
        agit::infra::credentials::save_at(
            &agit_home.join("credentials").join(format!("{key}.json")),
            &cred,
        )
        .unwrap();
        Lab {
            _tmp: tmp,
            home,
            agit_home,
            work,
        }
    }

    fn run(&self, args: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(&self.work)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.agit_home)
            .env("AGIT_HUB_URL", HUB)
            .env("AGIT_YES", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "`agit {}` failed:\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Appends records to the session's Claude Code transcript.
    fn append(&self, session: &str, bodies: &[serde_json::Value]) {
        let path = self
            .home
            .join(".claude")
            .join("projects")
            .join(agit::adapter::claude_code::slug_for(&self.work))
            .join(format!("{session}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let written = fs::read_to_string(&path).map_or(0, |text| text.lines().count());
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        for (i, body) in bodies.iter().enumerate() {
            let n = written + i;
            let mut record = serde_json::json!({
                "sessionId": session, "cwd": self.work,
                "uuid": format!("{session}-{n}"),
                "timestamp": format!("2026-10-26T11:00:{n:02}.000Z"),
            });
            record
                .as_object_mut()
                .unwrap()
                .extend(body.as_object().unwrap().clone());
            writeln!(file, "{record}").unwrap();
        }
    }

    /// Appends finished turns to the session's Claude Code transcript.
    fn turns(&self, session: &str, prompts: &[&str]) {
        let bodies: Vec<_> = prompts.iter().flat_map(|p| [prompt(p), reply()]).collect();
        self.append(session, &bodies);
    }

    fn meta_at(&self, branch: &str) -> serde_json::Value {
        let out = Command::new("git")
            .args([
                "cat-file",
                "-p",
                &format!("refs/heads/{branch}:session/meta.json"),
            ])
            .current_dir(self.agit_home.join("repos/me/qa"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn capture_at(&self, branch: &str) -> serde_json::Value {
        self.meta_at(branch)["capture"].clone()
    }
}

fn prompt(text: &str) -> serde_json::Value {
    serde_json::json!({"type": "user", "message": {"role": "user", "content": text}})
}

fn reply() -> serde_json::Value {
    serde_json::json!({"type": "assistant", "message": {"role": "assistant",
        "content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn"}})
}

#[test]
fn a_project_capture_records_its_opener_in_the_line_it_opens() {
    let lab = Lab::new();
    lab.run(&["init", "qa"]);
    let capture = [
        "import",
        CAPTURED,
        "--from",
        "claude-code",
        "--into",
        "me/qa@add-a-dark-mode-toggle-1a2b",
        "--independent",
        "--project-capture",
    ];
    // A session start can capture a conversation before its first turn is finished.
    lab.append(CAPTURED, &[prompt("add a dark mode toggle")]);
    lab.run(&capture);
    assert!(lab.meta_at("add-a-dark-mode-toggle-1a2b")["turn"].is_null());
    lab.append(CAPTURED, &[reply()]);
    lab.run(&capture);
    let opener = serde_json::json!({"runtime": "claude-code", "session": CAPTURED});
    assert_eq!(lab.capture_at("add-a-dark-mode-toggle-1a2b"), opener);

    lab.turns(CAPTURED, &["make it follow the system theme"]);
    lab.run(&["commit", "me/qa@add-a-dark-mode-toggle-1a2b"]);
    assert_eq!(lab.capture_at("add-a-dark-mode-toggle-1a2b"), opener);

    lab.run(&["fork", "me/qa@add-a-dark-mode-toggle-1a2b", "-b", "variant"]);
    assert!(lab.capture_at("variant").is_null());

    lab.turns(IMPORTED, &["sketch the release notes"]);
    lab.run(&[
        "import",
        IMPORTED,
        "--from",
        "claude-code",
        "--into",
        "me/qa@notes",
        "--independent",
    ]);
    assert!(lab.capture_at("notes").is_null());
}
