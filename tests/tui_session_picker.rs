#![cfg(unix)]

#[path = "support/terminal_screen.rs"]
mod terminal_screen;

use agit::domain::{meta, repo::Repo, storage, transcript};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: std::sync::mpsc::Receiver<Vec<u8>>,
    captured: Vec<u8>,
    columns: usize,
    checkpoint: Option<(usize, String)>,
    csi: regex::Regex,
}

impl Terminal {
    fn start(home: &std::path::Path, cwd: &std::path::Path, command: &str, columns: u16) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
        if !command.is_empty() {
            builder.arg(command);
        }
        builder.cwd(cwd);
        builder.env_clear();
        let runtime_home = home.join("test-runtime-home");
        std::fs::create_dir_all(&runtime_home).unwrap();
        let bin = runtime_home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        builder.env(
            "PATH",
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            )))
            .unwrap(),
        );
        builder.env("HOME", &runtime_home);
        builder.env("CODEX_HOME", runtime_home.join(".codex"));
        builder.env("AGIT_HOME", home);
        builder.env("AGIT_HUB_URL", "http://127.0.0.1:1");
        builder.env("TERM", "xterm-256color");
        builder.env("GIT_CONFIG_NOSYSTEM", "1");
        builder.env("GIT_CONFIG_GLOBAL", "/dev/null");
        builder.env("GIT_AUTHOR_NAME", "TUI fixture");
        builder.env("GIT_AUTHOR_EMAIL", "tui@example.test");
        builder.env("GIT_COMMITTER_NAME", "TUI fixture");
        builder.env("GIT_COMMITTER_EMAIL", "tui@example.test");
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let child = pair.slave.spawn_command(builder).unwrap();
        drop(pair.slave);
        let (sender, output) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _master = pair.master;
            let mut buffer = [0; 8192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            writer,
            output,
            captured: Vec::new(),
            columns: usize::from(columns),
            checkpoint: None,
            csi: regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap(),
        }
    }

    fn screen(&self) -> String {
        terminal_screen::screen_text(
            &String::from_utf8_lossy(&self.captured),
            &self.csi,
            24,
            self.columns,
        )
    }

    fn append(&mut self, chunk: &[u8]) {
        assert!(self.captured.len() + chunk.len() <= 8 * 1024 * 1024);
        self.captured.extend_from_slice(chunk);
    }

    fn checkpoint(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Ok(chunk) = self.output.try_recv() {
            assert!(
                Instant::now() < deadline,
                "terminal did not reach an output checkpoint"
            );
            self.append(&chunk);
        }
        // A new interaction retains the cells needed by a differential redraw, but an
        // unchanged hint in those cells cannot prove that the requested action completed.
        self.checkpoint = Some((self.captured.len(), self.screen()));
    }

    fn wait_for(&mut self, text: &str) {
        assert!(
            !text.contains('\u{1b}'),
            "control sequences need a raw-byte assertion"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = self.screen();
            assert!(
                Instant::now() < deadline,
                "waiting for {text:?}: deadline elapsed; screen: {screen}"
            );
            let (after, previous) = self
                .checkpoint
                .as_ref()
                .map_or((0, None), |(after, screen)| (*after, Some(screen.as_str())));
            if self.captured.len() > after
                && terminal_screen::changed_text_present(&screen, previous, text)
            {
                return;
            }
            let chunk = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "waiting for {text:?}: {error}; screen: {screen}; output: {}",
                        String::from_utf8_lossy(&self.captured)
                    )
                });
            self.append(&chunk);
        }
    }

    fn wait_for_control(&mut self, sequence: &[u8]) {
        assert!(!sequence.is_empty());
        let deadline = Instant::now() + Duration::from_secs(10);
        let after = self.checkpoint.as_ref().map_or(0, |(after, _)| *after);
        while !terminal_screen::control_present_after(&self.captured, after, sequence) {
            assert!(
                Instant::now() < deadline,
                "waiting for terminal control {sequence:?}: deadline elapsed"
            );
            let chunk = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!("waiting for terminal control {sequence:?}: {error}")
                });
            self.append(&chunk);
        }
    }

    fn type_keys(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture(home: &std::path::Path) -> Repo {
    let repo = Repo::init(&home.join("repos/me/fixture")).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    repo.add_all().unwrap();
    repo.commit("fixture shared files").unwrap();
    repo.git(&["branch", "-m", "main"]).unwrap();
    repo.git(&["switch", "-c", "work"]).unwrap();
    let id = format!("agit-{}", "a".repeat(40));
    let raw = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"fixture turn\"}}\n";
    let envelope = transcript::wrap_lines(raw, "claude-code", &id);
    storage::write_snapshot(repo.root(), &envelope, &envelope).unwrap();
    meta::write(
        repo.root(),
        &meta::Meta::new(id, "claude-code".into(), "/work".into()),
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("fixture conversation").unwrap();
    repo
}

const NATIVE_ID: &str = "abababab-0000-4000-8000-000000000001";

fn native_fixture(home: &std::path::Path, cwd: &std::path::Path, adopted: bool) {
    let project = home
        .join("test-runtime-home/.claude/projects")
        .join(agit::adapter::claude_code::slug_for(cwd));
    std::fs::create_dir_all(&project).unwrap();
    let path = project.join(format!("{NATIVE_ID}.jsonl"));
    std::fs::write(
        &path,
        serde_json::json!({
            "type": "user", "sessionId": NATIVE_ID, "cwd": cwd,
            "message": {"role": "user", "content": "A native conversation awaiting a name"}
        })
        .to_string()
            + "\n",
    )
    .unwrap();
    std::fs::File::open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        .unwrap();
    if adopted {
        let mut link = agit::domain::link::Link::new("claude-code", NATIVE_ID, Some(cwd));
        link.owner = Some("me".into());
        link.agent = Some("fixture".into());
        link.branch = Some("work".into());
        agit::domain::link::write(&agit::domain::store::Store::at(home.join("store")), &link)
            .unwrap();
    }
}

#[test]
fn bare_config_edits_and_unsets_the_persisted_value() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("agit");
    let mut terminal = Terminal::start(&home, tmp.path(), "config", 60);
    terminal.wait_for("configuration");
    terminal.type_keys("j\rcodex\r");
    terminal.wait_for("saved");
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("config.json")).unwrap()).unwrap();
    assert_eq!(saved["runtime.default"], "codex");
    terminal.checkpoint();
    terminal.type_keys("u");
    terminal.wait_for("unset runtime.default");
    terminal.type_keys("q");
    assert!(terminal.child.wait().unwrap().success());
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("config.json")).unwrap()).unwrap();
    assert!(saved.get("runtime.default").is_none());
}

