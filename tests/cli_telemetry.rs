#![cfg(feature = "cli")]

use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_agit");

struct Fixture {
    home: tempfile::TempDir,
    hub: String,
    sink: String,
}
impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            hub: "http://127.0.0.1:9".into(),
            sink: "http://127.0.0.1:9".into(),
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .current_dir(self.home.path())
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("AGIT_HOME", self.home.path().join("agit"))
            .env("AGIT_HUB_URL", &self.hub)
            .env("AGIT_TELEMETRY_HOST", &self.sink)
            .env("AGIT_TELEMETRY_KEY", "synthetic_project")
            .env("CODEX_HOME", self.home.path().join("codex"))
            .env("CLAUDE_CONFIG_DIR", self.home.path().join("claude"))
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("CI", "1");
        for name in [
            "AGIT_TELEMETRY_DEFER",
            "AGIT_TELEMETRY_DISABLED",
            "DO_NOT_TRACK",
            "AGIT_TELEMETRY_DEBUG",
            "AGIT_PROTOCOL_CHILD",
            "AGIT_SESSION",
            "AGIT_RC",
            "AGIT_MCP_TOOL",
            "AGIT_TELEMETRY_PARENT_ID",
            "AGIT_INSTALL_CHANNEL",
            "AGIT_ACQUISITION_ID",
            "AGIT_CAMPAIGN_URL",
            "AGIT_YES",
        ] {
            command.env_remove(name);
        }
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn enable(&self) {
        assert!(self.run(&["telemetry", "enable"]).status.success());
    }
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.home.path().join("agit/telemetry").join(name)
    }
    fn queue(&self) -> Value {
        std::fs::read(self.path("queue.json"))
            .ok()
            .and_then(|body| serde_json::from_slice(&body).ok())
            .unwrap_or(json!({"entries":[]}))
    }
    fn events(&self) -> Vec<Value> {
        self.queue()["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["event"].clone())
            .collect()
    }
    fn login(&self, account: Option<&str>) {
        let path = self.home.path().join("agit/credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(&self.hub).unwrap()
        ));
        agit::infra::credentials::save_at(
            &path,
            &agit::infra::credentials::HubCredential {
                account_id: account.map(str::to_owned),
                username: "private-username-canary".into(),
                email: Some("private-email-canary@example.test".into()),
                hub: Some(self.hub.clone()),
                access_token: "private-access-canary".into(),
                refresh_token: "private-refresh-canary".into(),
                access_expires_at: "2090-01-01T00:00:00Z".into(),
                refresh_expires_at: "2090-02-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    }
}

fn receive(mut stream: TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 8192);
    }
    let header = String::from_utf8(bytes).unwrap();
    let size = header
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0; size];
    stream.read_exact(&mut body).unwrap();
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"status\":1}").unwrap();
    assert!(!header.to_ascii_lowercase().contains("authorization:"));
    String::from_utf8(body).unwrap()
}

