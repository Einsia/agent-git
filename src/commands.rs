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
        .filter(|e| !is_self_encrypted_artifact(&e.path().strip_prefix(root).unwrap_or(e.path()).to_string_lossy()))
        .map(|e| e.path().to_path_buf())
        .collect()
}

/// The committed keybox (`.agit/keybox.jsonl`) is wrap-ciphertext BY DESIGN — one-shot X25519/AEAD
/// envelopes of the content key, already excluded from the crypt filter. Its base64 wraps are
/// high-entropy and would otherwise trip the `high-entropy-string` rule on EVERY keybox-encrypted store,
/// blocking commit and push (client hook + hub pre-receive), exactly as an AGITCRYPT ciphertext blob
/// would if the scan did not already skip it. So the secret scan skips this self-encrypting artifact.
fn is_self_encrypted_artifact(rel: &str) -> bool {
    let rel = rel.replace('\\', "/");
    rel == crate::keybox::KEYBOX_REL
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
        // The committed keybox is wrap-ciphertext (see is_self_encrypted_artifact) — never a plaintext
        // finding; skip it exactly as the binary/ciphertext branch below skips an AGITCRYPT blob.
        if is_self_encrypted_artifact(name) {
            continue;
        }
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

    errln!("{}", ui::warn(&format!("agit: secret gate: suspected secrets before {verb}:")));
    for (name, f) in &findings {
        errln!("  {name}:{}  [{}]  {}", f.line, f.rule, f.excerpt);
    }

    if allow_override_enabled() {
        errln!(
            "  {ALLOW_ENV}=1: gate bypassed, {verb} proceeds with suspected secrets (logged)"
        );
        if verb == "push" {
            errln!(
                "  push: the hub runs its own secret gate this flag does not bypass; mark the line (`{}`) or ask the operator to allowlist",
                scan::ALLOW_PRAGMA,
            );
        }
        Gate::Overridden
    } else {
        // State plainly that the action did NOT happen, so a blocked gate is never read as success.
        let not_done = match verb {
            "commit" => "No commit created",
            "push" => "Nothing pushed",
            "snap" => "Nothing committed",
            _ => "Did not complete",
        };
        let n = findings.len();
        let plural = if n == 1 { "" } else { "s" };
        errln!(
            "{not_done}: {n} suspected secret{plural}. Fix, allow-list (`{}` / `{}`), or {ALLOW_ENV}=1 to override",
            scan::ALLOW_PRAGMA,
            scan::ALLOW_FILE,
        );
        Gate::Blocked(findings.len())
    }
}

