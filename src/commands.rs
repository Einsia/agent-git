//! agit native verbs (commands beyond passthrough where agit adds value).
//! Native commands under the session model: scan (secret gate), workspace (pairing), clone, adapter, convert.
//! See docs/architecture.md.

use crate::adapter;
use crate::agent;
use crate::scan;
use crate::scope::{self, Scope};
use crate::ui;
use crate::{errln, out, outln};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

// ─────────────────────── Adapter: runtime ↔ AgentState ───────────────────────

pub fn adapter_list() -> Result<i32> {
    outln!("Registered runtime adapters:");
    for (name, desc) in adapter::list() {
        outln!("  {name:<14} {desc}");
    }
    Ok(0)
}

/// Scan for secrets within a scope. Scans the Agent Store's facts by default; --staged scans only what is staged.
pub fn scan_cmd(scope: Scope, staged: bool, paths: &[PathBuf]) -> Result<i32> {
    scan_root(&scope::root_for(scope)?, staged, paths)
}

/// hook-only: scan the git repo that cwd lives in, without going through scope discovery.
/// pre-commit/pre-push run inside the Agent Store, so cwd is it, and we scan it directly.
pub fn hook_scan(staged: bool) -> Result<i32> {
    let (_, top) = scope::git_in_status(std::path::Path::new("."), &["rev-parse", "--show-toplevel"]);
    let root = if top.is_empty() {
        std::env::current_dir()?
    } else {
        PathBuf::from(top)
    };
    scan_root(&root, staged, &[])
}

fn scan_root(root: &std::path::Path, staged: bool, paths: &[PathBuf]) -> Result<i32> {
    let allow = scan::Allowlist::load(root);
    let (findings, scanned) = if staged && paths.is_empty() {
        (staged_findings(root, &allow), 0usize)
    } else {
        let targets = scan_targets(root, paths);
        let n = targets.len();
        (tree_findings(root, &allow, &targets)?, n)
    };

    if !findings.is_empty() {
        errln!("Found suspected secrets:");
        for (name, f) in &findings {
            errln!("  {name}:{}  [{}]  {}", f.line, f.rule, f.excerpt);
        }
    }
    finish_scan(findings.len(), staged, scanned)
}

/// The files a whole-tree scan covers: every file in the store except `.git`, or an explicit path
/// list. An extension gate skipped .env/.pem/.key/.sh/.yaml and extensionless files, which hold
/// secrets just as well — binaries are detected by content in scan_file, not by name.
fn scan_targets(root: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    if !paths.is_empty() {
        return paths.iter().map(|p| root.join(p)).collect();
    }
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect()
}

/// Findings in the git INDEX — what a commit is about to write, not what sits in the working tree.
/// The filename comes from `git diff --cached`, but the CONTENT is read from the staged blob (`git
/// show :name`): if a secret is staged and the working tree is then reverted to a clean version
/// (git add -p / editing the transcript after staging), the secret still lands in the repo, and only
/// the blob shows it. `-z` separates with NUL and does no octal quoting, so odd filenames survive.
/// Entropy is on for everything: jsonl is parsed and only STRING VALUES are scanned, with shape
/// allowlists, so UUIDs/paths/requestIds no longer drown the signal (see scan.rs).
fn staged_findings(root: &Path, allow: &scan::Allowlist) -> Vec<(String, scan::Finding)> {
    let (_, out) = scope::git_in_status(
        root,
        &["diff", "--cached", "--name-only", "-z", "--diff-filter=ACM"],
    );
    let mut v = Vec::new();
    for name in out.split('\0').filter(|s| !s.is_empty()) {
        let (code, content) = scope::git_in_status(root, &["show", &format!(":{name}")]);
        if code != 0 {
            continue; // can't extract this blob (very rare), skip rather than abort
        }
        // Skip binary/encrypted blobs exactly as the whole-tree `scan_file_allow` path does (scan.rs).
        // On an agit-crypt store the staged blob is ciphertext: high-entropy bytes that would trip the
        // `high-entropy-string` rule on essentially every session and block every commit/push. The
        // AGITCRYPT magic's trailing NUL (and the binary AEAD tag) guarantee a NUL in the first bytes,
        // so `is_probably_binary` returns true and the gate no longer sees plaintext here — which is the
        // documented behaviour: the content is protected by encryption instead of by the scanner.
        if scan::is_probably_binary(content.as_bytes()) {
            continue;
        }
        for f in scan::scan_text_allow(&content, true, allow) {
            v.push((name.to_string(), f));
        }
    }
    v
}

/// Findings across a set of working-tree files — what a whole-tree `agit scan` covers.
fn tree_findings(
    root: &Path,
    allow: &scan::Allowlist,
    targets: &[PathBuf],
) -> Result<Vec<(String, scan::Finding)>> {
    let mut v = Vec::new();
    for t in targets {
        if !t.exists() {
            continue;
        }
        let rel = t.strip_prefix(root).unwrap_or(t).display().to_string();
        for f in scan::scan_file_allow(t, allow)? {
            v.push((rel.clone(), f));
        }
    }
    Ok(v)
}

/// The session blobs a push will PUBLISH: `(blob_sha, path)` for every blob under `sessions/` that is
/// reachable from HEAD but NOT already on the target remote(s). This is the object set git's own pack
/// negotiation would send, computed with `git rev-list --objects HEAD --not --remotes=<remote>`:
///
///  * First push (no remote-tracking ref for that remote yet) → `--not --remotes=<r>` excludes nothing,
///    so the range is everything reachable from HEAD.
///  * Nothing new (the remote already has HEAD) → the range is empty, so no blob is read and the whole
///    history is never rescanned.
///  * Multi-remote fan-out → the union over remotes of each remote's "new to it" set (deduped by sha),
///    i.e. everything that will be published to at least one remote.
///
/// The path prefix is filtered in code rather than via a `-- sessions` pathspec: a pathspec triggers
/// history simplification and can hide blob versions we must scan. Tree objects share the `sessions/`
/// prefix but are dropped later — `git cat-file blob` fails on them.
fn range_session_blobs(store: &Path, sources: &[String], remotes: &[String]) -> Vec<(String, String)> {
    // The TIPS to scan from are the source refs of the refspecs actually being pushed, NOT always HEAD:
    // `agit a push origin leak` publishes `leak`, so a HEAD-only scan would miss a secret on a non-HEAD
    // branch. An empty `sources` (a bare push, or no explicit refspec) means the current branch, HEAD.
    let tips: Vec<String> = if sources.is_empty() {
        vec!["HEAD".to_string()]
    } else {
        sources.to_vec()
    };
    // One rev-list per (tip, remote) = objects that tip has and the remote lacks; empty remotes = the
    // whole tip (a first push to a remote with no tracking ref).
    let mut ranges: Vec<Vec<String>> = Vec::new();
    for tip in &tips {
        if remotes.is_empty() {
            ranges.push(vec![tip.clone()]);
        } else {
            for r in remotes {
                ranges.push(vec![tip.clone(), "--not".to_string(), format!("--remotes={r}")]);
            }
        }
    }
    let prefix = format!("{}/", crate::session::SESSIONS_SUBDIR);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for range in &ranges {
        let mut args: Vec<&str> = vec!["rev-list", "--objects"];
        args.extend(range.iter().map(String::as_str));
        let (code, listing) = scope::git_in_status(store, &args);
        if code != 0 {
            continue;
        }
        for line in listing.lines() {
            // `--objects` prints `<sha> <path>` for blobs/trees (bare `<sha>` for commits).
            let Some((sha, path)) = line.split_once(' ') else {
                continue;
            };
            if sha.is_empty() || !path.starts_with(&prefix) {
                continue;
            }
            if seen.insert(sha.to_string()) {
                out.push((sha.to_string(), path.to_string()));
            }
        }
    }
    out
}

/// Findings in the blobs a push will actually publish — the push backstop. Reads each session blob in
/// the range (`git cat-file blob <sha>`) and scans its CONTENT, so a secret that was committed (e.g. via
/// a raw `git commit --no-verify`) and then deleted from the working tree is still caught, and an
/// uncommitted working-tree secret that will never be pushed no longer blocks the push.
///
/// Encryption: on an agit-crypt store the committed blob is ciphertext whose AGITCRYPT magic carries a
/// NUL, so `is_probably_binary` skips it exactly as the staged/tree paths do — a ciphertext blob is never
/// a plaintext finding, which is the documented behaviour (content is protected by encryption, not the
/// scanner). Tree objects that slipped through the prefix filter fail `cat-file blob` and are skipped.
fn range_findings(
    store: &Path,
    sources: &[String],
    remotes: &[String],
    allow: &scan::Allowlist,
) -> Vec<(String, scan::Finding)> {
    let mut v = Vec::new();
    for (sha, path) in range_session_blobs(store, sources, remotes) {
        let (code, content) = scope::git_in_status(store, &["cat-file", "blob", &sha]);
        if code != 0 {
            continue; // a tree object under the sessions/ prefix, or an unreadable blob — skip
        }
        if scan::is_probably_binary(content.as_bytes()) {
            continue; // binary or agit-crypt ciphertext — not a plaintext finding
        }
        for f in scan::scan_text_allow(&content, true, allow) {
            v.push((path.clone(), f));
        }
    }
    v
}

// ─────────────────────── The non-bypassable wrapper secret gate ───────────────────────

/// The visible, auditable override for agit's own secret gate. Set to `1` (or `true`/`yes`) to let a
/// commit/push/snap through THROUGH agit despite suspected secrets.
///
/// This exists on purpose. A gate with no legible exit gets bypassed at a coarser grain — someone
/// drops `--no-verify`, which slips past git's hook leaving no trace in the store, or stops using agit
/// altogether. This override is the opposite: agit DISCLOSES it every time it honors it (which agent,
/// which findings, that the consequences are owned), so the escape is on the record instead of hidden.
pub const ALLOW_ENV: &str = "AGIT_ALLOW_SECRETS";

/// Is the visible override switched on? Only the truthy spellings count, so a stray `AGIT_ALLOW_SECRETS=0`
/// does not silently disarm the gate.
pub fn allow_override_enabled() -> bool {
    std::env::var(ALLOW_ENV)
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        })
        .unwrap_or(false)
}

/// What the gate decided.
pub enum Gate {
    /// Nothing suspected — proceed.
    Clean,
    /// Findings, but the visible override is set — proceed, and it was disclosed on stderr.
    Overridden,
    /// Findings and no override — refuse. Carries the count for the caller's own message.
    Blocked(usize),
}

impl Gate {
    /// May the caller proceed to delegate to git? True for Clean and (disclosed) Overridden.
    pub fn allowed(&self) -> bool {
        !matches!(self, Gate::Blocked(_))
    }
}

/// The gate agit's OWN commit/push/snap wrappers run BEFORE delegating to git — so scanning holds even
/// when git's pre-commit/pre-push hook is skipped with `--no-verify`, which agit's own entry points must
/// not let anyone do silently. `staged` scans the git index (what a commit will write); otherwise the
/// working tree (what a push will publish). `verb` names the action for the disclosure line ("commit",
/// "push", "snap").
///
/// It REUSES scan.rs wholesale (Allowlist + scan_text_allow/scan_file_allow via the collectors above),
/// so the in-file `agit:allow-secret` pragma and the `.agit-allow-secrets` allowlist still suppress
/// known-safe hits exactly as before. The only new exit is the disclosed `AGIT_ALLOW_SECRETS` override.
pub fn secret_gate(store: &Path, staged: bool, verb: &str) -> Result<Gate> {
    let allow = scan::Allowlist::load(store);
    let findings = if staged {
        staged_findings(store, &allow)
    } else {
        tree_findings(store, &allow, &scan_targets(store, &[]))?
    };
    Ok(decide_gate(findings, verb))
}

/// The push backstop: scan what will actually be PUBLISHED — the session blobs in the commit range the
/// target `remotes` do not yet have — instead of the working tree. This catches a secret committed with a
/// raw `git commit --no-verify` and then deleted from the working tree (a working-tree scan sees nothing,
/// but the committed blob still ships), and it does NOT block on an uncommitted working-tree secret that
/// would never be pushed. An empty `remotes` slice scans everything reachable from HEAD.
///
/// Like `secret_gate`, it REUSES scan.rs wholesale (Allowlist + scan_text_allow), so the in-file
/// `agit:allow-secret` pragma and `.agit-allow-secrets` allowlist still suppress known-safe hits, and the
/// disclosed `AGIT_ALLOW_SECRETS` override still applies. The in-process commit/snap gates remain the
/// primary defense; this hardens the push so a committed-then-deleted secret can't slip out.
pub fn secret_gate_range(store: &Path, sources: &[String], remotes: &[String], verb: &str) -> Result<Gate> {
    let allow = scan::Allowlist::load(store);
    Ok(decide_gate(range_findings(store, sources, remotes, &allow), verb))
}

