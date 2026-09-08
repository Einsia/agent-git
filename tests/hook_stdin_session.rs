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

#[test]
fn imported_history_cannot_claim_the_code_state_observed_by_a_later_hook() {
    use agit::domain::meta::{self, Completeness, WorktreeStatus};

    let lab = Lab::new();
    let code = Repo::init(&lab.work).unwrap();
    code.git(&[
        "remote",
        "add",
        "origin",
        "git@example.invalid:team/project.git",
    ])
    .unwrap();
    code.git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--allow-empty",
        "-m",
        "code root",
    ])
    .unwrap();
    lab.append(
        A,
        &lab.turn(A, 1, "historical request", "historical answer"),
    );
    lab.append(
        A,
        &lab.turn(A, 2, "later historical request", "later historical answer"),
    );
    lab.run(&["init", "qa"]);
    lab.run(&["import", A, "--from", "claude-code", "--into", "me/qa@s1"]);

    let repo = Repo::open(lab.agit_home.join("repos/me/qa")).unwrap();
    let head = repo.git(&["rev-parse", "refs/heads/s1"]).unwrap();
    for reference in [head.trim().to_string(), format!("{}~1", head.trim())] {
        let historical = meta::read_at_ref(&repo, &reference).unwrap();
        assert!(historical.code.is_some());
        assert_eq!(historical.completeness, Some(Completeness::Unknown));
        assert!(historical.cwd_state.is_none());
    }

    lab.append(A, &lab.turn(A, 3, "live request", "live answer"));
    lab.hook(&["hooks", "settle"], A, Some("me/qa@s1"));
    let live = meta::read_at_ref(&repo, "refs/heads/s1").unwrap();
    assert_eq!(live.completeness, Some(Completeness::Exact));
    let state = live.cwd_state.unwrap();
    assert_eq!(
        state.origin.as_deref(),
        Some("git@example.invalid:team/project.git")
    );
    assert_eq!(state.worktree, WorktreeStatus::Clean);

    fs::write(lab.work.join("untracked.txt"), "uncommitted code\n").unwrap();
    lab.append(
        A,
        &lab.turn(A, 4, "dirty live request", "dirty live answer"),
    );
    lab.hook(&["hooks", "settle"], A, Some("me/qa@s1"));
    let dirty = meta::read_at_ref(&repo, "refs/heads/s1").unwrap();
    assert_eq!(dirty.completeness, Some(Completeness::Partial));
    assert_eq!(dirty.cwd_state.unwrap().worktree, WorktreeStatus::Dirty);
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

/// A recorded namespace wins over stale process and workspace identity during hook settlement.
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

    // The checkout and link name Alice while the workspace remains bound to the current user.
    lab.run(&["init", "qa"]);
    lab.run(&["import", A, "--from", "claude-code", "--into", "me/qa@s1"]);
    let repos = lab.agit_home.join("repos");
    fs::create_dir_all(repos.join("alice")).unwrap();
    fs::rename(repos.join("me/qa"), repos.join("alice/qa")).unwrap();
    let link_path = lab
        .agit_home
        .join("store/claude-code")
        .join(format!("{A}.json"));
    let mut lk: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&link_path).unwrap()).unwrap();
    lk["owner"] = serde_json::Value::String("alice".into());
    fs::write(&link_path, lk.to_string()).unwrap();

    // The recorded owner remains authoritative when the injected identity names another repo.
    lab.append(A, &lab.turn(A, 2, "A turn 2", "A answer 2"));
    lab.hook(stop, A, Some("me/qa@other"));
    let log = lab.run(&["log", "alice/qa@s1", "--oneline"]);
    assert_eq!(turn_subjects(&log), vec!["A turn 1", "A turn 2"], "{log}");

    // The same claim remains complete when no environment identity is supplied.
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

