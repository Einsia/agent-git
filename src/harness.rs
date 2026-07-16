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

// ── restore (phase 3): apply a captured harness back into the project ──

const PH_PREFIX: &str = "${AGIT_SECRET:";

/// `agit harness show` — list what's captured in the local Agent Store.
pub fn show(agent: &Path, runtime: &str) -> Result<i32> {
    let rt = norm(runtime);
    let src = agent.join("harness").join(&rt).join("project");
    if !src.is_dir() {
        println!("No captured harness for {rt}. Run `agit -a snap` to capture it.");
        return Ok(0);
    }
    let (servers, skills, commands, secrets) = summarize(agent, &rt, &src);
    println!("Captured harness ({rt}, project scope):");
    println!("  MCP servers : {servers}");
    println!("  skills      : {skills}");
    println!("  commands    : {commands}");
    println!("  secrets     : {secrets} redacted field(s) to provide on apply");
    println!("\n  Apply into this project: agit harness apply");
    Ok(0)
}

/// `agit harness apply` — union-merge the captured harness into the current project, asking first
/// (decision 3) and resolving each redacted secret from its env var, else prompting at a TTY.
pub fn apply(agent: &Path, env: &Path, runtime: &str, force: bool) -> Result<i32> {
    use std::io::{stdin, stdout, IsTerminal, Write};
    let rt = norm(runtime);
    let src = agent.join("harness").join(&rt).join("project");
    if !src.is_dir() {
        println!("No captured harness for {rt} to apply.");
        return Ok(0);
    }
    let (servers, skills, commands, secrets) = summarize(agent, &rt, &src);
    println!("Captured harness ({rt}): {servers} MCP servers, {skills} skills, {commands} commands, {secrets} secret(s) to provide.");

    // Ask (decision 3): applying rewrites local .mcp.json / .claude — never silent.
    if !force {
        if !stdin().is_terminal() {
            println!("Not applying: rerun at a terminal, or pass --force to apply non-interactively.");
            return Ok(0);
        }
        print!("Apply to this project? [y/N] ");
        let _ = stdout().flush();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut stdin().lock(), &mut line).ok();
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            println!("Skipped.");
            return Ok(0);
        }
    }

    let mut applied = Vec::new();
    let mut skipped = Vec::new();
    let mut unresolved = Vec::new();

    // MCP: union-merge servers into the project's .mcp.json, then resolve secret placeholders.
    let src_mcp = src.join("mcp.json");
    if src_mcp.is_file() {
        let merged = merge_mcp(&std::fs::read_to_string(&src_mcp)?, &env.join(".mcp.json"), &mut skipped)?;
        let mut v: Value = serde_json::from_str(&merged)?;
        resolve_secrets(&mut v, &mut unresolved);
        std::fs::write(env.join(".mcp.json"), serde_json::to_string_pretty(&v)? + "\n")?;
        applied.push(".mcp.json".to_string());
    }

    // Prose + trees: add what the project is missing; never clobber an existing local copy.
    apply_missing_file(&src.join("CLAUDE.md"), &env.join("CLAUDE.md"), "CLAUDE.md", &mut applied, &mut skipped)?;
    apply_missing_tree(&src.join("skills"), &env.join(".claude/skills"), "skills", &mut applied, &mut skipped)?;
    apply_missing_tree(&src.join("commands"), &env.join(".claude/commands"), "commands", &mut applied, &mut skipped)?;

    // settings.json can carry hooks (executable). Never auto-apply — surface it.
    if src.join("settings.json").is_file() {
        skipped.push("settings.json (may contain hooks that run commands — review and apply manually)".into());
    }

    println!("\nApplied: {}", if applied.is_empty() { "(nothing)".into() } else { applied.join(", ") });
    for s in &skipped {
        println!("  skipped: {s}");
    }
    for u in &unresolved {
        eprintln!("  ⚠ secret not provided: {u} — left as a placeholder in .mcp.json (set ${u} or edit it in)");
    }
    Ok(if unresolved.is_empty() { 0 } else { 1 })
}

fn norm(runtime: &str) -> String {
    match runtime {
        "claude" | "cc" | "claude-code" => "claude-code".into(),
        other => other.to_string(),
    }
}

