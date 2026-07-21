//! Session-aware renderers for `agit a log` and `agit a diff` (design §11c).
//!
//! `agit a log` / `agit a diff` are git verbs on the store, and a *raw* `git log`/`git diff` there is a
//! wall of jsonl transcript bytes — technically the truth, unreadable in practice. These render the
//! SESSION view instead, built on the same parse the rest of agit reads sessions with
//! (`convo::read_conversation`, walking `Event`/`EventKind`):
//!
//!   * `agit a log`  — a timeline of the store's sessions, most recent first: runtime, when, where it
//!     ran, its opening prompt, and its tool activity.
//!   * `agit a diff` — the session-level change between two refs: the prompts and the tool/edit activity
//!     ADDED, never a line-by-line diff of the jsonl itself.
//!
//! `--raw` (or `--git`) is the escape hatch back to real `git log`/`git diff` (see the dispatcher), so
//! the byte-level view and every scripted `--format` still work.
//!
//! Not a TUI (like `ui`): plain, pipeable text, colour as emphasis only.

use crate::commands;
use crate::convo::{self, narrate_call, ConversationIR, EventKind};
use crate::scope;
use crate::session;
use crate::ui;
use anyhow::Result;
use std::path::Path;

/// git's canonical empty-tree object — a stand-in "before" when the store has only one commit, so
/// `agit a diff` in a fresh store shows every session as added rather than erroring on `HEAD~1`.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The session facts the two views render, distilled from a parsed conversation.
struct Distilled {
    /// Real user prompts, in order (blank ones dropped).
    prompts: Vec<String>,
    /// Files the agent edited (deduplicated, first-seen order) — claude's `Write`/`Edit` tool calls and
    /// codex's `FileEdit` records both land here.
    edits: Vec<String>,
    /// Every tool call narrated to one line (`convo::narrate_call`), in order — the "what happened".
    activity: Vec<String>,
    /// Total tool calls (including uncategorized ones).
    tool_calls: usize,
}

/// Walk `Event`/`EventKind` once, pulling out everything both views need. This is the same semantic
/// overlay convert reads, so the log/diff view can never disagree with `agit a info` about what a
/// session is.
fn distill(ir: &ConversationIR) -> Distilled {
    let mut d = Distilled { prompts: Vec::new(), edits: Vec::new(), activity: Vec::new(), tool_calls: 0 };
    let mut note_edit = |p: &str| {
        if !p.is_empty() && !d.edits.iter().any(|e| e == p) {
            d.edits.push(p.to_string());
        }
    };
    for e in &ir.events {
        for k in &e.kinds {
            match k {
                EventKind::UserPrompt(s) => {
                    let s = s.trim();
                    if !s.is_empty() {
                        d.prompts.push(s.to_string());
                    }
                }
                EventKind::ToolCall { name, input, .. } => {
                    d.tool_calls += 1;
                    d.activity.push(narrate_call(name, input));
                    if matches!(name.as_str(), "Write" | "Edit" | "MultiEdit") {
                        if let Some(p) = input.get("file_path").and_then(|v| v.as_str()) {
                            note_edit(p);
                        }
                    }
                }
                EventKind::FileEdit { paths } => {
                    d.activity.push(format!("[edited: {}]", paths.join(", ")));
                    for p in paths {
                        note_edit(p);
                    }
                }
                _ => {}
            }
        }
    }
    d
}

/// Parse a session's raw text into the distilled view. `None` for an unrecognized runtime or a body
/// that yields no conversation.
fn read(runtime: &str, text: &str) -> Option<Distilled> {
    let ir = convo::read_conversation(runtime, text).ok()?;
    Some(distill(&ir))
}

/// The subset of `git log` options the SESSION timeline can honor. `git a log` is not a byte-level
/// `git log`, so most of git's flags have no session-level meaning; the ones that do are mapped here,
/// and anything else is surfaced (never silently swallowed) — see `agent_log`.
struct LogOpts {
    /// `-n <N>` / `-<N>` / `-n<N>`: cap the session list to the N most recent. `None` = all.
    limit: Option<usize>,
    /// `--oneline`: one physical line per session (no header, no blank lines).
    oneline: bool,
    /// A revision or range positional (`HEAD~1`, a sha, `A..B`): scope the list to sessions changed in
    /// that range. A bare rev `R` means `R..HEAD` (the sessions new since R).
    range: Option<String>,
}

