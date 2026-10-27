//! `agit commit --code` commits in the checkout it runs in, never in the claim's launch directory,
//! and never stages untracked files.

use agit::domain::{link, meta, storage, store::Store, transcript};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SID: &str = "00000000-0000-4000-8000-000000000140";

fn message(role: &str, text: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"response_item","payload":{
            "type":"message","role":role,"content":[{"type":"input_text","text":text}]
        }})
    )
}

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
        .env("CODEX_HOME", root.join("codex"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("AGIT_HUB_URL", "https://hub.test")
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
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct Lab {
    temp: tempfile::TempDir,
    native: PathBuf,
    launch: PathBuf,
    agent: PathBuf,
}

impl Lab {
    /// A session claimed in `launch` (the user's checkout, on their own branch, with stray
    /// untracked files) whose agent edits a second checkout, `agent`, and has one pending turn.
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_owned();
        fs::write(root.join("empty-config"), "").unwrap();
        let lab = Self {
            native: root
                .join("codex/sessions/2026/10/26")
                .join(format!("rollout-2026-10-26T00-00-00-{SID}.jsonl")),
            launch: root.join("burrow"),
            agent: root.join("burrow-alt"),
            temp,
        };

        let repo = root.join("agit/repos/alice/notes");
        fs::create_dir_all(&repo).unwrap();
        lab.git(&repo, &["init", "-q", "-b", "main"]);
        lab.git(&repo, &["config", "commit.gpgsign", "false"]);
        meta::write(&repo, &meta::Meta::new_file_line()).unwrap();
        lab.git(&repo, &["add", "-A"]);
        lab.git(&repo, &["commit", "-q", "-m", "initial shared state"]);
        lab.git(&repo, &["checkout", "-q", "-b", "topic"]);
        let prefix = format!(
            "{}\n{}{}",
            json!({"type":"session_meta","payload":{"id":SID}}),
            message("user", "settled question"),
            message("assistant", "settled answer")
        );
        let session = format!("agit-{}", "c".repeat(40));
        let log = transcript::wrap_lines(&prefix, "codex", &session);
        storage::write_snapshot(&repo, &log, &log).unwrap();
        let mut snapshot = meta::Meta::new(session, "codex".into(), "/fixture".into());
        snapshot.turn = Some(1);
        meta::write(&repo, &snapshot).unwrap();
        lab.git(&repo, &["add", "-A"]);
        lab.git(&repo, &["commit", "-q", "-m", "settled native prefix"]);
        fs::create_dir_all(lab.native.parent().unwrap()).unwrap();
        fs::write(&lab.native, &prefix).unwrap();
        lab.append_turn("empty state, alternative");

        for (dir, branch) in [(&lab.launch, "art-empty-states"), (&lab.agent, "alt")] {
            fs::create_dir_all(dir).unwrap();
            lab.git(dir, &["init", "-q", "-b", branch]);
            lab.git(dir, &["config", "commit.gpgsign", "false"]);
            lab.git(
                dir,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://example.invalid/burrow.git",
                ],
            );
            fs::write(dir.join("app.txt"), "v1\n").unwrap();
            lab.git(dir, &["add", "app.txt"]);
            lab.git(dir, &["commit", "-q", "-m", "seed"]);
        }
        fs::create_dir_all(lab.launch.join(".playwright-mcp")).unwrap();
        fs::write(lab.launch.join(".playwright-mcp/shot.png"), "png").unwrap();

        // Settlement names its author, so the home holds a signed-in account; nothing here
        // reaches the Hub.
        let credentials = root.join("agit/credentials/hub.test.json");
        fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        let account = json!({
            "username": "alice",
            "hub": "https://hub.test",
            "email": "alice@example.test",
            "access_token": "test-access-token",
            "access_expires_at": "2100-01-01T00:00:00Z",
            "refresh_token": "test-refresh-token",
            "refresh_expires_at": "2100-01-01T00:00:00Z",
        });
        fs::write(&credentials, format!("{account}\n")).unwrap();

        let mut claim = link::Link::new("codex", SID, None);
        claim.owner = Some("alice".into());
        claim.agent = Some("notes".into());
        claim.branch = Some("topic".into());
        claim.cwd = Some(lab.launch.to_string_lossy().into_owned());
        link::write(&Store::at(root.join("agit/store")), &claim).unwrap();
        lab
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        success(
            isolated("git", self.temp.path())
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap(),
        )
        .trim()
        .to_owned()
    }

    fn append_turn(&self, text: &str) {
        let mut native = fs::read_to_string(&self.native).unwrap();
        native.push_str(&message("user", text));
        native.push_str(&message("assistant", "done"));
        fs::write(&self.native, native).unwrap();
    }

    fn commit_code_in(&self, dir: &Path) -> Output {
        isolated(env!("CARGO_BIN_EXE_agit"), self.temp.path())
            .env("AGIT_SESSION", "alice/notes@topic")
            .args(["commit", "alice/notes@topic", "--code"])
            .current_dir(dir)
            .output()
            .unwrap()
    }

    fn session_tip(&self) -> String {
        let repo = self.temp.path().join("agit/repos/alice/notes");
        self.git(&repo, &["rev-parse", "refs/heads/topic"])
    }
}