/// Shared tail for every secret gate: disclose the findings, then honor the visible `AGIT_ALLOW_SECRETS`
/// override or refuse. Empty findings → Clean.
fn decide_gate(findings: Vec<(String, scan::Finding)>, verb: &str) -> Gate {
    if findings.is_empty() {
        return Gate::Clean;
    }

    errln!("agit: secret gate — suspected secrets in what you are about to {verb}:");
    for (name, f) in &findings {
        errln!("  {name}:{}  [{}]  {}", f.line, f.rule, f.excerpt);
    }

    if allow_override_enabled() {
        errln!(
            "  {ALLOW_ENV} is set — gate BYPASSED for this {verb}. This override is explicit and auditable \
             (unlike git --no-verify, which leaves no trace). You own the consequences: pushing publishes these to the team."
        );
        Gate::Overridden
    } else {
        errln!(
            "{} suspected. Fix them, mark a false positive with a `{}` pragma or a `{}` entry, or — to override \
             this gate wholesale — re-run with {ALLOW_ENV}=1 (disclosed and auditable, not a silent bypass).",
            findings.len(),
            scan::ALLOW_PRAGMA,
            scan::ALLOW_FILE,
        );
        Gate::Blocked(findings.len())
    }
}

/// scan_root wrap-up: unifies the "found/not found" report and exit code.
fn finish_scan(total: usize, staged: bool, scanned: usize) -> Result<i32> {
    if total > 0 {
        errln!("\n{total} of them. Once the AgentState is pushed, a teammate who pulls carries them along.");
        errln!("Fix it. Or use --no-verify to bypass this hook and explicitly own the consequences.");
        return Ok(1);
    }
    if !staged {
        outln!("Scanned {scanned} files, no secrets found.");
    }
    Ok(0)
}

// ─────────────────────── convert: convert sessions across runtimes ───────────────────────

/// Infer the runtime from the source file's content (session_meta=codex; sessionId/parentUuid=claude).
fn infer_runtime(text: &str) -> Option<&'static str> {
    for line in text.lines().take(20) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
            return Some("codex");
        }
        if v.get("sessionId").is_some() || v.get("parentUuid").is_some() {
            return Some("claude-code");
        }
    }
    None
}

/// `agit convert <src> --to <rt> [--from <rt>] [--cwd P] [--write]`
/// The install id for a materialized session: **always a UUID**, for every runtime.
///
/// This is a regression lock, not a style choice. `codex exec resume` advertises "UUID **or thread
/// name**", so agit briefly installed codex sessions under a proper name (`feature-a-3f9a2c`). That is
/// broken, and it fails OPEN — verified against codex 0.144.4 with a fact only the history could know:
///   * UUID id, file on disk, absent from codex's index → resume RECALLED the fact (codex reads the file)
///   * proper-name id, identical file      → resume answered from thin air, ZERO history, exit 0
/// Root cause: a non-UUID is resolved as a thread name via ~/.codex/state_5.sqlite (`threads` has
/// id/rollout_path/title — no name column), and a file agit drops on disk is never indexed there. So a
/// named install silently starts a FRESH session. A UUID, by contrast, is matched against the rollout
/// files themselves and works.
///
/// Proper names therefore survive only as a human-facing LABEL (see convo::proper_name), never as the id.
fn install_id(_to_rt: &str, _branch: Option<&str>, _seed: &str) -> String {
    crate::convo::fresh_id("session")
}

/// The shared front half of convert / materialize / resume: mint the install id, convert `src` into
/// runtime `to`, and resolve the destination cwd. Returns `(new_id, output, ir, cwd)`. It does NOT
/// install — the callers differ on what comes next (dry-run preview, launch, or marking the id as
/// agit-generated), so installing stays theirs.
fn convert_for_install(
    text: &str,
    src: &Path,
    from: &str,
    to: &str,
    cwd_override: Option<String>,
) -> Result<(String, String, crate::convo::ConversationIR, PathBuf)> {
    use crate::convo::{self, ConvertOpts};
    let new_id = install_id(to, convo::peek_branch(text).as_deref(), text);
    let opts = ConvertOpts { cwd: cwd_override, new_id: new_id.clone() };
    let (out, ir) = convo::convert(src, from, to, &opts)?;
    let cwd = match opts.cwd.clone().or_else(|| ir.cwd.clone()) {
        Some(c) => PathBuf::from(c),
        None => std::env::current_dir()?,
    };
    Ok((new_id, out, ir, cwd))
}

pub fn convert_cmd(
    src: &Path,
    from: Option<String>,
    to: &str,
    cwd_override: Option<String>,
    write: bool,
) -> Result<i32> {
    use crate::convo;

    let text = std::fs::read_to_string(src)
        .map_err(|e| anyhow::anyhow!("failed to read source session {}: {e}", src.display()))?;
    let from = match from {
        Some(f) => f,
        None => infer_runtime(&text)
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("can't recognize the source runtime, pass --from claude-code|codex explicitly"))?,
    };

    let (new_id, out, ir, cwd) = convert_for_install(&text, src, &from, to, cwd_override)?;
    let cross = convo::is_cross_vendor(&from, to);

    // The output = a new copy of sensitive content; scan it at high precision before writing to disk (jsonl: entropy detection off)
    let hits = scan::scan_text_opts(&out, true).len();

    outln!("convert {from} → {to}{}", if cross { " (cross-vendor: content-level, drops encrypted reasoning and narrated tools)" } else { " (same vendor: byte-level replay)" });
    outln!("  source   : {}", src.display());
    outln!("  new id   : {new_id}");
    outln!("  turns/ln : {} events → {} lines", ir.events.len(), out.lines().count());
    outln!("  dest cwd : {}", cwd.display());
    if hits > 0 {
        errln!("  ⚠ scanned {hits} suspected secrets in the output -- a new copy of content the source session saw, so be careful not to leak it.");
    }

    if !write {
        let preview: String = out.lines().take(3).collect::<Vec<_>>().join("\n");
        outln!("\n  preview (first 3 lines):\n{preview}");
        outln!("\n  -- dry-run, nothing written. Add --write to install and print the resume command.");
        return Ok(0);
    }

    let h = crate::register::install(to, &new_id, &cwd, &out)?;
    outln!("\n  written: {}", h.path.display());
    outln!("  resume : {}", h.resume_cmd);
    Ok(0)
}

/// Print the current WorkspaceRevision (Agent↔Environment pairing).
pub fn workspace_show() -> Result<i32> {
    let head = scope::workspace_dir()?.join("HEAD.json");
    if !head.exists() {
        outln!("No WorkspaceRevision yet. One is generated automatically after either repo commits.");
        return Ok(0);
    }
    outln!("{}", std::fs::read_to_string(head)?);
    Ok(0)
}

pub fn workspace_log() -> Result<i32> {
    let log = scope::workspace_dir()?.join("log.jsonl");
    if !log.exists() {
        outln!("No WorkspaceRevision yet.");
        return Ok(0);
    }
    out!("{}", std::fs::read_to_string(log)?);
    Ok(0)
}

/// Read the workspace log into a revision list (newest first).
fn workspace_revisions() -> Result<Vec<serde_json::Value>> {
    let log = scope::workspace_dir()?.join("log.jsonl");
    if !log.exists() {
        return Ok(vec![]);
    }
    let mut revs: Vec<serde_json::Value> = std::fs::read_to_string(log)?
        .lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect();
    revs.reverse(); // newest first
    Ok(revs)
}

