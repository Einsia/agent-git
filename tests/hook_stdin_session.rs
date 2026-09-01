//! The Stop hook settles the session named on stdin and nothing else.
//!
//! The scenario is `/new` inside the TUI: the runtime was launched by `agit resume`, so its
//! process tree carries the `AGIT_SESSION` injected at launch; once the user switches to a new
//! conversation, the Stop hook's stdin names the new session's id while `AGIT_SESSION` still
//! points at the old branch. The hook must follow stdin: a session not yet adopted gets nothing,
//! and the old session's hook settles its own branch as usual.

use agit::domain::repo::Repo;
use std::process::{Command, Stdio};
use std::{fs, io::Write as _};

const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const B: &str = "bbbbbbbb-0000-4000-8000-000000000002";

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
        // `agit commit` fills its author field from the credentials; the hub points at an
        // unreachable address, and commit itself never goes over the network.
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

    /// Feed the hook JSON in on stdin the way Claude Code does.
    fn hook(&self, args: &[&str], session_id: &str, agit_session: Option<&str>) {
        self.hook_env(args, session_id, agit_session, &[]);
    }

    /// The same, with extra environment variables in the process tree.
    fn hook_env(
        &self,
        args: &[&str],
        session_id: &str,
        agit_session: Option<&str>,
        env: &[(&str, &str)],
    ) {
        let mut c = self.agit(args);
        if let Some(s) = agit_session {
            c.env("AGIT_SESSION", s);
        }
        for (k, v) in env {
            c.env(k, v);
        }
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let payload = serde_json::json!({
            "session_id": session_id,
            "cwd": self.work,
            "transcript_path": self.transcript(session_id),
            "hook_event_name": "Stop",
        });
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A snapshot of local agit state: the path, size and modification time of every file under
    /// `AGIT_HOME` and the runtime memory directory. Two equal snapshots mean "nothing was
    /// touched".
    fn local_state(&self) -> Vec<(String, u64, std::time::SystemTime)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, u64, std::time::SystemTime)>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if let Ok(md) = fs::metadata(&path) {
                    out.push((
                        path.to_string_lossy().into_owned(),
                        md.len(),
                        md.modified().unwrap(),
                    ));
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.agit_home, &mut out);
        walk(&self.home.join(".claude/projects"), &mut out);
        out.sort();
        out
    }

    fn transcript(&self, session_id: &str) -> std::path::PathBuf {
        let slug = agit::adapter::claude_code::slug_for(&self.work);
        self.home
            .join(".claude")
            .join("projects")
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

const HUB: &str = "http://127.0.0.1:1";

fn turn_subjects(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.contains("[turn ]"))
        .map(|l| l.rsplit("] ").next().unwrap_or("").trim().to_string())
        .collect()
}

#[test]
fn a_stop_hook_settles_the_stdin_session_and_nothing_else() {
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "A turn 1", "A answer 1"));
    lab.append(B, &lab.turn(B, 1, "B turn 1", "B answer 1"));

    lab.run(&["init", "qa"]);
    lab.run(&["import", A, "--from", "claude-code", "--into", "me/qa@s1"]);
    // B has only been pre-registered by SessionStart: it has a cwd and is unmanaged.
    lab.hook(&["hooks", "ingest"], B, None);

    lab.append(A, &lab.turn(A, 2, "A turn 2", "A answer 2"));
    lab.append(B, &lab.turn(B, 2, "B turn 2", "B answer 2"));

    // B's Stop: the process tree carries a stale AGIT_SESSION. It must neither record A's new
    // turn onto s1 nor record B's content onto any branch — B is not adopted yet.
    lab.hook(&["commit", "--from-hook"], B, Some("me/qa@s1"));
    let log = lab.run(&["log", "me/qa@s1", "--oneline"]);
    assert_eq!(turn_subjects(&log), vec!["A turn 1"], "{log}");
    let repo = Repo::open(lab.agit_home.join("repos/me/qa")).unwrap();
    assert!(
        !repo.has_ref("refs/heads/s2"),
        "an unadopted session must not grow a branch out of nowhere"
    );

    // A's Stop: settles its own branch.
    lab.hook(&["commit", "--from-hook"], A, Some("me/qa@s1"));
    let log = lab.run(&["log", "me/qa@s1", "--oneline"]);
    assert_eq!(turn_subjects(&log), vec!["A turn 1", "A turn 2"], "{log}");
    assert!(
        !log.contains("B turn"),
        "B's content must not appear on A's branch: {log}"
    );
}

