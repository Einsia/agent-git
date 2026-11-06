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

#[path = "support/local_state.rs"]
mod local_state;

const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const B: &str = "bbbbbbbb-0000-4000-8000-000000000002";

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Rows(Vec<String>),
    File(u64, std::time::SystemTime),
}

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

    fn choose_independent_from_hint(&self, args: &[&str]) {
        let before = self.local_state();
        let output = self.agit(args).arg("--json").output().unwrap();
        assert_eq!(output.status.code(), Some(8), "{output:?}");
        assert_eq!(self.local_state(), before);
        let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let report = &document["result"]["value"];
        assert_eq!(report["operation"], "choice_required");
        let action = &report["independent"];
        let argv: Vec<_> = action["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(argv[0], "agit");
        assert!(argv.contains(&"--independent"));
        let mut command = self.agit(&argv[1..]);
        command.current_dir(action["cwd"].as_str().unwrap());
        for (name, value) in action["env"].as_object().unwrap() {
            command.env(name, value.as_str().unwrap());
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
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

    /// A snapshot of local agit state under `AGIT_HOME` and the runtime memory directory: the
    /// size and modification time of every file, and the rows of every SQLite database (see
    /// `support/local_state.rs`). Two equal snapshots mean "nothing was touched".
    fn local_state(&self) -> Vec<(String, State)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, State)>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.to_string_lossy().into_owned();
                if path.is_dir() {
                    walk(&path, out);
                } else if let Some(rows) = local_state::rows(&path) {
                    out.push((name, State::Rows(rows)));
                } else if let Ok(md) = fs::metadata(&path)
                    && !local_state::is_sidecar(&path)
                {
                    out.push((name, State::File(md.len(), md.modified().unwrap())));
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
                "message": {"role": "assistant", "stop_reason": "end_turn", "content": [{"type": "text", "text": reply}]}
            })
        )
    }
}

const HUB: &str = "http://127.0.0.1:1";

