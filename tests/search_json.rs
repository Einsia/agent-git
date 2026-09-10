//! CLI and MCP must preserve one structured result, including a partly failed batch.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn fixture(requests: usize) -> (tempfile::TempDir, String, std::thread::JoinHandle<()>) {
    let temp = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let credentials = temp.path().join("credentials");
    std::fs::create_dir_all(&credentials).unwrap();
    std::fs::write(
        credentials.join(format!("127.0.0.1_{port}.json")),
        serde_json::to_vec(&serde_json::json!({
            "username":"alice", "hub":base, "access_token":"test-only",
            "access_expires_at":"2099-01-01T00:00:00Z", "refresh_token":"test-only",
            "refresh_expires_at":"2099-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let server = std::thread::spawn(move || {
        for stream in listener.incoming().take(requests) {
            let mut stream = stream.unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut input = Vec::new();
            while !input.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut buffer = [0; 4096];
                let size = stream.read(&mut buffer).unwrap();
                assert_ne!(size, 0);
                input.extend_from_slice(&buffer[..size]);
            }
            let request = String::from_utf8(input).unwrap();
            assert!(request.starts_with("GET /api/search/sessions?"));
            let (status, value) = if request.contains("q=fail") {
                (
                    "503 Service Unavailable",
                    serde_json::json!({"error":"retry this query"}),
                )
            } else {
                (
                    "200 OK",
                    serde_json::json!({
                        "type":"sessions", "total":6, "page":1, "per":5,
                        "incomplete":true, "unknown":["runtim:codex"], "terms":["cache"],
                        "items":[{"session_id":"session-a", "timestamp":"2026-09-08T00:00:00Z",
                            "scope":"tool", "future_field":true}]
                    }),
                )
            };
            let body = value.to_string();
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (temp, base, server)
}

fn command(temp: &tempfile::TempDir, base: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .current_dir(temp.path())
        .env("AGIT_HOME", temp.path())
        .env("AGIT_HUB_URL", base)
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_SESSION_ID")
        .env_remove("AGIT_MERGE_TX")
        .env_remove("AGIT_RC")
        .env("NO_COLOR", "1");
    command
}

fn document(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout must be one JSON document: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn redirected_search_and_global_json_preserve_structured_fields() {
    for json in [false, true] {
        let (temp, base, server) = fixture(1);
        let mut command = command(&temp, &base);
        if json {
            command.arg("--json");
        }
        let output = command
            .args(["search", "cache", "--limit", "5"])
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(output.status.success());
        let doc = document(&output);
        let value = if json {
            assert_eq!(doc["ok"], true);
            assert_eq!(doc["result"]["format"], "json");
            &doc["result"]["value"]
        } else {
            &doc
        };
        assert_eq!(value["page"], 1);
        assert_eq!(value["per"], 5);
        assert_eq!(value["has_more"], true);
        assert_eq!(value["incomplete"], true);
        assert_eq!(value["unknown"][0], "runtim:codex");
        assert_eq!(value["hits"][0]["future_field"], true);
        assert_eq!(value["hits"][0]["timestamp"], "2026-09-08T00:00:00Z");
    }
}

#[test]
fn a_query_named_json_remains_a_value_in_both_output_modes() {
    for json in [false, true] {
        let (temp, base, server) = fixture(1);
        let mut command = command(&temp, &base);
        if json {
            command.arg("--json");
        }
        let output = command
            .args(["search", "--query", "--json"])
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(output.status.success());
        let doc = document(&output);
        let value = if json { &doc["result"]["value"] } else { &doc };
        assert_eq!(value["query"], "--json");
    }
}

#[test]
fn json_batch_failure_keeps_successes_in_one_envelope() {
    let (temp, base, server) = fixture(2);
    let output = command(&temp, &base)
        .args(["search", "--query", "cache", "--query", "fail", "--json"])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!output.status.success());
    let doc = document(&output);
    assert_eq!(doc["ok"], false);
    assert_eq!(doc["exit_code"], output.status.code().unwrap());
    assert_eq!(doc["result"]["format"], "json");
    let entries = &doc["result"]["value"]["results"];
    assert_eq!(entries[0]["query"], "cache");
    assert_eq!(entries[0]["ok"], true);
    assert_eq!(entries[1]["query"], "fail");
    assert_eq!(entries[1]["ok"], false);
}

#[test]
fn mcp_batch_failure_is_structured_and_flagged() {
    let (temp, base, server) = fixture(2);
    let mut child = command(&temp, &base)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let request = serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"tools/call",
        "params":{"name":"search", "arguments":{"queries":["cache", "fail"]}}});
    writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let doc = document(&output);
    assert_eq!(doc["result"]["isError"], true);
    let result: serde_json::Value =
        serde_json::from_str(doc["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["results"][0]["ok"], true);
    assert_eq!(result["results"][1]["ok"], false);
}

#[test]
fn invalid_search_limits_are_json_usage_errors_before_authentication() {
    let temp = tempfile::tempdir().unwrap();
    let output = command(&temp, "http://127.0.0.1:1")
        .args(["--json", "search", "cache", "--limit", "0"])
        .output()
        .unwrap();
    let doc = document(&output);
    assert!(!output.status.success());
    assert_eq!(doc["exit_code"], agit::ExitCode::Usage.as_i32());
    assert!(
        doc["diagnostics"]["stderr"][0]["message"]
            .as_str()
            .unwrap()
            .contains("--limit")
    );
}

#[test]
fn unclosed_query_quotes_refuse_explicit_filters_before_any_request() {
    let temp = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let output = command(&temp, &base)
        .args([
            "--json",
            "search",
            "--query",
            "-\"cache",
            "--repo",
            "alice/service",
        ])
        .output()
        .unwrap();
    let doc = document(&output);
    assert_eq!(doc["exit_code"], agit::ExitCode::Usage.as_i32());
    assert!(
        doc["diagnostics"]["stderr"]
            .to_string()
            .contains("balanced double quotes")
    );

    let mut child = command(&temp, &base)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let request = serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"tools/call",
        "params":{"name":"search", "arguments":{"queries":["-\"cache"], "repo":"alice/service"}}});
    writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
    let output = child.wait_with_output().unwrap();
    let doc = document(&output);
    assert_eq!(doc["result"]["isError"], true);
    assert!(
        doc["result"]["content"]
            .to_string()
            .contains("balanced double quotes")
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

fn local_files(
    root: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Option<Vec<u8>>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            let contents = entry
                .file_type()
                .is_file()
                .then(|| std::fs::read(entry.path()).unwrap());
            (
                entry.path().strip_prefix(root).unwrap().to_owned(),
                contents,
            )
        })
        .collect()
}

#[test]
fn search_does_not_migrate_local_storage_before_rejecting_invalid_arguments() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("synthetic-evidence"),
        "SYNTHETIC-UNCHANGED",
    )
    .unwrap();
    let before = local_files(temp.path());
    for version in ["1", "2"] {
        for tail in [
            vec![],
            vec!["--query", " "],
            vec!["--repo", ""],
            vec!["cache", "--page", "0"],
        ] {
            let output = command(&temp, "http://127.0.0.1:1")
                .args(["--json", "--json-version", version, "search"])
                .args(tail)
                .env("CI", "1")
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2), "{output:?}");
            assert_eq!(document(&output)["exit_code"], 2);
            assert_eq!(local_files(temp.path()), before);
        }
    }
}

#[test]
fn authenticated_hub_search_does_not_prepare_local_repositories() {
    let (temp, base, server) = fixture(1);
    std::fs::create_dir_all(temp.path().join("repos/synthetic/legacy/.git")).unwrap();
    std::fs::write(
        temp.path().join("repos/synthetic/legacy/evidence"),
        "SYNTHETIC-UNCHANGED",
    )
    .unwrap();
    let before = local_files(temp.path());
    let output = command(&temp, &base)
        .args(["--json", "search", "cache"])
        .env("CI", "1")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        document(&output)["result"]["value"]["hits"][0]["session_id"],
        "session-a"
    );
    assert_eq!(local_files(temp.path()), before);
}
