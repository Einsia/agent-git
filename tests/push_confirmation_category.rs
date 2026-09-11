//! Publication requires explicit confirmation before a foreign checkout becomes an owned copy.

use agit::domain::meta;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[path = "support/publication_http.rs"]
mod publication_http;
#[path = "support/publication_process.rs"]
mod publication_process;

struct Lab {
    mode: &'static str,
    deadline: Instant,
    root: tempfile::TempDir,
    home: PathBuf,
    hub: TcpListener,
    base: String,
}

impl Lab {
    fn new(mode: &'static str) -> Self {
        eprintln!("publication mode={mode} stage=fixture start");
        let deadline = Instant::now() + publication_process::MODE_LIMIT;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        for name in ["work", "tmp", "templates"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        hub.set_nonblocking(true).unwrap();
        let base = format!("http://{}", hub.local_addr().unwrap());
        let lab = Self {
            mode,
            deadline,
            root,
            home,
            hub,
            base,
        };
        let mut command = lab.command(env!("CARGO_BIN_EXE_agit"));
        command.args(["config", "hub.url"]);
        let output = lab.output(command, "config-warmup");
        assert!(output.status.success(), "{output:?}");
        let credential = agit::infra::credentials::HubCredential {
            username: "alice".into(),
            email: None,
            hub: Some(lab.base.clone()),
            access_token: "synthetic-publication-token".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "synthetic-publication-refresh".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        eprintln!("publication mode={mode} stage=credential-save start");
        agit::infra::credentials::save_at(
            &lab.home.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(&lab.base).unwrap()
            )),
            &credential,
        )
        .unwrap();
        eprintln!("publication mode={mode} stage=credential-save done");
        lab
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.env_clear();
        for name in ["PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", &self.base)
            .env("TMP", self.root.path().join("tmp"))
            .env("TEMP", self.root.path().join("tmp"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.root.path().join("empty-config"))
            .env("GIT_TEMPLATE_DIR", self.root.path().join("templates"))
            .env("GIT_AUTHOR_NAME", "Publication category fixture")
            .env("GIT_AUTHOR_EMAIL", "publication@example.invalid")
            .env("GIT_COMMITTER_NAME", "Publication category fixture")
            .env("GIT_COMMITTER_EMAIL", "publication@example.invalid")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_TUI", "0")
            .current_dir(self.root.path().join("work"))
            .stdin(Stdio::null());
        command
    }

    fn output(&self, command: Command, stage: &str) -> Output {
        publication_process::output(command, self.mode, stage, self.deadline)
            .unwrap_or_else(|error| panic!("publication mode={} stage={stage}: {error}", self.mode))
    }

    fn git(&self, path: &Path, args: &[&str]) -> String {
        let stage = match args[0] {
            "init" => "git-init",
            "config" => "git-config",
            "add" => "git-add",
            "commit" => "git-commit",
            "show-ref" => "git-show-ref",
            _ => panic!("a fixture Git command needs a fixed diagnostic stage"),
        };
        let mut command = self.command("git");
        command.arg("-C").arg(path).args(args);
        let out = self.output(command, stage);
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn seed(&self, owner: &str, name: &str, metadata: bool) -> PathBuf {
        let path = self.home.join("repos").join(owner).join(name);
        fs::create_dir_all(&path).unwrap();
        self.git(&path, &["init", "--initial-branch=main"]);
        self.git(&path, &["config", "commit.gpgsign", "false"]);
        if metadata {
            meta::write(&path, &meta::Meta::new_file_line()).unwrap();
        } else {
            fs::write(path.join("AGENTS.md"), "Synthetic shared content.\n").unwrap();
        }
        self.git(&path, &["add", "."]);
        self.git(
            &path,
            &["commit", "-m", "Create synthetic publication history"],
        );
        path
    }

    fn push(&self, mode: &str, dry_run: bool) -> Output {
        let mut command = self.command(env!("CARGO_BIN_EXE_agit"));
        if mode == "quiet" {
            command.arg("--quiet");
        }
        if let Some(version) = mode.strip_prefix("json") {
            command.args(["--json", "--json-version", version]);
        }
        command.args(["push", "other/qa", "-b", "main"]);
        if dry_run {
            command.arg("--dry-run");
        }
        self.output(
            command,
            if dry_run {
                "push-dry-run"
            } else {
                "push-confirmation"
            },
        )
    }

    fn state(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(self.root.path())
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(!entry.file_type().is_symlink());
                let data = entry
                    .file_type()
                    .is_file()
                    .then(|| fs::read(entry.path()).unwrap());
                (
                    entry
                        .path()
                        .strip_prefix(self.root.path())
                        .unwrap()
                        .to_owned(),
                    data,
                )
            })
            .collect()
    }