fn summarize(agent: &Path, rt: &str, src: &Path) -> (usize, usize, usize, usize) {
    let servers = std::fs::read_to_string(src.join("mcp.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("mcpServers").and_then(|m| m.as_object()).map(|m| m.len()))
        .unwrap_or(0);
    let skills = count_dirs(&src.join("skills"));
    let commands = count_files(&src.join("commands"));
    let secrets = std::fs::read_to_string(agent.join("harness").join(rt).join("manifest.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("redactions").and_then(|r| r.as_array()).map(|a| a.len()))
        .unwrap_or(0);
    (servers, skills, commands, secrets)
}

fn count_dirs(p: &Path) -> usize {
    std::fs::read_dir(p).map(|d| d.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).count()).unwrap_or(0)
}
fn count_files(p: &Path) -> usize {
    std::fs::read_dir(p).map(|d| d.filter_map(|e| e.ok()).filter(|e| e.path().is_file()).count()).unwrap_or(0)
}

/// Union-merge captured mcpServers into the project's .mcp.json. Adds missing servers, keeps local,
/// and flags a same-name-different-definition conflict (keeping the local one) into `conflicts`.
pub fn merge_mcp(captured: &str, local_path: &Path, conflicts: &mut Vec<String>) -> Result<String> {
    let cap: Value = serde_json::from_str(captured).context("captured mcp.json is not valid JSON")?;
    let mut local: Value = if local_path.is_file() {
        serde_json::from_str(&std::fs::read_to_string(local_path)?).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    let local_servers = local
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("mcpServers is not an object")?;
    if let Some(cap_servers) = cap.get("mcpServers").and_then(|m| m.as_object()) {
        for (name, def) in cap_servers {
            match local_servers.get(name) {
                None => {
                    local_servers.insert(name.clone(), def.clone());
                }
                Some(existing) if existing == def => {}
                Some(_) => conflicts.push(format!("MCP server '{name}' differs from local — kept local")),
            }
        }
    }
    Ok(serde_json::to_string_pretty(&local)? + "\n")
}

/// Replace every `${AGIT_SECRET:NAME}` placeholder: env var NAME wins; else prompt at a TTY; else record
/// NAME as unresolved and leave the placeholder in place.
fn resolve_secrets(v: &mut Value, unresolved: &mut Vec<String>) {
    match v {
        Value::Object(m) => m.values_mut().for_each(|c| resolve_secrets(c, unresolved)),
        Value::Array(a) => a.iter_mut().for_each(|c| resolve_secrets(c, unresolved)),
        Value::String(s) => {
            if let Some(name) = s.strip_prefix(PH_PREFIX).and_then(|r| r.strip_suffix('}')) {
                let name = name.to_string();
                if let Some(val) = resolve_one(&name) {
                    *s = val;
                } else if !unresolved.contains(&name) {
                    unresolved.push(name);
                }
            }
        }
        _ => {}
    }
}

fn resolve_one(name: &str) -> Option<String> {
    use std::io::{stdin, stdout, IsTerminal, Write};
    if let Ok(v) = std::env::var(name) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    if stdin().is_terminal() {
        print!("  secret {name} = ");
        let _ = stdout().flush();
        let mut line = String::new();
        if std::io::BufRead::read_line(&mut stdin().lock(), &mut line).is_ok() {
            let t = line.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn apply_missing_file(src: &Path, dst: &Path, label: &str, applied: &mut Vec<String>, skipped: &mut Vec<String>) -> Result<()> {
    if !src.is_file() {
        return Ok(());
    }
    if dst.exists() {
        skipped.push(format!("{label} (already present)"));
        return Ok(());
    }
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::copy(src, dst)?;
    applied.push(label.to_string());
    Ok(())
}

fn apply_missing_tree(src: &Path, dst: &Path, label: &str, applied: &mut Vec<String>, skipped: &mut Vec<String>) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    let mut added = 0;
    for entry in walkdir::WalkDir::new(src).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = dst.join(rel);
        if target.exists() {
            skipped.push(format!("{label}/{} (already present)", rel.display()));
            continue;
        }
        if let Some(p) = target.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(entry.path(), &target)?;
        added += 1;
    }
    if added > 0 {
        applied.push(format!("{label} (+{added})"));
    }
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

    #[test]
    fn merge_mcp_unions_new_and_keeps_local_on_conflict() {
        let dir = std::env::temp_dir().join(format!("agit-merge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join(".mcp.json");
        std::fs::write(&local, r#"{"mcpServers":{"github":{"command":"LOCAL"}}}"#).unwrap();

        let captured = r#"{"mcpServers":{"github":{"command":"npx"},"fs":{"command":"mcp-fs"}}}"#;
        let mut conflicts = Vec::new();
        let out = merge_mcp(captured, &local, &mut conflicts).unwrap();

        assert!(out.contains("LOCAL")); // local github kept
        assert!(out.contains("mcp-fs")); // fs added
        assert!(!out.contains("\"npx\"")); // captured github NOT overwritten
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("github"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_mcp_with_no_local_takes_captured() {
        let captured = r#"{"mcpServers":{"fs":{"command":"mcp-fs"}}}"#;
        let mut conflicts = Vec::new();
        let out = merge_mcp(captured, Path::new("/no/such/file.json"), &mut conflicts).unwrap();
        assert!(out.contains("mcp-fs"));
        assert!(conflicts.is_empty());
    }

    #[test]
    fn resolve_secrets_from_env_and_records_unresolved() {
        std::env::set_var("AGIT_TEST_TOKEN_XZ", "resolved-value-1");
        let mut v = json!({ "env": {
            "TOK": "${AGIT_SECRET:AGIT_TEST_TOKEN_XZ}",
            "MISS": "${AGIT_SECRET:AGIT_UNSET_VAR_QQ}"
        }});
        let mut unresolved = Vec::new();
        resolve_secrets(&mut v, &mut unresolved);
        assert_eq!(v["env"]["TOK"], json!("resolved-value-1"));
        assert!(unresolved.contains(&"AGIT_UNSET_VAR_QQ".to_string()));
        // the unresolved one keeps its placeholder
        assert!(v["env"]["MISS"].as_str().unwrap().starts_with(PH_PREFIX));
        std::env::remove_var("AGIT_TEST_TOKEN_XZ");
    }
}
