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
        self.command(dir, args).output().unwrap()
    }

    /// What the SessionStart hook answers when Claude Code resumes conversation `id` in `cwd`.
    fn resume(&self, cwd: &std::path::Path, id: &str) -> serde_json::Value {
        self.session_start(cwd, id, "resume")
    }

    /// What the Stop hook tells the person when `runtime`'s conversation `id` in `cwd` ends a turn:
    /// its `systemMessage`, or nothing.
    fn stop(
        &self,
        cwd: &std::path::Path,
        id: &str,
        runtime: &str,
        transcript: &std::path::Path,
    ) -> String {
        let event = serde_json::json!({"session_id": id, "cwd": cwd, "transcript_path": transcript,
            "hook_event_name": "Stop"});
        let mut child = self
            .command(cwd, &["hooks", "settle", "--runtime", runtime])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(event.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .ok()
            .and_then(|response| response["systemMessage"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// What the SessionStart hook answers when Claude Code starts conversation `id` in `cwd`,
    /// for `source` (`startup`, `resume`, …).
    fn session_start(&self, cwd: &std::path::Path, id: &str, source: &str) -> serde_json::Value {
        let transcript = self
            .home
            .join(".claude")
            .join("projects")
            .join(agit::adapter::claude_code::slug_for(cwd))
            .join(format!("{id}.jsonl"));
        let event = serde_json::json!({"session_id": id, "cwd": cwd, "transcript_path": transcript,
            "hook_event_name": "SessionStart", "source": source});
        let mut child = self
            .command(cwd, &["hooks", "ingest", "--runtime", "claude-code"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(event.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
    }

    fn command(&self, dir: &std::path::Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
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
            // A lab is a computer where agit is set up; the not-set-up notice has tests of its own.
            .env("AGIT_SKIP_SETUP", "1")
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        command
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

    /// An OpenCode conversation of one finished turn that ran in `cwd`, in OpenCode's own
    /// database, titled with its request, with no `opencode` command on PATH: an app, an editor
    /// or a PATH set only in new shells runs it, and its database is all agit sees. Each call adds
    /// one more conversation to the same database.
    fn opencode_conversation(&self, cwd: &std::path::Path, id: &str, prompt: &str) {
        let data = self.home.join(".local/share/opencode");
        std::fs::create_dir_all(&data).unwrap();
        let database = rusqlite::Connection::open(data.join("opencode.db")).unwrap();
        database.execute_batch(
            "CREATE TABLE IF NOT EXISTS project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, directory TEXT NOT NULL, title TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, version TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);",
        ).unwrap();
        let cwd = cwd.to_str().unwrap();
        database
            .execute("INSERT OR IGNORE INTO project VALUES ('global', ?1)", [cwd])
            .unwrap();
        database
            .execute(
                "INSERT INTO session (id, project_id, parent_id, directory, title, time_created, time_updated, version) VALUES (?1, 'global', NULL, ?2, ?3, 100, 150, '1.18.13')",
                [id, cwd, prompt],
            )
            .unwrap();
        let (user, assistant) = (format!("{id}-u1"), format!("{id}-a1"));
        for (message, created, body) in [
            (
                &user,
                110,
                serde_json::json!({"role":"user", "time":{"created":110}}),
            ),
            (
                &assistant,
                120,
                serde_json::json!({"role":"assistant", "parentID":user, "finish":"stop", "time":{"created":120}}),
            ),
        ] {
            database
                .execute(
                    "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![message, id, created, body.to_string()],
                )
                .unwrap();
        }
        for (part, message, created, body) in [
            (
                format!("{user}text"),
                &user,
                111,
                serde_json::json!({"type":"text", "text":prompt}),
            ),
            (
                format!("{assistant}text"),
                &assistant,
                121,
                serde_json::json!({"type":"text", "text":"Done."}),
            ),
        ] {
            database
                .execute(
                    "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![part, message, id, created, body.to_string()],
                )
                .unwrap();
        }
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

    /// Turns `dir` on for `repository` as `agit on` records it, without asking the Hub.
    fn turn_on(&self, dir: &std::path::Path, repository: &str) {
        use sha2::{Digest, Sha256};
        let policies = self.agit_home.join("projects");
        std::fs::create_dir_all(&policies).unwrap();
        let digest = hex::encode(Sha256::digest(dir.to_string_lossy().as_bytes()));
        std::fs::write(
            policies.join(format!("{}.json", &digest[..24])),
            serde_json::to_vec_pretty(&serde_json::json!({
                "root": dir, "repository": repository,
                "identity": {"hub": self.hub, "agent_id": "11111111-0000-4000-8000-000000000001"},
                "account": "me", "account_id": null, "enabled": true, "auto_upload": true,
                "history": "none", "excluded": [], "last_result": null,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// Runs agit at a terminal in `dir`, types `answer` once the screen shows `prompt`, and
    /// returns what the terminal showed.
    #[cfg(all(unix, feature = "rc"))]
    fn agit_at_terminal(
        &self,
        dir: &std::path::Path,
        args: &[&str],
        prompt: &str,
        answer: &[u8],
    ) -> String {
        use std::time::{Duration, Instant};
        let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        command.env(
            "PATH",
            std::env::join_paths(
                std::iter::once(self.bin.clone()).chain(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                )),
            )
            .unwrap(),
        );
        for (name, value) in [
            ("HOME", self.home.as_os_str()),
            ("AGIT_HOME", self.agit_home.as_os_str()),
            ("CLAUDE_CONFIG_DIR", self.home.join(".claude").as_os_str()),
            ("CODEX_HOME", self.home.join(".codex").as_os_str()),
        ] {
            command.env(name, value);
        }
        command.env("AGIT_HUB_URL", &self.hub);
        command.env("AGIT_TELEMETRY_DISABLED", "1");
        command.env("AGIT_SKIP_SETUP", "1");
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        command.env("GIT_TERMINAL_PROMPT", "0");
        command.env("TERM", "xterm-256color");
        command.cwd(dir);
        command.args(args);
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut reader = pty.master.try_clone_reader().unwrap();
        let mut writer = pty.master.take_writer().unwrap();
        let screen = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&screen);
        let output = std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(read) = reader.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                shared.lock().unwrap().extend_from_slice(&chunk[..read]);
            }
        });
        let mut child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut answered = false;
        while child.try_wait().unwrap().is_none() {
            if !answered && String::from_utf8_lossy(&screen.lock().unwrap()).contains(prompt) {
                writer.write_all(answer).unwrap();
                writer.flush().unwrap();
                answered = true;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!(
                    "no exit within a minute: {}",
                    String::from_utf8_lossy(&screen.lock().unwrap())
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(writer);
        drop(pty.master);
        output.join().unwrap();
        String::from_utf8_lossy(&screen.lock().unwrap()).into_owned()
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
        .env("AGIT_SKIP_SETUP", "1")
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

/// A command that stopped part-way leaves a recovery note that read-only commands refuse on. The
/// step they name finishes it without uploading anything, so status answers again. A step that
/// named `agit save` would steer a person, or an agent reading the hint, into an upload, and in a
/// folder that was never on, into turning it on.
#[test]
fn the_named_step_finishes_what_a_stopped_command_left_without_uploading() {
    let lab = Lab::new();
    let dir = lab.folder("notes", false);
    let recovery = lab.agit_home.join("layout-v1-recovery");
    std::fs::create_dir_all(&recovery).unwrap();
    std::fs::write(recovery.join("pending-clone-history-1-0"), b"me/notes\n").unwrap();

    let out = lab.agit(&dir, &["status"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert!(
        stderr.contains("`agit config --list` finishes it"),
        "{stderr}"
    );
    assert!(!stderr.contains("agit save"), "{stderr}");

    let out = lab.agit(&dir, &["config", "--list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_dir(&recovery).unwrap().count(), 0);
    let out = lab.agit(&dir, &["status"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// What a running command holds is its own: a save meanwhile neither waits for it nor clears it.
#[test]
fn a_save_leaves_a_running_commands_recovery_alone() {
    let lab = Lab::new();
    let dir = lab.folder("notes", false);
    let recovery = lab.agit_home.join("layout-v1-recovery");
    std::fs::create_dir_all(&recovery).unwrap();
    let pending = recovery.join("pending-clone-history-1-0");
    std::fs::write(&pending, b"me/notes\n").unwrap();
    let lock = std::fs::File::create(lab.agit_home.join("layout-v1.lock")).unwrap();
    lock.lock_shared().unwrap();

    let out = lab.agit(&dir, &["save"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(pending.exists());
}

/// What a folder does with a conversation is said the same way by a conversation's start, the
/// overview and `agit on`. A folder that is on while its repository's automatic upload is off on
/// this computer saves turns here and uploads nothing, so its start says the conversation stays on
/// this computer, the overview names the hold, and `agit on --yes` switches the repository back
/// on, the off recorded for it before its checkout existed included, after which the start says
/// it is uploaded; without a terminal or `--yes`, `agit on` names the hold and promises no upload.
/// A folder set up for a repository without saving by itself says at a conversation's start, once
/// per folder, that it isn't being saved there. A folder turned off says nothing at a
/// conversation's start. A folder that uploads by itself says, once OpenCode is installed, that
/// OpenCode's conversations go up only when its agent commits or the person saves, not at each
/// turn's end; the overview, `agit status`, `agit doctor` and `agit on` say it only there, never
/// in a folder that is held, only set up for a repository, or off. A surface that read only the
/// folder's own switch would promise uploads that never happen; an `agit on` that moved only the
/// checkout's setting would leave the recorded off in force while saying it is back on; one that
/// repeated itself at every start, or spoke in a folder the person turned off, would be noise.
#[test]
fn every_surface_says_what_a_folder_does_with_its_conversations() {
    let lab = Lab::new();
    let value = |at: &std::path::Path, args: &[&str]| {
        let out = lab.agit(at, args);
        let document: serde_json::Value =
            serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{args:?}: {out:?}"));
        document["result"]["value"].clone()
    };
    let said_at_start = |cwd: &std::path::Path, id: &str| {
        lab.resume(cwd, id)["systemMessage"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    let opencode = |installed: bool| {
        for (file, script) in [
            ("opencode", "#!/bin/sh\nexit 0\n"),
            ("opencode.cmd", "@exit /b 0\r\n"),
        ] {
            if installed {
                std::fs::write(lab.bin.join(file), script).unwrap();
            } else {
                std::fs::remove_file(lab.bin.join(file)).unwrap();
            }
        }
    };
    // Whether the overview, `agit status` and `agit doctor` in `at` say that OpenCode's
    // conversations don't go up at each turn's end.
    let says_opencode = |at: &std::path::Path| {
        let tail = "not at the end of each turn";
        [
            value(at, &["--json"])["opencode_not_captured"]
                .as_str()
                .is_some_and(|said| said.contains(tail)),
            String::from_utf8_lossy(&lab.agit(at, &["status"]).stdout).contains(tail),
            String::from_utf8_lossy(&lab.agit(at, &["doctor"]).stdout).contains(tail),
        ]
    };

    // On, with the repository's automatic upload off on this computer.
    let dir = lab.folder("solo", false);
    // `agit on` records the folder's binding and its project.
    lab.bind(&dir, "me/solo");
    lab.turn_on(&dir, "me/solo");
    // Turned off for the repository before its checkout existed, which outranks the checkout's
    // own setting, and off in the checkout as well.
    let recorded = lab.agit(&dir, &["config", "--repo", "me/solo", "push.auto", "false"]);
    assert!(recorded.status.success(), "{recorded:?}");
    let checkout = lab.agit_home.join("repos/me/solo");
    std::fs::create_dir_all(&checkout).unwrap();
    for args in [&["init", "-q"][..], &["config", "agit.autoPush", "false"]] {
        let out = Command::new("git")
            .args(args)
            .current_dir(&checkout)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &lab.home)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }
    lab.conversation(
        &dir,
        "21212121-2121-4212-8212-212121212121",
        "Fix the parser",
    );
    let said = said_at_start(&dir, "21212121-2121-4212-8212-212121212121");
    assert!(
        said.contains("this conversation is saved on this computer for me/solo")
            && said.contains("Automatic upload is off")
            && !said.contains("is uploaded to"),
        "{said}"
    );
    let overview = value(&dir, &["--json"]);
    assert_eq!(overview["held"]["code"], "push_auto_off", "{overview}");
    // A held folder uploads nothing when OpenCode's agent commits, so nothing says it does.
    opencode(true);
    assert_eq!(says_opencode(&dir), [false; 3]);
    // Without a terminal or --yes, `agit on` names the hold and promises no upload.
    let unattended = lab.agit(&dir, &["on"]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&unattended.stdout),
        String::from_utf8_lossy(&unattended.stderr)
    );
    assert!(
        said.contains("automatic upload to me/solo is off on this computer")
            && !said.contains("sessions here upload to")
            && !said.contains("not at the end of each turn"),
        "{said}"
    );
    let on = value(&dir, &["--json", "--yes", "on"]);
    assert_eq!(on["resumed"], "push_auto_off", "{on}");
    assert!(
        on["runtimes"].as_array().is_some_and(|runtimes| runtimes
            .iter()
            .any(|runtime| runtime["runtime"] == "opencode" && runtime["state"] == "not_captured")),
        "{on}"
    );
    assert_eq!(on["held"], serde_json::Value::Null, "{on}");
    assert_eq!(value(&dir, &["--json"])["held"], serde_json::Value::Null);
    let said = said_at_start(&dir, "21212121-2121-4212-8212-212121212121");
    assert!(said.contains("is uploaded to me/solo "), "{said}");
    // An installed OpenCode tells agit nothing when a turn ends, so a folder that uploads by
    // itself says that OpenCode's conversations aren't, where the person looks at the folder.
    let overview = value(&dir, &["--json"]);
    let said = overview["opencode_not_captured"]
        .as_str()
        .unwrap_or_default();
    assert!(
        said.contains("its agent runs `agit commit`")
            && said.contains("`agit save`")
            && said.contains("not at the end of each turn"),
        "{overview}"
    );
    assert_eq!(says_opencode(&dir), [true; 3]);
    opencode(false);
    assert_eq!(says_opencode(&dir), [false; 3]);
    opencode(true);

    // Set up for a repository, without saving by itself: told once.
    let bound = lab.folder("bound", false);
    lab.bind(&bound, "me/bound");
    for id in [
        "22222222-2222-4222-8222-222222222222",
        "24242424-2424-4242-8242-242424242424",
    ] {
        lab.conversation(&bound, id, "Plan the release");
    }
    let said = said_at_start(&bound, "22222222-2222-4222-8222-222222222222");
    assert!(
        said.contains("this conversation isn't being saved to me/bound")
            && said.contains("`agit save`")
            && said.contains("`agit on` saves this folder's conversations from then on"),
        "{said}"
    );
    let again = said_at_start(&bound, "24242424-2424-4242-8242-242424242424");
    assert!(!again.contains("isn't being saved"), "{again}");
    assert_eq!(says_opencode(&bound), [false; 3]);

    // Turned off.
    let off = lab.agit(&dir, &["off"]);
    assert!(off.status.success(), "{off:?}");
    assert_eq!(says_opencode(&dir), [false; 3]);
    lab.conversation(
        &dir,
        "23232323-2323-4232-8232-232323232323",
        "Sketch the docs",
    );
    assert_eq!(
        lab.resume(&dir, "23232323-2323-4232-8232-232323232323"),
        serde_json::Value::Null
    );
}

/// A folder that is on covers the git worktrees of its checkout (#162), from when it covers each
/// one. Its overview names each worktree with the sessions there from before; in a worktree, the
/// overview and `agit status` name the folder whose setting it follows and count those sessions
/// with `agit save --all`, and a resume of one of those sessions tells the person it is not
/// uploaded and hands them a save that takes it alone; at a terminal `agit save --today` counts a worktree's sessions on a line of its own
/// before it asks; `agit off` there stops only the worktree. An implementation that
/// judged by the folder's path alone says nothing in the worktree and saves nothing there; one
/// that took the worktree's earlier sessions along uploads them unasked; one whose `agit off`
/// reached the folder turns the folder off too.
/// A conversation another session continues says so at its turn's end, and never also that
/// nothing saves its folder, though no agit setting covers that folder: its claim still holds a
/// branch, and only a conversation nothing claims is told about the folder. A conversation nothing
/// claims, in another such folder, is told, so the first folder's silence isn't silence for any
/// folder. Telling a superseded conversation both would turn its one true line into two.
#[test]
fn a_superseded_conversation_names_where_it_continues_never_its_unsaved_folder() {
    const UNSAVED: &str = "agit isn't saving this folder's conversations";
    let lab = Lab::new();
    let store = agit::domain::store::Store::at(lab.agit_home.join("store"));
    let old = lab.folder("old-window", false);
    let id = "21212121-2121-4212-8212-212121212121";
    lab.conversation(&old, id, "Plan the move");
    let mut claim = agit::domain::link::Link::new("claude-code", id, Some(&old));
    claim.owner = Some("me".into());
    claim.agent = Some("notes".into());
    claim.branch = Some("work".into());
    claim.superseded_by = Some("claude-code/31313131-3131-4313-8313-313131313131".into());
    agit::domain::link::write(&store, &claim).unwrap();
    let transcript = lab
        .home
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&old))
        .join(format!("{id}.jsonl"));
    let said = lab.stop(&old, id, "claude-code", &transcript);
    assert!(
        said.contains("agit: this conversation continues as another session (31313131)"),
        "{said}"
    );
    assert!(!said.contains(UNSAVED), "{said}");

    let plain = lab.folder("plain-window", false);
    let fresh = "41414141-4141-4141-8141-414141414141";
    lab.conversation(&plain, fresh, "Sketch it");
    let transcript = lab
        .home
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&plain))
        .join(format!("{fresh}.jsonl"));
    let said = lab.stop(&plain, fresh, "claude-code", &transcript);
    assert!(said.contains(UNSAVED), "{said}");
}

/// A Codex conversation another session continues is told so at its turn's end together with
/// whatever its settlement refused, in one message. A refusal held back because another line was
/// already there would leave the person not knowing the turn went nowhere.
#[test]
fn a_superseded_codex_turn_end_says_where_it_continues_and_what_was_refused() {
    let lab = Lab::new();
    let store = agit::domain::store::Store::at(lab.agit_home.join("store"));
    let dir = lab.folder("codex-window", false);
    let id = "51515151-5151-4515-8515-515151515151";
    let mut claim = agit::domain::link::Link::new("codex", id, Some(&dir));
    claim.owner = Some("me".into());
    claim.agent = Some("notes".into());
    claim.branch = Some("work".into());
    claim.superseded_by = Some("codex/61616161-6161-4616-8616-616161616161".into());
    agit::domain::link::write(&store, &claim).unwrap();
    let said = lab.stop(&dir, id, "codex", &dir.join("rollout.jsonl"));
    let lines: Vec<&str> = said.lines().collect();
    assert!(
        lines.contains(&"agit: this conversation continues as another session (61616161), so its turns here stay on this computer and aren't uploaded. To continue me/notes@work: `agit resume me/notes@work`."),
        "{said}"
    );
    assert!(
        lines.iter().any(
            |line| line.starts_with("agit: ") && !line.contains("continues as another session")
        ),
        "the refused settlement is told too: {said}"
    );
}

/// A conversation starting in a folder nothing saves tells the person once that agit isn't
/// saving it, and the command that turns it on; the agent's context never carries it. The next
/// start there says nothing, nor does one inside agit's own home, nor any once `notice.unsaved` is
/// false. A line that repeated every session would be noise; one in the agent's context would get
/// uploads turned on by the agent.
#[test]
fn a_conversation_in_a_folder_nothing_saves_tells_the_person_once() {
    const LINE: &str = "agit isn't saving this folder's conversations; `agit on` here saves them from now on. Said once per folder; `agit config notice.unsaved false` stops it everywhere.";
    let lab = Lab::new();
    let plain = lab.folder("plain", false);
    let first = lab.session_start(&plain, "17171717-1717-4171-8171-171717171717", "startup");
    assert!(
        first["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains(LINE)),
        "{first}"
    );
    assert!(
        !first["hookSpecificOutput"]
            .to_string()
            .contains("isn't saving"),
        "{first}"
    );
    let again = lab.session_start(&plain, "18181818-1818-4181-8181-181818181818", "startup");
    assert!(!again.to_string().contains("isn't saving"), "{again}");

    let own = lab.agit_home.join("repos/me/notes");
    std::fs::create_dir_all(&own).unwrap();
    let own = lab.session_start(&own, "19191919-1919-4191-8191-191919191919", "startup");
    assert!(!own.to_string().contains("isn't saving"), "{own}");

    let other = lab.folder("other", false);
    let set = lab.agit(&other, &["config", "notice.unsaved", "false"]);
    assert!(set.status.success(), "{set:?}");
    let quiet = lab.session_start(&other, "20202020-2020-4202-8202-202020202020", "startup");
    assert!(!quiet.to_string().contains("isn't saving"), "{quiet}");
}

/// Sessions from before a folder covered them are the person's to upload. Without a terminal,
/// where an agent reads the output, no line and no JSON field offers `agit save --all` as a step
/// to take: each says to ask the person, in `agit on`, `agit save`, the overview and status. A
/// hint that handed an agent the command would publish sessions nobody chose.
#[test]
fn without_a_terminal_no_output_hands_an_agent_save_all_for_earlier_sessions() {
    let lab = Lab::new();
    let dir = lab.folder("notes", false);
    let git = |at: &std::path::Path, args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(at)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &lab.home)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    };
    git(&dir, &["init", "-q"]);
    git(&dir, &["commit", "-q", "--allow-empty", "-m", "start"]);
    git(&dir, &["worktree", "add", "-q", "../notes-tree"]);
    let tree = dir
        .parent()
        .unwrap()
        .join("notes-tree")
        .canonicalize()
        .unwrap();
    lab.turn_on(&dir, "me/notes");
    lab.conversation(
        &tree,
        "15151515-1515-4151-8151-151515151515",
        "Try the adapter idea",
    );
    lab.conversation(
        &tree,
        "16161616-1616-4161-8161-161616161616",
        "Sketch the parser",
    );
    fn strings<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
        match value {
            serde_json::Value::String(text) => out.push(text),
            serde_json::Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
            serde_json::Value::Object(fields) => {
                fields.values().for_each(|item| strings(item, out))
            }
            _ => {}
        }
    }
    let mut asked = 0;
    for (at, args) in [
        (&tree, &["on"][..]),
        (&tree, &["--json", "on"]),
        (&tree, &["save"]),
        (&tree, &[]),
        (&tree, &["--json"]),
        (&tree, &["status"]),
        (&tree, &["status", "--json"]),
        (&dir, &[]),
    ] {
        let out = lab.agit(at, args);
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let mut texts: Vec<&str> = said.lines().collect();
        let document = serde_json::from_slice::<serde_json::Value>(&out.stdout).ok();
        if let Some(document) = &document {
            strings(document, &mut texts);
        }
        for text in texts.iter().filter(|text| text.contains("agit save --all")) {
            assert!(text.contains("ask the person"), "{args:?}: {text}");
            asked += 1;
        }
    }
    assert!(asked > 0, "no output mentioned the sessions from before");
}

#[test]
fn a_folder_covers_its_git_worktrees_from_now_on() {
    let lab = Lab::new();
    let dir = lab.folder("notes", false);
    let git = |at: &std::path::Path, args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(at)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &lab.home)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    };
    git(&dir, &["init", "-q"]);
    git(&dir, &["commit", "-q", "--allow-empty", "-m", "start"]);
    git(&dir, &["worktree", "add", "-q", "../notes-tree"]);
    let tree = dir
        .parent()
        .unwrap()
        .join("notes-tree")
        .canonicalize()
        .unwrap();
    lab.turn_on(&dir, "me/notes");
    lab.conversation(
        &tree,
        "14141414-1414-4141-8141-141414141414",
        "Try the adapter idea",
    );
    let value = |at: &std::path::Path, args: &[&str]| {
        let out = lab.agit(at, args);
        let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        document["result"]["value"].clone()
    };

    let main = value(&dir, &["--json"]);
    assert_eq!(main["state"], "on", "{main}");
    assert_eq!(
        main["worktrees"],
        serde_json::json!([{"folder": tree, "earlier_sessions_not_uploaded": 1}]),
        "{main}"
    );

    let here = value(&tree, &["--json"]);
    assert_eq!(here["state"], "on", "{here}");
    assert_eq!(here["through"], serde_json::json!(dir), "{here}");
    assert_eq!(here["repository"], "me/notes", "{here}");
    assert_eq!(here["earlier_sessions_not_uploaded"], 1, "{here}");
    assert!(
        here["next"]["command"].is_null()
            && here["next"]["ask_user"]
                .as_str()
                .is_some_and(|ask| ask.contains("ask the person")),
        "{here}"
    );
    // `agit remote` lists the remotes of the repository the folder saves to, from the worktree
    // too; this lab has no checkout of it, so it names the repository and how to get it. One that
    // fell back to AGIT_SESSION alone could not tell which repository a folder means.
    let remote = lab.agit(&tree, &["remote"]);
    assert_eq!(remote.status.code(), Some(4), "{remote:?}");
    let said = String::from_utf8_lossy(&remote.stderr);
    assert!(
        said.contains("me/notes") && said.contains("agit clone me/notes"),
        "{said}"
    );
    let status = value(&tree, &["status", "--json"]);
    let said = status["worktree"].as_str().unwrap_or_default();
    assert!(
        said.contains("notes-tree is a git worktree of")
            && said.contains("which uploads new sessions to me/notes"),
        "{status}"
    );
    assert_eq!(
        status["earlier_sessions_not_uploaded"],
        serde_json::json!({"count": 1, "ask_user": "1 session that ran here before isn't uploaded; ask the person whether to upload it (`agit save --all`)"}),
        "{status}"
    );
    // Resumed, that conversation tells the person, and only the person, that it is not uploaded
    // and how to upload it alone. A resume that said nothing reads as being saved, and any field
    // beside the person's message would reach the model.
    let resumed = lab.resume(&tree, "14141414-1414-4141-8141-141414141414");
    assert_eq!(
        resumed
            .as_object()
            .map(|fields| fields.keys().cloned().collect::<Vec<_>>()),
        Some(vec!["systemMessage".to_owned()]),
        "{resumed}"
    );
    let said = resumed["systemMessage"].as_str().unwrap_or_default();
    assert!(
        said.contains("before agit covered it as a git worktree of")
            && said.contains("so it is not uploaded. To upload it to me/notes")
            && said.contains("`agit save 14141414` in"),
        "{said}"
    );

    // At a terminal `agit save --today` names the worktree and how many of its sessions would go
    // before it asks once; no uploads nothing.
    #[cfg(all(unix, feature = "rc"))]
    {
        let screen = lab.agit_at_terminal(&dir, &["save", "--today"], "Upload these", b"n\r");
        assert!(
            screen
                .lines()
                .any(|line| line.contains("notes-tree") && line.contains("1 session(s)")),
            "{screen}"
        );
        assert!(screen.contains("nothing was uploaded"), "{screen}");
        assert!(lab.created().is_empty(), "nothing is created on the Hub");
    }

    // The command that line hands the person, run in the folder as printed, takes that
    // conversation alone and does not ask: another earlier conversation of the worktree stays out.
    // A short id that resolved to nothing, or to the other conversation, fails here. The lab's Hub
    // accepts no upload, so what is pinned is which conversation the save takes.
    lab.conversation(
        &tree,
        "15151515-1515-4151-8151-151515151515",
        "Sketch another idea",
    );
    let out = lab.agit(&dir, &["--json", "save", "14141414"]);
    let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_ne!(out.status.code(), Some(8), "{document}");
    let taken: Vec<&str> = document["result"]["value"]["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|session| session["session_id"].as_str())
        .collect();
    assert_eq!(
        taken,
        ["14141414-1414-4141-8141-141414141414"],
        "{document}"
    );

    let off = value(&tree, &["--json", "off"]);
    assert_eq!(off["through"], serde_json::json!(dir), "{off}");
    assert_eq!(value(&tree, &["--json"])["state"], "off");
    assert_eq!(value(&dir, &["--json"])["state"], "on");

    // Turned off, as after the person says no once a save turned it on, the folder stops its
    // worktrees with it: one that followed it reads as off, and even `agit save --all` there
    // uploads nothing and says which folder is off.
    git(&dir, &["worktree", "add", "-q", "../notes-second"]);
    let second = dir
        .parent()
        .unwrap()
        .join("notes-second")
        .canonicalize()
        .unwrap();
    lab.conversation(
        &second,
        "16161616-1616-4161-8161-161616161616",
        "Sketch the parser",
    );
    assert_eq!(value(&second, &["--json"])["state"], "on");
    let off = value(&dir, &["--json", "off"]);
    assert_eq!(off["on"], false, "{off}");
    assert_eq!(value(&second, &["--json"])["state"], "off");
    let out = lab.agit(&second, &["--json", "save", "--all"]);
    let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(4), "{document}");
    assert!(
        document.to_string().contains("is a git worktree of"),
        "{document}"
    );
    assert!(lab.created().is_empty(), "nothing is created on the Hub");
}

/// A conversation already saved in another repository is said to be saved there, in a plain line,
/// not reported as an upload that failed, and saving again isn't offered: it changes nothing. Shown
/// as an error with a retry, it reads as agit broken, and the retry meets the same answer.
#[test]
fn a_conversation_saved_elsewhere_is_said_so_and_no_retry_is_offered() {
    let lab = Lab::new();
    let dir = lab.folder("notes", false);
    lab.turn_on(&dir, "me/notes");
    let id = "17171717-1717-4171-8171-171717171717";
    lab.conversation(&dir, id, "Plan the move");
    let store = agit::domain::store::Store::at(lab.agit_home.join("store"));
    let mut claim = agit::domain::link::Link::new("claude-code", id, Some(&dir));
    claim.owner = Some("other".into());
    claim.agent = Some("place".into());
    claim.branch = Some("work".into());
    agit::domain::link::write(&store, &claim).unwrap();

    let out = lab.agit(&dir, &["save", id]);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(
        stdout.contains(
            "\"Plan the move\" is already saved in other/place@work, so it isn't uploaded here"
        ),
        "{stdout}{stderr}"
    );
    assert!(!stderr.contains("could not upload"), "{stderr}");
    assert!(!stderr.contains("again to retry"), "{stderr}");

    let out = lab.agit(&dir, &["--json", "save", id]);
    let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let value = &document["result"]["value"];
    assert_eq!(value["sessions"][0]["status"], "elsewhere", "{document}");
    assert_eq!(
        value["sessions"][0]["saved_in"], "other/place@work",
        "{document}"
    );
    assert!(value["next"].is_null(), "{document}");
    assert!(lab.created().is_empty(), "nothing is created on the Hub");
}

/// `agit save` finds an OpenCode conversation in a folder that is on, though OpenCode tells agit
/// nothing when a turn ends and no `opencode` command is on PATH, and keeps the folder's earlier
/// history out as it does for the other runtimes. A save that listed only runtimes whose command
/// it can find on PATH leaves the conversation out of every save, even one that names it. The folder was bound without earlier history by a policy that never recorded OpenCode's
/// conversations, so the one already there counts as earlier: a plain save leaves it out and says
/// so, and naming it or `--all` takes it, as an OpenCode session. The lab's Hub accepts no upload,
/// so what is pinned is which conversation each save takes. A save that looked only at the
/// runtimes a folder uploads by itself finds nothing to name; one that did not count OpenCode as
/// earlier takes it in the plain save.
#[test]
fn a_save_finds_an_opencode_conversation_and_keeps_earlier_history_out() {
    let lab = Lab::new();
    let dir = lab.folder("notes", false);
    lab.turn_on(&dir, "me/notes");
    lab.opencode_conversation(&dir, "ses_save_opencode", "Plan the release");
    let save = |args: &[&str]| {
        let out = lab.agit(&dir, args);
        let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        document["result"]["value"].clone()
    };
    let taken = |value: &serde_json::Value| -> Vec<(String, String)> {
        value["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|session| {
                Some((
                    session["runtime"].as_str()?.to_owned(),
                    session["session_id"].as_str()?.to_owned(),
                ))
            })
            .collect()
    };

    let plain = save(&["--json", "save"]);
    assert!(taken(&plain).is_empty(), "{plain}");
    assert_eq!(plain["left_out"], 1, "{plain}");
    for args in [
        &["--json", "save", "ses_save_opencode"][..],
        &["--json", "save", "--all"],
    ] {
        let value = save(args);
        assert_eq!(
            taken(&value),
            [("opencode".to_owned(), "ses_save_opencode".to_owned())],
            "{args:?}: {value}"
        );
    }
}

/// A folder bound before agit recorded OpenCode's earlier conversations counts every OpenCode
/// conversation there as earlier until an explicit `agit save` or `agit on` looks at OpenCode's
/// store once. The background capture leaves such a policy as it is. The save that looks keeps
/// the two conversations from before out, the one in a git worktree it covers outside the folder
/// included, and says how many; one started after it is the folder's own. Names match OpenCode's
/// own titles. A save that never looked keeps the later conversation out too; a background path
/// that looked would decide in the person's absence; a look that counted only the folder itself
/// would let the worktree's earlier conversation into the next plain save.
#[test]
fn an_explicit_save_brings_an_old_policy_forward_so_later_opencode_conversations_are_the_folders() {
    let lab = Lab::new();
    let dir = lab.folder("notes", false);
    // One of the conversations from before ran in a git worktree the folder covers, outside it.
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &lab.home)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "start"]);
    git(&["worktree", "add", "-q", "../notes-tree"]);
    let tree = dir
        .parent()
        .unwrap()
        .join("notes-tree")
        .canonicalize()
        .unwrap();
    lab.turn_on(&dir, "me/notes");
    lab.opencode_conversation(&dir, "ses_before_one", "Plan the release");
    lab.opencode_conversation(&tree, "ses_before_two", "Write the notes");
    let looked = || -> bool {
        let policies: Vec<_> = std::fs::read_dir(lab.agit_home.join("projects"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
            .collect();
        assert_eq!(policies.len(), 1);
        let policy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(policies[0].path()).unwrap()).unwrap();
        policy["earlier_opencode"] == true
    };
    let background = lab.agit(
        &dir,
        &["--quiet", "project", "capture", dir.to_str().unwrap()],
    );
    assert!(!looked(), "{background:?}");

    let first = lab.agit(&dir, &["save"]);
    let said = String::from_utf8_lossy(&first.stdout);
    assert!(
        said.contains(
            "2 OpenCode conversations here from before now count as earlier; `agit save <id>` saves one"
        ),
        "{said}{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(looked());

    lab.opencode_conversation(&dir, "ses_after", "Ship the release");
    let save = |args: &[&str]| {
        let out = lab.agit(&dir, args);
        let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        document["result"]["value"].clone()
    };
    let taken = |value: &serde_json::Value| -> Vec<String> {
        value["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|session| Some(session["session_id"].as_str()?.to_owned()))
            .collect()
    };
    let plain = save(&["--json", "save"]);
    assert_eq!(taken(&plain), ["ses_after"], "{plain}");
    let named = save(&["--json", "save", "Write the notes"]);
    assert_eq!(taken(&named), ["ses_before_two"], "{named}");
}