/// Parse `agit a log` arguments into the options the session view honors. A dash flag the view cannot
/// map is NOT swallowed (git-parity): a one-line note points at `--raw`, and parsing continues so the
/// flags that DO map still take effect.
fn parse_log_opts(args: &[String]) -> LogOpts {
    let mut opts = LogOpts { limit: None, oneline: false, range: None };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--oneline" {
            opts.oneline = true;
            i += 1;
        } else if a == "-n" {
            // `-n <N>`
            opts.limit = args.get(i + 1).and_then(|s| s.parse::<usize>().ok());
            i += 2;
        } else if let Some(n) = a.strip_prefix("-n").filter(|s| !s.is_empty()).and_then(|s| s.parse::<usize>().ok()) {
            // `-n<N>` glued
            opts.limit = Some(n);
            i += 1;
        } else if let Some(n) = a.strip_prefix('-').filter(|s| !s.is_empty()).and_then(|s| s.parse::<usize>().ok()) {
            // `-<N>`
            opts.limit = Some(n);
            i += 1;
        } else if a.starts_with('-') {
            eprintln!("note: agit a log does not map '{a}'; use `agit a log --raw` for the git log.");
            i += 1;
        } else {
            // A revision or range positional scopes the list (last one wins).
            opts.range = Some(a.to_string());
            i += 1;
        }
    }
    opts
}

/// `agit a log` — the store's sessions as a timeline, most recent first.
pub fn agent_log(args: &[String]) -> Result<i32> {
    let opts = parse_log_opts(args);
    let agent = crate::agent::resolve(None)?;
    let mut sessions = commands::store_sessions(&agent.store);
    // Most recent first, by what the store RECORDS (recency), not the filesystem — the same ordering
    // `latest_session` trusts, and for the same reason (a clone flattens every mtime).
    sessions.sort_by_key(|s| std::cmp::Reverse(s.recency()));

    if sessions.is_empty() {
        println!("{} has no sessions yet: agit start, then agit a snap.", ui::bold(&agent.name));
        return Ok(0);
    }

    // A revision positional scopes the list to sessions that CHANGED in the range. A bare rev `R`
    // becomes `R..HEAD` (what is new since R); a `..`/`...` positional is a range already.
    if let Some(rev) = &opts.range {
        let spec = if rev.contains("..") { rev.clone() } else { format!("{rev}..HEAD") };
        let (code, out) =
            scope::git_in_status(&agent.store, &["diff", "--name-only", &spec, "--", "sessions"]);
        if code != 0 {
            anyhow::bail!(
                "could not scope to {spec} on the store: check the ref (or use `agit a log --raw` for the git log)."
            );
        }
        let changed: std::collections::HashSet<&str> = out.lines().collect();
        sessions.retain(|s| {
            s.path
                .strip_prefix(&agent.store)
                .ok()
                .and_then(|rel| rel.to_str())
                .map(|rel| changed.contains(rel))
                .unwrap_or(false)
        });
    }

    // `-n <N>` / `-<N>`: keep the N most recent (after scoping, before rendering).
    if let Some(n) = opts.limit {
        sessions.truncate(n);
    }

    // `--oneline`: exactly one line per session, no header or blank lines — the pipe-friendly form.
    if opts.oneline {
        for s in &sessions {
            let text = std::fs::read_to_string(&s.path).unwrap_or_default();
            let gist = convo::read_conversation(s.runtime, &text)
                .ok()
                .map(|ir| distill(&ir))
                .and_then(|d| d.prompts.first().cloned())
                .map(|g| ui::one_line(&g, 60))
                .unwrap_or_else(|| "(no prompt)".to_string());
            println!(
                "{} {} {} {}",
                ui::accent(&short_id(&s.path)),
                s.runtime,
                ui::dim(&ui::ago(s.recency())),
                ui::dim(&format!("\"{gist}\""))
            );
        }
        return Ok(0);
    }

    if sessions.is_empty() {
        println!("{} · no sessions in that range.", ui::bold(&agent.name));
        return Ok(0);
    }

    let count = sessions.len();
    println!(
        "{} · {count} session{}",
        ui::bold(&agent.name),
        if count == 1 { "" } else { "s" }
    );

    for s in &sessions {
        let when = ui::ago(s.recency());
        // One parse yields both the origin and the distilled facts — the same parse the rest of agit
        // reads sessions with.
        let text = std::fs::read_to_string(&s.path).unwrap_or_default();
        let ir = convo::read_conversation(s.runtime, &text).ok();
        let distilled = ir.as_ref().map(distill);
        // Where it ran: the cwd the transcript recorded (`~`-collapsed), else the store's partition slug.
        let origin = ir
            .as_ref()
            .and_then(|ir| ir.cwd.clone())
            .map(|c| ui::tilde(Path::new(&c)))
            .or_else(|| s.env_slug.clone());

        println!();
        let loc = match origin {
            Some(o) => format!("{} · {when} · {}", s.runtime, ui::accent(&o)),
            None => format!("{} · {when}", s.runtime),
        };
        println!("● {loc}");

        match &distilled {
            Some(d) => {
                if let Some(gist) = d.prompts.first() {
                    println!("    {}", ui::dim(&format!("\"{}\"", ui::one_line(gist, 72))));
                }
                let mut tail = String::new();
                if d.tool_calls > 0 {
                    tail.push_str(&format!("{} tool call{}", d.tool_calls, if d.tool_calls == 1 { "" } else { "s" }));
                }
                if !d.edits.is_empty() {
                    if !tail.is_empty() {
                        tail.push_str(" · ");
                    }
                    tail.push_str(&format!("edited {}", edit_summary(&d.edits)));
                }
                if !tail.is_empty() {
                    println!("    {}", ui::dim(&tail));
                }
            }
            None => println!("    {}", ui::dim("(unreadable transcript)")),
        }
    }
    Ok(0)
}

