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

/// `git`, feeding `stdin` to the subprocess. The plumbing writers (`hash-object --stdin`, `mktree`)
/// take their input on stdin rather than argv, so an agent.toml with newlines or odd bytes never has
/// to survive an argv round-trip. Returns trimmed stdout on success.
fn git_stdin(repo: &Path, args: &[&str], stdin: &[u8]) -> Option<String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(stdin).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Bootstrap an EMPTY bare repo into a valid, sessionless agent store: one commit on the default
/// branch (`main`) whose tree is a single `agent.toml` carrying a freshly minted `agt_<uuid>`
/// identity, written byte-for-byte in the format the CLIENT mints (`agit a init` → `write_agent_toml`
/// in src/agent.rs). After this, a `git clone` — or `agit a clone` — of the store immediately succeeds:
/// it has an agent.toml with an `agt_` aid, so it is adoptable, which closes the chicken-and-egg where
/// a hub-created store could not be cloned until someone first pushed an identity to it.
///
/// Pure git plumbing (hash-object → mktree → commit-tree → update-ref); there is no working tree, so it
/// is safe on a bare repo. The committer identity is passed per-invocation with `-c`, never written to
/// global config, so it neither depends on nor mutates the operator's git identity. Returns the minted
/// aid. Any git step failing is an `Err(String)` and leaves the repo empty (the ref is only moved as the
/// last step).
pub(crate) fn initialize_store(repo: &Path, name: &str) -> Result<String, String> {
    // Mint the aid exactly like the client: `agt_` + a uuid-shaped id from the shared `fresh_id`.
    let aid = format!("agt_{}", agit::convo::fresh_id("agent-identity"));
    let created = agit::hub::store::now_iso();
    // The client refuses a name that would need TOML escaping (check_toml_value); a hub agent name is
    // already `valid_agent_name` ([A-Za-z0-9._-]), so it can never contain a quote/backslash/control.
    let toml = format!(
        "# Agent identity — committed, so the aid travels with the store's history.\n\
         # The id is minted once and never changes; the name is a label and may be renamed.\n\
         [agent]\n\
         id      = \"{aid}\"\n\
         name    = \"{name}\"\n\
         created = \"{created}\"\n"
    );
    // hash-object -w --stdin: write the agent.toml blob into the object db, get its sha.
    let blob = git_stdin(repo, &["hash-object", "-w", "-t", "blob", "--stdin"], toml.as_bytes())
        .ok_or_else(|| "git hash-object failed writing agent.toml".to_string())?;
    // mktree: a one-entry tree { agent.toml -> blob }. A literal TAB separates mode/type/sha from path.
    let tree = git_stdin(repo, &["mktree"], format!("100644 blob {blob}\tagent.toml\n").as_bytes())
        .ok_or_else(|| "git mktree failed".to_string())?;
    // commit-tree: the root commit (no parent), with a deterministic hub committer identity passed via
    // `-c` so it neither reads nor writes global git config.
    let commit = git(
        repo,
        &[
            "-c",
            "user.name=agit-hub",
            "-c",
            "user.email=hub@agit.local",
            "commit-tree",
            &tree,
            "-m",
            "chore: initialize agent store",
        ],
    )
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .ok_or_else(|| "git commit-tree failed".to_string())?;
    // update-ref: publish the commit on `main` (the branch `git init -b main` set HEAD to). Only now
    // does the repo become non-empty, so a mid-way failure above leaves it exactly as it was.
    git(repo, &["update-ref", "refs/heads/main", &commit]).ok_or_else(|| "git update-ref failed".to_string())?;
    Ok(aid)
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

/// One readable turn in the conversation view: a user prompt, or an assistant reply and the tool
/// activity it drove. A turn now carries an ordered `blocks` list reconstructed from the same ordered
/// events, so the SPA can render a Claude-Code/Codex-style transcript (text, tool call, tool result,
/// file edit) instead of one flat markdown blob. Each block is a tagged JSON value:
///   - `{"kind":"text","text": <markdown, clipped to [`TURN_CLIP`]>}`
///   - `{"kind":"tool_use","name": <str>, "input": <compact one-line preview, clipped to [`TOOL_INPUT_CLIP`]>}`
///   - `{"kind":"tool_result","output": <clipped to [`TOOL_RESULT_CLIP`]>}`
///   - `{"kind":"file_edit","paths": [<str>...], "more": <n>}` (paths capped at [`FILE_EDIT_CAP`]; `more` = the overflow count, present only when > 0)
///
/// `text` keeps a flat concat of the turn's text blocks (clipped) for back-compat/search; `tools`
/// counts the tool calls folded into an assistant turn (0 for a user turn); `truncated` is true when
/// any block or the flat text was clipped, so the SPA can surface a "truncated" note.
pub(crate) struct Turn {
    pub(crate) role: &'static str,
    pub(crate) text: String,
    pub(crate) tools: usize,
    pub(crate) blocks: Vec<serde_json::Value>,
    pub(crate) truncated: bool,
}

/// The ordered conversation plus whether it was capped (a huge session must not return megabytes).
pub(crate) struct Turns {
    pub(crate) turns: Vec<Turn>,
    pub(crate) capped: bool,
}

/// Max turns returned by the session view; a longer conversation is truncated (`capped`). Generous,
/// because virtualization is client-side — but the payload still has to stay bounded.
pub(crate) const TURN_CAP: usize = 500;
/// Per-text-block (and flat `text`) char bound; a longer text is clipped with a trailing marker.
pub(crate) const TURN_CLIP: usize = 3000;
/// Char bound for a tool call's input preview (a compact one-line render of the JSON input).
pub(crate) const TOOL_INPUT_CLIP: usize = 600;
/// Char bound for a tool result's output text.
pub(crate) const TOOL_RESULT_CLIP: usize = 1000;
/// Max file-edit paths listed on one block; the overflow is reported as a `more` count.
pub(crate) const FILE_EDIT_CAP: usize = 20;

/// Clip to `n` chars, char-safe, returning `(clipped, was_clipped)`. Unlike [`clip_marked`] the caller
/// learns whether the text was actually cut (a text that already ends in "..." is not a false positive).
fn clip_flag(s: &str, n: usize) -> (String, bool) {
    let t = s.trim();
    if t.chars().count() <= n {
        (t.to_string(), false)
    } else {
        let head: String = t.chars().take(n).collect();
        (format!("{head}..."), true)
    }
}

/// Reconstruct the ordered back-and-forth from a transcript, IN ORDER, as turns of ordered blocks, so
/// the session view can render a real Claude-Code/Codex-style conversation instead of two flattened
/// lists. Walks the same ordered ConversationIR events the spine does:
///   - each real `UserPrompt` starts a user turn (one text block);
///   - `AssistantText` opens (or extends) the current assistant turn with a text block;
///   - `ToolCall` appends a `tool_use` block to the current assistant turn (opening one if a tool call
///     leads the reply) and bumps its `tools` count;
///   - `ToolResult` / `FileEdit` append their blocks to the current assistant turn.
/// A user prompt closes the open assistant turn. Empty/whitespace text is skipped; every payload is
/// clipped to a bound so hundreds of turns stay a sane JSON size, and the whole thing is capped at
/// [`TURN_CAP`] turns (`capped`).
///
/// Runtime-agnostic (claude-code / codex) via [`agit::convo::read_conversation`]; an unparsable
/// transcript yields an empty, uncapped result rather than an error — the view degrades to its other
/// sections.
pub(crate) fn extract_turns(runtime: &str, jsonl: &str) -> Turns {
    use agit::convo::EventKind;
    let Ok(ir) = agit::convo::read_conversation(runtime, jsonl) else {
        return Turns { turns: Vec::new(), capped: false };
    };
    let mut turns: Vec<Turn> = Vec::new();
    let mut capped = false;
    // Index of the open assistant turn — tool activity and further assistant text fold into it. Reset to
    // None by a user prompt, so the next assistant utterance starts a fresh turn.
    let mut cur_assist: Option<usize> = None;
    'walk: for e in &ir.events {
        for k in &e.kinds {
            match k {
                EventKind::UserPrompt(s) => {
                    if s.trim().is_empty() {
                        continue;
                    }
                    if turns.len() >= TURN_CAP {
                        capped = true;
                        break 'walk;
                    }
                    let (t, clipped) = clip_flag(s, TURN_CLIP);
                    turns.push(Turn {
                        role: "user",
                        text: t.clone(),
                        tools: 0,
                        blocks: vec![serde_json::json!({ "kind": "text", "text": t })],
                        truncated: clipped,
                    });
                    cur_assist = None;
                }
                EventKind::AssistantText(s) => {
                    if s.trim().is_empty() {
                        continue;
                    }
                    let (t, clipped) = clip_flag(s, TURN_CLIP);
                    match cur_assist {
                        Some(i) => {
                            if !turns[i].text.is_empty() {
                                turns[i].text.push_str("\n\n");
                            }
                            turns[i].text.push_str(&t);
                            turns[i].blocks.push(serde_json::json!({ "kind": "text", "text": t }));
                            turns[i].truncated |= clipped;
                        }
                        None => {
                            if turns.len() >= TURN_CAP {
                                capped = true;
                                break 'walk;
                            }
                            turns.push(Turn {
                                role: "assistant",
                                text: t.clone(),
                                tools: 0,
                                blocks: vec![serde_json::json!({ "kind": "text", "text": t })],
                                truncated: clipped,
                            });
                            cur_assist = Some(turns.len() - 1);
                        }
                    }
                }
                EventKind::ToolCall { name, input, .. } => {
                    // A reply may lead with a tool call (assistant message with no text); open a turn so
                    // the tool is never dropped.
                    let i = match cur_assist {
                        Some(i) => i,
                        None => {
                            if turns.len() >= TURN_CAP {
                                capped = true;
                                break 'walk;
                            }
                            turns.push(Turn {
                                role: "assistant",
                                text: String::new(),
                                tools: 0,
                                blocks: Vec::new(),
                                truncated: false,
                            });
                            let idx = turns.len() - 1;
                            cur_assist = Some(idx);
                            idx
                        }
                    };
                    // A compact one-line render of the input (attacker-authored — the SPA shows it as
                    // plain text in a mono box, never as a markdown/HTML sink).
                    let compact = serde_json::to_string(input).unwrap_or_default();
                    let (preview, clipped) = clip_flag(&compact, TOOL_INPUT_CLIP);
                    turns[i].blocks.push(serde_json::json!({ "kind": "tool_use", "name": name, "input": preview }));
                    turns[i].tools += 1;
                    turns[i].truncated |= clipped;
                }
                EventKind::ToolResult { output, .. } => {
                    if let Some(i) = cur_assist {
                        let (out, clipped) = clip_flag(output, TOOL_RESULT_CLIP);
                        turns[i].blocks.push(serde_json::json!({ "kind": "tool_result", "output": out }));
                        turns[i].truncated |= clipped;
                    }
                }
                EventKind::FileEdit { paths } => {
                    if let Some(i) = cur_assist {
                        let shown: Vec<&String> = paths.iter().take(FILE_EDIT_CAP).collect();
                        let more = paths.len().saturating_sub(FILE_EDIT_CAP);
                        let mut block = serde_json::json!({ "kind": "file_edit", "paths": shown });
                        if more > 0 {
                            block["more"] = serde_json::json!(more);
                            turns[i].truncated = true;
                        }
                        turns[i].blocks.push(block);
                    }
                }
            }
        }
    }
    Turns { turns, capped }
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
