//! `agit run` tells the Hub which session it starts from, and the run never depends on the
//! answer.

use agit::domain::{meta, repo::Repo, storage, transcript};
use agit::hub::identity::{self, RemoteIdentity};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

const AGENT: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const NATIVE: &str = "cccccccc-0000-4000-8000-000000000003";
const TOKEN: &str = "synthetic-access";

#[derive(Debug, Clone, PartialEq)]
struct Receipt {
    path: String,
    authorization: Option<String>,
    body: Value,
}

/// Answers every reuse receipt with `204` and everything else with `404`, recording each
/// request line it serves.
struct Hub {
    url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    receipts: Arc<Mutex<Vec<Receipt>>>,
    server: Option<JoinHandle<()>>,
}

impl Hub {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let receipts = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::clone(&stop);
        let served = Arc::clone(&requests);
        let received = Arc::clone(&receipts);
        let server = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("local hub accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request = read_request(&mut stream);
                let (head, body) = request.split_once("\r\n\r\n").unwrap();
                let mut first = head.lines().next().unwrap().split_whitespace();
                let (method, path) = (first.next().unwrap(), first.next().unwrap());
                served.lock().unwrap().push(format!("{method} {path}"));
                let status = if method == "POST" && path.ends_with("/reuses") {
                    received.lock().unwrap().push(Receipt {
                        path: path.to_owned(),
                        authorization: head.lines().find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("authorization")
                                .then(|| value.trim().to_owned())
                        }),
                        body: serde_json::from_str(body).unwrap(),
                    });
                    "204 No Content"
                } else {
                    "404 Not Found"
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        });
        Self {
            url,
            stop,
            requests,
            receipts,
            server: Some(server),
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "local hub request ended before its body");
        request.extend_from_slice(&chunk[..read]);
        let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..end]);
        let length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
            .unwrap_or(0);
        if request.len() >= end + 4 + length {
            return String::from_utf8(request).unwrap();
        }
    }
}

/// `me/qa`, pinned to `hub` with its origin there: `work` holds two settled turns of one
/// session, and the tag `v1` names the first of them.
struct Lab {
    temporary: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    hub: String,
    session: String,
    first: String,
    head: String,
}

impl Lab {
    fn new(hub: &str) -> Self {
        Self::with_access_until(hub, "2099-01-01T00:00:00Z")
    }

    fn with_access_until(hub: &str, access_expires_at: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("agit");
        let work = temporary.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let work = work.canonicalize().unwrap();
        let credential = agit::infra::credentials::HubCredential {
            account_id: None,
            username: "me".into(),
            email: None,
            hub: Some(hub.to_owned()),
            access_token: TOKEN.into(),
            access_expires_at: access_expires_at.into(),
            refresh_token: "synthetic-refresh".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        agit::infra::credentials::save_at(
            &home.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(hub).unwrap()
            )),
            &credential,
        )
        .unwrap();
        let repo = Repo::init(&home.join("repos/me/qa")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let session = format!("agit-{}", "a".repeat(40));
        let mut commits = Vec::new();
        for turn in 1..=2 {
            let raw = format!(
                "{}\n{}\n",
                json!({"type": "user", "sessionId": NATIVE, "cwd": work, "uuid": format!("user-{turn}"),
                    "message": {"role": "user", "content": format!("Turn {turn}.")}}),
                json!({"type": "assistant", "sessionId": NATIVE, "cwd": work, "uuid": format!("assistant-{turn}"),
                    "message": {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}}),
            );
            let events = transcript::wrap_lines(&raw, "claude-code", &session);
            storage::write_snapshot(repo.root(), &events, &events).unwrap();
            let mut snapshot = meta::Meta::new(
                session.clone(),
                "claude-code".into(),
                work.to_string_lossy().into_owned(),
            );
            snapshot.turn = Some(turn);
            meta::write(repo.root(), &snapshot).unwrap();
            repo.add_all().unwrap();
            repo.commit(&format!("Turn {turn}")).unwrap();
            commits.push(repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned());
        }
        repo.git(&["branch", "-m", "work"]).unwrap();
        repo.git(&["tag", "v1", &commits[0]]).unwrap();
        identity::pin(&repo, &RemoteIdentity::new(hub, AGENT).unwrap()).unwrap();
        repo.git(&["remote", "add", "origin", &format!("{hub}/me/qa.git")])
            .unwrap();
        Self {
            temporary,
            home,
            work,
            hub: hub.to_owned(),
            session,
            first: commits[0].clone(),
            head: commits[1].clone(),
        }
    }