/// A session start tells the model nothing, not even for a conversation agit manages: no binding,
/// no commands and no saved environment, since anything there invites the agent to run agit in
/// turns that have nothing to do with it. Here a managed conversation resumes in a folder that
/// does not upload by itself, so the person has nothing to be told either and the hook prints
/// nothing at all.
#[test]
fn a_session_start_tells_the_model_nothing() {
    let lab = Lab::new();
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&lab.work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(git.status.success());
    fs::write(lab.work.join("earlier.txt"), "uncommitted code").unwrap();
    lab.append(A, &lab.turn(A, 1, "work on code", "code updated"));
    lab.run(&["init", "qa"]);
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);
    lab.append(
        A,
        &lab.turn(A, 2, "record the current code state", "current turn done"),
    );
    lab.hook(&["hooks", "settle"], A, Some("me/qa@s1"));
    fs::remove_dir_all(lab.agit_home.join("workspaces")).unwrap();
    fs::write(lab.work.join("later.txt"), "a newer change").unwrap();

    let mut child = lab
        .agit(&["hooks", "ingest", "--runtime", "claude-code"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = serde_json::json!({
        "session_id": A,
        "cwd": lab.work,
        "transcript_path": lab.transcript(A),
        "source": "resume",
        "session_title": "User chosen title",
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

impl Lab {
    /// The hook commands `agit setup --hooks` installed into this home's Claude Code settings
    /// for `event`.
    fn installed_hooks(&self, event: &str) -> Vec<String> {
        let path = self.home.join(".claude/settings.json");
        let settings: serde_json::Value = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        settings["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|group| group["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|hook| hook["command"].as_str().map(str::to_owned))
            .collect()
    }

    /// Run every installed hook for `event` the way Claude Code does: through a shell, in the
    /// session's directory, with the event's JSON on stdin.
    fn fire(&self, event: &str, session_id: &str, extra: serde_json::Value) {
        let mut payload = serde_json::json!({
            "session_id": session_id,
            "cwd": self.work,
            "transcript_path": self.transcript(session_id),
            "hook_event_name": event,
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().cloned().unwrap_or_default());
        for command in self.installed_hooks(event) {
            let mut child = Command::new("sh")
                .args(["-c", &command])
                .current_dir(&self.work)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &self.home)
                .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
                .env("AGIT_HOME", &self.agit_home)
                .env("AGIT_HUB_URL", HUB)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.to_string().as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{event} hook `{command}` failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

/// The last turn before `/exit` is settled without the person running `agit commit`.
///
/// Claude Code runs Stop hooks before its transcript writer has flushed the turn's final reply,
/// and writes that reply, with its hook and duration summaries, only afterwards. A turn's Stop
/// therefore reads the turn unanswered; ordinarily the next turn's Stop settles it, but after the
/// last turn the session ends with only SessionEnd left to fire. An implementation that settles
/// solely from what Stop reads leaves that turn unsettled, so automatic upload never sees it; one
/// that settles the unanswered prompt at Stop lands the turn without its reply.
///
/// A session driven over stream-json input runs the Stop before it writes any of the turn, so the
/// file still ends at the previous turn, answered; only the `prompt_id` the Stop reports shows the
/// turn is missing. Settling what the file holds then lands nothing new and leaves this turn to a
/// later Stop, and one that settled the previous turn again would duplicate it.
#[test]
fn the_last_turn_before_exit_settles_with_its_reply() {
    let lab = Lab::new();
    let config = lab.home.join(".claude");
    fs::create_dir_all(&config).unwrap();
    lab.run(&["setup", "--hooks", "--runtime", "claude-code", "--yes"]);
    assert!(
        !lab.installed_hooks("Stop").is_empty(),
        "setup installed no Stop hook"
    );

    lab.append(A, &lab.turn(A, 1, "A turn 1", "A answer 1"));
    lab.run(&["init", "qa"]);
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);

    // The last turn: Stop fires while only its prompt is on disk.
    let turn = lab.turn(A, 2, "A turn 2", "A answer 2");
    let (prompt, reply) = turn.split_at(turn.find('\n').unwrap() + 1);
    lab.append(A, prompt);
    lab.fire("Stop", A, serde_json::json!({"stop_hook_active": false}));

    // The writer flushes the reply and the turn's bookkeeping, then the person types /exit.
    lab.append(A, reply);
    let answered = fs::metadata(lab.transcript(A)).unwrap().len();
    lab.append(
        A,
        &format!(
            "{}\n{}\n",
            serde_json::json!({"type": "system", "subtype": "stop_hook_summary", "sessionId": A}),
            serde_json::json!({"type": "system", "subtype": "turn_duration", "sessionId": A}),
        ),
    );
    lab.fire(
        "SessionEnd",
        A,
        serde_json::json!({"reason": "prompt_input_exit"}),
    );

    // Settlement may finish in a process the hooks detached, so wait a bounded time for it.
    let link = lab
        .agit_home
        .join("store/claude-code")
        .join(format!("{A}.json"));
    let checkpoint = || {
        fs::read_to_string(&link)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|lk| lk["native_checkpoint"]["bytes"].as_u64())
            .unwrap_or(0)
    };
    let settled_through = |end: u64| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while checkpoint() < end && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    };
    settled_through(answered);
    let log = lab.run(&["log", "me/qa@s1", "--oneline"]);
    assert_eq!(turn_subjects(&log), vec!["A turn 1", "A turn 2"], "{log}");
    assert!(
        checkpoint() >= answered,
        "the last turn settled without its reply: checkpoint {} < {answered}",
        checkpoint()
    );

    // The session is resumed over stream-json input: its Stop comes before any of the turn.
    let prompt_id = "aaaaaaaa-0000-4000-8000-0000000000p3";
    lab.fire(
        "Stop",
        A,
        serde_json::json!({"stop_hook_active": false, "prompt_id": prompt_id}),
    );
    let log = lab.run(&["log", "me/qa@s1", "--oneline"]);
    assert_eq!(turn_subjects(&log), vec!["A turn 1", "A turn 2"], "{log}");
    let mut records = lab.turn(A, 3, "A turn 3", "A answer 3");
    let mut prompt: serde_json::Value =
        serde_json::from_str(&records[..records.find('\n').unwrap()]).unwrap();
    prompt["promptId"] = prompt_id.into();
    records.replace_range(..records.find('\n').unwrap(), &prompt.to_string());
    lab.append(A, &records);
    let answered = fs::metadata(lab.transcript(A)).unwrap().len();
    lab.append(
        A,
        &format!(
            "{}\n",
            serde_json::json!({"type": "system", "subtype": "stop_hook_summary", "sessionId": A}),
        ),
    );
    settled_through(answered);
    let log = lab.run(&["log", "me/qa@s1", "--oneline"]);
    assert_eq!(
        turn_subjects(&log),
        vec!["A turn 1", "A turn 2", "A turn 3"],
        "{log}"
    );
}

fn turn_subjects(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.contains("[turn ]"))
        .map(|line| {
            let (_, activity) = line.split_once("[turn ] ").unwrap();
            let (events, rest) = activity.split_once(" events ").unwrap();
            let (tools, subject) = rest.split_once(" ToolUse ").unwrap();
            assert!(events.parse::<usize>().is_ok(), "{line}");
            assert!(tools.parse::<usize>().is_ok(), "{line}");
            subject.trim().to_string()
        })
        .collect()
}

#[test]
fn a_stop_hook_settles_the_stdin_session_and_nothing_else() {
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "A turn 1", "A answer 1"));
    lab.append(B, &lab.turn(B, 1, "B turn 1", "B answer 1"));

    lab.run(&["init", "qa"]);
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);
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
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);

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
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);
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
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);
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
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);
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
    let environment = fs::read_to_string(environment_file).unwrap();
    assert!(
        environment.starts_with("unset AGIT_SESSION\n")
            && !environment.contains("export AGIT_SESSION="),
        "{environment}"
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
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);
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
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);
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

