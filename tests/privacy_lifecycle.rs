//! The executable protocol must preserve recovery across independent device homes and retries.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(Default)]
struct Cloud {
    packages: Vec<(String, Value)>,
    reject_upload: bool,
}

struct Hub {
    url: String,
    state: Arc<Mutex<Cloud>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Hub {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(Cloud::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (cloud, stopping) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("privacy fixture accept: {error}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                    assert!(header.len() < 16384);
                }
                let header = String::from_utf8(header).unwrap();
                assert!(
                    header
                        .to_ascii_lowercase()
                        .contains("authorization: bearer fixture-access")
                );
                let mut request = header.lines().next().unwrap().split_whitespace();
                let (method, path) = (request.next().unwrap(), request.next().unwrap());
                let length = header
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                assert!(length <= 512 * 1024);
                let mut body = vec![0; length];
                socket.read_exact(&mut body).unwrap();
                let mut cloud = cloud.lock().unwrap();
                let (status, value) = if path == "/api/me/privacy/key" {
                    (
                        200,
                        json!({"account_id":"fixture-account","version":1,"key":STANDARD.encode([7;32])}),
                    )
                } else if let Some(query) = path.strip_prefix("/api/me/privacy/packages?after=") {
                    let after: usize = query.split('&').next().unwrap().parse().unwrap();
                    let entries: Vec<_> = cloud.packages.iter().enumerate().skip(after).map(|(index,(id, value))| {
                        json!({"package_id":id,"dictionary_id":value["dictionary_id"],"key_version":1,"sequence":index+1})
                    }).collect();
                    (200, json!({"packages":entries,"next_after":null}))
                } else if let Some(id) = path.strip_prefix("/api/me/privacy/packages/") {
                    if method == "PUT" {
                        let envelope: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(envelope.as_object().unwrap().len(), 5);
                        assert_eq!(envelope["format"], 1);
                        assert!(envelope["ciphertext"].as_str().is_some());
                        assert!(!String::from_utf8_lossy(&body).contains("AKIA"));
                        if cloud.reject_upload {
                            (503, json!({"error":"dictionary temporarily unavailable"}))
                        } else {
                            let index = cloud
                                .packages
                                .iter()
                                .position(|(saved, _)| saved == id)
                                .unwrap_or_else(|| {
                                    cloud.packages.push((id.to_owned(), envelope.clone()));
                                    cloud.packages.len() - 1
                                });
                            assert_eq!(cloud.packages[index].1, envelope);
                            (200, json!({"package_id":id,"sequence":index+1}))
                        }
                    } else {
                        (
                            200,
                            cloud
                                .packages
                                .iter()
                                .find(|(saved, _)| saved == id)
                                .unwrap()
                                .1
                                .clone(),
                        )
                    }
                } else {
                    panic!("unexpected privacy request: {method} {path}")
                };
                let bytes = serde_json::to_vec(&value).unwrap();
                write!(socket,"HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).unwrap();
                socket.write_all(&bytes).unwrap();
            }
        });
        Self {
            url,
            state,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if !std::thread::panicking() {
                thread.join().unwrap();
            } else {
                let _ = thread.join();
            }
        }
    }
}

struct Device {
    _root: tempfile::TempDir,
    home: PathBuf,
    hub: String,
}

