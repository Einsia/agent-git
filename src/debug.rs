//! `agit doctor` + `agit debug` — the local diagnostic bundle (design:
//! docs/plans/2026-07-22-debug-info-collection-design.md, Wave A).
//!
//! `doctor` is the fast health check a user pastes into a bug report; `debug` is the full, redacted
//! bundle they attach. Both collect the SAME state; `debug` writes it to a directory (sectioned text +
//! a machine-readable `debug.json`) with the doctor summary on top, and echoes it so the user reviews
//! before sending. NOTHING is uploaded.
//!
//! Safety is the whole point: every value passes through TWO redaction layers before it is written or
//! printed — (1) explicit field masks (every `AGIT_*` value blanked, every URL's `user:pass` stripped
//! via [`crate::hubapi::redact_url`]), and (2) a FINAL pass of the ENTIRE assembled bundle through
//! agit's real secret scanner ([`crate::scan::scan_text`], the same one the commit/push gate uses), so
//! anything that slipped into a log line or a config value is caught and masked. We reuse the scanner;
//! we never write a second one.

use crate::{adapter, agent, commands, hubapi, scan, scope, session, shadow, ui};
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ─────────────────────────────── redaction ───────────────────────────────

use once_cell::sync::Lazy;
use regex::Regex;

/// A URL-ish token anywhere in a line — matched so its `user:pass@` userinfo can be stripped by
/// [`hubapi::redact_url`] while the host is kept. Deliberately broad (any `scheme://…`) so a credential
/// in a git remote line, a `.agit.toml`, or a git-config value never escapes.
static URL_TOKEN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s\x22'<>|]+").unwrap());

/// Layer 1a: strip `user:pass@` from every URL in a string, keeping the host (design: "keep host, drop
/// user:pass"). Applied to every string leaf, so it is harmless on non-URL text.
fn strip_url_creds(s: &str) -> String {
    URL_TOKEN.replace_all(s, |c: &regex::Captures| hubapi::redact_url(&c[0])).into_owned()
}