/// An unmanaged-session refusal preserves native identity while leaving lineage as an explicit choice.
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
    lab.choose_independent_from_hint(&arguments[1..]);
    let repo = Repo::open(lab.agit_home.join("repos/me/qa")).unwrap();
    assert!(repo.has_ref("refs/heads/saved"));
    let log = lab.run(&["log", "me/qa@saved", "--oneline"]);
    assert_eq!(
        turn_subjects(&log),
        vec!["preserve this unmanaged conversation"]
    );
}

/// Offline adoption leaves identity unclaimed until a printed command and lineage choice are accepted.
#[test]
fn offline_import_hint_records_the_selected_first_version() {
    assert_offline_adoption_hint(false);
}

/// Committing an unclaimed offline link directs the caller to explicit native and lineage selection.
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
    lab.choose_independent_from_hint(&arguments[1..]);
    let repo = Repo::open(lab.agit_home.join("repos/me/qa")).unwrap();
    assert!(repo.has_ref("refs/heads/saved"));
    let claim: serde_json::Value = serde_json::from_slice(&fs::read(&link_path).unwrap()).unwrap();
    assert_eq!(claim["owner"], "me");
    assert_eq!(claim["agent"], "qa");
    assert_eq!(claim["branch"], "saved");
    let log = lab.run(&["log", "me/qa@saved", "--oneline"]);
    assert_eq!(turn_subjects(&log), vec!["preserve offline work"]);
}

/// Re-importing a session describes its claim: a registration is about to be saved, a saved
/// claim names its destination and how to settle later turns. The link file path is never
/// offered as a next step. A message keyed only on "a link file exists" says "already adopted"
/// for a registration nothing has saved.
///
/// A refused import restores the link it wrote, including removing it. The restore compares the
/// link against the bytes the import wrote, so a rollback that rewrites them differently leaves
/// a placeholder behind, which the last assertions catch.
#[test]
fn reimport_wording_follows_the_claim_state() {
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "keep this", "kept"));
    lab.run(&["import", A, "--from", "claude-code", "--link-only"]);
    let link_path = lab
        .agit_home
        .join("store/claude-code")
        .join(format!("{A}.json"));
    let import = [
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@saved",
        "--independent",
    ];

    let registered = lab.agit(&import).output().unwrap();
    assert!(registered.status.success(), "{registered:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&registered.stdout),
        String::from_utf8_lossy(&registered.stderr)
    );
    assert!(
        text.contains("was registered but not saved yet; this import saves it to me/qa@saved"),
        "{text}"
    );
    assert!(!text.contains("already adopted"), "{text}");
    assert!(!text.contains(&*link_path.to_string_lossy()), "{text}");

    lab.append(A, &lab.turn(A, 2, "keep more", "kept more"));
    let saved = lab.agit(&import).output().unwrap();
    assert!(saved.status.success(), "{saved:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&saved.stdout),
        String::from_utf8_lossy(&saved.stderr)
    );
    assert!(text.contains("is already saved at me/qa@saved"), "{text}");
    assert!(text.contains("`agit commit me/qa@saved`"), "{text}");
    assert!(!text.contains(&*link_path.to_string_lossy()), "{text}");

    // An import that cannot claim the branch leaves the other session's link as it found it:
    // absent when there was none, byte for byte when a registration existed.
    lab.append(B, &lab.turn(B, 1, "someone else", "refused"));
    let other_link = link_path.with_file_name(format!("{B}.json"));
    let onto_claimed = [
        "import",
        B,
        "--from",
        "claude-code",
        "--into",
        "me/qa@saved",
        "--independent",
        "-y",
    ];
    let refused = lab.agit(&onto_claimed).output().unwrap();
    assert_eq!(refused.status.code(), Some(7), "{refused:?}");
    assert!(!other_link.exists());
    lab.run(&["import", B, "--from", "claude-code", "--link-only"]);
    let registration = std::fs::read(&other_link).unwrap();
    let refused = lab.agit(&onto_claimed).output().unwrap();
    assert_eq!(refused.status.code(), Some(7), "{refused:?}");
    assert_eq!(std::fs::read(&other_link).unwrap(), registration);
}

/// A session aimed at `main` is refused before anything is adopted or created, with the reason
/// and a replacement in the caller's own `--into` spelling. Refusing only at settlement leaves a
/// new repository and a session link behind. The lab is a computer where agit was never set up,
/// so a not-set-up notice shown ahead of the refusal leaves its record behind as well.
#[test]
fn main_branch_target_is_refused_before_adoption() {
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "not for main", "ok"));
    let before = lab.local_state();
    let output = lab
        .agit(&[
            "import",
            A,
            "--from",
            "claude-code",
            "--into",
            "me/qa@main",
            "--independent",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("shared files"), "{stderr}");
    assert!(stderr.contains("`--into me/qa@"), "{stderr}");
    assert!(!stderr.contains("-b <"), "{stderr}");
    assert!(!lab.agit_home.join("repos/me/qa").exists());
    assert!(!lab.agit_home.join("store/claude-code").exists());
    assert_eq!(lab.local_state(), before);
}