#[test]
fn ownerless_hook_claims_require_explicit_readoption() {
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "registered turn", "done"));
    lab.run(&["init", "qa"]);
    lab.run(&["import", A, "--from", "claude-code", "--into", "me/qa@s1"]);
    let link_path = lab
        .agit_home
        .join("store/claude-code")
        .join(format!("{A}.json"));
    let mut claim: serde_json::Value =
        serde_json::from_slice(&fs::read(&link_path).unwrap()).unwrap();
    claim.as_object_mut().unwrap().remove("owner");
    fs::write(&link_path, serde_json::to_vec(&claim).unwrap()).unwrap();
    let environment_file = lab
        .home
        .join(".claude/session-env")
        .join(A)
        .join("sessionstart-hook.sh");
    fs::create_dir_all(environment_file.parent().unwrap()).unwrap();
    lab.hook_env(
        &["hooks", "ingest"],
        A,
        None,
        &[("CLAUDE_ENV_FILE", environment_file.to_str().unwrap())],
    );
    assert_eq!(
        fs::read_to_string(environment_file).unwrap(),
        "unset AGIT_SESSION\n"
    );
    let repo = Repo::open(lab.agit_home.join("repos/me/qa")).unwrap();
    let before = repo.git(&["rev-parse", "refs/heads/s1"]).unwrap();
    lab.append(A, &lab.turn(A, 2, "pending turn", "done"));
    for command in [["commit", "--from-hook"], ["hooks", "settle"]] {
        for env in [None, Some("me/qa@different-branch")] {
            lab.hook(&command, A, env);
            assert_eq!(
                repo.git(&["rev-parse", "refs/heads/s1"]).unwrap(),
                before,
                "a hook must not infer a missing namespace from its environment or directory"
            );
        }
    }
    lab.run(&["import", A, "--from", "claude-code", "--into", "me/qa@s1"]);
    let claim: serde_json::Value = serde_json::from_slice(&fs::read(&link_path).unwrap()).unwrap();
    assert_eq!(claim["owner"], "me");
    let settled = repo.git(&["rev-parse", "refs/heads/s1"]).unwrap();
    lab.append(A, &lab.turn(A, 3, "claimed turn", "done"));
    lab.hook(&["hooks", "settle"], A, Some("other/qa@different-branch"));
    assert_ne!(repo.git(&["rev-parse", "refs/heads/s1"]).unwrap(), settled);
}

/// A native transcript ID cannot supply a missing repository namespace after an account switch.
#[test]
fn native_id_commit_requires_the_recorded_repository_owner() {
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "saved under Alice", "done"));
    lab.run(&["init", "qa"]);
    lab.run(&["import", A, "--from", "claude-code", "--into", "me/qa@s1"]);
    let repos = lab.agit_home.join("repos");
    fs::create_dir_all(repos.join("alice")).unwrap();
    fs::rename(repos.join("me/qa"), repos.join("alice/qa")).unwrap();
    let link_path = lab
        .agit_home
        .join("store/claude-code")
        .join(format!("{A}.json"));
    let mut claim: serde_json::Value =
        serde_json::from_slice(&fs::read(&link_path).unwrap()).unwrap();
    claim.as_object_mut().unwrap().remove("owner");
    fs::write(&link_path, serde_json::to_vec(&claim).unwrap()).unwrap();
    let before_link = fs::read(&link_path).unwrap();
    let repo = Repo::open(repos.join("alice/qa")).unwrap();
    let before_head = repo.git(&["rev-parse", "refs/heads/s1"]).unwrap();
    lab.append(A, &lab.turn(A, 2, "new local work", "done"));
    let output = lab.agit(&["commit", A]).output().unwrap();
    assert!(
        !output.status.success(),
        "ownerless native ID unexpectedly settled: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("recorded repository owner"));
    assert_eq!(
        repo.git(&["rev-parse", "refs/heads/s1"]).unwrap(),
        before_head
    );
    assert_eq!(fs::read(&link_path).unwrap(), before_link);
    assert!(!repos.join("me/qa").exists());
    claim["owner"] = serde_json::Value::String("alice".into());
    fs::write(&link_path, serde_json::to_vec(&claim).unwrap()).unwrap();
    lab.run(&["commit", A]);
    assert_ne!(
        repo.git(&["rev-parse", "refs/heads/s1"]).unwrap(),
        before_head
    );
    assert!(!repos.join("me/qa").exists());
}