#[test]
fn maintenance_setup_does_not_establish_a_telemetry_preference() {
    let f = Fixture::new();
    let output = f.run(&["setup", "--skill", "--installed-only", "--quiet"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(!f.path("preferences.json").exists());
    assert!(!f.path("queue.json").exists());
}

#[test]
fn quiet_setup_discloses_the_first_choice_and_suppresses_repeated_notices() {
    let mut f = Fixture::new();
    f.sink.clear();
    let args = ["setup", "--skill", "--runtime", "codex", "--yes", "--quiet"];
    let first = f.run(&args);
    assert!(first.status.success(), "{first:?}");
    assert!(first.stdout.is_empty(), "{first:?}");
    let notice = String::from_utf8(first.stderr).unwrap();
    assert!(notice.contains("PostHog"));
    assert!(notice.contains("Usage statistics are enabled"));
    assert!(notice.contains("agit telemetry disable"));
    let preferences = std::fs::read(f.path("preferences.json")).unwrap();
    let repeated = f.run(&args);
    assert!(repeated.status.success(), "{repeated:?}");
    assert!(repeated.stdout.is_empty(), "{repeated:?}");
    assert!(repeated.stderr.is_empty(), "{repeated:?}");
    assert_eq!(
        std::fs::read(f.path("preferences.json")).unwrap(),
        preferences
    );
}

#[test]
fn local_commands_keep_json_contract_and_capture_only_classified_values() {
    let sink = TcpListener::bind("127.0.0.1:0").unwrap();
    sink.set_nonblocking(true).unwrap();
    let mut f = Fixture::new();
    f.sink = format!("http://{}", sink.local_addr().unwrap());
    f.enable();
    f.login(Some("account-alice"));
    let output = f
        .command()
        .args(["status", "--json"])
        .env("AGIT_SESSION_ID", "private-native-canary")
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ok"], true);
    let invalid = f.run(&[
        "search",
        "private-query-canary",
        "--local",
        "--tool",
        "private-tool-canary",
        "--limit",
        "5",
        "--json",
    ]);
    assert!(serde_json::from_slice::<Value>(&invalid.stdout).is_ok());
    let entries = f.events();
    let text = serde_json::to_string(&entries).unwrap();
    for canary in [
        "private-native-canary",
        "private-query-canary",
        "private-tool-canary",
        "private-username-canary",
        "private-email-canary",
        "private-access-canary",
        "private-refresh-canary",
    ] {
        assert!(!text.contains(canary), "leaked {canary}");
    }
    let event = entries
        .iter()
        .find(|event| {
            event["event"] == "cli_command_finished" && event["properties"]["command"] == "status"
        })
        .unwrap();
    assert_eq!(event["properties"]["user_id"], "account-alice");
    assert_eq!(event["properties"]["agit_session_id_env_present"], true);
    assert_eq!(event["properties"]["agit_session_env_present"], false);
    assert_eq!(event["properties"]["ci"], true);
    assert!(matches!(sink.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
}

#[test]
fn disable_overrides_debug_purges_events_and_stops_a_running_protocol_parent() {
    let f = Fixture::new();
    f.enable();
    f.run(&["status"]);
    assert!(!f.events().is_empty());
    let mut child = f
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"id\":1,\"method\":\"initialize\"}\n")
        .unwrap();
    let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
    assert!(f.run(&["telemetry", "disable"]).status.success());
    child.stdin.take();
    assert!(child.wait().unwrap().success());
    assert!(!f.path("queue.json").exists());
    let output = f
        .command()
        .args(["status", "--json"])
        .env("AGIT_TELEMETRY_DEBUG", "1")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stderr).contains("telemetry_preview"));
    assert!(!f.path("queue.json").exists());
    f.run(&["--internal-telemetry-flush"]);
    assert!(!f.path("queue.json").exists());
}

#[test]
fn overrides_and_self_hosted_destinations_fail_closed() {
    let f = Fixture::new();
    f.run(&["--version"]);
    f.run(&["--private-canary"]);
    f.run(&["telemetry", "status"]);
    assert!(!f.path("preferences.json").exists());
    f.enable();
    for (name, value) in [
        ("DO_NOT_TRACK", "1"),
        ("AGIT_TELEMETRY_DISABLED", "invalid"),
        ("AGIT_TELEMETRY_DEFER", "1"),
    ] {
        let out = f
            .command()
            .args(["status"])
            .env(name, value)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(f.events().is_empty());
    }
    let out = f
        .command()
        .arg("status")
        .env_remove("AGIT_TELEMETRY_HOST")
        .env_remove("AGIT_TELEMETRY_KEY")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(f.events().is_empty());
    let preview = f.run(&[
        "telemetry",
        "preview",
        "--",
        "commit",
        "private-repo-canary",
        "--milestone",
        "private-message-canary",
    ]);
    assert!(preview.status.success());
    assert!(!String::from_utf8_lossy(&preview.stdout).contains("canary"));
    assert!(f.events().is_empty());
}

#[test]
fn account_switches_rotate_activity_without_rewriting_old_events() {
    let f = Fixture::new();
    f.enable();
    f.login(Some("account-alice"));
    f.run(&["status"]);
    let old = f
        .events()
        .into_iter()
        .find(|event| event["event"] == "cli_command_finished")
        .unwrap();
    f.login(Some("account-bob"));
    f.run(&["status"]);
    let all = f.events();
    let new = all
        .iter()
        .rev()
        .find(|event| event["event"] == "cli_command_finished")
        .unwrap();
    assert_eq!(old["properties"]["user_id"], "account-alice");
    assert_eq!(new["properties"]["user_id"], "account-bob");
    assert_ne!(
        old["properties"]["device_id"],
        new["properties"]["device_id"]
    );
    assert_ne!(
        old["properties"]["session_id"],
        new["properties"]["session_id"]
    );
    assert!(
        all.iter()
            .any(|event| event["properties"]["user_id"] == "account-alice")
    );
}

#[test]
fn explicit_flush_uses_the_posthog_envelope_without_hub_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut f = Fixture::new();
    f.sink = format!("http://{}", listener.local_addr().unwrap());
    f.enable();
    f.login(Some("account-alice"));
    f.run(&["status"]);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        tx.send(receive(stream)).unwrap();
    });
    assert!(f.run(&["--internal-telemetry-flush"]).status.success());
    let raw = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let body: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(body["api_key"], "synthetic_project");
    for event in body["batch"].as_array().unwrap() {
        assert!(uuid::Uuid::parse_str(event["uuid"].as_str().unwrap()).is_ok());
        assert_eq!(event["uuid"], event["properties"]["event_id"]);
        assert!(chrono::DateTime::parse_from_rfc3339(event["timestamp"].as_str().unwrap()).is_ok());
        assert_eq!(event["properties"]["$geoip_disable"], true);
        assert_eq!(event["properties"]["user_id"], "account-alice");
        assert_eq!(event["distinct_id"], "development:account-alice");
    }
    assert!(!raw.contains("private-access-canary"));
    assert!(!raw.contains("private-refresh-canary"));
    assert!(f.events().is_empty());
}

