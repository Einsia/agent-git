//! Contracts of the front door, pinned through the binary against a fake Hub.
//!
//! `agit on` uploads every future turn of a folder's sessions without asking again, so the
//! destination it chooses on its own must never be readable by anyone. The fake Hub records every
//! creation request:
//!
//! - among the Hub repositories linked to the folder's Git origin, a public one is never chosen,
//!   even when it is writable;
//! - with nothing linked, the private repository named after the project in the Hub organization
//!   that owns the code origin is joined instead of a new personal one, and a public one there is
//!   passed over;
//! - when the account's repository named after the folder is public, a private name is used;
//! - a folder already set up for a public repository is refused before anything is created,
//!   linked or enabled, and the refusal names the command that uploads there knowingly.
//!
//! An implementation that filters only on write access, or that takes an existing binding at its
//! word, sends a creation request naming the public repository, or enables the folder.
//!
//! A front-door command that needs an account and finds none starts the sign-in itself: outside a
//! terminal it records the request and answers with the link and exit code 8, so an agent hands the
//! link to the person and reruns the same command. One that only says "run agit login" leaves
//! the agent a second, different command to discover.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

const ORIGIN: &str = "https://forge.example/team/app.git";

/// Serves `for-repo` (a public and a private repository linked to [`ORIGIN`], both writable),
/// `me/app` (public) and `me/solo` (public); every other repository is missing. Records each
/// `POST /api/agents` body. Creation answers succeed; nothing after them is served, so a command
/// that reaches binding fails there, after the request under test was recorded.
fn fake_hub(created: Arc<Mutex<Vec<serde_json::Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server_base = base.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let Ok(k) = sock.read(&mut chunk) else {
                    break None;
                };
                if k == 0 {
                    break None;
                }
                buf.extend_from_slice(&chunk[..k]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(i + 4);
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
            let length = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            while buf.len() < header_end + length {
                let Ok(k) = sock.read(&mut chunk) else { break };
                if k == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..k]);
            }
            let body = &buf[header_end..(header_end + length).min(buf.len())];
            let line = head.lines().next().unwrap_or_default().to_string();
            let path = line.split_whitespace().nth(1).unwrap_or_default();
            let agent = |owner: &str, name: &str, visibility: &str, id: &str| {
                serde_json::json!({
                    "agent_id": id, "owner": owner, "name": name, "visibility": visibility,
                    "clone_url": format!("{server_base}/{owner}/{name}.git"),
                })
            };
            let (status, reply) = if line.starts_with("POST /api/auth/cli/session ") {
                (
                    "200 OK",
                    serde_json::json!({
                        "state": "state-1",
                        "url": format!("{server_base}/auth/cli?state=state-1"),
                        "expires_in": 600,
                    }),
                )
            } else if line.starts_with("POST /api/agents ") {
                let request: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                created.lock().unwrap().push(request.clone());
                let owner = request["owner"].as_str().unwrap_or("me").to_string();
                let name = request["name"].as_str().unwrap_or_default().to_string();
                (
                    "200 OK",
                    serde_json::json!({
                        "agent_id": "dddddddd-0000-4000-8000-000000000004",
                        "owner": owner, "name": name,
                        "push_url": format!("{server_base}/{owner}/{name}.git"),
                        "web_url": format!("{server_base}/@{owner}/{name}"),
                    }),
                )
            } else if path.starts_with("/api/agents/for-repo?") && path.contains("epsilon") {
                (
                    "200 OK",
                    serde_json::json!([
                        agent(
                            "me",
                            "gear",
                            "private",
                            "55555555-0000-4000-8000-000000000055"
                        ),
                        agent(
                            "epsilon",
                            "gear-agents",
                            "private",
                            "66666666-0000-4000-8000-000000000066"
                        ),
                    ]),
                )
            } else if path.starts_with("/api/agents/for-repo?")
                && ["widget", "beta", "gamma", "delta"]
                    .iter()
                    .any(|org| path.contains(org))
            {
                ("200 OK", serde_json::json!([]))
            } else if let Some(org) = path
                .strip_prefix("/api/orgs/")
                .filter(|org| ["beta", "gamma", "delta"].contains(org))
            {
                let role = if org == "delta" { "owner" } else { "member" };
                (
                    "200 OK",
                    serde_json::json!({"name": org, "role": role, "created_at": "2026-01-01T00:00:00Z"}),
                )
            } else if path == "/api/agents?owner=beta" {
                (
                    "200 OK",
                    serde_json::json!([agent(
                        "beta",
                        "gizmo-agents",
                        "private",
                        "44444444-0000-4000-8000-000000000044"
                    )]),
                )
            } else if path == "/api/agents?owner=gamma" || path == "/api/agents?owner=delta" {
                ("200 OK", serde_json::json!([]))
            } else if path.starts_with("/beta/gizmo-agents.git/info/refs") {
                (
                    "403 Forbidden",
                    serde_json::json!({"error": "forbidden", "kind": "forbidden"}),
                )
            } else if path == "/api/orgs/acme" {
                (
                    "200 OK",
                    serde_json::json!({"name": "acme", "role": "member", "created_at": "2026-01-01T00:00:00Z"}),
                )
            } else if path == "/api/agents?owner=acme" {
                (
                    "200 OK",
                    serde_json::json!([
                        agent(
                            "acme",
                            "widget",
                            "public",
                            "11111111-0000-4000-8000-000000000011"
                        ),
                        agent(
                            "acme",
                            "widget-agents",
                            "private",
                            "22222222-0000-4000-8000-000000000022"
                        ),
                        agent(
                            "acme",
                            "gadget",
                            "private",
                            "33333333-0000-4000-8000-000000000033"
                        ),
                    ]),
                )
            } else if path.starts_with("/api/agents/for-repo?") {
                (
                    "200 OK",
                    serde_json::json!([
                        agent(
                            "team",
                            "shared",
                            "public",
                            "aaaaaaaa-0000-4000-8000-000000000001"
                        ),
                        agent(
                            "team",
                            "app-sessions",
                            "private",
                            "bbbbbbbb-0000-4000-8000-000000000002"
                        ),
                    ]),
                )
            } else if path == "/api/agents/me/app" {
                (
                    "200 OK",
                    agent(
                        "me",
                        "app",
                        "public",
                        "cccccccc-0000-4000-8000-000000000003",
                    ),
                )
            } else if path == "/api/agents/me/solo" {
                (
                    "200 OK",
                    agent(
                        "me",
                        "solo",
                        "public",
                        "eeeeeeee-0000-4000-8000-000000000005",
                    ),
                )
            } else if path.ends_with(".git/info/refs?service=git-receive-pack") {
                ("200 OK", serde_json::json!("0000"))
            } else {
                (
                    "404 Not Found",
                    serde_json::json!({"error": "not found", "kind": "not_found"}),
                )
            };
            let reply = match reply {
                serde_json::Value::String(raw) => raw,
                other => other.to_string(),
            };
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .as_bytes(),
            );
        }
    });
    base
}