/// Layer 2: the mandatory final pass. Runs agit's real secret scanner over `text`; any line it flags is
/// replaced whole by a marker, so the raw secret can never reach the file or the terminal. Keeping the
/// replacement line-granular means a single leaked value blanks only its own line, and — crucially for
/// `debug.json` — when this runs on a JSON string value BEFORE serialization, serde re-escapes the
/// result, so the JSON stays valid in the normal (no-finding) case.
fn scan_scrub(text: &str) -> String {
    let findings = scan::scan_text(text);
    if findings.is_empty() {
        return text.to_string();
    }
    let bad: HashSet<usize> = findings.iter().map(|f| f.line).collect();
    text.split('\n')
        .enumerate()
        .map(|(i, line)| {
            if bad.contains(&(i + 1)) {
                "[redacted: agit secret scanner flagged this line]".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Both layers on one string: strip URL credentials, then the scanner net.
fn redact_str(s: &str) -> String {
    scan_scrub(&strip_url_creds(s))
}

/// Recursively apply [`redact_str`] to every string leaf of a JSON value — the "final pass of the whole
/// assembled bundle" over the machine-readable half.
fn redact_value(v: &mut Value) {
    match v {
        Value::String(s) => *s = redact_str(s),
        Value::Array(a) => a.iter_mut().for_each(redact_value),
        Value::Object(m) => m.values_mut().for_each(redact_value),
        _ => {}
    }
}

// ─────────────────────────────── small process/system helpers ───────────────────────────────

/// Run a command and return its trimmed stdout on success, else `None`. Used only for fast, bounded
/// version probes (`rustc --version`, `node --version`, …), never for anything that can block.
fn cmd_out(bin: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// First directory on `$PATH` holding an executable named `cmd`, if any — a dependency-free `which`.
fn which(cmd: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(cmd);
        if p.is_file() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

/// The CLI binary names that back a registered runtime. Iterated from the adapter registry so a new
/// adapter is covered for free; the alias list is deliberately NOT reused for PATH probing because some
/// aliases (`cc`) collide with unrelated system tools.
fn runtime_binaries(rt: &str) -> Vec<&'static str> {
    match rt {
        "claude-code" => vec!["claude"],
        "codex" => vec!["codex"],
        _ => vec![],
    }
}

/// A short OS description: `/etc/os-release` PRETTY_NAME when present, else `uname -s`, else the compile
/// target OS.
fn os_pretty() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
        })
        .or_else(|| cmd_out("uname", &["-s"]))
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}

/// Total byte size of a directory tree (best effort; unreadable entries are skipped).
fn dir_size(root: &Path) -> u64 {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn env_or(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

// ─────────────────────────────── health checks ───────────────────────────────

/// One health-check line: `ok` renders `OK`, otherwise `WARN`.
struct Check {
    ok: bool,
    label: String,
    detail: String,
}

impl Check {
    fn ok(label: impl Into<String>, detail: impl Into<String>) -> Check {
        Check { ok: true, label: label.into(), detail: detail.into() }
    }
    fn warn(label: impl Into<String>, detail: impl Into<String>) -> Check {
        Check { ok: false, label: label.into(), detail: detail.into() }
    }
    fn to_value(&self) -> Value {
        json!({ "status": if self.ok { "OK" } else { "WARN" }, "check": self.label, "detail": self.detail })
    }
}

/// The committer identity a snap/commit would use, from the current directory's git config
/// (local → global), exactly as `git config user.email` resolves it. Empty email ⇒ snap refuses.
fn git_identity() -> (String, String) {
    let email = cmd_out("git", &["config", "user.email"]).unwrap_or_default();
    let name = cmd_out("git", &["config", "user.name"]).unwrap_or_default();
    (email, name)
}

/// Run every health check. Returns the check lines plus the resolved `(env_root, active agent)` so the
/// caller reuses them instead of resolving twice.
fn run_checks() -> (Vec<Check>, Option<PathBuf>, Option<agent::Agent>) {
    let mut checks = Vec::new();
    let env = scope::env_root().ok();
    let active = agent::resolve(None).ok();

    // 1) runtimes on PATH, via the adapter registry.
    for rt in adapter::names() {
        let found = runtime_binaries(rt).into_iter().find_map(|b| which(b).map(|p| (b, p)));
        match found {
            Some((bin, path)) => checks.push(Check::ok(
                format!("runtime {rt}"),
                format!("`{bin}` on PATH ({path})"),
            )),
            None => checks.push(Check::warn(
                format!("runtime {rt}"),
                format!("not on PATH; install it or add it to $PATH to capture {rt} sessions"),
            )),
        }
    }

    // 2) git committer identity — snap refuses to attribute a session without one.
    let (email, name) = git_identity();
    if email.is_empty() {
        checks.push(Check::warn(
            "git identity",
            "user.email unset; set it: git config --global user.email you@example.com",
        ));
    } else {
        let who = if name.is_empty() { email.clone() } else { format!("{name} <{email}>") };
        checks.push(Check::ok("git identity", who));
    }

    // 3) $AGIT_HOME resolvable.
    match scope::agit_home() {
        Ok(h) => checks.push(Check::ok("agit home", h.display().to_string())),
        Err(e) => checks.push(Check::warn("agit home", format!("unresolvable: {e}"))),
    }

    // 4) an agent bound to this repo.
    match &active {
        Some(a) => checks.push(Check::ok("active agent", format!("{} ({})", a.name, a.aid))),
        None => checks.push(Check::warn(
            "active agent",
            "no agent bound here; run `agit init --agent <name>` or `agit a init <name>`",
        )),
    }

    // 5) active store's position vs its remote.
    if let Some(a) = &active {
        match commands::ahead_behind(&a.store) {
            None => checks.push(Check::warn(
                "store vs remote",
                "no upstream yet; `agit a push` to publish",
            )),
            Some((0, 0)) => checks.push(Check::ok("store vs remote", "up to date")),
            Some((ahead, 0)) => checks.push(Check::warn(
                "store vs remote",
                format!("{ahead} unpushed; `agit a push` to publish"),
            )),
            Some((0, behind)) => checks.push(Check::warn(
                "store vs remote",
                format!("{behind} behind; `agit a pull` to integrate"),
            )),
            Some((ahead, behind)) => checks.push(Check::warn(
                "store vs remote",
                format!("{ahead} ahead, {behind} behind (diverged); `agit a merge` to reconcile"),
            )),
        }
    }

    // 6) watch daemon state (informational; not-running is not a problem).
    let watch_pid = active.as_ref().and_then(|a| session::watching_pid(&a.aid));
    match watch_pid {
        Some(pid) => checks.push(Check::ok("watch daemon", format!("watching (pid {pid})"))),
        None => checks.push(Check::ok("watch daemon", "not running (optional)")),
    }

    (checks, env, active)
}

// ─────────────────────────────── environment block ───────────────────────────────

/// The compact environment block printed under the doctor checks and stored as `platform` in the
/// bundle. `env` is the resolved code-repo root (for the active agent line).
fn platform_block(active: &Option<agent::Agent>) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    let push = |rows: &mut Vec<(String, String)>, k: &str, v: String| rows.push((k.to_string(), v));

    push(&mut rows, "agit version", env!("CARGO_PKG_VERSION").to_string());
    push(
        &mut rows,
        "build sha",
        option_env!("AGIT_BUILD_SHA").unwrap_or("unknown").to_string(),
    );
    push(&mut rows, "rustc", cmd_out("rustc", &["--version"]).unwrap_or_else(|| "not found".into()));
    push(&mut rows, "edition", "2021".to_string());
    push(&mut rows, "os", os_pretty());
    push(&mut rows, "kernel", cmd_out("uname", &["-r"]).unwrap_or_default());
    push(&mut rows, "arch", std::env::consts::ARCH.to_string());
    push(&mut rows, "shell", env_or("SHELL"));
    push(&mut rows, "term", env_or("TERM"));
    let locale = {
        let l = env_or("LC_ALL");
        if l.is_empty() { env_or("LANG") } else { l }
    };
    push(&mut rows, "locale", locale);
    push(&mut rows, "node", cmd_out("node", &["--version"]).unwrap_or_else(|| "not found".into()));
    push(&mut rows, "npm", cmd_out("npm", &["--version"]).unwrap_or_else(|| "not found".into()));
    push(
        &mut rows,
        "agit home",
        scope::agit_home().map(|h| h.display().to_string()).unwrap_or_else(|_| "unresolvable".into()),
    );
    push(
        &mut rows,
        "active agent",
        active.as_ref().map(|a| format!("{} ({})", a.name, a.aid)).unwrap_or_else(|| "none".into()),
    );
    rows
}

fn rows_to_value(rows: &[(String, String)]) -> Value {
    let mut m = Map::new();
    for (k, v) in rows {
        m.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(m)
}

// ─────────────────────────────── section collectors (debug bundle) ───────────────────────────────

fn collect_runtimes(env: Option<&Path>) -> Value {
    let mut installed = Map::new();
    for rt in adapter::names() {
        let path = runtime_binaries(rt).into_iter().find_map(which);
        let version = path.as_deref().and_then(|_| {
            let bin = runtime_binaries(rt).into_iter().next()?;
            cmd_out(bin, &["--version"])
        });
        installed.insert(
            rt.to_string(),
            json!({
                "installed": path.is_some(),
                "path": path.unwrap_or_default(),
                "version": version.unwrap_or_default(),
            }),
        );
    }
    let adapters: Vec<Value> =
        adapter::list().into_iter().map(|(n, d)| json!({ "name": n, "desc": d })).collect();
    let live: Vec<&str> = env.map(session::live_runtimes).unwrap_or_default();
    json!({
        "runtimes": Value::Object(installed),
        "adapters": adapters,
        "live_here": live,
    })
}

fn collect_git(env: Option<&Path>) -> Value {
    let (email, name) = git_identity();
    let mut obj = json!({
        "git_version": cmd_out("git", &["--version"]).unwrap_or_default(),
        "user_email": email,
        "user_name": name,
        "is_repo": env.is_some(),
    });
    if let Some(root) = env {
        let g = |args: &[&str]| scope::git_in_status(root, args).1;
        let porcelain = g(&["status", "--porcelain"]);
        let changed = porcelain.lines().filter(|l| !l.is_empty()).count();
        let untracked = porcelain.lines().filter(|l| l.starts_with("??")).count();
        // Remotes with credentials stripped (redact_value strips them again as a backstop).
        let remotes: Vec<String> = g(&["remote", "-v"]).lines().map(strip_url_creds).collect();
        let relevant_config: Vec<String> =
            g(&["config", "--get-regexp", r"^(core|credential)\."]).lines().map(str::to_string).collect();
        let map = obj.as_object_mut().unwrap();
        map.insert("toplevel".into(), json!(root.display().to_string()));
        map.insert("branch".into(), json!(g(&["rev-parse", "--abbrev-ref", "HEAD"])));
        map.insert("head".into(), json!(g(&["rev-parse", "HEAD"])));
        map.insert("changed_files".into(), json!(changed));
        map.insert("untracked_files".into(), json!(untracked));
        map.insert("remotes".into(), json!(remotes));
        map.insert("relevant_config".into(), json!(relevant_config));
    }
    obj
}

fn collect_agit_env(env: Option<&Path>) -> Value {
    // Every AGIT_* variable: name always, value ALWAYS redacted (a value like `sk-LEAKTEST` is too
    // short for any scanner rule, so the field mask is the only thing standing between it and the
    // bundle — mask unconditionally).
    let mut agit_vars: BTreeMap<String, String> = BTreeMap::new();
    for (k, _v) in std::env::vars() {
        if k.starts_with("AGIT_") {
            agit_vars.insert(k, "[redacted]".to_string());
        }
    }

    // .agit.toml with URL credentials stripped, read RAW (not parsed) so a stray token line is still
    // present for the scanner net to catch.
    let agit_toml = env
        .map(|r| r.join(agent::BINDING_FILE))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| strip_url_creds(&t))
        .unwrap_or_default();

    let active_ptr = env
        .and_then(|r| agent::read_active(r).ok().flatten())
        .unwrap_or_default();

    let shadow: Vec<Value> = shadow::status_lines()
        .into_iter()
        .map(|(label, path)| json!({ "shell": label, "profile": path }))
        .collect();

    json!({
        "agit_home": scope::agit_home().map(|h| h.display().to_string()).unwrap_or_default(),
        "home": env_or("HOME"),
        "agit_vars": agit_vars,
        "agit_toml": agit_toml,
        "active_pointer": active_ptr,
        "shadow_installed": shadow,
    })
}

fn collect_store(active: Option<&agent::Agent>) -> Value {
    let Some(a) = active else {
        return json!({ "resolved": false, "detail": "no active agent to inspect" });
    };
    let store = &a.store;
    let g = |args: &[&str]| scope::git_in_status(store, args).1;

    // Session count per (env-slug, runtime).
    let sessions = commands::store_sessions(store);
    let mut per_env: BTreeMap<String, usize> = BTreeMap::new();
    for s in &sessions {
        let key = format!("{}/{}", s.env_slug.as_deref().unwrap_or("(flat)"), s.runtime);
        *per_env.entry(key).or_insert(0) += 1;
    }

    let (ahead, behind) = commands::ahead_behind(store).unwrap_or((0, 0));
    let remotes: Vec<String> = agent::store_remotes(store)
        .into_iter()
        .map(|(n, url)| format!("{n} {}", hubapi::redact_url(&url)))
        .collect();

    let others: Vec<Value> = agent::list()
        .unwrap_or_default()
        .into_iter()
        .map(|x| json!({ "name": x.name, "aid": x.aid, "sessions": commands::store_sessions(&x.store).len() }))
        .collect();

    json!({
        "resolved": true,
        "aid": a.aid,
        "name": a.name,
        "path": store.display().to_string(),
        "committer_email": commands::committer_email(store),
        "log": g(&["log", "--oneline", "-10"]),
        "status": g(&["status", "--short", "--branch"]),
        "remotes": remotes,
        "ahead": ahead,
        "behind": behind,
        "session_count": sessions.len(),
        "sessions_per_env": per_env,
        "store_bytes": dir_size(store),
        "agents": others,
    })
}

fn collect_runtime_dumps(env: Option<&Path>) -> Value {
    // Layout + COUNTS only — never session contents (design). Whether THIS repo's slug dir exists is
    // the wrong-cwd class of bug.
    let claude = {
        let dir = adapter::claude_code::projects_dir().ok();
        let slug_dir = env.and_then(|e| {
            adapter::claude_code::projects_dir().ok().map(|d| d.join(adapter::claude_code::slug_for(e)))
        });
        let (projects, transcripts, newest) =
            dir.as_deref().map(count_tree).unwrap_or((0, 0, String::new()));
        json!({
            "root": dir.map(|d| d.display().to_string()).unwrap_or_default(),
            "project_dirs": projects,
            "transcripts": transcripts,
            "newest_mtime": newest,
            "this_repo_slug_dir_exists": slug_dir.as_deref().map(Path::exists).unwrap_or(false),
            "this_repo_slug_dir": slug_dir.map(|d| d.display().to_string()).unwrap_or_default(),
        })
    };
    let codex = {
        let root = adapter::codex::sessions_root().ok();
        let (dirs, rollouts, newest) = root.as_deref().map(count_tree).unwrap_or((0, 0, String::new()));
        json!({
            "root": root.map(|d| d.display().to_string()).unwrap_or_default(),
            "date_dirs": dirs,
            "rollouts": rollouts,
            "newest_mtime": newest,
        })
    };
    json!({ "claude_code": claude, "codex": codex })
}

/// `(immediate subdir count, total .jsonl file count, newest .jsonl mtime as RFC3339)` for a runtime
/// dump root. Contents are never read — only names, counts, and mtimes.
fn count_tree(root: &Path) -> (usize, usize, String) {
    let subdirs = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .count();
    let mut files = 0usize;
    let mut newest: Option<std::time::SystemTime> = None;
    for e in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if e.file_type().is_file() && e.path().extension().map(|x| x == "jsonl").unwrap_or(false) {
            files += 1;
            if let Some(m) = e.metadata().ok().and_then(|m| m.modified().ok()) {
                if newest.map(|n| m > n).unwrap_or(true) {
                    newest = Some(m);
                }
            }
        }
    }
    let newest = newest
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
        .unwrap_or_default();
    (subdirs, files, newest)
}

fn collect_daemon(env: Option<&Path>, active: Option<&agent::Agent>) -> Value {
    // The daemon writes its pid + log under `<env>/.agit/` (agit-watch.pid / agit-watch.log).
    let rundir = env.map(|e| e.join(".agit"));
    let pid = rundir
        .as_deref()
        .and_then(|d| std::fs::read_to_string(d.join("agit-watch.pid")).ok())
        .and_then(|s| s.trim().parse::<u32>().ok());
    let logp = rundir.as_deref().map(|d| d.join("agit-watch.log"));
    let log_tail = logp
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
    let watching = active.and_then(|a| session::watching_pid(&a.aid));
    json!({
        "env_pidfile_pid": pid,
        "watching_pid": watching,
        "log_path": logp.map(|p| p.display().to_string()).unwrap_or_default(),
        "log_tail": log_tail,
    })
}

fn collect_connectivity(active: Option<&agent::Agent>) -> Value {
    let remotes: Vec<String> = active
        .map(|a| {
            agent::store_remotes(&a.store)
                .into_iter()
                .map(|(n, url)| format!("{n} {}", hubapi::redact_url(&url)))
                .collect()
        })
        .unwrap_or_default();

    // The hub reachability probe. Resolve the endpoint (env override or the agent's primary remote);
    // if there is none, there is nothing to reach — not an error. The base carries no credential
    // (parse strips userinfo), so it is safe to print. We GET `/api/version` (Wave B) UNAUTHENTICATED —
    // we only want reachability + TLS + the correlation id, not the body — and record the
    // `X-Request-Id` of the response for cross-referencing with server logs. If the hub does not send
    // one yet, the field stays empty; a probe failure is reported, never fatal.
    let hub = match hubapi::HubEndpoint::resolve() {
        Err(_) => json!({ "configured": false }),
        Ok(ep) => {
            let is_tls = ep.base.starts_with("https://");
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(4)))
                .build()
                .into();
            let url = format!("{}/api/version", ep.base);
            let (reachable, status, request_id, error) = match agent.get(&url).call() {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let rid = resp
                        .headers()
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    (true, status, rid, String::new())
                }
                Err(e) => (false, 0u16, String::new(), e.to_string()),
            };
            json!({
                "configured": true,
                "hub_url": ep.base,
                "token_present": ep.auth.is_some(),
                "tls": is_tls,
                "reachable": reachable,
                "http_status": status,
                "failed_request_id": request_id,
                "error": error,
            })
        }
    };
    json!({ "store_remotes": remotes, "hub": hub })
}