/// `agit workspace restore [<N|agent-rev>]` -- roll both repos back together to the joint state recorded by
/// a WorkspaceRevision (the "undo" half of JointVersionControl). With no argument, list the available revisions.
pub fn workspace_restore(selector: Option<&str>) -> Result<i32> {
    let revs = workspace_revisions()?;
    if revs.is_empty() {
        anyhow::bail!("No WorkspaceRevision to restore yet. One is generated automatically after either repo commits.");
    }

    let short = |s: &str| s.chars().take(9).collect::<String>();
    let Some(sel) = selector else {
        // No selector given: list the joint states to choose from (newest first).
        outln!("Restorable joint states (newest first):\n");
        for (i, r) in revs.iter().enumerate() {
            let ts = r.get("ts").and_then(|v| v.as_str()).unwrap_or("?");
            let trig = r.get("trigger").and_then(|v| v.as_str()).unwrap_or("?");
            let ar = r.get("agent_rev").and_then(|v| v.as_str()).unwrap_or("");
            let ec = r.get("env").and_then(|e| e.get("head_commit")).and_then(|v| v.as_str()).unwrap_or("");
            outln!("  {:>2}. {ts}  {trig:14}  agent {} · env {}", i + 1, short(ar), short(ec));
        }
        outln!("\nUse `agit workspace restore <number>` or `restore <agent-rev prefix>` to roll back.");
        return Ok(0);
    };

    // Selector: a pure number = index (1 = newest); otherwise match by agent_rev prefix.
    let chosen = if let Ok(n) = sel.parse::<usize>() {
        revs.get(n.wrapping_sub(1))
    } else {
        revs.iter().find(|r| {
            r.get("agent_rev").and_then(|v| v.as_str()).map(|a| a.starts_with(sel)).unwrap_or(false)
        })
    };
    let Some(rev) = chosen else {
        anyhow::bail!("No WorkspaceRevision matching `{sel}`. Run `agit workspace restore` to see the options.");
    };

    let agent_rev = rev.get("agent_rev").and_then(|v| v.as_str()).unwrap_or("");
    let env_commit = rev
        .get("env")
        .and_then(|e| e.get("head_commit"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if env_commit.is_empty() {
        anyhow::bail!("This revision has no env.head_commit, so it can't be restored.");
    }

    let env = scope::env_root()?;
    outln!("Restoring joint state → env {} · agent {}", short(env_commit), short(agent_rev));
    outln!("(Both repos will checkout to that commit, entering detached HEAD; git will refuse to overwrite uncommitted changes.)\n");

    // Environment first, then Agent Store. Stop on the first failure so the user sees git's real error.
    outln!("Environment:");
    let ec = scope::git_in_inherit(&env, &["checkout", env_commit]);
    if ec != 0 {
        anyhow::bail!("Environment checkout failed (exit code {ec}). Commit or stash your unsaved changes first.");
    }
    if !agent_rev.is_empty() {
        if let Ok(agent) = scope::agent_root() {
            outln!("Agent Store:");
            // Moving HEAD under a concurrent snap is what the store lock exists for: one store is
            // shared by every repo that tracks the agent, and a watcher in another repo is a writer.
            let _lock = crate::session::lock_store(&agent)?;
            let ac = scope::git_in_inherit(&agent, &["checkout", agent_rev]);
            if ac != 0 {
                anyhow::bail!("Agent Store checkout failed (exit code {ac}). Environment was already rolled back, Agent Store untouched.");
            }
        }
    }
    outln!("\nBack at that joint state. To build on it, create a branch with `agit checkout -b <branch>` / `agit -a checkout -b <branch>`.");
    Ok(0)
}

// ─────────────────────── graph: the Workspace-State timeline + relations ───────────────────────

/// `agit graph` -- render the WorkspaceRevision DAG: each joint state, plus the Agent↔Environment /
/// Agent↔Agent edges recorded at that point.
pub fn workspace_graph() -> Result<i32> {
    let mut revs = workspace_revisions()?;
    if revs.is_empty() {
        outln!("No WorkspaceRevisions yet. One is generated automatically after either repo moves a ref.");
        return Ok(0);
    }
    revs.reverse(); // oldest first, so the timeline reads top-to-bottom
    let short = |s: &str| s.chars().take(9).collect::<String>();
    outln!("Workspace timeline ({} revisions, oldest first):\n", revs.len());
    for r in &revs {
        let ts = r.get("ts").and_then(|v| v.as_str()).unwrap_or("?");
        let trig = r.get("trigger").and_then(|v| v.as_str()).unwrap_or("?");
        let ar = r.get("agent_rev").and_then(|v| v.as_str()).unwrap_or("");
        let ec = r.get("env").and_then(|e| e.get("head_commit")).and_then(|v| v.as_str()).unwrap_or("");
        outln!("● {ts}  {trig}");
        outln!("│   agent {}  ·  env {}", if ar.is_empty() { "∅".into() } else { short(ar) }, short(ec));
        if let Some(rels) = r.get("relations").and_then(|v| v.as_array()) {
            for e in rels {
                if let Some(e) = e.as_str() {
                    outln!("│   ⇄ {e}");
                }
            }
        }
        outln!("│");
    }
    Ok(0)
}

// ─────────────────────── resume: load a session into a runtime and continue ───────────────────────

/// Convert a session file to runtime `to`, install it into that runtime's native store so it can be
/// resumed, and return the id it installed under with the resume handle. Shared by `start` and the
/// auto-convert worker; both need the id, because it is the only key capture can attribute by (§6).
pub fn materialize_id(
    src: &Path,
    from: &str,
    to: &str,
    cwd_override: Option<String>,
) -> Result<(String, crate::register::ResumeHandle)> {
    let text = std::fs::read_to_string(src)?;
    let (new_id, out, _ir, cwd) = convert_for_install(&text, src, from, to, cwd_override)?;
    let h = crate::register::install(to, &new_id, &cwd, &out)?;
    // Record that agit produced this id, so the watcher never re-converts its own output (which would
    // otherwise feed back: A→B, then snap B, then B→A, forever).
    mark_generated(&new_id);
    Ok((new_id, h))
}

/// The id-registry of sessions agit itself materialized (one id per line).
///
/// Machine-local, like the launch record and for the same reason: `materialize` mints an id long
/// before capture decides which agent's store the session belongs to, and one store is shared by many
/// environments. Keyed inside a single store, the guard below would miss every id routed elsewhere —
/// and a missed guard is not a cosmetic loss, it is the A→B→A conversion loop running forever.
fn generated_file(home: &Path) -> PathBuf {
    home.join("generated")
}

fn mark_generated(id: &str) {
    let Ok(home) = scope::agit_home() else { return };
    use std::io::Write;
    let _ = std::fs::create_dir_all(&home);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(generated_file(&home))
        .and_then(|mut f| writeln!(f, "{id}"));
}

fn load_generated() -> std::collections::HashSet<String> {
    scope::agit_home()
        .ok()
        .and_then(|h| std::fs::read_to_string(generated_file(&h)).ok())
        .map(|s| s.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default()
}

/// `agit convert --watch [--interval N]` — the auto-convert background worker. Watches the Agent Store's
/// per-runtime session trees and, for each session not yet mirrored into the OTHER runtime, converts and
/// installs it under its proper name — so any captured session is immediately resumable in either CLI
/// (`codex exec resume <name>` / `claude --resume <id>`). Conversion is a pure deterministic transform
/// (no model calls). Ctrl-C to stop.
/// One cross-runtime conversion pass over the Agent Store: for each session not already converted
/// (tracked by source-content hash in `seen`), materialize it into the OTHER runtime under its proper
/// name. Shared by `convert --watch` and the unified `agit watch` loop.
pub fn convert_pass(agent: &Path, env: &Path, seen: &mut std::collections::HashSet<String>) {
    let generated = load_generated();
    // Whose sessions these are. The converted copy is agit's own output on behalf of THIS agent, so it
    // gets a launch record of its own: without one, capture reads it as hand-started and files it under
    // the repo's DEFAULT agent — which lands one agent's transcript in another agent's store, and
    // pushes it to that team. A legacy store has no identity to record, and needs none: it is the only
    // store its sessions can go to.
    let owner = agent::read_identity(agent).ok();
    // `store_sessions`, not a glob of `sessions/<rt>/`: it reads BOTH store layouts, so an
    // env-partitioned store does not silently stop auto-converting, and it already excludes a
    // session's sidecars (`<id>/subagents/*.jsonl`), which the old `max_depth(1)` was there for.
    let sessions = store_sessions(agent);
    // Every ordered pair of distinct runtimes, from the registry — so a third runtime is converted to
    // and from the others with no change here.
    let runtimes = crate::session::runtimes();
    let mut pairs = Vec::new();
    for &from in &runtimes {
        for &to in &runtimes {
            if from != to {
                pairs.push((from, to));
            }
        }
    }
    for (from, to) in pairs {
        for e in sessions.iter().filter(|s| s.runtime == from) {
            // never re-convert a session agit itself produced — that's the feedback-loop guard.
            let stem = e.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if generated.contains(&stem) {
                continue;
            }
            let content = match std::fs::read_to_string(&e.path) {
                Ok(c) if !c.trim().is_empty() => c,
                _ => continue,
            };
            // key on source content — an unchanged session converts once; once marked seen (even on
            // error) it won't spin re-attempting the same input.
            let key = format!("{from}->{to}:{}", &crate::convo::sha256_hex(&content)[..16]);
            if !seen.insert(key) {
                continue;
            }
            match materialize_id(&e.path, from, to, None) {
                Ok((new_id, h)) => {
                    if let Some(o) = &owner {
                        // Unsigned here: the converted copy's captured content differs from this source,
                        // so its authoritative signature is written at capture, into the sidecar.
                        if let Err(err) = record_launch(&new_id, &o.aid, &o.name, env, to, None) {
                            errln!("  ⚠ {from}→{to} launch record not written ({err:#}) — capture will attribute this copy by repo default.");
                        }
                    }
                    outln!("  ● {from}→{to}  {}", h.resume_cmd);
                }
                Err(err) => errln!("  ⚠ {from}→{to} {}: {err:#}", e.path.display()),
            }
        }
    }
}

pub fn convert_watch(interval_secs: u64) -> Result<i32> {
    use std::time::Duration;
    // Converting is not attribution — it makes THIS agent's sessions resumable in either CLI — so the
    // resolved agent is the right answer, and capture no longer writes the store `-a` resolves to.
    let agent = crate::session::convert_target()?;
    let env = scope::env_root()?;
    let interval = Duration::from_secs(interval_secs.max(1));
    let mut seen = std::collections::HashSet::new();
    outln!(
        "Auto-converting sessions both ways (claude-code ↔ codex) every {}s. Ctrl-C to stop.",
        interval.as_secs()
    );
    loop {
        convert_pass(&agent, &env, &mut seen);
        std::thread::sleep(interval);
    }
}

/// `agit resume <src-session> [--as <rt>] [--cwd <path>] [--exec]` -- the universal loader: install a
/// session so a runtime can resume it (converting across runtimes when `--as` differs from the source),
/// then print or (with --exec) launch the resume command. A thin, first-class wrapper over convert/register.
pub fn resume_cmd(
    src: &Path,
    as_rt: Option<String>,
    cwd_override: Option<String>,
    env_override: Option<String>,
    exec: bool,
    relocate: bool,
) -> Result<i32> {
    use crate::convo;

    let text = std::fs::read_to_string(src)
        .map_err(|e| anyhow::anyhow!("failed to read source session {}: {e}", src.display()))?;
    let from = infer_runtime(&text)
        .ok_or_else(|| anyhow::anyhow!("could not detect the source runtime; the file doesn't look like a claude-code or codex session"))?;
    // Default target = the source runtime (a plain resume, no conversion); --as forces a different one.
    let to = as_rt.unwrap_or_else(|| from.to_string());

    // --env rebinds the Agent State to a DIFFERENT Environment than the one it ran in: the session is
    // re-pointed at that checkout, so the same agent context can continue against another repo/clone.
    let (cwd_override, rebound) = match env_override {
        Some(e) => {
            let p = std::fs::canonicalize(&e)
                .map_err(|_| anyhow::anyhow!("no such environment path: {e}"))?;
            let (rc, _) = crate::scope::git_in_status(&p, &["rev-parse", "--is-inside-work-tree"]);
            if rc != 0 {
                anyhow::bail!("{} is not a git repository — an Environment is a code repo", p.display());
            }
            (Some(p.to_string_lossy().into_owned()), Some(p))
        }
        None => (cwd_override, None),
    };

    let (new_id, out, ir, cwd) = convert_for_install(&text, src, from, &to, cwd_override)?;

    // --relocate: rewrite the ORIGINAL environment's path prefix everywhere in the transcript (bash
    // commands, file_paths, narrated tools), not just the session's own cwd. Only correct when the
    // SAME project moved to a new path — pointing an agent at a different repo must keep its paths,
    // since those are its real memory of the other codebase.
    let out = match (relocate, ir.cwd.as_deref()) {
        (true, Some(old)) => convo::swap_path(&out, old, &cwd.to_string_lossy()),
        _ => out,
    };

    let hits = scan::scan_text_opts(&out, true).len();
    if hits > 0 {
        errln!("  ⚠ {hits} suspected secret(s) in the materialized session -- a fresh copy of what the source saw.");
    }
    let h = crate::register::install(&to, &new_id, &cwd, &out)?;
    outln!("Installed → {}", h.path.display());
    if let Some(envp) = &rebound {
        let origin = ir.cwd.as_deref().unwrap_or("(unknown)");
        outln!("Environment: {}  (rebound from {origin})", envp.display());
        if relocate {
            outln!("  relocated: rewrote {origin} → {} throughout the transcript", envp.display());
        } else {
            errln!(
                "  note: the session's own cwd is rebound; paths it recorded under {origin} are kept as-is\n         (they're its memory of that codebase). Pass --relocate if this is the SAME project moved."
            );
        }
    }
    outln!("Resume: {}", h.resume_cmd);

    if exec {
        outln!("\nLaunching…\n");
        let status = std::process::Command::new("sh").arg("-c").arg(&h.resume_cmd).status()?;
        return Ok(status.code().unwrap_or(0));
    }
    Ok(0)
}


// ─────────────────────── The launch record (§6): whose session is this? ───────────────────────

/// One launch: `session-id → {aid, env, runtime, started}`.
///
/// Why this exists at all: the runtime dumps per PROJECT (`~/.claude/projects/<cwd-slug>/`), not per
/// agent, so two agents working in one repo write to the SAME folder. The active pointer cannot tell
/// their sessions apart — attributing capture by it misfiles silently, into the wrong agent, and pushes
/// a transcript to the wrong team. `agit start` launched the session, so it alone knows whose it is.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Launch {
    pub session: String,
    /// Identity, not the label: a rename must not orphan the record.
    pub aid: String,
    pub name: String,
    pub env: String,
    pub runtime: String,
    pub started: String,
    /// The machine's signature over this launch's context (§ provenance). Optional and skipped when
    /// absent, so records written before signing existed still parse, and a launch with no key/content
    /// to sign is simply unsigned — never a failed launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

/// How a session was attributed to an agent. Capture must be able to SAY which — a guess reported as a
/// fact is the failure mode this whole record exists to remove.
pub enum Attribution {
    /// `agit start` wrote a record: authoritative. Boxed — a `Launch` now carries an optional signature,
    /// which would otherwise make this variant dwarf `RepoDefault`.
    Launched(Box<Launch>),
    /// No record (a plain `claude`/`codex` session) → the repo's default agent, reported as a fallback.
    RepoDefault { aid: String, name: String },
}

impl Attribution {
    pub fn aid(&self) -> &str {
        match self {
            Attribution::Launched(l) => &l.aid,
            Attribution::RepoDefault { aid, .. } => aid,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Attribution::Launched(l) => &l.name,
            Attribution::RepoDefault { name, .. } => name,
        }
    }
    /// The line capture prints. `None` when the record is authoritative — there is nothing to disclose.
    pub fn note(&self) -> Option<String> {
        match self {
            Attribution::Launched(_) => None,
            Attribution::RepoDefault { name, .. } => Some(format!(
                "not started by agit, so it has no launch record — filing it under this repo's default agent `{name}`. \
                 Start it with `agit start --agent <name>` to attribute it exactly."
            )),
        }
    }
}

/// Machine-local, spans repos: a session id is a UUID, so one file can never collide, and capture in any
/// environment can read it. Append-only; the last record for an id wins.
fn launches_file(home: &Path) -> PathBuf {
    home.join("launches.jsonl")
}

fn record_launch_at(home: &Path, l: &Launch) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(home)?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(launches_file(home))?;
    writeln!(f, "{}", serde_json::to_string(l)?)?;
    Ok(())
}

fn lookup_launch_at(home: &Path, session: &str) -> Option<Launch> {
    let text = std::fs::read_to_string(launches_file(home)).ok()?;
    text.lines()
        .filter_map(|l| serde_json::from_str::<Launch>(l.trim()).ok()).rfind(|l| l.session == session)
}

/// Write the launch record. Called by `start` BEFORE the runtime is exec'd: a session captured before
/// its record exists is a session attributed by guesswork.
///
/// `sign_over` is `Some((content, email))` when the launch carries content worth binding to the machine
/// key — the session being resumed. The record is then stamped with a signature over
/// `(sha256(content) ‖ aid ‖ email ‖ started)`. Signing is best-effort: if this machine has no usable
/// key the record is written unsigned rather than not at all, because a launch record — even unsigned —
/// beats none for attribution. The authoritative, content-bound provenance is written at capture, into
/// the session's committed sidecar.
pub fn record_launch(
    session: &str,
    aid: &str,
    name: &str,
    env: &Path,
    runtime: &str,
    sign_over: Option<(&str, &str)>,
) -> Result<()> {
    let started = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let provenance = sign_over.and_then(|(content, email)| {
        agent::machine_signing_key()
            .ok()
            .map(|key| sign_provenance(&key, content, aid, email, &started))
    });
    record_launch_at(
        &scope::agit_home()?,
        &Launch {
            session: session.to_string(),
            aid: aid.to_string(),
            name: name.to_string(),
            env: env.display().to_string(),
            runtime: runtime.to_string(),
            started,
            provenance,
        },
    )
}

pub fn lookup_launch(session: &str) -> Option<Launch> {
    lookup_launch_at(&scope::agit_home().ok()?, session)
}

/// Attribute a captured session to an agent: the launch record if there is one, else the repo's default
/// — never the active pointer, which cannot tell two agents in one dump folder apart.
pub fn attribute_session(session: &str) -> Result<Attribution> {
    if let Some(l) = lookup_launch(session) {
        return Ok(Attribution::Launched(Box::new(l)));
    }
    let a = agent::resolve(None)?;
    Ok(Attribution::RepoDefault { aid: a.aid, name: a.name })
}

// ─────────────────────── Provenance: a session, cryptographically tied to its producer ───────────────────────

/// A signed statement that a specific machine captured a specific session for a specific agent.
///
/// It travels two ways: inside a session's committed sidecar (so it survives a clone and reaches a
/// teammate) and inside the machine-local launch record. The signed message is the tuple
/// `(content_digest ‖ aid ‖ email ‖ started)` — the session content is already content-addressed, so a
/// single edit to the transcript changes `content_digest` and breaks the signature. `content_digest` is
/// recorded too, but verification RECOMPUTES it from the transcript on disk: the stored copy is only for
/// a legible "the content changed" message, never the thing trusted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Provenance {
    pub aid: String,
    /// The git committer email in force when the session was signed. Read, never set: agit must not
    /// touch the developer's git identity, so whatever the store already commits under is what is bound.
    pub email: String,
    pub started: String,
    /// sha256 of the transcript at signing time. Recomputed at verify — a mismatch is a tampered session.
    pub content_digest: String,
    /// The machine's ed25519 public key, hex. Self-verify checks the signature against THIS key; who the
    /// key belongs to (pubkey→person) is the hub's trust problem, out of scope here.
    pub pubkey: String,
    pub sig: String,
}

/// The exact bytes signed. A version tag domain-separates it from any other agit signature, and newlines
/// join the fields: a session content digest is hex and an aid/email/timestamp none contain a newline, so
/// no two distinct tuples can serialize to the same bytes.
fn provenance_message(content_digest: &str, aid: &str, email: &str, started: &str) -> Vec<u8> {
    format!("agit-provenance-v1\n{content_digest}\n{aid}\n{email}\n{started}").into_bytes()
}

/// Sign a session's provenance with the machine key. Pure: the same content and tuple always yield the
/// same record (ed25519 is deterministic), so re-signing an unchanged session rewrites nothing.
pub fn sign_provenance(
    key: &ed25519_dalek::SigningKey,
    content: &str,
    aid: &str,
    email: &str,
    started: &str,
) -> Provenance {
    let content_digest = crate::convo::sha256_hex(content);
    let msg = provenance_message(&content_digest, aid, email, started);
    Provenance {
        aid: aid.to_string(),
        email: email.to_string(),
        started: started.to_string(),
        pubkey: hex::encode(key.verifying_key().to_bytes()),
        sig: agent::sign_hex(key, &msg),
        content_digest,
    }
}

/// The outcome of self-verifying a session's provenance. Only `Verified` is a cryptographic pass;
/// everything else is a reported reason, never a panic and never a block — an unsigned or unreadable
/// session degrades to `Unsigned`, mirroring the "attribution fallback is never silent" contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenanceStatus {
    Verified { aid: String, email: String, pubkey: String },
    /// No signature to check (a session captured before signing, or with no key available at capture).
    Unsigned,
    /// The transcript's current digest differs from the one signed: the content changed after signing.
    ContentTampered { recorded: String, actual: String },
    /// The signature does not verify against its own recorded public key.
    BadSignature,
}

