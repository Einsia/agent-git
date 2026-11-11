#![cfg(all(unix, feature = "cli"))]

use sha2::{Digest, Sha256};
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

#[path = "support/startup_cache.rs"]
mod startup_cache;

fn command(root: &Path, args: &[&str]) -> Command {
    command_from(Path::new(env!("CARGO_BIN_EXE_agit")), root, args)
}

/// [`command`] run from the agit at `exe`.
fn command_from(exe: &Path, root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(exe);
    let path = std::iter::once(root.join("bin"))
        .chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ))
        .collect::<Vec<_>>();
    command
        .args(args)
        .current_dir(root.join("workspace"))
        .env("PATH", std::env::join_paths(path).unwrap())
        .env("AGIT_HOME", root.join("agit-home"))
        .env("HOME", root.join("runtime-home"))
        .env("AGIT_HUB_URL", "http://127.0.0.1:9")
        .env("AGIT_SECRETS_KEYSTORE", "file")
        .env("AGIT_TELEMETRY", "off")
        .env("CI", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_RC")
        .env_remove("CLAUDE_CONFIG_DIR");
    command
}

/// Puts a `claude` first on the daemon's PATH, so a model launch runs this stand-in rather than
/// whatever Claude Code the machine has. It answers the version probe; any other invocation is a
/// model launch and leaves `claude-launched` in `root`.
fn install_claude(root: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '2.1.310 (Claude Code)'; exit 0; fi\necho \"$*\" >> '{}'\nexit 1\n",
            root.join("claude-launched").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Stop<'a>(&'a Path);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = command(self.0, &["rc", "local", "stop"]).output();
    }
}

/// A completed turn on disk and its archive job, for a daemon to recover: a landed repository
/// whose branch does not yet hold the turn, the native transcript, and the roster and workspace
/// rows that bind it. The job is written by [`CompletedTurn::queue`], so a test decides when the
/// daemon may see it.
struct CompletedTurn {
    repo: std::path::PathBuf,
    branch: &'static str,
    transcript: std::path::PathBuf,
    text: String,
    job_path: std::path::PathBuf,
    job: String,
}

impl CompletedTurn {
    fn queue(&self) {
        std::fs::write(&self.job_path, &self.job).unwrap();
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", &format!("refs/heads/{}", self.branch)])
    }
}

