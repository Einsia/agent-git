#![cfg(unix)]

#[path = "support/terminal_screen.rs"]
mod terminal_screen;

use agit::domain::{meta, repo::Repo};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime};
use terminal_screen::screen_text;

const AGENT_ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

#[derive(Clone)]
enum Listing {
    Ready(Vec<Value>),
    Unavailable,
    Unauthorized,
    Pending,
}

struct Hub {
    url: String,
    listing: Arc<(Mutex<Listing>, Condvar)>,
    requests: Arc<Mutex<Vec<String>>>,
    stopped: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Hub {
    fn start(listing: Listing) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let listing = Arc::new((Mutex::new(listing), Condvar::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let state = listing.clone();
        let seen = requests.clone();
        let stop = stopped.clone();
        let server = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept mock Hub request: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let line = request.lines().next().unwrap_or_default();
                let path = line.split_whitespace().nth(1).unwrap_or_default();
                seen.lock().unwrap().push(path.to_owned());
                assert!(line.starts_with("GET "), "unexpected Hub mutation: {line}");
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer synthetic-token\r\n"),
                    "repository discovery must carry the signed-in identity"
                );
                let (lock, changed) = &*state;
                let mut listing = lock.lock().unwrap();
                while matches!(*listing, Listing::Pending) && !stop.load(Ordering::Relaxed) {
                    listing = changed.wait(listing).unwrap();
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let (status, body) = match &*listing {
                    Listing::Ready(repos) if path == "/api/agents?owner=me" => {
                        ("200 OK", json!(repos))
                    }
                    Listing::Ready(repos) => match repos.iter().find(|repo| {
                        path == format!(
                            "/api/agents/{}/{}",
                            repo["owner"].as_str().unwrap(),
                            repo["name"].as_str().unwrap()
                        )
                    }) {
                        Some(repo) => ("200 OK", repo.clone()),
                        None => (
                            "404 Not Found",
                            json!({"error":"not found","kind":"not_found"}),
                        ),
                    },
                    Listing::Unavailable => (
                        "503 Service Unavailable",
                        json!({"error":"repository list unavailable","kind":"unavailable"}),
                    ),
                    Listing::Unauthorized => (
                        "401 Unauthorized",
                        json!({"error":"access token expired","kind":"unauthorized"}),
                    ),
                    Listing::Pending => unreachable!(),
                };
                drop(listing);
                let body = body.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            url,
            listing,
            requests,
            stopped,
            server: Some(server),
        }
    }

    fn set_listing(&self, value: Listing) {
        let (listing, changed) = &*self.listing;
        *listing.lock().unwrap() = value;
        changed.notify_all();
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn wait_for_request(&self, path: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.requests().iter().any(|request| request == path) {
            assert!(Instant::now() < deadline, "Hub never received {path}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.set_listing(Listing::Ready(Vec::new()));
        let result = self.server.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

struct Lab {
    temp: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
}

impl Lab {
    fn new(hub: &Hub) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("agit");
        let work = temp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        agit::infra::credentials::save_at(
            &home.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(&hub.url).unwrap()
            )),
            &agit::infra::credentials::HubCredential {
                username: "me".into(),
                email: Some("me@example.test".into()),
                hub: Some(hub.url.clone()),
                access_token: "synthetic-token".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_token: "synthetic-refresh".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
        std::fs::write(
            home.join("cli-update.json"),
            json!({
                "checked_at":SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
                "latest":env!("CARGO_PKG_VERSION"),
            }).to_string(),
        )
        .unwrap();
        Self { temp, home, work }
    }

    fn remote(&self, name: &str) -> (Value, Repo) {
        let source = file_repo(&self.temp.path().join(format!("source-{name}")));
        source.git(&["switch", "-c", "release-files"]).unwrap();
        std::fs::write(
            source.root().join("AGENTS.md"),
            "Release shared instructions.\n",
        )
        .unwrap();
        source.add_all().unwrap();
        source.commit("release shared instructions").unwrap();
        source.git(&["tag", "release"]).unwrap();
        source.git(&["switch", "main"]).unwrap();
        let bare = self.temp.path().join(format!("{name}.git"));
        let output = Command::new("git")
            .args(["clone", "--quiet", "--bare"])
            .arg(source.root())
            .arg(&bare)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (remote("me", name, bare.to_str().unwrap()), source)
    }

    fn terminal(&self, hub: &Hub, from: Option<&str>) -> Terminal {
        Terminal::start(self, hub, from)
    }
}

fn remote(owner: &str, name: &str, clone_url: &str) -> Value {
    json!({
        "agent_id":AGENT_ID, "owner":owner, "name":name, "clone_url":clone_url,
        "visibility":"private", "session_count":3,
    })
}

fn file_repo(path: &Path) -> Repo {
    let repo = Repo::init(path).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    std::fs::write(repo.root().join("AGENTS.md"), "Main shared instructions.\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("shared instructions").unwrap();
    repo.git(&["branch", "-m", "main"]).unwrap();
    repo
}

struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    initial_flags: (
        libc::tcflag_t,
        libc::tcflag_t,
        libc::tcflag_t,
        libc::tcflag_t,
    ),
    writer: Box<dyn Write + Send>,
    output: std::sync::mpsc::Receiver<Vec<u8>>,
    captured: Vec<u8>,
}

impl Terminal {
    fn start(lab: &Lab, hub: &Hub, from: Option<&str>) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 28,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let initial_flags = terminal_flags(&*pair.master);
        let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
        builder.args(["new", "--no-launch"]);
        if let Some(from) = from {
            builder.args(["--from", from]);
        }
        let runtime_home = lab.temp.path().join("runtime-home");
        std::fs::create_dir_all(&runtime_home).unwrap();
        builder.cwd(&lab.work);
        builder.env_clear();
        builder.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        builder.env("HOME", &runtime_home);
        builder.env("CODEX_HOME", runtime_home.join(".codex"));
        builder.env("AGIT_HOME", &lab.home);
        builder.env("AGIT_HUB_URL", &hub.url);
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
            let mut buffer = [0; 8192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            master: pair.master,
            initial_flags,
            writer,
            output,
            captured: Vec::new(),
        }
    }

    fn text(&self) -> String {
        let ansi = regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap();
        let captured = String::from_utf8_lossy(&self.captured);
        format!(
            "{}\n{}",
            ansi.replace_all(&captured, ""),
            screen_text(&captured, &ansi, 28, 120)
        )
    }

    fn wait_for(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.text().contains(text) {
            let chunk = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!("waiting for {text:?}: {error}; output: {}", self.text())
                });
            self.captured.extend(chunk);
        }
    }

    fn type_keys(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn finish(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            while let Ok(chunk) = self.output.try_recv() {
                self.captured.extend(chunk);
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "command failed: {}", self.text());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "command did not exit: {}",
                self.text()
            );
            if let Ok(chunk) = self.output.recv_timeout(Duration::from_millis(10)) {
                self.captured.extend(chunk);
            }
        }
        loop {
            match self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(chunk) => self.captured.extend(chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!(
                        "terminal output remained open after command exit: {}",
                        self.text()
                    );
                }
            }
        }
    }

