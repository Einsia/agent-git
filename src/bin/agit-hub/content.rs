//! Session/raw/compare/diff read endpoints (sync bodies). Verbatim from the monolith.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::path::Path;

use crate::gitplumb::*;
use crate::api::{param, valid_repo_path, valid_rev};
use crate::http::Resp;

// ── sessions (read access was already decided at the call site) ──

pub(crate) fn session_summary(repo: &Path, r: &SessionRef, jsonl: &str) -> serde_json::Value {
    let d = digest(&r.runtime, &r.id, jsonl);
    let p = provenance(repo, &r.path, jsonl);
    serde_json::json!({
        "id": d.id,
        "env": r.env,
        "runtime": r.runtime,
        "branch": d.branch,
        "model": p.model,
        "author": p.author,
        "when": p.when,
        "commit": p.commit,
        "title": d.prompts.first().map(|s| first_line(s)).unwrap_or_default(),
        "conclusion": d.texts.last().map(|t| clip(t, 280)).unwrap_or_default(),
        "files": d.files,
        "tools": d.tools,
        "n_prompts": d.prompts.len(),
        "n_texts": d.texts.len(),
        "spine": spine_string(&r.runtime, jsonl),
    })
}

pub(crate) fn api_session(repo: &Path, id: &str, query: &str) -> Resp {
    let Some(r) = session_refs(repo).into_iter().find(|r| r.id == id) else {
        return Resp::err(404, "not found");
    };
    let at = param(query, "at");
    let Some(jsonl) = load_session(repo, &r.path, at.as_deref()) else {
        return Resp::err(404, "no such revision");
    };
    let d = digest(&r.runtime, &r.id, &jsonl);
    let p = provenance(repo, &r.path, &jsonl);
    let revisions: Vec<serde_json::Value> = session_revisions(repo, &r.path)
        .into_iter()
        .map(|(sha, when, subject)| serde_json::json!({ "sha": sha, "when": when, "subject": subject }))
        .collect();

    Resp::json(serde_json::json!({
        "id": d.id,
        "env": r.env,
        "runtime": r.runtime,
        "branch": d.branch,
        "cwd": d.cwd,
        "model": p.model,
        "author": p.author,
        "when": p.when,
        "commit": p.commit,
        "prompts": d.prompts.iter().map(|s| first_line(s)).collect::<Vec<_>>(),
        "texts": d.texts.iter().rev().take(8).rev().map(|t| clip(t, 700)).collect::<Vec<_>>(),
        "files": d.files,
        "spine": spine_string(&r.runtime, &jsonl),
        "revisions": revisions,
        "pinned": at,
    }))
}

/// Bytes served by the raw route in one response. A store holds transcripts, not releases.
pub(crate) const RAW_MAX: u64 = 8 * 1024 * 1024;
/// Rows of `compare` output. A diff between two distant points is unbounded; the answer to "what
/// changed across 40,000 files" is not a JSON array.
pub(crate) const COMPARE_MAX: usize = 500;

/// `GET /api/agent/<name>/raw/<path>?at=<rev>` — a file out of the store, as bytes.
///
/// **This is the one route whose response headers are the security control.** Everything it serves is
/// pushed content, so it is attacker-authored by definition: a session file can hold `<script>` as
/// easily as it holds JSON, and it is served from the Hub's own origin — the origin the session
/// cookie belongs to. So the content-type is never guessed from the extension, and never negotiated:
///
///   - `application/octet-stream` — a guessed `text/html` here is stored XSS, full stop.
///   - `attachment` — a browser following the link downloads it instead of rendering it.
///   - `nosniff` — without it a browser will content-sniff its way back to `text/html` whatever the
///     header said, which is exactly the bug the header was supposed to prevent.
///   - `sandbox` + a null CSP — defence in depth: if something does render it, it renders inert.
///
/// The SPA reads this with fetch() and decides how to display it. That is the right place for the
/// decision, because the SPA knows it is showing a transcript rather than a document.
pub(crate) fn api_raw(repo: &Path, path: &str, query: &str) -> Resp {
    if !valid_repo_path(path) {
        return Resp::err(400, "invalid path");
    }
    let at = param(query, "at").unwrap_or_else(|| "HEAD".into());
    if !valid_rev(&at) {
        return Resp::err(400, "invalid revision");
    }
    let spec = format!("{at}:{path}");
    // Size first, from the object header, so an enormous blob is refused before it is read into
    // memory rather than after.
    let size: u64 = match git(repo, &["cat-file", "-s", &spec]).and_then(|s| s.trim().parse().ok()) {
        Some(n) => n,
        None => return Resp::err(404, "not found"),
    };
    if size > RAW_MAX {
        return Resp::err(413, &format!("this file is {size} bytes; the raw view stops at {RAW_MAX}. Clone the store for it."));
    }
    let Some(body) = git_bytes(repo, &["cat-file", "blob", &spec]) else {
        return Resp::err(404, "not found");
    };
    Resp::new(200, "application/octet-stream", body)
        .with("Content-Disposition", &format!("attachment; filename=\"{}\"", safe_filename(path)))
        .with("X-Content-Type-Options", "nosniff")
        .with("Content-Security-Policy", "default-src 'none'; sandbox")
}

