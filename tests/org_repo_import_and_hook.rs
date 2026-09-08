//! Two contracts for org repos, entered through the binary:
//!
//! 1. `agit import --into <org>/<name>@<branch>` asks the hub's write gate first (receive-pack's
//!    `info/refs`), and lands in `~/.agit/repos/<org>/<name>` only once it allows. When the hub
//!    says it is not writable, not one version lands.
//! 2. Once it has landed in the org repo, the installed hooks (`hooks settle` / `hooks ingest`)
//!    still resolve to the same repo — the claim records the namespace, Stop no longer fills the
//!    owner in from the signed-in account to look for `<me>/<name>`, and the `AGIT_SESSION`
//!    SessionStart writes back is the org's too.

use agit::domain::repo::Repo;
use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::{fs, net::TcpListener};

const SID: &str = "cccccccc-0000-4000-8000-000000000003";
const QA_AGENT_ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

/// A fake hub that answers the way the hub does: `einsia/qa` exists and is writable (once the
/// `gate_closed` file appears the write gate answers 404 instead — simulating a revoked grant;
/// whoever may write must still pass the identity fence: no Expected-Agent-Id is 428, a wrong one
/// is 412); `einsia/locked` exists and its write gate answers 404 (the hub gives one answer for
/// both "exists but not writable" and "does not exist"); `einsia/fresh` does not exist and I am
/// the owner of `einsia`; `acme/ghost` does not exist and I am only a plain member of `acme`.
/// Serves until the process ends.
fn fake_hub(
    gate_closed: std::path::PathBuf,
    clone_url: std::path::PathBuf,
    latest_version: std::path::PathBuf,
    version_requests: Arc<AtomicUsize>,
    hub_requests: Arc<AtomicUsize>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server_base = base.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(k) = sock.read(&mut chunk) {
                if k == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..k]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            hub_requests.fetch_add(1, Ordering::SeqCst);
            let req = String::from_utf8_lossy(&buf).into_owned();
            let line = req.lines().next().unwrap_or_default().to_string();
            let authed = req.to_ascii_lowercase().contains("authorization: bearer ");
            let path = line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let not_found = (
                "404 Not Found",
                r#"{"error":"agent not found","kind":"not_found"}"#.to_string(),
            );
            let agent = |owner: &str, name: &str| {
                let clone_url = fs::read_to_string(&clone_url).unwrap_or_else(|_| "x".into());
                (
                    "200 OK",
                    format!(
                        r#"{{"agent_id":"{QA_AGENT_ID}","owner":"{owner}","name":"{name}","clone_url":{},"visibility":"public"}}"#,
                        serde_json::to_string(clone_url.trim()).unwrap()
                    ),
                )
            };
            let (status, body) = if !authed {
                (
                    "401 Unauthorized",
                    r#"{"error":"auth","kind":"unauthorized"}"#.to_string(),
                )
            } else if path == "/api/cli/version" {
                version_requests.fetch_add(1, Ordering::SeqCst);
                let version = fs::read_to_string(&latest_version)
                    .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").into());
                (
                    "200 OK",
                    serde_json::json!({
                        "version": version.trim(),
                        "tag": format!("agit-v{}", version.trim()),
                    })
                    .to_string(),
                )
            } else if line.starts_with("POST /api/agents/einsia/qa/clone ") {
                (
                    "200 OK",
                    serde_json::json!({
                        "agent_id": "bbbbbbbb-0000-4000-8000-000000000002",
                        "forked_from": QA_AGENT_ID,
                        "owner": "me", "name": "qa",
                        "push_url": format!("{server_base}/me/qa.git"),
                        "web_url": format!("{server_base}/me/qa"),
                    })
                    .to_string(),
                )
            } else if path == "/api/agents/einsia/qa" {
                agent("einsia", "qa")
            } else if path == "/api/agents/einsia/locked" {
                agent("einsia", "locked")
            } else if path == "/api/agents/acme/forbidden" {
                (
                    "403 Forbidden",
                    r#"{"error":"access denied","kind":"forbidden"}"#.to_string(),
                )
            } else if path.starts_with("/denied.git/") {
                ("401 Unauthorized", String::new())
            } else if path.starts_with("/forbidden.git/") {
                ("403 Forbidden", String::new())
            } else if path == "/api/orgs/einsia" {
                (
                    "200 OK",
                    r#"{"name":"einsia","role":"owner","created_at":"2026-01-01T00:00:00Z"}"#
                        .to_string(),
                )
            } else if path == "/api/orgs/acme" {
                (
                    "200 OK",
                    r#"{"name":"acme","role":"member","created_at":"2026-01-01T00:00:00Z"}"#
                        .to_string(),
                )
            } else if path == "/einsia/qa.git/info/refs?service=git-receive-pack"
                && !gate_closed.exists()
            {
                let expected = req.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("x-agentgit-expected-agent-id")
                        .then(|| v.trim().to_string())
                });
                match expected.as_deref() {
                    Some(QA_AGENT_ID) => ("200 OK", "0000".to_string()),
                    Some(_) => (
                        "412 Precondition Failed",
                        r#"{"error":"this repository name now refers to a different Agent identity","kind":"identity_precondition_failed"}"#.to_string(),
                    ),
                    None => (
                        "428 Precondition Required",
                        r#"{"error":"pushes require an immutable Agent identity; upgrade agit and retry","kind":"identity_precondition_required"}"#.to_string(),
                    ),
                }
            } else {
                not_found
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    base
}

