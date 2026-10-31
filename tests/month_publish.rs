//! `agit month publish` puts each section with the version of the section its draft read.

#![cfg(all(unix, feature = "rc"))]

use agit::infra::credentials::{HubCredential, save_at};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const ACCESS: &str = "SYNTHETIC-month-publish-access";
const SECTION: &str = "/api/orgs/acme/activity/months/2026-10/sections/rid-hub/author";

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    body: Vec<u8>,
}

/// A loopback Hub that answers the section's PUT with `status` and anything else with 404.
struct Hub {
    base: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Vec<Request>>>,
}

impl Hub {
    fn new(status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(120);
            let mut requests = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "loopback Hub deadline elapsed");
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let request = read_request(&mut stream);
                let (code, body) = if request.method == "PUT" && request.target == SECTION {
                    if status == 200 {
                        (200, json!({ "month": "2026-10", "repositories": [] }))
                    } else {
                        (
                            status,
                            json!({ "error": "this section changed after your agent read it" }),
                        )
                    }
                } else {
                    (404, json!({ "error": "not found" }))
                };
                requests.push(request);
                let body = body.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {code} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                stream.flush().unwrap();
            }
            requests
        });
        Self {
            base,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> Vec<Request> {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let end = loop {
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "request headers were truncated");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let header = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let mut lines = header.lines();
    let mut start = lines.next().unwrap().split_whitespace();
    let method = start.next().unwrap().to_owned();
    let target = start.next().unwrap().to_owned();
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    while bytes.len() < end + length {
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "request body was truncated");
        bytes.extend_from_slice(&buffer[..read]);
    }
    Request {
        method,
        target,
        body: bytes[end..end + length].to_vec(),
    }
}

/// A signed-in home with one drafted repository and the member's summary of it.
struct Lab {
    _root: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    session: String,
}

impl Lab {
    fn new(base: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        let lab = Self {
            home: path.join("home"),
            store: path.join("agit"),
            session: format!("agit-{}", "a".repeat(40)),
            _root: root,
        };
        fs::create_dir_all(&lab.home).unwrap();
        save_at(
            &lab.store.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(base).unwrap()
            )),
            &HubCredential {
                account_id: None,
                username: "bob".into(),
                email: None,
                hub: Some(base.into()),
                access_token: ACCESS.into(),
                refresh_token: String::new(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2000-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
        let bundle = lab.bundle();
        fs::create_dir_all(&bundle).unwrap();
        fs::write(
            bundle.join("acme__hub.json"),
            json!({
                "repository": "acme/hub",
                "month": "2026-10",
                "repository_id": "rid-hub",
                "version": "version-the-draft-read",
                "sessions": [{ "session": lab.session }],
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            bundle.join("summary.md"),
            format!(
                "## acme/hub\n\nReworked the lease pool ([Lease pool]({})).\n",
                lab.session
            ),
        )
        .unwrap();
        lab
    }

    fn bundle(&self) -> PathBuf {
        self.store.join("month").join("acme").join("2026-10")
    }

    /// Runs `agit month publish acme 2026-10` at a terminal, confirms the question, and returns
    /// the exit code and what the terminal showed.
    fn publish(&self, base: &str) -> (u32, String) {
        let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        command.env("HOME", &self.home);
        command.env("AGIT_HOME", &self.store);
        command.env("AGIT_HUB_URL", base);
        command.env("AGIT_TELEMETRY_DISABLED", "1");
        command.env("AGIT_SECRETS_KEYSTORE", "file");
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        command.env("NO_COLOR", "1");
        command.env("TERM", "xterm-256color");
        command.cwd(&self.home);
        command.args(["month", "publish", "acme", "2026-10"]);
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut reader = pty.master.try_clone_reader().unwrap();
        let mut writer = pty.master.take_writer().unwrap();
        let screen = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&screen);
        let output = thread::spawn(move || {
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
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if !answered && String::from_utf8_lossy(&screen.lock().unwrap()).contains("Put these") {
                writer.write_all(b"y\r").unwrap();
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
            thread::sleep(Duration::from_millis(20));
        };
        drop(writer);
        drop(pty.master);
        output.join().unwrap();
        let shown = String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
        assert!(
            !shown.contains(ACCESS),
            "the credential reached the terminal"
        );
        (status.exit_code(), shown)
    }
}

fn month_reads(requests: &[Request]) -> Vec<&str> {
    requests
        .iter()
        .filter(|request| request.method == "GET" && request.target.contains("/activity/months/"))
        .map(|request| request.target.as_str())
        .collect()
}

/// The text goes out with the version of the section its draft read, and publish never reads the
/// month again before it: a version read at publish time would describe the section as it is now,
/// so text drafted from a day the member hid since would pass the Hub's check. When the Hub
/// refuses the version because the section changed, publish says to draft again, keeps the
/// draft, and exits with the precondition code rather than reporting success.
#[test]
fn publish_sends_the_drafted_version_and_redrafts_on_a_changed_section() {
    let hub = Hub::new(200);
    let lab = Lab::new(&hub.base);
    let (code, shown) = lab.publish(&hub.base);
    let requests = hub.finish();
    assert_eq!(code, 0, "{shown}");
    assert_eq!(
        month_reads(&requests),
        Vec::<&str>::new(),
        "publish read the month again"
    );
    let puts: Vec<&Request> = requests
        .iter()
        .filter(|request| request.method == "PUT")
        .collect();
    assert_eq!(puts.len(), 1, "{requests:?}");
    assert_eq!(puts[0].target, SECTION);
    let body: Value = serde_json::from_slice(&puts[0].body).unwrap();
    assert_eq!(body["version"], "version-the-draft-read");
    assert_eq!(body["sources"], json!([lab.session]));
    assert!(!lab.bundle().exists(), "a published draft is deleted");

    let hub = Hub::new(409);
    let lab = Lab::new(&hub.base);
    let (code, shown) = lab.publish(&hub.base);
    let requests = hub.finish();
    assert_eq!(code, 4, "{shown}");
    assert!(
        shown.contains("agit month draft acme 2026-10"),
        "the refusal names the way out: {shown}"
    );
    assert_eq!(month_reads(&requests), Vec::<&str>::new());
    assert!(
        lab.bundle().join("acme__hub.json").exists(),
        "a refused draft is kept"
    );
}
