//! The RC supervisor's strict settlement handshake: `commit --from-supervisor` writes the new
//! commit into the result file, and the supervisor compares it against the watermark taken on
//! either side of the settlement. The main checkout parks on main and a settlement lands in the
//! branch's own worktree, so the watermark must read `refs/heads/<branch>` — the main checkout's
//! HEAD is the same on both sides, and a HEAD-based watermark judges every successful settlement
//! as "HEAD did not move". This pins that contract from the CLI entry point.

use agit::domain::repo::Repo;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::{fs, io::Write as _};

const HUB: &str = "http://127.0.0.1:1";
const AGENT_ID: &str = "0198f2a0-0000-7000-8000-00000000abcd";
const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";

struct Lab {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    agit_home: PathBuf,
    work: PathBuf,
}

impl Lab {
    fn new() -> Lab {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let agit_home = tmp.path().join("agit");
        let work = tmp.path().join("work");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&work).unwrap();
        let cred = agit::infra::credentials::HubCredential {
            username: "me".into(),
            email: None,
            hub: Some(HUB.into()),
            access_token: "fake".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "fake".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let key = agit::infra::config::hub_host_key(HUB);
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

    fn agit(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agit"));
        c.args(args)
            .current_dir(&self.work)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("AGIT_HOME", &self.agit_home)
            .env("AGIT_HUB_URL", HUB)
            .env("AGIT_YES", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        c
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.agit(args).output().unwrap();
        assert!(
            out.status.success(),
            "`agit {}` failed:\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The memory directory Claude keeps for this working directory.
    fn memory_dir(&self) -> PathBuf {
        let project =
            agit::infra::runtime_memory::encode_project(&self.work.canonicalize().unwrap());
        self.home
            .join(".claude/projects")
            .join(project)
            .join("memory")
    }

    fn strict_commit(&self) -> (std::process::Output, String) {
        let result = self.agit_home.join("supervisor-result");
        let _ = fs::remove_file(&result);
        let mut c = self.agit(&["commit", "--from-supervisor"]);
        c.env("AGIT_SESSION", "me/qa@s1")
            .env("AGIT_EXPECTED_AGENT_ID", AGENT_ID)
            .env("AGIT_RC_SUPERVISOR_COMMIT_RESULT", &result)
            .stdin(Stdio::null());
        let out = c.output().unwrap();
        let reported = fs::read_to_string(&result)
            .unwrap_or_default()
            .trim()
            .to_string();
        (out, reported)
    }

    fn transcript(&self, session_id: &str) -> PathBuf {
        let slug = agit::adapter::claude_code::slug_for(&self.work);
        self.home
            .join(".claude/projects")
            .join(slug)
            .join(format!("{session_id}.jsonl"))
    }

    fn append(&self, session_id: &str, lines: &str) {
        let p = self.transcript(session_id);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .unwrap();
        f.write_all(lines.as_bytes()).unwrap();
    }

    fn turn(&self, session_id: &str, n: usize, prompt: &str, reply: &str) -> String {
        let cwd = self.work.to_string_lossy();
        format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "user", "sessionId": session_id, "cwd": cwd,
                "uuid": format!("{session_id}-u{n}"),
                "timestamp": format!("2026-08-29T00:00:{n:02}.000Z"),
                "message": {"role": "user", "content": prompt}
            }),
            serde_json::json!({
                "type": "assistant", "sessionId": session_id, "cwd": cwd,
                "uuid": format!("{session_id}-a{n}"),
                "timestamp": format!("2026-08-29T00:00:{n:02}.500Z"),
                "message": {"role": "assistant", "content": [{"type": "text", "text": reply}]}
            })
        )
    }
}

#[test]
fn the_strict_result_names_the_branch_tip_not_the_primary_head() {
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "first turn", "answer one"));
    lab.run(&["init", "qa"]);
    lab.run(&["import", A, "--from", "claude-code", "--into", "me/qa@s1"]);

    // Pin the checkout to an immutable identity the way `agit clone` does: the supervisor's
    // strict path requires it.
    let repo = Repo::open(lab.agit_home.join("repos/me/qa")).unwrap();
    let identity = agit::hub::identity::RemoteIdentity::new(HUB, AGENT_ID).unwrap();
    agit::hub::identity::pin(&repo, &identity).unwrap();
    assert_eq!(
        repo.current_branch().as_deref(),
        Some("main"),
        "the primary parks on main"
    );
    let primary_head_before = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let branch_before = repo.git(&["rev-parse", "refs/heads/s1"]).unwrap();

    lab.append(A, &lab.turn(A, 2, "second turn", "answer two"));
    let result = lab.agit_home.join("supervisor-result");
    let mut c = lab.agit(&["commit", "--from-supervisor"]);
    c.env("AGIT_SESSION", "me/qa@s1")
        .env("AGIT_EXPECTED_AGENT_ID", AGENT_ID)
        .env("AGIT_RC_SUPERVISOR_COMMIT_RESULT", &result)
        .stdin(Stdio::null());
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let reported = fs::read_to_string(&result).unwrap().trim().to_string();
    let branch_after = repo.git(&["rev-parse", "refs/heads/s1"]).unwrap();
    let primary_head_after = repo.git(&["rev-parse", "HEAD"]).unwrap();
    assert_ne!(
        branch_after.trim(),
        branch_before.trim(),
        "the turn landed on s1"
    );
    assert_eq!(
        reported,
        branch_after.trim(),
        "the result names the new branch tip"
    );
    assert_eq!(
        primary_head_after.trim(),
        primary_head_before.trim(),
        "the primary checkout did not move: a HEAD-based watermark would see no change"
    );
}

