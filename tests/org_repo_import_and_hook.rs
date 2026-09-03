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
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
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
            } else if path == "/api/agents/einsia/qa" {
                agent("einsia", "qa")
            } else if path == "/api/agents/einsia/locked" {
                agent("einsia", "locked")
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
        fs::write(&clone_url, "x").unwrap();
        fs::write(&latest_version, env!("CARGO_PKG_VERSION")).unwrap();
        let hub = fake_hub(
            gate_closed.clone(),
            clone_url.clone(),
            latest_version.clone(),
            Arc::clone(&version_requests),
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
        let key = agit::infra::config::hub_host_key(&hub);
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

    // Ordinary context resolution reaches the org one too: with no AGIT_SESSION and no directory
    // binding, resolving through "the only adopted session in this directory" and through the
    // harness's session id environment variable must both land on einsia/qa.
    for env in [vec![], vec![("CLAUDE_SESSION_ID", SID)]] {
        let mut cmd = lab.agit(&["log", "--oneline"]);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("start the org line"),
            "context resolution ({env:?}) must reach the org repo:\n{stdout}{}",
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