/// scan_root wrap-up: unifies the "found/not found" report and exit code.
fn finish_scan(total: usize, staged: bool, scanned: usize) -> Result<i32> {
    if total > 0 {
        errln!("\n{total} suspected. Once pushed, a teammate who pulls carries them along.");
        // scan has NO --no-verify: it is a report, not a git hook. The commit/push gates it mirrors have
        // exactly one disclosed override, AGIT_ALLOW_SECRETS; point there, not at a flag that does not
        // exist and would walk the user toward silently committing the secret.
        errln!(
            "Fix, allow-list (`{}` / `{}`), or {ALLOW_ENV}=1 to override the commit/push gate (logged).",
            scan::ALLOW_PRAGMA,
            scan::ALLOW_FILE,
        );
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
/// The install id for a materialized session: a UUID-shaped id that is **deterministic** in
/// `(source session id, target runtime)`.
///
/// UUID shape is a regression lock, not a style choice. `codex exec resume` advertises "UUID **or thread
/// name**", so agit briefly installed codex sessions under a proper name (`feature-a-3f9a2c`). That is
/// broken, and it fails OPEN — verified against codex 0.144.4 with a fact only the history could know:
///   * UUID id, file on disk, absent from codex's index → resume RECALLED the fact (codex reads the file)
///   * proper-name id, identical file      → resume answered from thin air, ZERO history, exit 0
/// Root cause: a non-UUID is resolved as a thread name via ~/.codex/state_5.sqlite (`threads` has
/// id/rollout_path/title — no name column), and a file agit drops on disk is never indexed there. So a
/// named install silently starts a FRESH session. A UUID, by contrast, is matched against the rollout
/// files themselves and works. Proper names therefore survive only as a human-facing LABEL (see
/// convo::proper_name), never as the id.
///
/// DETERMINISM is the fix for auto-convert store bloat. The old `fresh_id()` minted a new random UUID on
/// every pass, so the watcher (whose in-process `seen` set resets on each restart) re-converted the SAME
/// source into a NEW file, which capture then snapped as a NEW committed session — one source fanned out
/// into a fresh rollup on every daemon restart. Keying the id on the SOURCE session id (plus the target
/// runtime) makes re-converting the same source resolve to the SAME id, so `register::install` overwrites
/// the one file in place and the next snap sees identical content: nothing new to commit. A genuinely-new
/// source has a different source id, so it still converts to its own id.
fn install_id(to_rt: &str, source_id: &str) -> String {
    stable_uuid(&format!("{source_id}\u{0}{to_rt}"))
}

/// A deterministic UUID-shaped id (8-4-4-4-12) derived from `key`. Same shape as [`crate::convo::fresh_id`]
/// — version nibble 7, variant nibble 8, so codex/claude both accept it as a real id — but sourced from
/// `sha256(key)` alone, with no clock or pid, so the SAME key always yields the SAME id.
fn stable_uuid(key: &str) -> String {
    let hex = crate::convo::sha256_hex(key);
    format!(
        "{}-{}-7{}-8{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
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
    // Key the id on the SOURCE session's own id (its file stem), so re-converting the same source is
    // idempotent — see `install_id`. A source with no usable file stem (e.g. converting an anonymous
    // in-memory transcript) falls back to a content hash, which is still stable for identical input.
    let source_id = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| convo::sha256_hex(text));
    let new_id = install_id(to, &source_id);
    // Resume/convert install a session to run in THIS environment, not where it was captured. Default
    // the target cwd to the agent's repo root (else the current dir) so BOTH the runtime session slug
    // and the transcript's own cwd point where you resume. Without this a session captured on another
    // machine or path installs under the CAPTURE path's slug, and `claude --resume` (which resolves by
    // the dir you run it in) reports "No conversation found" -- the collaboration/merge resume break.
    let cwd_override = cwd_override.or_else(|| {
        crate::scope::env_root()
            .ok()
            .or_else(|| std::env::current_dir().ok())
            .map(|p| p.display().to_string())
    });
    let opts = ConvertOpts { cwd: cwd_override, new_id: new_id.clone() };
    let (out, ir) = convo::convert(src, from, to, &opts)?;
    let cwd = match opts.cwd.clone().or_else(|| ir.cwd.clone()) {
        Some(c) => PathBuf::from(c),
        None => std::env::current_dir()?,
    };
    Ok((new_id, out, ir, cwd))
}

pub fn convert_cmd(
    sel: Option<&str>,
    from: Option<String>,
    to: &str,
    cwd_override: Option<String>,
    write: bool,
) -> Result<i32> {
    use crate::convo;

    // Unified resolution: a file path / session id -> that transcript; an agent NAME -> its latest
    // session; no selector -> the active agent's latest session.
    let src = resolve_session_selector(sel)?;
    let src = src.as_path();
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
                            errln!("  ⚠ {from}→{to} launch record not written ({err:#}); capture will attribute this copy by repo default.");
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

/// `agit resume [<sel>] [--as <rt>] [--cwd <path>] [--exec]` -- the universal loader: install a
/// session so a runtime can resume it (converting across runtimes when `--as` differs from the source),
/// then print or (with --exec) launch the resume command. A thin, first-class wrapper over convert/register.
///
/// `<sel>` follows the unified resolution (see [`classify_session_selector`]): a file path or session
/// id is unchanged, an agent NAME resumes that agent's latest session, and NO argument resumes the
/// ACTIVE agent's latest session. (`agit start`, by contrast, launches a fresh runtime here carrying
/// that latest context; `agit resume` continues the existing session record itself.)
pub fn resume_cmd(
    sel: Option<&str>,
    as_rt: Option<String>,
    cwd_override: Option<String>,
    env_override: Option<String>,
    exec: bool,
    relocate: bool,
) -> Result<i32> {
    use crate::convo;

    let src = resolve_session_selector(sel)?;
    let src = src.as_path();
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
                anyhow::bail!("{} is not a git repository; an Environment is a code repo", p.display());
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

// ─────────────────────── relocate: bring sessions started in the wrong dir here ───────────────────────

/// `agit relocate [<selector>] [--to <path>] [--yes]` — the escape hatch for a session started in the
/// WRONG directory (the runtime ran in a parent/monorepo dir, or the same repo at another path, so
/// capture dropped it as not-this-repo and stranded the work).
///
/// BARE `agit relocate` is the common case: it auto-detects every stranded session that belongs HERE
/// (the same `plausibly_here` test the drop-warning uses), lists them, asks once, then rewrites each
/// transcript's cwd to this repo and captures it into the active agent's store — no id or path typed. A
/// `<selector>` narrows it to one session; `--to` overrides the destination (default: this repo's root).
pub fn relocate_cmd(selector: Option<&str>, to: Option<String>, yes: bool) -> Result<i32> {
    // Destination defaults to this repo's root; `--to` overrides but MUST be a git work tree (a session
    // installs under a slug derived from a real checkout, mirroring `resume --env`'s check).
    let env = match to {
        Some(p) => {
            let cp = std::fs::canonicalize(&p).map_err(|_| anyhow::anyhow!("no such path: {p}"))?;
            let (rc, _) = scope::git_in_status(&cp, &["rev-parse", "--is-inside-work-tree"]);
            if rc != 0 {
                anyhow::bail!("{} is not a git work tree; a relocate destination is a code repo", cp.display());
            }
            scope::git_toplevel(&cp).unwrap_or(cp)
        }
        None => scope::env_root()?,
    };

    let mut stranded = crate::session::stranded_here(&env);
    // A selector targets ONE session (uncommon path): match its id, its transcript path, or a substring
    // of the directory it ran in (the recorded-cwd slug the user would have seen).
    if let Some(sel) = selector {
        stranded.retain(|s| relocate_selector_matches(s, sel));
        if stranded.is_empty() {
            anyhow::bail!(
                "no stranded session here matches `{sel}`.\n  run `agit relocate` with no arguments to list what would move."
            );
        }
    }

    if stranded.is_empty() {
        // Not an error: running it in the right place with nothing stranded is a valid, complete outcome.
        outln!("nothing to relocate: no sessions here were started in another directory.");
        return Ok(0);
    }

    outln!("Sessions that ran elsewhere but belong in {}:", env.display());
    for s in &stranded {
        let gist = relocate_gist(&s.path, s.runtime);
        let when = recorded_activity(&s.path)
            .map(|t| ui::ago(std::time::SystemTime::from(t)))
            .unwrap_or_else(|| "unknown".into());
        outln!("  {} · {} · {when}", s.runtime, s.recorded_cwd);
        if let Some(g) = gist {
            outln!("      {}", ui::dim(&format!("\"{g}\"")));
        }
    }

    // One confirmation, unless --yes. A non-interactive shell with no --yes is treated as "no": relocate
    // moves history into the store, so it never proceeds unattended without an explicit go-ahead.
    if !yes {
        if !ui::interactive() {
            errln!(
                "  refusing to relocate {} session(s) without confirmation; re-run with --yes (no terminal to prompt on).",
                stranded.len()
            );
            return Ok(1);
        }
        use std::io::{stdin, stdout, BufRead, Write};
        print!("bring these {} session(s) into {}? [Y/n] ", stranded.len(), env.display());
        let _ = stdout().flush();
        let mut line = String::new();
        stdin().lock().read_line(&mut line).ok();
        let ans = line.trim().to_ascii_lowercase();
        if !(ans.is_empty() || ans == "y" || ans == "yes") {
            outln!("aborted; nothing moved.");
            return Ok(0);
        }
    }

    let ag = agent::resolve(None)?;
    let mut moved = 0usize;
    let mut touched: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
    for s in &stranded {
        match relocate_one(s, &env, &ag) {
            Ok(()) => {
                moved += 1;
                touched.insert(s.runtime);
            }
            Err(e) => errln!("  ⚠ could not relocate {} (ran in {}): {e:#}", s.id, s.recorded_cwd),
        }
    }
    // Now capture the freshly-installed, now-owned sessions into the active agent's store.
    for rt in &touched {
        if let Err(e) = crate::session::capture_relocated(&env, rt) {
            errln!("  ⚠ {rt} capture after relocate failed: {e:#}");
        }
    }

    if moved == 0 {
        anyhow::bail!("relocate found sessions but moved none; see the warnings above.");
    }
    outln!("relocated {moved} session(s) into {}; `agit a log` to see them.", env.display());
    Ok(0)
}

/// One stranded session → installed under `env`'s slug (its transcript cwd rewritten to `env`, reusing
/// `convert_for_install`'s same-runtime cwd rewrite), with a launch record attributing it to `ag` so the
/// following capture routes it into `ag`'s store rather than the repo default.
fn relocate_one(s: &crate::adapter::StrandedSession, env: &Path, ag: &agent::Agent) -> Result<()> {
    let text = std::fs::read_to_string(&s.path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", s.path.display()))?;
    // Same-runtime (no cross-vendor convert): byte replay with the recorded cwd swapped to env.
    let (new_id, out, _ir, cwd) =
        convert_for_install(&text, &s.path, s.runtime, s.runtime, Some(env.display().to_string()))?;
    crate::register::install(s.runtime, &new_id, &cwd, &out)?;
    // Unsigned, best-effort: attribution just needs the store, and capture writes the authoritative
    // content-bound provenance into the sidecar (mirrors convert_pass).
    record_launch(&new_id, &ag.aid, &ag.name, env, s.runtime, None)?;
    Ok(())
}

/// A `<selector>` matches a stranded session by its id, its transcript path, or a substring of the
/// directory it ran in.
fn relocate_selector_matches(s: &crate::adapter::StrandedSession, sel: &str) -> bool {
    s.id == sel
        || s.path.to_string_lossy() == sel
        || s.path.file_stem().map(|st| st.to_string_lossy() == sel).unwrap_or(false)
        || s.recorded_cwd.contains(sel)
}

/// A one-line opening gist for the relocate listing, read cheaply from the transcript's first prompt.
fn relocate_gist(path: &Path, runtime: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let ir = match runtime {
        "codex" => crate::adapter::codex::parse_rollout(&content, "x"),
        _ => crate::adapter::claude_code::parse_jsonl(&content, "x"),
    };
    ir.prompts.into_iter().map(|p| ui::one_line(&p, 72)).find(|p| !p.is_empty())
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
                "no launch record, filed under default agent `{name}` (agit start --agent <name> to attribute)"
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
    /// Self-verify passed: the content is intact and the signature matches the RECORDED key. This says
    /// nothing about WHO that key belongs to — that is the registry's job (`VerifiedAs`/`KeyMismatch`).
    Verified { aid: String, email: String, pubkey: String },
    /// Registry-attributed: self-verify passed AND the provenance `pubkey` IS the ed25519 key the hub
    /// registry has for the account that owns the committer `email`. This is the only "verified as a
    /// person" verdict — "signed by alice's registered key".
    VerifiedAs { username: String, aid: String, email: String, pubkey: String },
    /// The security-critical case: self-verify passed, and the committer `email` DOES map to a registered
    /// account, but that account's registered ed25519 key DIFFERS from the provenance `pubkey`. The
    /// session was signed by a key that is NOT the claimed identity's registered key — a possible forgery
    /// / impersonation. Never a pass.
    KeyMismatch { email: String, claimed_username: String, registered_pubkey: String, actual_pubkey: String },
    /// Self-verify passed, but the committer `email` maps to NO registered account (or no hub was
    /// reachable). Falls back to today's self-verify meaning: the signature is internally consistent, but
    /// there is nothing to attribute it TO. Never "verified as a person".
    SignedUnregistered { aid: String, email: String, pubkey: String },
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
            ProvenanceStatus::VerifiedAs { username, email, .. } => {
                format!("VERIFIED AS {username} <{email}> · signed by {username}'s registered key")
            }
            ProvenanceStatus::KeyMismatch { claimed_username, .. } => {
                format!(
                    "KEY MISMATCH · signed by a key that is NOT {claimed_username}'s registered key (possible forgery)"
                )
            }
            ProvenanceStatus::SignedUnregistered { email, pubkey, .. } => {
                format!("signed · {email} maps to no registered account; self-verified only (key {})", short_key(pubkey))
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
    /// Whether the signature + content self-verify passed (regardless of registry attribution). Both
    /// `Verified` and the registry-classified `VerifiedAs`/`SignedUnregistered` imply a good self-verify;
    /// `KeyMismatch` does NOT (its signature is valid over its own key, but that key is not the claimed
    /// person's — so it is not a pass).
    pub fn is_verified(&self) -> bool {
        matches!(
            self,
            ProvenanceStatus::Verified { .. }
                | ProvenanceStatus::VerifiedAs { .. }
                | ProvenanceStatus::SignedUnregistered { .. }
        )
    }
    /// Whether the hub registry positively attributed this to a person: only `VerifiedAs`.
    pub fn is_attributed(&self) -> bool {
        matches!(self, ProvenanceStatus::VerifiedAs { .. })
    }
}

/// A registered identity as the hub publishes it: the account username and the SET of ed25519 signing
/// pubkeys (hex) it has registered — one per device key (SSH-keys style). Resolved from a committer email
/// via the registry's `by-email` lookup. A session's signing key attributes to this person when it EQUALS
/// ANY of these keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredIdentity {
    pub username: String,
    /// Every NON-REVOKED device key the account has registered. A revoked key is never included, so it can
    /// never count as a provenance match (revocation actually de-attributes its sessions).
    pub ed25519_keys: Vec<String>,
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

/// Verify a provenance record's signature ALONE, against its own recorded `content_digest` — without the
/// transcript on hand. This is the SERVER's check on a push: it proves the signed tuple is internally
/// consistent (the recorded pubkey really signed `(digest ‖ aid ‖ email ‖ started)`), which is all the
/// hub needs to then attribute the key to a person. Content-tamper (transcript vs recorded digest) stays
/// the client's read-time job — [`verify_provenance`] — since only the client holds the transcript.
pub fn verify_provenance_signature(p: &Provenance) -> bool {
    let msg = provenance_message(&p.content_digest, &p.aid, &p.email, &p.started);
    agent::verify_hex(&p.pubkey, &msg, &p.sig)
}

/// Compare two hex pubkeys for equality, case-insensitively (hub and client may differ in hex case). A
/// non-hex value never equals anything — it cannot be a real key.
fn same_pubkey(a: &str, b: &str) -> bool {
    !a.is_empty() && a.eq_ignore_ascii_case(b)
}

/// The pure attribution step, shared by the client and the hub: given a self-verify status and the
/// registry's answer for the committer email, upgrade a `Verified` into `VerifiedAs` / `KeyMismatch` /
/// `SignedUnregistered`. Everything non-`Verified` (Unsigned/Tampered/BadSignature) passes through
/// unchanged. `registered` is the account the email maps to, or `None` when it maps to nothing (or the
/// caller could not reach the registry — offline degrades to `SignedUnregistered`, never a false
/// "verified as").
///
/// **Match ANY device key.** The session attributes to the person when its signing key EQUALS ANY of the
/// account's registered keys — so a session signed on a second enrolled machine is still `VerifiedAs`, not
/// a false `KeyMismatch`. It is `KeyMismatch` only when the email maps to a registered account but the
/// signing key matches NONE of its keys (a possible forgery). A revoked key is never in the set, so it
/// never counts as a match.
///
/// `trusted_keys`, when set, is the set to compare against instead of `registered.ed25519_keys` — the
/// CLIENT passes its TOFU-pinned copy so a hub key-substitution cannot manufacture a false match; the hub
/// passes `None` (it IS the registry).
pub fn attribute_with_registry(
    self_status: ProvenanceStatus,
    registered: Option<RegisteredIdentity>,
    trusted_keys: Option<&[String]>,
) -> ProvenanceStatus {
    let ProvenanceStatus::Verified { aid, email, pubkey } = self_status else {
        return self_status;
    };
    let Some(reg) = registered else {
        return ProvenanceStatus::SignedUnregistered { aid, email, pubkey };
    };
    let candidates: &[String] = trusted_keys.unwrap_or(&reg.ed25519_keys);
    if candidates.iter().any(|k| same_pubkey(&pubkey, k)) {
        ProvenanceStatus::VerifiedAs { username: reg.username, aid, email, pubkey }
    } else {
        ProvenanceStatus::KeyMismatch {
            email,
            claimed_username: reg.username,
            // A representative registered key for the message (the account may have several); the point is
            // simply that NONE of them is the signing key.
            registered_pubkey: candidates.first().cloned().unwrap_or_default(),
            actual_pubkey: pubkey,
        }
    }
}

/// Self-verify a session's provenance and then attribute it against the identity registry — the full
/// "verified as person X" path. `lookup` resolves a committer email to the registered account owning it
/// (`Ok(Some)` = registered, `Ok(None)` = no account, `Err` = no hub / unreachable — both non-hits
/// degrade to `SignedUnregistered`, so an offline verify is never a false attribution).
///
/// TOFU: the registered ed25519 key the hub hands back is pinned on first sighting; a CHANGED registered
/// key is a HARD failure (an `Err` from this function) unless `repin` — matching the encryption
/// recipient-pinning decision, so a hub cannot silently swap the key it attributes a session to. The
/// comparison then uses the pinned copy, not the freshly-fetched one.
pub fn verify_provenance_with_registry<F>(
    home: &Path,
    content: &str,
    p: Option<&Provenance>,
    repin: bool,
    lookup: F,
) -> Result<ProvenanceStatus>
where
    F: FnOnce(&str) -> Result<Option<RegisteredIdentity>>,
{
    let self_status = verify_provenance(content, p);
    let ProvenanceStatus::Verified { email, .. } = &self_status else {
        // Unsigned / tampered / bad-signature: nothing to attribute.
        return Ok(self_status);
    };
    // Non-hits (unknown email, or no hub reachable) degrade to self-verify only — never a false pass.
    let registered = match lookup(email) {
        Ok(Some(reg)) => reg,
        Ok(None) | Err(_) => {
            return Ok(attribute_with_registry(self_status, None, None));
        }
    };
    // TOFU-pin the account's registered key SET; a changed set HARD-FAILS (Err) unless re-pinned, so a hub
    // cannot silently swap (or inject) a key it attributes sessions to. The comparison then uses the
    // pinned/confirmed set, not a freshly-fetched one that could differ.
    let trusted = pin_registered_key_set(home, &registered, repin)?;
    Ok(attribute_with_registry(self_status, Some(registered), Some(&trusted)))
}

/// TOFU-pin an account's registered ed25519 signing-key SET, returning the trusted (pinned) set. The pin
/// anchor is a fingerprint over the sorted set — reusing the existing single-key provenance-pin file as a
/// set anchor — so ANY change to the set (a key added OR removed OR swapped) is a hard error carrying a
/// re-pin instruction, unless `repin`. First sighting pins it. On success the fetched set matched the
/// pinned fingerprint, so the returned set is the trusted one to attribute against.
fn pin_registered_key_set(home: &Path, reg: &RegisteredIdentity, repin: bool) -> Result<Vec<String>> {
    let mut keys = reg.ed25519_keys.clone();
    keys.sort();
    keys.dedup();
    // Fingerprint the sorted set as a 32-byte value and TOFU-pin THAT via the existing prov-key pin.
    let fp_hex = crate::convo::sha256_hex(&keys.join("\n"));
    let fp = crate::keybox::decode_pub32_hex(&fp_hex)
        .context("internal: sha256 of the registered key set is not 32 bytes")?;
    crate::keybox::pin_provenance_key(home, &reg.username, &fp, repin)?;
    Ok(keys)
}

/// The committer email the store commits under, read never written (agit must not touch git identity).
/// This is the user's git-resolved `user.email` (local → global), exactly like any git repo, and it is
/// the provenance lookup handle that bridges a signature to a hub account. Empty when git has nothing
/// configured: the caller (the snap/merge gate) refuses to attribute a session under an unset identity
/// rather than binding it to a synthetic default.
pub fn committer_email(store: &Path) -> String {
    scope::git_in(store, &["config", "user.email"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

/// True when the store has no committer identity to attribute a session to — git's `user.email` resolves
/// to nothing (local → global). The snap/merge gate and `agit a commit` both refuse in this case, so a
/// session capture never lands in history unattributed. agit's own bookkeeping commits are exempt: they
/// pass an explicit `-c user.email=agit@local`, so they never read this.
pub fn committer_identity_unset(store: &Path) -> bool {
    committer_email(store).is_empty()
}

/// The git-style refusal shown when the committer identity is unset, shared by the snap/merge gate and
/// `agit a commit` so every session-writing path guides the user the same way.
pub fn warn_committer_identity_unset() {
    errln!(
        "{}",
        ui::warn(
            "agit: your committer identity is unset, so a session can't be attributed.\n  \
             set it like git:  git config --global user.email you@example.com\n  \
             \x20                git config --global user.name  \"Your Name\"\n  \
             (or per agent:    agit a config user.email you@example.com)\n\
             this email is your provenance identity; register your device key with\n\
             \"agit identity register <you>\" so \"agit a provenance\" can verify it."
        )
    );
}

/// Read the `provenance` block from a session's sidecar, if it has one. Absent sidecar, unparsable JSON,
/// or a sidecar with no provenance all yield `None` — the caller reports "unsigned", never fails.
pub fn sidecar_provenance(transcript: &Path) -> Option<Provenance> {
    let text = std::fs::read_to_string(sidecar_path(transcript)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    serde_json::from_value(v.get("provenance")?.clone()).ok()
}

/// `agit provenance [verify [<session|agent>] | key]` — self-verify a captured session's signature, or
/// show this machine's public key. `verify` with no argument checks the active agent's latest session,
/// with an agent name checks every session that agent has, and with a path/id checks that one session.
/// Verification never blocks: an unsigned or tampered session reports and returns 0 for `key`/no-arg,
/// and a non-zero code only when a `verify` finds a session NOT verified.
pub fn provenance_cmd(args: &[String]) -> Result<i32> {
    match args.first().map(|s| s.as_str()) {
        Some("verify") => provenance_verify(&args[1..]),
        Some("key") | Some("show") | None => provenance_key(),
        Some(other) => {
            errln!("agit provenance: unknown subcommand `{other}`");
            errln!("  usage: agit provenance verify [<session|agent>] [--repin]   ·   agit provenance key");
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

// ─────────────────────── Identity registry: enroll/show against the hub (encryption-recipients Wave 1) ───────────────────────

/// `agit identity register <you>` / `agit identity show [<user>]` — the client half of the shared
/// identity registry. The registry publishes this machine's ed25519 + X25519 PUBLIC keys so teammates
/// can verify provenance signatures and (later) wrap encryption content-keys to them. The private key
/// never leaves the machine. `register` is OFFLINE: it PRINTS a signed enroll block to paste into the
/// hub web UI (where you are already logged in), rather than POSTing it — no token, no network call.
pub fn identity_cmd(args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str());
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    match sub {
        Some("register") => identity_register(rest),
        Some("keys") => identity_keys(),
        Some("revoke") => identity_revoke(rest),
        Some("show") => identity_show(rest.first().map(|s| s.trim()).filter(|s| !s.is_empty())),
        Some("pin") => identity_pin(rest),
        // No subcommand = show my own local identity + enrollment status, the friendliest default.
        None => identity_show(None),
        Some(other) => {
            errln!("agit identity: unknown subcommand `{other}`");
            errln!(
                "  usage: agit identity register <you> [--label <name>]   ·   keys   ·   \
                 revoke <fpr-or-label>   ·   show [<user>]   ·   pin <user> [--repin] [--key HEX]"
            );
            Ok(2)
        }
    }
}

/// A default device label for a freshly enrolled key: this machine's hostname, so `agit identity keys`
/// reads like GitHub's SSH-key list ("work-laptop", "ci-runner"). Falls back to a generic name when the
/// hostname is not discoverable — a label is a cosmetic hint, never load-bearing.
fn default_device_label() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "this-device".to_string())
}

/// This machine's registered device key within a fetched identity set (matched by ed25519 pubkey), if the
/// account has already enrolled it. Returns the matching entry from the response's `keys` array.
fn my_device_key<'a>(set: &'a serde_json::Value, my_ed_pub: &str) -> Option<&'a serde_json::Value> {
    set.get("keys")?.as_array()?.iter().find(|k| field(k, "ed25519_pub") == my_ed_pub)
}

/// The pubkeys of THIS machine's identity, deriving the X25519 half from the same ed25519 secret.
fn local_identity_pubkeys() -> Result<(String, String)> {
    Ok((agent::machine_pubkey_hex()?, agent::machine_x25519_pubkey_hex()?))
}

fn field<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

/// A client-side monotonic epoch: nanoseconds since the Unix epoch, as an i64. Every fresh invocation
/// reads a strictly greater value (wall-clock nanos always advance between two process runs), so a
/// re-printed block always out-ranks the previous one and the hub's per-key monotonic-epoch check accepts
/// the updated paste. Falls back to a small nonzero floor on the (impossible in practice) pre-1970 clock.
fn register_epoch() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(1)
}

/// `agit identity register <you> [--label <name>]` — OFFLINE. Build and PRINT a paste-able enroll block
/// for the hub account `<you>`, for the user to paste into the hub web UI (Account -> Signing keys -> Add
/// a signing key), where their logged-in session authenticates the add. NO network call, NO token.
///
/// The block is `{ ed25519_pub, x25519_pub, epoch, enroll_sig, label }`. `enroll_sig` signs
/// `(username ‖ epoch ‖ ed25519_pub ‖ x25519_pub)` with THIS machine's key — the SAME bytes the hub's
/// `POST /api/identity/enroll` re-derives and verifies against the submitted `ed25519_pub`. The username
/// is REQUIRED and is baked into the signed bytes: the hub verifies the signature over the SESSION
/// username, so a block signed for one account cannot be pasted under another — that binding is the point.
/// `epoch` is a client-side monotonic value (nanos), so a re-print always advances and the hub accepts the
/// updated re-paste. `label` defaults to the machine hostname; the block carries ONLY public key material
/// plus the signature — never any private key.
fn identity_register(args: &[String]) -> Result<i32> {
    let Some(username) = register_username(args) else {
        bail!(
            "agit identity register: name the hub account this key is for\n  \
             usage: agit identity register <you> [--label <name>]"
        );
    };
    let sk = agent::machine_signing_key()?;
    let (ed_pub, x_pub) = local_identity_pubkeys()?;
    let epoch = register_epoch();
    let device_label = flag_value(args, "--label")
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(default_device_label);

    let msg = agent::identity_enroll_message(&username, epoch, &ed_pub, &x_pub);
    let enroll_sig = agent::sign_hex(&sk, &msg);
    let block = serde_json::json!({
        "ed25519_pub": ed_pub,
        "x25519_pub": x_pub,
        "epoch": epoch,
        "enroll_sig": enroll_sig,
        "label": device_label,
    });
    // Compact one-line JSON: trivial to copy in a single line. serde_json::to_string is already compact.
    outln!("{}", serde_json::to_string(&block)?);
    outln!("");
    outln!("paste this into the hub: Account -> Signing keys -> Add a signing key");
    outln!("  (signed for {username:?} on device {device_label:?}; contains only public keys; no secret leaves this machine)");
    Ok(0)
}

/// The `<you>` positional for `register`, skipping the `--label <value>` pair so the username is found
/// whichever side of the flag it lands on.
fn register_username(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--label" {
            it.next(); // consume the flag's value
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        let t = a.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

/// `agit identity keys` — list THIS account's enrolled device keys (fingerprint, label, when), marking the
/// one that lives on this machine. The read half of the SSH-keys UX.
fn identity_keys() -> Result<i32> {
    let (my_ed_pub, _) = local_identity_pubkeys()?;
    let ep = crate::hubapi::HubEndpoint::resolve()?;
    let username = ep.me()?;
    let Some(set) = ep.get_identity(&username)? else {
        outln!("no device keys enrolled for {username} yet; run `agit identity register {username}` and paste the block into the hub.");
        return Ok(0);
    };
    let keys = set.get("keys").and_then(|k| k.as_array()).cloned().unwrap_or_default();
    if keys.is_empty() {
        outln!("no device keys enrolled for {username} yet; run `agit identity register {username}` and paste the block into the hub.");
        return Ok(0);
    }
    outln!("device keys for {username}");
    for k in &keys {
        let fpr = field(k, "key_fpr");
        let label = field(k, "label");
        let created = field(k, "created");
        let mine = if field(k, "ed25519_pub") == my_ed_pub { "  (this device)" } else { "" };
        let label_show = if label.is_empty() { "-".to_string() } else { label.to_string() };
        outln!("  {fpr}  {label_show}  added {created}{mine}");
    }
    outln!("  revoke one with `agit identity revoke <fpr-or-label>`.");
    Ok(0)
}

/// `agit identity revoke <fpr-or-label>` — revoke ONE of MY enrolled device keys, named by its
/// fingerprint (full or a unique prefix) or its label. Caller-only at the hub; a non-owner cannot revoke.
fn identity_revoke(args: &[String]) -> Result<i32> {
    let Some(sel) = first_positional(args).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        bail!("agit identity revoke: name a key by fingerprint or label\n  usage: agit identity revoke <fpr-or-label>");
    };
    let ep = crate::hubapi::HubEndpoint::resolve()?;
    let username = ep.me()?;
    let set = ep.get_identity(&username)?;
    let keys = set.as_ref().and_then(|v| v.get("keys")).and_then(|k| k.as_array()).cloned().unwrap_or_default();
    // Resolve the selector to exactly one enrolled key: an exact/prefix fingerprint match, or a label match.
    let matches: Vec<&serde_json::Value> = keys
        .iter()
        .filter(|k| {
            let fpr = field(k, "key_fpr");
            fpr == sel || fpr.starts_with(&sel) || field(k, "label") == sel
        })
        .collect();
    let fpr = match matches.as_slice() {
        [] => {
            errln!("no enrolled device key matches {sel:?}. List them with `agit identity keys`.");
            return Ok(1);
        }
        [only] => field(only, "key_fpr").to_string(),
        many => {
            errln!("{sel:?} matches {} device keys; use a full fingerprint from `agit identity keys`.", many.len());
            return Ok(1);
        }
    };
    ep.revoke_identity_key(&fpr)?;
    outln!("revoked device key {fpr} for {username}.");
    outln!("  sessions signed by that key no longer verify as you; its encryption access is cut on the next rewrap.");
    Ok(0)
}

/// `agit identity pin <user> [--repin] [--key HEX]` — TOFU-pin a recipient's X25519 pubkey for wrapping.
/// With `--key`, pin that exact hex key (offline); otherwise fetch it from the hub registry. A CHANGED
/// key is a HARD failure unless `--repin` is given after an out-of-band fingerprint check.
fn identity_pin(args: &[String]) -> Result<i32> {
    let home = scope::agit_home()?;
    let repin = args.iter().any(|a| a == "--repin");
    let key = flag_value(args, "--key");
    let Some(user) = first_positional(args) else {
        bail!("agit identity pin: name a <user>\n  usage: agit identity pin <user> [--repin] [--key <hex-x25519-pub>]");
    };
    let k = crate::keybox::pin_user(&home, &user, key.as_deref(), repin)?;
    outln!("pinned {user}");
    outln!("  x25519  {}", hex::encode(k));
    Ok(0)
}

/// Show an identity: with no `<user>`, this machine's local keys plus its hub enrollment status (a
/// best-effort lookup — a machine with no hub still prints its local identity); with `<user>`, that
/// person's published keys fetched from the hub.
fn identity_show(user: Option<&str>) -> Result<i32> {
    match user {
        None => {
            let (ed_pub, x_pub) = local_identity_pubkeys()?;
            outln!("this machine's identity");
            outln!("  ed25519  {ed_pub}");
            outln!("  x25519   {x_pub}");
            outln!("  stored   {}", scope::agit_home()?.join("identity").join("ed25519").display());
            // Enrollment status is a courtesy: never fail `show` just because no hub is reachable.
            match hub_enrollment_status(&ed_pub) {
                Ok(Some(line)) => outln!("  {line}"),
                Ok(None) => outln!("  not enrolled on the hub yet; run `agit identity register <you>` and paste the block into the hub web UI."),
                Err(_) => outln!("  (no hub configured; showing local identity only)"),
            }
            Ok(0)
        }
        Some(u) => {
            let ep = crate::hubapi::HubEndpoint::resolve()?;
            match ep.get_identity(u)? {
                Some(v) => {
                    let keys = v.get("keys").and_then(|k| k.as_array()).cloned().unwrap_or_default();
                    outln!("{u}; {} device key(s)", keys.len());
                    for k in &keys {
                        outln!("  {}  {}", field(k, "key_fpr"), {
                            let l = field(k, "label");
                            if l.is_empty() { "-".to_string() } else { l.to_string() }
                        });
                        outln!("    ed25519  {}", field(k, "ed25519_pub"));
                        outln!("    x25519   {}", field(k, "x25519_pub"));
                        outln!("    epoch    {}", k.get("epoch").and_then(|e| e.as_i64()).unwrap_or(0));
                    }
                    if keys.is_empty() {
                        errln!("no identity enrolled for {u} on the hub.");
                        return Ok(1);
                    }
                    Ok(0)
                }
                None => {
                    errln!("no identity enrolled for {u} on the hub.");
                    Ok(1)
                }
            }
        }
    }
}

/// The one-line hub enrollment status for a local ed25519 pubkey: `Some(line)` when THIS machine's key is
/// enrolled in the account's device-key set, `None` when it is not (or the account has no keys), `Err`
/// when no hub is reachable/configured.
fn hub_enrollment_status(local_ed_pub: &str) -> Result<Option<String>> {
    let ep = crate::hubapi::HubEndpoint::resolve()?;
    let username = ep.me()?;
    let Some(set) = ep.get_identity(&username)? else {
        return Ok(None);
    };
    let count = set.get("keys").and_then(|k| k.as_array()).map(|a| a.len()).unwrap_or(0);
    match my_device_key(&set, local_ed_pub) {
        Some(k) => {
            let epoch = k.get("epoch").and_then(|e| e.as_i64()).unwrap_or(0);
            Ok(Some(format!("enrolled as {username} at epoch {epoch} ({count} device key(s); this machine is one)")))
        }
        None if count > 0 => Ok(Some(format!(
            "enrolled as {username} with {count} device key(s), but NOT this machine; run `agit identity register {username}` and paste the block into the hub"
        ))),
        None => Ok(None),
    }
}

/// `agit provenance verify [<sel>] [--repin]` — self-verify a captured session's signature. `<sel>`
/// follows the unified resolution (see [`classify_session_selector`]): a file path or session id
/// verifies THAT session (unchanged); NO argument verifies the ACTIVE agent's LATEST session; a known
/// AGENT NAME verifies EVERY session that agent has (whole-agent mode — non-zero if any is unverified).
fn provenance_verify(args: &[String]) -> Result<i32> {
    let repin = args.iter().any(|a| a == "--repin");
    let sel = first_positional(args).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    match classify_session_selector(sel.as_deref())? {
        SessionTarget::One(transcript) => provenance_verify_one(&transcript, repin),
        SessionTarget::ActiveLatest(agent) => {
            provenance_verify_one(&agent_latest_transcript(&agent)?, repin)
        }
        SessionTarget::Agent(agent) => provenance_verify_agent(&agent, repin),
    }
}

/// Verify one transcript's provenance against the hub registry — no printing, just the status.
///
/// Consults the hub identity registry to upgrade a good self-verify into "verified as <person>". No hub
/// reachable degrades to `SignedUnregistered` (self-verify only), never a false attribution. A TOFU pin
/// change on the registered key is a hard error (the `?`), matching the encryption decision.
fn verify_transcript(transcript: &Path, repin: bool) -> Result<ProvenanceStatus> {
    let content = std::fs::read_to_string(transcript)
        .with_context(|| format!("cannot read session {}", transcript.display()))?;
    let prov = sidecar_provenance(transcript);
    let home = scope::agit_home()?;
    let endpoint = crate::hubapi::HubEndpoint::resolve();
    verify_provenance_with_registry(&home, &content, prov.as_ref(), repin, |email| match &endpoint {
        Ok(ep) => ep.get_identity_by_email(email).map(|opt| opt.map(registered_from_json)),
        // No hub configured/reachable: fall back to self-verify (SignedUnregistered), never block.
        Err(_) => Ok(None),
    })
}

/// The exit code for a verification status. A soft "unverified" (unsigned/unregistered) exits 0, like
/// the attribution fallback; a signature that is present but does NOT check out, and a KEY MISMATCH (a
/// possible forgery), are hard failures worth a non-zero code.
fn provenance_exit_code(status: &ProvenanceStatus) -> i32 {
    match status {
        ProvenanceStatus::Verified { .. }
        | ProvenanceStatus::VerifiedAs { .. }
        | ProvenanceStatus::SignedUnregistered { .. }
        | ProvenanceStatus::Unsigned => 0,
        ProvenanceStatus::ContentTampered { .. }
        | ProvenanceStatus::BadSignature
        | ProvenanceStatus::KeyMismatch { .. } => 1,
    }
}

/// Verify a single session and print its full report. Returns the exit code.
fn provenance_verify_one(transcript: &Path, repin: bool) -> Result<i32> {
    let status = verify_transcript(transcript, repin)?;
    outln!("session {}", transcript.display());
    outln!("  {}", status.summary());
    match &status {
        ProvenanceStatus::VerifiedAs { username, email, pubkey, .. } => {
            outln!("  identity  {username} <{email}>");
            outln!("  key       {pubkey}");
        }
        ProvenanceStatus::KeyMismatch { email, claimed_username, registered_pubkey, actual_pubkey } => {
            errln!("  committer      {email} (registered to {claimed_username})");
            errln!("  registered key {registered_pubkey}");
            errln!("  signing key    {actual_pubkey}");
            errln!("  this session was signed by a key that is NOT {claimed_username}'s registered key.");
        }
        ProvenanceStatus::SignedUnregistered { email, pubkey, .. } => {
            outln!("  committer {email}");
            outln!("  key       {pubkey}");
        }
        ProvenanceStatus::Verified { email, .. } => outln!("  committer {email}"),
        ProvenanceStatus::ContentTampered { recorded, actual } => {
            outln!("  signed digest  {recorded}");
            outln!("  current digest {actual}");
        }
        ProvenanceStatus::Unsigned | ProvenanceStatus::BadSignature => {}
    }
    Ok(provenance_exit_code(&status))
}

/// Whole-agent mode: verify EVERY session an agent has, one verdict line each, newest first. Returns a
/// non-zero code if ANY session did not verify — so `agit provenance verify <agent>` is a single gate
/// over that agent's whole recorded history.
fn provenance_verify_agent(agent: &agent::Agent, repin: bool) -> Result<i32> {
    let mut sessions = store_sessions(&agent.store);
    if sessions.is_empty() {
        anyhow::bail!(
            "agent `{}` has no sessions yet; nothing to verify (run one, or `agit a log` to list)",
            agent.name
        );
    }
    // Newest first, matching the session views (recency is recorded and committed, not mtimed — so this
    // order is the SAME for every teammate; see `recency_order_key`).
    sessions.sort_by_key(|s| std::cmp::Reverse(recency_order_key(s)));

    outln!("verifying {} session(s) for agent `{}`", sessions.len(), agent.name);
    let mut worst = 0;
    for s in &sessions {
        let status = verify_transcript(&s.path, repin)?;
        let code = provenance_exit_code(&status);
        worst = worst.max(code);
        let id = s.path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mark = if code == 0 { "ok  " } else { "FAIL" };
        outln!("  {mark}  {id}  {}", status.summary());
    }
    if worst != 0 {
        errln!("\n  at least one session did NOT verify.");
    }
    Ok(worst)
}

/// Extract a [`RegisteredIdentity`] from a hub `by-email` / identity JSON response. The response carries
/// the account plus its `keys` array (the device-key set); we collect every `ed25519_pub`. Missing keys
/// collapse to an empty set — which compares equal to nothing, so it classifies as a mismatch rather than
/// a false pass. `username` is read from the top level, falling back to the first key's username.
fn registered_from_json(v: serde_json::Value) -> RegisteredIdentity {
    let keys = v
        .get("keys")
        .and_then(|k| k.as_array())
        .map(|arr| arr.iter().filter_map(|k| k.get("ed25519_pub").and_then(|e| e.as_str()).map(String::from)).collect::<Vec<_>>())
        .unwrap_or_default();
    let username = {
        let top = field(&v, "username");
        if !top.is_empty() {
            top.to_string()
        } else {
            v.get("keys")
                .and_then(|k| k.as_array())
                .and_then(|arr| arr.first())
                .map(|k| field(k, "username").to_string())
                .unwrap_or_default()
        }
    };
    RegisteredIdentity { username, ed25519_keys: keys }
}

/// The sidecar-to-transcript mapping: when `p` is an `<id>.agit.json` sidecar sitting next to its
/// `<id>.jsonl`, point back at the transcript; otherwise `p` is itself the transcript.
fn transcript_from_path(p: &Path) -> PathBuf {
    if p.extension().map(|e| e == "json").unwrap_or(false)
        && p.file_stem().map(|s| Path::new(s).extension().map(|e| e == "agit").unwrap_or(false)).unwrap_or(false)
    {
        let jsonl = p.with_file_name(format!(
            "{}.jsonl",
            p.file_stem().and_then(|s| Path::new(s).file_stem()).unwrap_or_default().to_string_lossy()
        ));
        if jsonl.is_file() {
            return jsonl;
        }
    }
    p.to_path_buf()
}

/// A bare session id looked up in a store: the transcript whose file stem is exactly `id`.
fn session_id_in_store(store: &Path, id: &str) -> Option<PathBuf> {
    store_sessions(store)
        .into_iter()
        .find(|s| s.path.file_stem().map(|n| n == id).unwrap_or(false))
        .map(|s| s.path)
}

/// What a `<session>` selector points at, before it becomes a concrete transcript. The three commands
/// that take one (`convert`, `resume`, `provenance verify`) share this classification, so a file path,
/// a session id, an agent NAME, and an absent selector all mean the same thing everywhere; they differ
/// only in what they do with a whole agent (its latest session, or verify-every-session).
enum SessionTarget {
    /// A concrete transcript: a file path, a sidecar path, or a session id in the resolved agent's store.
    One(PathBuf),
    /// A known agent named on the command line. convert/resume take its latest session; provenance
    /// verify checks every session it has.
    Agent(Box<agent::Agent>),
    /// No selector at all: the active agent.
    ActiveLatest(Box<agent::Agent>),
}

/// Classify an optional `<session>` selector — the ONE shared resolver. Order, most specific first:
/// 1. a file path that exists on disk -> that transcript (sidecar `<id>.agit.json` -> `<id>.jsonl`);
/// 2. a session id present in the resolved (active / `--agent`) agent's store -> that session;
/// 3. a known agent NAME on this machine (the same name lookup `agit a switch` uses) -> that agent;
/// 4. absent -> the active agent.
///
/// Rungs 1 and 2 are exactly today's behavior for an explicit path/id. Anything that matches none of
/// the four is a clear error naming what was tried.
fn classify_session_selector(sel: Option<&str>) -> Result<SessionTarget> {
    let Some(sel) = sel.map(str::trim).filter(|s| !s.is_empty()) else {
        // 4. no selector -> the active agent.
        return Ok(SessionTarget::ActiveLatest(Box::new(agent::resolve(None)?)));
    };
    // 1. a filesystem path (transcript or its sidecar).
    let p = Path::new(sel);
    if p.is_file() {
        return Ok(SessionTarget::One(transcript_from_path(p)));
    }
    // 2. a session id in the ACTIVE agent's store — only if there is an active agent. A missing active
    //    agent must NOT abort: naming a known agent (rung 3) should resolve on its own.
    let active = agent::resolve(None).ok();
    if let Some(a) = &active {
        if let Some(path) = session_id_in_store(&a.store, sel) {
            return Ok(SessionTarget::One(path));
        }
    }
    // 3. a known agent name.
    if let Ok(named) = agent::info(sel) {
        return Ok(SessionTarget::Agent(Box::new(named)));
    }
    // Nothing matched — say what was tried.
    match &active {
        Some(a) => anyhow::bail!(
            "no session or agent `{sel}`; not a transcript path, not a session id in the active agent `{}`'s store, and not a known agent name.\n  run a session, or `agit a log` to list what this agent has.",
            a.name
        ),
        None => anyhow::bail!(
            "no session or agent `{sel}`; not a transcript path and not a known agent name (and no active agent to look up a session id in).\n  `agit a list` shows this machine's agents."
        ),
    }
}

/// The latest session in an agent's store, or a clear error when it has none yet.
fn agent_latest_transcript(agent: &agent::Agent) -> Result<PathBuf> {
    latest_session(&agent.store).map(|s| s.path).with_context(|| {
        format!("agent `{}` has no sessions yet; run one, or `agit a log` to list", agent.name)
    })
}

/// Resolve an optional `<session>` selector to a single transcript, used by `convert` and `resume`.
/// A file path or session id -> that transcript; an agent NAME -> that agent's latest session; absent
/// -> the active agent's latest session. See [`classify_session_selector`] for the full order.
fn resolve_session_selector(sel: Option<&str>) -> Result<PathBuf> {
    match classify_session_selector(sel)? {
        SessionTarget::One(p) => Ok(p),
        SessionTarget::Agent(a) | SessionTarget::ActiveLatest(a) => agent_latest_transcript(&a),
    }
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
    store_sessions(store).into_iter().max_by_key(recency_order_key)
}

/// The order two teammates must agree on, from values git COMMITS — never the filesystem mtime, which a
/// clone flattens and a `touch` reorders, so an mtime tiebreak let two peers with a byte-identical store
/// resolve a different "latest". Recorded recency first (`last_activity`, now never null for a snapped
/// session), then the session's committed location (`<env>/<runtime>/<id>`) to break a tie deterministically.
fn recency_order_key(s: &StoredSession) -> (Option<chrono::DateTime<chrono::Utc>>, String) {
    let stem = s.path.file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
    (s.last_activity, format!("{}/{}/{stem}", s.env_slug.as_deref().unwrap_or(""), s.runtime))
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

/// The name of the tracked upstream ref, e.g. `origin/main` — the ref a dialogue merge should reconcile
/// against when the local and remote sessions have diverged. Returns `None` when there is no upstream.
///
/// This exists because the divergence hint must suggest a REF (`agit a merge origin/main`), never the
/// agent's own NAME: `agit a merge <self-name>` resolves to this same agent and dead-ends with "no local
/// session to represent it". The upstream remote-tracking ref is the diverged side, so merging it is the
/// command that actually reconciles.
pub fn upstream_ref(store: &Path) -> Option<String> {
    let (code, out) =
        scope::git_in_status(store, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]);
    let name = out.trim();
    (code == 0 && !name.is_empty()).then(|| name.to_string())
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
        outln!("  up to date; no new sessions on the remote.");
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
        None => "no upstream yet; agit a push to publish".to_string(),
        Some((0, 0)) => "up to date with the remote".to_string(),
        Some((ahead, 0)) => format!("{ahead} unpushed; agit a push to publish"),
        Some((0, behind)) => format!("{behind} behind; agit a pull to integrate"),
        Some((ahead, behind)) => {
            format!("{ahead} ahead, {behind} behind (diverged); agit a merge to reconcile")
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
                outln!("no agents bound to this repo; agit a init <name> mints one.");
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
                        .unwrap_or_else(|| "-".into());
                    (status, sessions.len().to_string(), last)
                }
                None => (ui::dim("not cloned").to_string(), "-".into(), "-".into()),
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
    // Disclose sessions dropped for running in the wrong directory (a parent dir, or the same repo at
    // another path) before launching — otherwise stranded work is invisible here. Points at `agit relocate`.
    crate::session::warn_stranded(&env);
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
        errln!("  ⚠ launch record not written ({e:#}); capture will attribute this session by repo default.");
    }
    exec(&h.resume_cmd)
}

/// No sessions yet: start FRESH but bound to the agent, and say so.
fn start_fresh(ag: &agent::Agent, env: &Path, as_rt: Option<&str>) -> Result<i32> {
    let rt = crate::session::resolve_runtime(as_rt, &[], "start").map_err(|e| {
        anyhow::anyhow!("{e}\n  `{}` has no sessions yet, so there is no runtime to continue in; name one: agit start --as claude-code|codex", ag.name)
    })?;
    let cli = if rt == "codex" { "codex" } else { "claude" };
    outln!("┌ {} · {} · {rt}", ui::bold(&ag.name), ui::accent(&ui::tilde(env)));
    outln!("└ no sessions yet; starting FRESH, bound to this agent.");
    // The runtime mints the id, so there is nothing to write a launch record against yet: this session
    // will be attributed to the repo's default agent when captured. Said, never assumed.
    errln!(
        "  note: a fresh session gets its id from {cli}, so it has no launch record; capture files it \
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

    // Per-session keybox encryption: `--team`, `--readers a,b`, and/or `--public`. Distinct from the
    // machine-global key path (`agit a encrypt` with no flags) — this mints a per-session content key into
    // a repo-local keyring and commits a keybox wrapping it to the named recipients.
    let team = args.iter().any(|a| a == "--team");
    let public = args.iter().any(|a| a == "--public");
    let readers: Vec<String> = match args.iter().position(|a| a == "--readers") {
        Some(i) => args
            .get(i + 1)
            .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    };
    if args.iter().any(|a| a == "--readers") && readers.is_empty() && !public && !team {
        bail!("agit a encrypt --readers needs a comma-separated list of users (or add --public / --team)");
    }
    if team {
        // Wrap the session CK under the owning org's CURRENT Team KEK and write a team stanza. `--team`
        // may be combined with `--readers`/`--public` (all wrap the same kid-0 CK).
        return crypt_enable_keybox_team(&home, flag_value(args, "--org").as_deref(), &readers, public, yes);
    }
    if public || !readers.is_empty() {
        return crypt_enable_keybox(&home, &readers, public, yes);
    }

    // Zero-config (Wave 4): no --team/--readers/--public. The DEFAULT reader set now depends on the
    // session's owning scope — team-readable (not public) under an org, explicit for a personal owner.
    crypt_enable_zero_config(&home, yes)
}

/// The reader set a no-flag `agit a encrypt` defaults to, decided from the session's owning scope. Pure
/// (no I/O) so the zero-config POLICY is unit-testable: the network probes that produce its inputs live
/// in [`crypt_enable_zero_config`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum DefaultTarget {
    /// No hub remote at all: the machine-global no-hub encryption path (unchanged Wave-1/2 behavior).
    MachineGlobal,
    /// The owning scope is an ORG the hub recognizes: wrap the CK under its Team KEK (a team stanza), so
    /// the zero-config result is "readable to the team, not the public". Carries the org name.
    Team(String),
    /// A personal owner (not an org): there is no team to default to, so the user must name readers.
    /// Carries the owner for the message.
    RequireExplicit(String),
}

/// Decide the zero-config default reader set. `owner` is `None` when the store has no hub remote; `is_org`
/// is whether the hub recognizes that owner as an org the caller can see. Team when org, explicit when
/// personal, machine-global when there is no hub at all.
fn default_target(owner: Option<&str>, is_org: bool) -> DefaultTarget {
    match owner {
        None => DefaultTarget::MachineGlobal,
        Some(o) if is_org => DefaultTarget::Team(o.to_string()),
        Some(o) => DefaultTarget::RequireExplicit(o.to_string()),
    }
}

/// `agit a encrypt` with NO --team/--readers/--public — the zero-config team default. Resolves the
/// session's owning scope, then dispatches per [`default_target`]: a team stanza under the owning org
/// (erroring actionably if the org has no Team KEK yet), an explicit-readers demand for a personal owner,
/// or the machine-global path when there is no hub remote.
fn crypt_enable_zero_config(home: &Path, yes: bool) -> Result<i32> {
    let a = agent::resolve(None)?;
    let store = a.store.clone();
    // No hub remote → the machine-global no-hub encryption path (there is no org to default to).
    let owner = match owning_owner_of(&store) {
        Ok(o) => o,
        Err(_) => return crypt_enable(home, yes),
    };
    // Is the owning scope an org the hub recognizes (and the caller can see)? A member's GET returns the
    // roster; a personal owner (or an unknown/invisible org) 404s → Ok(None). We have a hub remote (owner
    // resolved), so a hub we cannot reach or authenticate to must NOT silently downgrade to a machine-only
    // key that teammates could never decrypt — that would defeat the zero-config team default at exit 0.
    // Refuse with an actionable error and let the user retry or choose the reader set explicitly.
    let is_org = match crate::hubapi::HubEndpoint::resolve().and_then(|ep| ep.get_org(&owner)) {
        Ok(org) => org.is_some(),
        Err(e) => bail!(
            "could not confirm whether `{owner}` is a team: the hub is unreachable or your login is \
             not valid ({e:#}).\n\
             \x20      Refusing to silently encrypt to a machine-only key your teammates cannot decrypt.\n\
             \x20      Re-run once the hub is reachable, or choose the reader set explicitly:\n\
             \x20        agit a encrypt --team            readable to your org's team\n\
             \x20        agit a encrypt --readers <a,b>   wrap to specific people\n\
             \x20        agit a encrypt --public          readable to anyone with the repo"
        ),
    };
    match default_target(Some(&owner), is_org) {
        // Team default: crypt_enable_keybox_team already errors actionably ("run agit hub team rekey")
        // when the org has no Team KEK yet, so a zero-config encrypt never silently falls back.
        DefaultTarget::Team(org) => crypt_enable_keybox_team(home, Some(&org), &[], false, yes),
        DefaultTarget::RequireExplicit(o) => bail!(
            "`{o}` is a personal account, not a team; there is no team reader set to default to.\n\
             \x20      Name who can read this session explicitly:\n\
             \x20        agit a encrypt --readers <a,b>   wrap the content key to specific people\n\
             \x20        agit a encrypt --public          readable to anyone who has the repo\n\
             \x20      (or `agit a encrypt --team` if `{o}` is actually an org you belong to)."
        ),
        DefaultTarget::MachineGlobal => crypt_enable(home, yes),
    }
}

/// Print the two mandatory, non-negotiable warnings (req.5) before encryption does anything.
fn crypt_print_warnings() {
    errln!("agit encrypt; read both before continuing:");
    errln!(
        "  (1) The hub cannot render or server-side-scan an encrypted store; it never holds the key.\n\
         \x20     Encryption is only coherent for a no-hub, public-remote setup; you are trading hub\n\
         \x20     features for at-rest confidentiality."
    );
    errln!(
        "  (2) Your local secret gate now scans ENCRYPTED content, so it no longer sees plaintext\n\
         \x20     secrets in these sessions; the content is protected by encryption instead of by the\n\
         \x20     scanner."
    );
}

/// A yes/no gate honoured by `--yes` non-interactively; refuses (never hangs) when it cannot ask.
fn crypt_confirm(prompt: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !ui::interactive() {
        bail!("{prompt}\n  refusing without confirmation; re-run with --yes to proceed non-interactively");
    }
    out!("{prompt} [y/N] ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading confirmation")?;
    if !matches!(line.trim(), "y" | "Y" | "yes" | "Yes") {
        bail!("aborted; encryption not enabled");
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
    // Explicit agit identity: this is agit's own bookkeeping, so it stays labeled agit and never fails
    // when the user has no git identity configured.
    let path = msg.path().to_string_lossy();
    let args = [
        crate::agent::AGIT_META_IDENT.as_slice(),
        &["commit", "--no-verify", "-F", path.as_ref()],
    ]
    .concat();
    scope::git_in(store, &args)?;
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
        "agit-crypt master key; this IS the secret that decrypts every encrypted store.\n\
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
                    errln!("  ⚠ could not re-checkout sessions/**; run `git checkout -- .` in the store yourself");
                }
            }
        }
        Err(_) => {
            outln!("  (no agent resolves here yet; after `agit a clone <name>`, run `agit a encrypt` to wire the filter)");
        }
    }
    Ok(0)
}

// ─────────────────────── Per-session keybox: encrypt --readers/--public, readers, rekey, unlock ───────────────────────

/// The `.gitattributes` line excluding the committed keybox from the crypt filter: it is already
/// wrap-ciphertext, and filtering it would double-encrypt and deadlock the bootstrap.
const KEYBOX_ATTR_LINE: &str = "/.agit/keybox.jsonl -filter";

/// The next non-flag positional argument's value for `--name <value>` (a value that is itself a flag is
/// treated as absent).
fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .filter(|s| !s.starts_with('-'))
        .cloned()
}

/// The first bare (non-`-`) positional argument.
fn first_positional(args: &[String]) -> Option<String> {
    args.iter().find(|a| !a.starts_with('-')).cloned()
}

/// Ensure `.gitattributes` carries the keybox exclusion (`/.agit/keybox.jsonl -filter`). Returns true if
/// the line was added.
fn keybox_write_gitattributes(store: &Path) -> Result<bool> {
    let path = store.join(".gitattributes");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| {
        let t = l.trim();
        t.starts_with("/.agit/keybox.jsonl") && t.contains("-filter")
    }) {
        return Ok(false);
    }
    let mut s = existing;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(KEYBOX_ATTR_LINE);
    s.push('\n');
    std::fs::write(&path, s).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}

/// The resolved store's repo-local keyring path, or a clear error if the store is not a git repo.
fn store_keyring_path(store: &Path) -> Result<PathBuf> {
    crate::crypt::repo_keyring_path_from(store)
        .context("cannot resolve the repo-local keyring path (is the store a git repo?)")
}

/// Load the repo-local keyring, or an actionable error when this session is not keybox-encrypted.
fn require_session_keyring(store: &Path) -> Result<(PathBuf, crate::crypt::Keyring)> {
    let kp = store_keyring_path(store)?;
    let ring = crate::crypt::load_keyring_at(&kp)?.ok_or_else(|| {
        anyhow::anyhow!(
            "this session is not keybox-encrypted (no {}).\n\
             \x20      Enable it first: agit a encrypt --readers <a,b> | --public",
            kp.display()
        )
    })?;
    Ok((kp, ring))
}

/// `agit a encrypt --readers a,b | --public` — enable PER-SESSION keybox encryption: mint a fresh content
/// key (CK, generation 0) into the repo-local keyring, wrap it to each named reader (X25519) and/or write
/// a public stanza, install the filter + committed keybox, and re-encrypt tracked sessions under CK.
fn crypt_enable_keybox(home: &Path, readers: &[String], public: bool, yes: bool) -> Result<i32> {
    let a = agent::resolve(None)?;
    let store = a.store.clone();
    let kp = store_keyring_path(&store)?;
    if kp.exists() {
        bail!(
            "this session already has a per-session keyring at {}.\n\
             \x20      Manage readers with `agit a readers add/rm` or rotate with `agit a rekey`.",
            kp.display()
        );
    }

    crypt_print_warnings();
    crypt_confirm("Enable per-session keybox encryption on this store?", yes)?;

    // Mint CK gen 0 into the repo-local keyring BEFORE any `git add`: the clean filter reads it from there.
    let ck = crate::crypt::random_master()?;
    crate::crypt::init_repo_keyring_at(&kp, ck)?;

    // The owner must be able to unlock a fresh clone from the keybox alone (the repo-local keyring is
    // never pushed). Best-effort: if the owner has a hub identity AND an enrolled key, add them as a
    // normal reader so rekey/rm re-resolve it like any other. Public sessions need no self-stanza (the CK
    // is already recoverable from the repo). Any failure just prints guidance -- it never blocks encrypt.
    let mut readers: Vec<String> = readers.to_vec();
    if !public {
        let me = crate::hubapi::HubEndpoint::resolve().ok().and_then(|ep| ep.me().ok());
        match me {
            Some(me) if readers.iter().any(|r| r == &me) => {} // already an explicit reader
            Some(me) if crate::keybox::resolve_recipient(home, &me, None, false).is_ok() => {
                outln!("  including you ({me}) as a reader; you can unlock a fresh clone");
                readers.push(me);
            }
            _ => outln!(
                "  note: to unlock a fresh clone on another machine, register (`agit identity register <you>`, paste into the hub) and \
                 add yourself (`agit a readers add <you>`), or encrypt with --public."
            ),
        }
    }

    // Build the keybox stanzas for kid 0 (resolve + TOFU-pin each reader).
    let mut stanzas = Vec::new();
    if public {
        stanzas.push(crate::keybox::public_stanza(&ck, 0));
    }
    for r in &readers {
        let key = crate::keybox::resolve_recipient(home, r, None, false)
            .with_context(|| format!("resolving reader `{r}`"))?;
        stanzas.push(crate::keybox::user_stanza(&ck, 0, r, 0, &key)?);
        outln!("  wrapped the content key to {r}");
    }
    if public {
        outln!("  wrote a public stanza (anyone with the repo can read)");
    }
    crate::keybox::write_keybox(&store, &stanzas)?;

    let _lock = crate::session::lock_store(&store)?;
    let _ = crypt_write_gitattributes(&store)?;
    let _ = keybox_write_gitattributes(&store)?;
    crypt_wire_filter(&store)?;

    // Commit the keybox + attributes first, then renormalize tracked sessions under CK.
    let _ = scope::git_in_status(&store, &["add", "--", ".gitattributes", ".agit/keybox.jsonl"]);
    if scope::git_in_status(&store, &["diff", "--cached", "--quiet"]).0 != 0 {
        crypt_commit(
            &store,
            "chore(crypt): enable per-session keybox encryption\n\nMints a per-session content key, wraps it to the readers, and commits the keybox.",
        )?;
    }
    let _ = scope::git_in_status(&store, &["add", "--renormalize", "--", "sessions"]);
    if scope::git_in_status(&store, &["diff", "--cached", "--quiet"]).0 != 0 {
        crypt_commit(&store, "chore(crypt): re-encrypt tracked sessions under the per-session key")?;
    }

    outln!("per-session keybox encryption enabled on {} ({}).", a.name, a.aid);
    outln!("  readers unlock after cloning with `agit crypt unlock`.");
    Ok(0)
}

// ─────────────────────── Team KEK (encryption-recipients Wave 3): owning-org resolution, TK obtain ───────────────────────

/// The owner segment of a store's primary remote URL — the account (a user OR an org) a session lives
/// under. `https://host/acme/frontend.git` → `acme`. Errors clearly when the store has no hub remote.
fn owning_owner_of(store: &Path) -> Result<String> {
    let primary = agent::primary_remote_name(store);
    let remotes = agent::store_remotes(store);
    let url = match primary {
        Some(n) => remotes.into_iter().find(|(rn, _)| *rn == n).map(|(_, u)| u),
        None => remotes.into_iter().next().map(|(_, u)| u),
    }
    .context(
        "this session has no hub remote to read an owning org from.\n\
         \x20      Push it first (`agit a push`), or name the org: `--org <name>`.",
    )?;
    owner_from_url(&url)
}

/// Parse the owner (first path segment) out of an `scheme://[userinfo@]authority/owner/name(.git)` URL.
fn owner_from_url(url: &str) -> Result<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = rest
        .split_once('/')
        .map(|(_, p)| p)
        .context("the hub remote URL has no path to read an owning org from")?;
    let first = path.split('/').next().unwrap_or("").trim().trim_end_matches(".git");
    if first.is_empty() {
        bail!("could not read an owning org from the hub remote URL");
    }
    Ok(first.to_string())
}

