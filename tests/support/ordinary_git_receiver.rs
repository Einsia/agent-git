//! Real Git and native Git LFS exercise ordinary publication without privacy APIs.

use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

pub fn git(root: &Path, args: &[&str]) -> String {
    let output = command(root).args(args).output().unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.env_clear();
    for name in ["PATH", "SYSTEMROOT", "WINDIR", "COMSPEC"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .current_dir(root)
        .env("HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("absent-global-config"));
    command
}

pub fn initialize(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "--bare", "--initial-branch=main"]);
    git(root, &["config", "http.receivepack", "true"]);
}

pub fn respond(
    root: &Path,
    base: &str,
    method: &str,
    target: &str,
    content_type: &str,
    body: &[u8],
    payloads: &mut BTreeMap<String, Vec<u8>>,
) -> (String, Vec<u8>) {
    if target.ends_with("/info/lfs/objects/batch") {
        let request: Value = serde_json::from_slice(body).unwrap();
        let objects: Vec<_> = request["objects"].as_array().unwrap().iter().map(|object| {
            let oid = object["oid"].as_str().unwrap();
            if payloads.contains_key(oid) {
                if request["operation"] == "download" {
                    json!({"oid":oid,"size":object["size"],"actions":{"download":{
                        "href":format!("{base}/alice/demo.git/info/lfs/objects/{oid}"),
                        "header":{"Authorization":"Bearer synthetic-access","X-AgentGit-Expected-Agent-Id":crate::ID}
                    }}})
                } else { object.clone() }
            } else if request["operation"] == "download" {
                json!({"oid":oid,"size":object["size"],"error":{"code":404,"message":"not found"}})
            } else {
                let headers = json!({"Authorization":"Bearer synthetic-access","X-AgentGit-Expected-Agent-Id":crate::ID});
                json!({"oid":oid,"size":object["size"],"authenticated":true,"actions":{
                    "upload":{"href":format!("{base}/alice/demo.git/info/lfs/objects/{oid}"),"header":headers},
                    "verify":{"href":format!("{base}/alice/demo.git/info/lfs/objects/{oid}/verify"),"header":headers}
                }})
            }
        }).collect();
        return (
            "Content-Type: application/vnd.git-lfs+json".into(),
            serde_json::to_vec(&json!({"transfer":"basic","objects":objects})).unwrap(),
        );
    }
    if let Some(oid) = target.strip_prefix("/alice/demo.git/info/lfs/objects/") {
        if let Some(oid) = oid.strip_suffix("/verify") {
            assert_eq!(method, "POST");
            let verification: Value = serde_json::from_slice(body).unwrap();
            assert_eq!(verification["oid"], oid);
            assert_eq!(verification["size"], payloads.get(oid).unwrap().len());
            return (
                "Content-Type: application/vnd.git-lfs+json".into(),
                b"{}".to_vec(),
            );
        }
        if method == "PUT" {
            use sha2::Digest;
            assert_eq!(hex::encode(sha2::Sha256::digest(body)), oid);
            payloads.insert(oid.into(), body.into());
            return ("Content-Type: application/octet-stream".into(), Vec::new());
        }
        return (
            "Content-Type: application/octet-stream".into(),
            payloads.get(oid).unwrap().clone(),
        );
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut child = command(root)
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_TYPE", content_type)
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("REMOTE_USER", "alice")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let split = output
        .stdout
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap();
    let headers = String::from_utf8(output.stdout[..split].into()).unwrap();
    assert!(!headers.contains("Status:"), "{headers}");
    (headers, output.stdout[split + 4..].into())
}