/// No hook exits 2: Claude Code reads a Stop hook's exit 2 as "block, and hand stderr to the
/// model", which would turn a hook's own failure into the agent's next instruction. An argument
/// this build cannot parse is the usage failure no dispatch-level mapping can reach, so a mapping
/// inside `hooks` alone lets clap's 2 through. Outside a hook the same failure keeps exit 2, so a
/// mapping applied to every command fails too.
#[test]
fn hook_usage_failure_exits_as_plain_failure() {
    let lab = Lab::new();
    for args in [
        ["hooks", "settle", "--no-such-flag"],
        ["commit", "--from-hook", "--no-such-flag"],
    ] {
        let output = lab.agit(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--no-such-flag"),
            "{output:?}"
        );
    }
    let output = lab.agit(&["status", "--no-such-flag"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

/// Inherited supervisor routing cannot make a runtime helper own the parent branch.
#[test]
fn supervised_start_hooks_leave_native_registration_to_the_supervisor() {
    let lab = Lab::new();
    lab.run(&["init", "qa"]);
    let before = lab.local_state();
    let mut child = lab
        .agit(&["hooks", "ingest", "--runtime", "codex"])
        .env("AGIT_SESSION", "me/qa@s1")
        .env(agit::rc::harness::SUPERVISED_HOOK_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(serde_json::json!({"session_id":B,"source":"startup","cwd":lab.home.join(".codex/memories"),"hook_event_name":"SessionStart"}).to_string().as_bytes()).unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "{result:?}");
    assert!(result.stdout.is_empty());
    assert_eq!(lab.local_state(), before);
}

/// An exact supervisor key selects its own transcript while branch-only callers stay ambiguous.
#[test]
fn supervisor_settles_its_exact_native_link_and_checks_the_branch() {
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "A turn 1", "A answer 1"));
    lab.run(&["init", "qa"]);
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);
    let path = lab
        .agit_home
        .join("store/claude-code")
        .join(format!("{B}.json"));
    fs::write(&path, serde_json::json!({"cwd":lab.home.join(".codex/memories"),"owner":"me","agent":"qa","branch":"s1"}).to_string()).unwrap();
    lab.append(A, &lab.turn(A, 2, "A turn 2", "A answer 2"));
    let native = serde_json::json!({"runtime":"claude-code","session_id":A}).to_string();
    let before = lab.local_state();
    let wrong = lab
        .agit(&["commit", "--from-supervisor"])
        .env("AGIT_SESSION", "me/qa@wrong")
        .env("AGIT_SETTLEMENT_NATIVE", &native)
        .output()
        .unwrap();
    assert!(!wrong.status.success(), "{wrong:?}");
    assert_eq!(lab.local_state(), before);
    let ambiguous = lab.agit(&["commit", "me/qa@s1"]).output().unwrap();
    assert!(!ambiguous.status.success());
    let settled = lab
        .agit(&["commit", "--from-supervisor"])
        .env("AGIT_SESSION", "me/qa@s1")
        .env("AGIT_SETTLEMENT_NATIVE", &native)
        .output()
        .unwrap();
    assert!(settled.status.success(), "{settled:?}");
    assert_eq!(
        turn_subjects(&lab.run(&["log", "me/qa@s1", "--oneline"])),
        vec!["A turn 1", "A turn 2"]
    );
    assert!(path.exists(), "Unrelated native evidence must be retained");
}