/// Parse `(owner, name)` — the first two path segments — out of a hub remote URL. `name` has any `.git`
/// suffix stripped. Errors clearly when the URL has no `<owner>/<name>` path.
fn owner_and_name_from_url(url: &str) -> Result<(String, String)> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = rest
        .split_once('/')
        .map(|(_, p)| p)
        .context("the hub remote URL has no `<owner>/<name>` path")?;
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    let owner = segs.next().unwrap_or("").trim().to_string();
    let name = segs.next().unwrap_or("").trim().trim_end_matches(".git").to_string();
    if owner.is_empty() || name.is_empty() {
        bail!("could not read `<owner>/<name>` from the hub remote URL");
    }
    Ok((owner, name))
}

/// The `(owner, name)` a store's primary hub remote points at.
fn owner_and_name_of(store: &Path) -> Result<(String, String)> {
    let primary = agent::primary_remote_name(store);
    let remotes = agent::store_remotes(store);
    let url = match primary {
        Some(n) => remotes.into_iter().find(|(rn, _)| *rn == n).map(|(_, u)| u),
        None => remotes.into_iter().next().map(|(_, u)| u),
    }
    .context(
        "this session has no hub remote; `agit hub doctor` reconciles a pushed session's ACL against\n\
         \x20      its keybox. Push it first (`agit a push`).",
    )?;
    owner_and_name_from_url(&url)
}