fn completed_turn(root: &Path) -> CompletedTurn {
    let cwd = root.join("workspace");
    let home = root.join("agit-home");
    let rc = home.join("desktop-rc");
    let runtime_home = root.join("runtime-home");
    for path in [&cwd, &rc, &runtime_home] {
        std::fs::create_dir_all(path).unwrap();
    }
    startup_cache::seed(&home);
    let machine = "00000000-0000-0000-0000-000000000001";
    let repository_id = "00000000-0000-0000-0000-000000000002";
    let native = "00000000-0000-0000-0000-000000000003";
    let branch = "conversation";
    let slug = "desktop-fixture/project";
    let lineage = format!("{slug}@{branch}");
    let repo = home.join("repos").join(slug);
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(rc.join("identity.json"), serde_json::json!({
        "machine_fingerprint":machine,"display_name":"fixture","created_at":"2026-01-01T00:00:00Z"
    }).to_string()).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "agit.desktopIdentity", repository_id]);
    git(&[
        "config",
        "agit.desktopAuthority",
        &format!("local:{machine}"),
    ]);
    git(&["config", "agit.autoPush", "false"]);
    // As an earlier agit left it: a main line committed under the local owner's name, which the
    // repository's configuration still carries.
    agit::domain::meta::write(&repo, &agit::domain::meta::Meta::new_file_line()).unwrap();
    git(&["config", "user.name", "desktop-fixture"]);
    git(&["config", "user.email", "desktop-fixture@agit.local"]);
    git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "agit: init (main file line)"]);
    let transcript_dir = runtime_home
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&cwd));
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let transcript = transcript_dir.join(format!("{native}.jsonl"));
    let records = [
        serde_json::json!({"type":"user","sessionId":native,"uuid":"u","cwd":cwd,"message":{"role":"user","content":"inspect the project"}}),
        serde_json::json!({"type":"assistant","sessionId":native,"uuid":"a","parentUuid":"u","cwd":cwd,"message":{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"Bash","input":{"command":"printf ok"}}]}}),
        serde_json::json!({"type":"user","sessionId":native,"uuid":"r","parentUuid":"a","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":"ok"}]}}),
        serde_json::json!({"type":"assistant","sessionId":native,"uuid":"f","parentUuid":"r","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"inspection completed"}]}}),
    ];
    let text = records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
    std::fs::write(&transcript, &text).unwrap();
    let landed = command(
        &root,
        &[
            "rc",
            "land",
            "--local-owner",
            "--slug",
            slug,
            "--agent-id",
            repository_id,
            "--branch",
            branch,
            "--runtime",
            "claude-code",
            "--session",
            native,
            "--cwd",
            cwd.to_str().unwrap(),
        ],
    )
    .output()
    .unwrap();
    assert!(
        landed.status.success(),
        "landing: {}",
        String::from_utf8_lossy(&landed.stderr)
    );
    let capture = serde_json::json!({"kind":"device_local","agent_id":repository_id});
    let job = serde_json::json!({
        "version":1,"logical":"logical-fixture","native":native,"runtime":"claude-code","native_source":null,
        "cwd":cwd,"lineage":lineage,"repository_id":repository_id,"capture":capture,"turn_id":"turn-one",
        "transcript":transcript,"prefix_bytes":text.len(),"prefix_hash":format!("{:x}",Sha256::digest(text.as_bytes())),
        "required_bytes":text.len(),"archive_handoff":null
    });
    let coordinates = serde_json::json!([
        "logical-fixture",
        "claude-code",
        null,
        native,
        lineage,
        repository_id,
        "turn-one"
    ]);
    let filename = format!(
        "{:x}.json",
        Sha256::digest(serde_json::to_vec(&coordinates).unwrap())
    );
    let jobs = rc.join("pending-archives");
    std::fs::create_dir_all(&jobs).unwrap();
    let job_path = jobs.join(filename);
    let job = job.to_string();
    // The conversation runs in a project the device's RC workspace binds, as a Hub-started one does.
    std::fs::write(
        rc.join("workspaces.json"),
        serde_json::json!({"workspaces":{"local-owner":{"project":cwd}}}).to_string(),
    )
    .unwrap();
    std::fs::write(rc.join("sessions.json"), serde_json::json!({
        "captures":{"logical-fixture":capture},"sessions":{"logical-fixture":{
            "runtime":"claude-code","thread_id":native,"cwd":cwd,"workspace_id":"local-owner",
            "agit_session":lineage,"expected_agent_id":repository_id,"prior_threads":[],"ever_dangerous":false
        }}
    }).to_string()).unwrap();
    CompletedTurn {
        repo,
        branch,
        transcript,
        text,
        job_path,
        job,
    }
}