/// `--rerun "<subcmd>"`: re-run an agit subcommand under `RUST_LOG=debug RUST_BACKTRACE=full` and
/// capture stdout+stderr+exit code — usually the single most useful artifact. Runs THIS binary
/// (`current_exe`) so the reproduction matches exactly what the user is on.
fn collect_rerun(cmd: &str) -> Value {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return json!({ "command": cmd, "error": format!("cannot locate agit binary: {e}") }),
    };
    let args: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
    let out = std::process::Command::new(&exe)
        .args(&args)
        .env("RUST_LOG", "debug")
        .env("RUST_BACKTRACE", "full")
        .output();
    match out {
        Ok(o) => json!({
            "command": format!("agit {cmd}"),
            "exit_code": o.status.code(),
            "stdout": String::from_utf8_lossy(&o.stdout),
            "stderr": String::from_utf8_lossy(&o.stderr),
        }),
        Err(e) => json!({ "command": format!("agit {cmd}"), "error": e.to_string() }),
    }
}

// ─────────────────────────────── assembly + rendering ───────────────────────────────

/// The complete, already-field-masked bundle as a JSON value, PLUS the doctor summary rows. The FINAL
/// scanner pass has NOT run yet — the caller applies [`redact_value`] before anything is written.
fn build_bundle(rerun: Option<&str>) -> (Value, Vec<Check>, Vec<(String, String)>) {
    let (checks, env, active) = run_checks();
    let envp = env.as_deref();
    let platform = platform_block(&active);

    let mut root = Map::new();
    root.insert("agit_version".into(), json!(env!("CARGO_PKG_VERSION")));
    root.insert("generated_at".into(), json!(chrono::Utc::now().to_rfc3339()));
    root.insert(
        "summary".into(),
        json!({
            "checks": checks.iter().map(Check::to_value).collect::<Vec<_>>(),
            "environment": rows_to_value(&platform),
        }),
    );
    root.insert("platform".into(), rows_to_value(&platform));
    root.insert("runtimes".into(), collect_runtimes(envp));
    root.insert("git".into(), collect_git(envp));
    root.insert("agit_env".into(), collect_agit_env(envp));
    root.insert("store".into(), collect_store(active.as_ref()));
    root.insert("runtime_dumps".into(), collect_runtime_dumps(envp));
    root.insert("daemon".into(), collect_daemon(envp, active.as_ref()));
    root.insert("connectivity".into(), collect_connectivity(active.as_ref()));
    if let Some(cmd) = rerun {
        root.insert("rerun".into(), collect_rerun(cmd));
    }

    (Value::Object(root), checks, platform)
}