/// The branch's team memory reaches a Codex conversation only from agit's own launch for that
/// branch, as the session-start hook's additional context: a fresh conversation that takes its
/// launch's claim (`agit new`) and an already-linked conversation its resume launch started
/// (`agit run`) get the index; a conversation under the same variables that isn't that launch's
/// gets none, and neither does the branch's own conversation resumed under variables an earlier
/// launch left. The index names each memory file and its first heading and never a body line, a
/// name that isn't short printable text is left out of it (its copy still written), and
/// the copies it points at are agit's own: an edit there is written over at the next launch and
/// never reaches the branch. A hook that gave the index to every conversation under the variables
/// would hand one launch's team memory to another conversation; one that took body lines would put
/// whatever a memory file says into the model's instructions.
#[test]
fn a_codex_conversation_gets_the_team_memory_only_from_its_own_launch() {
    const C: &str = "cccccccc-0000-4000-8000-000000000003";
    const F: &str = "ffffffff-0000-4000-8000-000000000006";
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "A turn 1", "A answer 1"));
    lab.run(&["init", "qa"]);
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);
    // The branch's memory: a first heading and a body line.
    let repo = lab.agit_home.join("repos/me/qa");
    let git = |args: &[&str], index: Option<&std::path::Path>| -> String {
        let mut command = Command::new("git");
        command
            .current_dir(&repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@test.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@test.invalid");
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        let out = command.output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let body = "# Decisions\n\nBODY-LINE-284 ship on Fridays\n";
    let source = lab.home.join("decisions.md");
    fs::write(&source, body).unwrap();
    let blob = git(&["hash-object", "-w", source.to_str().unwrap()], None);
    let index = lab.home.join("memory-index");
    git(&["read-tree", "refs/heads/s1"], Some(&index));
    // Beside it, files whose names run long or hold a newline: a name is text whoever can push
    // wrote, as a heading is, so neither enters the index.
    let long_name = format!("{}.md", "n".repeat(120));
    for name in ["decisions.md", long_name.as_str(), "evil\n- injected.md"] {
        git(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},memory/{name}"),
            ],
            Some(&index),
        );
    }
    let tree = git(&["write-tree"], Some(&index));
    let commit = git(
        &["commit-tree", &tree, "-p", "refs/heads/s1", "-m", "memory"],
        None,
    );
    git(&["update-ref", "refs/heads/s1", &commit], None);

    let launches = lab.agit_home.join("launch-claims/codex");
    fs::create_dir_all(&launches).unwrap();
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let claim = |id: &str, start: &str| {
        fs::write(
            launches.join(format!("{id}.json")),
            serde_json::json!({"slug":"me/qa","branch":"s1","cwd":lab.work,"created_ms":created,
                "start":start})
            .to_string(),
        )
        .unwrap();
    };
    let start = |source: &str, session: &str, env: &[(&str, &str)]| -> serde_json::Value {
        let dir = lab.home.join(".codex/sessions/2026/11/06");
        fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join(format!("rollout-2026-11-06T00-00-00-{session}.jsonl"));
        if !transcript.exists() {
            fs::write(
                &transcript,
                format!(
                    "{}\n",
                    serde_json::json!({"timestamp":"2026-11-06T00:00:00Z","type":"session_meta",
                        "payload":{"id":session,"timestamp":"2026-11-06T00:00:00Z","cwd":lab.work,
                            "originator":"codex_cli_rs","cli_version":"0.167.0","source":"cli"}})
                ),
            )
            .unwrap();
        }
        let mut command = lab.agit(&["hooks", "ingest", "--runtime", "codex"]);
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                serde_json::json!({"session_id":session,"source":source,"cwd":lab.work,
                    "transcript_path":transcript,"hook_event_name":"SessionStart"})
                .to_string()
                .as_bytes(),
            )
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if text.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&text).unwrap()
        }
    };
    let context = |response: &serde_json::Value| {
        response["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .map(str::to_owned)
    };

    // `agit new`: a fresh conversation takes its launch's claim and gets the index.
    let fresh = "11111111-2222-4333-8444-555555555555";
    claim(fresh, "startup");
    let launched = [("AGIT_SESSION", "me/qa@s1"), ("AGIT_LAUNCH", fresh)];
    let index = context(&start("startup", B, &launched)).expect("its own launch gets the index");
    assert!(index.contains("decisions.md: Decisions"), "{index}");
    assert!(
        !index.contains("BODY-LINE-284"),
        "never a body line: {index}"
    );
    assert!(
        !index.contains("nnnnnnnn"),
        "a long name stays out: {index}"
    );
    assert!(
        !index.contains("injected"),
        "a name with a newline stays out: {index}"
    );
    // Another conversation under the same variables gets none.
    let foreign = start("startup", C, &launched);
    assert_eq!(context(&foreign), None, "{foreign}");

    // The copies are agit's own: an edit there is written over at the next launch, and the branch
    // keeps its memory.
    let copy = index
        .split(" are in ")
        .nth(1)
        .and_then(|rest| rest.split(". ").next())
        .map(|folder| std::path::Path::new(folder).join("decisions.md"))
        .expect("the index names its folder");
    assert_eq!(fs::read_to_string(&copy).unwrap(), body);
    assert!(
        copy.with_file_name(&long_name).is_file(),
        "its copy is still written"
    );
    fs::write(&copy, "# Decisions\n\nedited in the copy\n").unwrap();

    // `agit run`: a conversation already linked to the branch, started by its resume launch,
    // gets the index too, from the branch, not from the edited copy.
    let resumed = "33333333-4444-4555-8666-777777777777";
    fs::write(
        launches.join(format!("{resumed}.json")),
        serde_json::json!({"slug":"me/qa","branch":"s1","cwd":lab.work,"created_ms":created,
            "start":"resume","thread":B})
        .to_string(),
    )
    .unwrap();
    let rerun = [("AGIT_SESSION", "me/qa@s1"), ("AGIT_LAUNCH", resumed)];
    let again = context(&start("resume", B, &rerun)).expect("its resume launch gets the index");
    assert!(again.contains("decisions.md: Decisions"), "{again}");
    assert_eq!(
        fs::read_to_string(&copy).unwrap(),
        body,
        "the copy is written over"
    );
    assert_eq!(
        git(&["show", "refs/heads/s1:memory/decisions.md"], None),
        body.trim_end()
    );
    // A conversation resumed under that launch's variables that isn't the one it resumes: none.
    let other = start("resume", F, &rerun);
    assert_eq!(context(&other), None, "{other}");
    // The branch's own conversation, resumed inside a Codex whose variables an earlier launch left
    // (no launch claim of its own): none either. It holds the branch, but not by this launch.
    let leaked = start("resume", B, &[("AGIT_SESSION", "me/qa@s1")]);
    assert_eq!(context(&leaked), None, "{leaked}");
}