    fn credentials(&self) -> Vec<u8> {
        std::fs::read(self.home.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(&self.hub).unwrap()
        )))
        .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.temporary.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", &self.hub)
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self.temporary.path().join("empty-gitconfig"),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.work);
        #[cfg(windows)]
        {
            for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command.env("USERPROFILE", self.temporary.path());
        }
        command.output().unwrap()
    }

    /// Continue `work`, then fork from `v1`; both runs must succeed and prepare their session.
    fn continue_and_fork(&self) -> [(String, String); 2] {
        [
            vec!["run", "qa@work", "--no-launch"],
            vec!["run", "qa@v1", "-b", "forked", "--no-launch"],
        ]
        .map(|args| {
            let output = self.run(&args);
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                output.status.success(),
                "{args:?}: stdout={stdout} stderr={stderr}"
            );
            assert!(stdout.contains("materialized VIEW"), "{args:?}: {stdout}");
            (stdout, stderr)
        })
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;

        if let Ok(bytes) = std::fs::read(self.home.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

/// Continuing a branch reports its head commit; forking from a tag reports the tagged commit,
/// not the branch head or the new fork. Each run sends exactly one receipt, under the signed-in
/// token, with its own per-process operation id.
#[test]
fn continue_and_fork_each_report_their_source_point_once() {
    let hub = Hub::new();
    let lab = Lab::new(&hub.url);
    let outputs = lab.continue_and_fork();
    assert!(outputs[0].0.contains("→ continue (resume)"), "{outputs:?}");
    assert!(
        outputs[1].0.contains("forked out me/qa @ forked"),
        "{outputs:?}"
    );

    let receipts = hub.receipts.lock().unwrap().clone();
    let path = format!("/api/agents/me/qa/sessions/{}/reuses", lab.session);
    assert_eq!(receipts.len(), 2, "{receipts:?}");
    for (receipt, (mode, commit)) in receipts
        .iter()
        .zip([("continue", &lab.head), ("fork", &lab.first)])
    {
        assert_eq!(receipt.path, path);
        assert_eq!(receipt.authorization, Some(format!("Bearer {TOKEN}")));
        assert_eq!(receipt.body["mode"], mode);
        assert_eq!(receipt.body["commit"], commit.as_str());
        let operation = receipt.body["operation_id"].as_str().unwrap();
        assert_eq!(
            uuid::Uuid::parse_str(operation).unwrap().get_version_num(),
            4
        );
        assert_eq!(receipt.body.as_object().unwrap().len(), 3, "{receipt:?}");
    }
    assert_ne!(
        receipts[0].body["operation_id"], receipts[1].body["operation_id"],
        "separate runs are separate operations"
    );
}

/// A Hub that cannot be reached leaves both runs exactly as they are with a Hub that records
/// the receipts: the same exit status and the same output on both streams, once the parts
/// that differ between any two labs (temporary paths, ids, commits) are masked.
#[test]
fn an_unreachable_hub_changes_neither_run() {
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let unreachable = Lab::new(&closed);
    let hub = Hub::new();
    let reachable = Lab::new(&hub.url);
    let mask = |text: &str| {
        let text = regex::Regex::new(r"tmp[A-Za-z0-9]{6}")
            .unwrap()
            .replace_all(text, "tmp");
        let text = regex::Regex::new(r"[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}")
            .unwrap()
            .replace_all(&text, "<uuid>");
        regex::Regex::new(r"\b[0-9a-f]{40}\b")
            .unwrap()
            .replace_all(&text, "<commit>")
            .into_owned()
    };
    for ((stdout, stderr), (reachable_stdout, reachable_stderr)) in unreachable
        .continue_and_fork()
        .into_iter()
        .zip(reachable.continue_and_fork())
    {
        assert_eq!(mask(&stdout), mask(&reachable_stdout));
        assert_eq!(mask(&stderr), mask(&reachable_stderr));
    }
    assert_eq!(hub.receipts.lock().unwrap().len(), 2);
}

/// A signed-in account whose access token has expired sends no receipt: renewing it on the
/// receipt's short deadline can spend the single-use refresh token and lose its successor,
/// signing the user out, and dropping the expired token would record the owner's own run as
/// someone else's. Neither run sends a receipt or a renewal, and the saved credentials are left
/// as they were.
#[test]
fn an_expired_access_token_sends_nothing_and_keeps_the_credentials() {
    let hub = Hub::new();
    let lab = Lab::with_access_until(&hub.url, "2000-01-01T00:00:00Z");
    let before = lab.credentials();
    lab.continue_and_fork();
    let requests = hub.requests.lock().unwrap().clone();
    assert!(
        !requests
            .iter()
            .any(|request| request.ends_with("/reuses") || request.contains("/api/auth/")),
        "{requests:?}"
    );
    assert_eq!(lab.credentials(), before);
}

/// A continue that resume refuses after arbitration has chosen it reuses nothing, so it sends
/// no receipt; announcing at arbitration would record every refused attempt and each retry.
#[test]
fn a_continue_that_resume_refuses_sends_no_receipt() {
    let hub = Hub::new();
    let lab = Lab::new(&hub.url);
    let output = lab.run(&["run", "qa@work", "--as", "no-such-runtime", "--no-launch"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success(), "{output:?}");
    assert!(stdout.contains("→ continue (resume)"), "{stdout}");
    assert_eq!(*hub.receipts.lock().unwrap(), Vec::<Receipt>::new());
}