/// Obtain the unwrapped Team KEK for `(org, gen)`: the local cache first, else fetch the caller's OWN
/// `team_keks` envelope from the hub and unwrap it with this machine's X25519 secret, caching the result.
/// Fail-closed: a missing envelope (not a member, or the gen is not sealed to the caller) is an error.
fn obtain_tk(home: &Path, org: &str, gen: i64) -> Result<[u8; 32]> {
    if let Some(tk) = crate::keybox::read_cached_tk(home, org, gen)? {
        return Ok(tk);
    }
    let ep = crate::hubapi::HubEndpoint::resolve()?;
    let env = ep.get_kek_envelope(org, gen)?.with_context(|| {
        format!(
            "no team-KEK envelope for you at org `{org}` generation {gen}; you are not a member, or this\n\
             \x20      generation was not sealed to you. Ask an admin to run `agit hub team sync {org}`."
        )
    })?;
    let wrapped = env
        .get("wrapped_kek")
        .and_then(|w| w.as_str())
        .context("the hub returned a team-KEK envelope with no wrapped_kek")?;
    let sk = agent::machine_signing_key()?;
    let secret = agent::derive_x25519_secret(&sk);
    let tk = crate::keybox::open_tk_envelope(wrapped, &secret)
        .context("could not unwrap your team-KEK envelope with this machine's identity")?;
    crate::keybox::write_cached_tk(home, org, gen, &tk)?;
    Ok(tk)
}

/// Seal `tk` to every current member of `org` (TOFU-pinning each fetched pubkey), returning the JSON
/// envelope array to publish plus the members skipped for having no enrolled identity key. Used by both
/// `hub team rekey` (a fresh TK) and `hub team sync` (the current TK).
fn seal_tk_to_members(
    home: &Path,
    ep: &crate::hubapi::HubEndpoint,
    org: &str,
    tk: &[u8; 32],
) -> Result<(Vec<serde_json::Value>, Vec<String>)> {
    let org_json = ep
        .get_org(org)?
        .with_context(|| format!("cannot read org `{org}`; are you a member/admin of it?"))?;
    let members = org_json.get("members").and_then(|m| m.as_array()).cloned().unwrap_or_default();
    if members.is_empty() {
        bail!("org `{org}` has no members to seal a team KEK to");
    }
    let mut envelopes = Vec::new();
    let mut skipped = Vec::new();
    for m in &members {
        let Some(uname) = m.get("username").and_then(|u| u.as_str()).filter(|u| !u.is_empty()) else {
            continue;
        };
        // Fetch the member's enrolled identity (x25519 pubkey + epoch). No enrolled key → skip loudly.
        let idv = match ep.get_identity(uname)? {
            Some(v) => v,
            None => {
                skipped.push(uname.to_string());
                continue;
            }
        };
        let x_hex = idv.get("x25519_pub").and_then(|x| x.as_str()).unwrap_or("");
        let epoch = idv.get("epoch").and_then(|e| e.as_i64()).unwrap_or(0);
        if x_hex.is_empty() {
            skipped.push(uname.to_string());
            continue;
        }
        // TOFU-pin the fetched key (hard-fail on a changed pubkey) exactly like the individual path.
        let key = crate::keybox::resolve_recipient(home, uname, Some(x_hex), false)
            .with_context(|| format!("resolving team member `{uname}`'s key"))?;
        let wrapped = crate::keybox::seal_tk_for_member(tk, &key)?;
        envelopes.push(serde_json::json!({
            "recipient": uname,
            "wrapped_kek": wrapped,
            "recipient_epoch": epoch,
        }));
    }
    // Wave 5, feature 1: if (and ONLY if) the org has an OPT-IN offline recovery recipient set, seal TK to
    // it too as an extra `@recovery` envelope. Unset (the default) ⇒ no extra envelope ⇒ exactly Wave 3/4.
    if let Some(rec) = recovery_envelope(&org_json, tk)? {
        envelopes.push(rec);
    }
    Ok((envelopes, skipped))
}

/// Build the reserved `@recovery` Team-KEK envelope when the org has an OPT-IN offline recovery recipient
/// (Wave 5, feature 1): TK sealed to the org's recovery X25519 pubkey. Returns `None` when unset (the
/// default) — then team rekey/sync behave EXACTLY as Wave 3/4 with NO recovery envelope. Pure, so the
/// "unset ⇒ none; set ⇒ one @recovery envelope" invariant is unit-testable without a hub.
///
/// SECURITY: sealing to this key RE-TRUSTS an OFFLINE admin (not the hub) and WEAKENS forward secrecy for
/// the team — whoever holds the matching offline SECRET can decrypt every TK generation they were sealed
/// to. It is off unless an org owner explicitly registers a recovery key with `agit hub org recovery set`.
fn recovery_envelope(org_json: &serde_json::Value, tk: &[u8; 32]) -> Result<Option<serde_json::Value>> {
    let hexkey = org_json.get("recovery_x25519").and_then(|k| k.as_str()).unwrap_or("").trim();
    if hexkey.is_empty() {
        return Ok(None);
    }
    let key = crate::keybox::decode_x25519_hex(hexkey)
        .context("the org's recovery_x25519 recipient is not a valid X25519 pubkey")?;
    let wrapped = crate::keybox::seal_tk_for_member(tk, &key)?;
    Ok(Some(serde_json::json!({
        "recipient": crate::hub::store::RECOVERY_RECIPIENT,
        "wrapped_kek": wrapped,
        "recipient_epoch": 0,
    })))
}

/// `agit hub <team ...|doctor>` — hub-side operations that are not per-agent git.
pub fn hub_cmd(args: &[String]) -> Result<i32> {
    match args.first().map(|s| s.as_str()) {
        Some("team") => hub_team_cmd(&args[1..]),
        Some("doctor") => hub_doctor(&args[1..]),
        Some("org") => hub_org_cmd(&args[1..]),
        Some(other) => {
            errln!("agit hub: unknown subcommand `{other}`");
            errln!("  usage: agit hub team rekey <org> [--rekey-all]  ·  team sync <org>  ·  doctor [--org <org>] [--check] [--fix]");
            errln!("         agit hub org recovery set <org> --key <hex>  ·  recovery clear|show <org>  ·  escrow <org> --mode hub-assist|none");
            Ok(2)
        }
        None => {
            errln!("usage: agit hub team rekey <org> [--rekey-all]  ·  team sync <org>  ·  doctor [--org <org>] [--check] [--fix]");
            errln!("       agit hub org recovery set <org> --key <hex>  ·  recovery clear|show <org>  ·  escrow <org> --mode hub-assist|none");
            Ok(2)
        }
    }
}

/// `agit hub org <recovery|escrow> …` — the Wave-5 opt-in org escape hatches (both OFF by default). Every
/// verb here is OWNER-only at the hub (a non-owner is a 403); these commands only carry the request.
///
/// `recovery set <org> --key <hex>` registers an OFFLINE recovery X25519 pubkey (re-trusts an offline
/// admin, NOT the hub) so `team rekey` seals TK to it too; `recovery clear <org>` removes it; `recovery
/// show <org>` prints it. `escrow <org> --mode hub-assist|none` flips hub-assist key release for the org
/// (re-trusts the HUB). See the design doc's Wave-5 section.
fn hub_org_cmd(args: &[String]) -> Result<i32> {
    let ep = crate::hubapi::HubEndpoint::resolve()?;
    match args.first().map(|s| s.as_str()) {
        Some("recovery") => {
            let sub = args.get(1).map(|s| s.as_str());
            // The org is the first bare positional after the verb.
            let org = args.iter().skip(2).find(|s| !s.starts_with('-')).map(|s| s.trim()).filter(|s| !s.is_empty());
            match (sub, org) {
                (Some("set"), Some(org)) => {
                    let Some(key) = flag_value(args, "--key") else {
                        errln!("usage: agit hub org recovery set <org> --key <hex-x25519>");
                        return Ok(2);
                    };
                    // Validate locally so a typo fails fast with a clear message before the round-trip.
                    let key = key.trim().to_ascii_lowercase();
                    if crate::keybox::decode_x25519_hex(&key).is_err() {
                        bail!("--key must be a 64-hex-char (32-byte) X25519 public key");
                    }
                    ep.set_org_recovery(org, &key)?;
                    outln!("set the offline recovery recipient for {org}.");
                    outln!("  ⚠ this RE-TRUSTS whoever holds the matching offline SECRET: they can decrypt every");
                    outln!("    future team-KEK generation `agit hub team rekey {org}` seals (weakens forward secrecy).");
                    Ok(0)
                }
                (Some("clear"), Some(org)) => {
                    ep.clear_org_recovery(org)?;
                    outln!("cleared the offline recovery recipient for {org} (future rekeys emit no @recovery envelope).");
                    Ok(0)
                }
                (Some("show"), Some(org)) => {
                    let org_json = ep.get_org(org)?.with_context(|| format!("cannot read org `{org}` (are you a member?)"))?;
                    let rec = org_json.get("recovery_x25519").and_then(|k| k.as_str()).unwrap_or("");
                    if rec.is_empty() {
                        outln!("{org}: no offline recovery recipient set (the default).");
                    } else {
                        outln!("{org}: offline recovery recipient = {rec}");
                    }
                    Ok(0)
                }
                _ => {
                    errln!("usage: agit hub org recovery set <org> --key <hex>  ·  recovery clear <org>  ·  recovery show <org>");
                    Ok(2)
                }
            }
        }
        Some("escrow") => {
            let org = args.iter().skip(1).find(|s| !s.starts_with('-')).map(|s| s.trim()).filter(|s| !s.is_empty());
            let Some(org) = org else {
                errln!("usage: agit hub org escrow <org> --mode hub-assist|none");
                return Ok(2);
            };
            let Some(mode) = flag_value(args, "--mode") else {
                errln!("usage: agit hub org escrow <org> --mode hub-assist|none");
                return Ok(2);
            };
            let mode = mode.trim();
            if mode != "hub-assist" && mode != "none" {
                bail!("--mode must be hub-assist or none");
            }
            ep.set_org_escrow(org, mode)?;
            if mode == "hub-assist" {
                outln!("{org}: escrow mode = hub-assist.");
                outln!("  ⚠ this RE-TRUSTS the hub: for sessions whose owner runs `agit a escrow enable`, the hub");
                outln!("    can RELEASE the content key to any caller the ACL lets read (via `agit crypt unlock`).");
            } else {
                outln!("{org}: escrow mode = none (hub-assist key release is off; existing escrow rows go unused).");
            }
            Ok(0)
        }
        Some(other) => {
            errln!("agit hub org: unknown subcommand `{other}`");
            errln!("  usage: agit hub org recovery set|clear|show <org>  ·  escrow <org> --mode hub-assist|none");
            Ok(2)
        }
        None => {
            errln!("usage: agit hub org recovery set|clear|show <org>  ·  escrow <org> --mode hub-assist|none");
            Ok(2)
        }
    }
}

/// `agit hub team <rekey|sync> <org> [--rekey-all]`.
fn hub_team_cmd(args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str());
    // The org is the first bare positional (so `rekey --rekey-all acme` and `rekey acme --rekey-all`
    // both work); `--rekey-all` is the labeled O(sessions) panic button.
    let org = args.iter().skip(1).find(|s| !s.starts_with('-')).map(|s| s.trim()).filter(|s| !s.is_empty());
    let rekey_all = args.iter().any(|a| a == "--rekey-all");
    match (sub, org) {
        (Some("rekey"), Some(o)) => hub_team_rekey(o, rekey_all),
        (Some("sync"), Some(o)) => hub_team_sync(o),
        (Some("rekey"), None) => {
            errln!("usage: agit hub team rekey <org> [--rekey-all]");
            Ok(2)
        }
        (Some("sync"), None) => {
            errln!("usage: agit hub team sync <org>");
            Ok(2)
        }
        _ => {
            errln!("agit hub team: rekey <org> [--rekey-all]  ·  sync <org>");
            Ok(2)
        }
    }
}