/// Inside a process tree launched by the supervisor, the Stop command touches no local state:
/// branches, links and the memory directory are identical before and after — a branch moves only
/// inside the supervisor's lease. The same command outside the gate settles as usual, which pins
/// that what stops it is the gate and not some other precondition.
#[test]
fn a_supervised_stop_hook_leaves_local_state_alone() {
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "A turn 1", "A answer 1"));
    lab.run(&["init", "qa"]);
    lab.run(&["import", A, "--from", "claude-code", "--into", "me/qa@s1"]);
    lab.append(A, &lab.turn(A, 2, "A turn 2", "A answer 2"));

    let before = lab.local_state();
    lab.hook_env(
        &["hooks", "settle"],
        A,
        Some("me/qa@s1"),
        &[
            (agit::rc::harness::SUPERVISED_HOOK_ENV, "1"),
            (
                agit::hub::identity::EXPECTED_AGENT_ID_ENV,
                "0198f2a0-0000-7000-8000-00000000abcd",
            ),
        ],
    );
    assert_eq!(
        lab.local_state(),
        before,
        "a supervised Stop hook must not touch local state"
    );
    let log = lab.run(&["log", "me/qa@s1", "--oneline"]);
    assert_eq!(turn_subjects(&log), vec!["A turn 1"], "{log}");

    lab.hook(&["hooks", "settle"], A, Some("me/qa@s1"));
    let log = lab.run(&["log", "me/qa@s1", "--oneline"]);
    assert_eq!(turn_subjects(&log), vec!["A turn 1", "A turn 2"], "{log}");
}

/// Someone else's repo checked out locally: the link carries only the bare agent name `qa`, and
/// the owner is not the current user. The hook recovers `alice` from the injected session
/// identity or the directory binding; it must not go looking for `me/qa`.
#[test]
fn a_stop_hook_keeps_the_owner_of_someone_elses_repo() {
    someone_elses_checkout_settles_under_its_owner(&["commit", "--from-hook"]);
}

/// The same invariant, through the Stop command `setup` actually installs.
#[test]
fn the_installed_stop_command_keeps_the_owner_of_someone_elses_repo() {
    someone_elses_checkout_settles_under_its_owner(&["hooks", "settle"]);
}

fn someone_elses_checkout_settles_under_its_owner(stop: &[&str]) {
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "A turn 1", "A answer 1"));

    // Build a local checkout of alice/qa holding a claimed session line s1 and a link that
    // records only the bare name: create it under the current user, then move it and rewrite
    // the binding — the shape `agit clone alice/qa` produces, where the link carries only
    // `agent = "qa"`.
    lab.run(&["init", "qa"]);
    lab.run(&["import", A, "--from", "claude-code", "--into", "me/qa@s1"]);
    let repos = lab.agit_home.join("repos");
    fs::create_dir_all(repos.join("alice")).unwrap();
    fs::rename(repos.join("me/qa"), repos.join("alice/qa")).unwrap();
    // A link pulled down by `agit clone alice/qa` carries only the bare agent name, with no
    // namespace; this link was written by import under the current user and records `me` —
    // stripping that is what makes this the scenario. A link that records the namespace binds
    // to that one directory, which is a different contract (see org_repo_import_and_hook).
    let link_path = lab
        .agit_home
        .join("store/claude-code")
        .join(format!("{A}.json"));
    let mut lk: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&link_path).unwrap()).unwrap();
    lk.as_object_mut().unwrap().remove("owner");
    fs::write(&link_path, lk.to_string()).unwrap();
    let ws = lab.agit_home.join("workspaces");
    let binding = fs::read_dir(&ws).unwrap().next().unwrap().unwrap().path();
    let mut v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&binding).unwrap()).unwrap();
    v["repo"] = serde_json::Value::String("alice/qa".into());
    fs::write(&binding, v.to_string()).unwrap();

    // The process tree carries the identity injected at launch: the owner comes from it.
    lab.append(A, &lab.turn(A, 2, "A turn 2", "A answer 2"));
    lab.hook(stop, A, Some("alice/qa@s1"));
    let log = lab.run(&["log", "alice/qa@s1", "--oneline"]);
    assert_eq!(turn_subjects(&log), vec!["A turn 1", "A turn 2"], "{log}");

    // No injected identity (an ordinary session): the owner comes from the directory binding.
    lab.append(A, &lab.turn(A, 3, "A turn 3", "A answer 3"));
    lab.hook(stop, A, None);
    let log = lab.run(&["log", "alice/qa@s1", "--oneline"]);
    assert_eq!(
        turn_subjects(&log),
        vec!["A turn 1", "A turn 2", "A turn 3"],
        "{log}"
    );
    assert!(
        !repos.join("me/qa").exists(),
        "a repo must not be created under the current user out of nowhere"
    );
}
