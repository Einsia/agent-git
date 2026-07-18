//! Versioning the harness — the MCP servers, skills, commands, and memory that shape how the agent
//! behaves — as part of Agent State, alongside the raw session dump.
//!
//! This module is PHASE 1 of docs/plans/2026-07-16-harness-versioning-design.md: capture claude-code
//! PROJECT-scope harness into the Agent Store, with default-deny secret redaction. Not yet done here:
//! restore (`resume` apply-with-ask), user-scope capture, codex parity, and the sync union-merge.
//!
//! Layout written: <agent-store>/harness/<env-slug>/<runtime>/project/{mcp.json,settings.json,CLAUDE.md,skills/,commands/}
//! plus harness/<env-slug>/<runtime>/manifest.json recording what was captured, the checkout it was
//! captured in, and which secret fields were redacted.
//!
//! The <env-slug> level partitions by CHECKOUT (design §6): one store is shared by several code repos,
//! so without it the second repo to capture silently overwrites the first repo's MCP config and
//! settings. Stores written before that level exist on disk, so reads also resolve the flat
//! harness/<runtime>/project — those keep working, and are never rewritten.

use crate::{errln, outln};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

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

// ── partitioning: one store, many checkouts ──

/// The harness partition key. Transcripts key on the *environment*; the harness is project-scoped, so
/// it keys on the **checkout** — one env can be many worktrees, and folding them together is what makes
/// a config ping-pong between checkouts (design §6). A path-derived slug is the checkout, exactly.
///
/// `slug_for` maps every non-alphanumeric character to `-`, so `/my/app`, `/my-app`, `/my_app` and
/// `/my.app` share one partition. Same key as `session::env_slug`, deliberately, so the two partitions
/// agree; the manifest's `env` field, never the slug, is the authority on which checkout captured what.
fn env_slug(env: &Path) -> String {
    crate::adapter::claude_code::slug_for(env)
}

/// One captured harness for one runtime, and the checkout it came from.
///
/// `slug`/`env` are None for a store written before the per-checkout layout: which checkout captured a
/// flat `harness/<rt>/project` is recorded nowhere, so it is never claimed to be this one.
#[derive(Debug, Clone)]
pub struct Partition {
    dir: PathBuf,
    slug: Option<String>,
    env: Option<String>,
    captured_at: Option<String>,
}

impl Partition {
    fn project(&self) -> PathBuf {
        self.dir.join("project")
    }

    /// How a human is told where a harness came from: the captured path, else the slug it is filed
    /// under, else an admission that a pre-partition store does not record it.
    pub fn label(&self) -> String {
        match (&self.env, &self.slug) {
            (Some(e), _) => e.clone(),
            (None, Some(s)) => format!("(checkout {s})"),
            (None, None) => "(a checkout from before agit partitioned the harness — agit cannot tell which)".to_string(),
        }
    }
}