/// `agit a diff [<from>] [<to>]` — the SESSION-level change between two refs of the store: the sessions
/// added or updated, and for each, the prompts and tool/edit activity ADDED. Never a line-by-line diff
/// of the jsonl (that is `agit a diff --raw`).
///
/// With no refs: `@{u}..HEAD` (this repo's unpushed work), falling back to `HEAD~1..HEAD`, then to the
/// empty tree for a single-commit store. One positional ref diffs it against `HEAD`; two name both ends.
pub fn agent_diff(args: &[String]) -> Result<i32> {
    let agent = crate::agent::resolve(None)?;
    let store = &agent.store;
    let refs: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).map(String::as_str).collect();
    // `diff_range` is what we hand `git diff` (a single `A..B`/`A...B` token, or two endpoint refs);
    // `from`/`to` are the endpoints we `git show` each side of and print in the header. A single
    // positional that ALREADY contains `..`/`...` is a range: pass it through verbatim, never append
    // `..HEAD` (that produced the doubled `HEAD~2..HEAD..HEAD` git error).
    let (diff_range, from, to): (Vec<String>, String, String) = match refs.as_slice() {
        [] => {
            let f = default_from(store);
            (vec![f.clone(), "HEAD".to_string()], f, "HEAD".to_string())
        }
        [a] if a.contains("..") => {
            let sep = if a.contains("...") { "..." } else { ".." };
            let (l, r) = a.split_once(sep).unwrap_or((a, ""));
            let from = if l.is_empty() { "HEAD".to_string() } else { l.to_string() };
            let to = if r.is_empty() { "HEAD".to_string() } else { r.to_string() };
            (vec![(*a).to_string()], from, to)
        }
        [a] => (vec![(*a).to_string(), "HEAD".to_string()], (*a).to_string(), "HEAD".to_string()),
        [a, b, ..] => (
            vec![(*a).to_string(), (*b).to_string()],
            (*a).to_string(),
            (*b).to_string(),
        ),
    };

    // Which session files differ across the range, restricted to `sessions/`.
    let mut diff_args: Vec<&str> = vec!["diff", "--name-status"];
    diff_args.extend(diff_range.iter().map(String::as_str));
    diff_args.extend(["--", "sessions"]);
    let (code, out) = scope::git_in_status(store, &diff_args);
    if code != 0 {
        anyhow::bail!(
            "could not diff {from}..{to} on the store: check the refs (or use `agit a diff --raw` for the git diff)."
        );
    }

    println!("{} · {}", ui::bold(&agent.name), ui::dim(&format!("{from}..{to}")));

    let changed: Vec<(char, String)> = out
        .lines()
        .filter_map(|l| {
            let mut parts = l.split('\t');
            let status = parts.next()?.chars().next()?;
            // For a rename git prints `old\tnew`; the new path is the last field either way.
            let path = parts.next_back()?.to_string();
            path.ends_with(".jsonl").then_some((status, path))
        })
        .collect();

    if changed.is_empty() {
        println!("no session changes.");
        return Ok(0);
    }

    for (status, path) in &changed {
        let Some(runtime) = runtime_of(path) else { continue };
        let old = show(store, &from, path);
        let new = show(store, &to, path);

        println!();
        match status {
            'D' => {
                println!("● {} {} {}", ui::warn("removed"), runtime, ui::dim(path));
                continue;
            }
            'A' => println!("● {} {} {}", ui::accent("new session"), runtime, ui::dim(path)),
            _ => println!("● updated {} {}", runtime, ui::dim(path)),
        }

        let before = old.as_deref().and_then(|t| read(runtime, t));
        let Some(after) = new.as_deref().and_then(|t| read(runtime, t)) else {
            println!("    {}", ui::dim("(unreadable transcript)"));
            continue;
        };

        // Append-only transcripts: "added" = what's in `after` and not in `before`. A set-difference
        // (not a suffix slice) so a rebase/reorder can't smuggle an unchanged line in as "added".
        let added_prompts = added(&after.prompts, before.as_ref().map(|b| &b.prompts));
        let added_activity = added(&after.activity, before.as_ref().map(|b| &b.activity));

        if added_prompts.is_empty() && added_activity.is_empty() {
            println!("    {}", ui::dim("(no new prompts or tool activity)"));
            continue;
        }
        for p in &added_prompts {
            println!("    {} \"{}\"", ui::accent("+"), ui::one_line(p, 72));
        }
        // Cap the activity so a long session doesn't scroll the terminal off its top.
        const MAX: usize = 20;
        for a in added_activity.iter().take(MAX) {
            println!("    {} {}", ui::dim("+"), ui::dim(&ui::one_line(a, 76)));
        }
        if added_activity.len() > MAX {
            println!("    {}", ui::dim(&format!("… {} more", added_activity.len() - MAX)));
        }
    }
    Ok(0)
}