/// A fresh daemon recovers the standard archive without a viewer, prompt, or runtime launch.
/// Disabled publication retains the durable job across another daemon restart. Once publication
/// is on, recovery's upload is recorded like any automatic upload: one that fails, here because
/// nobody is signed in, keeps the job and is listed by `agit status`. A recovery that pushed
/// without recording would leave that failure in the daemon's log alone.
#[test]
fn daemon_restart_recovers_a_completed_turn_without_resuming_the_model() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let rc = root.join("agit-home").join("desktop-rc");
    let turn = completed_turn(&root);
    turn.queue();
    let (repo, branch, transcript, text, job_path) = (
        turn.repo.clone(),
        turn.branch,
        turn.transcript.clone(),
        turn.text.clone(),
        turn.job_path.clone(),
    );
    let git = |args: &[&str]| turn.git(args);
    let before = git(&["rev-parse", &format!("refs/heads/{branch}")]);
    let _stop = Stop(&root);
    for pass in 0..2 {
        let started = command(&root, &["rc", "local", "start", "--detach"])
            .output()
            .unwrap();
        assert!(
            started.status.success(),
            "daemon start: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let head = git(&["rev-parse", &format!("refs/heads/{branch}")]);
            let attempts = std::fs::read_dir(&rc)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry.file_name().to_string_lossy().starts_with("agitd-")
                        && entry.path().extension().is_some_and(|ext| ext == "log")
                })
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .map(|text| {
                    text.matches("completed turn archive remains pending (logical-fixture)")
                        .count()
                })
                .sum::<usize>();
            if head != before && attempts > pass {
                let (log, view) = agit::domain::storage::materialize_pair_at(&repo, &head).unwrap();
                assert!(
                    log.contains("inspection completed")
                        && view.contains("printf ok")
                        && view.contains("tool_result")
                );
                assert!(job_path.exists(), "unpublished work must remain durable");
                // Every commit the line adds to main, the land's and the recovered turn's, carries
                // the repository's own identity, though an earlier agit left another in its
                // configuration: no machine or local owner name, and no account email.
                let headers = git(&[
                    "log",
                    "--format=%an <%ae>%n%cn <%ce>",
                    &format!("refs/heads/main..{head}"),
                ]);
                assert!(!headers.is_empty());
                for line in headers.lines() {
                    assert_eq!(
                        line,
                        "agit-00000000-0000-0000-0000-000000000002 \
                         <00000000-0000-0000-0000-000000000002@agit.local>",
                        "{headers}"
                    );
                }
                assert!(!headers.contains("000000000001"), "{headers}");
                assert!(!headers.contains("desktop-fixture"), "{headers}");
                assert_eq!(
                    std::fs::read_to_string(&transcript).unwrap(),
                    text,
                    "recovery cannot append model output"
                );
                let status = command(&root, &["rc", "status", "--json"])
                    .output()
                    .unwrap();
                assert!(status.status.success());
                let status = String::from_utf8(status.stdout).unwrap();
                assert!(
                    !status.contains("logical-fixture"),
                    "recovery must not start a live model: {status}"
                );
                if pass == 1 {
                    assert_eq!(
                        git(&["rev-list", "--count", &format!("refs/heads/{branch}")]),
                        "3"
                    );
                }
                break;
            }
            assert!(
                Instant::now() < deadline,
                "daemon failed to recover the completed prefix"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            command(&root, &["rc", "local", "stop"])
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    let identity = serde_json::json!({
        "hub": "http://127.0.0.1:9", "agent_id": "00000000-0000-4000-8000-000000000009",
    });
    git(&["config", "agit.autoPush", "true"]);
    git(&["config", "agit.remoteIdentity", &identity.to_string()]);
    git(&[
        "config",
        "agit.desktopPublication",
        &serde_json::json!({
            "version": 1, "local_agent_id": "00000000-0000-0000-0000-000000000002",
            "repository": "alice/notes", "identity": identity,
        })
        .to_string(),
    ]);
    let started = command(&root, &["rc", "local", "start", "--detach"])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "daemon start: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let listed = loop {
        let status = command(&root, &["status", "--json"]).output().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap_or_default();
        let items = value["result"]["value"]["uploads_not_through"]["items"].clone();
        if items.as_array().is_some_and(|items| !items.is_empty()) {
            break items;
        }
        assert!(
            Instant::now() < deadline,
            "recovery's upload left no record: {}\n{}",
            String::from_utf8_lossy(&status.stdout),
            daemon_output(&rc)
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(listed[0]["target"], "desktop-fixture/project@conversation");
    assert_eq!(listed[0]["state"], "failed");
    assert!(job_path.exists(), "a failed publication keeps its job");
}

/// Turns a session left waiting to be saved, because its daemon couldn't start its saving step,
/// stop being listed by `agit status` once a restarted daemon's archive recovery saves them. The
/// session is gone by then, so only recovery's save can end the wait.
#[test]
fn a_wait_to_be_saved_ends_when_recovery_saves_the_turn() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let rc = root.join("agit-home").join("desktop-rc");
    let turn = completed_turn(&root);
    turn.queue();
    let target = "desktop-fixture/project@conversation";
    // The record the session left: one file per conversation, named by a digest of its target.
    let waits = root.join("agit-home").join("settle-blocked");
    std::fs::create_dir_all(&waits).unwrap();
    std::fs::write(
        waits.join(format!(
            "{}.json",
            &blake3::hash(target.as_bytes()).to_hex()[..32]
        )),
        serde_json::json!({"target": target, "since": 1_700_000_000u64}).to_string(),
    )
    .unwrap();
    let save_waits = || {
        let status = command(&root, &["status", "--json"]).output().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap_or_default();
        value["result"]["value"]["save_waits"].clone()
    };
    assert_eq!(save_waits()[0]["target"], target, "the wait is listed");
    let before = turn.head();
    let _stop = Stop(&root);
    let started = command(&root, &["rc", "local", "start", "--detach"])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "daemon start: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    // The wait ends just after the commit moves the branch, so both are waited for.
    let deadline = Instant::now() + Duration::from_secs(20);
    while turn.head() == before || save_waits() != serde_json::json!([]) {
        assert!(
            Instant::now() < deadline,
            "the turn was not recovered (head moved: {}) or still waits: {}\n{}",
            turn.head() != before,
            save_waits(),
            daemon_output(&rc)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A daemon whose executable an upgrade replaces while it runs still settles its work: every
/// subprocess it starts for it (landing, the commit, its privacy worker) runs the daemon's own
/// build, not what now sits at the installed path. One that started from the path would run the
/// new binary's arguments and replies as its own, or find nothing there once the file is renamed
/// over, and the turn would stay unsettled until the daemon is replaced.
#[test]
fn a_binary_replaced_under_a_running_daemon_still_settles_its_turns() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let turn = completed_turn(&root);
    let installed = root.join("install").join("agit");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_agit"), &installed).unwrap();
    let before = turn.head();
    let _stop = Stop(&root);
    let started = command_from(&installed, &root, &["rc", "local", "start", "--detach"])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "daemon start: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    // The upgrade renames another program over the installed path; it records every start.
    let ran = root.join("replacement-ran");
    let replacement = root.join("install").join(".agit-new");
    std::fs::write(
        &replacement,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 1\n", ran.display()),
    )
    .unwrap();
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&replacement, &installed).unwrap();
    turn.queue();

    let deadline = Instant::now() + Duration::from_secs(60);
    while turn.head() == before {
        assert!(
            Instant::now() < deadline,
            "the replaced daemon did not settle the turn:\n{}",
            daemon_output(&root.join("agit-home").join("desktop-rc"))
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Only the installation watch, which looks for the new build on purpose, may run it.
    for line in std::fs::read_to_string(&ran).unwrap_or_default().lines() {
        assert!(
            line.starts_with("rc local "),
            "a subprocess for the work ran the replaced binary: {line}"
        );
    }
}

/// A privacy worker for the daemon whose first start by an archive commit misses its start-up
/// deadline; every other start runs the real worker. The commit then waits for the privacy
/// check, which the daemon must retry.
fn slow_first_archive_worker(root: &Path) -> std::path::PathBuf {
    slow_first_worker(root, "--from-supervisor", "archive-worker-missed")
}

/// A privacy worker whose first start by a process whose arguments hold `parent` misses its
/// start-up deadline, leaving `missed` in `root`; every other start runs the real worker.
fn slow_first_worker(root: &Path, parent: &str, missed: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let worker = root.join("bin/slow-worker");
    std::fs::write(
        &worker,
        format!(
            "#!/bin/sh\ncase \"$(ps -o args= -p $PPID)\" in *'{parent}'*) if mkdir '{}' 2>/dev/null; then sleep 15; exit 1; fi;; esac\nexec '{}' \"$@\"\n",
            root.join(missed).display(),
            env!("CARGO_BIN_EXE_agit"),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();
    worker
}

/// What the daemon wrote about itself: its startup output (`agitd-*.log`, where it reports a
/// capture or archive that stays pending) and its diagnostics (`diagnostics-*.jsonl`).
fn daemon_output(rc: &Path) -> String {
    let mut files: Vec<_> = std::fs::read_dir(rc)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    (name.starts_with("agitd-") && name.ends_with(".log"))
                        || (name.starts_with("diagnostics-") && name.ends_with(".jsonl"))
                })
        })
        .collect();
    files.sort();
    files
        .iter()
        .map(|path| {
            format!(
                "--- {}\n{}",
                path.display(),
                std::fs::read_to_string(path)
                    .unwrap_or_else(|error| format!("unreadable: {error}"))
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The index locks and checkout transaction journals left in `repo`'s Git directory and in
/// those of its linked worktrees.
fn checkout_leftovers(repo: &Path) -> Vec<std::path::PathBuf> {
    let git = repo.join(".git");
    let linked = std::fs::read_dir(git.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path());
    std::iter::once(git.clone())
        .chain(linked)
        .flat_map(|dir| ["index.lock", "agit-checkout-transaction.json"].map(|name| dir.join(name)))
        .filter(|path| path.exists())
        .collect()
}

/// Opens the FIFO at `path` on a thread of its own, since an open waits for the other end; the
/// receiver gets the open file.
fn open_fifo(
    path: std::path::PathBuf,
    write: bool,
) -> tokio::sync::oneshot::Receiver<std::fs::File> {
    let (opened, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let file = std::fs::OpenOptions::new()
            .read(!write)
            .write(write)
            .open(path)
            .unwrap();
        let _ = opened.send(file);
    });
    receiver
}

/// Watching retains per-turn collection after viewers disconnect and the daemon restarts.
/// Native input stays outside the daemon, failed publication retains archive jobs, and an
/// archive commit that waited for a privacy worker missing its start-up is tried again.
///
/// A stop that lands while the first archived turn's checkout is held after its branch moved and
/// before its refresh lets that refresh finish within the daemon's grace, so it leaves neither an
/// index lock nor a checkout journal. A daemon that ended the archive commit at the stop, or
/// revoked the commit's settlement authority before its archive work finished, would leave the
/// journal, and the lock too when Git was writing the index.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_native_turns_archive_without_a_viewer_or_model_takeover() {
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let cwd = root.join("workspace");
    let home = root.join("agit-home");
    let runtime_home = root.join("runtime-home");
    for path in [&cwd, &home, &runtime_home] {
        std::fs::create_dir_all(path).unwrap();
    }
    startup_cache::seed(&home);
    install_claude(&root);
    let worker = slow_first_archive_worker(&root);
    // The daemon retries within a second, so a failure it backs off from can't outlast the wait
    // below however slow the machine.
    let daemon = |args: &[&str]| {
        let mut command = command(&root, args);
        command
            .env("AGIT_TEST_ARCHIVE_RETRY_MS", "1000")
            .env("AGIT_TEST_PRIVACY_WORKER", &worker);
        command
    };
    let native = "00000000-0000-0000-0000-000000000013";
    let transcript_dir = runtime_home
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&cwd));
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let transcript = transcript_dir.join(format!("{native}.jsonl"));
    let turn = |number: usize| {
        [
            json!({"type":"user","sessionId":native,"uuid":format!("u{number}"),"cwd":cwd,"message":{"role":"user","content":format!("Inspect turn {number}")}}),
            json!({"type":"assistant","sessionId":native,"uuid":format!("a{number}"),"parentUuid":format!("u{number}"),"cwd":cwd,"message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":format!("Observed answer {number}")}]}}),
        ].iter().map(|row| format!("{row}\n")).collect::<String>()
    };
    let first = turn(1);
    std::fs::write(&transcript, &first).unwrap();
    // The first checkout refresh after a branch move waits until the test closes `hold`, and a
    // stopping daemon opens `stopping` as it starts waiting for its archive work.
    let hold = root.join("refresh-hold");
    std::fs::create_dir(&hold).unwrap();
    std::fs::write(hold.join("armed"), "").unwrap();
    let stopping = root.join("daemon-stopping");
    for fifo in [hold.join("hold"), stopping.clone()] {
        use std::os::unix::ffi::OsStrExt as _;
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }
    let _stop = Stop(&root);
    let started = daemon(&["rc", "local", "start", "--detach"])
        .env("AGIT_TEST_CHECKOUT_REFRESH_HOLD", &hold)
        .env("AGIT_TEST_DAEMON_ARCHIVE_WAIT", &stopping)
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let mut bridge = tokio::process::Command::from(command(&root, &["rc", "local", "bridge"]))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = bridge.stdin.take().unwrap();
    let mut lines = BufReader::new(bridge.stdout.take().unwrap()).lines();
    let mut replies = vec![];
    for (id, method, params) in [
        (
            "bind",
            "project.bind",
            json!({"workspace_id":"local-owner","project_id":"observed-project","local_path":cwd}),
        ),
        (
            "watch",
            "session.watch",
            json!({"workspace_id":"local-owner","session_id":native}),
        ),
    ] {
        input
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let row: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                if row["id"] == id {
                    break row;
                }
            }
        })
        .await
        .unwrap();
        assert!(reply.get("error").is_none(), "{reply}");
        replies.push(reply);
    }
    assert_eq!(replies[1]["result"]["read_only"], true);
    let selection = &replies[1]["result"]["native_selection"];
    assert_eq!(selection["session_ref"], native);
    assert_eq!(selection["runtime_session_id"], native);
    assert_eq!(selection["runtime"], "claude-code");
    assert_eq!(selection["project_id"], "observed-project");
    assert_eq!(selection["cwd"], cwd.to_string_lossy().as_ref());
    let logical = replies[1]["result"]["archive_session"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(replies[1]["result"]["session"]["session_id"], logical);
    drop(input);
    tokio::time::timeout(Duration::from_secs(10), bridge.wait())
        .await
        .unwrap()
        .unwrap();
    let mut initial_head = None;
    for number in 1..=2 {
        if number == 2 {
            let held =
                tokio::time::timeout(Duration::from_secs(120), open_fifo(hold.join("hold"), true))
                    .await
                    .expect("the first archived turn's checkout refresh never reached its hold")
                    .unwrap();
            let mut stop = tokio::process::Command::from(command(&root, &["rc", "local", "stop"]));
            let stopped = tokio::spawn(async move { stop.output().await });
            tokio::time::timeout(Duration::from_secs(60), open_fifo(stopping.clone(), false))
                .await
                .expect("the stopping daemon never waited for its archive work")
                .unwrap();
            drop(held);
            let stopped = stopped.await.unwrap().unwrap();
            assert!(
                stopped.status.success(),
                "{}",
                String::from_utf8_lossy(&stopped.stderr)
            );
            let saved: Value = serde_json::from_slice(
                &std::fs::read(home.join("desktop-rc/sessions.json")).unwrap(),
            )
            .unwrap();
            let (slug, _) = saved["sessions"][&logical]["agit_session"]
                .as_str()
                .unwrap()
                .split_once('@')
                .unwrap();
            let left = checkout_leftovers(&home.join("repos").join(slug));
            assert!(
                left.is_empty(),
                "the stop inside the checkout refresh left {left:?}\n{}",
                daemon_output(&home.join("desktop-rc"))
            );
            std::fs::write(&transcript, format!("{first}{}", turn(2))).unwrap();
            assert!(
                daemon(&["rc", "local", "start", "--detach"])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let saved: Value = serde_json::from_slice(
                &std::fs::read(home.join("desktop-rc/sessions.json")).unwrap(),
            )
            .unwrap();
            if let Some(lineage) = saved["sessions"][&logical]["agit_session"].as_str() {
                let (slug, branch) = lineage.split_once('@').unwrap();
                let repo = home.join("repos").join(slug);
                let head = Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(["rev-parse", &format!("refs/heads/{branch}")])
                    .output()
                    .unwrap();
                if head.status.success() {
                    let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
                    if let Ok((_, view)) = agit::domain::storage::materialize_pair_at(&repo, &head)
                        && view.contains(&format!("Observed answer {number}"))
                    {
                        assert!(view.contains("Observed answer 1"));
                        if let Some(before) = &initial_head {
                            let commits = Command::new("git")
                                .arg("-C")
                                .arg(&repo)
                                .args(["rev-list", "--count", &format!("{before}..{head}")])
                                .output()
                                .unwrap();
                            assert_eq!(String::from_utf8(commits.stdout).unwrap().trim(), "1");
                        }
                        initial_head = Some(head);
                        break;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "observed completed turn {number} did not enter its canonical archive\n{}",
                daemon_output(&home.join("desktop-rc"))
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    assert!(
        root.join("archive-worker-missed").exists(),
        "the first archive commit met a worker that missed its start-up"
    );
    let status = command(&root, &["rc", "status", "--json"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8(status.stdout).unwrap().contains(&logical),
        "observation cannot launch a model"
    );
    assert!(
        !root.join("claude-launched").exists(),
        "observation cannot launch a model"
    );
    assert_eq!(
        std::fs::read_to_string(&transcript).unwrap(),
        format!("{first}{}", turn(2))
    );
    assert!(
        std::fs::read_dir(home.join("desktop-rc/pending-archives"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "json"))
    );
}

/// A new observed conversation whose first landing waited for the privacy check (its worker
/// missed its start-up) is landed again, declared a session line, and its finished turn
/// archived. A landing that left the branch it created behind undeclared would have every
/// archive commit refused as a file line's, for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_landing_that_waited_for_the_privacy_check_lands_again_and_archives() {
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let cwd = root.join("workspace");
    let home = root.join("agit-home");
    let runtime_home = root.join("runtime-home");
    for path in [&cwd, &home, &runtime_home] {
        std::fs::create_dir_all(path).unwrap();
    }
    startup_cache::seed(&home);
    install_claude(&root);
    let worker = slow_first_worker(&root, "rc land", "landing-worker-missed");
    let daemon = |args: &[&str]| {
        let mut command = command(&root, args);
        command
            .env("AGIT_TEST_ARCHIVE_RETRY_MS", "1000")
            .env("AGIT_TEST_PRIVACY_WORKER", &worker);
        command
    };
    let native = "00000000-0000-0000-0000-000000000033";
    let transcript_dir = runtime_home
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&cwd));
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let rows = [
        json!({"type":"user","sessionId":native,"uuid":"u1","cwd":cwd,"message":{"role":"user","content":"Inspect turn 1"}}),
        json!({"type":"assistant","sessionId":native,"uuid":"a1","parentUuid":"u1","cwd":cwd,"message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Observed answer 1"}]}}),
    ];
    std::fs::write(
        transcript_dir.join(format!("{native}.jsonl")),
        rows.iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let _stop = Stop(&root);
    assert!(
        daemon(&["rc", "local", "start", "--detach"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let mut bridge = tokio::process::Command::from(command(&root, &["rc", "local", "bridge"]))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = bridge.stdin.take().unwrap();
    let mut lines = BufReader::new(bridge.stdout.take().unwrap()).lines();
    let mut logical = String::new();
    for (id, method, params) in [
        (
            "bind",
            "project.bind",
            json!({"workspace_id":"local-owner","project_id":"observed-project","local_path":cwd}),
        ),
        (
            "watch",
            "session.watch",
            json!({"workspace_id":"local-owner","session_id":native}),
        ),
    ] {
        input
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let row: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                if row["id"] == id {
                    break row;
                }
            }
        })
        .await
        .unwrap();
        assert!(reply.get("error").is_none(), "{reply}");
        if id == "watch" {
            logical = reply["result"]["archive_session"]["session_id"]
                .as_str()
                .unwrap()
                .to_owned();
        }
    }
    drop(input);
    tokio::time::timeout(Duration::from_secs(10), bridge.wait())
        .await
        .unwrap()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let saved: Value = std::fs::read(home.join("desktop-rc/sessions.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        if let Some(lineage) = saved["sessions"][&logical]["agit_session"].as_str() {
            let (slug, branch) = lineage.split_once('@').unwrap();
            let repo = home.join("repos").join(slug);
            let head = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", &format!("refs/heads/{branch}")])
                .output()
                .unwrap();
            if head.status.success() {
                let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
                if let Ok((_, view)) = agit::domain::storage::materialize_pair_at(&repo, &head)
                    && view.contains("Observed answer 1")
                {
                    break;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "the turn of a conversation whose first landing waited did not enter its archive\n{}",
            daemon_output(&home.join("desktop-rc"))
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        root.join("landing-worker-missed").exists(),
        "the first landing met a worker that missed its start-up"
    );
}

/// A job an earlier build queued with its end past the reply, after the `stop_hook_summary`
/// Claude Code writes once its Stop hooks return, finishes: its turn ends where settlement ends
/// it, at the reply, and the archive commit that stops there covers it. The hook record, here
/// with a hook's error output, is not archived and stays in the transcript past the archived
/// end. A coverage check that compared the recorded end alone retried the job for as long as the
/// conversation stayed idle.
#[test]
fn a_job_queued_past_the_reply_finishes_at_settlements_end_of_the_turn() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let cwd = root.join("workspace");
    let home = root.join("agit-home");
    let rc = home.join("desktop-rc");
    let runtime_home = root.join("runtime-home");
    for path in [&cwd, &rc, &runtime_home] {
        std::fs::create_dir_all(path).unwrap();
    }
    startup_cache::seed(&home);
    let machine = "00000000-0000-0000-0000-000000000041";
    let repository_id = "00000000-0000-0000-0000-000000000042";
    let native = "00000000-0000-0000-0000-000000000043";
    let branch = "conversation";
    let slug = "desktop-fixture/project";
    let lineage = format!("{slug}@{branch}");
    let repo = home.join("repos").join(slug);
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(rc.join("identity.json"), serde_json::json!({
        "machine_fingerprint":machine,"display_name":"fixture","created_at":"2026-01-01T00:00:00Z"
    }).to_string()).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "agit.desktopIdentity", repository_id]);
    git(&[
        "config",
        "agit.desktopAuthority",
        &format!("local:{machine}"),
    ]);
    git(&["config", "agit.autoPush", "false"]);
    let transcript_dir = runtime_home
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&cwd));
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let transcript = transcript_dir.join(format!("{native}.jsonl"));
    let hook_output = "hook-error-output-that-stays-local";
    // The shape of a stuck conversation: the reply, then the Stop hooks' summary, then records
    // with no time.
    let reply = [
        serde_json::json!({"type":"user","sessionId":native,"uuid":"u","cwd":cwd,"message":{"role":"user","content":"inspect the project"}}),
        serde_json::json!({"type":"assistant","sessionId":native,"uuid":"f","parentUuid":"u","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"inspection completed"}]}}),
    ];
    let hook = serde_json::json!({"type":"system","subtype":"stop_hook_summary","sessionId":native,"uuid":"h","parentUuid":"f","cwd":cwd,"hookCount":1,"hookInfos":[{"command":"check"}],"hookErrors":[hook_output],"preventedContinuation":false,"stopReason":"","hasOutput":true,"level":"suggestion","isSidechain":false});
    let after = [
        serde_json::json!({"type":"last-prompt","lastPrompt":"inspect the project","sessionId":native}),
        serde_json::json!({"type":"cost-state","sessionId":native,"totalCostUSD":0.001}),
    ];
    let line = |record: &serde_json::Value| format!("{record}\n");
    let replied: String = reply.iter().map(line).collect();
    let queued = format!("{replied}{}", line(&hook));
    let text = format!("{queued}{}", after.iter().map(line).collect::<String>());
    std::fs::write(&transcript, &text).unwrap();
    let landed = command(
        &root,
        &[
            "rc",
            "land",
            "--local-owner",
            "--slug",
            slug,
            "--agent-id",
            repository_id,
            "--branch",
            branch,
            "--runtime",
            "claude-code",
            "--session",
            native,
            "--cwd",
            cwd.to_str().unwrap(),
        ],
    )
    .output()
    .unwrap();
    assert!(
        landed.status.success(),
        "landing: {}",
        String::from_utf8_lossy(&landed.stderr)
    );
    let capture = serde_json::json!({"kind":"device_local","agent_id":repository_id});
    // Queued as an earlier build did: its end after the hook summary.
    let job = serde_json::json!({
        "version":1,"logical":"logical-fixture","native":native,"runtime":"claude-code","native_source":null,
        "cwd":cwd,"lineage":lineage,"repository_id":repository_id,"capture":capture,"turn_id":"turn-one",
        "transcript":transcript,"prefix_bytes":queued.len(),"prefix_hash":format!("{:x}",Sha256::digest(queued.as_bytes())),
        "required_bytes":queued.len(),"archive_handoff":null
    });
    let coordinates = serde_json::json!([
        "logical-fixture",
        "claude-code",
        null,
        native,
        lineage,
        repository_id,
        "turn-one"
    ]);
    let filename = format!(
        "{:x}.json",
        Sha256::digest(serde_json::to_vec(&coordinates).unwrap())
    );
    let jobs = rc.join("pending-archives");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(jobs.join(filename), job.to_string()).unwrap();
    std::fs::write(
        rc.join("workspaces.json"),
        serde_json::json!({"workspaces":{"local-owner":{"project":cwd}}}).to_string(),
    )
    .unwrap();
    std::fs::write(rc.join("sessions.json"), serde_json::json!({
        "captures":{"logical-fixture":capture},"sessions":{"logical-fixture":{
            "runtime":"claude-code","thread_id":native,"cwd":cwd,"workspace_id":"local-owner",
            "agit_session":lineage,"expected_agent_id":repository_id,"prior_threads":[],"ever_dangerous":false
        }}
    }).to_string()).unwrap();
    let before = git(&["rev-parse", &format!("refs/heads/{branch}")]);
    let _stop = Stop(&root);
    let mut started = command(&root, &["rc", "local", "start", "--detach"]);
    started.env("AGIT_TEST_ARCHIVE_RETRY_MS", "1000");
    let started = started.output().unwrap();
    assert!(
        started.status.success(),
        "daemon start: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    // The first attempt's outcome: the job is settled and goes on to publication, which this
    // repository has turned off, or it is reported as not covered.
    let deadline = Instant::now() + Duration::from_secs(60);
    let reported = loop {
        let lines: Vec<String> = daemon_output(&rc)
            .lines()
            .filter(|line| {
                line.starts_with("agitd: completed turn archive")
                    && line.contains("logical-fixture")
            })
            .map(str::to_owned)
            .collect();
        if !lines.is_empty() {
            break lines;
        }
        assert!(
            Instant::now() < deadline,
            "the queued job was never attempted\n{}",
            daemon_output(&rc)
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        !reported
            .iter()
            .any(|line| line.contains("does not cover the completed native turn")),
        "{reported:#?}"
    );
    let head = git(&["rev-parse", &format!("refs/heads/{branch}")]);
    assert_ne!(head, before, "{reported:#?}");
    let (log, view) = agit::domain::storage::materialize_pair_at(&repo, &head).unwrap();
    assert!(log.contains("inspection completed"));
    assert!(
        !log.contains(hook_output) && !view.contains(hook_output),
        "the hook record is not archived"
    );
    let meta: serde_json::Value = serde_json::from_str(&git(&[
        "show",
        &format!("{head}:{}", agit::domain::meta::FILE),
    ]))
    .unwrap();
    assert_eq!(
        meta["baseline_bytes"],
        replied.len(),
        "settlement ends the turn at the reply"
    );
    assert_eq!(
        std::fs::read_to_string(&transcript).unwrap(),
        text,
        "the hook record stays in the transcript past the archived end"
    );
}