impl ProvenanceStatus {
    /// The one-line verdict, verified or not. Never the word "trusted": self-verify proves the content is
    /// intact and the signature matches the recorded key, not who that key belongs to.
    pub fn summary(&self) -> String {
        match self {
            ProvenanceStatus::Verified { aid, pubkey, .. } => {
                format!("verified · signed for {aid} by key {}", short_key(pubkey))
            }
            ProvenanceStatus::Unsigned => "unverified · no signature recorded".to_string(),
            ProvenanceStatus::ContentTampered { .. } => {
                "UNVERIFIED · content changed since it was signed (tampered)".to_string()
            }
            ProvenanceStatus::BadSignature => {
                "UNVERIFIED · signature does not match its recorded public key".to_string()
            }
        }
    }
    pub fn is_verified(&self) -> bool {
        matches!(self, ProvenanceStatus::Verified { .. })
    }
}

fn short_key(pubkey: &str) -> String {
    if pubkey.len() > 16 {
        format!("{}…", &pubkey[..16])
    } else {
        pubkey.to_string()
    }
}

/// Self-verify a session's provenance: recompute the transcript digest, rebuild the signed message, and
/// check the signature against the pubkey the record itself carries. Degrades gracefully — a `None`
/// record is `Unsigned`, not an error.
pub fn verify_provenance(content: &str, p: Option<&Provenance>) -> ProvenanceStatus {
    let Some(p) = p else { return ProvenanceStatus::Unsigned };
    let actual = crate::convo::sha256_hex(content);
    if actual != p.content_digest {
        return ProvenanceStatus::ContentTampered { recorded: p.content_digest.clone(), actual };
    }
    let msg = provenance_message(&p.content_digest, &p.aid, &p.email, &p.started);
    if agent::verify_hex(&p.pubkey, &msg, &p.sig) {
        ProvenanceStatus::Verified {
            aid: p.aid.clone(),
            email: p.email.clone(),
            pubkey: p.pubkey.clone(),
        }
    } else {
        ProvenanceStatus::BadSignature
    }
}

