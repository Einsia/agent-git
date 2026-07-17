//! Git readers over bare repos + cross-runtime session parsing. Verbatim from the monolith.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use agit::hub::identity::Identity;
use agit::hub::identity;

use crate::scan::BINARY_SNIFF_BYTES;
use crate::api::valid_rev;

// ─────────────────────── git reads (bare repos) ───────────────────────

pub(crate) fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git`, without the lossy UTF-8 conversion. Anything serving a blob's **bytes** has to use this:
/// `from_utf8_lossy` silently rewrites every invalid sequence to U+FFFD, which corrupts the file it
/// claims to be handing over.
pub(crate) fn git_bytes(repo: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    out.status.success().then_some(out.stdout)
}

pub(crate) fn has_head(repo: &Path) -> bool {
    git(repo, &["rev-parse", "HEAD"]).is_some()
}

pub(crate) fn recent_log(repo: &Path, n: usize) -> Vec<(String, String)> {
    git(repo, &["log", &format!("-{n}"), "--format=%h%x09%s"])
        .map(|s| {
            s.lines()
                .filter_map(|l| l.split_once('\t').map(|(a, b)| (a.to_string(), b.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Relative time + subject of the last commit, used by the home page (cheap, a single git log).
pub(crate) fn last_activity(repo: &Path) -> (String, String) {
    git(repo, &["log", "-1", "--format=%cr\x1f%s"])
        .and_then(|s| s.trim().split_once('\x1f').map(|(a, b)| (a.to_string(), b.to_string())))
        .unwrap_or_default()
}

/// The agent identity inside the store. **The store itself is the authority** (agent.toml is
/// committed into its history); the Hub never mints an aid.
/// Returns (aid, source). Source values:
///   "agent.toml"   — read it
///   "none"         — the repo is still empty, or has no agent.toml (that's a freshly created repo
///                    nobody has pushed to)
///   "unidentified" — agent.toml exists but carries no agt_ identity (an old store's placeholder id)
/// Bytes of README served with the agent detail. Prose, not a book — and it rides along on a route
/// people hit constantly.
pub(crate) const README_MAX: usize = 64 * 1024;

/// The store's README, read out of the default ref. None = there isn't one, which is the common case
/// and not an error.
///
/// Returned as **text for a JSON field**, never as a document: it is pushed content, so it is
/// attacker-authored by definition, and the moment it is served as its own response it needs the same
/// treatment as `api_raw`. The SPA renders it; the SPA must not render it as HTML.
pub(crate) fn readme(repo: &Path) -> Option<String> {
    if !has_head(repo) {
        return None;
    }
    for candidate in ["README.md", "readme.md", "README"] {
        let Some(out) = git_bytes(repo, &["show", &format!("HEAD:{candidate}")]) else {
            continue;
        };
        // A binary blob called README.md is not a README; it is a way to put NULs in a JSON string.
        if out.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0) {
            return None;
        }
        let text = String::from_utf8_lossy(&out).into_owned();
        return Some(clip(&text, README_MAX));
    }
    None
}

pub(crate) fn agent_aid(repo: &Path) -> (Option<String>, &'static str) {
    let Some(text) = git(repo, &["show", "HEAD:agent.toml"]) else {
        return (None, "none");
    };
    match identity::parse_agent_toml(&text) {
        Identity::Aid(a) => (Some(a), "agent.toml"),
        Identity::Unidentified => (None, "unidentified"),
    }
}

/// Where one session lives in the store.
///
/// Both layouts must be recognized (the new one in the design doc carries the environment; old
/// repos don't):
///   sessions/<env>/<runtime>/<id>.jsonl   — new
///   sessions/<runtime>/<id>.jsonl         — old (env = None)
pub(crate) struct SessionRef {
    pub(crate) env: Option<String>,
    pub(crate) runtime: String,
    pub(crate) id: String,
    pub(crate) path: String,
}

pub(crate) fn session_refs(repo: &Path) -> Vec<SessionRef> {
    let mut out = vec![];
    let Some(list) = git(repo, &["ls-tree", "-r", "--name-only", "HEAD", "sessions/"]) else {
        return out;
    };
    for path in list.lines() {
        let path = path.trim();
        if !path.ends_with(".jsonl") {
            continue;
        }
        let segs: Vec<&str> = path.split('/').collect();
        let (env, runtime, file) = match segs.len() {
            3 => (None, segs[1], segs[2]),
            4 => (Some(segs[1].to_string()), segs[2], segs[3]),
            _ => continue,
        };
        out.push(SessionRef {
            env,
            runtime: runtime.to_string(),
            id: file.trim_end_matches(".jsonl").to_string(),
            path: path.to_string(),
        });
    }
    out
}

pub(crate) fn load_session(repo: &Path, path: &str, at: Option<&str>) -> Option<String> {
    let at = at.unwrap_or("HEAD");
    // The rev arrives off the query string and is concatenated into a `<rev>:<path>` **argv slot**,
    // so a leading `-` makes git read the whole thing as an option — and `git show` has options that
    // write files (`--output=`). Checked here rather than at each caller: this is the one place the
    // value reaches git, so it is the one place that cannot be forgotten.
    if !valid_rev(at) {
        return None;
    }
    git(repo, &["show", &format!("{at}:{path}")])
}

/// Session count and last activity per environment. environment = which code repo the session came from.
pub(crate) fn environments(repo: &Path, refs: &[SessionRef]) -> Vec<serde_json::Value> {
    // Keep the order (by first appearance) and note each group's directories, to scope the git log.
    let mut order: Vec<Option<String>> = vec![];
    let mut counts: HashMap<Option<String>, usize> = HashMap::new();
    let mut dirs: HashMap<Option<String>, Vec<String>> = HashMap::new();
    for r in refs {
        if !counts.contains_key(&r.env) {
            order.push(r.env.clone());
        }
        *counts.entry(r.env.clone()).or_insert(0) += 1;
        // The new layout scopes by the env directory; the old one (env=None) has no env directory,
        // so it can only be scoped by the runtime directory.
        let dir = match &r.env {
            Some(e) => format!("sessions/{e}"),
            None => format!("sessions/{}", r.runtime),
        };
        let d = dirs.entry(r.env.clone()).or_default();
        if !d.contains(&dir) {
            d.push(dir);
        }
    }
    order
        .into_iter()
        .map(|env| {
            let last = dirs
                .get(&env)
                .and_then(|ds| {
                    let mut args: Vec<String> = vec!["log".into(), "-1".into(), "--format=%cr".into(), "--".into()];
                    // `:(literal)` turns off pathspec globbing — directory names come from repo
                    // content and may contain `*`/`?`.
                    args.extend(ds.iter().map(|d| format!(":(literal){d}")));
                    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                    git(repo, &argv)
                })
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            serde_json::json!({ "env": env, "sessions": counts.get(&env).copied().unwrap_or(0), "last": last })
        })
        .collect()
}

pub(crate) fn branches(repo: &Path) -> Vec<serde_json::Value> {
    git(repo, &["for-each-ref", "--format=%(refname:short)\x1f%(objectname:short)\x1f%(committerdate:relative)", "refs/heads/"])
        .map(|s| {
            s.lines()
                .filter_map(|l| {
                    let mut it = l.split('\x1f');
                    let name = it.next()?;
                    Some(serde_json::json!({
                        "name": name,
                        "commit": it.next().unwrap_or(""),
                        "when": it.next().unwrap_or(""),
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Bytes the repo occupies. git count-objects reports KiB.
pub(crate) fn size_bytes(repo: &Path) -> u64 {
    let Some(out) = git(repo, &["count-objects", "-v"]) else {
        return 0;
    };
    let mut kib = 0u64;
    for line in out.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if matches!(k.trim(), "size" | "size-pack") {
                kib += v.trim().parse::<u64>().unwrap_or(0);
            }
        }
    }
    kib * 1024
}

/// Runtimes seen in the store. Alphabetical — claude-code and codex are **peers**, neither is the default.
pub(crate) fn runtimes(refs: &[SessionRef]) -> Vec<String> {
    let mut v: Vec<String> = refs.iter().map(|r| r.runtime.clone()).collect();
    v.sort();
    v.dedup();
    v
}

// ─────────── Session parsing (cross-runtime, through the agit lib) ───────────

pub(crate) struct SessionDigest {
    pub(crate) id: String,
    pub(crate) branch: String,
    pub(crate) cwd: String,
    pub(crate) prompts: Vec<String>,
    pub(crate) texts: Vec<String>,
    pub(crate) tools: usize,
    pub(crate) files: Vec<String>,
}

pub(crate) fn digest(runtime: &str, id: &str, jsonl: &str) -> SessionDigest {
    // Parse through the adapter registry; an unknown runtime falls back to the claude-code parser,
    // as before.
    let ir = agit::adapter::get(runtime)
        .map(|a| a.parse(jsonl, id))
        .unwrap_or_else(|_| agit::adapter::claude_code::parse_jsonl(jsonl, id));
    let mut files = Vec::new();
    for w in &ir.writes {
        let f = w.rsplit('/').next().unwrap_or(w).to_string();
        if !files.contains(&f) {
            files.push(f);
        }
    }
    SessionDigest {
        id: ir.session_id,
        branch: ir.git_branch.unwrap_or_default(),
        cwd: ir.cwd.unwrap_or_default(),
        prompts: ir.prompts,
        texts: ir.agent_texts,
        tools: ir.tool_uses,
        files,
    }
}

pub(crate) struct Provenance {
    pub(crate) author: String,
    pub(crate) when: String,
    pub(crate) commit: String,
    pub(crate) model: String,
}

pub(crate) fn provenance(repo: &Path, path: &str, jsonl: &str) -> Provenance {
    let raw = git(repo, &["log", "-1", "--format=%an\x1f%cr\x1f%H", "--", path]).unwrap_or_default();
    let mut it = raw.trim().split('\x1f');
    Provenance {
        author: it.next().unwrap_or("").to_string(),
        when: it.next().unwrap_or("").to_string(),
        commit: it.next().unwrap_or("").to_string(),
        model: extract_model(jsonl).unwrap_or_default(),
    }
}

pub(crate) fn extract_model(jsonl: &str) -> Option<String> {
    for line in jsonl.lines().take(400) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let candidates = [
            v.get("message").and_then(|m| m.get("model")),
            v.get("payload").and_then(|p| p.get("model")),
            v.get("model"),
        ];
        for c in candidates.into_iter().flatten() {
            if let Some(m) = c.as_str() {
                if !m.is_empty() {
                    return Some(m.to_string());
                }
            }
        }
    }
    None
}

pub(crate) fn session_revisions(repo: &Path, path: &str) -> Vec<(String, String, String)> {
    git(repo, &["log", "--format=%H\x1f%cr\x1f%s", "--", path])
        .map(|s| {
            s.lines()
                .filter_map(|l| {
                    let mut it = l.split('\x1f');
                    Some((it.next()?.to_string(), it.next().unwrap_or("").to_string(), it.next().unwrap_or("").to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A session's event spine: ordered kinds → a 'p'/'a'/'t'/'e' string (the SPA renders it as a
/// waveform). Cross-runtime via ConversationIR.
pub(crate) fn spine_string(runtime: &str, jsonl: &str) -> String {
    use agit::convo::EventKind;
    let Ok(ir) = agit::convo::read_conversation(runtime, jsonl) else {
        return String::new();
    };
    let mut out = String::new();
    for e in &ir.events {
        for k in &e.kinds {
            out.push(match k {
                EventKind::UserPrompt(_) => 'p',
                EventKind::AssistantText(_) => 'a',
                EventKind::ToolCall { .. } | EventKind::ToolResult { .. } => 't',
                EventKind::FileEdit { .. } => 'e',
            });
            if out.len() >= 600 {
                return out;
            }
        }
    }
    out
}

pub(crate) fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

pub(crate) fn clip(s: &str, n: usize) -> String {
    s.trim().chars().take(n).collect()
}