#[test]
fn ordinary_online_command_launches_a_bounded_sender_after_its_final_event() {
    let sink = TcpListener::bind("127.0.0.1:0").unwrap();
    let hub = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut f = Fixture::new();
    f.sink = format!("http://{}", sink.local_addr().unwrap());
    f.hub = format!("http://{}", hub.local_addr().unwrap());
    f.enable();
    f.login(None);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = sink.accept().unwrap();
        tx.send(receive(stream)).unwrap();
    });
    std::thread::spawn(move || {
        let (mut stream, _) = hub.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        let body =
            json!({"username":"private-username-canary","account_id":"account-alice"}).to_string();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    let start = Instant::now();
    let result = f.run(&["whoami", "--check", "--json"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    let raw = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let body: Value = serde_json::from_str(&raw).unwrap();
    let final_event = body["batch"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event"] == "cli_command_finished")
        .unwrap();
    assert_eq!(final_event["properties"]["user_id"], "account-alice");
    assert_eq!(final_event["properties"]["exit_category"], "ok");
}

#[test]
fn an_inflight_identity_check_cannot_attribute_its_completion_to_another_account() {
    let hub = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut f = Fixture::new();
    f.hub = format!("http://{}", hub.local_addr().unwrap());
    f.enable();
    f.login(Some("account-alice"));
    let mut child = f
        .command()
        .args(["whoami", "--check", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut stream, _) = hub.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    f.login(Some("account-bob"));
    let body =
        json!({"username":"private-username-canary","account_id":"account-alice"}).to_string();
    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(
        f.events()
            .iter()
            .all(|event| event["properties"]["user_id"] != "account-bob")
    );
    assert!(
        !f.events()
            .iter()
            .any(|event| event["event"] == "cli_command_finished")
    );
}

#[test]
fn identity_refresh_cannot_revive_a_collector_after_disable_and_enable() {
    let hub = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut f = Fixture::new();
    f.hub = format!("http://{}", hub.local_addr().unwrap());
    f.enable();
    f.login(None);
    let mut child = f
        .command()
        .args(["whoami", "--check", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut stream, _) = hub.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    assert!(f.run(&["telemetry", "disable"]).status.success());
    f.enable();
    let body =
        json!({"username":"private-username-canary","account_id":"account-alice"}).to_string();
    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(f.events().is_empty());
    assert!(!f.path("activity.json").exists());
}

#[test]
fn hooks_aggregate_without_extending_foreground_activity() {
    let f = Fixture::new();
    f.enable();
    f.run(&["status"]);
    let before: Value =
        serde_json::from_slice(&std::fs::read(f.path("activity.json")).unwrap()).unwrap();
    for _ in 0..4 {
        assert!(f.run(&["hooks", "settle"]).status.success());
    }
    let after: Value =
        serde_json::from_slice(&std::fs::read(f.path("activity.json")).unwrap()).unwrap();
    assert_eq!(before["last_active"], after["last_active"]);
    let summaries = f
        .events()
        .into_iter()
        .filter(|event| event["event"] == "cli_integration_summary")
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0]["properties"]["integration_count"], 4);
}

#[test]
fn failed_upload_retries_preserve_uuid_timestamp_and_occurrence_identity() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut f = Fixture::new();
    f.sink = format!("http://{}", listener.local_addr().unwrap());
    f.enable();
    f.login(Some("account-alice"));
    f.run(&["status"]);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for status in [503, 200] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut header = Vec::new();
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let header = String::from_utf8(header).unwrap();
            let size = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap();
            let mut body = vec![0; size];
            stream.read_exact(&mut body).unwrap();
            write!(
                stream,
                "HTTP/1.1 {status} Result\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            tx.send(body).unwrap();
        }
    });
    f.run(&["--internal-telemetry-flush"]);
    let first: Value =
        serde_json::from_slice(&rx.recv_timeout(Duration::from_secs(3)).unwrap()).unwrap();
    assert!(!f.events().is_empty());
    let mut queue = f.queue();
    assert!(queue["next_send"].as_i64().unwrap() > chrono::Utc::now().timestamp_millis());
    queue["next_send"] = json!(0);
    std::fs::write(f.path("queue.json"), serde_json::to_vec(&queue).unwrap()).unwrap();
    f.login(Some("account-bob"));
    f.run(&["--internal-telemetry-flush"]);
    let second: Value =
        serde_json::from_slice(&rx.recv_timeout(Duration::from_secs(3)).unwrap()).unwrap();
    assert_eq!(first["batch"], second["batch"]);
    assert!(f.events().is_empty());
}

#[test]
fn disable_waits_for_the_upload_gate_and_prevents_any_followup_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut f = Fixture::new();
    f.sink = format!("http://{}", listener.local_addr().unwrap());
    f.enable();
    f.run(&["status"]);
    let (started_tx, started_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut data = [0; 2048];
        let _ = stream.read(&mut data).unwrap();
        started_tx.send(()).unwrap();
        finish_rx.recv_timeout(Duration::from_secs(4)).unwrap();
        drop(stream);
        listener.set_nonblocking(true).unwrap();
        listener
    });
    let mut worker = f
        .command()
        .arg("--internal-telemetry-flush")
        .spawn()
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let start = Instant::now();
    let output = f.run(&["telemetry", "disable"]);
    assert!(output.status.success());
    assert!(start.elapsed() < Duration::from_secs(4));
    assert!(worker.wait().unwrap().success());
    assert!(!f.path("queue.json").exists());
    finish_tx.send(()).unwrap();
    let listener = server.join().unwrap();
    f.run(&["--internal-telemetry-flush"]);
    assert!(matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
}

