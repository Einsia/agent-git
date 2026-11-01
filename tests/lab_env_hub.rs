//! An agit a test starts through the lab environment reports nothing to production.
//!
//! The first property: a lab command resolves the local Hub. The listing reads local state only
//! and names the Hub the binary resolved. If the helper cleared the environment without setting
//! the Hub, the binary would take its built-in production Hub, this listing would name it, and
//! every lab's first command would report a new installation to production usage statistics.
//!
//! The second: a lab command that removes the Hub to test the production Hub itself delivers its
//! statistics to the sink the helper configures. The binary ignores a sink whose address or key
//! fails its check and sends to production instead; a refusing sink would hide that, so this one
//! listens and must receive the batch.

#[path = "support/lab_env.rs"]
mod lab_env;

use lab_env::{LOCAL_HUB, LabEnv, REFUSING_PORT, SINK_KEY};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

fn agit(root: &Path, args: &[&str], configure: impl FnOnce(&mut Command)) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .args(args)
        .env_clear_local()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("AGIT_HOME", root.join("agit"));
    if let Some(system) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system);
    }
    configure(&mut command);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "agit {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// One HTTP request's body, answered with an empty success.
fn receive(stream: std::net::TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let mut stream = stream;
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
        .unwrap();
    String::from_utf8(body).unwrap()
}

#[test]
fn a_lab_command_resolves_the_local_hub() {
    let root = tempfile::tempdir().unwrap();
    let output = agit(root.path(), &["--json", "project", "list"], |command| {
        // Statistics go to a refusing sink even if the Hub above were missing.
        command.telemetry_sink(REFUSING_PORT);
    });
    let listing: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listing["result"]["value"]["hub"], LOCAL_HUB, "{listing}");
}

#[test]
fn a_lab_command_under_the_production_hub_reports_only_to_its_sink() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sent, received) = mpsc::channel();
    // Every sender is answered, the command's own and the explicit flush below alike.
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let _ = sent.send(receive(stream.unwrap()));
        }
    });
    let production = |command: &mut Command| {
        command.env_remove("AGIT_HUB_URL").telemetry_sink(port);
    };
    let output = agit(root.path(), &["--json", "project", "list"], production);
    let listing: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        listing["result"]["value"]["hub"], "https://agent-git.com",
        "{listing}"
    );
    agit(root.path(), &["--internal-telemetry-flush"], production);

    let body: Value =
        serde_json::from_str(&received.recv_timeout(Duration::from_secs(30)).unwrap()).unwrap();
    assert_eq!(body["api_key"], SINK_KEY);
    let events: Vec<_> = body["batch"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert!(events.contains(&"cli_install_succeeded"), "{events:?}");
}