/// The committer email the store commits under, read never written (agit must not touch git identity).
/// Falls back to the store default when git has nothing configured, so signing always has a stable field.
pub fn committer_email(store: &Path) -> String {
    scope::git_in(store, &["config", "user.email"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "agit@local".to_string())
}

/// Read the `provenance` block from a session's sidecar, if it has one. Absent sidecar, unparsable JSON,
/// or a sidecar with no provenance all yield `None` — the caller reports "unsigned", never fails.
pub fn sidecar_provenance(transcript: &Path) -> Option<Provenance> {
    let text = std::fs::read_to_string(sidecar_path(transcript)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    serde_json::from_value(v.get("provenance")?.clone()).ok()
}

/// `agit provenance [verify <session> | key]` — self-verify a captured session's signature, or show this
/// machine's public key. Verification never blocks: an unsigned or tampered session reports and returns 0
/// for `key`/no-arg, and a non-zero code only when an explicit `verify` finds a session NOT verified.
pub fn provenance_cmd(args: &[String]) -> Result<i32> {
    match args.first().map(|s| s.as_str()) {
        Some("verify") => provenance_verify(args.get(1).map(|s| s.as_str())),
        Some("key") | Some("show") | None => provenance_key(),
        Some(other) => {
            errln!("agit provenance: unknown subcommand `{other}`");
            errln!("  usage: agit provenance verify <session>   ·   agit provenance key");
            Ok(2)
        }
    }
}

/// Print this machine's signing identity (minting it if absent). The public key is safe to show and share.
fn provenance_key() -> Result<i32> {
    let pk = agent::machine_pubkey_hex()?;
    outln!("machine signing key (ed25519)");
    outln!("  pubkey {pk}");
    outln!("  stored {}", scope::agit_home()?.join("identity").join("ed25519").display());
    outln!("  (private key is 0600; sessions you capture are signed with it)");
    Ok(0)
}

/// Resolve `<session>` to a transcript on disk — a direct path, a sidecar path, or a session id in the
/// resolved agent's store — then self-verify its provenance.
fn provenance_verify(session: Option<&str>) -> Result<i32> {
    let Some(sel) = session.map(str::trim).filter(|s| !s.is_empty()) else {
        anyhow::bail!("agit provenance verify: name a session\n  usage: agit provenance verify <session-path|id>");
    };
    let transcript = resolve_session_transcript(sel)?;
    let content = std::fs::read_to_string(&transcript)
        .with_context(|| format!("cannot read session {}", transcript.display()))?;
    let status = verify_provenance(&content, sidecar_provenance(&transcript).as_ref());

    outln!("session {}", transcript.display());
    outln!("  {}", status.summary());
    if let ProvenanceStatus::Verified { email, .. } = &status {
        outln!("  committer {email}");
    }
    if let ProvenanceStatus::ContentTampered { recorded, actual } = &status {
        outln!("  signed digest  {recorded}");
        outln!("  current digest {actual}");
    }
    // Never block: an unsigned session is a soft "unverified" (exit 0, like the attribution fallback); a
    // signature that is present but does NOT check out is a hard failure worth a non-zero code.
    Ok(match status {
        ProvenanceStatus::Verified { .. } | ProvenanceStatus::Unsigned => 0,
        ProvenanceStatus::ContentTampered { .. } | ProvenanceStatus::BadSignature => 1,
    })
}

/// A session selector is a filesystem path (to the transcript or its sidecar) when one exists, else a
/// bare session id looked up in the resolved agent's store.
fn resolve_session_transcript(sel: &str) -> Result<PathBuf> {
    let p = Path::new(sel);
    if p.is_file() {
        // A sidecar was passed: point back at its transcript (`<id>.agit.json` → `<id>.jsonl`).
        if p.extension().map(|e| e == "json").unwrap_or(false)
            && p.file_stem().map(|s| Path::new(s).extension().map(|e| e == "agit").unwrap_or(false)).unwrap_or(false)
        {
            let jsonl = p.with_file_name(format!(
                "{}.jsonl",
                p.file_stem().and_then(|s| Path::new(s).file_stem()).unwrap_or_default().to_string_lossy()
            ));
            if jsonl.is_file() {
                return Ok(jsonl);
            }
        }
        return Ok(p.to_path_buf());
    }
    // Not a path: treat it as a session id in the resolved agent's store.
    let store = agent::resolve(None)?.store;
    store_sessions(&store)
        .into_iter()
        .find(|s| s.path.file_stem().map(|n| n == sel).unwrap_or(false))
        .map(|s| s.path)
        .with_context(|| format!("no session `{sel}` here — pass a transcript path, or a session id in this agent's store"))
}

// ─────────────────────── start: the agent's latest session, here ───────────────────────

/// A session in the store, wherever it was recorded.
pub struct StoredSession {
    pub path: PathBuf,
    pub runtime: &'static str,
    /// The environment it was recorded in (`sessions/<env-slug>/<rt>/`), when the store is partitioned.
    pub env_slug: Option<String>,
    /// When it last did anything, as the STORE records it: the `<id>.agit.json` sidecar, else the
    /// transcript's own last timestamp. `None` only for a session that records no time at all.
    pub last_activity: Option<chrono::DateTime<chrono::Utc>>,
    pub mtime: std::time::SystemTime,
}

impl StoredSession {
    /// What to show a user as "when". mtime is the fallback, not the source: see `latest_session`.
    pub fn recency(&self) -> std::time::SystemTime {
        self.last_activity.map(std::time::SystemTime::from).unwrap_or(self.mtime)
    }
}

/// `<id>.jsonl` → `<id>.agit.json` — the sidecar snap writes beside every captured session (§6).
pub fn sidecar_path(transcript: &Path) -> PathBuf {
    transcript.with_extension("agit.json")
}

/// When a stored session last did anything, read from content **git preserves**: the sidecar first
/// (one line, always there for anything snap captured), else the transcript's own last timestamp,
/// which costs a scan but keeps a store written before sidecars existed orderable.
fn recorded_activity(transcript: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    let ts = sidecar_last_activity(transcript).or_else(|| crate::session::last_activity(transcript))?;
    chrono::DateTime::parse_from_rfc3339(ts.trim())
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

fn sidecar_last_activity(transcript: &Path) -> Option<String> {
    let text = std::fs::read_to_string(sidecar_path(transcript)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("last_activity")?.as_str().map(str::to_string)
}

/// The agent's latest session from ANY environment.
///
/// Read from the store's files, **never from git-log topology**. Verified: `git log -1 --name-only`
/// prints no file names at all on a merge commit (it prints the header and stops), so a log-derived
/// leaf-finder returns nothing exactly after a merge or a pull — the moment `start` matters most.
///
/// A candidate's parent directory must BE the runtime dir, which holds for both the flat
/// `sessions/<rt>/` layout and the partitioned `sessions/<env>/<rt>/` one, and excludes a session's
/// sidecars (`<id>/subagents/*.jsonl`), which are not sessions.
pub fn store_sessions(store: &Path) -> Vec<StoredSession> {
    let mut out = Vec::new();
    for e in WalkDir::new(store.join(crate::session::SESSIONS_SUBDIR))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let p = e.path();
        if p.extension().map(|x| x != "jsonl").unwrap_or(true) {
            continue;
        }
        let Some(parent) = p.parent() else { continue };
        let Some(rt) = crate::session::runtimes()
            .into_iter()
            .find(|rt| parent.file_name().map(|n| n == *rt).unwrap_or(false))
        else {
            continue;
        };
        let Some(mtime) = e.metadata().ok().and_then(|m| m.modified().ok()) else { continue };
        let env_slug = parent
            .parent()
            .and_then(|g| g.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| n != crate::session::SESSIONS_SUBDIR);
        out.push(StoredSession {
            last_activity: recorded_activity(p),
            path: p.to_path_buf(),
            runtime: rt,
            env_slug,
            mtime,
        });
    }
    out
}

/// The most recent session, ordered by what the store RECORDS — never by the filesystem.
///
/// git does not preserve mtimes: after the `git clone` that `agit a clone` performs, every session
/// carries the checkout time, identical to the nanosecond, so an mtime order collapses into whatever
/// order the directory walk happened to produce. A session the store records a time for therefore
/// always beats one it does not, and mtime only breaks a tie.
pub fn latest_session(store: &Path) -> Option<StoredSession> {
    store_sessions(store).into_iter().max_by_key(|s| (s.last_activity, s.mtime))
}

// ─────────────────────── ahead/behind — one implementation, shared by status/pull/fetch ───────────────────────

/// How the store's HEAD stands against its tracked upstream: `(ahead, behind)` — the commits HEAD has
/// that the upstream lacks, and the commits the upstream has that HEAD lacks. `None` when there is no
/// resolvable upstream to compare against (a store with no tracking branch, e.g. never pushed).
///
/// The ONE ahead/behind primitive: `agit a status` reads it for its summary, `diverged` derives the
/// pull/merge decision from it, and both go through `scope::git_in_status` so a missing upstream is a
/// clean `None`, never a hard error.
pub fn ahead_behind(store: &Path) -> Option<(u64, u64)> {
    let (code, out) =
        scope::git_in_status(store, &["rev-list", "--left-right", "--count", "@{u}...HEAD"]);
    if code != 0 {
        return None;
    }
    // `--left-right --count @{u}...HEAD` prints "<behind>\t<ahead>": the left side is the upstream.
    let mut counts = out.split_whitespace().filter_map(|s| s.parse::<u64>().ok());
    let behind = counts.next()?;
    let ahead = counts.next()?;
    Some((ahead, behind))
}

/// True when HEAD and its upstream each hold commits the other does not — the one case a fast-forward
/// cannot cover and a textual merge must not. An absent or unresolvable upstream is not "diverged".
///
/// Promoted out of `main.rs` so `agit a pull`, `agit a fetch` and `agit a status` share the single
/// `ahead_behind` above rather than each reimplementing the rev-list.
pub fn diverged(store: &Path) -> bool {
    matches!(ahead_behind(store), Some((ahead, behind)) if ahead > 0 && behind > 0)
}

/// Best-effort summary of the sessions on the tracked upstream that HEAD does not yet have. Silent if
/// there is no upstream to compare against (a fetch with an explicit refspec and no tracking branch).
///
/// Promoted out of `main.rs` alongside `diverged` so `agit a fetch` and any other session-aware reader
/// narrate incoming work the same way.
pub fn report_incoming(store: &Path) {
    let (code, out) = scope::git_in_status(
        store,
        &["diff", "--name-only", "--diff-filter=A", "HEAD..@{u}", "--", "sessions"],
    );
    if code != 0 {
        return;
    }
    let new: Vec<&str> = out.lines().filter(|l| l.ends_with(".jsonl")).collect();
    if new.is_empty() {
        outln!("  up to date — no new sessions on the remote.");
        return;
    }
    // Break the count down per runtime by asking the registry which runtimes exist, not by naming any
    // — a new adapter shows up here for free.
    let breakdown: Vec<String> = crate::adapter::names()
        .iter()
        .filter_map(|rt| {
            let n = new.iter().filter(|f| f.contains(&format!("/{rt}/"))).count();
            (n > 0).then(|| format!("{rt}: {n}"))
        })
        .collect();
    let suffix = if breakdown.is_empty() { String::new() } else { format!(" ({})", breakdown.join(", ")) };
    outln!("  {} new session(s) on the remote{suffix}.", new.len());
    outln!("  integrate with: agit a pull");
}

// ─────────────────────── status: this repo's agents at a glance ───────────────────────

/// A one-line summary of the active store's position against its upstream, for `agit a status`.
fn upstream_line(store: &Path) -> String {
    match ahead_behind(store) {
        None => "no upstream yet — agit a push to publish".to_string(),
        Some((0, 0)) => "up to date with the remote".to_string(),
        Some((ahead, 0)) => format!("{ahead} unpushed — agit a push to publish"),
        Some((0, behind)) => format!("{behind} behind — agit a pull to integrate"),
        Some((ahead, behind)) => {
            format!("{ahead} ahead, {behind} behind — diverged; agit a merge to reconcile")
        }
    }
}

/// `agit a status` — a per-repo overview: which agents this repo works with (from the committed
/// binding), which one is active here, each one's session count and last activity, whether a watcher is
/// live for it, and where the active store stands against its remote.
///
/// Read-only and resilient: a bound agent this machine has not cloned is shown as such rather than
/// erroring, and the integrity check that `resolve` runs is allowed to fail softly (the overview still
/// lists the rest).
pub fn agent_status() -> Result<i32> {
    let env = scope::env_root()?;
    let binding = agent::Binding::load(&env)?;
    let local = agent::list().unwrap_or_default();
    // The active agent as commands actually resolve it (pointer → $AGIT_AGENT → default). `ok()` so a
    // recreated-remote integrity error never blanks the whole overview.
    let resolved = agent::resolve(None).ok();
    let active_aid = resolved.as_ref().map(|a| a.aid.clone());

    outln!("{}", ui::dim(&format!("repo {}", ui::tilde(&env))));

    // The agents this repo works with = the committed binding; fall back to the resolved active agent
    // when there is no binding yet (a repo that only ran `agit a init` before this landed).
    let bound: Vec<(String, String)> = match &binding {
        Some(b) if !b.agents.is_empty() => {
            b.agents.iter().map(|e| (e.name.clone(), e.id.clone())).collect()
        }
        _ => match &resolved {
            Some(a) => vec![(a.name.clone(), a.aid.clone())],
            None => {
                outln!("no agents bound to this repo — agit a init <name> mints one.");
                return Ok(0);
            }
        },
    };

    let rows: Vec<Vec<String>> = bound
        .iter()
        .map(|(name, aid)| {
            let store = local.iter().find(|a| &a.aid == aid).map(|a| a.store.clone());
            let (status, sessions, last) = match &store {
                Some(store) => {
                    let sessions = store_sessions(store);
                    let status = match crate::session::watching_pid(aid) {
                        Some(_) => ui::accent("● watching"),
                        None => ui::dim("·").to_string(),
                    };
                    let last = sessions
                        .iter()
                        .map(|s| s.recency())
                        .max()
                        .map(ui::ago)
                        .unwrap_or_else(|| "—".into());
                    (status, sessions.len().to_string(), last)
                }
                None => (ui::dim("not cloned").to_string(), "—".into(), "—".into()),
            };
            let here = if Some(aid) == active_aid.as_ref() { "  (active)" } else { "" };
            vec![name.clone(), status, sessions, format!("{last}{here}")]
        })
        .collect();
    outln!("{}", ui::table(&["AGENT", "STATUS", "SESSIONS", "LAST"], &rows));

    if let Some(d) = binding.as_ref().and_then(|b| b.default.clone()) {
        outln!("{}", ui::dim(&format!("default: {d}")));
    }

    // Where the active store stands against its remote — the unpushed/ahead-behind the overview exists
    // to surface. Only for a cloned active agent (a store to ask git about).
    if let Some(a) = resolved.as_ref().filter(|a| a.store.join(".git").exists()) {
        outln!("\n{} {}", ui::bold(&a.name), ui::dim(&upstream_line(&a.store)));
    }
    Ok(0)
}

/// Where a stored session came from, and what it was about — the two things the `start` header carries
/// beyond the agent's own name (§11c).
///
/// The origin is the cwd the session RECORDED, not `env_slug`: the slug is the store's partition name
/// (`web`), while the header has room for the path the agent actually worked in (`~/code/web`), and a
/// store written in the flat layout has no slug at all. The slug remains the fallback.
///
/// One parse yields both, and it is the same parse the rest of agit reads sessions with — a header that
/// disagreed with `agit -a info` about what a session is would be worse than no header.
fn origin_and_gist(s: &StoredSession) -> (Option<String>, Option<String>) {
    let Ok(content) = std::fs::read_to_string(&s.path) else { return (s.env_slug.clone(), None) };
    let ir = match s.runtime {
        "codex" => crate::adapter::codex::parse_rollout(&content, "x"),
        _ => crate::adapter::claude_code::parse_jsonl(&content, "x"),
    };
    let origin = ir
        .cwd
        .map(|c| ui::tilde(Path::new(&c)))
        .or_else(|| s.env_slug.clone());
    let gist = ir
        .prompts
        .into_iter()
        .map(|p| ui::one_line(&p, 72))
        .find(|p| !p.is_empty());
    (origin, gist)
}

/// `agit start [--agent X] [--as claude-code|codex]` — launch a real session in THIS repo already
/// carrying the agent's context, with no file paths and no ids typed (§5.1).
pub fn start_cmd(agent_sel: Option<&str>, as_rt: Option<&str>) -> Result<i32> {
    let env = scope::env_root()?;
    let ag = agent::resolve(agent_sel)?;
    match latest_session(&ag.store) {
        Some(s) => start_carrying(&ag, &env, s, as_rt),
        None => start_fresh(&ag, &env, as_rt),
    }
}

fn start_carrying(ag: &agent::Agent, env: &Path, s: StoredSession, as_rt: Option<&str>) -> Result<i32> {
    // Explicit --as wins; otherwise the session's OWN runtime — a session knows what produced it. No
    // default: the runtimes are peers (§5.3).
    let rt = crate::session::resolve_runtime(as_rt, &[s.runtime], "start")?;

    let here = ui::tilde(env);
    outln!("┌ {} · {} · {rt}", ui::bold(&ag.name), ui::accent(&here));
    // The origin is the point of §5.1's cross-environment carry: a frontend agent continuing in the
    // backend repo carries a session recorded somewhere else, and the header is where you find that out.
    // Suppressed when it IS here — the line above already said where "here" is.
    let (origin, gist) = origin_and_gist(&s);
    let from = match origin.filter(|o| *o != here) {
        Some(o) => format!(" (from {o}, {})", ui::ago(s.recency())),
        None => format!(" ({})", ui::ago(s.recency())),
    };
    outln!("└ carrying its latest session{}", ui::dim(&from));
    if let Some(g) = gist {
        outln!("    {}", ui::dim(&format!("\"{g}\"")));
    }

    // Rebind cwd to this repo but KEEP the paths it recorded elsewhere: those are its real memory of
    // that other codebase, not stale strings. That is `resume --env` without `--relocate` (which is only
    // correct when the SAME project moved).
    let (id, h) = materialize_id(&s.path, s.runtime, &rt, Some(env.display().to_string()))?;

    // Bind the launch to this machine's key by signing over the context it carries — the session being
    // resumed. Best-effort: reading it just to sign must never fail a start, so a read error simply
    // records the launch unsigned (capture will sign the session's own content into its sidecar).
    let sign_over = std::fs::read_to_string(&s.path).ok();
    let email = committer_email(&ag.store);
    let sign_over = sign_over.as_deref().map(|c| (c, email.as_str()));

    // The record must exist before the runtime does. Its absence is not fatal — a session that captures
    // to the default agent beats no session — but it is never silent.
    if let Err(e) = record_launch(&id, &ag.aid, &ag.name, env, &rt, sign_over) {
        errln!("  ⚠ launch record not written ({e:#}) — capture will attribute this session by repo default.");
    }
    exec(&h.resume_cmd)
}

/// No sessions yet: start FRESH but bound to the agent, and say so.
fn start_fresh(ag: &agent::Agent, env: &Path, as_rt: Option<&str>) -> Result<i32> {
    let rt = crate::session::resolve_runtime(as_rt, &[], "start").map_err(|e| {
        anyhow::anyhow!("{e}\n  `{}` has no sessions yet, so there is no runtime to continue in — name one: agit start --as claude-code|codex", ag.name)
    })?;
    let cli = if rt == "codex" { "codex" } else { "claude" };
    outln!("┌ {} · {} · {rt}", ui::bold(&ag.name), ui::accent(&ui::tilde(env)));
    outln!("└ no sessions yet — starting FRESH, bound to this agent.");
    // The runtime mints the id, so there is nothing to write a launch record against yet: this session
    // will be attributed to the repo's default agent when captured. Said, never assumed.
    errln!(
        "  note: a fresh session gets its id from {cli}, so it has no launch record — capture files it \
         under this repo's default agent. Once it is snapped, `agit start --agent {}` carries it exactly.",
        ag.name
    );
    exec_in(cli, env)
}

/// Launch the runtime directly. No shell, so the repo path needs no quoting: `env` comes from `git
/// rev-parse --show-toplevel`, and under `sh -c` a path holding a space, `$`, a backtick or `;` would
/// break the command or inject into it.
fn exec_in(cli: &str, dir: &Path) -> Result<i32> {
    let status = std::process::Command::new(cli)
        .current_dir(dir)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to launch `{cli}` in {}: {e}", dir.display()))?;
    Ok(status.code().unwrap_or(0))
}

fn exec(cmd: &str) -> Result<i32> {
    let status = std::process::Command::new("sh").arg("-c").arg(cmd).status()?;
    Ok(status.code().unwrap_or(0))
}

// ─────────────────────── crypt: opt-in at-rest store encryption (Feature C) ───────────────────────

/// The git clean filter driver (`filter.agit-crypt.clean`). Reads plaintext on stdin, writes convergent
/// ciphertext on stdout. Invoked by git at staging, never by a human — so stdout carries RAW BYTES, the
/// one deliberate exception to the outln!/errln! sink rule (a git filter is a binary pipe). Keyed from
/// `$AGIT_HOME` regardless of cwd (git runs filters with cwd = store top). A failure exits nonzero so
/// `filter.agit-crypt.required=true` makes git abort rather than stage plaintext.
pub fn crypt_clean() -> Result<i32> {
    use std::io::{Read, Write};
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .context("crypt-clean: reading stdin")?;
    let keys = crate::crypt::keys_for_filter(&scope::agit_home()?)?;
    let out = crate::crypt::seal(&keys, &input);
    std::io::stdout()
        .lock()
        .write_all(&out)
        .context("crypt-clean: writing stdout")?;
    Ok(0)
}

/// The git smudge filter driver (`filter.agit-crypt.smudge`). Reads committed bytes on stdin, writes
/// plaintext on stdout. If stdin does NOT begin with the AGITCRYPT magic it is emitted unchanged — this
/// keeps checkout working on blobs committed before encryption was enabled, or a clone whose filter is
/// wired but whose key predates a given file. Raw bytes on stdout (see crypt_clean). A decrypt failure
/// (wrong/absent key, tampering) exits nonzero so `required=true` aborts the checkout loudly rather than
/// writing ciphertext into the working tree.
pub fn crypt_smudge() -> Result<i32> {
    use std::io::{Read, Write};
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .context("crypt-smudge: reading stdin")?;

    // Robustness rule: pass through anything that is not agit-crypt output, without needing a key.
    if !crate::crypt::is_ciphertext(&input) {
        std::io::stdout()
            .lock()
            .write_all(&input)
            .context("crypt-smudge: writing stdout")?;
        return Ok(0);
    }

    let keys = match crate::crypt::keys_for_filter(&scope::agit_home()?) {
        Ok(k) => k,
        Err(e) => {
            errln!("agit crypt-smudge: {e:#}");
            return Ok(1); // required=true → git aborts the checkout
        }
    };
    match crate::crypt::open(&keys, &input) {
        Ok(plain) => {
            std::io::stdout()
                .lock()
                .write_all(&plain)
                .context("crypt-smudge: writing stdout")?;
            Ok(0)
        }
        Err(e) => {
            errln!("agit crypt-smudge: {e:#}");
            Ok(1) // never write ciphertext-as-plaintext; nonzero → git aborts
        }
    }
}

/// The `.gitattributes` line that binds sessions to the filter. `-text` disables git's EOL
/// normalization so the clean/smudge round-trip is byte-exact (EOL munging before clean breaks
/// convergence). Committed, so a clone already carries the pattern; only the driver + key are local.
const CRYPT_ATTR_LINE: &str = "sessions/** filter=agit-crypt -text";
const CRYPT_FILTER_NAME: &str = "agit-crypt";

/// `agit a encrypt [--export [<file>]] [--import <keyfile>] [--force] [--yes]`.
pub fn agent_encrypt(args: &[String]) -> Result<i32> {
    let home = scope::agit_home()?;
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let force = args.iter().any(|a| a == "--force");

    if let Some(i) = args.iter().position(|a| a == "--export") {
        // The optional file argument is the next token that is not itself a flag.
        let file = args.get(i + 1).filter(|s| !s.starts_with('-')).map(|s| s.as_str());
        return crypt_export(&home, file);
    }
    if let Some(i) = args.iter().position(|a| a == "--import") {
        let Some(keyfile) = args.get(i + 1).filter(|s| !s.starts_with('-')) else {
            bail!("agit a encrypt --import needs a key file: agit a encrypt --import <keyfile>");
        };
        return crypt_import(&home, Path::new(keyfile), force, yes);
    }

    crypt_enable(&home, yes)
}

/// Print the two mandatory, non-negotiable warnings (req.5) before encryption does anything.
fn crypt_print_warnings() {
    errln!("agit encrypt — read both before continuing:");
    errln!(
        "  (1) The hub cannot render or server-side-scan an encrypted store — it never holds the key.\n\
         \x20     Encryption is only coherent for a no-hub, public-remote setup; you are trading hub\n\
         \x20     features for at-rest confidentiality."
    );
    errln!(
        "  (2) Your local secret gate now scans ENCRYPTED content, so it no longer sees plaintext\n\
         \x20     secrets in these sessions — the content is protected by encryption instead of by the\n\
         \x20     scanner."
    );
}

/// A yes/no gate honoured by `--yes` non-interactively; refuses (never hangs) when it cannot ask.
fn crypt_confirm(prompt: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !ui::interactive() {
        bail!("{prompt}\n  refusing without confirmation — re-run with --yes to proceed non-interactively");
    }
    out!("{prompt} [y/N] ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading confirmation")?;
    if !matches!(line.trim(), "y" | "Y" | "yes" | "Yes") {
        bail!("aborted — encryption not enabled");
    }
    Ok(())
}

/// Is the local filter driver already wired for this store?
fn crypt_filter_wired(store: &Path) -> bool {
    let (code, val) = scope::git_in_status(store, &["config", "--get", "filter.agit-crypt.clean"]);
    code == 0 && !val.trim().is_empty()
}

/// Set the three local `filter.agit-crypt.*` configs to invoke this very agit binary as the driver.
fn crypt_wire_filter(store: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("could not locate agit's own path")?;
    let q = crate::init::sh_single_quote(&exe.to_string_lossy());
    scope::git_in(store, &["config", "filter.agit-crypt.clean", &format!("{q} crypt-clean")])?;
    scope::git_in(store, &["config", "filter.agit-crypt.smudge", &format!("{q} crypt-smudge")])?;
    // required=true: if the filter can't run (key missing, decrypt fails) git ABORTS rather than
    // silently committing plaintext or checking out ciphertext — the whole point of "never silently".
    scope::git_in(store, &["config", "filter.agit-crypt.required", "true"])?;
    Ok(())
}

/// Ensure `.gitattributes` at the store root carries the sessions filter line (merge, don't clobber).
/// Returns true if the file was created or the line appended.
fn crypt_write_gitattributes(store: &Path) -> Result<bool> {
    let path = store.join(".gitattributes");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| {
        let t = l.trim();
        t.starts_with("sessions/**") && t.contains("filter=agit-crypt")
    }) {
        return Ok(false);
    }
    let mut s = existing;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(CRYPT_ATTR_LINE);
    s.push('\n');
    std::fs::write(&path, s).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}

/// Commit staged changes in `store` under a message, via `-F <tempfile>` (never `-m`, per the repo's
/// commit-message hygiene). No-op friendly: the caller checks there is something staged.
fn crypt_commit(store: &Path, message: &str) -> Result<()> {
    let mut msg = tempfile::NamedTempFile::new().context("cannot create a commit-message temp file")?;
    use std::io::Write;
    msg.write_all(message.as_bytes())?;
    msg.flush()?;
    // --no-verify: the message-carrying commit does not need the pre-commit scan (agit's own gate runs
    // on the paths it stages elsewhere); this keeps enabling encryption from tripping git's hook.
    scope::git_in(
        store,
        &["commit", "--no-verify", "-F", msg.path().to_string_lossy().as_ref()],
    )?;
    Ok(())
}

/// `agit a encrypt` — enable/wire encryption on the resolved store. Idempotent.
fn crypt_enable(home: &Path, yes: bool) -> Result<i32> {
    let a = agent::resolve(None)?;
    let store = a.store.clone();

    let key_present = crate::crypt::read_master(home)?.is_some();
    let attr_present = store.join(".gitattributes").exists()
        && std::fs::read_to_string(store.join(".gitattributes"))
            .unwrap_or_default()
            .lines()
            .any(|l| l.trim().starts_with("sessions/**") && l.contains("filter=agit-crypt"));
    let filter_present = crypt_filter_wired(&store);

    if key_present && attr_present && filter_present {
        outln!("agit-crypt is already enabled on {} ({}).", a.name, a.aid);
        outln!("  key        {}", crate::crypt::key_path(home).display());
        outln!("  filter     filter.agit-crypt.{{clean,smudge,required}} set locally");
        outln!("  attributes sessions/** filter=agit-crypt -text (committed)");
        outln!("  Nothing to do. Share the key out of band with `agit a encrypt --export`.");
        return Ok(0);
    }

    crypt_print_warnings();

    // Hub-remote gate: an encrypted store pushes ciphertext the hub cannot use. Warn + require confirm.
    if agent::store_remotes(&store).iter().any(|(name, _)| name == "hub") {
        crypt_confirm(
            "This store has a `hub` remote, which will receive ciphertext it cannot render or scan. Enable encryption anyway?",
            yes,
        )?;
    } else {
        crypt_confirm("Enable at-rest encryption on this store?", yes)?;
    }

    // (4) mint the key if absent.
    let minted = !key_present;
    let _ = crate::crypt::load_or_create_master(home)?;
    if minted {
        errln!(
            "  ⚠ minted a NEW symmetric key at {}. BACK IT UP NOW (password manager / Signal).\n\
             \x20     There is no escrow and no recovery: losing this key loses every blob it encrypts.\n\
             \x20     It is machine-global, never committed, never pushed.",
            crate::crypt::key_path(home).display()
        );
    }

    let _lock = crate::session::lock_store(&store)?;

    // (5) write/merge .gitattributes, (6) set the three local filter configs.
    let attr_changed = crypt_write_gitattributes(&store)?;
    crypt_wire_filter(&store)?;

    // (7) commit .gitattributes.
    let _ = scope::git_in_status(&store, &["add", "--", ".gitattributes"]);
    if scope::git_in_status(&store, &["diff", "--cached", "--quiet"]).0 != 0 {
        crypt_commit(&store, "chore(crypt): enable agit-crypt on sessions\n\nCommits sessions/** as ciphertext via a convergent clean/smudge filter.")?;
        if attr_changed {
            outln!("  committed .gitattributes (sessions/** filter=agit-crypt -text)");
        }
    }

    // (8) re-encrypt already-tracked plaintext: --renormalize pushes existing blobs through the clean
    // filter. Warn that HISTORY still holds the old plaintext (going-forward at-rest encryption).
    let _ = scope::git_in_status(&store, &["add", "--renormalize", "--", "sessions"]);
    if scope::git_in_status(&store, &["diff", "--cached", "--quiet"]).0 != 0 {
        crypt_commit(
            &store,
            "chore(crypt): re-encrypt tracked sessions\n\nGoing-forward at-rest encryption. Prior commits still hold plaintext; a full purge needs git-filter-repo.",
        )?;
        outln!("  re-encrypted already-tracked sessions (git add --renormalize)");
        errln!(
            "  ⚠ git HISTORY still holds the old plaintext. This is going-forward encryption; a full\n\
             \x20     purge of past blobs needs git-filter-repo (out of scope)."
        );
    }

    outln!("agit-crypt enabled on {} ({}).", a.name, a.aid);
    outln!("  Share the key with teammates out of band: `agit a encrypt --export <file>`.");
    Ok(0)
}

/// `agit a encrypt --export [<file>]` — reveal the symmetric master for out-of-band sharing.
fn crypt_export(home: &Path, file: Option<&str>) -> Result<i32> {
    let master = crate::crypt::load_or_create_master(home)?;
    let hex_key = hex::encode(master);
    errln!(
        "agit-crypt master key — this IS the secret that decrypts every encrypted store.\n\
         \x20 Distribute it out of band (password manager / Signal). NEVER commit or push it."
    );
    match file {
        Some(f) => {
            crate::agent::write_secret_0600(Path::new(f), &hex_key)?;
            outln!("  wrote the key (0600) to {f}");
        }
        None => {
            // The raw payload goes to stdout deliberately (this IS the key), one hex line.
            outln!("{hex_key}");
        }
    }
    Ok(0)
}

/// `agit a encrypt --import <keyfile>` — install a teammate's key, then wire the local filter and
/// re-checkout so a clone goes from raw ciphertext to decrypted working tree.
fn crypt_import(home: &Path, keyfile: &Path, force: bool, yes: bool) -> Result<i32> {
    let text = std::fs::read_to_string(keyfile)
        .with_context(|| format!("cannot read key file {}", keyfile.display()))?;
    let raw = hex::decode(text.trim())
        .with_context(|| format!("{} is not valid hex", keyfile.display()))?;
    let incoming: [u8; 32] = raw
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("{} is not a 32-byte key", keyfile.display()))?;

    if let Some(existing) = crate::crypt::read_master(home)? {
        if existing != incoming && !force {
            bail!(
                "a DIFFERENT crypt key already exists at {}.\n\
                 \x20      Overwriting it orphans everything encrypted under the old key. Re-run with --force to replace it.",
                crate::crypt::key_path(home).display()
            );
        }
    }

    std::fs::create_dir_all(crate::crypt::key_path(home).parent().unwrap())?;
    crate::agent::write_secret_0600(&crate::crypt::key_path(home), &hex::encode(incoming))?;
    outln!("installed the crypt key at {}", crate::crypt::key_path(home).display());

    // Wire the local filter for the resolved store and offer to re-checkout so ciphertext in the
    // working tree becomes plaintext. A missing store (import before clone) is not fatal — the key is in.
    match agent::resolve(None) {
        Ok(a) => {
            crypt_wire_filter(&a.store)?;
            outln!("  wired filter.agit-crypt.{{clean,smudge,required}} for {} ({})", a.name, a.aid);
            if crypt_confirm("Re-checkout sessions/** now to decrypt the working tree?", yes).is_ok() {
                let (code, _) = scope::git_in_status(&a.store, &["checkout", "--", "sessions"]);
                if code == 0 {
                    outln!("  re-checked-out sessions/** (now decrypted)");
                } else {
                    errln!("  ⚠ could not re-checkout sessions/** — run `git checkout -- .` in the store yourself");
                }
            }
        }
        Err(_) => {
            outln!("  (no agent resolves here yet — after `agit a clone <name>`, run `agit a encrypt` to wire the filter)");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod launch_record_tests {
    use super::*;

    fn launch(session: &str, aid: &str, name: &str) -> Launch {
        Launch {
            session: session.into(),
            aid: aid.into(),
            name: name.into(),
            env: "/code/web".into(),
            runtime: "claude-code".into(),
            started: "2026-07-16T00:00:00Z".into(),
            provenance: None,
        }
    }

    /// The correctness core of multi-agent: two agents in ONE repo dump into the SAME folder, so the id
    /// is the only thing that can tell their sessions apart.
    #[test]
    fn two_agents_in_one_repo_are_told_apart_by_session_id() {
        let home = tempfile::tempdir().unwrap();
        record_launch_at(home.path(), &launch("sess-frontend", "agt_01", "frontend")).unwrap();
        record_launch_at(home.path(), &launch("sess-api", "agt_02", "api")).unwrap();

        assert_eq!(lookup_launch_at(home.path(), "sess-frontend").unwrap().aid, "agt_01");
        assert_eq!(lookup_launch_at(home.path(), "sess-api").unwrap().aid, "agt_02");
        assert!(lookup_launch_at(home.path(), "never-launched").is_none(), "no record must not resolve to a neighbour");
    }

    #[test]
    fn the_log_is_append_only_and_the_last_record_wins() {
        let home = tempfile::tempdir().unwrap();
        record_launch_at(home.path(), &launch("s", "agt_01", "frontend")).unwrap();
        record_launch_at(home.path(), &launch("s", "agt_02", "api")).unwrap();
        assert_eq!(lookup_launch_at(home.path(), "s").unwrap().aid, "agt_02", "re-launch supersedes");
        assert_eq!(
            std::fs::read_to_string(launches_file(home.path())).unwrap().lines().count(),
            2,
            "history is kept, not rewritten"
        );
    }

    /// A record is authoritative and says nothing; a fallback must always disclose itself.
    #[test]
    fn attribution_by_default_agent_is_never_silent() {
        let launched = Attribution::Launched(Box::new(launch("s", "agt_01", "frontend")));
        assert_eq!(launched.aid(), "agt_01");
        assert!(launched.note().is_none(), "an authoritative record has nothing to disclose");

        let fallback = Attribution::RepoDefault { aid: "agt_09".into(), name: "api".into() };
        let note = fallback.note().expect("a guess MUST be reported");
        assert!(note.contains("no launch record"), "{note}");
        assert!(note.contains("api"), "the note must name the agent it filed under: {note}");
    }
}

#[cfg(test)]
mod crypt_gate_tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success();
        assert!(ok, "git {args:?} failed");
    }

    /// Test 9 — regression guard for the push-breaking interaction. A staged AGITCRYPT blob is
    /// high-entropy ciphertext that would trip the `high-entropy-string` rule on every encrypted
    /// session and block every commit/push. The staged scan path must skip it (via is_probably_binary),
    /// while the SAME bytes in plaintext must still be caught — proving the seal is what suppresses it.
    #[test]
    fn staged_ciphertext_is_skipped_but_plaintext_is_not() {
        let keys = crate::crypt::derive_subkeys(&[3u8; 32]);
        let secret = b"aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY0000\n";

        // Plaintext staged → the gate DOES find it (control).
        let plain = tempfile::tempdir().unwrap();
        git(plain.path(), &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(plain.path().join("sessions")).unwrap();
        std::fs::write(plain.path().join("sessions/s.jsonl"), secret).unwrap();
        git(plain.path(), &["add", "sessions/s.jsonl"]);
        let allow = scan::Allowlist::load(plain.path());
        assert!(
            !staged_findings(plain.path(), &allow).is_empty(),
            "the plaintext secret must trip the gate, or this test proves nothing"
        );

        // Sealed staged → the gate finds NOTHING (the content is protected by encryption instead).
        let enc = tempfile::tempdir().unwrap();
        git(enc.path(), &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(enc.path().join("sessions")).unwrap();
        std::fs::write(enc.path().join("sessions/s.jsonl"), crate::crypt::seal(&keys, secret)).unwrap();
        git(enc.path(), &["add", "sessions/s.jsonl"]);
        let allow = scan::Allowlist::load(enc.path());
        let findings = staged_findings(enc.path(), &allow);
        assert!(findings.is_empty(), "encrypted blob must yield no findings, got {}", findings.len());
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    fn key(home: &Path) -> ed25519_dalek::SigningKey {
        agent::load_or_create_signing_key(home).unwrap()
    }

    /// The happy path: sign a session's content, then self-verify it. The signature must check out and
    /// report the aid and committer it was signed for.
    #[test]
    fn sign_then_verify_round_trips() {
        let home = tempfile::tempdir().unwrap();
        let k = key(home.path());
        let content = "the session transcript, verbatim\n";
        let p = sign_provenance(&k, content, "agt_01", "dev@x.com", "2026-07-16T00:00:00Z");

        let status = verify_provenance(content, Some(&p));
        assert!(status.is_verified(), "a freshly signed session must verify: {status:?}");
        match status {
            ProvenanceStatus::Verified { aid, email, pubkey } => {
                assert_eq!(aid, "agt_01");
                assert_eq!(email, "dev@x.com");
                assert_eq!(pubkey, hex::encode(k.verifying_key().to_bytes()), "the signer's own key");
            }
            other => panic!("expected Verified, got {other:?}"),
        }
    }

    /// The tamper case: change one byte of the transcript after signing, and verification must fail —
    /// the recorded content digest no longer matches what is on disk.
    #[test]
    fn a_tampered_session_fails_verification() {
        let home = tempfile::tempdir().unwrap();
        let k = key(home.path());
        let p = sign_provenance(&k, "original content\n", "agt_01", "dev@x.com", "t0");

        let status = verify_provenance("original content, edited\n", Some(&p));
        assert!(!status.is_verified(), "an edited transcript must not verify");
        assert!(
            matches!(status, ProvenanceStatus::ContentTampered { .. }),
            "the reason must be the content digest, not the signature: {status:?}"
        );
    }

    /// A signature that does not belong to its recorded key (here, a record whose pubkey was swapped for
    /// a second machine's) is rejected as a bad signature — not a tamper, not a pass.
    #[test]
    fn a_signature_that_does_not_match_its_key_is_rejected() {
        let home1 = tempfile::tempdir().unwrap();
        let home2 = tempfile::tempdir().unwrap();
        let content = "content\n";
        let mut p = sign_provenance(&key(home1.path()), content, "agt_01", "dev@x.com", "t0");
        // Claim a different machine's public key while keeping machine 1's signature.
        p.pubkey = hex::encode(key(home2.path()).verifying_key().to_bytes());

        assert_eq!(
            verify_provenance(content, Some(&p)),
            ProvenanceStatus::BadSignature,
            "a signature must not verify against a foreign key"
        );
    }

    /// The graceful-degradation contract: a session with NO provenance recorded is "unsigned", reported
    /// plainly — never a panic and never a hard failure.
    #[test]
    fn a_session_with_no_signature_is_unsigned_not_a_panic() {
        let status = verify_provenance("anything at all", None);
        assert_eq!(status, ProvenanceStatus::Unsigned);
        assert!(!status.is_verified());
        assert!(status.summary().contains("no signature"), "{}", status.summary());
    }

    /// Malformed pubkey/signature hex must degrade to `false`, never panic: verification runs on data a
    /// teammate's clone supplied, which cannot be trusted to be well-formed.
    #[test]
    fn malformed_signature_material_never_panics() {
        assert!(!agent::verify_hex("not-hex", b"msg", "also-not-hex"));
        assert!(!agent::verify_hex("", b"msg", ""));
        assert!(!agent::verify_hex("aa", b"msg", "bb"), "too-short but valid hex");
    }

    /// The machine key is minted once and then stable: a second load returns the identical key, so a
    /// signature made today still verifies tomorrow.
    #[test]
    fn the_machine_key_is_stable_across_loads() {
        let home = tempfile::tempdir().unwrap();
        let first = key(home.path());
        let again = key(home.path());
        assert_eq!(first.to_bytes(), again.to_bytes(), "the key must not rotate on reload");
    }
}

#[cfg(test)]
mod start_tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        // Per-invocation identity only: a global git config would clobber the developer's real one.
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    /// The bug this keys on: git does not preserve mtimes. `agit a clone` CLONES the store, and a clone
    /// stamps every file at checkout time — verified identical to the nanosecond — so an mtime-ordered
    /// leaf-finder returns whichever session WalkDir happened to hand back last. Recency must therefore
    /// come from recorded CONTENT. This is acceptance criterion 3: bob clones and picks up alice's LATEST.
    #[test]
    fn recency_survives_a_clone_which_flattens_every_mtime() {
        let src = tempfile::tempdir().unwrap();
        let d = src.path();
        git(d, &["init", "-q", "-b", "main", "."]);
        let s = d.join("sessions/web/claude-code");

        // `old` is genuinely older BY CONTENT, but is written second so that both write order and
        // WalkDir order favour it — the test must not pass by luck.
        write(
            &s.join("new.jsonl"),
            "{\"sessionId\":\"n\",\"timestamp\":\"2026-07-16T12:00:00Z\",\"type\":\"user\"}\n",
        );
        write(
            &s.join("old.jsonl"),
            "{\"sessionId\":\"o\",\"timestamp\":\"2020-01-01T00:00:00Z\",\"type\":\"user\"}\n",
        );
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "sessions"]);

        let dst = tempfile::tempdir().unwrap();
        let clone = dst.path().join("c");
        git(d, &["clone", "-q", ".", &clone.to_string_lossy()]);

        // The premise, asserted rather than remembered: after a clone there is no recency left in the
        // fs. Checkout stamps both files as it writes them, so they land together — but "together" is
        // scheduling, not an atomic act, and git promises no more than that. Demanding the two stamps
        // match to the NANOSECOND made this fail whenever a loaded machine straddled a clock tick. What
        // must hold is that the gap carries no information: 6 YEARS apart by content, indistinguishable
        // on disk.
        let m = |n: &str| {
            std::fs::metadata(clone.join("sessions/web/claude-code").join(n)).unwrap().modified().unwrap()
        };
        let (a, b) = (m("old.jsonl"), m("new.jsonl"));
        let gap = a.duration_since(b).or_else(|_| b.duration_since(a)).unwrap_or_default();
        assert!(
            gap < std::time::Duration::from_secs(60),
            "a clone must erase the content-age gap (these are 6 years apart by content), got {gap:?} — \
             otherwise this test proves nothing"
        );

        let latest = latest_session(&clone).expect("a cloned store still has sessions");
        assert!(
            latest.path.ends_with("new.jsonl"),
            "picked {:?} — ordering fell back to the filesystem, which a clone has erased",
            latest.path
        );
    }

    /// Both store layouts resolve, from one walk. A flat store is never migrated (§12 step 2), so
    /// `sessions/<rt>/` is not a legacy path to be read second — it is one of two live layouts, and a
    /// reader that understood only the partitioned one would report an existing store as EMPTY.
    #[test]
    fn latest_session_spans_environments_and_ignores_sidecars() {
        let store = tempfile::tempdir().unwrap();
        let s = store.path().join("sessions");
        // the partitioned layout (sessions/<env>/<rt>/) and the flat one both resolve
        write(&s.join("web/claude-code/old.jsonl"), "{}\n");
        write(&s.join("api/codex/new.jsonl"), "{}\n");
        // a session's sidecars are not sessions
        write(&s.join("api/codex/new/subagents/sub.jsonl"), "{}\n");
        // and a merge transcript is not one either
        write(&s.join("merges/a-b.md"), "# transcript\n");

        // make ordering explicit rather than trusting write order
        let newer = std::time::SystemTime::now();
        let older = newer - std::time::Duration::from_secs(7200);
        filetime(&s.join("web/claude-code/old.jsonl"), older);
        filetime(&s.join("api/codex/new.jsonl"), newer);
        filetime(&s.join("api/codex/new/subagents/sub.jsonl"), newer + std::time::Duration::from_secs(60));

        let all = store_sessions(store.path());
        assert_eq!(all.len(), 2, "only real sessions: {:?}", all.iter().map(|x| &x.path).collect::<Vec<_>>());

        let latest = latest_session(store.path()).unwrap();
        assert!(latest.path.ends_with("api/codex/new.jsonl"), "picked {:?}", latest.path);
        assert_eq!(latest.runtime, "codex", "the runtime comes from the session, not a default");
        assert_eq!(latest.env_slug.as_deref(), Some("api"), "an env-partitioned store reports where it ran");
        // the newest FILE is a sidecar; it must not become the session we carry
        assert!(!latest.path.to_string_lossy().contains("subagents"));
    }

    /// The regression the design calls out: `git log -1 --name-only` prints NOTHING on a merge commit,
    /// so a log-derived leaf-finder finds nothing exactly after a merge/pull. Asserted here against real
    /// git, so the trap stays proven rather than remembered.
    #[test]
    fn the_leaf_survives_a_merge_commit_because_it_is_not_read_from_git_log() {
        let store = tempfile::tempdir().unwrap();
        let d = store.path();
        git(d, &["init", "-q", "-b", "main", "."]);
        write(&d.join("sessions/claude-code/base.jsonl"), "{}\n");
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "base"]);

        git(d, &["checkout", "-qb", "peer"]);
        write(&d.join("sessions/claude-code/theirs.jsonl"), "{}\n");
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "theirs"]);

        git(d, &["checkout", "-q", "main"]);
        write(&d.join("sessions/claude-code/mine.jsonl"), "{}\n");
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "mine"]);
        git(d, &["merge", "-q", "--no-ff", "peer", "-m", "merge peer"]);

        // the trap, demonstrated in-repo: the merge commit names no files at all
        assert!(git(d, &["log", "-1", "--format=", "--name-only", "HEAD"]).is_empty(), "git changed: a merge commit now names files");
        assert!(!git(d, &["log", "-1", "--format=", "--name-only", "HEAD^"]).is_empty(), "a non-merge commit does name them");

        // …and the leaf-finder is unaffected, because it reads the files.
        let latest = latest_session(d).expect("a merge must not hide the agent's sessions");
        assert_eq!(latest.runtime, "claude-code");
        assert_eq!(store_sessions(d).len(), 3, "every session survives the merge");
    }

    fn filetime(p: &Path, t: std::time::SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(t).unwrap();
    }

    /// Acceptance criterion 3: bob clones the agent and picks up alice's LATEST session. `agit a clone`
    /// is a `git clone`, and git does not preserve mtimes — every file gets the checkout time — so an
    /// mtime-ordered leaf-finder returns whatever the directory walk happened to hand back last.
    /// Asserted against real git, so the trap stays proven rather than remembered.
    #[test]
    fn the_latest_session_survives_a_real_clone_because_recency_is_recorded_not_mtimed() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        git(&store, &["init", "-q", "-b", "main", "."]);

        // alice's two sessions. `new` is the one bob must pick up.
        let s = store.join("sessions/claude-code");
        write(&s.join("old.jsonl"), "{\"timestamp\":\"2026-07-10T10:00:00.000Z\"}\n");
        write(&s.join("old.agit.json"), "{\"last_activity\":\"2026-07-10T10:00:00.000Z\"}\n");
        write(&s.join("new.jsonl"), "{\"timestamp\":\"2026-07-16T10:00:00.000Z\"}\n");
        write(&s.join("new.agit.json"), "{\"last_activity\":\"2026-07-16T10:00:00.000Z\"}\n");
        git(&store, &["add", "-A"]);
        git(&store, &["commit", "-qm", "alice's sessions"]);
        assert_eq!(latest_session(&store).unwrap().path.file_stem().unwrap(), "new");

        // bob: `agit a clone` → git clone.
        let clone = d.path().join("clone");
        let out = Command::new("git").args(["clone", "-q"]).arg(&store).arg(&clone).output().unwrap();
        assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(store_sessions(&clone).len(), 2, "the clone must carry both sessions");

        // The sidecars travelled (git carries content), so recency did too.
        assert_eq!(
            latest_session(&clone).expect("bob must find alice's sessions").path.file_stem().unwrap(),
            "new"
        );

        // …and it is genuinely not the filesystem talking: hand the STALE session the newest mtime.
        // After a checkout the mtimes carry no recency at all, so this is not a contrived ordering —
        // it is simply one the filesystem is free to report.
        let cs = clone.join("sessions/claude-code");
        filetime(&cs.join("old.jsonl"), std::time::SystemTime::now());
        filetime(&cs.join("new.jsonl"), std::time::SystemTime::now() - std::time::Duration::from_secs(7200));
        let latest = latest_session(&clone).unwrap();
        assert_eq!(latest.path.file_stem().unwrap(), "new", "mtime overruled the recorded activity: {:?}", latest.path);
    }

    /// A store captured before sidecars existed must still order: the transcript records its own time.
    #[test]
    fn recency_falls_back_to_the_transcripts_own_last_timestamp_when_there_is_no_sidecar() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("sessions/codex");
        write(
            &s.join("first.jsonl"),
            "{\"timestamp\":\"2026-07-01T00:00:00.000Z\",\"type\":\"session_meta\"}\n\
             {\"timestamp\":\"2026-07-02T00:00:00.000Z\",\"type\":\"event_msg\"}\n",
        );
        write(&s.join("second.jsonl"), "{\"timestamp\":\"2026-07-03T00:00:00.000Z\",\"type\":\"session_meta\"}\n");
        // the filesystem says the opposite of what the transcripts say
        filetime(&s.join("first.jsonl"), std::time::SystemTime::now());
        filetime(&s.join("second.jsonl"), std::time::SystemTime::now() - std::time::Duration::from_secs(7200));

        assert_eq!(
            latest_session(d.path()).unwrap().path.file_stem().unwrap(),
            "second",
            "recency must come from the transcript, not from the file"
        );
        // the session's LAST activity, not its first
        let all = store_sessions(d.path());
        let first = all.iter().find(|x| x.path.ends_with("first.jsonl")).unwrap();
        assert_eq!(first.last_activity.unwrap().to_rfc3339(), "2026-07-02T00:00:00+00:00");
    }

    /// A session that records no time at all must not outrank one that does — mtime is the last
    /// resort, and after a clone it is pure noise.
    #[test]
    fn a_recorded_time_beats_a_session_that_records_none() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("sessions/claude-code");
        write(&s.join("timed.jsonl"), "{\"timestamp\":\"2026-07-01T00:00:00.000Z\"}\n");
        write(&s.join("untimed.jsonl"), "{\"type\":\"queue-operation\"}\n");
        filetime(&s.join("untimed.jsonl"), std::time::SystemTime::now());
        filetime(&s.join("timed.jsonl"), std::time::SystemTime::now() - std::time::Duration::from_secs(7200));

        assert_eq!(latest_session(d.path()).unwrap().path.file_stem().unwrap(), "timed");
    }
}

#[cfg(test)]
mod install_id_tests {
    use super::*;

    /// Regression lock. codex advertises "UUID or thread name", but a name-id install silently starts a
    /// FRESH session (verified against codex 0.144.4: name-resume answered with zero history, exit 0).
    /// Only a UUID is matched against the rollout files on disk. Never hand a runtime a non-UUID id.
    #[test]
    fn install_id_is_always_a_uuid_even_for_codex() {
        for rt in ["codex", "claude-code"] {
            let id = install_id(rt, Some("feature-a"), "seed-content");
            assert!(!id.starts_with("feature-a"), "{rt}: got a proper name, not a uuid: {id}");
            let parts: Vec<&str> = id.split('-').collect();
            assert_eq!(parts.len(), 5, "{rt}: not uuid-shaped: {id}");
            assert_eq!(
                (parts[0].len(), parts[1].len(), parts[2].len(), parts[3].len(), parts[4].len()),
                (8, 4, 4, 4, 12),
                "{rt}: not 8-4-4-4-12: {id}"
            );
            assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{rt}: non-hex: {id}");
        }
    }
}