struct Lab {
    _tmp: tempfile::TempDir,
    hub: String,
    /// Creating this file stops the fake hub from allowing writes to `einsia/qa`.
    gate_closed: std::path::PathBuf,
    clone_url: std::path::PathBuf,
    latest_version: std::path::PathBuf,
    version_requests: Arc<AtomicUsize>,
    hub_requests: Arc<AtomicUsize>,
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
        // The cwd in the link comes from the transcript while the process sees the canonical
        // path; both must spell it the same way for "the only adopted session in this directory"
        // to match.
        let work = work.canonicalize().unwrap();
        let gate_closed = tmp.path().join("gate-closed");
        let clone_url = tmp.path().join("clone-url");
        let latest_version = tmp.path().join("latest-version");
        let version_requests = Arc::new(AtomicUsize::new(0));
        let hub_requests = Arc::new(AtomicUsize::new(0));
        fs::write(&clone_url, "x").unwrap();
        fs::write(&latest_version, env!("CARGO_PKG_VERSION")).unwrap();
        let hub = fake_hub(
            gate_closed.clone(),
            clone_url.clone(),
            latest_version.clone(),
            Arc::clone(&version_requests),
            Arc::clone(&hub_requests),
        );
        let cred = agit::infra::credentials::HubCredential {
            username: "me".into(),
            email: None,
            hub: Some(hub.clone()),
            access_token: "fake".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "fake".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let key = agit::infra::config::hub_host_key(&hub).unwrap();
        agit::infra::credentials::save_at(
            &agit_home.join("credentials").join(format!("{key}.json")),
            &cred,
        )
        .unwrap();
        Lab {
            _tmp: tmp,
            hub,
            gate_closed,
            clone_url,
            latest_version,
            version_requests,
            hub_requests,
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
            .env("AGIT_HUB_URL", &self.hub)
            .env("AGIT_YES", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        c
    }

    /// Feed the hook JSON to the **installed** entry points (`agit hooks settle` / `ingest`) the
    /// way the harness does.
    fn hook(
        &self,
        action: &str,
        session_id: &str,
        extra: &[(&str, &str)],
        env: &[(&str, &str)],
    ) -> std::process::Output {
        let mut cmd = self.agit(&["hooks", action]);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut payload = serde_json::json!({
            "session_id": session_id,
            "cwd": self.work,
            "transcript_path": self.transcript(session_id),
            "hook_event_name": if action == "settle" { "Stop" } else { "SessionStart" },
        });
        for (k, v) in extra {
            payload[k] = serde_json::Value::String(v.to_string());
        }
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn transcript(&self, session_id: &str) -> std::path::PathBuf {
        let slug = agit::adapter::claude_code::slug_for(&self.work);
        self.home
            .join(".claude")
            .join("projects")
            .join(slug)
            .join(format!("{session_id}.jsonl"))
    }

    fn append_turn(&self, session_id: &str, n: usize, prompt: &str, reply: &str) {
        let cwd = self.work.to_string_lossy();
        let lines = format!(
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
        );
        let p = self.transcript(session_id);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .unwrap();
        f.write_all(lines.as_bytes()).unwrap();
    }

    fn repo(&self, owner: &str, name: &str) -> std::path::PathBuf {
        self.agit_home.join("repos").join(owner).join(name)
    }
}

fn commits_on(dir: &std::path::Path, branch: &str) -> usize {
    let repo = Repo::open(dir).expect("repo exists");
    repo.git(&["rev-list", "--count", &format!("refs/heads/{branch}")])
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn active_links_on(
    lab: &Lab,
    owner: &str,
    agent: &str,
    branch: &str,
) -> Vec<agit::domain::link::Link> {
    let store = agit::domain::store::Store::at(lab.agit_home.join("store"));
    agit::domain::link::active_for_branch(&store, owner, agent, branch)
}

fn advance_branch_without_changing_its_tree(repo: &Repo, branch: &str) -> String {
    let old = repo
        .git(&["rev-parse", &format!("refs/heads/{branch}")])
        .unwrap();
    let tree = repo
        .git(&["rev-parse", &format!("refs/heads/{branch}^{{tree}}")])
        .unwrap();
    let new = repo
        .git(&[
            "commit-tree",
            tree.trim(),
            "-p",
            old.trim(),
            "-m",
            "advance branch",
        ])
        .unwrap();
    repo.git(&[
        "update-ref",
        &format!("refs/heads/{branch}"),
        new.trim(),
        old.trim(),
    ])
    .unwrap();
    new.trim().to_string()
}

fn push_with_fresh_update_cache(
    lab: &Lab,
    args: &[&str],
    env: &[(&str, &str)],
) -> (std::process::Output, usize) {
    let _ = fs::remove_file(lab.agit_home.join("cli-update.json"));
    let before = lab.version_requests.load(Ordering::SeqCst);
    let mut cmd = lab.agit(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().unwrap();
    let requests = lab.version_requests.load(Ordering::SeqCst) - before;
    (output, requests)
}

fn update_notice_count(output: &std::process::Output) -> usize {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .matches("agit 999.0.0 is available")
    .count()
}

/// The update check has one owner: binary startup. This deliberately drives a successful push
/// all the way through Git so a second call reintroduced at the tail would print the cached
/// notice twice. Suppressed modes start without a cache, making any bypass visible as a request.
#[test]
fn push_dispatches_one_startup_update_check_and_suppressed_modes_dispatch_none() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "publish this turn", "done");
    let imported = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(
        imported.status.success(),
        "{}{}",
        String::from_utf8_lossy(&imported.stdout),
        String::from_utf8_lossy(&imported.stderr)
    );

    let bare = lab._tmp.path().join("remote.git");
    let initialized = Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(&bare)
        .status()
        .unwrap();
    assert!(
        initialized.success(),
        "temporary push remote must initialize"
    );
    fs::write(&lab.clone_url, bare.to_string_lossy().as_bytes()).unwrap();
    fs::write(&lab.latest_version, "999.0.0").unwrap();
    let repo = Repo::open(lab.repo("einsia", "qa")).unwrap();
    repo.set_remote(bare.to_str().unwrap()).unwrap();
    let identity = agit::hub::identity::RemoteIdentity::new(&lab.hub, QA_AGENT_ID).unwrap();
    agit::hub::identity::pin(&repo, &identity).unwrap();

    let push = ["push", "einsia/qa", "-b", "work"];
    let (ordinary, requests) = push_with_fresh_update_cache(&lab, &push, &[]);
    assert!(
        ordinary.status.success(),
        "ordinary push must reach its successful tail:\n{}{}",
        String::from_utf8_lossy(&ordinary.stdout),
        String::from_utf8_lossy(&ordinary.stderr)
    );
    assert_eq!(
        requests, 1,
        "ordinary push asks for the latest version once"
    );
    assert_eq!(
        update_notice_count(&ordinary),
        1,
        "ordinary push prints exactly one startup notice"
    );

    for (label, args, env) in [
        (
            "JSON",
            vec!["--json", "push", "einsia/qa", "-b", "work"],
            vec![],
        ),
        (
            "quiet",
            vec!["-q", "push", "einsia/qa", "-b", "work"],
            vec![],
        ),
        ("CI", push.to_vec(), vec![("CI", "1")]),
    ] {
        let (output, requests) = push_with_fresh_update_cache(&lab, &args, &env);
        assert!(
            output.status.success(),
            "{label} push must succeed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(requests, 0, "{label} push must not check for an update");
        assert_eq!(
            update_notice_count(&output),
            0,
            "{label} push must not print an update notice"
        );
    }
}

#[test]
fn an_org_import_lands_in_the_org_repo_and_the_next_stop_hook_follows_it() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");

    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "import into a writable org repo must succeed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let org = lab.repo("einsia", "qa");
    assert!(
        org.join(".git").exists(),
        "the checkout lives under the org namespace"
    );
    assert!(
        !lab.repo("me", "qa").exists(),
        "nothing may land under the signed-in account's namespace"
    );
    let after_import = commits_on(&org, "work");

    // The next turn settles through the Stop hook: the claim records `einsia`, so the hook must
    // not go looking for `me/qa`.
    lab.append_turn(SID, 2, "second turn", "done");
    let out = lab.hook("settle", SID, &[], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        commits_on(&org, "work"),
        after_import + 1,
        "the hook must settle into the org repo it was claimed on"
    );
    assert!(
        !lab.repo("me", "qa").exists(),
        "a hook that fell back to the account namespace would have created me/qa"
    );

    // Explicit environment identity preserves the org namespace with or without runtime evidence.
    for env in [vec![], vec![("CLAUDE_SESSION_ID", SID)]] {
        let mut cmd = lab.agit(&["log", "--oneline"]);
        cmd.env("AGIT_SESSION", "einsia/qa@work");
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("start the org line"),
            "explicit context ({env:?}) must reach the org repo:\n{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let status = lab.agit(&["status"]).output().unwrap();
    let status_out = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_out.contains("einsia/qa") && !status_out.contains("me/qa"),
        "status must name the org repo:\n{status_out}"
    );

    // The identity SessionStart writes back into the session environment must be the org one too:
    // the process tree carries a stale `me/qa@work`, yet what lands in the written file must be
    // the `einsia/qa@work` the link records.
    let env_file = lab
        ._tmp
        .path()
        .join("session-env")
        .join(SID)
        .join("sessionstart-hook-1.sh");
    fs::create_dir_all(env_file.parent().unwrap()).unwrap();
    let out = lab.hook(
        "ingest",
        SID,
        &[("source", "resume")],
        &[
            ("AGIT_SESSION", "me/qa@work"),
            ("CLAUDE_ENV_FILE", env_file.to_str().unwrap()),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("SessionStart must return one hook response: {e}"));
    assert_eq!(
        response["hookSpecificOutput"]["sessionTitle"],
        "agit einsia/qa@work"
    );
    assert!(
        response["hookSpecificOutput"]
            .get("additionalContext")
            .is_none(),
        "a managed session must not be told to import itself again: {response}"
    );
    let written = fs::read_to_string(&env_file).unwrap();
    assert!(
        written.contains("einsia/qa@work"),
        "SessionStart must write the claimed org slug back, got: {written}"
    );
    assert!(!written.contains("me/qa"), "{written}");
}

/// An org owner may import a session into a repo the hub does not have yet: the first push
/// creates it under the org. The checkout lands under the org namespace, not the signed-in
/// account's.
#[test]
fn an_org_owner_may_import_into_a_repo_that_does_not_exist_yet() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "brand new", "ok");
    let out = lab
        .agit(&["import", SID, "--into", "einsia/fresh@work"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "an org owner may create a repo through its first import:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(lab.repo("einsia", "fresh").join(".git").exists());
    assert!(!lab.repo("me", "fresh").exists());
    assert!(commits_on(&lab.repo("einsia", "fresh"), "work") >= 2);
}

#[test]
fn an_import_the_hub_refuses_lands_nothing() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "hello", "hi");

    let out = lab
        .agit(&["import", SID, "--into", "einsia/locked@work"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(agit::ExitCode::Policy.as_i32()),
        "a read-only org repo must be refused by policy:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !lab.repo("einsia", "locked").exists() && !lab.repo("me", "locked").exists(),
        "a refused import must not create a checkout anywhere"
    );

    // Does not exist and I am only a plain member: a plain member cannot create an agent under
    // the org.
    let out = lab
        .agit(&["import", SID, "--into", "acme/ghost@work"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(agit::ExitCode::Ref.as_i32()),
        "a missing repo a mere member cannot create is a bad reference:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!lab.repo("acme", "ghost").exists());

    let out = lab
        .agit(&["import", SID, "--into", "Einsia/qa@work"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(agit::ExitCode::Usage.as_i32()),
        "a non-lowercase owner is rejected at the entrance:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!lab.repo("Einsia", "qa").exists());
}

/// `agit run` arbitrates an org branch by asking the hub's write gate, not by asking whether I am
/// the owner: a granted member running the org branch head → continue (no new branch); once the
/// hub revokes the grant, the same branch → forking is mandatory, and the suggested fork name
/// skips an existing `-run-<n>`.
#[test]
fn run_continues_a_granted_org_branch_and_forks_once_the_gate_closes() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let org = Repo::open(lab.repo("einsia", "qa")).expect("org checkout");

    let out = lab
        .agit(&["run", "einsia/qa@work", "--no-launch"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("→ continue (resume)"),
        "a granted member must continue the org branch, not fork it:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !org.has_ref("refs/heads/work-run-1"),
        "continuing must not leave a fork behind"
    );

    // Grant revoked: the same branch can only be forked. With no terminal there is no
    // confirmation to be had, but the suggested name skips the taken `-run-1` instead of
    // colliding and leaving the renaming to the person.
    fs::write(&lab.gate_closed, "").unwrap();
    org.git(&["branch", "work-run-1", "work"]).unwrap();
    let out = lab
        .agit(&["run", "einsia/qa@work", "--no-launch"])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("grants you no write access") && text.contains("forking is mandatory"),
        "a branch the hub won't let you write must fork:\n{text}"
    );
    assert!(
        out.status.code() == Some(agit::ExitCode::Interactive.as_i32())
            && text.contains("-b work-run-2"),
        "the suggested fork name must skip the taken `-run-1`:\n{text}"
    );
    assert!(
        !org.has_ref("refs/heads/work-run-2"),
        "nothing is forked without a name"
    );

    let out = lab
        .agit(&["run", "einsia/qa@work", "-b", "work-again", "--no-launch"])
        .output()
        .unwrap();
    assert!(
        out.status.success() && org.has_ref("refs/heads/work-again"),
        "with a name given the fork must land:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An org repo the hub does not have yet: after the owner imports it, `run` on its branch head
/// continues too — the first push creates it under the org, the same answer push / import give
/// for "creatable"; forking it as if it were someone else's line is that arbitration error in
/// another shape.
#[test]
fn run_continues_an_org_owners_branch_the_hub_does_not_have_yet() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "brand new", "ok");
    let out = lab
        .agit(&["import", SID, "--into", "einsia/fresh@work"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let org = Repo::open(lab.repo("einsia", "fresh")).expect("org checkout");

    let out = lab
        .agit(&["run", "einsia/fresh@work", "--no-launch"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("→ continue (resume)"),
        "an org owner must continue a branch of a repo the hub will create on push:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !org.has_ref("refs/heads/work-run-1"),
        "continuing must not leave a fork behind"
    );
}

/// The baseline and the later comparison against the live transcript go through the same read
/// path: in a file-backed runtime the two are the same bytes. (In library-backed OpenCode the two
/// differ by construction — the export payload vs the canonical materialization — and the
/// baseline must take the latter; this pins the "same path" invariant.)
#[test]
fn a_cross_runtime_resume_baselines_what_the_live_read_returns() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let out = lab
        .agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The materialized rollout and the baseline in its store link must agree.
    let mut rollouts = vec![];
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    walk(&lab.home.join(".codex").join("sessions"), &mut rollouts);
    assert_eq!(rollouts.len(), 1, "exactly one rollout");
    let links: Vec<_> = fs::read_dir(lab.agit_home.join("store").join("codex"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(links.len(), 1, "exactly one store link");
    let link: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(links[0].path()).unwrap()).unwrap();
    assert_eq!(
        link["baseline_bytes"].as_u64().unwrap(),
        fs::metadata(&rollouts[0]).unwrap().len(),
        "baseline = the byte length of the live transcript right now"
    );
}

/// An existing runtime remains the resume target unless the caller explicitly requests another.
#[test]
fn runtime_default_does_not_replace_an_existing_native_session() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "keep the native runtime", "ok");
    assert!(
        lab.agit(&["import", SID, "--into", "einsia/qa@work"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        lab.agit(&["config", "runtime.default", "codex"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let resumed = lab
        .agit(&["resume", "einsia/qa@work", "--no-launch"])
        .output()
        .unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(resumed.status.success(), "{output}");
    assert!(
        output.contains("reusing the local native session"),
        "{output}"
    );
    assert!(output.contains(SID), "{output}");
    let active = active_links_on(&lab, "einsia", "qa", "work");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].session_id, SID);
    assert_eq!(active[0].source, "claude-code");
}

/// Repeating the same prepare must return the existing runtime id instead of minting another
/// active writer for the branch.
#[test]
fn repeated_no_launch_reuses_the_prepared_session() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    let imported = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(imported.status.success());

    let first = lab
        .agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
        .output()
        .unwrap();
    assert!(first.status.success());
    let first_active = active_links_on(&lab, "einsia", "qa", "work");
    assert_eq!(first_active.len(), 1);
    let prepared = first_active[0].session_id.clone();

    assert!(
        lab.agit(&["config", "runtime.default", "claude-code"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let second = lab
        .agit(&["run", "einsia/qa@work", "--no-launch"])
        .output()
        .unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(second.status.success(), "{output}");
    assert!(
        output.contains("reusing the prepared runtime session"),
        "{output}"
    );
    assert!(output.contains(&prepared), "{output}");
    let second_active = active_links_on(&lab, "einsia", "qa", "work");
    assert_eq!(second_active.len(), 1);
    assert_eq!(second_active[0].session_id, prepared);
    assert_eq!(walk(&lab.home.join(".codex").join("sessions")).len(), 1);
}

/// Claude Desktop writes the same Claude Code jsonl file but intentionally has no listing
/// adapter. An already-materialized desktop link must still read that file for the idempotent
/// prepare check, rather than becoming an unverifiable active claim on the second run.
#[test]
fn repeated_no_launch_reuses_a_claude_desktop_session() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    assert!(
        lab.agit(&["import", SID, "--into", "einsia/qa@work"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let first = lab
        .agit(&[
            "run",
            "einsia/qa@work",
            "--no-launch",
            "--as",
            "claude-desktop",
        ])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let first_active = active_links_on(&lab, "einsia", "qa", "work");
    assert_eq!(first_active.len(), 1);
    assert_eq!(first_active[0].source, "claude-desktop");
    let prepared = first_active[0].session_id.clone();

    let second = lab
        .agit(&[
            "run",
            "einsia/qa@work",
            "--no-launch",
            "--as",
            "claude-desktop",
        ])
        .output()
        .unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(second.status.success(), "{output}");
    assert!(
        output.contains("reusing the prepared runtime session"),
        "{output}"
    );
    assert!(output.contains(&prepared), "{output}");
    let second_active = active_links_on(&lab, "einsia", "qa", "work");
    assert_eq!(second_active.len(), 1);
    assert_eq!(second_active[0].session_id, prepared);
}

/// A newer branch tip replaces an untouched materialization while preserving the old link as
/// recovery metadata.
#[test]
fn an_advanced_branch_supersedes_an_untouched_materialization() {
    check_superseded_harness_refusal(false);
}

/// An incomplete historical claim remains a veto and cannot lend settlement to a replacement.
#[test]
fn an_ownerless_superseded_harness_cannot_settle_its_replacement() {
    check_superseded_harness_refusal(true);
}

fn check_superseded_harness_refusal(ownerless: bool) {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    assert!(
        lab.agit(&["import", SID, "--into", "einsia/qa@work"])
            .output()
            .unwrap()
            .status
            .success()
    );
    if ownerless {
        let path = lab
            .agit_home
            .join("store/claude-code")
            .join(format!("{SID}.json"));
        let mut claim: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        claim.as_object_mut().unwrap().remove("owner");
        fs::write(path, serde_json::to_vec(&claim).unwrap()).unwrap();
    }
    assert!(
        lab.agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let old = active_links_on(&lab, "einsia", "qa", "work").pop().unwrap();
    let repo = Repo::open(lab.repo("einsia", "qa")).unwrap();
    let advanced = advance_branch_without_changing_its_tree(&repo, "work");

    let resumed = lab
        .agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
        .output()
        .unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(resumed.status.success(), "{output}");
    assert!(output.contains("superseded codex"), "{output}");

    let active = active_links_on(&lab, "einsia", "qa", "work");
    assert_eq!(active.len(), 1);
    assert_ne!(active[0].session_id, old.session_id);
    assert_eq!(
        active[0].materialized_from.as_deref(),
        Some(advanced.as_str())
    );
    let successor = active[0].instance();
    let store = agit::domain::store::Store::at(lab.agit_home.join("store"));
    let old = agit::domain::link::get(&store, "codex", &old.session_id).unwrap();
    assert_eq!(old.superseded_by.as_deref(), Some(successor.as_str()));

    let by_branch = lab.agit(&["commit", "einsia/qa@work"]).output().unwrap();
    let by_branch_text = format!(
        "{}{}",
        String::from_utf8_lossy(&by_branch.stdout),
        String::from_utf8_lossy(&by_branch.stderr)
    );
    assert!(by_branch.status.success(), "{by_branch_text}");
    assert!(!by_branch_text.contains("multiple session links"));

    let stale = lab.agit(&["commit", &old.session_id]).output().unwrap();
    let stale_text = format!(
        "{}{}",
        String::from_utf8_lossy(&stale.stdout),
        String::from_utf8_lossy(&stale.stderr)
    );
    assert!(!stale.status.success(), "{stale_text}");
    assert!(stale_text.contains("was superseded by"), "{stale_text}");

    // The old runtime can still be open after its untouched link is superseded. Its stable branch
    // environment must not make an implicit commit read the newer active runtime's transcript.
    let from_old_runtime = lab
        .agit(&["commit"])
        .env("AGIT_SESSION", "einsia/qa@work")
        .env("CODEX_SESSION_ID", &old.session_id)
        .output()
        .unwrap();
    let from_old_runtime_text = format!(
        "{}{}",
        String::from_utf8_lossy(&from_old_runtime.stdout),
        String::from_utf8_lossy(&from_old_runtime.stderr)
    );
    assert!(
        !from_old_runtime.status.success(),
        "{from_old_runtime_text}"
    );
    assert!(
        from_old_runtime_text.contains("was superseded by"),
        "{from_old_runtime_text}"
    );

    let active_path = walk(&lab.home.join(".codex/sessions"))
        .into_iter()
        .find(|path| path.to_string_lossy().contains(&active[0].session_id))
        .unwrap();
    let mut active_file = fs::OpenOptions::new()
        .append(true)
        .open(&active_path)
        .unwrap();
    for (role, kind, text) in [
        ("user", "input_text", "active replacement continuation"),
        ("assistant", "output_text", "active replacement answer"),
    ] {
        writeln!(
            active_file,
            "{}",
            serde_json::json!({"type": "response_item", "payload": {
                "type": "message", "role": role,
                "content": [{"type": kind, "text": text}]
            }})
        )
        .unwrap();
    }
    drop(active_file);
    let head_before = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
    let link_path = lab
        .agit_home
        .join("store/codex")
        .join(format!("{}.json", active[0].session_id));
    let link_before = fs::read(&link_path).unwrap();
    let live_before = fs::read(&active_path).unwrap();
    for (args, codex_id) in [
        (vec!["commit"], Some(old.session_id.as_str())),
        (vec!["commit", "@"], Some(old.session_id.as_str())),
        (vec!["commit"], None),
        (vec!["commit", "@"], None),
    ] {
        let mut command = lab.agit(&args);
        command
            .env("AGIT_SESSION", "einsia/qa@work")
            .env("CLAUDE_CODE_SESSION_ID", SID)
            .env("CLAUDE_SESSION_ID", SID);
        if let Some(codex_id) = codex_id {
            command.env("CODEX_SESSION_ID", codex_id);
        }
        let refused = command.output().unwrap();
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&refused.stdout),
            String::from_utf8_lossy(&refused.stderr)
        );
        assert!(!refused.status.success(), "{args:?}: {output}");
        assert!(output.contains("superseded"), "{output}");
        assert_eq!(
            repo.git(&["rev-parse", "refs/heads/work"]).unwrap(),
            head_before
        );
        assert_eq!(fs::read(&link_path).unwrap(), link_before);
        assert_eq!(fs::read(&active_path).unwrap(), live_before);
    }

    // A nested runtime can inherit the original Claude id while exposing the current Codex id.
    // The current active identity wins; the inherited superseded variable must not block it.
    let from_active_runtime = lab
        .agit(&["commit"])
        .env("AGIT_SESSION", "einsia/qa@work")
        .env("CLAUDE_CODE_SESSION_ID", SID)
        .env("CODEX_SESSION_ID", &active[0].session_id)
        .output()
        .unwrap();
    let from_active_runtime_text = format!(
        "{}{}",
        String::from_utf8_lossy(&from_active_runtime.stdout),
        String::from_utf8_lossy(&from_active_runtime.stderr)
    );
    assert!(
        from_active_runtime.status.success(),
        "{from_active_runtime_text}"
    );
    assert!(
        !from_active_runtime_text.contains("was superseded by"),
        "{from_active_runtime_text}"
    );
    assert_ne!(
        repo.git(&["rev-parse", "refs/heads/work"]).unwrap(),
        head_before
    );
}

/// If an outer Claude runtime is active on another line while an inner Codex runtime is the
/// superseded process, the stale Codex identity must not be hidden by the unrelated active link.
/// `AGIT_SESSION` supplies the line discriminator; without it, the resolver would fall through to
/// the newer active claim and settle the wrong transcript.
#[test]
fn a_nested_runtime_on_another_line_does_not_mask_a_superseded_harness() {
    const CLAUDE_ID: &str = "dddddddd-0000-4000-8000-000000000005";
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    assert!(
        lab.agit(&["import", SID, "--into", "einsia/qa@work"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        lab.agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let old = active_links_on(&lab, "einsia", "qa", "work").pop().unwrap();
    let repo = Repo::open(lab.repo("einsia", "qa")).unwrap();
    advance_branch_without_changing_its_tree(&repo, "work");
    assert!(
        lab.agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let claude_dir = lab.agit_home.join("store").join("claude-code");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(
        claude_dir.join(format!("{CLAUDE_ID}.json")),
        serde_json::json!({
            "cwd": lab.work.to_string_lossy(),
            "agent": "qa",
            "owner": "einsia",
            "branch": "outer-line"
        })
        .to_string(),
    )
    .unwrap();

    let refused = lab
        .agit(&["commit"])
        .env("AGIT_SESSION", "einsia/qa@work")
        .env("CLAUDE_CODE_SESSION_ID", CLAUDE_ID)
        .env("CODEX_SESSION_ID", &old.session_id)
        .output()
        .unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(!refused.status.success(), "{output}");
    assert!(output.contains("was superseded by"), "{output}");
}

#[test]
fn rc_rerouting_a_materialization_checks_history_and_can_settle_a_fresh_line() {
    check_rc_rerouted_settlement(false);
}

/// Legacy personal claims cannot lend their materialization baseline to an org's namesake line.
#[test]
fn rc_rerouting_a_legacy_personal_claim_checks_the_destination_namespace() {
    check_rc_rerouted_settlement(true);
}

fn check_rc_rerouted_settlement(legacy_personal: bool) {
    let lab = Lab::new();
    let (source, source_owner, destination_branch) = if legacy_personal {
        ("me/qa@work", "me", "work")
    } else {
        ("einsia/qa@work", "einsia", "unrelated")
    };
    lab.append_turn(SID, 1, "original context", "original answer");
    let run = |args: &[&str]| {
        let output = lab.agit(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["import", SID, "--into", source]);
    run(&["resume", source, "--force", "--no-launch"]);
    let mut active = active_links_on(&lab, source_owner, "qa", "work")
        .pop()
        .unwrap();
    assert!(active.baseline_bytes.is_some());
    if legacy_personal {
        active.owner = None;
        active.materialized_from = None;
        let store = agit::domain::store::Store::at(lab.agit_home.join("store"));
        agit::domain::link::write(&store, &active).unwrap();
    }
    lab.append_turn(
        &active.session_id,
        2,
        "new remote work",
        "new remote answer",
    );

    let unrelated = "cccccccc-0000-4000-8000-000000000099";
    lab.append_turn(unrelated, 1, "unrelated context", "unrelated answer");
    run(&[
        "import",
        unrelated,
        "--into",
        &format!("einsia/qa@{destination_branch}"),
    ]);
    let repo = Repo::open(lab.repo("einsia", "qa")).unwrap();
    let identity = agit::hub::identity::RemoteIdentity::new(&lab.hub, QA_AGENT_ID).unwrap();
    agit::hub::identity::pin(&repo, &identity).unwrap();
    let destination_ref = format!("refs/heads/{destination_branch}");
    let before = repo.git(&["rev-parse", &destination_ref]).unwrap();
    let land = |branch: &str| {
        run(&[
            "rc",
            "land",
            "--slug",
            "einsia/qa",
            "--agent-id",
            QA_AGENT_ID,
            "--branch",
            branch,
            "--runtime",
            &active.source,
            "--session",
            &active.session_id,
            "--cwd",
            lab.work.to_str().unwrap(),
        ]);
    };
    land(destination_branch);
    let refused = lab.agit(&["commit", &active.session_id]).output().unwrap();
    assert!(
        !refused.status.success(),
        "unrelated branch must reject the rerouted transcript: {}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(repo.git(&["rev-parse", &destination_ref]).unwrap(), before);

    land("recovered");
    run(&["commit", &active.session_id]);
    let log = agit::domain::storage::materialize_at(
        repo.root(),
        "refs/heads/recovered",
        agit::domain::meta::LOG_FILE,
    )
    .unwrap();
    assert!(log.contains("original context"));
    assert!(log.contains("new remote work"));
    assert!(!log.contains("unrelated context"));
}

/// A materialized runtime is a continuation of one exact branch tip. If that tip moves before the
/// runtime settles its appended turn, commit must preserve both histories instead of placing the
/// runtime turn after the independently advanced branch.
#[test]
fn a_materialized_commit_refuses_after_its_source_tip_moves() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    let imported = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(imported.status.success());

    let prepared = lab
        .agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
        .output()
        .unwrap();
    assert!(
        prepared.status.success(),
        "{}{}",
        String::from_utf8_lossy(&prepared.stdout),
        String::from_utf8_lossy(&prepared.stderr)
    );
    let active = active_links_on(&lab, "einsia", "qa", "work").pop().unwrap();
    let source_tip = active.materialized_from.clone().unwrap();
    let rollout_path = walk(&lab.home.join(".codex").join("sessions"))
        .pop()
        .unwrap();
    let mut rollout = fs::OpenOptions::new()
        .append(true)
        .open(rollout_path)
        .unwrap();
    writeln!(
        rollout,
        "{}",
        serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "local continuation"}]
            }
        })
    )
    .unwrap();
    writeln!(
        rollout,
        "{}",
        serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "local answer"}]
            }
        })
    )
    .unwrap();
    drop(rollout);

    let repo = Repo::open(lab.repo("einsia", "qa")).unwrap();
    let moved_tip = advance_branch_without_changing_its_tree(&repo, "work");
    assert_ne!(source_tip, moved_tip);

    let refused = lab.agit(&["commit", &active.session_id]).output().unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(!refused.status.success(), "{output}");
    assert!(output.contains("was materialized from"), "{output}");
    assert!(output.contains("advanced independently"), "{output}");
    assert_eq!(
        repo.git(&["rev-parse", "refs/heads/work"]).unwrap().trim(),
        moved_tip
    );
    let still_active = active_links_on(&lab, "einsia", "qa", "work").pop().unwrap();
    assert_eq!(
        still_active.materialized_from.as_deref(),
        Some(source_tip.as_str())
    );
}

/// Appended runtime bytes are unsettled work. A newer branch tip must not silently replace that
/// writer, even though materializing the tip itself would succeed.
#[test]
fn an_advanced_branch_refuses_to_supersede_unsettled_content() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    assert!(
        lab.agit(&["import", SID, "--into", "einsia/qa@work"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        lab.agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let active = active_links_on(&lab, "einsia", "qa", "work").pop().unwrap();
    let rollout = walk(&lab.home.join(".codex").join("sessions"))
        .pop()
        .unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(rollout)
        .unwrap()
        .write_all(b"{\"unsettled\":true}\n")
        .unwrap();
    let repo = Repo::open(lab.repo("einsia", "qa")).unwrap();
    advance_branch_without_changing_its_tree(&repo, "work");

    let refused = lab
        .agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
        .output()
        .unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(!refused.status.success(), "{output}");
    assert!(output.contains("already has unsettled content"), "{output}");
    assert!(
        output.contains(&format!("agit commit {}", active.session_id)),
        "{output}"
    );
    let after = active_links_on(&lab, "einsia", "qa", "work");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].session_id, active.session_id);
    assert_eq!(walk(&lab.home.join(".codex").join("sessions")).len(), 1);
}

/// Matching length alone does not prove an untouched materialization. Rewriting any byte inside
/// the recorded baseline must preserve the old claim and stop before another session is created.
#[test]
fn an_advanced_branch_refuses_to_supersede_a_rewritten_baseline() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    assert!(
        lab.agit(&["import", SID, "--into", "einsia/qa@work"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        lab.agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let active = active_links_on(&lab, "einsia", "qa", "work").pop().unwrap();
    let rollout = walk(&lab.home.join(".codex").join("sessions"))
        .pop()
        .unwrap();
    let mut bytes = fs::read(&rollout).unwrap();
    bytes[0] = if bytes[0] == b'{' { b'[' } else { b'{' };
    fs::write(&rollout, bytes).unwrap();
    let repo = Repo::open(lab.repo("einsia", "qa")).unwrap();
    advance_branch_without_changing_its_tree(&repo, "work");

    let refused = lab
        .agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
        .output()
        .unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(!refused.status.success(), "{output}");
    assert!(
        output.contains("rewritten inside its recorded baseline"),
        "{output}"
    );
    assert!(output.contains("--force --no-launch"), "{output}");
    let after = active_links_on(&lab, "einsia", "qa", "work");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].session_id, active.session_id);
    assert_eq!(walk(&lab.home.join(".codex").join("sessions")).len(), 1);
}

/// A legacy or damaged materialized link without a baseline hash fails closed. Byte length by
/// itself must never authorize automatic replacement.
#[test]
fn an_advanced_branch_refuses_to_supersede_an_unverifiable_baseline() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    assert!(
        lab.agit(&["import", SID, "--into", "einsia/qa@work"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        lab.agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let active = active_links_on(&lab, "einsia", "qa", "work").pop().unwrap();
    let link_path = lab
        .agit_home
        .join("store")
        .join("codex")
        .join(format!("{}.json", active.session_id));
    let mut body: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&link_path).unwrap()).unwrap();
    body.as_object_mut().unwrap().remove("baseline_hash");
    fs::write(&link_path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    let repo = Repo::open(lab.repo("einsia", "qa")).unwrap();
    advance_branch_without_changing_its_tree(&repo, "work");

    let refused = lab
        .agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"])
        .output()
        .unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(!refused.status.success(), "{output}");
    assert!(output.contains("cannot be proven untouched"), "{output}");
    let after = active_links_on(&lab, "einsia", "qa", "work");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].session_id, active.session_id);
    assert_eq!(walk(&lab.home.join(".codex").join("sessions")).len(), 1);
}

/// The branch lock covers the read-decide-write sequence. Concurrent prepares therefore converge
/// on one runtime id instead of both observing an empty claim set and minting independently.
#[test]
fn concurrent_no_launch_prepares_leave_one_active_claim() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    assert!(
        lab.agit(&["import", SID, "--into", "einsia/qa@work"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let mut first = lab.agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"]);
    let mut second = lab.agit(&["run", "einsia/qa@work", "--no-launch", "--as", "codex"]);
    let one = std::thread::spawn(move || first.output().unwrap());
    let two = std::thread::spawn(move || second.output().unwrap());
    let one = one.join().unwrap();
    let two = two.join().unwrap();
    assert!(
        one.status.success() && two.status.success(),
        "first:\n{}{}\nsecond:\n{}{}",
        String::from_utf8_lossy(&one.stdout),
        String::from_utf8_lossy(&one.stderr),
        String::from_utf8_lossy(&two.stdout),
        String::from_utf8_lossy(&two.stderr)
    );
    assert_eq!(active_links_on(&lab, "einsia", "qa", "work").len(), 1);
    assert_eq!(walk(&lab.home.join(".codex").join("sessions")).len(), 1);
}

/// Legacy stores can already contain several active links. The refusal must name commands that
/// select either runtime session instead of asking the user to edit store files by hand.
#[test]
fn legacy_multiple_link_error_offers_session_id_disambiguation() {
    const LEGACY_SID: &str = "cccccccc-0000-4000-8000-000000000005";
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    assert!(
        lab.agit(&["import", SID, "--into", "einsia/qa@work"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let dir = lab.agit_home.join("store").join("claude-code");
    let original = fs::read_to_string(dir.join(format!("{SID}.json"))).unwrap();
    fs::write(dir.join(format!("{LEGACY_SID}.json")), original).unwrap();

    let refused_run = lab
        .agit(&["run", "einsia/qa@work", "--no-launch"])
        .output()
        .unwrap();
    let run_output = format!(
        "{}{}",
        String::from_utf8_lossy(&refused_run.stdout),
        String::from_utf8_lossy(&refused_run.stderr)
    );
    assert!(!refused_run.status.success(), "{run_output}");
    assert!(
        run_output.contains(&format!("agit commit {SID}")),
        "{run_output}"
    );
    assert!(
        run_output.contains(&format!("agit commit {LEGACY_SID}")),
        "{run_output}"
    );
    assert!(run_output.contains("--force --no-launch"), "{run_output}");

    let refused = lab.agit(&["commit", "einsia/qa@work"]).output().unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(!refused.status.success(), "{output}");
    assert!(output.contains("2 active session links"), "{output}");
    assert!(output.contains(&format!("agit commit {SID}")), "{output}");
    assert!(
        output.contains(&format!("agit commit {LEGACY_SID}")),
        "{output}"
    );
    assert!(
        !output.contains("remove extra store links by hand"),
        "{output}"
    );
}

/// A rerouted claim must invalidate the materialization baseline: the baseline the earlier line
/// left behind covers the whole transcript, and carried onto a new branch with no history the
/// settlement region is the empty string — the entire history silently settles as zero turns.
#[test]
fn a_rerouted_claim_invalidates_the_materialization_baseline() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    lab.append_turn(SID, 2, "second turn", "done");
    // Forge the link left behind by "resume materialized this session onto another line":
    // baseline = the whole length. A fake hash does no harm — a reroute invalidates it, and
    // nobody reads it again.
    let transcript = fs::read(lab.transcript(SID)).unwrap();
    let dir = lab.agit_home.join("store").join("claude-code");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{SID}.json")),
        serde_json::json!({
            "cwd": lab.work.to_string_lossy(),
            "agent": "qa",
            "owner": "einsia",
            "branch": "elsewhere",
            "baseline_bytes": transcript.len(),
            "baseline_hash": "not-a-real-hash",
        })
        .to_string(),
    )
    .unwrap();

    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        commits_on(&lab.repo("einsia", "qa"), "work") >= 3,
        "history settles turn by turn instead of being cut to zero turns by the old baseline"
    );
}

/// A legacy link with no owner takes its namespace from the signed-in account: a cross-namespace
/// reroute invalidates the baseline all the same.
#[test]
fn a_legacy_link_without_owner_still_counts_as_a_reroute() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    lab.append_turn(SID, 2, "second turn", "done");
    let transcript = fs::read(lab.transcript(SID)).unwrap();
    let dir = lab.agit_home.join("store").join("claude-code");
    fs::create_dir_all(&dir).unwrap();
    // Same agent name, same branch name, no owner — the legacy form means "me/qa@work" and the
    // target is einsia/qa@work, so this is a reroute, not the same destination.
    fs::write(
        dir.join(format!("{SID}.json")),
        serde_json::json!({
            "cwd": lab.work.to_string_lossy(),
            "agent": "qa",
            "branch": "work",
            "baseline_bytes": transcript.len(),
            "baseline_hash": "not-a-real-hash",
        })
        .to_string(),
    )
    .unwrap();
    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        commits_on(&lab.repo("einsia", "qa"), "work") >= 3,
        "a cross-namespace reroute must invalidate the baseline and settle turn by turn"
    );
}

/// A reroute onto a branch **someone else has claimed and that is not empty**: once the baseline
/// is invalidated, the continuity check refuses it — another session's turns must not be silently
/// grafted into someone else's history.
#[test]
fn a_reroute_onto_a_claimed_branch_is_refused_not_grafted() {
    const SID2: &str = "cccccccc-0000-4000-8000-000000000004";
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let before = commits_on(&lab.repo("einsia", "qa"), "work");

    // Another session, claimed elsewhere by its link, with a baseline covering the whole
    // transcript.
    lab.append_turn(SID2, 1, "a different session", "hi");
    let transcript2 = fs::read(lab.transcript(SID2)).unwrap();
    fs::write(
        lab.agit_home
            .join("store")
            .join("claude-code")
            .join(format!("{SID2}.json")),
        serde_json::json!({
            "cwd": lab.work.to_string_lossy(),
            "agent": "qa",
            "owner": "einsia",
            "branch": "elsewhere",
            "baseline_bytes": transcript2.len(),
            "baseline_hash": "not-a-real-hash",
        })
        .to_string(),
    )
    .unwrap();
    let out = lab
        .agit(&["import", SID2, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "grafting must be refused: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        commits_on(&lab.repo("einsia", "qa"), "work"),
        before,
        "someone else's history must not gain a single turn"
    );
}

/// A refused import must restore the link too: returning the ref without returning the link
/// loses the original destination and baseline for good.
#[test]
fn a_refused_import_restores_the_previous_link() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let link_path = lab
        .agit_home
        .join("store")
        .join("claude-code")
        .join(format!("{SID}.json"));
    let before = fs::read_to_string(&link_path).unwrap();

    // Explicitly aimed at main (the file line): the settlement precondition refuses it and the
    // command fails.
    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@main"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "importing into the file line must be refused: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&link_path).unwrap()).unwrap();
    let before: serde_json::Value = serde_json::from_str(&before).unwrap();
    assert_eq!(
        after["branch"], before["branch"],
        "a failed import must not leave the claim at the new destination"
    );
    assert_eq!(after["baseline_bytes"], before["baseline_bytes"]);
}

/// The cwd fallback for resume is **the current directory**, matching a direct launch of the
/// runtime — not the top level of the git repo the current directory sits in. On a machine whose
/// home directory is itself a git repo, that extra layer installs the session into the home
/// directory.
#[test]
fn resume_falls_back_to_the_invocation_directory_not_the_git_toplevel() {
    let lab = Lab::new();
    // Put work inside a larger git repo: the top level is the tmp root, work is a subdirectory
    // of it.
    let root = lab.work.parent().unwrap().to_path_buf();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    lab.append_turn(SID, 1, "start the org line", "ok");
    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(out.status.success());
    // Fork a branch with no store link, forcing resume onto the cwd fallback (with a link it
    // uses what the link records).
    let out = lab
        .agit(&["fork", "einsia/qa@work", "-b", "cwd-probe"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = lab
        .agit(&[
            "resume",
            "einsia/qa@cwd-probe",
            "--as",
            "codex",
            "--no-launch",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let sessions = lab.home.join(".codex").join("sessions");
    let mut metas = vec![];
    for e in walk(&sessions) {
        let first = std::fs::read_to_string(&e)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        metas.push(serde_json::from_str::<serde_json::Value>(&first).unwrap());
    }
    assert_eq!(metas.len(), 1, "exactly one rollout is materialized");
    assert_eq!(
        metas[0]["payload"]["cwd"].as_str().unwrap(),
        lab.work.to_string_lossy(),
        "cwd must be where resume was invoked, not the top level of the enclosing git repo"
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
    out
}

/// When a web id enters through the context entry point, the normalized destination is carried
/// all the way to fork: the fork happens on the branch the id folds back to, not by handing the
/// raw id to the fork resolver.
#[test]
fn a_web_id_forks_on_the_folded_branch_from_context() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let org = Repo::open(lab.repo("einsia", "qa")).expect("org checkout");
    let tip = org
        .git(&["rev-parse", "refs/heads/work"])
        .unwrap()
        .trim()
        .to_string();
    // Write gate closed: once the web id folds back to the branch, only a fork is left.
    fs::write(&lab.gate_closed, "").unwrap();
    let mut cmd = lab.agit(&[
        "run",
        &format!("agit-{tip}"),
        "-b",
        "probe",
        "--no-launch",
        "--as",
        "codex",
    ]);
    cmd.env("AGIT_SESSION", "einsia/qa@work");
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("→ branch `work`"),
        "an id folds back to the branch name: {text}"
    );
    assert!(
        org.has_ref("refs/heads/probe"),
        "the fork lands on the branch it folded back to: {text}"
    );
}

/// The OID a web id names is the head of that line right now: when the local branch is behind,
/// run folds back, fast-forwards, then arbitrates — it must not continue the stale head, and must
/// not degrade a live line into a historical snapshot.
#[test]
fn a_web_id_fast_forwards_a_stale_local_branch_before_arbitration() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let org = Repo::open(lab.repo("einsia", "qa")).expect("org checkout");
    let tip = org
        .git(&["rev-parse", "refs/heads/work"])
        .unwrap()
        .trim()
        .to_string();
    let old = org
        .git(&["rev-parse", "refs/heads/work~1"])
        .unwrap()
        .trim()
        .to_string();
    // Move the local branch back one; the same-named branch on the remote stays at the real head.
    org.git(&["update-ref", "refs/remotes/origin/work", &tip])
        .unwrap();
    org.git(&["update-ref", "refs/heads/work", &old]).unwrap();

    let mut cmd = lab.agit(&[
        "run",
        &format!("agit-{tip}"),
        "--no-launch",
        "--as",
        "codex",
    ]);
    cmd.env("AGIT_SESSION", "einsia/qa@work");
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("fast-forwarded"),
        "align first, then arbitrate: {text}"
    );
    assert_eq!(
        org.git(&["rev-parse", "refs/heads/work"]).unwrap().trim(),
        tip,
        "the local branch fast-forwards to the head the id names"
    );
    assert!(
        text.contains("continue (resume)"),
        "once aligned it continues on the branch head instead of forking: {text}"
    );
}

/// When the local line is ahead of the published tip the web shows (the id is an ancestor of the
/// local head), it continues on the local head — no false divergence report, and the local branch
/// is not moved back.
#[test]
fn a_web_id_behind_the_local_head_continues_on_the_local_line() {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "start the org line", "ok");
    let out = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let org = Repo::open(lab.repo("einsia", "qa")).expect("org checkout");
    let tip = org
        .git(&["rev-parse", "refs/heads/work"])
        .unwrap()
        .trim()
        .to_string();
    let published = org
        .git(&["rev-parse", "refs/heads/work~1"])
        .unwrap()
        .trim()
        .to_string();
    // The remote (= what the web shows) stays at the old head; the local line is already ahead.
    org.git(&["update-ref", "refs/remotes/origin/work", &published])
        .unwrap();

    let mut cmd = lab.agit(&[
        "run",
        &format!("agit-{published}"),
        "--no-launch",
        "--as",
        "codex",
    ]);
    cmd.env("AGIT_SESSION", "einsia/qa@work");
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("ahead of the published tip"),
        "being ahead locally is stated, not reported as divergence: {text}"
    );
    assert!(text.contains("continue (resume)"), "{text}");
    assert_eq!(
        org.git(&["rev-parse", "refs/heads/work"]).unwrap().trim(),
        tip,
        "the local head must not be moved back"
    );
}

fn new_session_remote(lab: &Lab) -> std::path::PathBuf {
    let remote = lab._tmp.path().join("new-remote");
    let repo = Repo::init(&remote).unwrap();
    agit::domain::meta::write(repo.root(), &agit::domain::meta::Meta::new_file_line()).unwrap();
    fs::write(
        repo.root().join("AGENTS.md"),
        "Inherited project instructions\n",
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("shared file line").unwrap();
    fs::write(&lab.clone_url, remote.to_string_lossy().as_bytes()).unwrap();
    remote
}

#[cfg(unix)]
#[test]
fn missing_repository_recovery_preserves_workspace_binding_through_resume() {
    let publisher = Lab::new();
    publisher.append_turn(SID, 1, "restore this published session", "synthetic reply");
    let imported = publisher
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .env("CI", "1")
        .output()
        .unwrap();
    assert!(imported.status.success(), "{imported:?}");
    let source = Repo::open(publisher.repo("einsia", "qa")).unwrap();
    let source_head = source.git(&["rev-parse", "refs/heads/work"]).unwrap();
    let remote = publisher._tmp.path().join("published.git");
    let published = Command::new("git")
        .args(["clone", "--bare", "--quiet"])
        .arg(source.root())
        .arg(&remote)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(published.status.success(), "{published:?}");

    for already_bound in [true, false] {
        let lab = Lab::new();
        fs::write(&lab.clone_url, remote.to_string_lossy().as_bytes()).unwrap();
        if already_bound {
            let initialized = lab
                .agit(&["init", "project"])
                .env("CI", "1")
                .output()
                .unwrap();
            assert!(initialized.status.success(), "{initialized:?}");
        }
        let bindings = lab.agit_home.join("workspaces");
        let snapshot = || {
            if bindings.try_exists().unwrap() {
                resume_json_snapshot(&bindings)
            } else {
                Default::default()
            }
        };
        let before = snapshot();
        assert_eq!(before.is_empty(), !already_bound);
        if already_bound {
            assert!(before.values().any(|bytes| {
                serde_json::from_slice::<serde_json::Value>(bytes).unwrap()["repo"] == "me/project"
            }));
        }
        let resume_args = ["--json", "resume", "einsia/qa@work", "--no-launch"];
        let output = lab.agit(&resume_args).env("CI", "1").output().unwrap();
        assert_eq!(output.status.code(), Some(3), "{output:?}");
        let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let action = &document["fix"][0];
        assert_eq!(action["requires_interaction"], false);
        assert!(!lab.repo("einsia", "qa").exists());
        assert_eq!(snapshot(), before);
        let argv: Vec<_> = action["argv"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .map(|value| value.as_str().unwrap())
            .collect();
        let mut retry = lab.agit(&argv);
        retry
            .current_dir(action["cwd"].as_str().unwrap())
            .env("CI", "1");
        for (key, value) in action["env"].as_object().unwrap() {
            retry.env(key, value.as_str().unwrap());
        }
        let cloned = retry.output().unwrap();
        assert!(cloned.status.success(), "{cloned:?}");
        assert_eq!(snapshot(), before);
        let received = Repo::open(lab.repo("einsia", "qa")).unwrap();
        assert_eq!(
            received.git(&["rev-parse", "refs/heads/work"]).unwrap(),
            source_head
        );
        assert!(active_links_on(&lab, "einsia", "qa", "work").is_empty());
        let prepared = lab.agit(&resume_args).env("CI", "1").output().unwrap();
        assert!(prepared.status.success(), "{prepared:?}");
        let document: serde_json::Value = serde_json::from_slice(&prepared.stdout).unwrap();
        assert_eq!(document["fix"], serde_json::json!([]));
        let claims = active_links_on(&lab, "einsia", "qa", "work");
        assert_eq!(claims.len(), 1);
        assert_eq!(
            claims[0].materialized_from.as_deref(),
            Some(source_head.as_str())
        );
        assert_ne!(claims[0].session_id, SID);
        assert_eq!(snapshot(), before);
        assert_eq!(bindings.exists(), already_bound);
        assert_eq!(
            received.git(&["rev-parse", "refs/heads/work"]).unwrap(),
            source_head
        );
    }
}

#[test]
fn new_clones_an_explicit_missing_repository_without_binding_cwd() {
    let lab = Lab::new();
    let remote = new_session_remote(&lab);
    let output = lab
        .agit(&["new", "einsia/qa", "-b", "fresh", "--no-launch"])
        .env("CI", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let repo = Repo::open(lab.repo("einsia", "qa")).expect("explicit repository is cloned");
    let identity = agit::hub::identity::read(&repo).unwrap().unwrap();
    assert_eq!(identity.agent_id, QA_AGENT_ID);
    assert_eq!(
        repo.git(&["remote", "get-url", "origin"]).unwrap().trim(),
        remote.to_string_lossy()
    );
    assert!(repo.has_ref("refs/heads/fresh"));
    assert_eq!(
        agit::domain::meta::line_at_ref(&repo, "fresh"),
        Some(agit::domain::meta::Line::Session)
    );
    assert_eq!(
        agit::domain::storage::materialize_at(repo.root(), "fresh", agit::domain::meta::LOG_FILE)
            .unwrap(),
        ""
    );
    assert_eq!(
        agit::domain::storage::materialize_at(repo.root(), "fresh", agit::domain::meta::VIEW_FILE)
            .unwrap(),
        ""
    );
    assert_eq!(
        fs::read_to_string(lab.work.join("AGENTS.md"))
            .unwrap()
            .trim(),
        "Inherited project instructions"
    );
    assert!(
        !lab.agit_home.join("workspaces").exists(),
        "internal clone must not bind cwd"
    );
    assert!(
        !lab.repo("me", "qa").exists(),
        "read-only pickup must not promote ownership"
    );
}

#[test]
fn new_refuses_unmanaged_runtime_before_cloning_unless_fresh() {
    let lab = Lab::new();
    new_session_remote(&lab);
    lab.append_turn(SID, 1, "Existing conversation", "Keep this evidence");
    let original = fs::read(lab.transcript(SID)).unwrap();
    let args = ["new", "einsia/qa", "-b", "fresh", "--no-launch"];
    let refused = lab
        .agit(&args)
        .env("CI", "1")
        .env("CLAUDE_CODE_SESSION_ID", SID)
        .output()
        .unwrap();
    assert_eq!(
        refused.status.code(),
        Some(agit::ExitCode::Precondition.as_i32())
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("agit import"));
    assert_eq!(lab.hub_requests.load(Ordering::SeqCst), 0);
    assert!(!lab.repo("einsia", "qa").exists());
    assert_eq!(fs::read(lab.transcript(SID)).unwrap(), original);
    let allowed = lab
        .agit(&args)
        .arg("--fresh")
        .env("CI", "1")
        .env("CLAUDE_CODE_SESSION_ID", SID)
        .output()
        .unwrap();
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert!(
        Repo::open(lab.repo("einsia", "qa"))
            .unwrap()
            .has_ref("refs/heads/fresh")
    );
    assert_eq!(fs::read(lab.transcript(SID)).unwrap(), original);
}

#[test]
fn new_keeps_existing_and_implicit_repository_resolution_offline() {
    let lab = Lab::new();
    let remote = new_session_remote(&lab);
    for args in [
        vec!["new", "qa", "-b", "fresh", "--no-launch"],
        vec!["new", "-b", "fresh", "--no-launch"],
    ] {
        let output = lab.agit(&args).env("CI", "1").output().unwrap();
        assert!(!output.status.success());
        assert_eq!(lab.hub_requests.load(Ordering::SeqCst), 0);
    }
    fs::create_dir_all(lab.repo("einsia", "qa").parent().unwrap()).unwrap();
    fs::rename(remote, lab.repo("einsia", "qa")).unwrap();
    let output = lab
        .agit(&["new", "einsia/qa", "-b", "fresh", "--no-launch"])
        .env("CI", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(lab.hub_requests.load(Ordering::SeqCst), 0);
}

#[test]
fn new_remote_pickup_failures_preserve_auth_and_network_exit_codes() {
    for scenario in ["unauthorized", "forbidden", "missing", "connection"] {
        for json in [false, true] {
            let lab = Lab::new();
            if scenario == "unauthorized" {
                fs::remove_dir_all(lab.agit_home.join("credentials")).unwrap();
            }
            let target = match scenario {
                "forbidden" => "acme/forbidden",
                "missing" => "acme/ghost",
                _ => "einsia/qa",
            };
            let mut args = vec!["new", target, "-b", "fresh", "--no-launch"];
            if json {
                args.push("--json");
            }
            let mut command = lab.agit(&args);
            command.env("CI", "1");
            if scenario == "connection" {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let unavailable = format!("http://{}", listener.local_addr().unwrap());
                drop(listener);
                command.env("AGIT_HUB_URL", unavailable);
            }
            let output = command.output().unwrap();
            let expected = if matches!(scenario, "unauthorized" | "forbidden") {
                agit::ExitCode::Auth.as_i32()
            } else {
                agit::ExitCode::Network.as_i32()
            };
            assert_eq!(
                output.status.code(),
                Some(expected),
                "scenario={scenario} json={json}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if json {
                let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(document["exit_code"], expected);
                assert_eq!(document["ok"], false);
                assert_eq!(document["command"], "new");
            }
            assert!(!lab.work.join("AGENTS.md").exists());
            assert!(!lab.agit_home.join("workspaces").exists());
            assert!(!lab.repo("einsia", "qa").exists());
        }
    }
}

#[test]
fn new_git_clone_auth_failures_preserve_auth_exit_codes() {
    for path in ["denied.git", "forbidden.git"] {
        for json in [false, true] {
            let lab = Lab::new();
            fs::write(&lab.clone_url, format!("{}/{path}", lab.hub)).unwrap();
            let mut args = vec!["new", "einsia/qa", "-b", "fresh", "--no-launch"];
            if json {
                args.push("--json");
            }
            let output = lab.agit(&args).env("CI", "1").output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(agit::ExitCode::Auth.as_i32()),
                "path={path} json={json}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if json {
                let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(document["exit_code"], agit::ExitCode::Auth.as_i32());
                assert_eq!(document["ok"], false);
            }
            assert!(lab.hub_requests.load(Ordering::SeqCst) > 1);
            assert!(!lab.work.join("AGENTS.md").exists());
            assert!(!lab.agit_home.join("workspaces").exists());
            assert!(Repo::open(lab.repo("einsia", "qa")).is_none());
        }
    }
}

#[test]
fn new_git_auth_with_a_credential_helper_is_independent_of_locale() {
    for json in [false, true] {
        let lab = Lab::new();
        fs::write(&lab.clone_url, format!("{}/denied.git", lab.hub)).unwrap();
        let mut args = vec!["new", "einsia/qa", "-b", "fresh", "--no-launch"];
        if json {
            args.push("--json");
        }
        let output = lab
            .agit(&args)
            .env("CI", "1")
            .env("LANG", "zh_CN.UTF-8")
            .env("LC_ALL", "zh_CN.UTF-8")
            .env("LANGUAGE", "zh_CN")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "credential.helper")
            .env(
                "GIT_CONFIG_VALUE_0",
                "!f() { printf 'username=fixture\\npassword=fixture\\n'; }; f",
            )
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(agit::ExitCode::Auth.as_i32()),
            "json={json}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if json {
            let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(document["exit_code"], agit::ExitCode::Auth.as_i32());
            assert_eq!(document["ok"], false);
        }
        assert!(!lab.work.join("AGENTS.md").exists());
        assert!(!lab.agit_home.join("workspaces").exists());
        assert!(Repo::open(lab.repo("einsia", "qa")).is_none());
    }
}

#[test]
fn new_preserves_local_state_when_remote_pickup_fails() {
    for scenario in ["missing", "unauthorized", "occupied", "inheritance"] {
        let lab = Lab::new();
        new_session_remote(&lab);
        if scenario == "unauthorized" {
            fs::remove_dir_all(lab.agit_home.join("credentials")).unwrap();
        }
        if scenario == "occupied" {
            fs::create_dir_all(lab.repo("einsia", "qa")).unwrap();
            fs::write(
                lab.repo("einsia", "qa").join("user-file"),
                "Preserve unrelated content",
            )
            .unwrap();
        }
        let target = match scenario {
            "missing" => "acme/ghost",
            "inheritance" => "einsia/qa@missing-file-line",
            _ => "einsia/qa",
        };
        let output = lab
            .agit(&["new", target, "-b", "fresh", "--no-launch"])
            .env("CI", "1")
            .output()
            .unwrap();
        assert!(!output.status.success(), "{scenario}");
        assert!(!lab.work.join("AGENTS.md").exists(), "{scenario}");
        assert!(!lab.agit_home.join("workspaces").exists(), "{scenario}");
        if let Some(repo) = Repo::open(lab.repo("einsia", "qa")) {
            assert!(!repo.has_ref("refs/heads/fresh"), "{scenario}");
        }
        if scenario == "occupied" {
            assert_eq!(
                fs::read_to_string(lab.repo("einsia", "qa").join("user-file")).unwrap(),
                "Preserve unrelated content"
            );
        }
    }
}

#[test]
fn new_clones_the_explicit_file_line_and_finishes_legacy_recovery() {
    use agit::domain::meta::{self, LayoutVersion};
    for legacy in [false, true] {
        let lab = Lab::new();
        let remote = new_session_remote(&lab);
        let source = Repo::at(&remote);
        let target = if legacy {
            let mut snapshot = meta::Meta::new_file_line();
            snapshot.layout = LayoutVersion::V0;
            meta::write(source.root(), &snapshot).unwrap();
            source.add_all().unwrap();
            source.commit("legacy shared file line").unwrap();
            "einsia/qa"
        } else {
            source.git(&["checkout", "-b", "shared/topic"]).unwrap();
            fs::write(
                source.root().join("AGENTS.md"),
                "Selected shared instructions\n",
            )
            .unwrap();
            source.add_all().unwrap();
            source.commit("specific shared file line").unwrap();
            source.git(&["checkout", "main"]).unwrap();
            "einsia/qa@shared/topic"
        };
        let output = lab
            .agit(&["new", target, "-b", "fresh", "--no-launch"])
            .env("CI", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "legacy={legacy}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let repo = Repo::open(lab.repo("einsia", "qa")).unwrap();
        assert_eq!(
            meta::read_at_ref(&repo, "fresh").unwrap().layout,
            LayoutVersion::V1
        );
        if legacy {
            assert_eq!(
                meta::read_at_ref(&repo, "main").unwrap().layout,
                LayoutVersion::V1
            );
        } else {
            assert_eq!(
                fs::read_to_string(lab.work.join("AGENTS.md"))
                    .unwrap()
                    .trim(),
                "Selected shared instructions"
            );
        }
        assert_eq!(
            fs::read_dir(lab.agit_home.join("layout-v1-recovery"))
                .unwrap()
                .count(),
            0
        );
    }
}

/// Namespace publication and directory movement exclude branch claim preparation.
#[test]
fn promotion_excludes_prepare_while_publishing_claim_ownership() {
    let lab = promotion_lab();
    let store = agit::domain::store::Store::at(lab.agit_home.join("store"));
    let claim_guard = agit::domain::link::lock(&store, "claude-code", SID).unwrap();
    let mut promote = lab.agit(&["clone", "einsia/qa", "--mine", "--no-bind"]);
    let promote = std::thread::spawn(move || promote.output().unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !lab.repo("me", "qa").join(".git").exists() {
        if promote.is_finished() {
            let output = promote.join().unwrap();
            panic!(
                "promotion exited before publication: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        assert!(
            std::time::Instant::now() < deadline,
            "promotion must reach claim publication"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut prepare = lab.agit(&["resume", "me/qa@work", "--no-launch", "--as", "codex"]);
    let (send, receive) = std::sync::mpsc::channel();
    let prepare = std::thread::spawn(move || send.send(prepare.output().unwrap()).unwrap());
    let premature = receive.recv_timeout(std::time::Duration::from_millis(500));
    drop(claim_guard);
    let promoted = promote.join().unwrap();
    assert!(
        promoted.status.success(),
        "{}",
        String::from_utf8_lossy(&promoted.stderr)
    );
    let waited = premature.is_err();
    let prepared = premature.unwrap_or_else(|_| {
        receive
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
    });
    prepare.join().unwrap();
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    assert!(
        waited,
        "prepare must wait until promotion publishes the source claim; active destination claims: {}",
        active_links_on(&lab, "me", "qa", "work").len()
    );
    assert_eq!(active_links_on(&lab, "me", "qa", "work").len(), 1);
    assert!(active_links_on(&lab, "einsia", "qa", "work").is_empty());
}

fn promotion_lab() -> Lab {
    let lab = Lab::new();
    lab.append_turn(SID, 1, "preserve the promoted line", "ok");
    let imported = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let source = Repo::open(lab.repo("einsia", "qa")).unwrap();
    agit::hub::identity::pin(
        &source,
        &agit::hub::identity::RemoteIdentity::new(&lab.hub, QA_AGENT_ID).unwrap(),
    )
    .unwrap();
    lab
}

/// A destination claim cannot be combined with the promoted source namespace.
#[test]
fn promotion_refuses_destination_claims_before_moving_the_checkout() {
    let lab = promotion_lab();
    let store = agit::domain::store::Store::at(lab.agit_home.join("store"));
    let mut existing =
        agit::domain::link::Link::new("codex", "destination-session", Some(&lab.work));
    existing.owner = Some("me".into());
    existing.agent = Some("qa".into());
    existing.branch = Some("work".into());
    agit::domain::link::write(&store, &existing).unwrap();
    let output = lab
        .agit(&["clone", "einsia/qa", "--mine", "--no-bind"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already has an active runtime claim")
    );
    assert!(lab.repo("einsia", "qa").join(".git").exists());
    assert!(!lab.repo("me", "qa").exists());
    let source = Repo::open(lab.repo("einsia", "qa")).unwrap();
    assert_eq!(
        agit::hub::identity::read(&source)
            .unwrap()
            .unwrap()
            .agent_id,
        QA_AGENT_ID
    );
    assert_eq!(active_links_on(&lab, "einsia", "qa", "work").len(), 1);
    assert_eq!(active_links_on(&lab, "me", "qa", "work").len(), 1);
}

fn resume_tracking_fixture(shape: &str) -> (Lab, Repo, String) {
    let lab = Lab::new();
    lab.append_turn(
        SID,
        1,
        "preserve selected tracking history",
        "synthetic reply",
    );
    let imported = lab
        .agit(&["import", SID, "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let repo = Repo::open(lab.repo("einsia", "qa")).unwrap();
    let head = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
    repo.git(&["checkout", "main"]).unwrap();
    assert_eq!(repo.current_branch().as_deref(), Some("main"));
    if shape != "absent" {
        repo.git(&[
            "remote",
            "add",
            "mirror",
            &format!("{}/unreachable.git", lab.hub),
        ])
        .unwrap();
        repo.git(&["config", "branch.work.remote", "mirror"])
            .unwrap();
        let topic = if shape == "diverged-unicode" {
            "topic\u{2003}"
        } else {
            "topic"
        };
        repo.git(&[
            "config",
            "branch.work.merge",
            &format!("refs/heads/{topic}"),
        ])
        .unwrap();
        if shape != "missing" {
            let ancestor = repo.git(&["rev-parse", &format!("{head}^")]).unwrap();
            let tracking = match shape {
                "equal" => head.clone(),
                "local-ahead" => ancestor,
                "remote-ahead" | "diverged" | "diverged-unicode" => {
                    let tree = repo
                        .git(&["rev-parse", &format!("{head}^{{tree}}")])
                        .unwrap();
                    let parent = if shape == "remote-ahead" {
                        &head
                    } else {
                        &ancestor
                    };
                    repo.git(&[
                        "commit-tree",
                        &tree,
                        "-p",
                        parent,
                        "-m",
                        "synthetic tracking advance",
                    ])
                    .unwrap()
                }
                _ => panic!("unknown tracking fixture shape"),
            };
            repo.git(&[
                "update-ref",
                &format!("refs/remotes/mirror/{topic}"),
                &tracking,
            ])
            .unwrap();
        }
    }
    (lab, repo, head)
}

fn resume_json_snapshot(
    root: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| {
            entry.file_type().is_file()
                && matches!(
                    entry.path().extension().and_then(|ext| ext.to_str()),
                    Some("json" | "jsonl")
                )
        })
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

/// Known tracking advances exclude both native reuse and replacement of the selected branch.
#[test]
fn resume_refuses_known_tracking_advances_even_with_force() {
    for shape in ["remote-ahead", "diverged", "diverged-unicode"] {
        for force in [false, true] {
            let (lab, repo, head) = resume_tracking_fixture(shape);
            let native_before = resume_json_snapshot(&lab.home);
            let links_before = resume_json_snapshot(&lab.agit_home.join("store"));
            let refs_before = repo.git(&["show-ref"]).unwrap();
            let requests_before = lab.hub_requests.load(Ordering::SeqCst);
            let mut args = vec!["resume", "einsia/qa@work", "--no-launch"];
            if force {
                args.push("--force");
            }
            let output = lab.agit(&args).env("CI", "1").output().unwrap();
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                output.status.code(),
                Some(4),
                "{shape} force={force}: {diagnostic}"
            );
            assert!(diagnostic.contains("tracking"), "{diagnostic}");
            assert!(diagnostic.contains("einsia/qa@work"), "{diagnostic}");
            assert_eq!(resume_json_snapshot(&lab.home), native_before);
            assert_eq!(
                resume_json_snapshot(&lab.agit_home.join("store")),
                links_before
            );
            assert_eq!(repo.git(&["show-ref"]).unwrap(), refs_before);
            assert_eq!(repo.git(&["rev-parse", "refs/heads/work"]).unwrap(), head);
            assert_eq!(lab.hub_requests.load(Ordering::SeqCst), requests_before);
        }
    }
}

/// A reconciliation agent can prepare the frozen target while ordinary continuation stays blocked.
#[cfg(all(unix, feature = "rc"))]
#[test]
fn interactive_merge_launches_for_diverged_tracking_after_settlement() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    let (lab, repo, initial_head) = resume_tracking_fixture("diverged");
    let tracking = repo
        .git(&["rev-parse", "refs/remotes/mirror/topic"])
        .unwrap();
    lab.append_turn(
        SID,
        2,
        "settle before freezing the merge",
        "preserve this reply",
    );
    let bin = lab._tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let runtime = bin.join("codex");
    fs::write(
        &runtime,
        "#!/bin/sh\nprintf '%s\\0' \"$AGIT_SESSION\" \"$AGIT_MERGE_TX\" \"$@\" > \"$AGIT_TEST_MERGE_LAUNCH\"\n",
    )
    .unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
    let capture = lab._tmp.path().join("merge-launch");
    let source = format!("einsia/qa@{}", agit::domain::meta::id_from_sha(&tracking));
    let template = lab.agit(&[]);
    let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
    command.args([
        "merge",
        &source,
        "--into",
        "einsia/qa@work",
        "--as",
        "codex",
    ]);
    command.cwd(&lab.work);
    command.env_clear();
    for (name, value) in template.get_envs() {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    let mut path = vec![bin];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    command.env("PATH", std::env::join_paths(path).unwrap());
    command.env("AGIT_YES", "1");
    command.env("AGIT_TEST_MERGE_LAUNCH", &capture);
    let pty = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize::default())
        .unwrap();
    let mut reader = pty.master.try_clone_reader().unwrap();
    let output = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(pty.master);
    let output = output.join().unwrap();
    assert!(status.is_some_and(|status| status.success()), "{output}");
    assert_eq!(output.matches("fork point  ").count(), 1, "{output}");
    assert!(
        output.contains("this side  +2 turns    source side  +1 turns"),
        "{output}"
    );
    let captured = fs::read(&capture).unwrap_or_else(|error| panic!("{error}: {output}"));
    let arguments: Vec<&str> = captured
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| std::str::from_utf8(part).unwrap())
        .collect();
    assert_eq!(
        &arguments[..3],
        &["einsia/qa@work", "einsia/qa@work", "resume"]
    );
    assert!(arguments.last().unwrap().contains("as the merge agent"));
    assert!(arguments.last().unwrap().contains(&source));
    let tx = agit::domain::mergetx::read(repo.root()).unwrap().unwrap();
    assert_eq!(tx.target, "work");
    assert_eq!(tx.source_head, tracking);
    assert_ne!(tx.target_head, initial_head);
    assert_eq!(
        tx.target_head,
        repo.git(&["rev-parse", "refs/heads/work"]).unwrap()
    );
    assert_eq!(
        tx.base,
        repo.git(&["merge-base", &tx.target_head, &tracking])
            .unwrap()
    );
    let links = active_links_on(&lab, "einsia", "qa", "work");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].source, "codex");
    assert_eq!(
        links[0].materialized_from.as_deref(),
        Some(tx.target_head.as_str())
    );
    assert_eq!(arguments[3], links[0].session_id);
    let installed = resume_json_snapshot(&lab.home.join(".codex/sessions"));
    let transcript = installed
        .iter()
        .find(|(path, _)| path.to_string_lossy().contains(&links[0].session_id))
        .map(|(_, content)| String::from_utf8_lossy(content))
        .unwrap();
    assert!(transcript.contains("settle before freezing the merge"));
    let native_before = resume_json_snapshot(&lab.home);
    let links_before = resume_json_snapshot(&lab.agit_home.join("store"));
    let resumed = lab
        .agit(&["resume", "einsia/qa@work", "--force", "--no-launch"])
        .env("AGIT_MERGE_TX", "einsia/qa@work")
        .output()
        .unwrap();
    assert_eq!(resumed.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&resumed.stderr).contains("tracking"));
    assert_eq!(resume_json_snapshot(&lab.home), native_before);
    assert_eq!(
        resume_json_snapshot(&lab.agit_home.join("store")),
        links_before
    );
    let aborted = lab
        .agit(&["merge", "--abort", "--into", "einsia/qa@work"])
        .output()
        .unwrap();
    assert!(aborted.status.success());
}

/// Invalid graph preflight cannot settle the target's pending transcript before refusing.
#[test]
fn merge_preflight_refuses_before_settling_a_pending_target() {
    let (lab, repo, head) = resume_tracking_fixture("diverged");
    let tracking = repo
        .git(&["rev-parse", "refs/remotes/mirror/topic"])
        .unwrap();
    let source = format!("einsia/qa@{}", agit::domain::meta::id_from_sha(&tracking));
    lab.append_turn(
        SID,
        2,
        "keep pending until preflight accepts",
        "synthetic pending reply",
    );
    fs::write(
        repo.common_dir().unwrap().join("shallow"),
        format!("{head}\n"),
    )
    .unwrap();
    let native_before = resume_json_snapshot(&lab.home);
    let links_before = resume_json_snapshot(&lab.agit_home.join("store"));
    let refs_before = repo.git(&["show-ref"]).unwrap();
    let output = lab
        .agit(&["merge", &source, "--into", "einsia/qa@work", "--manual"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("shallow repository"),
        "{output:?}"
    );
    assert_eq!(repo.git(&["show-ref"]).unwrap(), refs_before);
    assert_eq!(repo.git(&["rev-parse", "refs/heads/work"]).unwrap(), head);
    assert_eq!(resume_json_snapshot(&lab.home), native_before);
    assert_eq!(
        resume_json_snapshot(&lab.agit_home.join("store")),
        links_before
    );
    assert!(agit::domain::mergetx::read(repo.root()).unwrap().is_none());
}

/// Offline continuation does not depend on an existing published tracking ref.
#[test]
fn resume_allows_absent_equal_or_integrated_tracking_without_fetching() {
    for shape in ["absent", "missing", "equal", "local-ahead"] {
        let (lab, repo, head) = resume_tracking_fixture(shape);
        let requests_before = lab.hub_requests.load(Ordering::SeqCst);
        let output = lab
            .agit(&["resume", "einsia/qa@work", "--no-launch"])
            .env("CI", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{shape}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(SID));
        assert_eq!(repo.git(&["rev-parse", "refs/heads/work"]).unwrap(), head);
        assert_eq!(repo.current_branch().as_deref(), Some("main"));
        assert_eq!(lab.hub_requests.load(Ordering::SeqCst), requests_before);
    }
}

/// A configured ref that cannot be inspected is not an absent tracking history.
#[test]
fn resume_refuses_unreadable_or_invalid_tracking_without_writes() {
    for fault in ["broken-ref", "dangling-ref", "noncommit", "unknown-remote"] {
        let (lab, repo, head) = resume_tracking_fixture("equal");
        match fault {
            "broken-ref" => {
                let path = repo
                    .git(&["rev-parse", "--git-path", "refs/remotes/mirror/topic"])
                    .unwrap();
                fs::write(repo.root().join(path), "invalid object identity\n").unwrap();
            }
            "dangling-ref" => {
                repo.git(&[
                    "symbolic-ref",
                    "refs/remotes/mirror/topic",
                    "refs/remotes/mirror/missing",
                ])
                .unwrap();
            }
            "noncommit" => {
                let blob = repo
                    .git(&["rev-parse", "refs/heads/work:session/meta.json"])
                    .unwrap();
                repo.git(&["update-ref", "refs/remotes/mirror/topic", &blob])
                    .unwrap();
            }
            "unknown-remote" => {
                repo.git(&["config", "branch.work.remote", "missing-remote"])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let native_before = resume_json_snapshot(&lab.home);
        let links_before = resume_json_snapshot(&lab.agit_home.join("store"));
        let requests_before = lab.hub_requests.load(Ordering::SeqCst);
        let output = lab
            .agit(&["resume", "einsia/qa@work", "--force", "--no-launch"])
            .env("CI", "1")
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{fault} was treated as absent tracking"
        );
        assert_eq!(resume_json_snapshot(&lab.home), native_before);
        assert_eq!(
            resume_json_snapshot(&lab.agit_home.join("store")),
            links_before
        );
        assert_eq!(repo.git(&["rev-parse", "refs/heads/work"]).unwrap(), head);
        assert_eq!(lab.hub_requests.load(Ordering::SeqCst), requests_before);
    }
}

/// Missing promised history must refuse without fetching objects or changing runtime state.
#[test]
fn resume_tracking_does_not_lazy_fetch_missing_promised_objects() {
    for missing in ["tip", "ancestor"] {
        let (lab, repo, head) = resume_tracking_fixture("diverged");
        let tracking = repo
            .git(&["rev-parse", "refs/remotes/mirror/topic"])
            .unwrap();
        let absent = if missing == "tip" {
            tracking
        } else {
            repo.git(&["rev-parse", &format!("{head}^")]).unwrap()
        };
        let remote = lab._tmp.path().join("promisor.git");
        repo.git(&[
            "clone",
            "--bare",
            "--no-hardlinks",
            &repo.root().to_string_lossy(),
            &remote.to_string_lossy(),
        ])
        .unwrap();
        repo.git(&[
            "--git-dir",
            &remote.to_string_lossy(),
            "config",
            "uploadpack.allowAnySHA1InWant",
            "true",
        ])
        .unwrap();
        repo.git(&[
            "--git-dir",
            &remote.to_string_lossy(),
            "config",
            "uploadpack.allowFilter",
            "true",
        ])
        .unwrap();
        repo.git(&["remote", "set-url", "mirror", &remote.to_string_lossy()])
            .unwrap();
        repo.git(&["config", "remote.mirror.promisor", "true"])
            .unwrap();
        repo.git(&["config", "remote.mirror.partialclonefilter", "blob:none"])
            .unwrap();
        let objects = repo.root().join(".git/objects");
        fs::remove_file(objects.join(&absent[..2]).join(&absent[2..])).unwrap();
        let object_snapshot = || {
            walkdir::WalkDir::new(&objects)
                .into_iter()
                .map(Result::unwrap)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| (entry.path().to_path_buf(), fs::read(entry.path()).unwrap()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let objects_before = object_snapshot();
        let native_before = resume_json_snapshot(&lab.home);
        let links_before = resume_json_snapshot(&lab.agit_home.join("store"));
        let refs_before = repo
            .git(&["for-each-ref", "--format=%(refname)%09%(objectname)"])
            .unwrap();
        let trace = lab._tmp.path().join("tracking-trace");
        let output = lab
            .agit(&["resume", "einsia/qa@work", "--force", "--no-launch"])
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_NO_LAZY_FETCH", "0")
            .env("GIT_TRACE", &trace)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{missing}: {output:?}");
        assert!(object_snapshot() == objects_before, "{missing}: {output:?}");
        let trace = fs::read_to_string(trace).unwrap();
        assert!(!trace.contains("upload-pack"), "{missing}: {trace}");
        assert_eq!(resume_json_snapshot(&lab.home), native_before);
        assert_eq!(
            resume_json_snapshot(&lab.agit_home.join("store")),
            links_before
        );
        assert_eq!(
            repo.git(&["for-each-ref", "--format=%(refname)%09%(objectname)"])
                .unwrap(),
            refs_before
        );
        assert_eq!(repo.git(&["rev-parse", "refs/heads/work"]).unwrap(), head);
    }
}

/// The recovery command names the frozen tracking graph and survives shell argument parsing.
#[cfg(unix)]
#[test]
fn resume_tracking_recovery_hint_resolves_the_frozen_graph() {
    for branch in ["work", "work;literal'quoted‘’‚‛"] {
        let (lab, repo, head) = resume_tracking_fixture("diverged");
        if branch != "work" {
            repo.git(&["branch", "-m", "work", branch]).unwrap();
        }
        let tracking = repo
            .git(&["rev-parse", "refs/remotes/mirror/topic"])
            .unwrap();
        let base = repo.git(&["merge-base", &head, &tracking]).unwrap();
        let target = format!("einsia/qa@{branch}");
        let native_before = resume_json_snapshot(&lab.home);
        let links_before = resume_json_snapshot(&lab.agit_home.join("store"));
        let refs_before = repo.git(&["show-ref"]).unwrap();
        let requests_before = lab.hub_requests.load(Ordering::SeqCst);
        let output = lab
            .agit(&["resume", &target, "--no-launch"])
            .env("CI", "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        let command = diagnostic.split('`').nth(1).unwrap();
        let prefix = command.strip_suffix("--manual").unwrap_or_else(|| {
            panic!("recovery must not require a launched runtime: {diagnostic}")
        });
        let template = lab.agit(&[]);
        let mut shell = Command::new("sh");
        shell
            .args(["-c", &format!("{prefix}--dry-run")])
            .current_dir(&lab.work)
            .env_clear();
        for (name, value) in template.get_envs() {
            if let Some(value) = value {
                shell.env(name, value);
            }
        }
        let mut path = vec![
            std::path::Path::new(env!("CARGO_BIN_EXE_agit"))
                .parent()
                .unwrap()
                .to_path_buf(),
        ];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let recovery = shell
            .env("PATH", std::env::join_paths(path).unwrap())
            .env("CI", "1")
            .output()
            .unwrap();
        assert!(recovery.status.success(), "{command}: {recovery:?}");
        let report = String::from_utf8_lossy(&recovery.stdout);
        assert!(report.contains(&base[..9]), "{report}");
        assert_eq!(resume_json_snapshot(&lab.home), native_before);
        assert_eq!(
            resume_json_snapshot(&lab.agit_home.join("store")),
            links_before
        );
        assert_eq!(repo.git(&["show-ref"]).unwrap(), refs_before);
        assert_eq!(lab.hub_requests.load(Ordering::SeqCst), requests_before);
    }
}