#[test]
fn bare_init_creates_only_the_named_repo_with_binding_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("agit");
    let mut terminal = Terminal::start(&home, tmp.path(), "init", 60);
    terminal.wait_for("agit init");
    terminal.type_keys("\rtui-created\r \t\t\r");
    terminal.wait_for("local/tui-created");
    assert!(terminal.child.wait().unwrap().success());
    let repo = Repo::open(home.join("repos/local/tui-created")).unwrap();
    assert_eq!(meta::line_at_ref(&repo, "main"), Some(meta::Line::File));
    assert!(!home.join("workspaces").exists());
}

#[test]
fn bare_resume_and_bare_agit_show_the_same_adopted_session() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    let home = tmp.path().join("agit");
    fixture(&home);
    native_fixture(&home, &cwd, true);
    for command in ["resume", ""] {
        let mut terminal = Terminal::start(&home, &cwd, command, 100);
        terminal.wait_for("enter continue");
        terminal.wait_for("me/fixture");
        terminal.wait_for("work");
        terminal.type_keys("q");
        assert!(terminal.child.wait().unwrap().success());
    }
}

#[test]
fn bare_import_selects_a_native_session_and_can_register_it_without_login() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    let home = tmp.path().join("agit");
    native_fixture(&home, &cwd, false);
    let mut terminal = Terminal::start(&home, &cwd, "import", 60);
    terminal.wait_for("agit import");
    terminal.type_keys("l");
    terminal.wait_for("A native conversation");
    terminal.type_keys("\r\r");
    assert!(terminal.child.wait().unwrap().success());
    let link = agit::domain::link::read(
        &home
            .join("store/claude-code")
            .join(format!("{NATIVE_ID}.json")),
    )
    .unwrap();
    assert_eq!(link.cwd.as_deref(), cwd.to_str());
    assert!(link.agent.is_none());
    assert!(!home.join("repos").exists());
}

