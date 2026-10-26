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
    /// Status, content type and body answered to every package upload.
    refuse_upload: Option<(u16, &'static str, &'static str)>,
    upload_attempts: usize,
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
                if !header
                    .to_ascii_lowercase()
                    .contains("authorization: bearer fixture-access")
                {
                    // A signed-out device's requests carry no account, and the Hub refuses them.
                    let _ = socket.write_all(
                        b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    );
                    continue;
                }
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
                let mut refusal = None;
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
                        cloud.upload_attempts += 1;
                        if let Some((status, kind, body)) = cloud.refuse_upload {
                            refusal = Some((kind, body));
                            (status, Value::Null)
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
                let (kind, bytes) = match refusal {
                    Some((kind, body)) => (kind, body.as_bytes().to_vec()),
                    None => ("application/json", serde_json::to_vec(&value).unwrap()),
                };
                write!(socket,"HTTP/1.1 {status} Fixture\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).unwrap();
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
        let device = Self::signed_out(hub);
        device.sign_in(None);
        device
    }

    fn signed_out(hub: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        Self {
            _root: root,
            home,
            hub: hub.into(),
        }
    }

    /// Save the credential a login saves; a real login's carries the account id.
    fn sign_in(&self, account_id: Option<&str>) {
        let hub = &self.hub;
        let credential = agit::infra::credentials::HubCredential {
            account_id: account_id.map(str::to_owned),
            username: "fixture".into(),
            email: None,
            hub: Some(hub.into()),
            access_token: "fixture-access".into(),
            refresh_token: "fixture-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        agit::infra::credentials::save_at(
            &self.home.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(hub).unwrap()
            )),
            &credential,
        )
        .unwrap();
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

    fn doctor(&self) -> String {
        let output = self.command().arg("doctor").output().unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr)
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
    /// `text` protected once the journal is free: a worker that starts while the previous
    /// command's worker is still closing the journal answers `store_busy`, and like any caller
    /// the test asks again.
    fn protect(&mut self, text: &str) -> Value {
        for _ in 0..50 {
            let outcome = self.call("protect_text", text);
            if outcome["reason"] != "store_busy" {
                return outcome;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the privacy journal stayed busy");
    }

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

/// A value registered on this device stays protected whoever is signed in. Registered while signed
/// out, it is protected from the first request after signing in, before synchronization has
/// carried anything to the Hub, and its placeholder is restored; after signing out, or in to
/// another account, it is still protected; and a value another process registers for another Hub
/// is protected by a worker that was already running. A worker that reads only the journal the
/// current credential selects passes the registered value through as Complete on every switch, and
/// one that reloads only when its own journal changes keeps passing the later value through.
#[test]
fn a_rule_registered_on_this_device_protects_under_any_sign_in() {
    let hub = Hub::new();
    let device = Device::signed_out(&hub.url);
    let value = "purple-otter-meadow-7731";
    let mut add = device
        .command()
        .args(["secrets", "add", "otter", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    add.stdin
        .take()
        .unwrap()
        .write_all(value.as_bytes())
        .unwrap();
    let added = add.wait_with_output().unwrap();
    assert!(added.status.success(), "{added:?}");
    let text = format!("the value is {value}");
    let signed_out = device.worker().protect(&text);
    let placeholder = signed_out["content"].as_str().unwrap().to_owned();
    assert!(!placeholder.contains(value), "{signed_out}");

    device.sign_in(Some("fixture-account"));
    let mut worker = device.worker();
    let signed_in = worker.protect(&text);
    assert_eq!(signed_in["status"], "complete");
    assert_eq!(signed_in["content"], signed_out["content"]);
    assert_eq!(worker.call("hydrate_text", &placeholder)["content"], text);
    drop(worker);

    device.sync();
    assert_eq!(
        device.worker().protect(&text)["content"],
        signed_out["content"]
    );

    for account in [None, Some("another-account")] {
        match account {
            None => std::fs::remove_dir_all(device.home.join("credentials")).unwrap(),
            Some(_) => device.sign_in(account),
        }
        let switched = device.worker().protect(&text);
        assert_eq!(switched["status"], "complete");
        assert!(
            !switched["content"].as_str().unwrap().contains(value),
            "{account:?}: {switched}"
        );
    }

    // A worker that keeps running sees a rule another process registers for another Hub.
    let later = "amber-heron-lake-2048";
    let mut running = device.worker();
    let text = format!("the value is {later}");
    assert_eq!(running.protect(&text)["content"], text);
    let mut add = device
        .command()
        .env("AGIT_HUB_URL", "http://127.0.0.1:9")
        .args(["secrets", "add", "heron", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    add.stdin
        .take()
        .unwrap()
        .write_all(later.as_bytes())
        .unwrap();
    let added = add.wait_with_output().unwrap();
    assert!(added.status.success(), "{added:?}");
    let checked = running.protect(&text);
    assert_eq!(checked["status"], "complete");
    assert!(
        !checked["content"].as_str().unwrap().contains(later),
        "{checked}"
    );
}

/// A journal of rules this device registered under another account, which the device can no
/// longer read, leaves every check incomplete with the reason `device_rules`, and `agit doctor`
/// names the account to sign in to; journals that cannot even be listed do the same, and doctor
/// warns instead of reporting every rule readable. A worker that skipped the unreadable journal
/// would pass the value it blocks through as Complete; one that reported the generic reason would
/// leave the user no way to tell what to fix; a doctor that read a failed listing as an empty one
/// would contradict the withheld content.
#[test]
fn an_unreadable_journal_of_rules_withholds_and_names_its_account() {
    let hub = Hub::new();
    let device = Device::signed_out(&hub.url);
    device.sign_in(Some("fixture-account"));
    let value = "copper-finch-harbor-5520";
    let mut add = device
        .command()
        // Signed in, the CLI would also ask the Hub for its latest version; that is not this test.
        .env("AGIT_QUIET", "1")
        .args(["secrets", "add", "finch", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    add.stdin
        .take()
        .unwrap()
        .write_all(value.as_bytes())
        .unwrap();
    let added = add.wait_with_output().unwrap();
    assert!(added.status.success(), "{added:?}");
    // Synchronization encrypts the rule under the account's key and caches the key.
    device.sync();
    device.sign_in(Some("another-account"));
    let text = format!("the value is {value}");
    let readable = device.worker().protect(&text);
    assert_eq!(readable["status"], "complete");
    assert!(!readable["content"].as_str().unwrap().contains(value));

    // The key that rule is encrypted under is no longer on this device.
    for account in std::fs::read_dir(device.home.join("privacy/accounts")).unwrap() {
        let keys = account.unwrap().path().join("keys");
        if keys.exists() {
            std::fs::remove_dir_all(keys).unwrap();
        }
    }
    let unreadable = device.worker().protect(&text);
    assert_ne!(unreadable["status"], "complete", "{unreadable}");
    assert_eq!(unreadable["reason"], "device_rules", "{unreadable}");
    let doctor = device.doctor();
    assert!(doctor.contains("fixture-account"), "{doctor}");
    assert!(doctor.contains("agit login"), "{doctor}");

    // Journals that cannot even be listed hold content back too, and doctor says so rather than
    // reporting every rule readable.
    // An entry where an account directory belongs, which is a plain file.
    std::fs::write(
        device.home.join("privacy/accounts/unlistable"),
        "not a directory",
    )
    .unwrap();
    let unlisted = device.worker().protect(&text);
    assert_ne!(unlisted["status"], "complete", "{unlisted}");
    assert_eq!(unlisted["reason"], "device_rules", "{unlisted}");
    let doctor = device.doctor();
    assert!(doctor.contains("cannot be read"), "{doctor}");
    assert!(
        !doctor.contains("every rule registered on this computer can be read"),
        "{doctor}"
    );
}

/// Inspecting a device writes no privacy state: `agit doctor` starts the privacy worker to ask its
/// status, and a device that has no journal yet still has none afterwards. A worker that opened
/// its journal at start-up would create one, so a read-only command would change the device.
#[test]
fn read_only_inspection_creates_no_privacy_journal() {
    let hub = Hub::new();
    let device = Device::new(&hub.url);
    let doctor = device.doctor();
    assert!(doctor.contains("secret keystore"), "{doctor}");
    assert!(
        journals(&device.home).is_empty(),
        "{:?}",
        journals(&device.home)
    );
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
    // Publication batches retain one dictionary even when another worker adds mappings.
    let mut frozen = first.worker();
    assert_eq!(
        frozen.call("freeze_hydration", "false")["status"],
        "complete"
    );
    let before = frozen.call("hydrate_frozen_jsonl", &protected);
    assert_eq!(before["status"], "complete");
    assert_eq!(
        serde_json::from_str::<Value>(before["content"].as_str().unwrap()).unwrap(),
        serde_json::from_str::<Value>(&input).unwrap()
    );
    let next_input =
        json!({"type":"assistant","data":{"api_key":"AKIA5Z7YQXK4NMZ5VJ3T"}}).to_string() + "\n";
    let next = first.worker().call("protect_jsonl", &next_input);
    assert!(next["replacements"].as_u64().unwrap() > 0);
    let missing = frozen.call("hydrate_frozen_jsonl", next["content"].as_str().unwrap());
    assert_eq!(missing["status"], "partial");
    assert!(missing["unresolved"].as_u64().unwrap() > 0);
    assert_eq!(
        frozen.call("hydrate_frozen_jsonl", &protected)["content"],
        before["content"]
    );
    let values = frozen.call("hydration_values", "");
    assert!(
        serde_json::from_str::<Vec<String>>(values["content"].as_str().unwrap())
            .unwrap()
            .iter()
            .any(|value| value == secret)
    );
    assert_eq!(
        frozen.call("freeze_hydration", "false")["status"],
        "complete"
    );
    assert_eq!(
        frozen.call("hydrate_frozen_jsonl", next["content"].as_str().unwrap())["status"],
        "complete"
    );
    drop(frozen);
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
    hub.state.lock().unwrap().refuse_upload = Some((
        503,
        "application/json",
        r#"{"error":"dictionary temporarily unavailable"}"#,
    ));
    first.sync();
    assert!(hub.state.lock().unwrap().packages.is_empty());
    hub.state.lock().unwrap().refuse_upload = None;
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

/// A refusal that an immediate retry would meet again is paced and reported by cause. A sync
/// that treats it like a transient failure re-sends the outbox on every run; one that reports
/// every 403 as an account problem sends the user to `agit login` for a block that no sign-in
/// can lift.
#[test]
fn refused_dictionary_uploads_back_off_and_name_their_cause() {
    let hub = Hub::new();
    let input =
        json!({"type":"assistant","data":{"api_key":"AKIA2E7YQXK4NMZ5VJ3T"}}).to_string() + "\n";
    let edge = Device::new(&hub.url);
    let account = Device::new(&hub.url);
    for device in [&edge, &account] {
        let protected = device.worker().call("protect_jsonl", &input);
        assert!(protected["replacements"].as_u64().unwrap() > 0);
    }

    hub.state.lock().unwrap().refuse_upload = Some((
        403,
        "text/html",
        "<html><body><h1>403 Forbidden</h1></body></html>",
    ));
    edge.sync();
    let attempts = hub.state.lock().unwrap().upload_attempts;
    assert!(attempts > 0);
    edge.sync();
    assert_eq!(hub.state.lock().unwrap().upload_attempts, attempts);
    let report = edge.doctor();
    assert!(
        report.contains("blocked before it reached the Hub"),
        "{report}"
    );
    assert!(!report.contains("agit login`, then push"), "{report}");

    hub.state.lock().unwrap().refuse_upload = Some((
        403,
        "application/json",
        r#"{"error":"privacy storage is disabled for this account","kind":"forbidden"}"#,
    ));
    account.sync();
    let report = account.doctor();
    assert!(
        report.contains("privacy storage is disabled for this account"),
        "{report}"
    );
    assert!(report.contains("agit login`, then push"), "{report}");
    assert!(hub.state.lock().unwrap().packages.is_empty());
}
