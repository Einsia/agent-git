//! `--json whoami --check` puts nothing on stdout but that one JSON document when it succeeds.
//!
//! The human-mode "accepts the credentials" line goes to stdout; leaking it into JSON mode means
//! the wrapper no longer sees a single JSON value, and `check.authenticated` can no longer be
//! read as a field.

use std::io::{Read, Write};
use std::{fs, process::Command};

/// A fake hub that answers a single request: any GET /api/auth/me with a Bearer is alice.
fn fake_hub() -> (String, u16, std::thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let k = sock.read(&mut chunk).unwrap();
            if k == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..k]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let req = String::from_utf8_lossy(&buf).into_owned();
        let body = r#"{"username":"alice","email":null}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(resp.as_bytes()).unwrap();
        req
    });
    (base, port, handle)
}

#[test]
fn whoami_check_in_json_mode_emits_exactly_one_document() {
    let (base, port, hub) = fake_hub();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let creds = home.join("credentials");
    fs::create_dir_all(&creds).unwrap();
    // The credential file name is the host key: the `:` in the authority becomes `_`.
    fs::write(
        creds.join(format!("127.0.0.1_{port}.json")),
        serde_json::to_vec(&serde_json::json!({
            "username": "alice",
            "email": null,
            "hub": base,
            "access_token": "at-valid",
            "access_expires_at": "2099-01-01T00:00:00Z",
            "refresh_token": "rt-valid",
            "refresh_expires_at": "2099-02-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(["--json", "whoami", "--check"])
        .env("AGIT_HOME", &home)
        .env("AGIT_HUB_URL", &base)
        .env_remove("AGIT_SESSION")
        .output()
        .unwrap();
    let req = hub.join().unwrap();
    assert!(req.starts_with("GET /api/auth/me"), "{req}");
    assert!(req.contains("Bearer at-valid"), "{req}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("not one JSON document: {e}\n{stdout}"));
    assert_eq!(doc["schema"], "cli-output");
    assert_eq!(doc["exit_code"], 0);
    assert_eq!(doc["result"]["format"], "json", "{doc}");
    let check = &doc["result"]["value"]["check"];
    assert_eq!(check["requested"], true);
    assert_eq!(check["server_reachable"], true);
    assert_eq!(check["authenticated"], true);
    assert_eq!(doc["result"]["value"]["account"], "alice");
}