/// A fork launched in one checkout and working in another records its code where it worked: the
/// commit and the session's code anchor name the agent's checkout, and the launch checkout's
/// branch and stray files are untouched. Committing in the claim's recorded cwd would put a
/// commit on the user's branch there (with the stray files under `git add -A`).
#[test]
fn code_commit_lands_in_the_checkout_the_agent_worked_in() {
    let lab = Lab::new();
    let launch_head = lab.git(&lab.launch, &["rev-parse", "HEAD"]);
    fs::write(lab.agent.join("app.txt"), "v2\n").unwrap();

    let output = lab.commit_code_in(&lab.agent);
    let stdout = success(output);
    assert!(stdout.contains("branch `alt`"), "{stdout}");

    assert_eq!(lab.git(&lab.launch, &["rev-parse", "HEAD"]), launch_head);
    assert_eq!(
        lab.git(&lab.launch, &["status", "--porcelain"]),
        "?? .playwright-mcp/"
    );
    assert_eq!(
        lab.git(&lab.agent, &["log", "-1", "--format=%s"]),
        "work: alice/notes@topic (agit commit --code)"
    );
    assert_eq!(lab.git(&lab.agent, &["status", "--porcelain"]), "");
    let agent_head = lab.git(&lab.agent, &["rev-parse", "--short", "HEAD"]);
    let repo = lab.temp.path().join("agit/repos/alice/notes");
    let body = lab.git(&repo, &["log", "-1", "--format=%B", "refs/heads/topic"]);
    assert!(
        body.contains(&format!(
            "code: https://example.invalid/burrow.git@{agent_head}"
        )),
        "{body}"
    );
}

/// Untracked files are never staged: with one in the target checkout the settlement refuses,
/// names it, and writes neither a code commit nor a session commit, even when the user's Git
/// config hides untracked files from `git status`. A file the user has staged is theirs to
/// include, so after `git add` the same command commits it.
#[test]
fn untracked_files_refuse_the_code_commit_until_the_user_chooses() {
    let lab = Lab::new();
    let agent_head = lab.git(&lab.agent, &["rev-parse", "HEAD"]);
    let session_tip = lab.session_tip();
    fs::write(lab.agent.join("app.txt"), "v2\n").unwrap();
    fs::write(lab.agent.join("new.txt"), "new\n").unwrap();
    lab.git(&lab.agent, &["config", "status.showUntrackedFiles", "no"]);

    let output = lab.commit_code_in(&lab.agent);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("untracked") && stderr.contains("  new.txt"),
        "{stderr}"
    );
    assert_eq!(lab.git(&lab.agent, &["rev-parse", "HEAD"]), agent_head);
    assert_eq!(lab.session_tip(), session_tip);

    lab.git(&lab.agent, &["add", "new.txt"]);
    success(lab.commit_code_in(&lab.agent));
    assert_eq!(
        lab.git(&lab.agent, &["show", "--name-only", "--format=", "HEAD"]),
        "app.txt\nnew.txt"
    );
    assert_ne!(lab.session_tip(), session_tip);
}