/// The memory commit lands after the transcript commit: the result names the branch's **final**
/// tip, and a settlement carrying only memory and no new turn writes the receipt too. Both paths
/// are pinned through the real `commit --from-supervisor` handshake.
#[test]
fn the_strict_result_follows_the_memory_commit() {
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "first turn", "answer one"));
    lab.run(&["init", "qa"]);
    lab.run(&["import", A, "--from", "claude-code", "--into", "me/qa@s1"]);
    let repo = Repo::open(lab.agit_home.join("repos/me/qa")).unwrap();
    let identity = agit::hub::identity::RemoteIdentity::new(HUB, AGENT_ID).unwrap();
    agit::hub::identity::pin(&repo, &identity).unwrap();

    // Establish the baseline: an explicit sync collects the memory already sitting at the top.
    let mem = lab.memory_dir();
    fs::create_dir_all(&mem).unwrap();
    fs::write(mem.join("first.md"), "first fact\n").unwrap();
    lab.run(&["memory", "sync", "--into", "me/qa@s1"]);

    // One turn plus one new memory file: the result is the memory commit, not the transcript
    // commit.
    lab.append(A, &lab.turn(A, 2, "second turn", "answer two"));
    fs::write(mem.join("second.md"), "second fact\n").unwrap();
    let (out, reported) = lab.strict_commit();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tip = repo.git(&["rev-parse", "refs/heads/s1"]).unwrap();
    assert_eq!(
        reported,
        tip.trim(),
        "the result names the branch tip after the memory commit"
    );
    assert!(
        repo.show("refs/heads/s1", "memory/second.md").is_some(),
        "the memory commit landed"
    );
    assert!(
        repo.git(&["log", "-1", "--format=%s", "refs/heads/s1"])
            .unwrap()
            .contains("memory sync"),
        "the tip is the memory commit"
    );

    // Memory only, no new turn: a receipt is still written.
    fs::write(mem.join("third.md"), "third fact\n").unwrap();
    let (out, reported) = lab.strict_commit();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tip = repo.git(&["rev-parse", "refs/heads/s1"]).unwrap();
    assert_eq!(
        reported,
        tip.trim(),
        "a memory-only settlement still reports the new tip"
    );
}