impl Device {
    fn new(hub: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        let device = Self {
            _root: root,
            home,
            hub: hub.into(),
        };
        let credential = agit::infra::credentials::HubCredential {
            account_id: None,
            username: "fixture".into(),
            email: None,
            hub: Some(hub.into()),
            access_token: "fixture-access".into(),
            refresh_token: "fixture-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        agit::infra::credentials::save_at(
            &device.home.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(hub).unwrap()
            )),
            &credential,
        )
        .unwrap();
        device
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .env_clear()
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", &self.hub)
            .env("HOME", self._root.path())
            .env("USERPROFILE", self._root.path())
            .env("CI", "1")
            .env("NO_COLOR", "1");
        for name in [
            "PATH",
            "SystemRoot",
            "WINDIR",
            "ComSpec",
            "PATHEXT",
            "TEMP",
            "TMP",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }

    fn sync(&self) {
        let output = self
            .command()
            .args(["__privacy-sync-v1", &self.hub])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "privacy synchronization failed: {output:?}"
        );
    }

    fn worker(&self) -> Worker {
        Worker(
            self.command()
                .arg("__privacy-worker-v1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
}

struct Worker(Child);
impl Worker {
    fn call(&mut self, mode: &str, text: &str) -> Value {
        let bytes = serde_json::to_vec(&json!({"repo":null,"mode":mode,"text":text})).unwrap();
        let input = self.0.stdin.as_mut().unwrap();
        input
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        input.write_all(&bytes).unwrap();
        input.flush().unwrap();
        let output = self.0.stdout.as_mut().unwrap();
        let mut size = [0; 4];
        output.read_exact(&mut size).unwrap();
        let size = u32::from_be_bytes(size) as usize;
        assert!(size < 1024 * 1024);
        let mut bytes = vec![0; size];
        output.read_exact(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn journals(home: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(home.join("privacy"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() == "journal.sqlite")
        .map(|entry| entry.into_path())
        .collect()
}

/// Separate homes must recover every opaque alias after an interrupted upload. A worker that
/// keeps originals only in memory, sends plaintext, or loses retry state cannot satisfy this flow.
#[test]
fn independent_devices_recover_durable_aliases_after_cloud_failure() {
    let hub = Hub::new();
    let first = Device::new(&hub.url);
    let second = Device::new(&hub.url);
    let secret = "AKIA2E7YQXK4NMZ5VJ3T";
    let input =
        json!({"type":"assistant","data":{"api_key":secret,"text":"quoted \"line\"\nfixture"}})
            .to_string()
            + "\n";
    let mut worker = first.worker();
    let protected = worker.call("protect_jsonl", &input);
    assert!(protected["replacements"].as_u64().unwrap() > 0);
    let protected = protected["content"].as_str().unwrap().to_owned();
    assert!(!protected.contains(secret));
    drop(worker);
    let independent = second.worker().call("protect_jsonl", &input);
    let independent = independent["content"].as_str().unwrap().to_owned();
    assert_ne!(protected, independent);
    assert!(hub.state.lock().unwrap().packages.is_empty());
    assert!(journals(&first.home).iter().any(|path| {
        let db = rusqlite::Connection::open(path).unwrap();
        db.query_row(
            "SELECT COUNT(*) FROM packages WHERE encrypted=0",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    }));
    hub.state.lock().unwrap().reject_upload = true;
    first.sync();
    assert!(hub.state.lock().unwrap().packages.is_empty());
    hub.state.lock().unwrap().reject_upload = false;
    first.sync();
    let count = hub.state.lock().unwrap().packages.len();
    assert!(count > 0);
    first.sync();
    assert_eq!(hub.state.lock().unwrap().packages.len(), count);
    second.sync();
    assert!(hub.state.lock().unwrap().packages.len() > count);
    first.sync();
    let alias = first.worker().call("hydrate_jsonl", &independent);
    assert_eq!(
        serde_json::from_str::<Value>(alias["content"].as_str().unwrap()).unwrap(),
        serde_json::from_str::<Value>(&input).unwrap()
    );

    let hydrated = second.worker().call("hydrate_jsonl", &protected);
    assert_eq!(
        serde_json::from_str::<Value>(hydrated["content"].as_str().unwrap()).unwrap(),
        serde_json::from_str::<Value>(&input).unwrap()
    );
    // A local storage fault returns unchanged content while the worker remains usable.
    std::fs::rename(
        second.home.join("privacy"),
        second.home.join("saved-privacy"),
    )
    .unwrap();
    std::fs::write(second.home.join("privacy"), b"unavailable directory").unwrap();
    let output = second.worker().call("protect_text", secret);
    assert_eq!(output["status"], "skipped");
    assert_eq!(output["content"], secret);
}