#[test]
fn verified_install_is_observable_without_setup_or_login_and_is_deduplicated() {
    let f = Fixture::new();
    let acquisition = uuid::Uuid::new_v4().to_string();
    for _ in 0..2 {
        let output = f
            .command()
            .arg("--internal-install-completed")
            .env("AGIT_ACQUISITION_ID", &acquisition)
            .env("AGIT_INSTALL_CHANNEL", "create_agit")
            .output()
            .unwrap();
        assert!(output.status.success());
    }
    let events = f.events();
    let installs: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "cli_install_succeeded")
        .collect();
    assert_eq!(installs.len(), 1);
    let install = installs[0];
    assert_eq!(install["properties"]["acquisition_id"], acquisition);
    assert_eq!(install["properties"]["channel"], "create_agit");
    assert_eq!(install["properties"]["installation_verified"], true);
    assert_eq!(install["properties"]["ci"], true);
    assert!(install["properties"]["user_id"].is_null());
    assert!(
        events
            .iter()
            .all(|e| e["event"] != "cli_acquisition_linked")
    );
    let prefs: Value =
        serde_json::from_slice(&std::fs::read(f.path("preferences.json")).unwrap()).unwrap();
    assert_eq!(install["properties"]["installation_id"], prefs["device_id"]);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["event"] == "cli_install_attributed")
            .count(),
        1
    );
}