/// The basename, reduced to bytes that cannot break out of a quoted header value.
///
/// `Resp::with` writes headers verbatim, and this string comes from a URL: a `"` would end the value
/// early and a CR/LF would start a header of the attacker's choosing. Filtering rather than escaping,
/// because the only thing a filename has to do here is name the file.
pub(crate) fn safe_filename(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or("file");
    let s: String = base.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')).take(80).collect();
    match s.trim_matches('.').is_empty() {
        true => "file".into(),
        false => s,
    }
}

/// `GET /api/agent/<name>/compare?from=<rev>&to=<rev>` — what changed between two points of the
/// store, across the whole tree rather than within one session (that is `session/<id>/diff`).
pub(crate) fn api_compare(repo: &Path, query: &str) -> Resp {
    let (Some(from), Some(to)) = (param(query, "from"), param(query, "to")) else {
        return Resp::err(400, "need from and to");
    };
    if !valid_rev(&from) || !valid_rev(&to) {
        return Resp::err(400, "invalid revision");
    }
    // Resolve both before diffing: an unknown rev is a 404, not an empty diff that reads like "these
    // two points are identical".
    let (Some(fsha), Some(tsha)) = (rev_sha(repo, &from), rev_sha(repo, &to)) else {
        return Resp::err(404, "no such revision");
    };

    let raw = git(repo, &["diff", "--numstat", &fsha, &tsha, "--"]).unwrap_or_default();
    let mut files: Vec<serde_json::Value> = vec![];
    let mut truncated = false;
    for line in raw.lines() {
        if files.len() >= COMPARE_MAX {
            truncated = true;
            break;
        }
        let mut f = line.split('\t');
        let (added, deleted, path) = (f.next().unwrap_or("-"), f.next().unwrap_or("-"), f.next().unwrap_or(""));
        if path.is_empty() {
            continue;
        }
        // numstat prints "-" for a binary file rather than a count. Report null, not 0: "no lines
        // changed" and "lines are not the unit here" are different answers.
        files.push(serde_json::json!({
            "path": path,
            "added": added.parse::<u64>().ok(),
            "deleted": deleted.parse::<u64>().ok(),
            "binary": added == "-",
        }));
    }

    let commits: Vec<serde_json::Value> = git(repo, &["log", "--format=%H\x1f%s", &format!("{fsha}..{tsha}")])
        .unwrap_or_default()
        .lines()
        .take(COMPARE_MAX)
        .filter_map(|l| {
            let (sha, subject) = l.split_once('\x1f')?;
            Some(serde_json::json!({ "sha": sha, "subject": subject }))
        })
        .collect();

    Resp::json(serde_json::json!({
        "from": from,
        "to": to,
        // What the names resolved to, so a moving branch can be told from a fixed point later.
        "resolved": { "from": fsha, "to": tsha },
        "commits": commits,
        "files": files,
        "truncated": truncated,
    }))
}

/// Resolve a rev to a commit sha. None = it does not name a commit here.
pub(crate) fn rev_sha(repo: &Path, rev: &str) -> Option<String> {
    if !valid_rev(rev) {
        return None;
    }
    let out = git(repo, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])?;
    let s = out.trim().to_string();
    (s.len() == 40).then_some(s)
}

pub(crate) fn api_diff(repo: &Path, id: &str, query: &str) -> Resp {
    let Some(r) = session_refs(repo).into_iter().find(|r| r.id == id) else {
        return Resp::err(404, "not found");
    };
    let (Some(from), Some(to)) = (param(query, "from"), param(query, "to")) else {
        return Resp::err(400, "need from and to");
    };
    let (Some(ja), Some(jb)) = (load_session(repo, &r.path, Some(&from)), load_session(repo, &r.path, Some(&to))) else {
        return Resp::err(404, "no such revision");
    };
    let a = digest(&r.runtime, id, &ja);
    let b = digest(&r.runtime, id, &jb);
    Resp::json(serde_json::json!({
        "from": from,
        "to": to,
        "added_prompts": diff_list(&b.prompts, &a.prompts),
        "removed_prompts": diff_list(&a.prompts, &b.prompts),
        "added_files": diff_list(&b.files, &a.files),
        "removed_files": diff_list(&a.files, &b.files),
        "conclusion_before": a.texts.last().map(|t| clip(t, 300)).unwrap_or_default(),
        "conclusion_after": b.texts.last().map(|t| clip(t, 300)).unwrap_or_default(),
    }))
}

/// Elements in a but not in b (order-preserving, deduped, first line only).
pub(crate) fn diff_list(a: &[String], b: &[String]) -> Vec<String> {
    let bset: std::collections::HashSet<&String> = b.iter().collect();
    let mut seen = std::collections::HashSet::new();
    a.iter()
        .filter(|x| !bset.contains(*x) && seen.insert((*x).clone()))
        .map(|s| first_line(s))
        .collect()
}
