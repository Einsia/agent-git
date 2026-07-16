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

/// A key whose value is a secret (e.g. "token", "apiKey", "deployKey", "authorization").
fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["token", "secret", "password", "passwd", "pwd", "apikey", "api_key", "api-key", "accesskey",
     "access_key", "privatekey", "private_key", "authorization", "bearer", "credential", "key",
     "auth", "cookie", "signature", "webhook", "dsn", "connectionstring", "connection_string"]
        .iter()
        .any(|needle| k.contains(needle))
}

/// A CLI flag whose *following* argument is a secret (`--token X`, `--api-key X`, `--password X`).
fn is_secret_flag(s: &str) -> bool {
    if !s.starts_with('-') {
        return false;
    }
    let t = s.trim_start_matches('-').to_ascii_lowercase();
    ["token", "secret", "password", "passwd", "pwd", "apikey", "api-key", "api_key", "key", "auth",
     "credential", "bearer"]
        .iter()
        .any(|n| t.contains(n))
}

/// Does a URL carry a secret? userinfo (`scheme://user:pass@host`) or a long opaque path/query segment
/// (webhook tokens like slack/discord live in the path and match no format rule).
fn url_has_secret(s: &str) -> bool {
    let Some((_, rest)) = s.split_once("://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.contains('@') {
        return true;
    }
    // any 20+ char run of URL-token chars beyond the authority (path/query) is treated as a secret
    let after = &rest[authority.len()..];
    let mut run = 0usize;
    for c in after.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            run += 1;
            if run >= 20 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

fn record_and_redact(v: &mut Value, file: &str, path: &str, hint: &str, out: &mut Vec<Redaction>) {
    if let Value::String(_) = v {
        out.push(Redaction { file: file.into(), path: path.into(), hint: hint.into() });
        *v = Value::String(placeholder(hint));
    }
}

/// Redact EVERY string leaf under a value (used once a key/container is known to be secret-bearing).
/// Each leaf's hint is its own object key (so env-var leaves keep names like GITHUB_TOKEN for restore);
/// array elements fall back to the enclosing key.
fn redact_subtree(v: &mut Value, file: &str, path: &str, hint: &str, out: &mut Vec<Redaction>) {
    match v {
        Value::Object(m) => {
            for (k, c) in m.iter_mut() {
                redact_subtree(c, file, &format!("{path}.{k}"), k, out);
            }
        }
        Value::Array(a) => {
            for (i, c) in a.iter_mut().enumerate() {
                redact_subtree(c, file, &format!("{path}[{i}]"), hint, out);
            }
        }
        Value::String(_) => record_and_redact(v, file, path, hint, out),
        _ => {}
    }
}

/// Recursively redact a parsed config value in place. Default-deny: a secret container (env/headers)
/// or a secret-named key has its ENTIRE subtree redacted; an argument following a secret flag is
/// redacted; a URL carrying userinfo or a long opaque segment is redacted; and any remaining string
/// the scanner flags (entropy ON for these small configs) is redacted too.
fn redact_value(v: &mut Value, file: &str, path: &str, out: &mut Vec<Redaction>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                let child_path = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                if is_secret_container(k) || is_secret_key(k) {
                    redact_subtree(child, file, &child_path, k, out);
                } else {
                    redact_value(child, file, &child_path, out);
                }
            }
        }
        Value::Array(arr) => {
            let mut flag_hint: Option<String> = None;
            for (i, child) in arr.iter_mut().enumerate() {
                let child_path = format!("{path}[{i}]");
                if let Some(h) = flag_hint.take() {
                    record_and_redact(child, file, &child_path, &h, out);
                    continue;
                }
                if let Value::String(s) = child {
                    if is_secret_flag(s) {
                        flag_hint = Some(s.trim_start_matches('-').to_string());
                    }
                }
                redact_value(child, file, &child_path, out);
            }
        }
        Value::String(s) => {
            let hint = path.rsplit(['.', '[']).next().unwrap_or(path).trim_end_matches(']');
            if url_has_secret(s) || !crate::scan::scan_text_opts(s, true).is_empty() {
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
    redact_value(&mut v, file, "", &mut out);
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

/// Copy a prose file, but SKIP it (don't write it to the store) if it carries a suspected secret.
/// Capture is the gate — we never rely on the commit hook, which scans the harness tree unevenly.
fn copy_prose(src: &Path, dst: &Path, label: &str, report: &mut Report) -> Result<()> {
    if !src.is_file() {
        return Ok(());
    }
    let text = std::fs::read_to_string(src)?;
    if !crate::scan::scan_text_opts(&text, true).is_empty() {
        report.warnings.push(format!("{label} NOT captured — contains a suspected secret; remove it and re-snap"));
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dst, text)?;
    report.files += 1;
    Ok(())
}

/// Copy a skills/commands tree. `.json` files are field-redacted like the top-level configs; any other
/// file carrying a suspected secret is SKIPPED (not written), so a secret never enters the store —
/// regardless of extension or of what the commit hook happens to scan.
fn copy_tree_scanned(src: &Path, dst: &Path, label: &str, report: &mut Report) -> Result<()> {
    for entry in walkdir::WalkDir::new(src).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = dst.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        let is_json = entry.path().extension().map(|e| e == "json").unwrap_or(false);
        if is_json {
            // structured: redact fields rather than skip, so a config with a token still travels usable.
            match redact_json_doc(&text, &rel.to_string_lossy()) {
                Ok((redacted, reds)) => {
                    std::fs::write(&target, redacted)?;
                    report.redactions.extend(reds);
                    report.files += 1;
                }
                Err(_) => report.warnings.push(format!("{label}/{} NOT captured — unparseable JSON", rel.display())),
            }
            continue;
        }
        if !crate::scan::scan_text_opts(&text, true).is_empty() {
            report.warnings.push(format!("{label}/{} NOT captured — contains a suspected secret", rel.display()));
            continue;
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

/// The runtimes with a harness captured in the store, alphabetically.
pub fn captured_runtimes(agent: &Path) -> Vec<&'static str> {
    crate::session::RUNTIMES
        .into_iter()
        .filter(|rt| agent.join("harness").join(rt).join("project").is_dir())
        .collect()
}

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

    // Each of these is a concrete leak input from the adversarial review; all must redact.
    #[test]
    fn redacts_opaque_secrets_the_scanner_would_miss() {
        let doc = r#"{
          "mcpServers": {
            "a": { "command": "srv", "args": ["--token", "AbCd1234OpaqueValue", "--root", "/tmp"] },
            "b": { "url": "https://user:pass@internal.example.com" },
            "c": { "url": "https://hooks.slack.com/services/T00/B00/XXXXXXXXXXXXXXXXXXXXXXXX" },
            "d": { "deployKey": "opaquevalue" },
            "e": { "credential": { "custom": "opaquevalue" } },
            "f": { "token": ["opaque-positional-value"] }
          }
        }"#;
        let (out, _reds) = redact_json_doc(doc, "mcp.json").unwrap();
        for leaked in [
            "AbCd1234OpaqueValue",         // arg after --token (flag adjacency)
            "user:pass@internal",          // url userinfo
            "XXXXXXXXXXXXXXXXXXXXXXXX",     // webhook token in url path
            "\"opaquevalue\"",             // deployKey (custom secret-named) + credential.custom (container)
            "opaque-positional-value",     // token:[...] (secret key holding array)
        ] {
            assert!(!out.contains(leaked), "leaked: {leaked}\n{out}");
        }
        // non-secrets survive
        assert!(out.contains("\"srv\""));
        assert!(out.contains("/tmp"));
        assert!(out.contains("internal.example.com") || out.contains("${AGIT_SECRET:url}"));
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
