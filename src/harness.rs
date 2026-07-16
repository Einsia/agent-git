//! Versioning the harness — the MCP servers, skills, commands, and memory that shape how the agent
//! behaves — as part of Agent State, alongside the raw session dump.
//!
//! This module is PHASE 1 of docs/plans/2026-07-16-harness-versioning-design.md: capture claude-code
//! PROJECT-scope harness into the Agent Store, with default-deny secret redaction. Not yet done here:
//! restore (`resume` apply-with-ask), user-scope capture, codex parity, and the sync union-merge.
//!
//! Layout written: <agent-store>/harness/<runtime>/project/{mcp.json,settings.json,CLAUDE.md,skills/,commands/}
//! plus harness/<runtime>/manifest.json recording what was captured and which secret fields were redacted.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;

/// One redacted secret: where it was, and the hint a human/agent needs to re-supply it on restore.
#[derive(Debug, Clone)]
pub struct Redaction {
    pub file: String,
    pub path: String,
    pub hint: String,
}

/// What a capture did — for the snap summary line and the manifest.
#[derive(Debug, Default)]
pub struct Report {
    pub files: usize,
    pub redactions: Vec<Redaction>,
    pub warnings: Vec<String>,
}

/// The placeholder a redacted secret value is replaced with. Restore prompts for `<hint>`.
fn placeholder(hint: &str) -> String {
    format!("${{AGIT_SECRET:{hint}}}")
}

/// Object keys whose *values* are always secret-bearing (default-deny containers): every leaf string
/// inside them is redacted regardless of the value, because this is where MCP configs put credentials.
fn is_secret_container(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "env" | "headers" | "http_headers" | "requestheaders" | "request_headers"
    )
}

/// A scalar key whose own value is a secret (e.g. "token": "…", "apiKey": "…").
fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["token", "secret", "password", "passwd", "apikey", "api_key", "api-key", "accesskey",
     "access_key", "privatekey", "private_key", "authorization", "bearer", "credential"]
        .iter()
        .any(|needle| k.contains(needle))
}

/// Recursively redact a parsed config value in place. Default-deny for secret containers (env/headers):
/// every leaf value is replaced. Elsewhere, a secret-named key's string value is replaced, and any other
/// string the secret scanner flags is replaced too (belt-and-suspenders for creds in urls/args).
fn redact_value(v: &mut Value, file: &str, path: &str, in_secret_container: bool, out: &mut Vec<Redaction>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                let child_path = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                let container = in_secret_container || is_secret_container(k);
                if !container && is_secret_key(k) {
                    if let Value::String(_) = child {
                        out.push(Redaction { file: file.into(), path: child_path.clone(), hint: k.clone() });
                        *child = Value::String(placeholder(k));
                        continue;
                    }
                }
                redact_value(child, file, &child_path, container, out);
            }
        }
        Value::Array(arr) => {
            for (i, child) in arr.iter_mut().enumerate() {
                redact_value(child, file, &format!("{path}[{i}]"), in_secret_container, out);
            }
        }
        Value::String(s) => {
            // In a secret container every value goes; elsewhere, only if the scanner flags it.
            let hint = path.rsplit(['.', '[']).next().unwrap_or(path).trim_end_matches(']');
            if in_secret_container {
                out.push(Redaction { file: file.into(), path: path.into(), hint: hint.into() });
                *s = placeholder(hint);
            } else if !crate::scan::scan_text_opts(s, false).is_empty() {
                out.push(Redaction { file: file.into(), path: path.into(), hint: hint.into() });
                *s = placeholder(hint);
            }
        }
        _ => {}
    }
}

/// Redact a JSON document (mcp.json / settings.json), returning pretty JSON + the redactions made.
pub fn redact_json_doc(text: &str, file: &str) -> Result<(String, Vec<Redaction>)> {
    let mut v: Value = serde_json::from_str(text).with_context(|| format!("{file} is not valid JSON"))?;
    let mut out = Vec::new();
    redact_value(&mut v, file, "", false, &mut out);
    Ok((serde_json::to_string_pretty(&v)? + "\n", out))
}