/// `agit hub team rekey <org>` — ROTATE the org's Team KEK: mint a fresh random TK at gen = current+1,
/// seal it to EVERY current member's X25519 pubkey, publish the envelopes, and advance current_kek_gen.
/// O(members). This is the removal/rotation path: a removed member is simply absent from the new roster,
/// so gets no new-gen envelope and cannot open content sealed under the new generation.
fn hub_team_rekey(org: &str, rekey_all: bool) -> Result<i32> {
    let home = scope::agit_home()?;
    let ep = crate::hubapi::HubEndpoint::resolve()?;
    let current = ep
        .kek_gens(org)
        .with_context(|| format!("cannot read the team-KEK state of `{org}` (are you a member/admin?)"))?
        .get("current")
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    let next = current + 1;

    let tk = crate::crypt::random_master()?;
    let (envelopes, skipped) = seal_tk_to_members(&home, &ep, org, &tk)?;
    if envelopes.is_empty() {
        bail!(
            "no member of `{org}` has an enrolled identity key to seal the team KEK to.\n\
             \x20      Ask members to run `agit identity register <them>` and paste the block into the hub first."
        );
    }
    let sealed = envelopes.len();
    ep.post_kek_envelopes(org, next, serde_json::Value::Array(envelopes))
        .with_context(|| format!("publishing team-KEK generation {next} for `{org}`"))?;
    // Cache the new TK locally so we do not immediately refetch our own envelope.
    crate::keybox::write_cached_tk(&home, org, next, &tk)?;
    outln!("rotated team KEK for {org}: generation {next}, sealed to {sealed} member(s).");
    if !skipped.is_empty() {
        outln!("  skipped (no enrolled identity key): {}", skipped.join(", "));
    }
    outln!("  a removed member (absent from the roster) gets no gen-{next} envelope and cannot open new content.");

    // `--rekey-all`: the labeled O(sessions) panic button. Rotate the CK of every LOCALLY-available
    // team-encrypted session bound to this org and re-wrap under the new TK. Plain rekey (no flag) is the
    // TK-generation-only Wave-3 behavior — a removed member is already locked out of new content.
    if rekey_all {
        let failed = rekey_local_team_sessions(&home, org, next, &tk)?;
        if failed > 0 {
            errln!(
                "  ⚠ {failed} local session(s) could NOT be rekeyed (each left UNCHANGED on its old key -\n\
                 \x20     no corruption). Their new content is not yet under the rotated team key. Fix the\n\
                 \x20     cause (often a TOFU mismatch: `agit identity pin <user> --repin`) and re-run\n\
                 \x20     `agit hub team rekey {org} --rekey-all`."
            );
            return Ok(1);
        }
    }
    Ok(0)
}

/// `--rekey-all` bulk form: rotate the content key of every LOCALLY-available team-encrypted session
/// bound to `org`, re-wrapping under the freshly-rotated TK (`new_gen`/`tk`) and committing each. Reports
/// which sessions were rekeyed and warns that sessions NOT present on this machine were not touched (they
/// rekey on their own next local `agit a rekey`). Enumerates via the machine's agent registry.
fn rekey_local_team_sessions(home: &Path, org: &str, new_gen: i64, tk: &[u8; 32]) -> Result<usize> {
    let agents = agent::list().unwrap_or_default();
    let mut rekeyed: Vec<(String, String, u32)> = Vec::new();
    let mut failed = 0usize;
    for a in &agents {
        // Only sessions whose CURRENT keyring generation carries a team stanza for THIS org qualify.
        let Some(kp) = crate::crypt::repo_keyring_path_from(&a.store) else { continue };
        let Ok(Some(ring)) = crate::crypt::load_keyring_at(&kp) else { continue };
        let stanzas = crate::keybox::read_keybox(&a.store).unwrap_or_default();
        let bound = stanzas
            .iter()
            .any(|s| matches!(s, crate::keybox::Stanza::Team(t) if t.org == org && t.kid == ring.current));
        if !bound {
            continue;
        }
        match rekey_one_team_session(home, &a.store, org, new_gen, tk) {
            Ok(new_kid) => rekeyed.push((a.name.clone(), a.aid.clone(), new_kid)),
            Err(e) => {
                failed += 1;
                errln!("  ⚠ could not rekey {} ({}): {e:#}", a.name, a.aid);
            }
        }
    }
    if rekeyed.is_empty() {
        outln!("  --rekey-all: no locally-available team session bound to {org} to rotate.");
    } else {
        outln!("  --rekey-all: rotated the content key of {} local session(s) under gen {new_gen}:", rekeyed.len());
        for (name, aid, kid) in &rekeyed {
            outln!("      {name} ({aid}) → kid {kid}");
        }
    }
    outln!(
        "  ⚠ sessions NOT present on this machine were NOT rekeyed; they rotate on their next local\n\
         \x20     `agit a rekey` (or a later `--rekey-all` run on the machine that has them)."
    );
    Ok(failed)
}

/// Rotate ONE team-encrypted session's CK and re-wrap: mint CK', bump the repo-local keyring, RETAIN every
/// old stanza (past readers keep past access, forward-only), then append a team stanza under
/// `(org, new_gen, tk)` at the new kid plus re-wrap CK' to the session's current individual readers and
/// public flag. Commits only the keybox line. Returns the new kid. Other orgs' team stanzas are NOT
/// re-emitted here (their TK is not in hand); they rotate on that org's own `--rekey-all`.
fn rekey_one_team_session(home: &Path, store: &Path, org: &str, new_gen: i64, tk: &[u8; 32]) -> Result<u32> {
    let kp = store_keyring_path(store)?;
    let existing = crate::keybox::read_keybox(store)?;
    let ring = crate::crypt::load_keyring_at(&kp)?
        .context("this team session has no repo-local keyring to rotate")?;
    let cur_kid = ring.current;
    let readers = crate::keybox::readers_at(&existing, cur_kid);
    let keep_public = crate::keybox::is_public_at(&existing, cur_kid);

    let _lock = crate::session::lock_store(store)?;

    // Resolve EVERY individual reader's key BEFORE touching the keyring. resolve_recipient can bail (a TOFU
    // key mismatch, an unreachable hub); if that happened after rotate_keyring_at, the keyring would sit at
    // a new kid with no matching keybox stanza — content sealed under it would be unreadable by everyone,
    // author included. Doing all the fallible work first means a failure aborts with the session UNCHANGED.
    let reader_keys: Vec<(&String, [u8; 32])> = readers
        .iter()
        .map(|r| {
            crate::keybox::resolve_recipient(home, r, None, false)
                .with_context(|| format!("re-wrapping CK' to reader `{r}`"))
                .map(|k| (r, k))
        })
        .collect::<Result<_>>()?;

    // All recipients resolved — from here on only local, effectively-infallible work (RNG + AEAD + an
    // atomic keybox write). Mint CK', rotate the keyring, then seal the full stanza set under the new kid.
    let ck_prime = crate::crypt::random_master()?;
    let new_kid = crate::crypt::rotate_keyring_at(&kp, ck_prime)?;

    let mut stanzas = existing;
    stanzas.push(crate::keybox::team_stanza(&ck_prime, new_kid, org, new_gen, tk)?);
    if keep_public {
        stanzas.push(crate::keybox::public_stanza(&ck_prime, new_kid));
    }
    for (r, key) in &reader_keys {
        stanzas.push(crate::keybox::user_stanza(&ck_prime, new_kid, r, 0, key)?);
    }
    crate::keybox::write_keybox(store, &stanzas)?;

    let _ = scope::git_in_status(store, &["add", "--", ".agit/keybox.jsonl"]);
    if scope::git_in_status(store, &["diff", "--cached", "--quiet"]).0 != 0 {
        crypt_commit(
            store,
            "chore(crypt): team rekey-all rotate session content key\n\nRotates CK and re-wraps under the new team-KEK generation; new commits seal under the new key-id.",
        )?;
    }
    Ok(new_kid)
}

/// `agit hub team sync <org>` — JOIN: seal the CURRENT-gen TK to the org's members (no gen bump), so a
/// newly-added member gains an envelope and can unlock every team-wrapped session. Requires the caller
/// can obtain the current TK (a member with an envelope) AND publish (org-admin). Idempotent: republishing
/// the current generation re-seals existing members harmlessly (same TK, fresh ephemeral).
fn hub_team_sync(org: &str) -> Result<i32> {
    let home = scope::agit_home()?;
    let ep = crate::hubapi::HubEndpoint::resolve()?;
    let current = ep
        .kek_gens(org)
        .with_context(|| format!("cannot read the team-KEK state of `{org}` (are you a member/admin?)"))?
        .get("current")
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    if current < 1 {
        bail!("org `{org}` has no team KEK yet; run `agit hub team rekey {org}` first.");
    }
    // The caller must hold a current-gen envelope to obtain TK (fail-closed if they do not).
    let tk = obtain_tk(&home, org, current).with_context(|| {
        format!("you need a gen-{current} envelope to sync `{org}`; ask an admin to run `agit hub team rekey {org}`")
    })?;
    let (envelopes, skipped) = seal_tk_to_members(&home, &ep, org, &tk)?;
    if envelopes.is_empty() {
        bail!("no member of `{org}` has an enrolled identity key to seal the team KEK to.");
    }
    let sealed = envelopes.len();
    ep.post_kek_envelopes(org, current, serde_json::Value::Array(envelopes))
        .with_context(|| format!("republishing team-KEK generation {current} for `{org}`"))?;
    outln!("synced team KEK for {org}: generation {current}, sealed to {sealed} member(s).");
    if !skipped.is_empty() {
        outln!("  skipped (no enrolled identity key): {}", skipped.join(", "));
    }
    Ok(0)
}

/// The org's active Team-KEK generation, or a clear "run rekey first" error when the org has none. Used
/// by the `--team` encrypt/readers paths.
fn org_current_tk_gen(ep: &crate::hubapi::HubEndpoint, org: &str) -> Result<i64> {
    let current = ep
        .kek_gens(org)
        .with_context(|| format!("cannot read the team KEK for org `{org}` (are you a member?)"))?
        .get("current")
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    require_tk_gen(current, org)
}

/// Validate an org's reported current Team-KEK generation: a usable gen is `>= 1`; a `0`/absent gen means
/// the org has never rotated a TK, which is an ACTIONABLE error (run `agit hub team rekey`) rather than a
/// silent fall-back to owner-only or public. Pure so the zero-config/team no-TK path is unit-testable.
fn require_tk_gen(current: i64, org: &str) -> Result<i64> {
    if current < 1 {
        bail!(
            "org `{org}` has no team KEK yet.\n\
             \x20      An org admin must run `agit hub team rekey {org}` first, then retry."
        );
    }
    Ok(current)
}

// ─────────────────────── hub doctor (Wave 4): axis-1 (ACL) vs axis-2 (keybox) drift reconciliation ───────────────────────

/// The two drift classes `agit hub doctor` reconciles for one session. Pure data, produced by
/// [`reconcile`]: a fully authorized-but-undecryptable member, and a decryptable-but-unauthorized reader.
#[derive(Debug, Default, PartialEq, Eq)]
struct DoctorDrift {
    /// Authorized to FETCH (org member / ACL reader) but has NO keybox stanza — cannot decrypt.
    member_without_stanza: Vec<String>,
    /// Holds a keybox `user` stanza but is NOT an authorized fetcher — the hub refuses them the bytes.
    stanza_without_membership: Vec<String>,
}

impl DoctorDrift {
    fn has_drift(&self) -> bool {
        !self.member_without_stanza.is_empty() || !self.stanza_without_membership.is_empty()
    }
}

/// Reconcile axis-1 (who may fetch) against axis-2 (who can decrypt) for one session. `authorized` is the
/// folded fetch set (org members ∪ agent ACL readers ∪ personal owner). `user_readers` are the keybox
/// `user`-stanza names; `team_covered` are the org members a `team` stanza covers; `public` is whether a
/// public stanza makes the CK readable to anyone who has the repo. Pure — no I/O — so the reconciliation
/// policy is unit-testable independently of the hub.
fn reconcile(
    authorized: &std::collections::BTreeSet<String>,
    user_readers: &std::collections::BTreeSet<String>,
    team_covered: &std::collections::BTreeSet<String>,
    public: bool,
) -> DoctorDrift {
    // A public session hands the CK to anyone with the repo, so every authorized fetcher can decrypt →
    // no member-without-stanza. Otherwise coverage is the union of individual + team coverage.
    let covered: std::collections::BTreeSet<String> = if public {
        authorized.clone()
    } else {
        user_readers.union(team_covered).cloned().collect()
    };
    let mut member_without_stanza: Vec<String> = authorized.difference(&covered).cloned().collect();
    let mut stanza_without_membership: Vec<String> = user_readers.difference(authorized).cloned().collect();
    member_without_stanza.sort();
    stanza_without_membership.sort();
    DoctorDrift { member_without_stanza, stanza_without_membership }
}

/// `agit hub doctor [--org <org>] [--check] [--fix]` — reconcile axis-1 authorization (hub ACL / org
/// membership) against axis-2 confidentiality (keybox stanzas). Read-only by default. `--org` audits
/// every LOCALLY-available session bound to that org; the default is the current session. `--check` exits
/// non-zero when drift remains (a CI/user gate). `--fix` `readers add`s the missing members but NEVER
/// auto-removes readers (that advice is printed for the user to act on explicitly).
fn hub_doctor(args: &[String]) -> Result<i32> {
    let home = scope::agit_home()?;
    let check = args.iter().any(|a| a == "--check");
    let fix = args.iter().any(|a| a == "--fix");
    let org_filter = flag_value(args, "--org");

    let ep = crate::hubapi::HubEndpoint::resolve()?;

    let targets: Vec<agent::Agent> = match &org_filter {
        Some(org) => {
            let v: Vec<agent::Agent> = agent::list()
                .unwrap_or_default()
                .into_iter()
                .filter(|a| owning_owner_of(&a.store).ok().as_deref() == Some(org.as_str()))
                .collect();
            if v.is_empty() {
                outln!("hub doctor: no locally-available session bound to org `{org}` to audit.");
            }
            v
        }
        None => vec![agent::resolve(None)?],
    };

    let mut any_drift = false;
    for a in &targets {
        match doctor_one(&home, &ep, a, fix) {
            Ok(has_drift) => any_drift |= has_drift,
            Err(e) => {
                errln!("hub doctor: {} ({}): {e:#}", a.name, a.aid);
                any_drift = true; // an audit that could not complete is not "clean"
            }
        }
    }

    // --check gates on drift; without it, doctor is a report and exits 0.
    if check && any_drift {
        return Ok(1);
    }
    Ok(0)
}

/// Audit ONE session: derive the folded fetch set from the hub, the keybox coverage from disk, reconcile
/// them, print a report, and (with `fix`) `readers add` the missing members. Returns whether drift
/// REMAINS after any fix (so `--check` can gate on it).
fn doctor_one(home: &Path, ep: &crate::hubapi::HubEndpoint, a: &agent::Agent, fix: bool) -> Result<bool> {
    use std::collections::BTreeSet;
    let store = &a.store;
    let stanzas = crate::keybox::read_keybox(store)?;
    outln!("── hub doctor: {} ({}) ──", a.name, a.aid);
    if stanzas.is_empty() {
        outln!("  not keybox-encrypted; no confidentiality axis to reconcile.");
        return Ok(false);
    }
    let (owner, name) = owner_and_name_of(store)?;

    // axis-1: the folded fetch set = agent ACL members ∪ (org members when org-owned) ∪ personal owner.
    let mut authorized: BTreeSet<String> = BTreeSet::new();
    let agent_json = ep
        .get_agent(&owner, &name)?
        .with_context(|| format!("cannot read agent `{owner}/{name}` from the hub (is it pushed, and are you a reader?)"))?;
    if let Some(ms) = agent_json.get("members").and_then(|m| m.as_array()) {
        for m in ms {
            if let Some(u) = m.get("username").and_then(|u| u.as_str()) {
                authorized.insert(u.to_string());
            }
        }
    }
    let org_json = ep.get_org(&owner)?;
    let is_org = org_json.is_some();
    let mut org_members: BTreeSet<String> = BTreeSet::new();
    if let Some(org_json) = &org_json {
        if let Some(ms) = org_json.get("members").and_then(|m| m.as_array()) {
            for m in ms {
                if let Some(u) = m.get("username").and_then(|u| u.as_str()) {
                    org_members.insert(u.to_string());
                    authorized.insert(u.to_string());
                }
            }
        }
    } else {
        // A personal owner is themselves an authorized fetcher; an org owner name is not a user.
        authorized.insert(owner.clone());
    }

    // axis-2: keybox coverage at the CURRENT kid (fall back to the highest stanza kid when there is no
    // local keyring, e.g. an unrelated machine auditing via --org).
    let cur_kid = crate::crypt::repo_keyring_path_from(store)
        .and_then(|p| crate::crypt::load_keyring_at(&p).ok().flatten())
        .map(|r| r.current)
        .unwrap_or_else(|| stanzas.iter().map(|s| s.kid()).max().unwrap_or(0));
    let user_readers: BTreeSet<String> = crate::keybox::readers_at(&stanzas, cur_kid).into_iter().collect();
    let public = crate::keybox::is_public_at(&stanzas, cur_kid);
    let teams = crate::keybox::teams_at(&stanzas, cur_kid);
    // A team stanza for the owning org covers that org's members (they hold, or should hold, an envelope).
    let team_covered: BTreeSet<String> =
        if teams.iter().any(|(o, _)| o == &owner) { org_members.clone() } else { BTreeSet::new() };

    let drift = reconcile(&authorized, &user_readers, &team_covered, public);

    let readers_line = if user_readers.is_empty() {
        "-".to_string()
    } else {
        user_readers.iter().cloned().collect::<Vec<_>>().join(", ")
    };
    let team_line = if teams.is_empty() {
        String::new()
    } else {
        format!("  team: {}", teams.iter().map(|(o, g)| format!("{o}@{g}")).collect::<Vec<_>>().join(", "))
    };
    outln!(
        "  owner {owner}{}  ·  kid {cur_kid}  ·  readers: {readers_line}{}{team_line}",
        if is_org { " (org)" } else { " (user)" },
        if public { " [public]" } else { "" }
    );

    // Team-stanza freshness + my-envelope check.
    for (o, g) in &teams {
        if let Ok(v) = ep.kek_gens(o) {
            let cur = v.get("current").and_then(|c| c.as_i64()).unwrap_or(0);
            if *g != cur {
                outln!(
                    "  ⚠ STALE-GEN: team stanza for {o} is gen {g}, org current is {cur}; run\n\
                     \x20     `agit hub team rekey {o} --rekey-all` (or `agit a rekey`) to seal under the current gen."
                );
            }
        }
        match ep.get_kek_envelope(o, *g) {
            Ok(Some(_)) => {}
            Ok(None) => outln!(
                "  ⚠ you hold NO gen-{g} team-KEK envelope for {o}; you cannot open this team stanza\n\
                 \x20     (ask an admin to `agit hub team sync {o}`)."
            ),
            Err(_) => {}
        }
    }

    if !drift.has_drift() {
        outln!("  ✓ no drift: every authorized fetcher can decrypt, and every reader is authorized.");
    }
    if !drift.member_without_stanza.is_empty() {
        outln!(
            "  ✗ MEMBER-WITHOUT-STANZA (authorized to fetch, CANNOT decrypt): {}",
            drift.member_without_stanza.join(", ")
        );
    }
    if !drift.stanza_without_membership.is_empty() {
        outln!(
            "  ✗ STANZA-WITHOUT-MEMBERSHIP (holds a key, the hub REFUSES the bytes): {}",
            drift.stanza_without_membership.join(", ")
        );
        outln!(
            "    advisory: `agit a readers rm <user>` (eager rotation) if they should not read, or grant\n\
             \x20    them fetch access on the hub. `--fix` never removes a reader for you."
        );
    }

    // --fix: readers add the missing members only (never auto-remove readers).
    let mut fixed: Vec<String> = Vec::new();
    if fix && !drift.member_without_stanza.is_empty() {
        let (_kp, ring) = require_session_keyring(store)?;
        let ck = ring.current_master();
        let kid = ring.current;
        let _lock = crate::session::lock_store(store)?;
        for u in &drift.member_without_stanza {
            match crate::keybox::resolve_recipient(home, u, None, false) {
                Ok(key) => {
                    crate::keybox::append_stanza(store, &crate::keybox::user_stanza(&ck, kid, u, 0, &key)?)?;
                    fixed.push(u.clone());
                }
                Err(e) => errln!("    --fix: could not wrap the content key to {u}: {e:#}"),
            }
        }
        if !fixed.is_empty() {
            let _ = scope::git_in_status(store, &["add", "--", ".agit/keybox.jsonl"]);
            if scope::git_in_status(store, &["diff", "--cached", "--quiet"]).0 != 0 {
                crypt_commit(
                    store,
                    "chore(crypt): hub doctor --fix add authorized members as readers\n\nWraps the current content key to org members / ACL readers that had no keybox stanza (append only).",
                )?;
            }
            outln!("  --fix: added {} reader stanza(s): {}", fixed.len(), fixed.join(", "));
        }
    }

    // Drift that REMAINS after the fix: the un-added members + the (never-auto-removed) extras.
    let remaining_members = drift.member_without_stanza.len() - fixed.len();
    Ok(remaining_members > 0 || !drift.stanza_without_membership.is_empty())
}

