//! Identity reports and authenticated checks use the same selected Hub and credential snapshot.

use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const FUTURE: &str = "2099-01-01T00:00:00Z";
const PAST: &str = "2000-01-01T00:00:00Z";

#[derive(Clone, Debug)]
struct Request {
    path: String,
    authorization: Option<String>,
}

struct Hub {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Hub {
    fn new(account: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = stop.clone();
        let recorded = requests.clone();
        let account = account.to_string();
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &account, &recorded),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => panic!("fixture listener failed: {error}"),
                }
            }
        });
        Self {
            base,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
            && !std::thread::panicking()
        {
            panic!("fixture server failed");
        }
    }
}

fn serve(mut stream: TcpStream, account: &str, requests: &Mutex<Vec<Request>>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            return;
        }
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= 65536, "fixture headers exceed their bound");
        if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
            break;
        }
    }
    let header = String::from_utf8(bytes).unwrap();
    let path = header
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
    let authorization = header
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.trim().to_string());
    requests.lock().unwrap().push(Request {
        path: path.clone(),
        authorization: authorization.clone(),
    });
    let authenticated =
        authorization.as_deref() == Some(format!("Bearer synthetic-{account}-access").as_str());
    let (status, body) = if path == "/api/auth/me" && authenticated {
        (200, json!({"username":account,"email":null}))
    } else {
        (
            401,
            json!({"error":"synthetic rejection","kind":"unauthorized"}),
        )
    };
    let body = serde_json::to_vec(&body).unwrap();
    write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
    stream.write_all(&body).unwrap();
}

struct Lab {
    _root: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
    git_config: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let store = root.path().join("store");
        let work = root.path().join("work");
        for path in [&home, &store, &work] {
            fs::create_dir_all(path).unwrap();
        }
        fs::create_dir_all(store.join("credentials")).unwrap();
        fs::write(store.join("layout-v1.complete"), b"1\n").unwrap();
        let git_config = root.path().join("gitconfig");
        fs::write(&git_config, b"").unwrap();
        Self {
            _root: root,
            home,
            store,
            work,
            git_config,
        }
    }

    fn credential_path(&self, hub: &str) -> PathBuf {
        self.store.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(hub).unwrap()
        ))
    }

    fn save(&self, hub: &str, account: &str, access: &str, refresh: &str) {
        agit::infra::credentials::save_at(
            &self.credential_path(hub),
            &agit::infra::credentials::HubCredential {
                username: account.into(),
                email: None,
                hub: Some(hub.into()),
                access_token: format!("synthetic-{account}-access"),
                refresh_token: format!("synthetic-{account}-refresh"),
                access_expires_at: access.into(),
                refresh_expires_at: refresh.into(),
            },
        )
        .unwrap();
    }

    fn run(&self, hub: Option<&str>, json: bool, check: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        if json {
            command.arg("--json");
        }
        command.arg("whoami");
        if check {
            command.arg("--check");
        }
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_SECRETS_KEYSTORE", "file")
            .env("GIT_CONFIG_GLOBAL", &self.git_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("AGIT_TUI", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .current_dir(&self.work);
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        if let Some(hub) = hub {
            command.env("AGIT_HUB_URL", hub);
        }
        command.output().unwrap()
    }
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("whoami JSON must remain one document")
}

fn output_modes() -> impl Iterator<Item = bool> {
    [false, true]
        .into_iter()
        .filter(|json| !*json || cfg!(unix))
}

#[test]
fn malformed_expiry_is_explicit_in_human_and_json_reports() {
    let lab = Lab::new();
    let hub = "http://127.0.0.1:1";
    for expiry in ["é".repeat(10), String::new(), "invalid\n\u{1b}[31m".into()] {
        lab.save(hub, "alice", &expiry, FUTURE);
        let human = lab.run(Some(hub), false, false);
        assert!(human.status.success(), "{human:?}");
        let text = String::from_utf8(human.stdout).unwrap();
        assert!(text.contains("access invalid expiry"));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains("synthetic-alice-access"));
        if cfg!(unix) {
            let structured = lab.run(Some(hub), true, false);
            assert!(structured.status.success(), "{structured:?}");
            let doc = report(&structured);
            assert_eq!(doc["schema"], "cli-output");
            assert_eq!(
                doc["result"]["value"]["tokens"]["access"]["state"],
                "invalid"
            );
            assert_eq!(
                doc["result"]["value"]["tokens"]["access"]["expires_at"],
                expiry
            );
            assert_eq!(
                doc["result"]["value"]["tokens"]["refresh"]["state"],
                "valid"
            );
            assert!(
                !String::from_utf8_lossy(&structured.stdout).contains("synthetic-alice-access")
            );
        }
    }
}

#[test]
fn invalid_hub_errors_do_not_echo_sensitive_url_parts() {
    let lab = Lab::new();
    for hub in [
        "http://private-user:private-password@127.0.0.1:1",
        "http://127.0.0.1:1?private-query",
        "http://127.0.0.1:1#private-fragment",
    ] {
        for json in output_modes() {
            let output = lab.run(Some(hub), json, true);
            assert_eq!(output.status.code(), Some(5));
            assert!(!String::from_utf8_lossy(&output.stdout).contains("private-"));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("private-"));
        }
    }
}