#[test]
fn installation_obeys_opt_out_and_reenable_cannot_restore_the_old_acquisition() {
    for optout in [
        "DO_NOT_TRACK",
        "AGIT_TELEMETRY_DISABLED",
        "AGIT_TELEMETRY_DEFER",
    ] {
        let f = Fixture::new();
        f.command()
            .arg("--internal-install-completed")
            .env(optout, "1")
            .output()
            .unwrap();
        assert!(f.events().is_empty());
        assert!(!f.path("preferences.json").exists());
    }
    let f = Fixture::new();
    f.command()
        .arg("--internal-install-completed")
        .env("AGIT_ACQUISITION_ID", uuid::Uuid::new_v4().to_string())
        .output()
        .unwrap();
    assert!(f.run(&["telemetry", "disable"]).status.success());
    f.run(&["--internal-install-completed"]);
    assert!(f.events().is_empty());
    f.enable();
    f.run(&["--internal-install-completed"]);
    assert!(
        f.events()
            .iter()
            .all(|e| e["properties"]["acquisition_id"].is_null())
    );
}

#[test]
fn a_tagged_reinstall_attributes_an_existing_install_without_counting_it_twice() {
    let f = Fixture::new();
    f.run(&["--internal-install-completed"]);
    let id = uuid::Uuid::new_v4().to_string();
    f.command()
        .arg("--internal-install-completed")
        .env("AGIT_ACQUISITION_ID", &id)
        .output()
        .unwrap();
    let events = f.events();
    assert_eq!(
        events
            .iter()
            .filter(|e| e["event"] == "cli_install_succeeded")
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "cli_install_attributed"
                && e["properties"]["acquisition_id"] == id)
    );
}

fn reply_to_login(listener: &TcpListener, child: &mut std::process::Child, body: Value) {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "login did not reach the Hub");
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "login exited before its request"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    let length = String::from_utf8_lossy(&header)
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|s| s.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    stream.read_exact(&mut vec![0; length]).unwrap();
    let body = body.to_string();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
}

#[test]
fn first_browser_handoff_and_completed_login_join_the_install_without_linking_another_account() {
    let hub = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut f = Fixture::new();
    f.hub = format!("http://{}", hub.local_addr().unwrap());
    let acquisition = uuid::Uuid::new_v4().to_string();
    f.command()
        .arg("--internal-install-completed")
        .env("AGIT_ACQUISITION_ID", &acquisition)
        .output()
        .unwrap();
    let install = f
        .events()
        .into_iter()
        .find(|e| e["event"] == "cli_install_succeeded")
        .unwrap();
    let installation = install["properties"]["installation_id"].as_str().unwrap();
    let mut child = f
        .command()
        .args(["login", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    reply_to_login(
        &hub,
        &mut child,
        json!({"state":"private-state-canary", "url":format!("{}/auth/cli?state=private-state-canary", f.hub), "expires_in":300}),
    );
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(8));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains(&format!("installation_id={installation}"))
    );
    assert!(
        !f.events()
            .iter()
            .any(|e| e["event"] == "cli_acquisition_linked")
    );
    for account in ["account-alice", "account-bob"] {
        let mut child = f
            .command()
            .args(["login", "--complete", "private-state-canary"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        reply_to_login(
            &hub,
            &mut child,
            json!({"account_id":account, "username":"private-name-canary", "access_token":"private-access-canary", "refresh_token":"private-refresh-canary", "access_expires_at":"2099-01-01T00:00:00Z", "refresh_expires_at":"2099-02-01T00:00:00Z"}),
        );
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let events = f.events();
    let links: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "cli_acquisition_linked")
        .collect();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["properties"]["installation_id"], installation);
    assert_eq!(links[0]["properties"]["acquisition_id"], acquisition);
    assert_eq!(links[0]["properties"]["user_id"], "account-alice");
    assert!(
        events
            .iter()
            .filter(|e| e["properties"]["user_id"] == "account-bob")
            .all(|e| e["properties"]["installation_id"].is_null())
    );
    assert!(!serde_json::to_string(&events).unwrap().contains("canary"));
}

#[test]
fn official_installation_keys_do_not_cross_into_another_hubs_authorization_url() {
    for configured in [false, true] {
        let mut f = Fixture::new();
        f.hub = "https://agent-git.com".into();
        f.run(&["--internal-install-completed"]);
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        let other = format!("http://{}", hub.local_addr().unwrap());
        let mut command = f.command();
        if !configured {
            command
                .env_remove("AGIT_TELEMETRY_HOST")
                .env_remove("AGIT_TELEMETRY_KEY");
        }
        let mut child = command
            .args(["login", "--hub", &other, "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        reply_to_login(
            &hub,
            &mut child,
            json!({"state":"synthetic-other-hub", "url":format!("{other}/auth/cli?state=synthetic-other-hub"), "expires_in":300}),
        );
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(8));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("installation_id="));
    }
}