/// Render any JSON value as readable, indented `key: value` text for a `.txt` section.
fn render(v: &Value, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    match v {
        Value::Object(m) => {
            let mut out = String::new();
            for (k, vv) in m {
                match vv {
                    Value::Object(_) | Value::Array(_) => {
                        out.push_str(&format!("{pad}{k}:\n{}", render(vv, indent + 1)));
                    }
                    _ => out.push_str(&format!("{pad}{k}: {}\n", scalar(vv))),
                }
            }
            out
        }
        Value::Array(a) => {
            if a.is_empty() {
                return format!("{pad}(none)\n");
            }
            let mut out = String::new();
            for item in a {
                match item {
                    Value::Object(_) | Value::Array(_) => {
                        out.push_str(&format!("{pad}-\n{}", render(item, indent + 1)));
                    }
                    _ => out.push_str(&format!("{pad}- {}\n", scalar(item))),
                }
            }
            out
        }
        _ => format!("{pad}{}\n", scalar(v)),
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The `SUMMARY.txt` text (also printed to the terminal by `doctor`).
fn render_summary(checks: &[Check], platform: &[(String, String)]) -> String {
    let mut s = String::new();
    s.push_str("agit doctor — health check\n\n");
    for c in checks {
        let tag = if c.ok { "OK  " } else { "WARN" };
        s.push_str(&format!("  [{tag}] {}: {}\n", c.label, c.detail));
    }
    s.push_str("\nenvironment\n");
    for (k, v) in platform {
        s.push_str(&format!("  {k}: {v}\n"));
    }
    s
}

// ─────────────────────────────── public entry points ───────────────────────────────

/// `agit doctor` — the fast, human-readable health check + environment summary. Always exits 0 (it is a
/// report, not a gate); WARN lines tell the user what to fix. Both halves pass through the scanner net
/// before printing.
pub fn doctor() -> Result<i32> {
    let (checks, _env, active) = run_checks();
    let platform = platform_block(&active);
    let text = scan_scrub(&render_summary(&checks, &platform));
    print!("{text}");
    if checks.iter().any(|c| !c.ok) {
        println!("\nRun `agit debug` for the full, redacted bundle to attach to a bug report.");
    }
    Ok(0)
}

/// `agit debug [--out <dir>] [--rerun "<cmd>"]` — the full bundle. Collects as much as possible, runs
/// BOTH redaction layers over everything, writes a sectioned directory (`SUMMARY.txt`, `platform.txt`,
/// `git.txt`, `agit.txt`, `store.txt`, `runtimes.txt`, `connectivity.txt`, `rerun.txt`) plus
/// `debug.json`, and ECHOES the summary so the user reviews before sending. NOTHING is uploaded.
pub fn debug_cmd(out: Option<&str>, rerun: Option<&str>) -> Result<i32> {
    let (mut bundle, checks, platform) = build_bundle(rerun);

    // THE final pass: scrub every string leaf of the whole assembled bundle through the real scanner.
    redact_value(&mut bundle);

    // The output directory: a timestamped dir in the cwd unless the user named one.
    let dir = match out {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(format!("agit-debug-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"))),
    };
    std::fs::create_dir_all(&dir)?;

    // SUMMARY.txt (doctor output on top), then debug.json, then one .txt per section. Every text file
    // is scrubbed once more as a whole (belt-and-suspenders over the assembled bytes).
    let summary_text = scan_scrub(&render_summary(&checks, &platform));
    let obj = bundle.as_object().expect("bundle root is an object");

    let sections: &[(&str, &[&str])] = &[
        ("platform.txt", &["platform"]),
        ("runtimes.txt", &["runtimes", "runtime_dumps", "daemon"]),
        ("git.txt", &["git"]),
        ("agit.txt", &["agit_env"]),
        ("store.txt", &["store"]),
        ("connectivity.txt", &["connectivity"]),
    ];

    std::fs::write(dir.join("SUMMARY.txt"), &summary_text)?;
    std::fs::write(dir.join("debug.json"), serde_json::to_string_pretty(&bundle)? + "\n")?;
    for (file, keys) in sections {
        let mut body = String::new();
        for k in *keys {
            if let Some(v) = obj.get(*k) {
                body.push_str(&format!("── {k} ──\n"));
                body.push_str(&render(v, 1));
                body.push('\n');
            }
        }
        std::fs::write(dir.join(file), scan_scrub(&body))?;
    }
    if let Some(v) = obj.get("rerun") {
        let body = format!("── rerun ──\n{}", render(v, 1));
        std::fs::write(dir.join("rerun.txt"), scan_scrub(&body))?;
    }

    // ECHO exactly what was collected so the user reviews before sending. Nothing leaves the machine.
    print!("{summary_text}");
    println!("\n{}", ui::bold(&format!("wrote debug bundle to {}", dir.display())));
    println!("  review it, then attach the directory (or a .tgz of it) to your bug report.");
    println!("  nothing was uploaded.");
    Ok(0)
}