    fn no_requests(&self) {
        assert_eq!(
            self.hub.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;
        if let Ok(bytes) = fs::read(self.home.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

struct HttpWorker {
    mode: &'static str,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<Vec<String>>>,
}

impl HttpWorker {
    fn start(lab: &Lab) -> Self {
        let listener = lab.hub.try_clone().unwrap();
        let base = lab.base.clone();
        let mode = lab.mode;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + publication_process::MODE_LIMIT;
            let mut requests = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "loopback request deadline elapsed"
                );
                let (mut stream, _) = match publication_http::accept(&listener) {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                };
                let request_index = requests.len();
                eprintln!("publication mode={mode} stage=http-accepted request={request_index}");
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let header_deadline = Instant::now() + Duration::from_secs(3);
                let mut bytes = Vec::new();
                loop {
                    if stopping.load(Ordering::Acquire) {
                        return requests;
                    }
                    assert!(
                        Instant::now() < header_deadline,
                        "request header deadline elapsed"
                    );
                    let mut byte = [0];
                    match stream.read(&mut byte) {
                        Ok(1) => {}
                        Ok(_) => panic!("request header ended early"),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::Interrupted
                            ) =>
                        {
                            continue;
                        }
                        Err(error) => panic!("request header read failed: {error}"),
                    }
                    bytes.push(byte[0]);
                    assert!(bytes.len() <= 65536, "request header exceeded its bound");
                    if bytes.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let header = String::from_utf8(bytes).unwrap();
                let first = header.lines().next().unwrap().to_owned();
                assert!(header.lines().any(|line| {
                    line.split_once(':').is_some_and(|(name, value)| {
                        name.eq_ignore_ascii_case("authorization")
                            && value.trim() == "Bearer synthetic-publication-token"
                    })
                }));
                let (status, body) = match requests.len() {
                    0 => {
                        assert_eq!(first, "GET /api/agents/other/qa HTTP/1.1");
                        (
                            200,
                            serde_json::json!({
                                "agent_id":"9f2c3b53-7fe0-412f-b62a-bf68a6845ce7",
                                "owner":"other", "name":"qa", "visibility":"private",
                                "clone_url":format!("{base}/other/qa.git")
                            })
                            .to_string(),
                        )
                    }
                    1 => {
                        assert_eq!(
                            first,
                            "GET /other/qa.git/info/refs?service=git-receive-pack HTTP/1.1"
                        );
                        assert!(header.lines().any(|line| {
                            line.split_once(':').is_some_and(|(name, value)| {
                                name.eq_ignore_ascii_case(
                                    agit::hub::identity::EXPECTED_AGENT_ID_HEADER,
                                ) && value.trim() == "9f2c3b53-7fe0-412f-b62a-bf68a6845ce7"
                            })
                        }));
                        (403, "{}".to_owned())
                    }
                    _ => panic!("confirmation refusal performed an additional request: {first}"),
                };
                requests.push(first);
                write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                stream.flush().unwrap();
                eprintln!("publication mode={mode} stage=http-replied request={request_index}");
            }
            requests
        });
        Self {
            mode,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.stop.store(true, Ordering::Release);
        eprintln!("publication mode={} stage=http-join start", self.mode);
        let requests =
            publication_http::join(self.worker.take().unwrap()).expect("loopback worker failed");
        eprintln!(
            "publication mode={} stage=http-join done requests={}",
            self.mode,
            requests.len()
        );
        requests
    }
}

impl Drop for HttpWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let joined = publication_http::join(worker);
            eprintln!(
                "publication mode={} stage=http-cleanup joined={}",
                self.mode,
                joined.is_ok()
            );
            if !thread::panicking() {
                assert!(joined.is_ok(), "loopback worker failed during cleanup");
            }
        }
    }
}

fn assert_refusal(output: &Output, mode: &str) {
    assert_eq!(output.status.code(), Some(8), "{mode}: {output:?}");
    let text = if let Some(version) = mode.strip_prefix("json") {
        assert!(output.stderr.is_empty(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], version.parse::<u64>().unwrap());
        assert_eq!(value["command"], "push");
        assert_eq!(value["exit_code"], 8);
        assert_eq!(value["ok"], false);
        value["diagnostics"]["stderr"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["message"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::from_utf8(output.stderr.clone()).unwrap()
    };
    assert!(text.contains("no TTY here to ask"), "{text}");
    assert!(text.contains("agit clone other/qa --mine"), "{text}");
    for stream in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains("synthetic-publication-token"));
    }
}

#[test]
fn noninteractive_copy_confirmation_preserves_the_foreign_checkout() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new(mode);
        let repo = lab.seed("other", "qa", true);
        // Initialize the ordinary local scan state before measuring the confirmation refusal.
        let dry_run = lab.push("human", true);
        assert!(dry_run.status.success(), "{dry_run:?}");
        lab.no_requests();
        eprintln!("publication mode={mode} stage=snapshot-before start");
        let before = lab.state();
        eprintln!("publication mode={mode} stage=snapshot-before done");
        let refs = lab.git(&repo, &["show-ref"]);
        let worker = HttpWorker::start(&lab);
        let output = lab.push(mode, false);
        let requests = worker.finish();
        assert_refusal(&output, mode);
        assert_eq!(
            requests.len(),
            2,
            "the read capability proof must precede confirmation"
        );
        eprintln!("publication mode={mode} stage=postconditions start");
        assert_eq!(lab.state(), before, "{mode}: refusal changed local files");
        assert_eq!(lab.git(&repo, &["show-ref"]), refs);
        assert!(!lab.home.join("repos/alice/qa").exists());
        lab.no_requests();
        eprintln!("publication mode={mode} stage=postconditions done");
    }
}
