#![cfg(unix)]

use agit::domain::{meta, repo::Repo, storage, transcript};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::time::Duration;

struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: std::sync::mpsc::Receiver<Vec<u8>>,
    captured: String,
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
            captured: String::new(),
        }
    }

    fn wait_for(&mut self, text: &str) {
        while !self.captured.contains(text) {
            let chunk = self
                .output
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|error| {
                    panic!("waiting for {text:?}: {error}; output: {}", self.captured)
                });
            self.captured.push_str(&String::from_utf8_lossy(&chunk));
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
    terminal.captured.clear();
    terminal.type_keys("u");
    terminal.wait_for("unset");
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
    terminal.captured.clear();
    terminal.type_keys("\u{1b}");
    terminal.wait_for("s skip");
    terminal.captured.clear();
    terminal.type_keys("s");
    terminal.wait_for("enter continue");
    terminal.captured.clear();
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
        terminal.type_keys("q");
        terminal.wait_for("\u{1b}[?1049l");
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