/// The items in `after` not present in `before` (order preserved). `before == None` (the file did not
/// exist at the "from" ref) makes everything added.
fn added(after: &[String], before: Option<&Vec<String>>) -> Vec<String> {
    match before {
        None => after.to_vec(),
        Some(b) => after.iter().filter(|x| !b.contains(x)).cloned().collect(),
    }
}

/// A store ref's default "before" for a bare `agit a diff`: the upstream if it resolves (unpushed
/// work), else the previous commit, else git's empty tree (a single-commit store).
fn default_from(store: &Path) -> String {
    for r in ["@{u}", "HEAD~1"] {
        if scope::git_in_status(store, &["rev-parse", "--verify", "--quiet", r]).0 == 0 {
            return r.to_string();
        }
    }
    EMPTY_TREE.to_string()
}

/// A file's content at a git ref, or `None` when it does not exist there (a newly added session has no
/// "before"). The empty-tree ref makes every path a clean `None`, which is exactly "everything is new".
fn show(store: &Path, r: &str, path: &str) -> Option<String> {
    let (code, out) = scope::git_in_status(store, &["show", &format!("{r}:{path}")]);
    (code == 0).then_some(out)
}

/// The runtime a session path belongs to: the transcript's parent directory is the runtime dir under
/// both store layouts (`sessions/<rt>/…` and `sessions/<env>/<rt>/…`).
fn runtime_of(path: &str) -> Option<&'static str> {
    let parent = Path::new(path).parent()?.file_name()?.to_str()?;
    session::runtimes().into_iter().find(|rt| *rt == parent)
}