#[test]
fn import_pages_the_filtered_candidates_without_adopting_an_implicit_identity() {
    for scenario in ["forward", "back", "filtered"] {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().canonicalize().unwrap();
        let home = tmp.path().join("agit");
        let project = home
            .join("test-runtime-home/.claude/projects")
            .join(agit::adapter::claude_code::slug_for(&cwd));
        std::fs::create_dir_all(&project).unwrap();
        let mut sources = Vec::new();
        for index in 0..30 {
            let id = format!("acacacac-0000-4000-8000-{index:012}");
            let path = project.join(format!("{id}.jsonl"));
            let bytes = (serde_json::json!({
                "type": "user", "sessionId": id, "cwd": cwd,
                "message": {"role": "user", "content": format!("Page candidate {index:02}")}
            })
            .to_string()
                + "\n")
                .into_bytes();
            std::fs::write(&path, &bytes).unwrap();
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_030 - index),
                )
                .unwrap();
            sources.push((id, path, bytes));
        }
        let mut terminal = Terminal::start(&home, &cwd, "import", 60);
        terminal.wait_for("agit import");
        terminal.type_keys("l");
        terminal.wait_for("Page candidate 00");
        match scenario {
            "forward" => terminal.type_keys("\u{1b}[6~"),
            "back" => terminal.type_keys("\u{1b}[6~\u{1b}[5~"),
            "filtered" => terminal.type_keys(&format!("/{}\r\u{1b}[6~", sources[29].0)),
            _ => unreachable!(),
        }
        terminal.type_keys("\r\r");
        assert!(terminal.child.wait().unwrap().success());
        let links: Vec<_> = std::fs::read_dir(home.join("store/claude-code"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .map(|path| agit::domain::link::read(&path).unwrap())
            .collect();
        assert_eq!(links.len(), 1);
        let selected = sources
            .iter()
            .position(|(id, _, _)| *id == links[0].session_id)
            .unwrap();
        match scenario {
            "forward" => assert!((2..29).contains(&selected), "selected {selected}"),
            "back" => assert_eq!(selected, 0),
            "filtered" => assert_eq!(selected, 29),
            _ => unreachable!(),
        }
        assert_eq!(links[0].cwd.as_deref(), cwd.to_str());
        assert!(links[0].agent.is_none());
        assert!(!home.join("repos").exists());
        assert!(!home.join("workspaces").exists());
        for (_, path, bytes) in sources {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }
}

#[test]
fn runtime_and_project_choices_keep_import_identity_explicit_and_tab_stages_intact() {
    for scenario in ["other-project", "runtime", "cancel"] {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().canonicalize().unwrap();
        let other = cwd.join("another-project");
        std::fs::create_dir_all(&other).unwrap();
        let home = tmp.path().join("agit");
        let runtime_home = home.join("test-runtime-home");
        let mut sources = Vec::new();
        for (runtime, id, project, prompt) in [
            (
                "claude-code",
                "dededede-0000-4000-8000-000000000001",
                &cwd,
                "Claude current project",
            ),
            (
                "claude-code",
                "dededede-0000-4000-8000-000000000002",
                &other,
                "Claude other project",
            ),
            (
                "codex",
                "cececece-0000-4000-8000-000000000001",
                &cwd,
                "Codex current project",
            ),
        ] {
            let (path, bytes) = if runtime == "claude-code" {
                (
                    runtime_home
                        .join(".claude/projects")
                        .join(agit::adapter::claude_code::slug_for(project))
                        .join(format!("{id}.jsonl")),
                    format!(
                        "{}\n",
                        serde_json::json!({"type":"user", "sessionId":id, "cwd":project,
                        "message":{"role":"user", "content":prompt}})
                    ),
                )
            } else {
                (
                    runtime_home
                        .join(".codex/sessions")
                        .join(format!("rollout-{id}.jsonl")),
                    format!(
                        "{}\n{}\n{}\n",
                        serde_json::json!({"type":"session_meta", "payload":{"id":id, "cwd":project}}),
                        serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{"type":"input_text", "text":prompt}]}}),
                        serde_json::json!({"type":"event_msg", "payload":{"type":"user_message", "message":prompt}})
                    ),
                )
            };
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
                .unwrap();
            sources.push((runtime, id, path, bytes));
        }
        let mut terminal = Terminal::start(&home, &cwd, "import", 100);
        terminal.wait_for("choose a runtime scope");
        if scenario == "cancel" {
            terminal.type_keys("\u{1b}");
        } else {
            // Runtime tabs are consumed before import's ordinary repo/session/destination tabs.
            terminal.type_keys(if scenario == "runtime" {
                "\t\t\r"
            } else {
                "\t\r"
            });
            terminal.wait_for(if scenario == "runtime" {
                "Codex current project"
            } else {
                "Claude current project"
            });
            terminal.type_keys("l");
            let (expected, project) = if scenario == "runtime" {
                (sources[2].1, &cwd)
            } else {
                terminal.type_keys("a");
                terminal.type_keys(&format!("/{}\r", sources[1].1));
                (sources[1].1, &other)
            };
            terminal.type_keys("\t\r");
            assert!(terminal.child.wait().unwrap().success());
            let runtime = if scenario == "runtime" {
                "codex"
            } else {
                "claude-code"
            };
            let store = agit::domain::store::Store::at(home.join("store"));
            let links = agit::domain::link::list(&store);
            assert_eq!(links.len(), 1);
            assert_eq!(links[0].source, runtime);
            assert_eq!(links[0].session_id, expected);
            assert_eq!(links[0].cwd.as_deref(), project.to_str());
            assert!(links[0].agent.is_none());
        }
        if scenario == "cancel" {
            assert!(terminal.child.wait().unwrap().success());
            assert!(
                agit::domain::link::list(&agit::domain::store::Store::at(home.join("store")))
                    .is_empty()
            );
        }
        assert!(!home.join("repos").exists());
        assert!(!home.join("workspaces").exists());
        for (_, _, path, bytes) in sources {
            assert_eq!(std::fs::read_to_string(path).unwrap(), bytes);
        }
    }
}