#[test]
fn known_expired_refresh_stays_local_but_malformed_refresh_still_checks_authentication() {
    let lab = Lab::new();
    let hub = Hub::new("alice");
    lab.save(&hub.base, "alice", FUTURE, PAST);
    for json in output_modes() {
        let offline = lab.run(Some(&hub.base), json, false);
        assert!(offline.status.success());
        let checked = lab.run(Some(&hub.base), json, true);
        assert_eq!(checked.status.code(), Some(5));
        if json {
            let doc = report(&checked);
            let value = &doc["result"]["value"];
            assert_eq!(value["tokens"]["refresh"]["state"], "expired");
            assert_eq!(value["check"]["requested"], true);
            assert!(value["check"]["server_reachable"].is_null());
            assert!(value["check"]["authenticated"].is_null());
        }
    }
    assert!(hub.requests().is_empty());
    lab.save(&hub.base, "alice", FUTURE, &"é".repeat(10));
    for json in output_modes() {
        let checked = lab.run(Some(&hub.base), json, true);
        assert!(checked.status.success(), "{checked:?}");
        if json {
            let doc = report(&checked);
            assert_eq!(
                doc["result"]["value"]["tokens"]["refresh"]["state"],
                "invalid"
            );
            assert_eq!(doc["result"]["value"]["check"]["authenticated"], true);
        }
    }
    let requests = hub.requests();
    assert_eq!(requests.len(), output_modes().count());
    assert!(
        requests
            .iter()
            .all(|request| request.path == "/api/auth/me")
    );
}

#[test]
fn missing_or_unbound_credentials_never_become_anonymous_checks() {
    let lab = Lab::new();
    let hub = Hub::new("alice");
    let path = lab.credential_path(&hub.base);
    let mut invalid = json!({"username":"alice","hub":"http://other.example.test","access_token":"synthetic-alice-access","refresh_token":"synthetic-alice-refresh","access_expires_at":FUTURE,"refresh_expires_at":FUTURE});
    let foreign = serde_json::to_vec(&invalid).unwrap();
    invalid.as_object_mut().unwrap().remove("hub");
    for bytes in [
        None,
        Some(b"invalid json".to_vec()),
        Some(foreign),
        Some(serde_json::to_vec(&invalid).unwrap()),
    ] {
        if let Some(bytes) = &bytes {
            fs::write(&path, bytes).unwrap();
        }
        for json in output_modes() {
            let output = lab.run(Some(&hub.base), json, true);
            assert_eq!(output.status.code(), Some(5), "{output:?}");
            if json {
                assert_eq!(report(&output)["exit_code"], 5);
            }
        }
        assert!(hub.requests().is_empty());
        if let Some(bytes) = bytes {
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }
}

struct ConfigSwitcher {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ConfigSwitcher {
    fn new(store: PathBuf, hubs: [String; 2]) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            let temporary = store.join("config-pending.json");
            let config = store.join("config.json");
            let mut index = 0;
            while !stopped.load(Ordering::Acquire) {
                fs::write(
                    &temporary,
                    serde_json::to_vec(&json!({"hub.url":hubs[index]})).unwrap(),
                )
                .unwrap();
                fs::rename(&temporary, &config).unwrap();
                index = 1 - index;
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for ConfigSwitcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
            && !std::thread::panicking()
        {
            panic!("fixture config writer failed");
        }
    }
}

#[test]
fn reports_and_online_checks_keep_the_same_hub_while_selection_changes() {
    let lab = Lab::new();
    let alice = Hub::new("alice");
    let bob = Hub::new("bob");
    lab.save(&alice.base, "alice", FUTURE, FUTURE);
    lab.save(&bob.base, "bob", FUTURE, FUTURE);
    fs::write(
        lab.store.join("config.json"),
        serde_json::to_vec(&json!({"hub.url":alice.base})).unwrap(),
    )
    .unwrap();
    let _switcher = ConfigSwitcher::new(lab.store.clone(), [alice.base.clone(), bob.base.clone()]);
    for iteration in 0..80 {
        let structured = cfg!(unix) && iteration % 2 == 0;
        let checked = iteration % 3 == 0;
        let before = [alice.requests().len(), bob.requests().len()];
        let output = lab.run(None, structured, checked);
        assert!(output.status.success(), "{output:?}");
        let (selected, account) = if structured {
            let doc = report(&output);
            let value = &doc["result"]["value"];
            if checked {
                assert_eq!(value["check"]["authenticated"], true);
            }
            (
                value["hub"].as_str().unwrap().to_string(),
                value["account"].as_str().unwrap().to_string(),
            )
        } else {
            let text = String::from_utf8(output.stdout).unwrap();
            let read = |name: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(name))
                    .unwrap()
                    .trim()
                    .to_string()
            };
            (read("hub   "), read("account  "))
        };
        let chosen = if selected == alice.base {
            0
        } else {
            assert_eq!(selected, bob.base);
            1
        };
        assert_eq!(account, if chosen == 0 { "alice" } else { "bob" });
        let requests = [alice.requests(), bob.requests()];
        for (index, records) in requests.iter().enumerate() {
            assert_eq!(
                records.len() - before[index],
                usize::from(checked && index == chosen)
            );
            if records.len() > before[index] {
                let request = records.last().unwrap();
                assert_eq!(request.path, "/api/auth/me");
                assert_eq!(
                    request.authorization.as_deref(),
                    Some(format!("Bearer synthetic-{account}-access").as_str())
                );
            }
        }
    }
}