struct Lab {
    _tmp: tempfile::TempDir,
    hub: String,
    home: std::path::PathBuf,
    agit_home: std::path::PathBuf,
    /// Holds a stand-in `claude`, so a folder can be turned on whatever this machine has.
    bin: std::path::PathBuf,
    created: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Lab {
    fn signed_out() -> Lab {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("claude"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(bin.join("claude.cmd"), "@exit /b 0\r\n").unwrap();
        let created = Arc::new(Mutex::new(Vec::new()));
        let hub = fake_hub(Arc::clone(&created));
        Lab {
            agit_home: tmp.path().join("agit"),
            _tmp: tmp,
            hub,
            home,
            bin,
            created,
        }
    }

    fn new() -> Lab {
        let lab = Lab::signed_out();
        let credential = agit::infra::credentials::HubCredential {
            account_id: None,
            username: "me".into(),
            email: None,
            hub: Some(lab.hub.clone()),
            access_token: "fake".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "fake".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let key = agit::infra::config::hub_host_key(&lab.hub).unwrap();
        agit::infra::credentials::save_at(
            &lab.agit_home
                .join("credentials")
                .join(format!("{key}.json")),
            &credential,
        )
        .unwrap();
        lab
    }

    /// A project folder under the home directory, optionally a Git checkout of [`ORIGIN`].
    fn folder(&self, name: &str, origin: bool) -> std::path::PathBuf {
        self.checkout(name, origin.then_some(ORIGIN))
    }

    fn checkout(&self, name: &str, origin: Option<&str>) -> std::path::PathBuf {
        let dir = self.home.join("code").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(origin) = origin {
            let git = |args: &[&str]| {
                assert!(
                    Command::new("git")
                        .args(args)
                        .current_dir(&dir)
                        .env("GIT_CONFIG_NOSYSTEM", "1")
                        .env("HOME", &self.home)
                        .status()
                        .unwrap()
                        .success()
                );
            };
            git(&["init", "-q"]);
            git(&["remote", "add", "origin", origin]);
        }
        dir.canonicalize().unwrap()
    }

    fn agit(&self, dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env(
                "PATH",
                std::env::join_paths(std::iter::once(self.bin.clone()).chain(
                    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
                ))
                .unwrap(),
            )
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.agit_home)
            .env("AGIT_HUB_URL", &self.hub)
            .env("AGIT_TELEMETRY_DISABLED", "1")
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap()
    }

    /// A Claude Code conversation in `cwd` with one finished reply.
    fn conversation(&self, cwd: &std::path::Path, id: &str, prompt: &str) {
        let native = self
            .home
            .join(".claude")
            .join("projects")
            .join(agit::adapter::claude_code::slug_for(cwd));
        std::fs::create_dir_all(&native).unwrap();
        let records = [
            serde_json::json!({"type":"user","uuid":format!("{id}-u"),"sessionId":id,"cwd":cwd,
                "timestamp":"2026-10-25T10:00:00Z","message":{"role":"user","content":prompt}}),
            serde_json::json!({"type":"assistant","uuid":format!("{id}-a"),"parentUuid":format!("{id}-u"),
                "sessionId":id,"cwd":cwd,"timestamp":"2026-10-25T10:00:05Z",
                "message":{"role":"assistant","content":[{"type":"text","text":"Done."}]}}),
        ];
        let text: String = records.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(native.join(format!("{id}.jsonl")), text).unwrap();
    }

    /// Binds `dir` on its own to `repo`, as `agit bind` records it.
    fn bind(&self, dir: &std::path::Path, repo: &str) {
        use sha2::{Digest, Sha256};
        let canonical = dir.canonicalize().unwrap();
        let id = &hex::encode(Sha256::digest(canonical.to_string_lossy().as_bytes()))[..16];
        let bindings = self.agit_home.join("workspaces");
        std::fs::create_dir_all(&bindings).unwrap();
        std::fs::write(
            bindings.join(format!("{id}.json")),
            serde_json::to_vec(&serde_json::json!({"dir": canonical, "repo": repo})).unwrap(),
        )
        .unwrap();
    }

    fn created(&self) -> Vec<(String, String)> {
        self.created
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["owner"].as_str().unwrap_or("me").to_string(),
                    r["name"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }
}

#[test]
fn an_inferred_destination_is_never_public() {
    let lab = Lab::new();

    let linked = lab.folder("app", true);
    lab.agit(&linked, &["on"]);
    assert_eq!(
        lab.created(),
        [("team".to_string(), "app-sessions".to_string())],
        "the private repository linked to the origin is chosen; the public one never is"
    );

    lab.created.lock().unwrap().clear();
    let unlinked = lab.folder("solo", false);
    lab.agit(&unlinked, &["on"]);
    assert_eq!(
        lab.created(),
        [("me".to_string(), "solo-sessions".to_string())],
        "a public repository of the folder's name is passed over for a private one"
    );

    lab.created.lock().unwrap().clear();
    let team = lab.checkout("widget", Some("git@forge.example:acme/widget.git"));
    lab.agit(&team, &["on"]);
    assert_eq!(
        lab.created(),
        [("acme".to_string(), "widget-agents".to_string())],
        "the team's private repository is joined, and its origin linked, instead of a new one"
    );
}

#[test]
fn a_folder_set_up_for_a_public_repository_is_refused_before_anything_changes() {
    let lab = Lab::new();
    let dir = lab.folder("app", false);
    let binding = agit::domain::workspace::Workspace {
        dir: dir.to_string_lossy().into_owned(),
        repo: "me/app".into(),
    };
    let path = lab.agit_home.join("workspaces").join(
        agit::domain::workspace::path_for(&dir)
            .unwrap()
            .file_name()
            .unwrap(),
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string(&binding).unwrap()).unwrap();

    let out = lab.agit(&dir, &["on"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert!(
        stderr.contains("agit on --repo me/app --public"),
        "the refusal names how to upload there knowingly: {stderr}"
    );
    assert!(lab.created().is_empty(), "nothing is created or linked");
    let policies = std::fs::read_dir(lab.agit_home.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
        .count();
    assert_eq!(policies, 0, "the folder is not enabled");
}

/// A project whose code belongs to an organization keeps its sessions with the team. The
/// organization's repository wins over a personal one linked to the same code. A team
/// repository the person cannot upload to is named with the page where an owner gives access,
/// and nothing is created. An owner is never given a repository in the organization without
/// picking it: without a terminal both choices are named and nothing is created. A member with
/// no team repository gets their own private one. An implementation that fell back to a
/// personal or organization repository silently in either of the first two cases fails here.
#[test]
fn a_teams_project_goes_to_the_team_or_says_why_not() {
    let lab = Lab::new();

    let blocked = lab.checkout("gizmo", Some("git@forge.example:beta/gizmo.git"));
    let out = lab.agit(&blocked, &["on"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert!(
        stderr.contains("beta/gizmo-agents holds this project's sessions")
            && stderr.contains(&format!("{}/@beta/gizmo-agents/settings", lab.hub))
            && stderr.contains("agit on --repo me/gizmo"),
        "{stderr}"
    );
    assert!(lab.created().is_empty(), "nothing is created");

    let owned = lab.checkout("thing", Some("git@forge.example:delta/thing.git"));
    let out = lab.agit(&owned, &["on"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(8), "{stderr}");
    assert!(
        stderr.contains("agit on --repo delta/thing-agents")
            && stderr.contains("agit on --repo me/thing")
            && stderr.contains("private to delta: its owners and granted teams can read it"),
        "{stderr}"
    );
    assert!(lab.created().is_empty(), "nothing is created");

    let both = lab.checkout("gear", Some("git@forge.example:epsilon/gear.git"));
    lab.agit(&both, &["on"]);
    assert_eq!(
        lab.created(),
        [("epsilon".to_string(), "gear-agents".to_string())],
        "the organization's repository wins over a personal one linked to the same code"
    );
    lab.created.lock().unwrap().clear();

    let member = lab.checkout("doohickey", Some("git@forge.example:gamma/doohickey.git"));
    lab.agit(&member, &["on"]);
    assert_eq!(
        lab.created(),
        [("me".to_string(), "doohickey".to_string())],
        "a member without a team repository gets a private one of their own"
    );
}

/// A folder that no agent on this computer could upload from is refused before anything exists
/// on the Hub; creating the repository first would leave an empty one behind every attempt.
#[cfg(unix)]
#[test]
fn nothing_is_created_for_a_folder_no_agent_here_uploads_from() {
    let lab = Lab::new();
    let dir = lab.folder("solo", false);
    let out = Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(["on"])
        .current_dir(&dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &lab.home)
        .env("AGIT_HOME", &lab.agit_home)
        .env("AGIT_HUB_URL", &lab.hub)
        .env("AGIT_TELEMETRY_DISABLED", "1")
        .env("CLAUDE_CONFIG_DIR", lab.home.join(".claude"))
        .env("CODEX_HOME", lab.home.join(".codex"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert!(stderr.contains("install Claude Code"), "{stderr}");
    assert!(lab.created().is_empty(), "nothing is created on the Hub");
}

#[test]
fn a_command_that_needs_an_account_starts_the_sign_in() {
    let lab = Lab::signed_out();
    let dir = lab.folder("app", false);
    let out = lab.agit(&dir, &["--json", "on"]);
    assert_eq!(out.status.code(), Some(8));
    let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let value = &document["result"]["value"];
    assert_eq!(
        value["sign_in_url"],
        format!("{}/auth/cli?state=state-1", lab.hub),
        "{document}"
    );
    assert_eq!(value["next"]["then"], "agit on", "{document}");
    assert!(lab.created().is_empty());
}

/// Saving a folder for the first time uploads the sessions already in it, so without a person to
/// ask and without a conversation to name, nothing is uploaded or set up: the refusal names one
/// session or `--all`. An implementation that uploaded every session there on an agent's bare
/// `agit save` fails here.
#[test]
fn an_unattended_save_does_not_upload_a_folders_earlier_sessions() {
    let lab = Lab::new();
    let dir = lab.folder("app", false);
    lab.conversation(
        &dir,
        "11111111-1111-4111-8111-111111111111",
        "Fix the login page",
    );
    lab.conversation(
        &dir,
        "22222222-2222-4222-8222-222222222222",
        "Add a dark mode",
    );

    let out = lab.agit(&dir, &["--json", "save"]);
    let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(8), "{document}");
    let text = document.to_string();
    assert!(text.contains("all 2 sessions"), "{document}");
    assert!(text.contains("agit save --all"), "{document}");
    assert!(lab.created().is_empty(), "no repository is created");
    let policies = std::fs::read_dir(lab.agit_home.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
        .count();
    assert_eq!(policies, 0, "the folder is not turned on");
}

/// A directory bound on its own inside a project keeps its conversations. Saving the project
/// leaves them out, and saving from inside the directory takes only them, for its own
/// destination. An implementation that gathered every conversation below the Git top level would
/// count the nested conversation in the project, and from inside the directory would save the
/// whole project.
#[test]
fn a_directory_bound_on_its_own_keeps_its_sessions() {
    let lab = Lab::new();
    let dir = lab.folder("app", true);
    let tools = dir.join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let tools = tools.canonicalize().unwrap();
    lab.bind(&tools, "me/tools");
    lab.conversation(
        &dir,
        "11111111-1111-4111-8111-111111111111",
        "Fix the login page",
    );
    lab.conversation(
        &tools,
        "33333333-3333-4333-8333-333333333333",
        "Add a lint script",
    );

    let out = lab.agit(&dir, &["--json", "save"]);
    let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(8), "{document}");
    let text = document.to_string();
    assert!(text.contains("the 1 session in"), "{document}");
    assert!(!text.contains("tools"), "{document}");

    let out = lab.agit(&tools, &["--json", "save"]);
    let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(8), "{document}");
    let text = document.to_string();
    assert!(text.contains("the 1 session in"), "{document}");
    assert!(text.contains("app/tools"), "{document}");
    assert!(lab.created().is_empty(), "no repository is created");
}

/// A conversation the person stopped uploading (`agit project drop`) stays on this computer when
/// an agent asks for a folder's earlier sessions or for the whole day: `agit save --all` and
/// `--today`, with no terminal, leave it out without asking anyone, say how many they left out,
/// and leave the drop in place. An implementation that let every left-out session of the folder
/// in for `--all` or `--today` would upload it.
#[test]
fn a_dropped_conversation_is_left_out_of_all_and_today() {
    use sha2::{Digest, Sha256};
    let lab = Lab::new();
    let dir = lab.folder("app", false);
    let id = "44444444-4444-4444-8444-444444444444";
    lab.conversation(&dir, id, "Draft the salary review");
    let session = format!("claude-code:{id}");
    let policies = lab.agit_home.join("projects");
    std::fs::create_dir_all(&policies).unwrap();
    let digest = hex::encode(Sha256::digest(dir.to_string_lossy().as_bytes()));
    let policy = policies.join(format!("{}.json", &digest[..24]));
    std::fs::write(
        &policy,
        serde_json::to_vec_pretty(&serde_json::json!({
            "root": dir, "repository": "me/notes",
            "identity": {"hub": lab.hub, "agent_id": "11111111-0000-4000-8000-000000000001"},
            "account": "me", "account_id": null, "enabled": true, "auto_upload": true,
            "history": "none", "excluded": [session], "last_result": null,
        }))
        .unwrap(),
    )
    .unwrap();

    for flag in ["--all", "--today"] {
        let out = lab.agit(&dir, &["--json", "save", flag]);
        let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(out.status.code(), Some(0), "{flag}: {document}");
        let value = &document["result"]["value"];
        assert_eq!(value["dropped"], 1, "{flag}: {document}");
        assert_eq!(
            value["sessions"],
            serde_json::json!([]),
            "{flag}: {document}"
        );
    }
    let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(&policy).unwrap()).unwrap();
    assert_eq!(kept["excluded"], serde_json::json!([session]), "{kept}");
    assert!(lab.created().is_empty(), "nothing is created on the Hub");
}