#[test]
fn resume_runtime_tabs_and_project_toggle_preserve_claims_without_launching() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    let home = tmp.path().join("agit");
    fixture(&home);
    native_fixture(&home, &cwd, true);
    let other = cwd.join("other-project");
    let mut external = agit::domain::link::Link::new(
        "codex",
        "bcbcbcbc-0000-4000-8000-000000000001",
        Some(&other),
    );
    external.owner = Some("me".into());
    external.agent = Some("fixture".into());
    external.branch = Some("elsewhere".into());
    let store = agit::domain::store::Store::at(home.join("store"));
    agit::domain::link::write(&store, &external).unwrap();
    let before: Vec<_> = walkdir::WalkDir::new(&home)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().to_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    let mut terminal = Terminal::start(&home, &cwd, "resume", 100);
    terminal.wait_for("choose a runtime scope");
    terminal.type_keys("\r");
    terminal.wait_for("enter continue");
    terminal.checkpoint();
    terminal.type_keys("a");
    terminal.wait_for("sessions (2) · all runtimes");
    terminal.checkpoint();
    terminal.type_keys("\t");
    terminal.wait_for("sessions (1) · claude-code");
    terminal.checkpoint();
    terminal.type_keys("\t");
    terminal.wait_for("sessions (1) · codex");
    terminal.wait_for("elsewhere");
    terminal.wait_for("other-project");
    terminal.type_keys("q");
    assert!(terminal.child.wait().unwrap().success());
    for (path, bytes) in before {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!(agit::domain::link::list(&store).len(), 2);
}