    fn assert_restored(&mut self) {
        assert_eq!(terminal_flags(&*self.master), self.initial_flags);
        let output = String::from_utf8_lossy(&self.captured);
        assert!(
            output.contains("\x1b[?1049h"),
            "the TUI must enter the alternate screen"
        );
        assert!(
            output.contains("\x1b[?1049l"),
            "the TUI must leave the alternate screen"
        );
        assert!(
            output.contains("\x1b[?25h"),
            "the TUI must restore the cursor"
        );
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn terminal_flags(
    master: &dyn MasterPty,
) -> (
    libc::tcflag_t,
    libc::tcflag_t,
    libc::tcflag_t,
    libc::tcflag_t,
) {
    let flags = master.get_termios().unwrap();
    (
        flags.input_flags.bits(),
        flags.output_flags.bits(),
        flags.control_flags.bits(),
        flags.local_flags.bits(),
    )
}

#[test]
fn signed_in_remote_only_repositories_are_listed_without_cloning() {
    let hub = Hub::start(Listing::Ready(vec![
        remote("me", "remote-only", "unused"),
        remote("someone-else", "not-owned", "unused"),
    ]));
    let lab = Lab::new(&hub);
    let mut terminal = lab.terminal(&hub, None);
    terminal.wait_for("me/remote-only");
    terminal.wait_for("Hub · clone on selection");
    assert!(!lab.home.join("repos").exists());
    assert_eq!(hub.requests(), ["/api/agents?owner=me"]);
    terminal.type_keys("q");
    terminal.finish(Duration::from_secs(2));
    terminal.assert_restored();
    assert!(!lab.home.join("repos").exists());
    assert!(!terminal.text().contains("someone-else/not-owned"));
}

#[test]
fn selecting_a_hub_repository_clones_it_and_creates_the_named_session() {
    let hub = Hub::start(Listing::Pending);
    let lab = Lab::new(&hub);
    let (remote, source) = lab.remote("remote-only");
    hub.set_listing(Listing::Ready(vec![remote]));
    let mut terminal = lab.terminal(&hub, None);
    terminal.wait_for("Hub · clone on selection");
    assert!(!lab.home.join("repos").exists());
    terminal.type_keys("\r");
    terminal.wait_for("branch name for the new session");
    terminal.type_keys("from-hub\r");
    terminal.wait_for("Automatically push settled turns from this repository?");
    terminal.type_keys("\x1b[B\x1b[B\r");
    terminal.finish(Duration::from_secs(10));
    terminal.assert_restored();
    let repo = Repo::open(lab.home.join("repos/me/remote-only")).unwrap();
    assert_eq!(repo.auto_push_override().unwrap(), Some(false));
    assert_eq!(
        meta::line_at_ref(&repo, "from-hub"),
        Some(meta::Line::Session)
    );
    assert_eq!(
        repo.git(&["rev-parse", "from-hub^"]).unwrap(),
        source.git(&["rev-parse", "main"]).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(lab.work.join("AGENTS.md")).unwrap(),
        "Main shared instructions."
    );
    assert_eq!(
        hub.requests(),
        ["/api/agents?owner=me", "/api/agents/me/remote-only"]
    );
    assert!(!lab.home.join("workspaces").exists());
}

#[test]
fn selecting_a_hub_repository_preserves_the_requested_file_ref() {
    let hub = Hub::start(Listing::Pending);
    let lab = Lab::new(&hub);
    let (remote, source) = lab.remote("remote-only");
    hub.set_listing(Listing::Ready(vec![remote]));
    let mut terminal = lab.terminal(&hub, Some("release"));
    terminal.wait_for("Hub · clone on selection");
    terminal.type_keys("\r");
    terminal.wait_for("branch name for the new session");
    terminal.type_keys("release-session\r");
    terminal.wait_for("Automatically push settled turns from this repository?");
    terminal.type_keys("\r");
    terminal.finish(Duration::from_secs(10));
    let repo = Repo::open(lab.home.join("repos/me/remote-only")).unwrap();
    assert_eq!(
        repo.git(&["rev-parse", "release-session^"]).unwrap(),
        source.git(&["rev-parse", "release"]).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(lab.work.join("AGENTS.md")).unwrap(),
        "Release shared instructions."
    );
}

#[test]
fn slow_hub_discovery_does_not_delay_local_rows_or_cancellation() {
    let hub = Hub::start(Listing::Pending);
    let lab = Lab::new(&hub);
    let local = file_repo(&lab.home.join("repos/me/local"));
    let original = local.git(&["show-ref"]).unwrap();
    let mut terminal = lab.terminal(&hub, None);
    terminal.wait_for("me/local");
    terminal.wait_for("Hub: loading repositories");
    hub.wait_for_request("/api/agents?owner=me");
    terminal.type_keys("q");
    terminal.finish(Duration::from_secs(2));
    terminal.assert_restored();
    assert_eq!(local.git(&["show-ref"]).unwrap(), original);
    assert_eq!(hub.requests(), ["/api/agents?owner=me"]);
    assert_eq!(
        std::fs::read_dir(lab.home.join("repos/me"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn unavailable_hub_can_be_retried_without_leaving_the_picker() {
    let hub = Hub::start(Listing::Unavailable);
    let lab = Lab::new(&hub);
    let mut terminal = lab.terminal(&hub, None);
    terminal.wait_for("Hub: unavailable · r retry");
    hub.set_listing(Listing::Ready(vec![remote("me", "recovered", "unused")]));
    terminal.type_keys("r");
    terminal.wait_for("me/recovered");
    terminal.wait_for("Hub · clone on selection");
    terminal.type_keys("q");
    terminal.finish(Duration::from_secs(2));
    terminal.assert_restored();
    assert_eq!(
        hub.requests(),
        ["/api/agents?owner=me", "/api/agents?owner=me"]
    );
    assert!(!lab.home.join("repos").exists());
}

#[test]
fn expired_auth_during_discovery_never_rotates_the_refresh_token() {
    let hub = Hub::start(Listing::Unauthorized);
    let lab = Lab::new(&hub);
    let credential = lab.home.join("credentials").join(format!(
        "{}.json",
        agit::infra::config::hub_host_key(&hub.url).unwrap()
    ));
    let original_credential = std::fs::read(&credential).unwrap();
    let mut terminal = lab.terminal(&hub, None);
    terminal.wait_for("Hub: sign in with agit login · r retry");
    terminal.type_keys("q");
    terminal.finish(Duration::from_secs(2));
    terminal.assert_restored();
    assert_eq!(hub.requests(), ["/api/agents?owner=me"]);
    assert_eq!(std::fs::read(credential).unwrap(), original_credential);
    assert!(!lab.home.join("repos").exists());
}

#[test]
fn an_empty_signed_in_account_shows_repository_creation_guidance() {
    let hub = Hub::start(Listing::Ready(Vec::new()));
    let lab = Lab::new(&hub);
    let mut terminal = lab.terminal(&hub, None);
    terminal.wait_for("No repositories found");
    terminal.wait_for("agit init");
    terminal.type_keys("q");
    terminal.finish(Duration::from_secs(2));
    terminal.assert_restored();
    assert!(!lab.home.join("repos").exists());
}

#[test]
fn duplicate_hub_rows_preserve_local_branch_validation() {
    let hub = Hub::start(Listing::Ready(vec![remote("me", "local", "unused")]));
    let lab = Lab::new(&hub);
    let local = file_repo(&lab.home.join("repos/me/local"));
    local.git(&["branch", "existing"]).unwrap();
    let mut terminal = lab.terminal(&hub, None);
    terminal.wait_for("me/local");
    terminal.wait_for("Hub: repositories loaded");
    hub.wait_for_request("/api/agents?owner=me");
    terminal.type_keys("\r");
    terminal.wait_for("branch name for the new session");
    terminal.type_keys("existing\r");
    terminal.wait_for("already exists in me/local");
    terminal.type_keys("local-session\r");
    terminal.finish(Duration::from_secs(10));
    terminal.assert_restored();
    assert_eq!(
        meta::line_at_ref(&local, "local-session"),
        Some(meta::Line::Session)
    );
    assert_eq!(hub.requests(), ["/api/agents?owner=me"]);
    assert!(!terminal.text().contains("Hub · clone on selection"));
}
