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
    // The ordered conversation — the readable back-and-forth the SPA renders as markdown. Built from the
    // same ordered event walk the spine/digest use, so the interleaving (user, assistant, user, …) is
    // preserved instead of being flattened into two separate lists.
    let t = extract_turns(&r.runtime, &jsonl);
    let turns: Vec<serde_json::Value> = t
        .turns
        .iter()
        .map(|turn| serde_json::json!({ "role": turn.role, "text": turn.text, "tools": turn.tools, "blocks": turn.blocks, "truncated": turn.truncated }))
        .collect();
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
        "turns": turns,
        "turns_capped": t.capped,
        "files": d.files,
        "spine": spine_string(&r.runtime, &jsonl),
        "revisions": revisions,
        "pinned": at,
        // The cryptographic provenance verdict, queryable on the session itself. This is the SELF-VERIFY
        // status (signature + content intact), computed on read at the same revision — pure, no registry
        // call. The authoritative registry attribution ("verified as <person>" / "key mismatch") is
        // RECORDED at push time (the `provenance.verify` audit event); surfacing it live here would need
        // the registry in this sync read path and is left as a followup.
        "provenance": provenance_verdict(repo, &r.path, &jsonl, at.as_deref()),
    }))
}

/// The cryptographic provenance verdict for a session, read at `at` (the same revision the transcript was
/// loaded at). Reads the committed `<id>.agit.json` sidecar, self-verifies the signature against the
/// transcript, and reports a legible verdict. A session with no sidecar/provenance is `unsigned` — never
/// an error, matching the client's graceful-degradation contract.
///
/// This is the PURE SELF-VERIFY path (signature + content intact), with no registry call — the honest
/// answer for a sync caller with no store. The registry-attributed verdict ("verified as <person>" /
/// "key mismatch") is produced by the async read path in `api.rs`, which resolves the committer email
/// against the identity registry and re-classifies via [`provenance_self_status`] +
/// [`agit::commands::attribute_with_registry`].
fn provenance_verdict(repo: &Path, path: &str, jsonl: &str, at: Option<&str>) -> serde_json::Value {
    provenance_verdict_json(&provenance_self_status(repo, path, jsonl, at))
}

/// Self-verify a session's provenance (signature + content-digest) against its committed `<id>.agit.json`
/// sidecar, at the same revision the transcript was loaded at. A missing sidecar/provenance yields
/// [`agit::commands::ProvenanceStatus::Unsigned`] — never an error. This is the pure, registry-free step
/// the async read path then attributes against the identity registry.
pub(crate) fn provenance_self_status(
    repo: &Path,
    path: &str,
    jsonl: &str,
    at: Option<&str>,
) -> agit::commands::ProvenanceStatus {
    let Some(stem) = path.strip_suffix(".jsonl") else {
        return agit::commands::ProvenanceStatus::Unsigned;
    };
    let sidecar_path = format!("{stem}.agit.json");
    let prov = load_session(repo, &sidecar_path, at)
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| serde_json::from_value::<agit::commands::Provenance>(v.get("provenance")?.clone()).ok());
    agit::commands::verify_provenance(jsonl, prov.as_ref())
}

/// Resolve a session by id, load its transcript at `at` (from the `?at=<rev>` query), and self-verify its
/// provenance. `None` only when the session id or revision does not exist (a 404) — a session that simply
/// has no signature returns `Some(Unsigned)`. The async read path calls this on the blocking pool (it
/// shells out to git), then attributes the result against the registry off-thread.
pub(crate) fn session_self_provenance(
    repo: &Path,
    id: &str,
    query: &str,
) -> Option<agit::commands::ProvenanceStatus> {
    let r = session_refs(repo).into_iter().find(|r| r.id == id)?;
    let at = param(query, "at");
    let jsonl = load_session(repo, &r.path, at.as_deref())?;
    Some(provenance_self_status(repo, &r.path, &jsonl, at.as_deref()))
}

/// Render a [`agit::commands::ProvenanceStatus`] to the JSON verdict the SPA reads: a one-word `status`
/// (`verified` / `verified_as` / `key_mismatch` / `signed_unregistered` / `unsigned` / `content_tampered`
/// / `bad_signature`), a human `summary`, and the fields specific to that verdict. This is the SINGLE
/// word-mapping shared by the sync self-verify helper and the async registry-classified read path, so a
/// `KeyMismatch` (a forgery) can NEVER be emitted as `verified`/`verified_as` from either path.
pub(crate) fn provenance_verdict_json(status: &agit::commands::ProvenanceStatus) -> serde_json::Value {
    use agit::commands::ProvenanceStatus as S;
    let (word, extra) = match status {
        S::Verified { aid, email, pubkey } => (
            "verified",
            serde_json::json!({ "aid": aid, "email": email, "pubkey": pubkey }),
        ),
        S::Unsigned => ("unsigned", serde_json::json!({})),
        S::ContentTampered { recorded, actual } => (
            "content_tampered",
            serde_json::json!({ "recorded_digest": recorded, "actual_digest": actual }),
        ),
        S::BadSignature => ("bad_signature", serde_json::json!({})),
        // The registry-classified variants come from the async read path (`api.rs`), which attributes a
        // self-verified session against the identity registry. A KeyMismatch (a forgery) is mapped to its
        // OWN word — never "verified"/"verified_as" — so no path can turn a forgery green.
        S::VerifiedAs { username, aid, email, pubkey } => (
            "verified_as",
            serde_json::json!({ "username": username, "aid": aid, "email": email, "pubkey": pubkey }),
        ),
        S::KeyMismatch { email, claimed_username, .. } => (
            "key_mismatch",
            serde_json::json!({ "email": email, "claimed_username": claimed_username }),
        ),
        S::SignedUnregistered { aid, email, pubkey } => (
            "signed_unregistered",
            serde_json::json!({ "aid": aid, "email": email, "pubkey": pubkey }),
        ),
    };
    let mut obj = serde_json::json!({ "status": word, "summary": status.summary() });
    if let (Some(o), Some(e)) = (obj.as_object_mut(), extra.as_object()) {
        for (k, val) in e {
            o.insert(k.clone(), val.clone());
        }
    }
    obj
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