#[test]
fn the_first_saved_account_survives_a_busy_collector_and_a_broken_queue() {
    let hub = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut f = Fixture::new();
    f.hub = format!("http://{}", hub.local_addr().unwrap());
    f.run(&["--internal-install-completed"]);
    let gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.path("gate.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&gate).unwrap();
    std::fs::write(f.path("queue.json"), b"invalid queue").unwrap();
    let mut first = None;
    for account in ["account-alice", "account-bob"] {
        let mut child = f
            .command()
            .args(["login", "--complete", "private-state-canary"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        reply_to_login(
            &hub,
            &mut child,
            json!({"account_id":account, "username":"private-name-canary", "access_token":"private-access-canary", "refresh_token":"private-refresh-canary", "access_expires_at":"2099-01-01T00:00:00Z", "refresh_expires_at":"2099-02-01T00:00:00Z"}),
        );
        if first.is_none() {
            assert!(child.try_wait().unwrap().is_none());
            fs2::FileExt::unlock(&gate).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let preferences: Value =
            serde_json::from_slice(&std::fs::read(f.path("preferences.json")).unwrap()).unwrap();
        let owner = preferences["first_acquisition_account"].clone();
        assert_eq!(owner["account_id"], "account-alice");
        if let Some(first) = &first {
            assert_eq!(&owner, first);
        } else {
            first = Some(owner);
        }
        assert_eq!(preferences["acquisition_completed"], false);
    }
    std::fs::remove_file(f.path("queue.json")).unwrap();
    f.run(&["--internal-telemetry-flush"]);
    let linked: Vec<_> = f
        .events()
        .into_iter()
        .filter(|e| e["event"] == "cli_acquisition_linked")
        .collect();
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0]["properties"]["user_id"], "account-alice");
    assert_eq!(linked[0]["uuid"], first.as_ref().unwrap()["event_id"]);
    assert_eq!(linked[0]["timestamp"], first.as_ref().unwrap()["saved_at"]);
}

#[test]
fn hidden_install_defers_ids_until_visible_consent_and_preserves_its_occurrence_time() {
    let f = Fixture::new();
    let output = f
        .command()
        .args(["--internal-install-completed", "--defer-notice"])
        .env("AGIT_INSTALL_CHANNEL", "npm_global")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    assert!(!f.path("preferences.json").exists());
    assert!(f.events().is_empty());
    let fact: Value =
        serde_json::from_slice(&std::fs::read(f.path("pending-install.json")).unwrap()).unwrap();
    assert!(fact.get("device_id").is_none());
    f.run(&["--version"]);
    assert!(!f.path("preferences.json").exists());
    let enabled = f.run(&["telemetry", "enable"]);
    assert!(String::from_utf8_lossy(&enabled.stderr).contains("agit telemetry disable"));
    let installs: Vec<_> = f
        .events()
        .into_iter()
        .filter(|e| e["event"] == "cli_install_succeeded")
        .collect();
    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0]["timestamp"], fact["verified_at"]);
    assert_eq!(installs[0]["properties"]["channel"], "npm_global");
    assert!(!f.path("pending-install.json").exists());
}

#[test]
fn declining_statistics_purges_the_hidden_install_fact_before_reenable() {
    let f = Fixture::new();
    f.run(&["--internal-install-completed", "--defer-notice"]);
    assert!(f.path("pending-install.json").exists());
    f.run(&["telemetry", "disable"]);
    assert!(!f.path("pending-install.json").exists());
    f.run(&["--internal-install-completed", "--defer-notice"]);
    assert!(!f.path("pending-install.json").exists());
    f.enable();
    assert!(f.events().is_empty());
}