/// `agit a encrypt --team [--org <name>] [--readers a,b] [--public]` — enable PER-SESSION keybox
/// encryption wrapping the kid-0 CK under the OWNING ORG's current Team KEK (plus any named readers /
/// public). The org defaults to the session's remote owner. Errors clearly if the org has no TK yet.
fn crypt_enable_keybox_team(home: &Path, org_override: Option<&str>, readers: &[String], public: bool, yes: bool) -> Result<i32> {
    let a = agent::resolve(None)?;
    let store = a.store.clone();
    let kp = store_keyring_path(&store)?;
    if kp.exists() {
        bail!(
            "this session already has a per-session keyring at {}.\n\
             \x20      Add a team stanza with `agit a readers add --team`, or rotate with `agit a rekey`.",
            kp.display()
        );
    }
    let org = match org_override {
        Some(o) => o.to_string(),
        None => owning_owner_of(&store)?,
    };
    let ep = crate::hubapi::HubEndpoint::resolve()?;
    let gen = org_current_tk_gen(&ep, &org)?;
    let tk = obtain_tk(home, &org, gen)?;

    crypt_print_warnings();
    crypt_confirm("Enable per-session keybox (team) encryption on this store?", yes)?;

    let ck = crate::crypt::random_master()?;
    crate::crypt::init_repo_keyring_at(&kp, ck)?;

    let mut stanzas = vec![crate::keybox::team_stanza(&ck, 0, &org, gen, &tk)?];
    if public {
        stanzas.push(crate::keybox::public_stanza(&ck, 0));
    }
    for r in readers {
        let key = crate::keybox::resolve_recipient(home, r, None, false)
            .with_context(|| format!("resolving reader `{r}`"))?;
        stanzas.push(crate::keybox::user_stanza(&ck, 0, r, 0, &key)?);
        outln!("  wrapped the content key to {r}");
    }
    crate::keybox::write_keybox(&store, &stanzas)?;

    let _lock = crate::session::lock_store(&store)?;
    let _ = crypt_write_gitattributes(&store)?;
    let _ = keybox_write_gitattributes(&store)?;
    crypt_wire_filter(&store)?;

    let _ = scope::git_in_status(&store, &["add", "--", ".gitattributes", ".agit/keybox.jsonl"]);
    if scope::git_in_status(&store, &["diff", "--cached", "--quiet"]).0 != 0 {
        crypt_commit(
            &store,
            "chore(crypt): enable per-session keybox encryption (team)\n\nMints a per-session content key, wraps it under the org's team KEK, and commits the keybox.",
        )?;
    }
    let _ = scope::git_in_status(&store, &["add", "--renormalize", "--", "sessions"]);
    if scope::git_in_status(&store, &["diff", "--cached", "--quiet"]).0 != 0 {
        crypt_commit(&store, "chore(crypt): re-encrypt tracked sessions under the per-session key")?;
    }

    outln!("per-session keybox (team) encryption enabled on {} ({}) for org {org} (gen {gen}).", a.name, a.aid);
    outln!("  team members unlock after cloning with `agit crypt unlock`.");
    Ok(0)
}

/// `agit a readers <add|rm|ls>` — manage a keybox-encrypted session's reader set.
pub fn readers_cmd(args: &[String]) -> Result<i32> {
    let home = scope::agit_home()?;
    match args.first().map(|s| s.as_str()) {
        Some("add") => readers_add(&home, &args[1..]),
        Some("rm") | Some("remove") => readers_rm(&home, &args[1..]),
        Some("ls") | Some("list") | None => readers_ls(),
        Some(other) => {
            errln!("agit readers: unknown subcommand `{other}`");
            errln!("  usage: agit a readers add <user>|--public|--team [--key HEX] [--repin]  ·  rm <user>|--public  ·  ls");
            Ok(2)
        }
    }
}

/// `agit a readers add <user>|--public` — wrap the CURRENT content key to a new reader and append its
/// stanza. O(1): existing encrypted blobs are NOT re-cleaned (the keybox line is the only change).
fn readers_add(home: &Path, args: &[String]) -> Result<i32> {
    let a = agent::resolve(None)?;
    let store = a.store.clone();
    let (_kp, ring) = require_session_keyring(&store)?;
    let ck = ring.current_master();
    let kid = ring.current;

    let team = args.iter().any(|x| x == "--team");
    let public = args.iter().any(|x| x == "--public");
    let repin = args.iter().any(|x| x == "--repin");
    let key_override = flag_value(args, "--key");

    let _lock = crate::session::lock_store(&store)?;
    if team {
        // Wrap the CURRENT content key under the owning org's current Team KEK and append a team stanza.
        let org = match flag_value(args, "--org") {
            Some(o) => o,
            None => owning_owner_of(&store)?,
        };
        let ep = crate::hubapi::HubEndpoint::resolve()?;
        let gen = org_current_tk_gen(&ep, &org)?;
        let existing = crate::keybox::read_keybox(&store)?;
        if existing
            .iter()
            .any(|s| matches!(s, crate::keybox::Stanza::Team(t) if t.kid == kid && t.org == org && t.gen == gen))
        {
            outln!("already team-wrapped at kid {kid} (org {org} gen {gen}); nothing to do.");
            return Ok(0);
        }
        let tk = obtain_tk(home, &org, gen)?;
        crate::keybox::append_stanza(&store, &crate::keybox::team_stanza(&ck, kid, &org, gen, &tk)?)?;
        outln!("  wrapped the current content key (kid {kid}) to team {org} (gen {gen}); no content re-encrypted");
    } else if public {
        if crate::keybox::is_public_at(&crate::keybox::read_keybox(&store)?, kid) {
            outln!("already public at kid {kid}; nothing to do.");
            return Ok(0);
        }
        crate::keybox::append_stanza(&store, &crate::keybox::public_stanza(&ck, kid))?;
        outln!("  added a public stanza at kid {kid}");
    } else {
        let Some(user) = first_positional(args) else {
            bail!("agit a readers add: name a <user>, or pass --public");
        };
        let key = crate::keybox::resolve_recipient(home, &user, key_override.as_deref(), repin)?;
        crate::keybox::append_stanza(&store, &crate::keybox::user_stanza(&ck, kid, &user, 0, &key)?)?;
        outln!("  wrapped the current content key (kid {kid}) to {user}; no content re-encrypted");
    }

    // Commit ONLY the keybox line.
    let _ = scope::git_in_status(&store, &["add", "--", ".agit/keybox.jsonl"]);
    if scope::git_in_status(&store, &["diff", "--cached", "--quiet"]).0 != 0 {
        crypt_commit(&store, "chore(crypt): add a session reader (keybox append, no re-encryption)")?;
    }
    Ok(0)
}

/// `agit a readers rm <user>|--public` — EAGER rotation: rotate the content key to a new generation and
/// re-wrap CK' to the REMAINING readers only. The removed reader's key opens no new commits; their old
/// kid's stanza is retained, so already-committed content stays readable by them (forward-only).
fn readers_rm(home: &Path, args: &[String]) -> Result<i32> {
    let a = agent::resolve(None)?;
    let store = a.store.clone();
    let (kp, ring) = require_session_keyring(&store)?;
    let cur_kid = ring.current;

    let drop_public = args.iter().any(|x| x == "--public");
    let user = first_positional(args);
    if user.is_none() && !drop_public {
        bail!("agit a readers rm: name a <user>, or pass --public");
    }

    let existing = crate::keybox::read_keybox(&store)?;
    let mut remaining = crate::keybox::readers_at(&existing, cur_kid);
    let mut keep_public = crate::keybox::is_public_at(&existing, cur_kid);
    if drop_public {
        keep_public = false;
    }
    let mut removed_name = None;
    if let Some(u) = &user {
        let before = remaining.len();
        remaining.retain(|r| r != u);
        if remaining.len() != before {
            removed_name = Some(u.clone());
        }
    }

    let _lock = crate::session::lock_store(&store)?;
    // Eager rotation: mint CK' as the new current generation.
    let ck_prime = crate::crypt::random_master()?;
    let new_kid = crate::crypt::rotate_keyring_at(&kp, ck_prime)?;

    // Retain every OLD stanza (removed reader keeps their old-kid access), and re-wrap CK' to the
    // remaining readers only (+ public if it survives).
    let mut stanzas = existing;
    if keep_public {
        stanzas.push(crate::keybox::public_stanza(&ck_prime, new_kid));
    }
    for r in &remaining {
        let key = crate::keybox::resolve_recipient(home, r, None, false)
            .with_context(|| format!("re-wrapping CK' to remaining reader `{r}`"))?;
        stanzas.push(crate::keybox::user_stanza(&ck_prime, new_kid, r, 0, &key)?);
    }
    crate::keybox::write_keybox(&store, &stanzas)?;

    let _ = scope::git_in_status(&store, &["add", "--", ".agit/keybox.jsonl"]);
    if scope::git_in_status(&store, &["diff", "--cached", "--quiet"]).0 != 0 {
        crypt_commit(
            &store,
            "chore(crypt): remove a session reader (eager CK rotation)\n\nRotates the content key and re-wraps only to remaining readers; new commits seal under the new key-id, which the removed reader cannot open.",
        )?;
    }
    match (removed_name, drop_public) {
        (Some(u), _) => outln!(
            "  removed {u}: rotated to kid {new_kid}, re-wrapped to {} remaining reader(s){}",
            remaining.len(),
            if keep_public { " + public" } else { "" }
        ),
        (None, true) => outln!("  removed public access: rotated to kid {new_kid}"),
        (None, false) => outln!(
            "  {} was not a current reader; rotated to kid {new_kid} anyway (re-wrapped to {} reader(s))",
            user.as_deref().unwrap_or("(none)"),
            remaining.len()
        ),
    }
    Ok(0)
}

/// `agit a readers ls` — list the current reader set (and historical generations) of the keybox.
fn readers_ls() -> Result<i32> {
    let a = agent::resolve(None)?;
    let store = a.store.clone();
    let stanzas = crate::keybox::read_keybox(&store)?;
    if stanzas.is_empty() {
        outln!("no keybox; {} ({}) is not per-session encrypted.", a.name, a.aid);
        return Ok(0);
    }
    let cur = crate::crypt::repo_keyring_path_from(&store)
        .and_then(|p| crate::crypt::load_keyring_at(&p).ok().flatten())
        .map(|r| r.current);
    let kids: std::collections::BTreeSet<u32> = stanzas.iter().map(|s| s.kid()).collect();
    outln!("keybox for {} ({})", a.name, a.aid);
    for kid in kids {
        let readers = crate::keybox::readers_at(&stanzas, kid);
        let public = crate::keybox::is_public_at(&stanzas, kid);
        let teams = crate::keybox::teams_at(&stanzas, kid);
        let marker = if Some(kid) == cur { " (current)" } else { "" };
        let mut parts = readers.clone();
        for (org, gen) in &teams {
            parts.push(format!("team:{org}@{gen}"));
        }
        let who = if parts.is_empty() { "-".to_string() } else { parts.join(", ") };
        outln!("  kid {kid}{marker}: {who}{}", if public { " [public]" } else { "" });
    }
    Ok(0)
}

/// `agit a rekey` — rotate the content key and re-wrap CK' to ALL current readers.
pub fn rekey_cmd(_args: &[String]) -> Result<i32> {
    let home = scope::agit_home()?;
    let a = agent::resolve(None)?;
    let store = a.store.clone();
    let (kp, ring) = require_session_keyring(&store)?;
    let cur_kid = ring.current;
    let existing = crate::keybox::read_keybox(&store)?;
    let readers = crate::keybox::readers_at(&existing, cur_kid);
    let keep_public = crate::keybox::is_public_at(&existing, cur_kid);

    let _lock = crate::session::lock_store(&store)?;
    let ck_prime = crate::crypt::random_master()?;
    let new_kid = crate::crypt::rotate_keyring_at(&kp, ck_prime)?;

    let mut stanzas = existing;
    if keep_public {
        stanzas.push(crate::keybox::public_stanza(&ck_prime, new_kid));
    }
    for r in &readers {
        let key = crate::keybox::resolve_recipient(&home, r, None, false)?;
        stanzas.push(crate::keybox::user_stanza(&ck_prime, new_kid, r, 0, &key)?);
    }
    crate::keybox::write_keybox(&store, &stanzas)?;

    let _ = scope::git_in_status(&store, &["add", "--", ".agit/keybox.jsonl"]);
    if scope::git_in_status(&store, &["diff", "--cached", "--quiet"]).0 != 0 {
        crypt_commit(
            &store,
            "chore(crypt): rekey the session content key\n\nRotates CK and re-wraps to all current readers; new commits seal under the new key-id.",
        )?;
    }
    outln!(
        "rekeyed to kid {new_kid}: re-wrapped to {} reader(s){}.",
        readers.len(),
        if keep_public { " + public" } else { "" }
    );
    Ok(0)
}

// ─────────────────────── purge-history: rewrite the store so no plaintext survives in history ───────────────────────