/// Read the partition at `dir`, if it actually holds a captured harness.
fn load_partition(dir: PathBuf, slug: Option<String>) -> Option<Partition> {
    if !dir.join("project").is_dir() {
        return None;
    }
    let m = std::fs::read_to_string(dir.join("manifest.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let field = |k: &str| m.as_ref().and_then(|v| v.get(k)).and_then(|v| v.as_str()).map(str::to_string);
    Some(Partition { env: field("env"), captured_at: field("captured_at"), dir, slug })
}

/// This checkout's own partition for `rt`.
fn own_partition(agent: &Path, env: &Path, rt: &str) -> Option<Partition> {
    let slug = env_slug(env);
    load_partition(agent.join("harness").join(&slug).join(rt), Some(slug))
}

/// Every partition for `rt` that is NOT this checkout's, newest capture first — including a
/// pre-partition flat `harness/<rt>/project`.
fn other_partitions(agent: &Path, env: &Path, rt: &str) -> Vec<Partition> {
    let mine = env_slug(env);
    let mut out = Vec::new();
    for e in std::fs::read_dir(agent.join("harness")).into_iter().flatten().filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        if name == mine || !e.path().is_dir() {
            continue;
        }
        // A runtime name at this level is the pre-partition layout. It cannot be mistaken for a slug:
        // a slug comes from an ABSOLUTE path, so it always starts with '-'.
        out.extend(if crate::session::runtimes().contains(&name.as_str()) {
            if name == rt { load_partition(e.path(), None) } else { None }
        } else {
            load_partition(e.path().join(rt), Some(name))
        });
    }
    out.sort_by(|a, b| b.captured_at.cmp(&a.captured_at).then(a.slug.cmp(&b.slug)));
    out
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
    let slug = env_slug(env);
    let dst = agent.join("harness").join(&slug).join(rt).join("project");

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
        write_manifest(agent, &slug, rt, env, &report)?;
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

fn write_manifest(agent: &Path, slug: &str, rt: &str, env: &Path, report: &Report) -> Result<()> {
    let redactions: Vec<Value> = report
        .redactions
        .iter()
        .map(|r| json!({ "file": r.file, "path": r.path, "hint": r.hint }))
        .collect();
    // `env` and `captured_at` are what a user is shown when a harness is adopted from another
    // checkout: the slug is lossy and orders nothing, so neither question is answerable without them.
    let manifest = json!({
        "runtime": rt,
        "scope": "project",
        "env": env.to_string_lossy(),
        "captured_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "files": report.files,
        "redactions": redactions,
    });
    let dir = agent.join("harness").join(slug).join(rt);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("manifest.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    Ok(())
}

// ── restore (phase 3): apply a captured harness back into the project ──

const PH_PREFIX: &str = "${AGIT_SECRET:";

/// The runtimes this agent has a harness for, alphabetically — captured in THIS checkout or in any
/// other. The union is deliberate: a runtime the agent only ever captured in another repo still
/// resolves here, and without that `apply` could never adopt one, which is the point of a shared store.
pub fn captured_runtimes(agent: &Path, env: &Path) -> Vec<&'static str> {
    crate::session::runtimes()
        .into_iter()
        .filter(|rt| own_partition(agent, env, rt).is_some() || !other_partitions(agent, env, rt).is_empty())
        .collect()
}

/// `agit harness show` — what's captured in the local Agent Store. Once the store is shared, "the
/// harness" has no single answer, so this reports this checkout's and every other checkout's: an agent
/// carries all of them, and one it captured next door is exactly what a user is looking for here.
pub fn show(agent: &Path, env: &Path, runtime: &str) -> Result<i32> {
    let rt = norm(runtime);
    let own = own_partition(agent, env, &rt);
    let others = other_partitions(agent, env, &rt);
    match &own {
        Some(p) => {
            outln!("Captured harness ({rt}, project scope) — this checkout, {}:", env.display());
            let (servers, skills, commands, secrets) = summarize(p);
            outln!("  MCP servers : {servers}");
            outln!("  skills      : {skills}");
            outln!("  commands    : {commands}");
            outln!("  secrets     : {secrets} redacted field(s) to provide on apply");
            outln!("\n  Apply into this project: agit harness apply");
        }
        None if others.is_empty() => {
            outln!("No captured harness for {rt}. Run `agit -a snap` to capture it.");
            return Ok(0);
        }
        // "nothing here" and "the agent has one" are both true, and printed as two flat statements they
        // read as contradicting each other — a dead end announced directly above the thing it denies.
        // One sentence, so the user reconciles nothing.
        None => outln!(
            "Nothing captured in this checkout yet ({}) — but this agent carries a {rt} harness from elsewhere:",
            env.display()
        ),
    }
    if !others.is_empty() {
        if own.is_some() {
            outln!("\nThis agent also has a {rt} harness from other checkouts:");
        }
        for p in &others {
            let (servers, skills, commands, secrets) = summarize(p);
            outln!("  {} — {servers} MCP server(s), {skills} skill(s), {commands} command(s), {secrets} secret(s){}", p.label(), when(p));
        }
        outln!(
            "\n  {}",
            match (own.is_some(), others.len()) {
                (true, _) => "Adopt one instead of this checkout's: agit harness apply --from-env <checkout>".to_string(),
                // several is a question apply will ask rather than guess, so name the flag that answers it
                (false, 1) => "Adopt it here: agit harness apply    (the next snap gives this checkout its own)".to_string(),
                (false, n) => format!("Adopt one of the {n} here: agit harness apply --from-env <checkout>"),
            }
        );
    }
    Ok(0)
}

/// Pick the partition `apply` acts on. One candidate is an answer; several is a question, and agit asks
/// it rather than guessing — recency is not intent, and adopting the wrong repo's config unasked is the
/// ping-pong the design exists to prevent. `interactive` is passed in, not read from stdin here, so the
/// choice is testable and so a non-TTY caller can never be left at a prompt.
fn select(
    own: Option<Partition>,
    others: Vec<Partition>,
    env: &Path,
    from_env: Option<&str>,
    rt: &str,
    interactive: bool,
) -> Result<Option<(Partition, bool)>> {
    use std::io::{stdin, stdout, BufRead, Write};
    if let Some(sel) = from_env {
        let want = env_slug(Path::new(sel));
        let all = own.into_iter().map(|p| (p, true)).chain(others.into_iter().map(|p| (p, false)));
        for (p, is_own) in all {
            if p.env.as_deref() == Some(sel) || p.slug.as_deref() == Some(sel) || p.slug.as_deref() == Some(want.as_str()) {
                return Ok(Some((p, is_own)));
            }
        }
        bail!("no {rt} harness captured in `{sel}`. `agit harness show` lists the checkouts that have one.");
    }
    // This checkout's own capture wins: it is the only candidate that is not another repo's config.
    if let Some(p) = own {
        return Ok(Some((p, true)));
    }
    match others.len() {
        0 => Ok(None),
        1 => Ok(Some((others.into_iter().next().unwrap(), false))),
        _ => {
            let labels: Vec<String> = others.iter().map(|p| p.label()).collect();
            if !interactive {
                bail!(
                    "Nothing captured in this checkout ({}), and this agent captured a {rt} harness in {} others:\n  {}\n\
                     Which of them this checkout should adopt is not agit's to guess — say which with --from-env <checkout>.",
                    env.display(),
                    others.len(),
                    labels.join("\n  ")
                );
            }
            outln!("Nothing captured in this checkout ({}). This agent captured a {rt} harness in:", env.display());
            for (i, p) in others.iter().enumerate() {
                outln!("  {}) {}{}", i + 1, p.label(), when(p));
            }
            print!("Adopt which? [1-{}]: ", others.len());
            let _ = stdout().flush();
            let mut line = String::new();
            stdin().lock().read_line(&mut line).ok();
            let pick = line.trim();
            labels
                .iter()
                .position(|l| l == pick)
                .or_else(|| pick.parse::<usize>().ok().filter(|n| (1..=others.len()).contains(n)).map(|n| n - 1))
                .map(|i| Some((others.into_iter().nth(i).unwrap(), false)))
                .ok_or_else(|| anyhow::anyhow!("no checkout picked; rerun with --from-env <checkout>"))
        }
    }
}

fn when(p: &Partition) -> String {
    p.captured_at.as_deref().map(|t| format!(", captured {t}")).unwrap_or_default()
}

/// `agit harness apply` — union-merge a captured harness into the current project, asking first
/// (decision 3) and resolving each redacted secret from its env var, else prompting at a TTY.
///
/// WHICH harness, now that a store is shared: this checkout's own capture if it has one — restoring
/// what was captured here is the only case that touches no other repo's config. Otherwise the harness
/// another checkout captured, which is the point of carrying an agent between repos, and is ALWAYS
/// announced: a config arriving from another repo unexplained is indistinguishable from a bug.
pub fn apply(agent: &Path, env: &Path, runtime: &str, force: bool, from_env: Option<&str>) -> Result<i32> {
    use std::io::{stdin, stdout, IsTerminal, Write};
    let rt = norm(runtime);
    let own = own_partition(agent, env, &rt);
    let others = other_partitions(agent, env, &rt);
    let Some((chosen, is_own)) = select(own, others, env, from_env, &rt, stdin().is_terminal())? else {
        outln!("No captured harness for {rt} to apply.");
        return Ok(0);
    };
    let src = chosen.project();
    let (servers, skills, commands, secrets) = summarize(&chosen);
    if !is_own {
        outln!("Adopting the harness this agent captured in {}{} — this checkout ({}) has none of its own.", chosen.label(), when(&chosen), env.display());
    }
    outln!("Captured harness ({rt}): {servers} MCP servers, {skills} skills, {commands} commands, {secrets} secret(s) to provide.");

    // Ask (decision 3): applying rewrites local .mcp.json / .claude — never silent.
    if !force {
        if !stdin().is_terminal() {
            outln!("Not applying: rerun at a terminal, or pass --force to apply non-interactively.");
            return Ok(0);
        }
        print!("Apply to this project? [y/N] ");
        let _ = stdout().flush();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut stdin().lock(), &mut line).ok();
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            outln!("Skipped.");
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

    outln!("\nApplied: {}", if applied.is_empty() { "(nothing)".into() } else { applied.join(", ") });
    for s in &skipped {
        outln!("  skipped: {s}");
    }
    for u in &unresolved {
        errln!("  ⚠ secret not provided: {u} — left as a placeholder in .mcp.json (set ${u} or edit it in)");
    }
    Ok(if unresolved.is_empty() { 0 } else { 1 })
}

/// Canonical runtime name, delegating to the one shared alias map (`adapter::normalize`). Unknown
/// names pass through unchanged, preserving the prior behavior.
fn norm(runtime: &str) -> String {
    crate::adapter::normalize(runtime).map(str::to_string).unwrap_or_else(|| runtime.to_string())
}

fn summarize(p: &Partition) -> (usize, usize, usize, usize) {
    let src = p.project();
    let servers = std::fs::read_to_string(src.join("mcp.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("mcpServers").and_then(|m| m.as_object()).map(|m| m.len()))
        .unwrap_or(0);
    let skills = count_dirs(&src.join("skills"));
    let commands = count_files(&src.join("commands"));
    let secrets = std::fs::read_to_string(p.dir.join("manifest.json"))
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
        .context("local .mcp.json is not a JSON object")?
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

    /// A valid-JSON-but-non-object local `.mcp.json` (e.g. `[]` or a bare string) must return an error,
    /// not panic (was `local.as_object_mut().unwrap()`).
    #[test]
    fn merge_mcp_with_non_object_local_errors_without_panicking() {
        let dir = std::env::temp_dir().join(format!("agit-merge-nonobj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let captured = r#"{"mcpServers":{"fs":{"command":"mcp-fs"}}}"#;
        for bad in [r#"[]"#, r#""just a string""#, "42"] {
            let local = dir.join(".mcp.json");
            std::fs::write(&local, bad).unwrap();
            let mut conflicts = Vec::new();
            let res = merge_mcp(captured, &local, &mut conflicts);
            assert!(res.is_err(), "non-object local `{bad}` must be an Err, got {res:?}");
        }
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

    // ── partitioning: one store, many checkouts ──

    fn with_mcp(dir: &Path, server: &str) {
        std::fs::write(dir.join(".mcp.json"), format!(r#"{{"mcpServers":{{"{server}":{{"command":"{server}-srv"}}}}}}"#)).unwrap();
    }

    fn captured_mcp(store: &Path, env: &Path) -> String {
        let p = own_partition(store, env, "claude-code").expect("checkout has no partition");
        std::fs::read_to_string(p.project().join("mcp.json")).unwrap()
    }

    /// THE bug: one store shared by two checkouts. Under the flat `harness/<rt>/project` layout the
    /// second capture silently overwrote the first checkout's MCP config, and told nobody.
    #[test]
    fn two_checkouts_capture_without_overwriting_each_other() {
        let store = tempfile::tempdir().unwrap();
        let web = tempfile::tempdir().unwrap();
        let api = tempfile::tempdir().unwrap();
        with_mcp(web.path(), "web");
        with_mcp(api.path(), "api");

        capture(store.path(), web.path(), "claude-code").unwrap();
        capture(store.path(), api.path(), "claude-code").unwrap();

        assert!(captured_mcp(store.path(), web.path()).contains("web-srv"));
        assert!(!captured_mcp(store.path(), web.path()).contains("api-srv"), "api's capture clobbered web's");
        assert!(captured_mcp(store.path(), api.path()).contains("api-srv"));
        assert!(!captured_mcp(store.path(), api.path()).contains("web-srv"), "web's capture clobbered api's");

        // each partition records the checkout it came from — the slug is lossy and names no one
        let m = std::fs::read_to_string(
            store.path().join("harness").join(env_slug(web.path())).join("claude-code/manifest.json"),
        )
        .unwrap();
        let m: Value = serde_json::from_str(&m).unwrap();
        assert_eq!(m["env"], json!(web.path().to_string_lossy()));
        assert!(m["captured_at"].is_string());
    }

    /// A store written before the per-checkout layout keeps resolving forever: its files are never
    /// moved, so a reader that only knew the new layout would report an existing harness as missing.
    #[test]
    fn flat_layout_store_still_resolves() {
        let store = tempfile::tempdir().unwrap();
        let env = tempfile::tempdir().unwrap();
        let flat = store.path().join("harness/claude-code");
        std::fs::create_dir_all(flat.join("project")).unwrap();
        std::fs::write(flat.join("project/mcp.json"), r#"{"mcpServers":{"old":{"command":"old-srv"}}}"#).unwrap();
        std::fs::write(
            flat.join("manifest.json"),
            r#"{"runtime":"claude-code","scope":"project","files":1,"redactions":[{"file":"mcp.json","path":"x","hint":"TOK"}]}"#,
        )
        .unwrap();

        assert_eq!(captured_runtimes(store.path(), env.path()), vec!["claude-code"]);
        let found = other_partitions(store.path(), env.path(), "claude-code");
        assert_eq!(found.len(), 1);
        assert_eq!(summarize(&found[0]), (1, 0, 0, 1));
        // a flat store records no checkout, so it is never claimed to be this one
        assert!(own_partition(store.path(), env.path(), "claude-code").is_none());
        assert!(found[0].label().contains("before agit partitioned"));
    }

    /// Adopting what the agent captured next door is the point of a shared store — and it is named,
    /// because a config arriving from another repo unexplained looks exactly like a bug.
    #[test]
    fn apply_adopts_the_other_checkouts_harness_and_names_it() {
        let store = tempfile::tempdir().unwrap();
        let web = tempfile::tempdir().unwrap();
        let api = tempfile::tempdir().unwrap();
        with_mcp(web.path(), "web");
        capture(store.path(), web.path(), "claude-code").unwrap();

        let (chosen, is_own) = select(
            own_partition(store.path(), api.path(), "claude-code"),
            other_partitions(store.path(), api.path(), "claude-code"),
            api.path(),
            None,
            "claude-code",
            false,
        )
        .unwrap()
        .unwrap();
        assert!(!is_own);
        assert_eq!(chosen.label(), web.path().to_string_lossy());

        assert_eq!(apply(store.path(), api.path(), "claude-code", true, None).unwrap(), 0);
        assert!(std::fs::read_to_string(api.path().join(".mcp.json")).unwrap().contains("web-srv"));
    }

    #[test]
    fn apply_prefers_this_checkouts_own_partition() {
        let store = tempfile::tempdir().unwrap();
        let web = tempfile::tempdir().unwrap();
        let api = tempfile::tempdir().unwrap();
        with_mcp(web.path(), "web");
        with_mcp(api.path(), "api");
        capture(store.path(), web.path(), "claude-code").unwrap();
        capture(store.path(), api.path(), "claude-code").unwrap();

        let (chosen, is_own) = select(
            own_partition(store.path(), api.path(), "claude-code"),
            other_partitions(store.path(), api.path(), "claude-code"),
            api.path(),
            None,
            "claude-code",
            false,
        )
        .unwrap()
        .unwrap();
        assert!(is_own);
        assert_eq!(chosen.label(), api.path().to_string_lossy());
    }

    /// Several candidates is a real question with no right answer, so agit asks it. Non-interactive it
    /// refuses and says what needed deciding, rather than hang or pick one.
    #[test]
    fn apply_will_not_guess_between_several_other_checkouts() {
        let store = tempfile::tempdir().unwrap();
        let web = tempfile::tempdir().unwrap();
        let api = tempfile::tempdir().unwrap();
        let docs = tempfile::tempdir().unwrap();
        with_mcp(web.path(), "web");
        with_mcp(api.path(), "api");
        capture(store.path(), web.path(), "claude-code").unwrap();
        capture(store.path(), api.path(), "claude-code").unwrap();

        let pick = |from: Option<&str>| {
            select(
                own_partition(store.path(), docs.path(), "claude-code"),
                other_partitions(store.path(), docs.path(), "claude-code"),
                docs.path(),
                from,
                "claude-code",
                false,
            )
        };
        let err = pick(None).unwrap_err().to_string();
        assert!(err.contains("--from-env"), "{err}");
        assert!(err.contains(&*web.path().to_string_lossy()), "{err}");
        assert!(err.contains(&*api.path().to_string_lossy()), "{err}");

        // …and --from-env answers it without a prompt
        let (chosen, is_own) = pick(Some(&web.path().to_string_lossy())).unwrap().unwrap();
        assert!(!is_own);
        assert_eq!(chosen.label(), web.path().to_string_lossy());
        assert!(pick(Some("/no/such/checkout")).unwrap_err().to_string().contains("no claude-code harness captured"));
    }
}