#[test]
fn manual_installations_bind_their_key_before_the_first_authorization_handoff() {
    let mut f = Fixture::new();
    f.enable();
    let mut first_installation = None;
    for first in [true, false] {
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        f.hub = format!("http://{}", hub.local_addr().unwrap());
        let mut child = f
            .command()
            .args(["login", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        reply_to_login(
            &hub,
            &mut child,
            json!({"state":"private-state-canary", "url":format!("{}/auth/cli?state=private-state-canary", f.hub), "expires_in":300}),
        );
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(8));
        let prefs: Value =
            serde_json::from_slice(&std::fs::read(f.path("preferences.json")).unwrap()).unwrap();
        assert_eq!(prefs["install_reported"], false);
        if first {
            assert!(String::from_utf8_lossy(&output.stdout).contains("installation_id="));
            assert!(prefs["acquisition_route"].is_string());
            first_installation = Some(prefs["acquisition_route"].clone());
        } else {
            assert!(!String::from_utf8_lossy(&output.stdout).contains("installation_id="));
            assert_eq!(Some(prefs["acquisition_route"].clone()), first_installation);
        }
    }
}

#[test]
fn installer_retains_first_and_latest_campaigns_locally_and_opt_out_erases_them() {
    let fixture = Fixture::new();
    fixture.enable();
    for source in ["first", "latest"] {
        let output = fixture.command()
            .args(["--internal-install-completed"])
            .env("AGIT_CAMPAIGN_URL", format!("https://example.test/?utm_source={source}&utm_id=launch&code=private-campaign-canary"))
            .output().unwrap();
        assert!(output.status.success());
    }
    let preferences: Value =
        serde_json::from_slice(&std::fs::read(fixture.path("preferences.json")).unwrap()).unwrap();
    assert_eq!(
        preferences["campaign_first"]["parameters"]["utm_source"][0],
        "first"
    );
    assert_eq!(
        preferences["campaign_latest"]["parameters"]["utm_source"][0],
        "latest"
    );
    assert!(!preferences.to_string().contains("private-campaign-canary"));
    assert!(!fixture.queue().to_string().contains("utm_source"));
    assert!(fixture.run(&["telemetry", "disable"]).status.success());
    let preferences: Value =
        serde_json::from_slice(&std::fs::read(fixture.path("preferences.json")).unwrap()).unwrap();
    assert!(preferences["campaign_first"].is_null());
    assert!(preferences["campaign_latest"].is_null());
}

#[test]
fn maximum_escaped_campaign_round_trips_pending_and_active_preferences() {
    let mut raw = "https://example.test/?".to_owned();
    for index in 0..8 {
        if index > 0 {
            raw.push('&');
        }
        raw.push_str(&format!("utm_{index}={}", "\\".repeat(1024)));
    }
    raw.truncate(8192);
    let fixture = Fixture::new();
    assert!(
        fixture
            .command()
            .args(["--internal-install-completed", "--defer-notice"])
            .env("AGIT_CAMPAIGN_URL", &raw)
            .output()
            .unwrap()
            .status
            .success()
    );
    let pending: Value =
        serde_json::from_slice(&std::fs::read(fixture.path("pending-install.json")).unwrap())
            .unwrap();
    assert_eq!(
        pending["campaign"]["parameters"].as_object().unwrap().len(),
        8
    );
    fixture.enable();
    assert!(fixture.run(&["--version"]).status.success());
    let preferences: Value =
        serde_json::from_slice(&std::fs::read(fixture.path("preferences.json")).unwrap()).unwrap();
    assert_eq!(
        preferences["campaign_first"]["parameters"],
        pending["campaign"]["parameters"]
    );
    assert_eq!(
        preferences["campaign_latest"]["parameters"],
        pending["campaign"]["parameters"]
    );
    assert!(fixture.run(&["telemetry", "status"]).status.success());
    assert!(
        fixture
            .command()
            .args(["--internal-install-completed"])
            .env("AGIT_CAMPAIGN_URL", &raw)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(fixture.run(&["telemetry", "status"]).status.success());
}