#[test]
fn native_unnamed_sessions_offer_skip_then_reopen_the_naming_inbox() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    let home = tmp.path().join("agit");
    fixture(&home);
    native_fixture(&home, &cwd, false);
    let mut terminal = Terminal::start(&home, &cwd, "resume", 60);
    terminal.wait_for("agit name");
    terminal.wait_for("me/fixture");
    terminal.type_keys("\rnarrow-name");
    terminal.wait_for("esc stop editing");
    terminal.checkpoint();
    terminal.type_keys("\u{1b}");
    terminal.wait_for("s skip");
    terminal.checkpoint();
    terminal.type_keys("s");
    terminal.wait_for("enter continue");
    terminal.checkpoint();
    terminal.type_keys("\r");
    terminal.wait_for("agit name");
    terminal.type_keys("q");
    assert!(terminal.child.wait().unwrap().success());
    assert!(
        !home
            .join("store/claude-code")
            .join(format!("{NATIVE_ID}.json"))
            .exists()
    );
}

#[test]
fn bare_new_retries_invalid_names_and_hands_the_named_branch_to_the_runtime() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("agit");
    let repo = fixture(&home);
    let runtime = home.join("test-runtime-home/bin/claude");
    std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
    std::fs::write(
        &runtime,
        "#!/bin/sh\nprintf 'stub runtime: %s\\n' \"$AGIT_SESSION\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut terminal = Terminal::start(&home, tmp.path(), "new", 100);
    terminal.wait_for("me/fixture");
    terminal.type_keys("\r");
    terminal.wait_for("branch name for the new session");
    terminal.type_keys("bad name\r");
    terminal.wait_for("contains illegal character");
    terminal.type_keys("work\r");
    terminal.wait_for("already exists");
    terminal.type_keys("tui-created\r");
    terminal.wait_for("stub runtime: me/fixture@tui-created");
    assert!(terminal.child.wait().unwrap().success());
    assert_eq!(
        meta::line_at_ref(&repo, "tui-created"),
        Some(meta::Line::Session)
    );
    assert_eq!(meta::line_at_ref(&repo, "main"), Some(meta::Line::File));
}

#[test]
fn bare_share_can_select_and_cancel_without_a_workspace_binding() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("agit");
    let repo = fixture(&home);
    let before = repo.git(&["rev-parse", "HEAD"]).unwrap();
    for columns in [60, 100] {
        let mut terminal = Terminal::start(&home, tmp.path(), "share", columns);
        terminal.wait_for("session");
        terminal.wait_for("me/fixture@work");
        terminal.type_keys("\r");
        terminal.wait_for("settings");
        terminal.wait_for("Encrypted");
        terminal.type_keys(" ");
        terminal.wait_for("Public");
        terminal.type_keys("j ");
        terminal.wait_for("24h");
        terminal.type_keys("jj ");
        terminal.wait_for("Required");
        terminal.checkpoint();
        terminal.type_keys("q");
        terminal.wait_for_control(b"\x1b[?1049l");
        assert!(terminal.child.wait().unwrap().success());
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), before);
        assert!(!home.join("workspaces").exists());
    }
}

#[test]
fn bare_log_selects_a_session_before_resolving_directory_context() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("agit");
    fixture(&home);
    let mut terminal = Terminal::start(&home, tmp.path(), "log", 100);
    terminal.wait_for("me/fixture@work");
    terminal.type_keys("\r");
    terminal.wait_for("fixture conversation");
    terminal.type_keys("q");
    assert!(terminal.child.wait().unwrap().success());
    assert!(!home.join("workspaces").exists());
}

#[test]
fn bare_push_requires_a_transient_selection_in_a_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("agit");
    fixture(&home);
    let mut terminal = Terminal::start(&home, tmp.path(), "push", 100);
    terminal.wait_for("me/fixture@work");
    terminal.type_keys("q");
    assert!(terminal.child.wait().unwrap().success());
    assert!(!home.join("workspaces").exists());
}