/// Codex runs every conversation's hooks in one shared process, which keeps the variables of
/// whichever agit launch started it. A fresh Codex conversation holds the branch only its own
/// launch made a claim for, and only once: a second conversation under the same variables, in the
/// same folder or another, records nothing and is told so. An older link that holds a branch on
/// the variables' word alone, beside another conversation, keeps that branch off the Hub, and
/// `agit status` says so.
#[test]
fn a_codex_conversation_holds_only_the_branch_its_own_launch_claimed() {
    const C: &str = "cccccccc-0000-4000-8000-000000000003";
    const D: &str = "dddddddd-0000-4000-8000-000000000004";
    let lab = Lab::new();
    lab.append(A, &lab.turn(A, 1, "A turn 1", "A answer 1"));
    lab.run(&["init", "qa"]);
    lab.run(&[
        "import",
        A,
        "--from",
        "claude-code",
        "--into",
        "me/qa@s1",
        "--independent",
    ]);
    let launch = "11111111-2222-4333-8444-555555555555";
    let launches = lab.agit_home.join("launch-claims/codex");
    fs::create_dir_all(&launches).unwrap();
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    fs::write(
        launches.join(format!("{launch}.json")),
        serde_json::json!({"slug":"me/qa","branch":"s1","cwd":lab.work,"created_ms":created,"start":"startup"})
            .to_string(),
    )
    .unwrap();
    // A Codex rollout begins with its session header, naming the folder it started in.
    let rollout = |session: &str, cwd: &std::path::Path| {
        let dir = lab.home.join(".codex/sessions/2026/11/06");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-11-06T00-00-00-{session}.jsonl"));
        if path.exists() {
            return path;
        }
        fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::json!({"timestamp":"2026-11-06T00:00:00Z","type":"session_meta",
                    "payload":{"id":session,"timestamp":"2026-11-06T00:00:00Z","cwd":cwd,
                        "originator":"codex_cli_rs","cli_version":"0.167.0","source":"cli"}})
            ),
        )
        .unwrap();
        path
    };
    let hook_as =
        |source: &str, action: &str, session: &str, cwd: &std::path::Path, env: &[(&str, &str)]| {
            let transcript = rollout(session, cwd);
            let mut command = lab.agit(&["hooks", action, "--runtime", "codex"]);
            for (name, value) in env {
                command.env(name, value);
            }
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(
                    serde_json::json!({"session_id":session,"source":source,"cwd":cwd,
                    "transcript_path":transcript,
                    "hook_event_name":if action == "ingest" {"SessionStart"} else {"Stop"}})
                    .to_string()
                    .as_bytes(),
                )
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
    let hook = |action: &str, session: &str, cwd: &std::path::Path, env: &[(&str, &str)]| {
        hook_as("startup", action, session, cwd, env)
    };
    let start = |session: &str, cwd: &std::path::Path, env: &[(&str, &str)]| {
        hook("ingest", session, cwd, env)
    };
    let link = |session: &str| -> Option<serde_json::Value> {
        fs::read(
            lab.agit_home
                .join("store/codex")
                .join(format!("{session}.json")),
        )
        .ok()
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
    };
    let launched = [("AGIT_SESSION", "me/qa@s1"), ("AGIT_LAUNCH", launch)];
    let elsewhere = lab.home.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    for (session, cwd) in [(B, &lab.work), (C, &lab.work), (D, &elsewhere)] {
        rollout(session, cwd);
    }

    // The launched conversation takes its launch's claim.
    start(B, &lab.work, &launched);
    let b = link(B).expect("the launched conversation holds its branch");
    assert_eq!(b["branch"], "s1");
    assert_eq!(b["claimed_by"], "launch");
    // Another conversation under the same variables, in the same folder: nothing, and it's told.
    let said = start(C, &lab.work, &launched);
    assert!(link(C).is_none(), "{:?}", link(C));
    assert!(
        said.contains("isn't saving this Codex conversation"),
        "{said}"
    );
    // A launch that resumes a conversation makes its claim for a resumed session: a fresh Codex
    // conversation in the same folder, under that launch's variables, can't take it; the resumed
    // one does.
    const E: &str = "eeeeeeee-0000-4000-8000-000000000005";
    const F: &str = "ffffffff-0000-4000-8000-000000000006";
    let resumed = "22222222-3333-4444-8555-666666666666";
    fs::write(
        launches.join(format!("{resumed}.json")),
        serde_json::json!({"slug":"me/qa","branch":"s1","cwd":lab.work,"created_ms":created,"start":"resume"})
            .to_string(),
    )
    .unwrap();
    let resume_env = [("AGIT_SESSION", "me/qa@s1"), ("AGIT_LAUNCH", resumed)];
    rollout(E, &lab.work);
    rollout(F, &lab.work);
    let said = start(E, &lab.work, &resume_env);
    assert!(link(E).is_none(), "{:?}", link(E));
    assert!(said.contains("isn't saving"), "{said}");
    hook_as("resume", "ingest", F, &lab.work, &resume_env);
    let f = link(F).expect("the resumed conversation takes its launch's claim");
    assert_eq!(f["claimed_by"], "launch");
    // A resume launch whose Codex session is already linked by another path: its claim is retired
    // when that session starts, so another conversation resumed in the same folder, inside the
    // window and under the same variables, gets nothing.
    const G: &str = "abababab-0000-4000-8000-000000000007";
    let rejoined = "33333333-4444-4555-8666-777777777777";
    fs::write(
        launches.join(format!("{rejoined}.json")),
        serde_json::json!({"slug":"me/qa","branch":"s1","cwd":lab.work,"created_ms":created,"start":"resume"})
            .to_string(),
    )
    .unwrap();
    let rejoin_env = [("AGIT_SESSION", "me/qa@s1"), ("AGIT_LAUNCH", rejoined)];
    hook_as("resume", "ingest", B, &lab.work, &rejoin_env);
    assert!(
        !launches.join(format!("{rejoined}.json")).exists(),
        "the claim is retired"
    );
    rollout(G, &lab.work);
    let said = hook_as("resume", "ingest", G, &lab.work, &rejoin_env);
    assert!(link(G).is_none(), "{:?}", link(G));
    assert!(said.contains("isn't saving"), "{said}");
    // A resume launch's Codex may continue the resumed conversation as a new one that names it as
    // its origin, and report that start as a fresh one: it still takes the launch's claim, while a
    // fresh conversation that names no origin can't.
    const K: &str = "cdcdcdcd-0000-4000-8000-000000000008";
    const ORIGINAL: &str = "efefefef-0000-4000-8000-000000000009";
    let continued = "44444444-5555-4666-8777-888888888888";
    fs::write(
        launches.join(format!("{continued}.json")),
        serde_json::json!({"slug":"me/qa","branch":"s1","cwd":lab.work,"created_ms":created,
            "start":"resume","thread":ORIGINAL})
        .to_string(),
    )
    .unwrap();
    let continue_env = [("AGIT_SESSION", "me/qa@s1"), ("AGIT_LAUNCH", continued)];
    const L: &str = "dededede-0000-4000-8000-00000000000a";
    rollout(L, &lab.work);
    let said = start(L, &lab.work, &continue_env);
    assert!(link(L).is_none(), "{:?}", link(L));
    assert!(said.contains("isn't saving"), "{said}");
    let dir = lab.home.join(".codex/sessions/2026/11/06");
    fs::write(
        dir.join(format!("rollout-2026-11-06T00-00-00-{K}.jsonl")),
        format!(
            "{}\n",
            serde_json::json!({"timestamp":"2026-11-06T00:00:00Z","type":"session_meta",
                "payload":{"id":K,"forked_from_id":ORIGINAL,"timestamp":"2026-11-06T00:00:00Z",
                    "cwd":lab.work,"originator":"codex_cli_rs","cli_version":"0.167.0","source":"cli"}})
        ),
    )
    .unwrap();
    // Nor can another conversation resumed in the folder that doesn't continue it.
    const M: &str = "fafafafa-0000-4000-8000-00000000000b";
    rollout(M, &lab.work);
    let said = hook_as("resume", "ingest", M, &lab.work, &continue_env);
    assert!(link(M).is_none(), "{:?}", link(M));
    assert!(said.contains("isn't saving"), "{said}");
    start(K, &lab.work, &continue_env);
    let k = link(K).expect("the continued conversation takes its launch's claim");
    assert_eq!(k["claimed_by"], "launch");
    assert_eq!(k["branch"], "s1");
    // Under the variables of an older launch that made no claim, in another folder: the same.
    let before = lab.local_state();
    let said = start(D, &elsewhere, &[("AGIT_SESSION", "me/qa@s1")]);
    assert!(said.contains("isn't saving"), "{said}");
    let changed = |before: &[(String, State)], after: &[(String, State)]| -> Vec<String> {
        let names = |state: &[(String, State)]| -> std::collections::BTreeSet<String> {
            state.iter().map(|(name, _)| name.clone()).collect()
        };
        names(after)
            .symmetric_difference(&names(before))
            .cloned()
            .collect()
    };
    let after = lab.local_state();
    assert_eq!(
        changed(&before, &after),
        Vec::<String>::new(),
        "nothing is recorded for it"
    );
    assert_eq!(after, before);
    // Its turn's end records nothing either.
    let before = lab.local_state();
    hook(
        "settle",
        D,
        &elsewhere,
        &[("AGIT_SESSION", "me/qa@s1"), ("AGIT_LAUNCH", launch)],
    );
    // Only the note that Codex runs Stop hooks at all, which `agit on` reads, is written.
    let unchanged = |state: Vec<(String, State)>| -> Vec<(String, State)> {
        state
            .into_iter()
            .filter(|(name, _)| !name.ends_with("hooks-seen/codex-stop"))
            .collect()
    };
    let after = unchanged(lab.local_state());
    let before = unchanged(before);
    assert_eq!(changed(&before, &after), Vec::<String>::new());
    assert_eq!(after, before);

    // A link from before launches made claims, on the variables' word alone, beside another
    // conversation on the same branch: the branch stays on this computer.
    fs::write(
        lab.agit_home.join("store/codex").join(format!("{D}.json")),
        serde_json::json!({"cwd":lab.work,"owner":"me","agent":"qa","branch":"s1"}).to_string(),
    )
    .unwrap();
    let status = lab.run(&["status"]);
    assert!(
        status.contains("me/qa@s1 may hold another conversation's turns"),
        "{status}"
    );
    // The way on leads with the fork; lifting the hold is named second.
    let fork = status.find("agit fork me/qa@s1").expect("names the fork");
    let unhold = status
        .find("agit branch unhold s1")
        .expect("names the unhold");
    assert!(fork < unhold, "{status}");
    let pushed = lab.agit(&["push", "me/qa@s1"]).output().unwrap();
    assert_eq!(pushed.status.code(), Some(7), "{pushed:?}");
    assert!(
        String::from_utf8_lossy(&pushed.stderr).contains("will not be uploaded"),
        "{pushed:?}"
    );
    // Lifting the hold asks; with nobody to ask it changes nothing and names the command with
    // --yes. Both ways it says the branch's turns upload as they are.
    let unheld = lab
        .agit(&["branch", "unhold", "s1", "--repo", "me/qa"])
        .output()
        .unwrap();
    assert_ne!(unheld.status.code(), Some(0), "{unheld:?}");
    let said = String::from_utf8_lossy(&unheld.stderr).into_owned();
    assert!(
        said.contains("agit branch unhold s1 --repo me/qa --yes"),
        "{said}"
    );
    assert!(said.contains("turns as they are"), "{said}");
    // Moved by agit itself before any hook or push has seen it, the link leaves the hold behind.
    let holds = lab.agit_home.join("upload-holds");
    let legacy = lab.agit_home.join("store/codex").join(format!("{D}.json"));
    let image = fs::read(&legacy).unwrap();
    fs::remove_dir_all(&holds).unwrap();
    rollout(D, &lab.work);
    let moved = lab.run(&[
        "import",
        D,
        "--from",
        "codex",
        "--into",
        "me/qa@elsewhere",
        "--independent",
    ]);
    assert_eq!(
        link(D).and_then(|link| link["branch"].as_str().map(str::to_owned)),
        Some("elsewhere".to_owned()),
        "{moved}"
    );
    assert!(holds.exists(), "the move records the hold first");
    let pushed = lab.agit(&["push", "me/qa@s1"]).output().unwrap();
    assert_eq!(pushed.status.code(), Some(7), "{pushed:?}");
    fs::write(&legacy, image).unwrap();

    // Seen first by `agit status` (which changes nothing) and then by any Codex hook, the hold is
    // recorded there, so it stays when the link goes before any push.
    let status_only = lab.agit_home.join("upload-holds");
    let _ = fs::remove_dir_all(&status_only);
    let shown = lab.run(&["status"]);
    assert!(
        shown.contains("may hold another conversation's turns"),
        "{shown}"
    );
    assert!(!status_only.exists(), "status records nothing");
    start(C, &lab.work, &[]);
    assert!(status_only.exists(), "a Codex hook records the hold");
    // The hold stays when the link goes.
    fs::remove_file(lab.agit_home.join("store/codex").join(format!("{D}.json"))).unwrap();
    let pushed = lab.agit(&["push", "me/qa@s1"]).output().unwrap();
    assert_eq!(pushed.status.code(), Some(7), "{pushed:?}");
    // With --yes it lets the branch go, saying what that means; the push is no longer held.
    let unheld = lab
        .agit(&["branch", "unhold", "s1", "--repo", "me/qa", "--yes"])
        .output()
        .unwrap();
    assert!(unheld.status.success(), "{unheld:?}");
    assert!(
        String::from_utf8_lossy(&unheld.stderr).contains("turns as they are"),
        "{unheld:?}"
    );
    let pushed = lab.agit(&["push", "me/qa@s1"]).output().unwrap();
    assert!(
        !String::from_utf8_lossy(&pushed.stderr)
            .contains("will not be uploaded from this computer"),
        "{pushed:?}"
    );
}