/// A yes/no gate for the DESTRUCTIVE purge, honoured by `--yes` non-interactively and refusing (never
/// hanging) when it cannot ask. Returns whether the caller confirmed.
fn purge_confirm(prompt: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !ui::interactive() {
        bail!("{prompt}\n  refusing without confirmation; re-run with --yes to proceed non-interactively");
    }
    out!("{prompt} [y/N] ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).context("reading confirmation")?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// Is `git filter-repo` runnable? (`git filter-repo --version` exits 0.) Detected so we prefer it over the
/// slower, gotcha-laden `git filter-branch`, falling back to filter-branch (always present) when absent.
fn filter_repo_available() -> bool {
    std::process::Command::new("git")
        .args(["filter-repo", "--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Internal, git-invoked helper: the `--index-filter` body of the filter-branch purge backend. Re-seals
/// every `sessions/**` blob in the CURRENT index (`GIT_INDEX_FILE`) through the clean filter under the
/// current keyring, so no historical revision of a crypt-filtered path retains plaintext. Blobs already
/// ciphertext (carrying the AGITCRYPT magic) are left byte-for-byte untouched, and NOTHING outside
/// `sessions/**` is read or written — the keybox at `.agit/keybox.jsonl` must stay plaintext for the
/// unlock bootstrap. git runs this with `GIT_DIR` + `GIT_INDEX_FILE` in the environment, so every inner
/// `git` call inherits them (no `-C`, no cwd assumptions). A failure exits nonzero so filter-branch aborts
/// the rewrite rather than dropping a session from history.
pub fn crypt_purge_index() -> Result<i32> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let keys = crate::crypt::keys_for_filter(&scope::agit_home()?)?;

    // NUL-delimited index entries under sessions/: "<mode> <sha> <stage>\t<path>\0".
    let listed = Command::new("git")
        .args(["ls-files", "-s", "-z", "--", "sessions"])
        .output()
        .context("crypt-purge-index: git ls-files")?;
    if !listed.status.success() {
        bail!(
            "crypt-purge-index: git ls-files failed: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }

    let mut updates: Vec<(String, String, String)> = Vec::new(); // (mode, new-sha, path)
    for entry in listed.stdout.split(|&b| b == 0) {
        if entry.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(entry) else { continue };
        let Some((meta, path)) = text.split_once('\t') else { continue };
        let mut it = meta.split_whitespace();
        let (Some(mode), Some(sha)) = (it.next(), it.next()) else { continue };

        // Read the blob bytes and skip anything already sealed (convergent + idempotent, but skipping
        // keeps a rotated key's retired-key blobs intact rather than re-sealing them under the new key).
        let blob = Command::new("git")
            .args(["cat-file", "blob", sha])
            .output()
            .context("crypt-purge-index: git cat-file")?;
        if !blob.status.success() {
            bail!("crypt-purge-index: cat-file {sha} failed");
        }
        if crate::crypt::is_ciphertext(&blob.stdout) {
            continue;
        }

        let sealed = crate::crypt::seal(&keys, &blob.stdout);
        let mut child = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("crypt-purge-index: git hash-object")?;
        child
            .stdin
            .take()
            .context("crypt-purge-index: hash-object stdin")?
            .write_all(&sealed)?;
        let ho = child.wait_with_output()?;
        if !ho.status.success() {
            bail!("crypt-purge-index: git hash-object failed");
        }
        let new_sha = String::from_utf8_lossy(&ho.stdout).trim().to_string();
        updates.push((mode.to_string(), new_sha, path.to_string()));
    }

    for (mode, new_sha, path) in &updates {
        // Classic three-argument cacheinfo form so a path containing a comma cannot be mis-split.
        let st = Command::new("git")
            .args(["update-index", "--cacheinfo", mode, new_sha, path])
            .status()
            .context("crypt-purge-index: git update-index")?;
        if !st.success() {
            bail!("crypt-purge-index: git update-index failed for {path}");
        }
    }
    Ok(0)
}

/// `agit a purge-history [--yes]` — guard-railed history rewrite that re-encrypts EVERY historical
/// revision of `sessions/**` so no plaintext of an encrypted transcript survives in ANY commit.
///
/// `agit a encrypt` is forward-only: commits made before encryption was enabled still hold plaintext
/// blobs that `git cat-file` recovers from history. This scrubs that by re-running the clean filter across
/// history under the current keyring. It NEVER auto-pushes; it prints the exact force-push command(s) for
/// the user to run after they have reviewed the rewrite.
pub fn purge_history_cmd(args: &[String]) -> Result<i32> {
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let a = agent::resolve(None)?;
    let store = a.store.clone();

    // ── PRECONDITION 1: the session MUST be per-session encrypted (repo-local keyring + a keybox). ──
    // Without both there is no keyring to seal under and nothing this command can purge.
    let kp = store_keyring_path(&store)?;
    let has_keyring = crate::crypt::load_keyring_at(&kp)?.is_some();
    let has_keybox = !crate::keybox::read_keybox(&store)?.is_empty();
    if !has_keyring || !has_keybox {
        bail!(
            "nothing to purge: this session is not per-session encrypted.\n\
             \x20      purge-history re-seals sessions/** under the repo-local keyring, which needs a keybox\n\
             \x20      + keyring. Enable it first, then re-run:\n\
             \x20        agit a encrypt --readers <a,b>   wrap the content key to specific people\n\
             \x20        agit a encrypt --public          readable to anyone who has the repo\n\
             \x20        agit a encrypt --team            readable to your org's team"
        );
    }

    // ── PRECONDITION 2: a history rewrite needs a CLEAN working tree. ──
    let (code, status) = scope::git_in_status(&store, &["status", "--porcelain"]);
    if code != 0 {
        bail!("could not read git status of {} (is it a git repo?)", store.display());
    }
    if !status.trim().is_empty() {
        bail!(
            "the store working tree is dirty; a history rewrite needs a clean tree.\n\
             \x20      Commit or discard changes in {} first, then re-run. Outstanding:\n{}",
            store.display(),
            status
        );
    }

    // ── LOUD WARNINGS + CONFIRMATION (honoured by --yes; refuses rather than hangs when it cannot ask). ──
    errln!("agit a purge-history; DESTRUCTIVE, IRREVERSIBLE rewrite of this store's ENTIRE git history.");
    errln!(
        "  It re-runs every commit's sessions/** through the encryption clean filter so pre-encryption\n\
         \x20     plaintext (and blobs under retired keys) no longer exist in ANY commit. Consequences:\n\
         \x20       • EVERY commit SHA in this store changes; this is a full history rewrite;\n\
         \x20       • you MUST force-push afterwards, and every existing clone must RE-CLONE (not pull);\n\
         \x20       • provenance signatures / verdicts recorded against the OLD commit SHAs may need\n\
         \x20         re-verification;\n\
         \x20       • it cannot be undone except from a backup. BACK UP THE STORE FIRST."
    );
    if !purge_confirm("Rewrite history and purge sessions/** plaintext now?", yes)? {
        errln!("aborted; history not rewritten.");
        return Ok(1);
    }

    // Capture the pieces we need to report BEFORE the rewrite: filter-repo strips remotes, and the branch
    // ref is what the force-push targets.
    let (_bc, branch) = scope::git_in_status(&store, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let branch = {
        let b = branch.trim();
        if b.is_empty() || b == "HEAD" { "main".to_string() } else { b.to_string() }
    };
    let remotes = agent::store_remotes(&store);

    let _lock = crate::session::lock_store(&store)?;

    // ── THE REWRITE ── prefer git filter-repo; else fall back to the always-present git filter-branch.
    let exe = std::env::current_exe().context("could not locate agit's own path")?;
    let backend = if filter_repo_available() {
        run_filter_repo_purge(&store, &exe)?;
        "git filter-repo"
    } else {
        run_filter_branch_purge(&store, &exe)?;
        "git filter-branch"
    };

    // Leave the working tree checked out AND decryptable: reset to the rewritten HEAD so the smudge filter
    // recovers the original plaintext from the now-ciphertext blobs (purge changed only what is STORED in
    // history, not the plaintext you see). The tree was clean (precondition), so nothing is lost.
    let _ = scope::git_in_status(&store, &["reset", "--hard"]);

    outln!(
        "history rewritten via {backend}: every sessions/** revision re-encrypted under the current keyring."
    );
    outln!("  the working tree is still decryptable; checkout runs the smudge filter as before.");

    // ── DO NOT auto-push. Print the exact force-push command(s) for the user to run after reviewing. ──
    outln!("  NOT pushed. Review the rewrite, then force-push yourself:");
    if remotes.is_empty() {
        outln!(
            "    (no remote configured; add one, then) git -C {} push --force <remote> {branch}",
            store.display()
        );
    } else {
        for (name, _url) in &remotes {
            outln!("    git -C {} push --force {name} {branch}", store.display());
        }
    }
    errln!(
        "  ⚠ every teammate must RE-CLONE; a pull cannot reconcile a rewritten history; and any\n\
         \x20     provenance signatures / verdicts recorded against the old commit SHAs may need\n\
         \x20     re-verification."
    );
    Ok(0)
}

/// The filter-branch purge backend (always available). Its `--index-filter` re-stages sessions/** through
/// `agit crypt-purge-index`, so the crypt filter + keyring are in force for every historical revision.
/// `-f` overwrites any leftover refs/original from a prior run; the squelch var quiets filter-branch's
/// standard scary banner (we print our own, more specific warnings above).
fn run_filter_branch_purge(store: &Path, exe: &Path) -> Result<()> {
    use std::process::Command;

    // Neutralize benign filter-phantom diffs before filter-branch's clean-work-tree gate runs. git skips
    // clean/smudge filters on ZERO-LENGTH files, so a tracked empty file under the sessions/** crypt filter
    // (e.g. the store's `sessions/.gitkeep`, committed as ciphertext) reads as perpetually modified to
    // `git diff-files` even though `git status` (the caller's precondition) considers the tree clean. Mark
    // those phantom paths assume-unchanged for the rewrite, then clear the flag afterward.
    let (_c, phantom) = scope::git_in_status(store, &["diff-files", "--name-only", "-z"]);
    let phantom_paths: Vec<String> =
        phantom.split('\0').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
    for p in &phantom_paths {
        let _ = scope::git_in_status(store, &["update-index", "--assume-unchanged", "--", p]);
    }

    let index_filter = format!(
        "{} crypt-purge-index",
        crate::init::sh_single_quote(&exe.to_string_lossy())
    );
    let status = Command::new("git")
        .arg("-C")
        .arg(store)
        .args(["filter-branch", "-f", "--index-filter", &index_filter, "--", "--all"])
        .env("FILTER_BRANCH_SQUELCH_WARNING", "1")
        .status()
        .context("running git filter-branch");

    // Clear the assume-unchanged bits regardless of the rewrite's outcome, so normal operation resumes.
    for p in &phantom_paths {
        let _ = scope::git_in_status(store, &["update-index", "--no-assume-unchanged", "--", p]);
    }

    let status = status?;
    if !status.success() {
        bail!(
            "git filter-branch exited {}; history was NOT rewritten.",
            status.code().unwrap_or(-1)
        );
    }

    // filter-branch keeps the PRE-rewrite commits under refs/original/ as a backup, and the reflog still
    // references them — so the old plaintext blobs stay reachable (and `git cat-file`-recoverable) until
    // they are dropped. Delete the backup refs, expire every reflog, and prune, so no historical commit
    // retains plaintext. (git filter-repo does this itself; filter-branch requires it explicitly.)
    let (_c, origs) =
        scope::git_in_status(store, &["for-each-ref", "--format=%(refname)", "refs/original/"]);
    for r in origs.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let _ = scope::git_in_status(store, &["update-ref", "-d", r]);
    }
    let _ = scope::git_in_status(store, &["reflog", "expire", "--expire=now", "--all"]);
    let _ = scope::git_in_status(store, &["gc", "--prune=now"]);
    Ok(())
}

/// The filter-repo purge backend (preferred when installed). git-filter-repo does not re-apply
/// gitattributes filters, so the blob callback seals explicitly by invoking `agit crypt-clean`. It is
/// content-safe: it leaves anything already ciphertext (AGITCRYPT magic) and anything that is not a
/// session JSONL object untouched, and it skips the keybox — whose stanzas carry a top-level `"kid"` field
/// that a transcript's first object never has — so the wrap-ciphertext keybox is never double-encrypted.
/// `--force` because filter-repo refuses on a non-fresh clone; this is an explicit, confirmed rewrite.
fn run_filter_repo_purge(store: &Path, exe: &Path) -> Result<()> {
    use std::process::Command;
    let callback = "import os, subprocess\n\
        MAGIC = b\"AGITCRYPT\\x00\"\n\
        d = blob.data\n\
        first = d.split(b\"\\n\", 1)[0]\n\
        if (not d.startswith(MAGIC)) and d[:1] == b\"{\" and b'\"kid\"' not in first:\n\
        \x20   blob.data = subprocess.run([os.environ[\"AGIT_SELF\"], \"crypt-clean\"], input=d, stdout=subprocess.PIPE, check=True).stdout\n";
    let status = Command::new("git")
        .arg("-C")
        .arg(store)
        .args(["filter-repo", "--force", "--blob-callback", callback])
        .env("AGIT_SELF", exe)
        .status()
        .context("running git filter-repo")?;
    if !status.success() {
        bail!(
            "git filter-repo exited {}; history was NOT rewritten.",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

/// `agit crypt <unlock>` — the per-session keybox client verb.
pub fn crypt_cmd(args: &[String]) -> Result<i32> {
    match args.first().map(|s| s.as_str()) {
        Some("unlock") => crypt_unlock(),
        None => {
            errln!("usage: agit crypt unlock");
            Ok(2)
        }
        Some(other) => {
            errln!("agit crypt: unknown subcommand `{other}`");
            errln!("  usage: agit crypt unlock");
            Ok(2)
        }
    }
}

/// `agit a escrow <enable>` — the OPT-IN hub-assist escrow verb (encryption-recipients Wave 5, feature 2).
pub fn escrow_cmd(args: &[String]) -> Result<i32> {
    match args.first().map(|s| s.as_str()) {
        Some("enable") => escrow_enable(),
        None => {
            errln!("usage: agit a escrow enable");
            Ok(2)
        }
        Some(other) => {
            errln!("agit a escrow: unknown subcommand `{other}`");
            errln!("  usage: agit a escrow enable");
            Ok(2)
        }
    }
}

/// `agit a escrow enable` — wrap this session's content key(s) under the HUB escrow key and upload them, so
/// the hub can later RELEASE them to an ACL reader (`agit crypt unlock` fallback). ONLY permitted when the
/// owning org is in `escrow_mode = 'hub-assist'` (fail-closed: a session never escrows otherwise). This
/// RE-TRUSTS the hub — it is the one path that gives retroactive-for-unfetched revocation and hub-backed
/// recovery. The hub only ever stores CIPHERTEXT (CK sealed to its escrow PUBLIC key).
fn escrow_enable() -> Result<i32> {
    let a = agent::resolve(None)?;
    let store = a.store.clone();
    let (owner, name) = owner_and_name_of(&store)?;
    let ep = crate::hubapi::HubEndpoint::resolve()?;
    // Hub-assist must be ON for the owning org — a session never escrows unless the org opted in.
    let org_json = ep
        .get_org(&owner)
        .with_context(|| format!("cannot read owning org `{owner}`"))?
        .with_context(|| format!("owning org `{owner}` is not visible to you (are you a member?)"))?;
    let mode = org_json.get("escrow_mode").and_then(|m| m.as_str()).unwrap_or("none");
    if mode != "hub-assist" {
        bail!(
            "hub-assist escrow is not enabled for org `{owner}` (mode = {mode}).\n\
             \x20      An org owner must run `agit hub org escrow {owner} --mode hub-assist` first; this\n\
             \x20      RE-TRUSTS the hub, so it is a deliberate per-org decision."
        );
    }
    // This session must be keybox-encrypted (there must be a content key to escrow).
    let (_kp, ring) = require_session_keyring(&store)?;
    // Seal every content-key generation to the hub escrow pubkey and upload, so ALL kids are releasable.
    let hexpub = ep.escrow_pubkey()?;
    let pubkey = crate::keybox::decode_x25519_hex(&hexpub).context("the hub returned an invalid escrow pubkey")?;
    let mut escrowed = 0usize;
    for k in &ring.keys {
        let wrapped = crate::keybox::seal_tk_for_member(&k.master, &pubkey)?;
        ep.post_escrow_key(&owner, &name, k.id, &wrapped)
            .with_context(|| format!("uploading escrowed content key kid {}", k.id))?;
        escrowed += 1;
    }
    outln!("hub-assist escrow enabled for {} ({}): escrowed {escrowed} content-key generation(s).", a.name, a.aid);
    outln!("  ⚠ this RE-TRUSTS the hub: it can now RELEASE these keys to any caller the ACL lets read this");
    outln!("    session (they unlock via `agit crypt unlock`). Remove that reader from the ACL and the hub");
    outln!("    refuses future release; the one path with retroactive-for-unfetched revocation.");
    Ok(0)
}

/// Wave-5 FALLBACK for `agit crypt unlock`: ask the hub to RELEASE this session's escrowed content keys
/// (hub-assist escrow). Works only when the owning org is in hub-assist mode AND the caller passes the
/// hub's `acl::decide(_, Read)` gate — the hub is fail-closed. Returns a keyring built from the released
/// keys, or `None` when the hub releases nothing (not a hub-assist session, not permitted, none escrowed).
fn try_hub_assist_release(store: &Path) -> Result<Option<crate::crypt::Keyring>> {
    let Ok((owner, name)) = owner_and_name_of(store) else {
        return Ok(None); // no hub remote → no hub-assist path
    };
    let Ok(ep) = crate::hubapi::HubEndpoint::resolve() else {
        return Ok(None);
    };
    let Some(released) = ep.release_keys(&owner, &name)? else {
        return Ok(None); // 403/404 → the hub refused (fail-closed), no keyring
    };
    let arr = released.get("released").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    let mut keys: Vec<crate::crypt::KeyringEntry> = Vec::new();
    for e in &arr {
        let (Some(kid), Some(ck_hex)) = (e.get("kid").and_then(|k| k.as_i64()), e.get("ck").and_then(|c| c.as_str())) else {
            continue;
        };
        let Some(raw) = hex::decode(ck_hex.trim()).ok().filter(|b| b.len() == 32) else {
            continue;
        };
        let master: [u8; 32] = raw.try_into().expect("checked length 32");
        keys.push(crate::crypt::KeyringEntry { id: kid as u32, master });
    }
    if keys.is_empty() {
        return Ok(None);
    }
    let current = keys.iter().map(|k| k.id).max().expect("non-empty");
    Ok(Some(crate::crypt::Keyring { current, keys }))
}

/// `agit crypt unlock` — recover this machine's content keys from the committed keybox and write them to
/// the repo-local keyring, so the crypt filter can decrypt the session. FAIL-CLOSED: if I am not a reader
/// (no stanza opens with my identity), NO keyring is written and the smudge path stays locked — never a
/// silent plaintext leak.
fn crypt_unlock() -> Result<i32> {
    let a = agent::resolve(None)?;
    let store = a.store.clone();
    let stanzas = crate::keybox::read_keybox(&store)?;
    if stanzas.is_empty() {
        outln!(
            "no keybox at {}; nothing to unlock (this session is not per-session encrypted).",
            crate::keybox::keybox_path(&store).display()
        );
        return Ok(0);
    }
    let home = scope::agit_home()?;
    let sk = agent::machine_signing_key()?;
    let secret = agent::derive_x25519_secret(&sk);
    // Team stanzas resolve their content key through the org's Team KEK: the local TK cache, else a
    // fetch+unwrap of my own team_keks envelope. Fail-closed — an unavailable TK contributes nothing.
    let ring = match crate::keybox::recover_keyring_with(&stanzas, &secret, |org, gen| obtain_tk(&home, org, gen).ok()) {
        Some(r) => r,
        // Wave-5 FALLBACK: a hub-assist session may still unlock by asking the hub to RELEASE its content
        // keys to an ACL reader, even when no keybox stanza opens with this machine's identity. The hub is
        // fail-closed (only releases what `acl::decide(_, Read)` allows); if it releases nothing, we keep
        // the original fail-closed error and write NO keyring.
        None => match try_hub_assist_release(&store)? {
            Some(r) => {
                outln!("  no keybox stanza opened with this machine; unlocked via hub-assist escrow release");
                outln!("  (the hub RE-TRUSTS path: the hub released the content key under the ACL Read gate).");
                r
            }
            None => bail!(
                "you are not a reader of this encrypted session; none of the {} keybox stanza(s) open with\n\
                 \x20      this machine's identity (nor a team KEK you can obtain, nor a hub-assist escrow\n\
                 \x20      release). The keyring was NOT written; the session stays locked. Publish your key\n\
                 \x20      (agit identity register <you>, pasted into the hub) and ask a reader to `agit a readers add <you>`, or join the\n\
                 \x20      owning org and run `agit hub team sync <org>`.",
                stanzas.len()
            ),
        },
    };
    let kp = store_keyring_path(&store)?;
    let recovered = ring.keys.len();
    let current = ring.current;
    crate::crypt::save_keyring_at(&kp, &ring)?;
    crypt_wire_filter(&store)?;
    // Re-checkout sessions so any ciphertext in the working tree becomes plaintext under the recovered keys.
    let (code, _) = scope::git_in_status(&store, &["checkout", "--", "sessions"]);
    if code != 0 {
        errln!("  ⚠ wrote the keyring but could not re-checkout sessions/**; run `git checkout -- .` in the store");
    }
    outln!("unlocked {} ({}): recovered {recovered} content key(s), current kid {current}.", a.name, a.aid);
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

    // ── registry attribution: "signed by this key" → "verified as this person" ──

    fn reg(username: &str, ed25519_pub: &str) -> RegisteredIdentity {
        RegisteredIdentity { username: username.into(), ed25519_keys: vec![ed25519_pub.into()] }
    }

    fn reg_set(username: &str, keys: &[&str]) -> RegisteredIdentity {
        RegisteredIdentity { username: username.into(), ed25519_keys: keys.iter().map(|k| k.to_string()).collect() }
    }

    /// The committer email maps to a registered account whose key IS the one that signed the session →
    /// `VerifiedAs`. The only "verified as a person" verdict.
    #[test]
    fn a_matching_registered_key_verifies_as_the_person() {
        let home = tempfile::tempdir().unwrap();
        let k = key(home.path());
        let pubkey = hex::encode(k.verifying_key().to_bytes());
        let content = "transcript\n";
        let p = sign_provenance(&k, content, "agt_01", "alice@corp.com", "t0");

        let status = verify_provenance_with_registry(home.path(), content, Some(&p), false, |email| {
            assert_eq!(email, "alice@corp.com");
            Ok(Some(reg("alice", &pubkey)))
        })
        .unwrap();
        match status {
            ProvenanceStatus::VerifiedAs { username, aid, email, .. } => {
                assert_eq!(username, "alice");
                assert_eq!(aid, "agt_01");
                assert_eq!(email, "alice@corp.com");
            }
            other => panic!("expected VerifiedAs, got {other:?}"),
        }
    }

    /// Match-ANY: an account with SEVERAL registered device keys verifies-as the person when the session's
    /// signing key equals ANY of them — a session signed on a second enrolled machine is NOT a false
    /// mismatch. This is the whole point of the SSH-keys reshape.
    #[test]
    fn a_session_signed_by_any_registered_device_key_verifies_as_the_person() {
        let home = tempfile::tempdir().unwrap();
        let signer = key(home.path());
        let signing_pubkey = hex::encode(signer.verifying_key().to_bytes());
        let content = "transcript\n";
        let p = sign_provenance(&signer, content, "agt_01", "alice@corp.com", "t0");
        // alice has two device keys registered; the SECOND is this machine's — a match on it must verify.
        let other_device = "11".repeat(32);

        let status = verify_provenance_with_registry(home.path(), content, Some(&p), false, |_email| {
            Ok(Some(reg_set("alice", &[&other_device, &signing_pubkey])))
        })
        .unwrap();
        match status {
            ProvenanceStatus::VerifiedAs { username, .. } => assert_eq!(username, "alice"),
            other => panic!("a match on ANY registered device key must be VerifiedAs, got {other:?}"),
        }
    }

    /// The anti-forgery property: the email maps to a registered account, but the signing key matches NONE
    /// of its device keys → `KeyMismatch`, never `VerifiedAs`. This is the impersonation case.
    #[test]
    fn a_differing_registered_key_is_a_key_mismatch_not_verified() {
        let home = tempfile::tempdir().unwrap();
        let signer = key(home.path());
        let content = "transcript\n";
        let p = sign_provenance(&signer, content, "agt_01", "alice@corp.com", "t0");
        // alice's registered key is some OTHER key, not the one that signed this session.
        let alices_real_key = "22".repeat(32);

        let status = verify_provenance_with_registry(home.path(), content, Some(&p), false, |_email| {
            Ok(Some(reg("alice", &alices_real_key)))
        })
        .unwrap();
        assert!(!status.is_verified(), "a key mismatch is NOT a pass: {status:?}");
        match status {
            ProvenanceStatus::KeyMismatch { claimed_username, registered_pubkey, actual_pubkey, .. } => {
                assert_eq!(claimed_username, "alice");
                assert_eq!(registered_pubkey, alices_real_key);
                assert_eq!(actual_pubkey, p.pubkey);
            }
            other => panic!("expected KeyMismatch, got {other:?}"),
        }
    }

    /// The email maps to no registered account → `SignedUnregistered`: the signature is internally
    /// consistent, but there is nobody to attribute it to. Falls back to self-verify meaning.
    #[test]
    fn an_unregistered_email_is_signed_unregistered() {
        let home = tempfile::tempdir().unwrap();
        let k = key(home.path());
        let content = "transcript\n";
        let p = sign_provenance(&k, content, "agt_01", "nobody@corp.com", "t0");

        let status =
            verify_provenance_with_registry(home.path(), content, Some(&p), false, |_email| Ok(None)).unwrap();
        assert!(matches!(status, ProvenanceStatus::SignedUnregistered { .. }), "{status:?}");
        assert!(status.is_verified(), "self-verify still holds; it is just unattributed");
    }

    /// Offline / no hub reachable (the lookup errors) → `SignedUnregistered`, NEVER a false "verified as".
    #[test]
    fn offline_falls_back_to_signed_unregistered_never_verified_as() {
        let home = tempfile::tempdir().unwrap();
        let k = key(home.path());
        let content = "transcript\n";
        let p = sign_provenance(&k, content, "agt_01", "alice@corp.com", "t0");

        let status = verify_provenance_with_registry(home.path(), content, Some(&p), false, |_email| {
            anyhow::bail!("no hub reachable")
        })
        .unwrap();
        assert!(matches!(status, ProvenanceStatus::SignedUnregistered { .. }), "{status:?}");
        assert!(!status.is_attributed(), "an offline verify must never positively attribute");
    }

    /// A tampered transcript stays `ContentTampered` and a bad signature stays `BadSignature` even on the
    /// registry path — the registry is never consulted for a session that does not self-verify.
    #[test]
    fn registry_path_preserves_tamper_and_bad_signature() {
        let home = tempfile::tempdir().unwrap();
        let k = key(home.path());
        let p = sign_provenance(&k, "original\n", "agt_01", "alice@corp.com", "t0");
        let called = std::cell::Cell::new(false);

        let tampered = verify_provenance_with_registry(home.path(), "edited\n", Some(&p), false, |_e| {
            called.set(true);
            Ok(Some(reg("alice", &p.pubkey)))
        })
        .unwrap();
        assert!(matches!(tampered, ProvenanceStatus::ContentTampered { .. }), "{tampered:?}");
        assert!(!called.get(), "a session that does not self-verify must never hit the registry");

        let mut bad = sign_provenance(&k, "c\n", "agt_01", "alice@corp.com", "t0");
        bad.sig = "00".repeat(64);
        let badsig =
            verify_provenance_with_registry(home.path(), "c\n", Some(&bad), false, |_e| Ok(None)).unwrap();
        assert_eq!(badsig, ProvenanceStatus::BadSignature);
    }

    /// TOFU: once a person's registered key is pinned, a CHANGED registered key HARD-FAILS the verify
    /// (an `Err` with a re-pin instruction) — a hub cannot silently swap the key it attributes sessions
    /// to. `--repin` accepts the new key.
    #[test]
    fn a_changed_registered_key_hard_fails_until_repinned() {
        let home = tempfile::tempdir().unwrap();
        let k = key(home.path());
        let pubkey = hex::encode(k.verifying_key().to_bytes());
        let content = "transcript\n";
        let p = sign_provenance(&k, content, "agt_01", "alice@corp.com", "t0");

        // First sighting pins alice's registered key.
        let first = verify_provenance_with_registry(home.path(), content, Some(&p), false, |_e| {
            Ok(Some(reg("alice", &pubkey)))
        })
        .unwrap();
        assert!(matches!(first, ProvenanceStatus::VerifiedAs { .. }), "{first:?}");

        // The hub now hands back a DIFFERENT registered key for alice — a possible key-substitution.
        let rotated = "33".repeat(32);
        let err = verify_provenance_with_registry(home.path(), content, Some(&p), false, |_e| {
            Ok(Some(reg("alice", &rotated)))
        })
        .unwrap_err();
        assert!(err.to_string().contains("--repin"), "the hard failure must instruct a re-pin: {err}");

        // With --repin, the new key is accepted (and this session then mismatches it, as its signer key
        // is the old one — a key-mismatch, correctly, not a silent pass).
        let after = verify_provenance_with_registry(home.path(), content, Some(&p), true, |_e| {
            Ok(Some(reg("alice", &rotated)))
        })
        .unwrap();
        assert!(matches!(after, ProvenanceStatus::KeyMismatch { .. }), "{after:?}");
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
            "a clone must erase the content-age gap (these are 6 years apart by content), got {gap:?}; \
             otherwise this test proves nothing"
        );

        let latest = latest_session(&clone).expect("a cloned store still has sessions");
        assert!(
            latest.path.ends_with("new.jsonl"),
            "picked {:?}; ordering fell back to the filesystem, which a clone has erased",
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

        // Ordering comes from COMMITTED recency (the sidecars), not the filesystem: `new` is the latest
        // because its recorded last_activity is later, regardless of write order or mtime.
        write(&s.join("web/claude-code/old.agit.json"), "{\"last_activity\":\"2026-07-16T08:00:00.000Z\"}\n");
        write(&s.join("api/codex/new.agit.json"), "{\"last_activity\":\"2026-07-16T10:00:00.000Z\"}\n");
        // mtime is deliberately set to DISAGREE with recorded recency, proving the fs no longer decides.
        let newer = std::time::SystemTime::now();
        let older = newer - std::time::Duration::from_secs(7200);
        filetime(&s.join("web/claude-code/old.jsonl"), newer);
        filetime(&s.join("api/codex/new.jsonl"), older);

        let all = store_sessions(store.path());
        assert_eq!(all.len(), 2, "only real sessions: {:?}", all.iter().map(|x| &x.path).collect::<Vec<_>>());

        let latest = latest_session(store.path()).unwrap();
        assert!(latest.path.ends_with("api/codex/new.jsonl"), "picked {:?}", latest.path);
        assert_eq!(latest.runtime, "codex", "the runtime comes from the session, not a default");
        assert_eq!(latest.env_slug.as_deref(), Some("api"), "an env-partitioned store reports where it ran");
        // the newest FILE is a sidecar; it must not become the session we carry
        assert!(!latest.path.to_string_lossy().contains("subagents"));
    }

    /// Per-teammate `latest` drift (QA: `touch` an older session → it jumps to the top of `a log` with
    /// the store commit UNCHANGED). Two teammates with a byte-identical store must resolve the SAME
    /// latest, so ordering must come only from COMMITTED values — never the filesystem mtime, which a
    /// clone flattens and a `touch` reorders. When recorded recency ties (the timestamp-less case that
    /// used to be null and fall through to mtime), the committed id breaks it, so a `touch` cannot move it.
    #[test]
    fn latest_is_committed_ordered_so_a_touch_cannot_reorder_it() {
        let store = tempfile::tempdir().unwrap();
        let d = store.path();
        git(d, &["init", "-q", "-b", "main", "."]);
        let s = d.join("sessions/claude-code");

        // Two sessions that TIE on recorded recency — exactly what a pair of timestamp-less snapped
        // sessions now looks like (both floored, non-null). With only recency to go on, an mtime tiebreak
        // would let the filesystem decide; the committed id must decide instead.
        write(&s.join("aaa.jsonl"), "{}\n");
        write(&s.join("aaa.agit.json"), "{\"last_activity\":\"2026-07-16T10:00:00.000Z\"}\n");
        write(&s.join("bbb.jsonl"), "{}\n");
        write(&s.join("bbb.agit.json"), "{\"last_activity\":\"2026-07-16T10:00:00.000Z\"}\n");
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "two tied sessions"]);
        let head_before = git(d, &["rev-parse", "HEAD"]);

        let picked = latest_session(d).unwrap().path.file_stem().unwrap().to_string_lossy().into_owned();

        // Now `touch` the OTHER one to be far newer on disk. mtime ordering would flip the answer here.
        let other = if picked == "aaa" { "bbb" } else { "aaa" };
        filetime(&s.join(format!("{other}.jsonl")), std::time::SystemTime::now() + std::time::Duration::from_secs(86_400));

        let after = latest_session(d).unwrap().path.file_stem().unwrap().to_string_lossy().into_owned();
        assert_eq!(after, picked, "a touch reordered `latest`; ordering is still reading the filesystem mtime");
        assert_eq!(git(d, &["rev-parse", "HEAD"]), head_before, "no commit changed; the reorder would be purely local");
    }

    /// The other half of the same fix: a snapped session's sidecar `last_activity` is NEVER null, even
    /// when the transcript carries no timestamp — otherwise the reader falls back to mtime and the drift
    /// above returns. The floor is a stable committed value, so it does not churn a fresh commit per tick.
    #[test]
    fn a_timestampless_snapped_session_still_gets_a_committed_recency() {
        let store = tempfile::tempdir().unwrap();
        let s = store.path().join("sessions/claude-code");
        // A timestamp-less transcript, and its sidecar written with the floor (what the snap now records).
        write(&s.join("q.jsonl"), "{}\n");
        write(&s.join("q.agit.json"), &format!("{{\"last_activity\":\"{}\"}}\n", crate::session::NO_ACTIVITY_FLOOR));

        let ss = store_sessions(store.path());
        assert_eq!(ss.len(), 1);
        assert!(ss[0].last_activity.is_some(), "a floored sidecar must give a non-null, ord:able recency");
        assert_ne!(crate::session::NO_ACTIVITY_FLOOR, "", "the floor must be a real committed value, not empty/null");
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
            let id = install_id(rt, "feature-a-3f9a2c");
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

    /// The store-bloat fix: the same source id + target runtime must always resolve to the SAME id, so a
    /// re-convert overwrites its one file instead of minting a fresh rollup. A different source id (or a
    /// different target runtime) must resolve elsewhere, or a genuinely-new session would never convert.
    #[test]
    fn install_id_is_deterministic_in_source_and_runtime() {
        assert_eq!(
            install_id("codex", "src-abc"),
            install_id("codex", "src-abc"),
            "same source + runtime must be stable across passes (no fresh UUID per restart)"
        );
        assert_ne!(
            install_id("codex", "src-abc"),
            install_id("codex", "src-def"),
            "a different source must get its own id"
        );
        assert_ne!(
            install_id("codex", "src-abc"),
            install_id("claude-code", "src-abc"),
            "the same source converted to a different runtime is a different session"
        );
    }
}

#[cfg(test)]
mod wave4_tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::process::Command;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── item 1: zero-config default reader set ──

    // No hub remote → machine-global; an org owner → team; a personal owner → require explicit.
    #[test]
    fn zero_config_default_target_by_owning_scope() {
        assert_eq!(default_target(None, false), DefaultTarget::MachineGlobal, "no remote → machine-global");
        assert_eq!(
            default_target(Some("acme"), true),
            DefaultTarget::Team("acme".into()),
            "an org owner defaults to the TEAM reader set (not public, not owner-only)"
        );
        assert_eq!(
            default_target(Some("alice"), false),
            DefaultTarget::RequireExplicit("alice".into()),
            "a personal owner has no team to default to; the user must name readers"
        );
    }

    // The team default builds exactly one TEAM stanza for kid 0 — not public, not an individual owner
    // stanza. This is the concrete "readable to the team, not the public" result of a zero-config encrypt.
    #[test]
    fn zero_config_team_stanza_is_team_only_not_public_or_owner() {
        let ck = [0x5Au8; 32];
        let tk = [0x6Bu8; 32];
        // The stanza set crypt_enable_keybox_team builds with no --readers/--public: a single team stanza.
        let stanzas = vec![crate::keybox::team_stanza(&ck, 0, "acme", 1, &tk).unwrap()];
        assert_eq!(stanzas.len(), 1, "zero-config wraps to the team only");
        assert!(matches!(stanzas[0], crate::keybox::Stanza::Team(_)), "the sole stanza is a team stanza");
        assert!(!crate::keybox::is_public_at(&stanzas, 0), "zero-config is NOT public");
        assert!(crate::keybox::readers_at(&stanzas, 0).is_empty(), "zero-config wraps to NO individual owner");
        assert_eq!(crate::keybox::teams_at(&stanzas, 0), vec![("acme".to_string(), 1)], "team acme@1");
    }

    // An org with NO Team KEK yet is an ACTIONABLE error, never a silent fall-back to owner-only/public.
    #[test]
    fn zero_config_org_without_tk_errors_actionably() {
        let err = require_tk_gen(0, "acme").unwrap_err().to_string();
        assert!(err.contains("no team KEK"), "must name the missing TK: {err}");
        assert!(err.contains("agit hub team rekey acme"), "must tell the user the fix: {err}");
        // A real generation is accepted.
        assert_eq!(require_tk_gen(1, "acme").unwrap(), 1);
        assert_eq!(require_tk_gen(7, "acme").unwrap(), 7);
    }

    // The committed keybox is wrap-ciphertext and MUST be skipped by the secret scanners, or its
    // high-entropy base64 refuses every keybox-encrypted commit/push (client hook + hub pre-receive).
    #[test]
    fn keybox_is_excluded_from_the_secret_scan() {
        assert!(is_self_encrypted_artifact(crate::keybox::KEYBOX_REL), "the keybox path is a self-encrypted artifact");
        assert!(is_self_encrypted_artifact(".agit/keybox.jsonl"), "the exact committed path is skipped");
        assert!(is_self_encrypted_artifact(".agit\\keybox.jsonl"), "a windows-separator path is skipped too");
        assert!(!is_self_encrypted_artifact("sessions/web/s.jsonl"), "a real session is NOT skipped");
        assert!(!is_self_encrypted_artifact(".agit/other.json"), "only the keybox is skipped, not all of .agit/");
    }

    // ── item 2: hub doctor drift reconciliation ──

    // A member authorized to fetch but with no stanza is MEMBER-WITHOUT-STANZA; a keybox reader who is not
    // authorized is STANZA-WITHOUT-MEMBERSHIP; a matched set has no drift.
    #[test]
    fn reconcile_detects_both_drift_classes_and_a_clean_case() {
        // member-without-stanza: alice+bob+carol may fetch, only alice has a stanza.
        let d = reconcile(&set(&["alice", "bob", "carol"]), &set(&["alice"]), &set(&[]), false);
        assert_eq!(d.member_without_stanza, vec!["bob".to_string(), "carol".to_string()]);
        assert!(d.stanza_without_membership.is_empty());
        assert!(d.has_drift());

        // stanza-without-membership: only alice may fetch, but mallory also holds a stanza.
        let d = reconcile(&set(&["alice"]), &set(&["alice", "mallory"]), &set(&[]), false);
        assert!(d.member_without_stanza.is_empty());
        assert_eq!(d.stanza_without_membership, vec!["mallory".to_string()]);
        assert!(d.has_drift());

        // clean: the authorized set and the reader set match exactly → no drift, so --check exits zero.
        let d = reconcile(&set(&["alice", "bob"]), &set(&["alice", "bob"]), &set(&[]), false);
        assert!(!d.has_drift(), "a matched ACL/keybox has no drift");
    }

    // A team stanza covers the org's members: an org member is NOT member-without-stanza when a team
    // stanza covers them, and a public stanza covers every authorized fetcher.
    #[test]
    fn reconcile_team_and_public_coverage() {
        // bob has no individual stanza but the team covers him → no member-without-stanza.
        let d = reconcile(&set(&["alice", "bob"]), &set(&["alice"]), &set(&["alice", "bob"]), false);
        assert!(!d.has_drift(), "a team stanza covers the org members: {d:?}");

        // public: even with no individual/team coverage, everyone authorized can decrypt.
        let d = reconcile(&set(&["alice", "bob", "carol"]), &set(&[]), &set(&[]), true);
        assert!(d.member_without_stanza.is_empty(), "public covers every authorized fetcher");
    }

    // ── item 3: team rekey --rekey-all rotates a local team session's CK under the new gen ──

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    #[test]
    fn rekey_all_rotates_a_local_team_session_ck_under_the_new_gen() {
        let home = tempfile::tempdir().unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = repo_dir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        // A committer identity local to THIS repo (never global — see the repo's test hygiene).
        git(repo, &["config", "user.email", "t@t"]);
        git(repo, &["config", "user.name", "t"]);

        // A team-only session: keyring gen 0 with CK0, keybox wrapping CK0 under TK gen 1 for org acme.
        let ck0 = [0x11u8; 32];
        let tk1 = [0xA1u8; 32];
        let kp = crate::crypt::repo_keyring_path_from(repo).expect("repo-local keyring path");
        crate::crypt::init_repo_keyring_at(&kp, ck0).unwrap();
        crate::keybox::write_keybox(repo, &[crate::keybox::team_stanza(&ck0, 0, "acme", 1, &tk1).unwrap()]).unwrap();
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-qm", "team session"]);

        // Rotate: mint TK gen 2 and rekey this one session under it (the --rekey-all inner step).
        let tk2 = [0xA2u8; 32];
        let new_kid = rekey_one_team_session(home.path(), repo, "acme", 2, &tk2).unwrap();
        assert!(new_kid > 0, "the content key rotated to a new generation");

        let stanzas = crate::keybox::read_keybox(repo).unwrap();
        let ck_prime = crate::crypt::load_keyring_at(&kp).unwrap().unwrap().current_master();
        assert_ne!(ck_prime, ck0, "the content key was actually rotated");

        // The new team stanza is at the new kid under gen 2, and TK gen 2 opens it to recover CK'.
        assert_eq!(
            crate::keybox::teams_at(&stanzas, new_kid),
            vec![("acme".to_string(), 2)],
            "the session is re-wrapped under the NEW team-KEK generation"
        );
        let team_new = stanzas
            .iter()
            .find_map(|s| match s {
                crate::keybox::Stanza::Team(t) if t.kid == new_kid => Some(t.clone()),
                _ => None,
            })
            .expect("a team stanza at the new kid");
        assert_eq!(
            crate::keybox::unwrap_ck_with_tk(&team_new, &tk2).unwrap(),
            ck_prime,
            "the new-gen TK opens the new team stanza to the rotated CK'"
        );
        // The old gen-1 stanza is RETAINED (forward-only: past readers keep past access).
        assert_eq!(crate::keybox::teams_at(&stanzas, 0), vec![("acme".to_string(), 1)], "gen-1 stanza kept");
    }

    // ── Wave 5, feature 1: the OPT-IN offline recovery envelope ──

    /// The default (recovery unset) emits NO `@recovery` envelope — team rekey/sync are byte-for-byte
    /// Wave 3/4. Setting a recovery recipient makes it emit exactly one `@recovery` envelope, and whoever
    /// holds the matching offline SECRET can unwrap the current TK from it.
    #[test]
    fn recovery_envelope_is_absent_by_default_and_one_when_set() {
        let tk = [0x3Cu8; 32];
        // Unset (the default): no envelope, in either the missing-field or the empty-string form.
        assert!(recovery_envelope(&serde_json::json!({ "name": "acme" }), &tk).unwrap().is_none(), "missing field ⇒ no envelope");
        assert!(recovery_envelope(&serde_json::json!({ "recovery_x25519": "" }), &tk).unwrap().is_none(), "empty ⇒ no envelope");

        // Set: exactly one @recovery envelope, and the offline recovery holder can open it back to TK.
        let secret = [0x5Du8; 32];
        let pubk = crate::agent::x25519_public_from_secret(&secret);
        let org = serde_json::json!({ "recovery_x25519": hex::encode(pubk) });
        let env = recovery_envelope(&org, &tk).unwrap().expect("a set recovery recipient ⇒ one envelope");
        assert_eq!(env["recipient"], "@recovery", "filed under the reserved @recovery id");
        let wrapped = env["wrapped_kek"].as_str().unwrap();
        assert_eq!(
            crate::keybox::open_tk_envelope(wrapped, &secret).unwrap(),
            tk,
            "the offline recovery holder unwraps the current TK from the @recovery envelope"
        );
        // Junk recovery key is a loud error, never a silent skip.
        assert!(recovery_envelope(&serde_json::json!({ "recovery_x25519": "nothex" }), &tk).is_err());
    }
}

#[cfg(test)]
mod secretux_gate_tests {
    use super::*;
    use crate::output::testing::capture;

    fn finding() -> (String, scan::Finding) {
        (
            "sessions/x.jsonl".to_string(),
            scan::Finding { rule: "high-entropy-string", line: 1, excerpt: "Zx8Z…redacted".into() },
        )
    }

    // AGIT_ALLOW_SECRETS is process-global, so it is mutated only while holding the capture guard
    // (which serializes every output-capturing test) and cleared before the guard drops.
    fn gate_under_override(on: bool, verb: &str) -> (Gate, String) {
        let cap = capture();
        if on {
            std::env::set_var(ALLOW_ENV, "1");
        } else {
            std::env::remove_var(ALLOW_ENV);
        }
        let gate = decide_gate(vec![finding()], verb);
        std::env::remove_var(ALLOW_ENV);
        let err = cap.err();
        (gate, err)
    }

    /// A blocked commit must state plainly that no commit happened, so a refused gate is never read as
    /// success (the snap→commit→push confusion this wave fixes).
    #[test]
    fn blocked_commit_says_no_commit_was_created() {
        let (gate, err) = gate_under_override(false, "commit");
        assert!(matches!(gate, Gate::Blocked(1)));
        assert!(err.contains("No commit created"), "got: {err:?}");
    }

    /// The push override must warn that the hub runs its own server-side gate this flag cannot reach.
    #[test]
    fn push_override_carries_the_server_gate_note() {
        let (gate, err) = gate_under_override(true, "push");
        assert!(matches!(gate, Gate::Overridden));
        assert!(err.contains("the hub runs its own secret gate"), "got: {err:?}");
    }

    /// A commit override is accurate for the local gate — it must NOT carry the hub/server-gate note.
    #[test]
    fn commit_override_omits_the_server_gate_note() {
        let (gate, err) = gate_under_override(true, "commit");
        assert!(matches!(gate, Gate::Overridden));
        assert!(!err.contains("the hub runs its own secret gate"), "commit override must not mention the hub gate: {err:?}");
    }
}