/// An unmanaged-session refusal prints an executable adoption command for its detected native ID.
#[test]
fn new_guard_adoption_hint_preserves_the_detected_conversation() {
    let lab = Lab::new();
    lab.run(&["init", "qa"]);
    lab.append(
        A,
        &lab.turn(A, 1, "preserve this unmanaged conversation", "done"),
    );
    let output = lab
        .agit(&["new", "me/qa", "-b", "saved", "--no-launch"])
        .env("CLAUDE_CODE_SESSION_ID", A)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let hint = stderr
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("agit import "))
        .expect("adoption hint");
    let arguments: Vec<_> = hint.split_whitespace().collect();
    assert_eq!(arguments[2], A, "{hint}");
    lab.run(&arguments[1..]);
    let repo = Repo::open(lab.agit_home.join("repos/me/qa")).unwrap();
    assert!(repo.has_ref("refs/heads/saved"));
    let log = lab.run(&["log", "me/qa@saved", "--oneline"]);
    assert_eq!(
        turn_subjects(&log),
        vec!["preserve this unmanaged conversation"]
    );
}

/// Offline adoption leaves identity unclaimed until its printed command selects a repository.
#[test]
fn offline_import_hint_records_the_selected_first_version() {
    assert_offline_adoption_hint(false);
}

/// Committing an unclaimed offline link directs the caller to an executable explicit import.
#[test]
fn unclaimed_commit_hint_records_the_selected_first_version() {
    assert_offline_adoption_hint(true);
}

fn assert_offline_adoption_hint(from_commit: bool) {
    let lab = Lab::new();
    let credential_path = lab.agit_home.join("credentials").join(format!(
        "{}.json",
        agit::infra::config::hub_host_key(HUB).unwrap()
    ));
    let credentials = fs::read(&credential_path).unwrap();
    fs::remove_file(&credential_path).unwrap();
    lab.append(A, &lab.turn(A, 1, "preserve offline work", "done"));
    let output = lab.run(&["import", A, "--link-only"]);
    let link_path = lab
        .agit_home
        .join("store/claude-code")
        .join(format!("{A}.json"));
    let claim: serde_json::Value = serde_json::from_slice(&fs::read(&link_path).unwrap()).unwrap();
    assert!(claim.get("owner").is_none());
    assert!(!lab.agit_home.join("repos/me/qa").exists());
    fs::write(&credential_path, credentials).unwrap();
    let output = if from_commit {
        let before = fs::read(&link_path).unwrap();
        let result = lab.agit(&["commit", A]).output().unwrap();
        assert!(!result.status.success());
        assert_eq!(fs::read(&link_path).unwrap(), before);
        assert!(!lab.agit_home.join("repos/me/qa").exists());
        String::from_utf8_lossy(&result.stderr).into_owned()
    } else {
        output
    };
    let hint = if from_commit {
        output
            .lines()
            .find_map(|line| line.find("agit ").map(|start| &line[start..]))
    } else {
        output
            .lines()
            .find(|line| line.contains("records the first version"))
            .and_then(|line| line.split('`').nth(1))
    }
    .expect("offline adoption follow-up");
    let command = hint
        .replace("<owner/repo>", "me/qa")
        .replace("<branch>", "saved");
    let arguments: Vec<_> = command.split_whitespace().collect();
    assert_eq!(arguments[1], "import", "{hint}");
    assert_eq!(arguments[2], A, "{hint}");
    lab.run(&arguments[1..]);
    let repo = Repo::open(lab.agit_home.join("repos/me/qa")).unwrap();
    assert!(repo.has_ref("refs/heads/saved"));
    let claim: serde_json::Value = serde_json::from_slice(&fs::read(&link_path).unwrap()).unwrap();
    assert_eq!(claim["owner"], "me");
    assert_eq!(claim["agent"], "qa");
    assert_eq!(claim["branch"], "saved");
    let log = lab.run(&["log", "me/qa@saved", "--oneline"]);
    assert_eq!(turn_subjects(&log), vec!["preserve offline work"]);
}