/// Capture the project-scope harness for `runtime` into the Agent Store. Returns a Report; missing
/// files are simply skipped (a project may have no .mcp.json). Errors on a genuinely broken config
/// (unparseable JSON) rather than copying it with secrets unredacted.
pub fn capture(agent: &Path, env: &Path, runtime: &str) -> Result<Report> {
    let rt = match runtime {
        "claude" | "cc" | "claude-code" => "claude-code",
        other => other, // codex parity is a follow-up; nothing to capture yet
    };
    let mut report = Report::default();
    if rt != "claude-code" {
        return Ok(report);
    }
    let dst = agent.join("harness").join(rt).join("project");

    // (source relative to env, destination name) for the two redacted JSON configs.
    for (src_rel, dst_name) in [(".mcp.json", "mcp.json"), (".claude/settings.json", "settings.json")] {
        let src = env.join(src_rel);
        if !src.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&src)?;
        let (redacted, reds) = redact_json_doc(&text, dst_name)?;
        std::fs::create_dir_all(&dst)?;
        std::fs::write(dst.join(dst_name), redacted)?;
        report.files += 1;
        report.redactions.extend(reds);
    }

    // CLAUDE.md is prose: copy verbatim, but scan-and-warn (don't auto-redact prose — same policy as
    // session transcripts; the commit hook is the enforcing gate).
    copy_prose(&env.join("CLAUDE.md"), &dst.join("CLAUDE.md"), "CLAUDE.md", &mut report)?;

    // skills/ and commands/ trees: copy verbatim, scan-and-warn per file.
    for (src_rel, dst_name) in [(".claude/skills", "skills"), (".claude/commands", "commands")] {
        let src = env.join(src_rel);
        if src.is_dir() {
            copy_tree_scanned(&src, &dst.join(dst_name), dst_name, &mut report)?;
        }
    }

    if report.files > 0 {
        write_manifest(agent, rt, &report)?;
    }
    Ok(report)
}

fn copy_prose(src: &Path, dst: &Path, label: &str, report: &mut Report) -> Result<()> {
    if !src.is_file() {
        return Ok(());
    }
    let text = std::fs::read_to_string(src)?;
    if !crate::scan::scan_text_opts(&text, true).is_empty() {
        report.warnings.push(format!("{label} contains suspected secrets (not auto-redacted — prose)"));
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dst, text)?;
    report.files += 1;
    Ok(())
}

fn copy_tree_scanned(src: &Path, dst: &Path, label: &str, report: &mut Report) -> Result<()> {
    for entry in walkdir::WalkDir::new(src).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = dst.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        if !crate::scan::scan_text_opts(&text, true).is_empty() {
            report.warnings.push(format!("{label}/{} contains suspected secrets", rel.display()));
        }
        std::fs::copy(entry.path(), &target)?;
        report.files += 1;
    }
    Ok(())
}

fn write_manifest(agent: &Path, rt: &str, report: &Report) -> Result<()> {
    let redactions: Vec<Value> = report
        .redactions
        .iter()
        .map(|r| json!({ "file": r.file, "path": r.path, "hint": r.hint }))
        .collect();
    let manifest = json!({
        "runtime": rt,
        "scope": "project",
        "files": report.files,
        "redactions": redactions,
    });
    let dir = agent.join("harness").join(rt);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("manifest.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_env_and_header_values_keeps_shape() {
        let doc = r#"{
          "mcpServers": {
            "github": {
              "command": "npx",
              "args": ["-y", "@modelcontextprotocol/server-github"],
              "env": { "GITHUB_TOKEN": "ghp_realSecretValue12345", "PORT": "8080" }
            },
            "api": {
              "type": "http",
              "url": "https://api.example.com",
              "headers": { "Authorization": "Bearer sk-topsecret" }
            }
          }
        }"#;
        let (out, reds) = redact_json_doc(doc, "mcp.json").unwrap();
        // secrets gone
        assert!(!out.contains("ghp_realSecretValue12345"));
        assert!(!out.contains("sk-topsecret"));
        // shape preserved
        assert!(out.contains("\"command\""));
        assert!(out.contains("npx"));
        assert!(out.contains("@modelcontextprotocol/server-github"));
        assert!(out.contains("https://api.example.com"));
        // env/header VALUES redacted even for the non-obviously-secret PORT (default-deny container)
        assert!(out.contains("${AGIT_SECRET:GITHUB_TOKEN}"));
        assert!(out.contains("${AGIT_SECRET:PORT}"));
        assert!(out.contains("${AGIT_SECRET:Authorization}"));
        // recorded with restore hints
        let hints: Vec<&str> = reds.iter().map(|r| r.hint.as_str()).collect();
        assert!(hints.contains(&"GITHUB_TOKEN"));
        assert!(hints.contains(&"Authorization"));
    }

    #[test]
    fn redacts_secret_named_scalar_key() {
        let doc = r#"{ "apiKey": "AKIAIOSFODNN7EXAMPLE", "name": "myserver" }"#;
        let (out, reds) = redact_json_doc(doc, "mcp.json").unwrap();
        assert!(out.contains("${AGIT_SECRET:apiKey}"));
        assert!(out.contains("myserver")); // non-secret preserved
        assert_eq!(reds.len(), 1);
    }

    #[test]
    fn clean_config_is_untouched() {
        let doc = r#"{ "mcpServers": { "fs": { "command": "mcp-fs", "args": ["--root", "/tmp"] } } }"#;
        let (_out, reds) = redact_json_doc(doc, "mcp.json").unwrap();
        assert!(reds.is_empty());
    }
}