/// A short, stable handle for a session in the `--oneline` view: the transcript's file stem, clipped to
/// the first 8 chars (a UUID stem is otherwise a screenful). Not a git oid — the session IS the unit.
fn short_id(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("session");
    stem.chars().take(8).collect()
}

/// "a, b, c" up to three files, then "a, b and N more" — an edit list is context, not an inventory.
fn edit_summary(edits: &[String]) -> String {
    let base = |p: &str| Path::new(p).file_name().and_then(|n| n.to_str()).unwrap_or(p).to_string();
    let names: Vec<String> = edits.iter().map(|p| base(p)).collect();
    if names.len() <= 3 {
        names.join(", ")
    } else {
        format!("{} and {} more", names[..3].join(", "), names.len() - 3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = concat!(
        "{\"type\":\"user\",\"sessionId\":\"S1\",\"uuid\":\"u1\",\"parentUuid\":null,\"cwd\":\"/code/web\",\"gitBranch\":\"main\",\"message\":{\"role\":\"user\",\"content\":\"add a rate limiter\"}}\n",
        "{\"type\":\"assistant\",\"sessionId\":\"S1\",\"uuid\":\"u2\",\"parentUuid\":\"u1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"on it\"},{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Edit\",\"input\":{\"file_path\":\"/code/web/src/limit.rs\"}},{\"type\":\"tool_use\",\"id\":\"t2\",\"name\":\"Bash\",\"input\":{\"command\":\"cargo test\"}}]}}\n",
    );

    #[test]
    fn distill_pulls_prompts_edits_and_tool_activity_from_a_claude_transcript() {
        let d = read("claude-code", CLAUDE).expect("a claude transcript reads");
        assert_eq!(d.prompts, vec!["add a rate limiter"]);
        assert_eq!(d.edits, vec!["/code/web/src/limit.rs"], "Write/Edit tool calls are edits");
        assert_eq!(d.tool_calls, 2, "Edit + Bash");
        // narrate_call is the source of the activity lines
        assert!(d.activity.iter().any(|a| a.contains("cargo test")), "{:?}", d.activity);
    }

    #[test]
    fn added_is_a_set_difference_not_a_suffix() {
        let before = vec!["one".to_string(), "two".to_string()];
        let after = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        assert_eq!(added(&after, Some(&before)), vec!["three".to_string()]);
        // a wholly-new session (no "before") reports everything as added
        assert_eq!(added(&after, None), after);
        // reordering must not report unchanged lines as added
        let reordered = vec!["two".to_string(), "one".to_string()];
        assert!(added(&reordered, Some(&before)).is_empty());
    }

    #[test]
    fn runtime_of_reads_the_parent_dir_under_both_layouts() {
        assert_eq!(runtime_of("sessions/codex/a.jsonl"), Some("codex"));
        assert_eq!(runtime_of("sessions/web/claude-code/a.jsonl"), Some("claude-code"));
        assert_eq!(runtime_of("sessions/web/notarun/a.jsonl"), None);
    }

    #[test]
    fn edit_summary_caps_at_three_then_counts() {
        assert_eq!(edit_summary(&["/a/x.rs".into(), "/b/y.rs".into()]), "x.rs, y.rs");
        assert_eq!(
            edit_summary(&["/a.rs".into(), "/b.rs".into(), "/c.rs".into(), "/d.rs".into()]),
            "a.rs, b.rs, c.rs and 1 more"
        );
    }
}
