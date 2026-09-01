//! OpenCode adapter.
//!
//! # On-disk format
//!
//! ```text
//! ~/.local/share/opencode/opencode.db     SQLite, WAL mode (with -wal/-shm companion files)
//! ```
//!
//! The jsonl-layout era is over: a session lives across the four tables `session` / `message` /
//! `part` / `project`, and its content is the two JSON TEXT columns `message.data` /
//! `part.data`. The backing store is not a file but a set of database rows, so this adapter
//! reads in two layers (spec and evidence in docs/mechanism-probing/opencode-format.md; every
//! §n below refers to it):
//!
//! * listing / lookup → query the tables directly (the SQL in §5, millisecond-scale);
//! * when raw bytes are wanted → **materialize on demand** into a jsonl cache in the canonical
//!   shape of §4, then take the same "read a file" path as the other runtimes. Two
//!   materializations of a session at rest are byte-identical (verified in §4).
//!
//! # Resume
//!
//! ```bash
//! opencode --session <id>
//! ```
//!
//! Resume **continues in place** (it writes back to the same `session` row), so install never
//! reuses the original id: every id is reminted into a new copy, then `opencode import <file>`
//! writes it to the database. import is insert-ignore (§8.2), so it is idempotent and safe to
//! retry; directory/path/project are rewritten into the cwd context of the import — which is
//! what makes ownership come out right on its own.
//!
//! # Capability: Resumable (§8.3)
//!
//! It installs to disk (the official import writes to the database), it yields a resume command
//! that is certain to run (`opencode --session <id>`), and its last step goes through no private
//! index agit cannot observe — where it lands is the source-of-truth database itself.
//!
//! # Row mutability (why remint instead of editing in place)
//!
//! §3: a row past its terminal state never changes, but **the trailing assistant message still
//! being written, and its parts, are rewritten in place** (`finish`/`tokens`/`time.completed`
//! are filled in afterwards, a tool's `state.status` flips from running to completed).
//! `time_updated` is a noise column; canonical materialization and every comparison must
//! exclude it.

use super::{Adapter, Capability, Event, EventKind, Installed, Next, Session, SessionRef};
use crate::Result;
use anyhow::{Context, bail};
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct OpenCode;

// ── Paths ────────────────────────────────────────────────────────────────────

/// Where the session database lives. OpenCode follows XDG: `$XDG_DATA_HOME/opencode/`,
/// defaulting to `~/.local/share/opencode/` (both spellings occur in practice).
fn db_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty());
    match base {
        Some(x) => Some(x.join("opencode").join("opencode.db")),
        None => {
            let home = std::env::var("HOME").ok()?;
            Some(
                PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("opencode")
                    .join("opencode.db"),
            )
        }
    }
}

/// Cache directory for the materialized canonical jsonl.
///
/// The backing store is not a file but a set of database rows (§9), while every other consumer
/// (the store link's `read_bytes`, doctor's comparison against the live transcript, `import`'s
/// candidate list) wants a **file**. So `resolve` materializes one here per §4, at a path
/// determined by the id — resolving the same session again lands on the same file.
fn cache_dir() -> Result<PathBuf> {
    Ok(crate::infra::config::agit_home()?
        .join("cache")
        .join("opencode"))
}

fn cache_path_in(root: &Path, session_id: &str) -> PathBuf {
    root.join(format!("{session_id}.jsonl"))
}

/// Where the receipt (the export JSON fed to `opencode import`) is kept.
///
/// Not a junk file: import is idempotent, so this receipt is the evidence of what was installed,
/// and it is exactly what the user hands back to `opencode import` to retry by hand.
fn receipt_dir() -> Result<PathBuf> {
    Ok(crate::infra::config::agit_home()?
        .join("imports")
        .join("opencode"))
}

// ── Read-only connection ─────────────────────────────────────────────────────
//
// Follows the precedent of codex_index.rs (§5 names this convention explicitly):
// `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI`. **Never immutable** — under WAL, immutable reads
// torn or stale pages; a read-only connection does not disturb a running opencode (§5 observes
// no lock conflict at all). agit never writes a byte to this database; the only writer is
// `opencode import`.

fn open(path: &Path) -> Option<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()
}

/// Whether every table and column needed is present. When the schema changes, better wholly
/// unavailable than guessing from half a schema (the codex_index rule). §1 calls this out:
/// the `session_message` table exists but is empty, listing does not depend on it, and neither
/// does anything here.
fn schema_ok(con: &Connection) -> bool {
    con.prepare(
        "SELECT id, project_id, parent_id, directory, time_created, version FROM session LIMIT 0",
    )
    .is_ok()
        && con
            .prepare("SELECT id, session_id, time_created, data FROM message LIMIT 0")
            .is_ok()
        && con
            .prepare("SELECT id, message_id, session_id, time_created, data FROM part LIMIT 0")
            .is_ok()
}

/// epoch milliseconds → RFC3339 (UTC). Millisecond epochs are the shape of every time column in
/// the database (§2.1).
fn ms_to_rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn ms_to_systime(ms: i64) -> std::time::SystemTime {
    if ms <= 0 {
        return std::time::SystemTime::UNIX_EPOCH;
    }
    std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64)
}

// ── Listing and lookup (§5) ──────────────────────────────────────────────────

/// The minimal row behind what `sessions_for` / `all_sessions` return. Listing reads only the
/// `session` table and never touches part content (§9).
struct Listed {
    id: String,
    directory: Option<String>,
    time_created: i64,
    time_updated: i64,
}

fn row_to_listed(r: &rusqlite::Row<'_>) -> rusqlite::Result<Listed> {
    Ok(Listed {
        id: r.get(0)?,
        directory: r.get(1)?,
        time_created: r.get(2)?,
        time_updated: r.get(3)?,
    })
}

const LIST_COLS: &str = "SELECT id, directory, time_created, time_updated FROM session";

/// §5: "belongs to a repo" = an exact `directory = repo` match, merged with a
/// `project.worktree = repo` fallback (which covers sessions started from a subdirectory — the
/// directory match misses those). Child sessions (non-empty `parent_id`) are in the full set,
/// with no visibility filter — `opencode session list` shows only root sessions, but hiding
/// subagent sessions here only makes `import` / `doctor` blind to them.
fn list_for_repo(con: &Connection, repo: &str) -> Option<Vec<Listed>> {
    let exact = {
        let sql = format!("{LIST_COLS} WHERE directory = ?1");
        let mut st = con.prepare(&sql).ok()?;
        let rows = st.query_map([repo], row_to_listed).ok()?;
        rows.filter_map(|r| r.ok()).collect::<Vec<_>>()
    };
    let by_worktree = {
        let sql = "SELECT s.id, s.directory, s.time_created, s.time_updated \
                   FROM session s JOIN project p ON p.id = s.project_id WHERE p.worktree = ?1";
        let mut st = con.prepare(sql).ok()?;
        let rows = st.query_map([repo], row_to_listed).ok()?;
        rows.filter_map(|r| r.ok()).collect::<Vec<_>>()
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Listed> = vec![];
    for l in exact.into_iter().chain(by_worktree) {
        if seen.insert(l.id.clone()) {
            out.push(l);
        }
    }
    out.sort_by_key(|l| l.time_created);
    Some(out)
}

fn list_all(con: &Connection) -> Option<Vec<Listed>> {
    let sql = format!("{LIST_COLS} ORDER BY time_created");
    let mut st = con.prepare(&sql).ok()?;
    let rows = st.query_map([], row_to_listed).ok()?;
    Some(rows.filter_map(|r| r.ok()).collect())
}

fn to_ref(l: Listed, cache_root: &Path) -> SessionRef {
    SessionRef {
        // The content is in the database, not at this file; the path is where the materialized
        // cache belongs. It exists only after a resolve (see the comment on [`cache_dir`]).
        path: cache_path_in(cache_root, &l.id),
        id: l.id,
        runtime: "opencode",
        cwd: l.directory,
        mtime: ms_to_systime(l.time_updated),
        gist: None,
    }
}

// ── Canonical materialization (§4) ───────────────────────────────────────────
//
// The shape (an agit-defined spec; byte stability verified in §4):
//
//   {"id":"ses_...","kind":"opencode.meta","project_id":...,...}     ← immutable columns only
//   {"id":"msg_...","kind":"message","session_id":...,"time_created":...,"data":...}
//   {"id":"prt_...","kind":"part","message_id":...,"session_id":...,"time_created":...,"data":...}
//
// Rules: the meta line carries no title/cost/tokens/time_updated (all of them change); `data` is
// always the TEXT read out of the database **embedded verbatim**, never reserialized — once a row
// is at rest, rereading it yields identical bytes. Order per §2.3: (time_created, kind, id), with
// message before part inside the same millisecond (a part's time_created is observed to be
// strictly greater than its host message's; the kind order is only a fuse).
//
// Envelope key order comes from serde_json's BTreeMap (alphabetical) — the repo's envelope hash
// discipline already rests on one serialization shape, determinism is the requirement, and the
// key order drawn above is only illustrative.

struct Materialized {
    text: String,
    /// `session.time_updated`: the mtime written onto the materialized file, so the link's
    /// "has it moved" test (transcript mtime vs link mtime) keeps holding over a set of
    /// database rows.
    updated_ms: i64,
}

fn materialize(con: &Connection, session_id: &str) -> Option<Materialized> {
    let meta = con
        .query_row(
            "SELECT id, project_id, parent_id, directory, time_created, time_updated, version \
             FROM session WHERE id = ?1",
            [session_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .ok()?;
    let (id, project_id, parent_id, directory, created_ms, updated_ms, version) = meta;

    // The meta line carries immutable columns only (§4 rule 1): title is generated
    // asynchronously and changes, cost/tokens/time_updated all change, so none of them go in.
    let mut lines: Vec<(i64, u8, String, String)> = vec![];
    lines.push((
        created_ms,
        0,
        id.clone(),
        serde_json::json!({
            "id": id,
            "kind": "opencode.meta",
            "project_id": project_id,
            "parent_id": parent_id,
            "directory": directory,
            "time_created": created_ms,
            "version": version,
        })
        .to_string(),
    ));

    let mut ms = con
        .prepare("SELECT id, time_created, data FROM message WHERE session_id = ?1")
        .ok()?;
    let msg_rows = ms
        .query_map([session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .ok()?;
    for row in msg_rows.filter_map(|r| r.ok()) {
        let (mid, tc, data) = row;
        let q = |s: &str| serde_json::Value::String(s.to_string()).to_string();
        lines.push((
            tc,
            0,
            mid.clone(),
            format!(
                "{{\"id\":{},\"kind\":\"message\",\"session_id\":{},\"time_created\":{},\"data\":{}}}",
                q(&mid),
                q(&id),
                tc,
                data
            ),
        ));
    }

    let mut ps = con
        .prepare("SELECT id, message_id, time_created, data FROM part WHERE session_id = ?1")
        .ok()?;
    let part_rows = ps
        .query_map([session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .ok()?;
    for row in part_rows.filter_map(|r| r.ok()) {
        let (pid, mid, tc, data) = row;
        let q = |s: &str| serde_json::Value::String(s.to_string()).to_string();
        lines.push((
            tc,
            1,
            pid.clone(),
            format!(
                "{{\"id\":{},\"kind\":\"part\",\"message_id\":{},\"session_id\":{},\"time_created\":{},\"data\":{}}}",
                q(&pid),
                q(&mid),
                q(&id),
                tc,
                data
            ),
        ));
    }

    // Canonical order (time_created, kind, id) — the kind order (message=0 before part=1) is
    // only a fuse for rows that share a millisecond.
    lines.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut text = lines
        .into_iter()
        .map(|(_, _, _, l)| l)
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    Some(Materialized { text, updated_ms })
}

// ── canonical → IR (the §9.1 mapping table) ──────────────────────────────────

struct RawMsg {
    id: String,
    lineno: usize,
    time_created: i64,
    role: String,
    mode: Option<String>,
}

struct RawPart {
    message_id: String,
    lineno: usize,
    time_created: i64,
    data: serde_json::Value,
}

/// Timestamp of a part event: `data.time.start` wins (the §9.1 assistant text row), falling back
/// to the row-level `time_created` when it is absent — both are epoch milliseconds (§2.1).
fn part_ts(p: &RawPart) -> Option<String> {
    let ms = p
        .data
        .get("time")
        .and_then(|t| t.get("start"))
        .and_then(|x| x.as_i64())
        .unwrap_or(p.time_created);
    ms_to_rfc3339(ms)
}

/// Map a part to an event. **The test is the field, not the prefix** (§6): an injected prompt
/// carries `"synthetic": true`, and going by a text prefix instead misreads a
/// `<system-reminder>` that happens to talk about itself as an injection.
fn part_event(p: &RawPart, role: &str, host_is_compaction: bool) -> Option<Event> {
    let ty = p.data.get("type").and_then(|x| x.as_str()).unwrap_or("");
    let ts = part_ts(p);
    match ty {
        "text" => {
            let t = p.data.get("text").and_then(|x| x.as_str())?;
            if t.trim().is_empty() {
                return None;
            }
            let kind = match role {
                // §6: synthetic==true → no UserPrompt; an injected prompt counts as Other (the
                // text is kept so it can be read back) and stays out of counts().prompts. The
                // fallback direction is the safe one: a future injection carrying no flag lands
                // in UserPrompt, which only overcounts.
                "user" if is_synthetic(p) => EventKind::Other,
                "user" => EventKind::UserPrompt,
                // On the normal path a compaction summary has already been merged into the
                // boundary event; reaching here leaves only the truncated shape where the
                // boundary message is missing — that is a second-hand recap, not a reply.
                "assistant" if host_is_compaction => EventKind::Other,
                "assistant" => EventKind::AssistantReply,
                // Unknown role: allowlist philosophy (as in the codex adapter) — the worst
                // outcome is that it lands in dropped, not that the model is credited with
                // words it never said.
                _ => EventKind::Other,
            };
            Some(Event::text(kind, t, ts).at_line(p.lineno))
        }
        "tool" => {
            let name = p
                .data
                .get("tool")
                .and_then(|x| x.as_str())
                .unwrap_or("tool")
                .to_string();
            // §9.1: `state.input.filePath` goes into paths; `state.output` never enters the IR
            // (a single part can reach 8.1 MB) and is fetched back from raw via Event.line.
            let paths = p
                .data
                .pointer("/state/input/filePath")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| vec![s.to_string()])
                .unwrap_or_default();
            Some(Event {
                // §9.1: tool ∈ {edit, write} is promoted to FileEdit (an allowlist, same
                // reasoning as the cursor adapter; the names are this runtime's observed
                // shape). A miss only undercounts edits; it never reports a read as a write.
                kind: if matches!(name.as_str(), "edit" | "write") {
                    EventKind::FileEdit
                } else {
                    EventKind::ToolUse
                },
                text: Some(name.clone()),
                timestamp: ts,
                paths,
                tool: Some(name),
                line: Some(p.lineno),
            })
        }
        // Plaintext reasoning: not rendered (the same stance the other adapters take on
        // thinking), still counted.
        "reasoning" => Some(other_event(ts).at_line(p.lineno)),
        // patch is a receipt-shaped event for a file change; its hash points at the snapshot/
        // sidecar. files[] goes into paths; it is not promoted to FileEdit, which would count
        // the same edit twice alongside the edit/write event.
        "patch" => {
            let paths = p
                .data
                .get("files")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            let mut e = other_event(ts).at_line(p.lineno);
            e.paths = paths;
            Some(e)
        }
        // Attachments are handled at the message level (attached to the paths of the owning
        // UserPrompt/ToolUse); no event here.
        "file" => None,
        // Known structural parts: no event and not counted as dropped. step-finish's
        // tokens/cost stay in raw and are read back via Event.line; turns are delimited by user
        // prompts. No TurnEnd — opencode has no explicit turn-terminating record (§9.1 note).
        "step-start" | "step-finish" => None,
        // A compaction part is handled at the message level (the §7 boundary); no event here.
        "compaction" => None,
        // subtask/snapshot/agent from the §2.2 documentation, and future types: counted as
        // Other (into dropped), matching the codex adapter's unknown_record_types_are_counted —
        // discarding silently breaks the rule set at the top of adapter/mod.rs.
        _ => Some(other_event(ts).at_line(p.lineno)),
    }
}

fn is_synthetic(p: &RawPart) -> bool {
    p.data
        .get("synthetic")
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

fn other_event(ts: Option<String>) -> Event {
    Event {
        kind: EventKind::Other,
        text: None,
        timestamp: ts,
        paths: vec![],
        tool: None,
        line: None,
    }
}

// ── id minting (the §9 mint line) ────────────────────────────────────────────

/// `ses_` / `msg_` / `prt_` + 12 hex digits of the epoch millisecond + 14 random characters.
///
/// The observed shape `ses_0323b657bffeakH0buzVroXSsV` is exactly this structure (§9: `ses_`
/// plus a 24-character random string already passes import validation; the time prefix is there
/// only so the listing sorts nicely). import validates only that every id is a **string**, and
/// that a message with no parent drops the key instead of setting it to null (§8.2).
fn mint_key(prefix: &str) -> String {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let entropy = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}{now_ms:012x}{}", &entropy[..14])
}

// ── export shape and install (§8) ────────────────────────────────────────────
//
// The import validation surface, pinned by observation (the opencode 1.18.13 binary on this
// machine; the schema discriminates by role):
//   info                id / slug / title are required; directory and friends are rewritten
//                       into the cwd context of the import
//   user message        agent and model:{providerID, modelID} are required
//   assistant message   modelID / providerID are required (flat keys, not a model object);
//                       with no parent, parentID is **dropped** — null is rejected
//   tool part           a metadata key under state is required (an empty object will do)
// Idempotent: reimporting the same id does not overwrite content (insert-ignore), so retrying
// is safe.

/// Cross-runtime rendering (IR → export JSON).
///
/// The lossy path only (another family → opencode). opencode → opencode does not come here —
/// a same-family install goes through [`canonical_to_export`], which keeps reasoning, tool
/// output and the synthetic flag in full.
fn render_export(
    session: &Session,
    new_id: &str,
    cwd: &Path,
    details: &super::ToolDetails,
) -> Result<serde_json::Value> {
    let cwd_s = cwd.to_string_lossy().to_string();
    let mut messages: Vec<serde_json::Value> = vec![];
    let mut prev_msg: Option<String> = None;
    let mut first_ms: Option<i64> = None;
    let mut last_ms: Option<i64> = None;

    for (ei, e) in session.events.iter().enumerate() {
        let ms = event_ms(e);
        first_ms = first_ms.or(ms);
        if ms.is_some() {
            last_ms = ms;
        }
        let created = ms.unwrap_or(FIXED_MS);
        let msg = match e.kind {
            EventKind::UserPrompt | EventKind::AssistantReply => {
                let Some(t) = e.text.as_deref().filter(|t| !t.trim().is_empty()) else {
                    continue;
                };
                if e.kind == EventKind::UserPrompt {
                    user_msg(new_id, created, t, &mut prev_msg)
                } else {
                    assistant_msg(new_id, created, t, &cwd_s, &mut prev_msg, None)
                }
            }
            // The compact boundary **is emitted** (the same rendering policy as the other two
            // adapters): the summary is legitimate opening context once a compacted session is
            // resumed, so it goes out as an ordinary user message; the text of a filtered
            // boundary is not conversation content, so emitting it requires an explicit label.
            // An interjection takes the same path as a summary: both are user-side context the
            // agent has to read, so both go out as user messages.
            EventKind::UserInterjection | EventKind::CompactSummary => {
                let Some(t) = e.text.as_deref().filter(|t| !t.trim().is_empty()) else {
                    continue;
                };
                user_msg(new_id, created, t, &mut prev_msg)
            }
            EventKind::CompactFiltered => user_msg(
                new_id,
                created,
                &format!(
                    "(this context was compact-filtered by the source runtime: {})",
                    e.text
                        .as_deref()
                        .filter(|t| !t.trim().is_empty())
                        .unwrap_or("window / kept-count unknown")
                ),
                &mut prev_msg,
            ),
            EventKind::ToolUse => assistant_msg(
                new_id,
                created,
                "",
                &cwd_s,
                &mut prev_msg,
                Some((e, details.get(ei))),
            ),
            // A receipt-shaped edit (marked by the source extractor, see
            // ToolDetails::receipts): the real call is another ToolUse event in the same
            // transcript, so minting a second tool part makes one edit appear twice.
            EventKind::FileEdit => {
                if details.is_receipt(ei) {
                    continue;
                }
                assistant_msg(
                    new_id,
                    created,
                    "",
                    &cwd_s,
                    &mut prev_msg,
                    Some((e, details.get(ei))),
                )
            }
            // Other is what the IR cannot express (encrypted reasoning and the like); TurnEnd
            // is an internal signal of the source runtime; a ToolResult's text is paired up by
            // enrich and emitted with the call's tool part, so replaying it on its own
            // duplicates it — none of the three are replayed (as in the other two adapters).
            EventKind::ToolResult | EventKind::TurnEnd | EventKind::Other => continue,
        };
        messages.push(msg);
    }

    let created = first_ms.unwrap_or(FIXED_MS);
    let updated = last_ms.unwrap_or(created);
    Ok(serde_json::json!({
        "info": {
            "id": new_id,
            // slug is a required key (omitting it is rejected); an exported copy has no real
            // slug to use, so one is derived from the new id and the listing shows it was
            // installed by agit.
            "slug": slug_for_id(new_id),
            "projectID": "global",
            "directory": cwd_s,
            "path": "",
            "title": session
                .gist(60)
                .unwrap_or_else(|| "(session imported by agit)".into()),
            "agent": "build",
            "version": env!("CARGO_PKG_VERSION"),
            "time": { "created": created, "updated": updated },
        },
        "messages": messages,
    }))
}

/// Event timestamp (an RFC3339 string) → epoch milliseconds; None when there is none, and the
/// caller substitutes the fixed value.
fn event_ms(e: &Event) -> Option<i64> {
    let ts = e.timestamp.as_deref()?;
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// Rendering must be a pure function (the same comment stands in claude_code): an event with no
/// timestamp gets a fixed value, otherwise repeated conversion shows git a file that changes
/// forever. 2026-01-01T00:00:00.000Z.
const FIXED_MS: i64 = 1767225600000;

fn slug_for_id(id: &str) -> String {
    let tail: String = id.chars().skip(4).take(8).collect();
    format!("agit-{tail}")
}

/// Messages and parts are minted with the same mint_key as a session, only the prefix differs —
/// the prefix marks the shape (ses_/msg_/prt_), and within one rendering the time prefix is
/// shared while the random tail separates them.
///
/// A user message (export shape). Required in practice: agent, model:{providerID, modelID}.
///
/// Model attribution is per message (§2.1) while the IR carries no model — writing "agit" as the
/// provider is an honest provenance mark (the codex adapter writes `originator: "agit"` for the
/// same reason).
fn user_msg(sid: &str, created: i64, text: &str, prev: &mut Option<String>) -> serde_json::Value {
    let mid = mint_key("msg_");
    let v = serde_json::json!({
        "info": {
            "id": mid,
            "sessionID": sid,
            "role": "user",
            "time": { "created": created },
            "agent": "build",
            "model": { "providerID": "agit", "modelID": "unknown" },
        },
        "parts": [{
            "id": mint_key("prt_"),
            "sessionID": sid,
            "messageID": mid,
            "type": "text",
            "text": text,
        }],
    });
    *prev = Some(mid);
    v
}

/// An assistant message (export shape). Required in practice: the flat modelID / providerID
/// keys; with no parent, `parentID` is **dropped** (import rejects null).
///
/// When `tool_event` is set, this message carries a real tool part, so a cross-vendor resume
/// keeps the structure of which tool was called instead of degrading it to prose the way the
/// claude_code side does.
fn assistant_msg(
    sid: &str,
    created: i64,
    text: &str,
    cwd: &str,
    prev: &mut Option<String>,
    tool_event: Option<(&Event, Option<&super::ToolDetail>)>,
) -> serde_json::Value {
    let mid = mint_key("msg_");
    let mut info = serde_json::json!({
        "id": mid,
        "sessionID": sid,
        "role": "assistant",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": cwd, "root": cwd },
        "cost": 0,
        "tokens": { "input": 0, "output": 0, "reasoning": 0,
                    "cache": { "read": 0, "write": 0 } },
        "modelID": "unknown",
        "providerID": "agit",
        "time": { "created": created, "completed": created },
        "finish": "stop",
    });
    if let Some(p) = prev.as_ref() {
        info["parentID"] = serde_json::Value::String(p.clone());
    }
    let mut parts: Vec<serde_json::Value> = vec![];
    if !text.trim().is_empty() {
        parts.push(serde_json::json!({
            "id": mint_key("prt_"),
            "sessionID": sid,
            "messageID": mid,
            "type": "text",
            "text": text,
        }));
    }
    if let Some((te, td)) = tool_event {
        parts.push(tool_part(te, sid, &mid, created, td));
    }
    *prev = Some(mid);
    serde_json::json!({ "info": info, "parts": parts })
}

/// A tool part (export shape). The `state.metadata` key must exist (an empty object will do).
///
/// The IR keeps neither tool input nor output; enrich fetches them back from the source
/// transcript and passes them in. When it cannot, the input is just filePath and `state.output`
/// is written as the empty string — a real loss, not disguised.
fn tool_part(
    e: &Event,
    sid: &str,
    mid: &str,
    ms: i64,
    detail: Option<&super::ToolDetail>,
) -> serde_json::Value {
    let name = e.tool.clone().unwrap_or_else(|| "tool".into());
    let input = detail
        .and_then(|d| d.input.clone())
        .unwrap_or_else(|| match e.paths.first() {
            Some(p) => serde_json::json!({ "filePath": p }),
            None => serde_json::json!({}),
        });
    let output = detail.and_then(|d| d.output.clone()).unwrap_or_default();
    let failed = detail.is_some_and(|d| d.error);
    let mut part = serde_json::json!({
        "id": mint_key("prt_"),
        "sessionID": sid,
        "messageID": mid,
        "type": "tool",
        "tool": name,
        "callID": mint_key("call_"),
        "state": {
            // A failure is written as it happened: the text goes in the error key (OpenCode's
            // own shape) and output stays empty; a successful part carries no error key,
            // matching the shape OpenCode writes itself.
            "status": if failed { "error" } else { "completed" },
            "input": input,
            "output": if failed { String::new() } else { output.clone() },
            "title": e.tool.clone().unwrap_or_else(|| "tool".into()),
            "metadata": {},
            "time": { "start": ms, "end": ms },
        },
    });
    if failed {
        part["state"]["error"] = serde_json::Value::String(output);
    }
    part
}

/// The same-family lossless path: canonical jsonl → export JSON, with **every id reminted**.
///
/// Why not the line-by-line rewrite of [`rewrite_identity`](crate::domain::install): it knows
/// only a single identity key, while an opencode copy needs all three id layers
/// (session/message/part) replaced and **internal references** such as `parentID` /
/// `tail_start_id` rewritten along with them (§8.2 requires it: a non-empty reference must point
/// at an id that exists). That is beyond "rewrite an identity field line by line", so the
/// byte-rewriting branch for format="opencode" is deliberately absent and every remint happens
/// here.
fn canonical_to_export(content: &str, new_id: &str, cwd: &Path) -> Result<serde_json::Value> {
    let mut meta: Option<serde_json::Map<String, serde_json::Value>> = None;
    let mut msg_order: Vec<String> = vec![];
    let mut msg_data: HashMap<String, serde_json::Value> = HashMap::new();
    let mut part_order: Vec<(String, String)> = vec![]; // (part_id, message_id)
    let mut part_data: HashMap<String, serde_json::Value> = HashMap::new();
    let mut last_ms: i64 = 0;

    for (lineno, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("canonical line {lineno} is not valid JSON"))?;
        match v.get("kind").and_then(|x| x.as_str()).unwrap_or("") {
            "opencode.meta" => {
                meta = v.as_object().cloned();
            }
            "message" => {
                let id = v
                    .get("id")
                    .and_then(|x| x.as_str())
                    .with_context(|| format!("canonical line {lineno} has no id"))?;
                let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
                last_ms = last_ms.max(v.get("time_created").and_then(|x| x.as_i64()).unwrap_or(0));
                msg_order.push(id.to_string());
                msg_data.insert(id.to_string(), data);
            }
            "part" => {
                let id = v
                    .get("id")
                    .and_then(|x| x.as_str())
                    .with_context(|| format!("canonical line {lineno} has no id"))?;
                let mid = v
                    .get("message_id")
                    .and_then(|x| x.as_str())
                    .with_context(|| format!("canonical line {lineno} has no message_id"))?;
                let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
                last_ms = last_ms.max(v.get("time_created").and_then(|x| x.as_i64()).unwrap_or(0));
                part_order.push((id.to_string(), mid.to_string()));
                part_data.insert(id.to_string(), data);
            }
            other => bail!("canonical line {lineno} has unknown kind `{other}`"),
        }
    }
    let meta = meta.context("no opencode.meta line in canonical")?;

    // ── Mint the id map: session → new_id, each old id → a new id of the same shape ──
    let mut map: HashMap<String, String> = HashMap::new();
    if let Some(old) = meta.get("id").and_then(|x| x.as_str()) {
        map.insert(old.to_string(), new_id.to_string());
    }
    let msg_new: HashMap<&str, String> = msg_order
        .iter()
        .map(|m| (m.as_str(), mint_key("msg_")))
        .collect();
    for (o, n) in &msg_new {
        map.insert(o.to_string(), n.clone());
    }
    let part_new: HashMap<&str, String> = part_order
        .iter()
        .map(|(p, _)| (p.as_str(), mint_key("prt_")))
        .collect();
    for (o, n) in &part_new {
        map.insert(o.to_string(), n.clone());
    }

    // Assemble messages: keep canonical order, hang parts on their host message (`messageID`
    // takes the host's new id; a row whose host is gone stays honest — refusing to install beats
    // a set of database rows pointing at nothing).
    let mut messages: Vec<serde_json::Value> = vec![];
    let mut parts_by_msg: HashMap<&str, Vec<serde_json::Value>> = HashMap::new();
    for (pid, omid) in &part_order {
        let Some(nmid) = msg_new.get(omid.as_str()) else {
            bail!("part {pid} has host message {omid} that is not in the canonical data");
        };
        let mut data = part_data[pid].clone();
        remap_ids(&mut data, &map);
        let o = data.as_object_mut().context("part.data is not an object")?;
        o.insert("id".into(), part_new[pid.as_str()].clone().into());
        o.insert("sessionID".into(), new_id.into());
        o.insert("messageID".into(), nmid.clone().into());
        parts_by_msg.entry(omid).or_default().push(data);
    }
    for omid in &msg_order {
        let mut data = msg_data[omid].clone();
        remap_ids(&mut data, &map);
        let o = data
            .as_object_mut()
            .context("message.data is not an object")?;
        o.insert("id".into(), msg_new[omid.as_str()].clone().into());
        o.insert("sessionID".into(), new_id.into());
        let parts = parts_by_msg.remove(omid.as_str()).unwrap_or_default();
        messages.push(serde_json::json!({ "info": data, "parts": parts }));
    }

    // info: meta brings only immutable columns, so slug/title were never in it (they change).
    // directory is written as the target cwd — import rewrites ownership into the cwd context of
    // the import anyway (§8.2). **No parentID**: the lineage between a copy and its source
    // session is recorded explicitly by the store link (the install module documentation), and a
    // parent reference pointing back into the original database only misleads.
    let title = gist_from(&messages).unwrap_or_else(|| "(copy imported by agit)".to_string());
    let created = meta
        .get("time_created")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let cwd_s = cwd.to_string_lossy().to_string();
    Ok(serde_json::json!({
        "info": {
            "id": new_id,
            "slug": slug_for_id(new_id),
            "projectID": meta.get("project_id").cloned().unwrap_or("global".into()),
            "directory": cwd_s,
            "path": "",
            "title": title,
            "agent": "build",
            "version": meta.get("version").cloned().unwrap_or("unknown".into()),
            "time": { "created": created, "updated": last_ms.max(created) },
        },
        "messages": messages,
    }))
}

/// Recursively replace a string **whose value is exactly** an old id with the new id.
///
/// Only whole-string equality is replaced, never a substring: user text can perfectly well
/// mention an id (pasting a log, showing someone how to use the CLI), and rewriting user text is
/// an unacceptable loss. In the other direction, the known reference shapes (an assistant's
/// `parentID`, a compaction's `tail_start_id`) are all a whole value that is one id, so
/// whole-string equality covers them — and covers a reference field added later for free.
fn remap_ids(v: &mut serde_json::Value, map: &HashMap<String, String>) {
    match v {
        serde_json::Value::String(s) => {
            if let Some(n) = map.get(s.as_str()) {
                *s = n.clone();
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| remap_ids(x, map)),
        serde_json::Value::Object(o) => o.values_mut().for_each(|x| remap_ids(x, map)),
        _ => {}
    }
}

/// Take the first genuine user text out of export-shaped messages as the title.
fn gist_from(messages: &[serde_json::Value]) -> Option<String> {
    for m in messages {
        let role = m.pointer("/info/role").and_then(|x| x.as_str());
        if role != Some("user") {
            continue;
        }
        for p in m
            .get("parts")
            .and_then(|x| x.as_array())
            .into_iter()
            .flatten()
        {
            if p.get("type").and_then(|x| x.as_str()) != Some("text") {
                continue;
            }
            if p.get("synthetic")
                .and_then(|x| x.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            let t = p.get("text").and_then(|x| x.as_str())?.trim();
            if t.is_empty() {
                continue;
            }
            let one = t.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut s: String = one.chars().take(60).collect();
            if one.chars().count() > 60 {
                s.push('…');
            }
            return Some(s);
        }
    }
    None
}

/// install's input comes in two shapes — **discriminated by content** (the spirit of
/// infer_runtime):
///
/// * canonical jsonl (the same-family lossless path; `domain::install` does no byte rewriting
///   for format="opencode", and `rewrite_identity` deliberately has no branch for it): remint
///   everything;
/// * export JSON (the cross-family lossy path; [`render_export`] has already minted the ids):
///   persisted unchanged, because minting again would replace the new_id the resume command
///   refers to.
fn canonical_or_export(content: &str) -> bool {
    // true = canonical.
    content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .is_some_and(|v| v.get("kind").and_then(|x| x.as_str()) == Some("opencode.meta"))
}

/// Produce the export JSON string to feed to `opencode import`.
fn import_payload(content: &str, new_id: &str, cwd: &Path) -> Result<String> {
    let v = if canonical_or_export(content) {
        canonical_to_export(content, new_id, cwd)?
    } else {
        // The check at the exit of the rendering path: a single JSON object carrying a
        // messages array — otherwise the error happens **before anything reaches disk**.
        let v: serde_json::Value = serde_json::from_str(content)
            .context("neither canonical nor export shape — cannot install into OpenCode")?;
        if v.get("messages").and_then(|x| x.as_array()).is_none() {
            bail!(
                "export shape is missing the messages array — refusing to install (a broken file would make `opencode import` fail with an unreadable error)"
            );
        }
        v
    };
    // export is indented with two spaces (§8.1). Key order is serde_json's default
    // (alphabetical) — the import side parses by schema, where key order carries nothing.
    Ok(serde_json::to_string_pretty(&v)?)
}

/// Run the official import once (the command is prepared by the caller; import is idempotent
/// insert-ignore). On failure, surface stderr and name the receipt file — once the environment is
/// fixed it can be completed by hand.
fn run_import(cmd: &mut std::process::Command, file: &Path, cwd: &Path) -> Result<()> {
    let out = cmd
        .arg("import")
        .arg(file)
        .current_dir(cwd)
        .output()
        .context("failed to run opencode import")?;
    if out.status.success() {
        return Ok(());
    }
    bail!(
        "`opencode import` failed (exit {:?}): {}\n  \
         the receipt file is at {}; after fixing the environment you can `opencode import` it manually (idempotent).",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).trim(),
        file.display()
    )
}

impl Adapter for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn cli(&self) -> &'static str {
        "opencode"
    }

    /// §8.3: it installs to disk (import writes to the database), it yields a resume command
    /// certain to run, and its last step goes through no private index — Resumable.
    fn capability(&self) -> Capability {
        Capability::Resumable
    }

    fn format(&self) -> &'static str {
        // A family of its own: canonical jsonl (§4). Different from codex / claude-code, so
        // installing across families always goes through the IR (lossy); a same-family install
        // goes through canonical_to_export's full remint.
        "opencode"
    }

    fn sessions_for(&self, repo: &Path) -> Result<Vec<SessionRef>> {
        let Some(db) = db_path() else {
            // OpenCode not installed = no sessions; a normal state, not an error.
            return Ok(vec![]);
        };
        let want = repo.to_string_lossy().to_string();
        let cache = cache_dir()?;
        let Some(con) = open(&db) else {
            return Ok(vec![]);
        };
        if !schema_ok(&con) {
            return Ok(vec![]);
        }
        Ok(list_for_repo(&con, &want)
            .unwrap_or_default()
            .into_iter()
            .map(|l| to_ref(l, &cache))
            .collect())
    }

    fn all_sessions(&self) -> Result<Vec<SessionRef>> {
        let Some(db) = db_path() else {
            return Ok(vec![]);
        };
        let cache = cache_dir()?;
        let Some(con) = open(&db) else {
            return Ok(vec![]);
        };
        if !schema_ok(&con) {
            return Ok(vec![]);
        }
        Ok(list_all(&con)
            .unwrap_or_default()
            .into_iter()
            .map(|l| to_ref(l, &cache))
            .collect())
    }

    /// Lookup: straight through the primary key (§1); when the row exists, materialize the
    /// canonical form into the cache per §4 and return that path.
    ///
    /// The backing store is not a file but a set of database rows (§9) — materialization is the
    /// one step that turns those rows into the "one file" shape every other consumer knows.
    /// Rewritten every time: the session may still be growing, so the cache has to carry the
    /// latest content; the mtime is set to `time_updated` so the link's "has it moved" test
    /// keeps holding. A missing database, a missing session, and a cache that cannot be written
    /// all honestly report None (not found) instead of handing back stale bytes.
    fn resolve(&self, session_id: &str, _cwd: Option<&Path>) -> Option<PathBuf> {
        let db = db_path()?;
        let con = open(&db)?;
        if !schema_ok(&con) {
            return None;
        }
        let m = materialize(&con, session_id)?;
        let dir = cache_dir().ok()?;
        std::fs::create_dir_all(&dir).ok()?;
        let path = cache_path_in(&dir, session_id);
        std::fs::write(&path, &m.text).ok()?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .ok()?
            .set_modified(ms_to_systime(m.updated_ms))
            .ok()?;
        Some(path)
    }

    fn parse(&self, text: &str) -> Result<Session> {
        let mut id = String::new();
        let mut cwd = None;
        let mut events = vec![];

        // Read the structure out in one streaming pass, then map at the message level (§9.1):
        // the tests a part is judged by (role, mode, synthetic) all come from its host message.
        // Line numbers are recorded on every raw line, blank and corrupt lines included —
        // `Event::line` has to locate that exact line in the cache file.
        let mut msgs: Vec<RawMsg> = vec![];
        let mut parts: Vec<RawPart> = vec![];
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Skip a corrupt line (as in the other three adapters): a transcript can be
            // truncated, and a truncated session is still worth resuming.
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            match v.get("kind").and_then(|x| x.as_str()).unwrap_or("") {
                "opencode.meta" => {
                    if id.is_empty()
                        && let Some(s) = v.get("id").and_then(|x| x.as_str())
                    {
                        id = s.to_string();
                    }
                    if cwd.is_none()
                        && let Some(c) = v
                            .get("directory")
                            .and_then(|x| x.as_str())
                            .filter(|c| !c.is_empty())
                    {
                        cwd = Some(c.to_string());
                    }
                }
                "message" => {
                    let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
                    let tc = v.get("time_created").and_then(|x| x.as_i64()).unwrap_or(0);
                    if id.is_empty()
                        && let Some(s) = v.get("session_id").and_then(|x| x.as_str())
                    {
                        id = s.to_string();
                    }
                    msgs.push(RawMsg {
                        id: v
                            .get("id")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        lineno,
                        time_created: tc,
                        role: data
                            .get("role")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        mode: data.get("mode").and_then(|x| x.as_str()).map(String::from),
                    });
                }
                "part" => {
                    parts.push(RawPart {
                        message_id: v
                            .get("message_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        lineno,
                        time_created: v.get("time_created").and_then(|x| x.as_i64()).unwrap_or(0),
                        data: v.get("data").cloned().unwrap_or(serde_json::Value::Null),
                    });
                }
                // An unrecognized outer line in canonical (a kind added by a later version):
                // counted as Other (into dropped), never silently.
                _ => events.push(other_event(None).at_line(lineno)),
            }
        }

        // Message-level mapping. Parts are grouped by host, keeping the canonical stream order.
        let mut by_msg: HashMap<&str, Vec<&RawPart>> = HashMap::new();
        for p in &parts {
            by_msg.entry(p.message_id.as_str()).or_default().push(p);
        }
        let msg_index: HashMap<&str, usize> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id.as_str(), i))
            .collect();
        let mut consumed: HashSet<usize> = HashSet::new();
        let mut claimed: HashSet<usize> = HashSet::new();

        for (i, m) in msgs.iter().enumerate() {
            if consumed.contains(&i) {
                continue;
            }
            let mps = by_msg.get(m.id.as_str()).cloned().unwrap_or_default();
            for p in &mps {
                claimed.insert(p.lineno);
            }

            // §7 compaction comes in three pieces: the boundary (a user message holding a
            // compaction part) + the summary (the text part of an assistant message with
            // mode=="compaction") + the continuation injection (synthetic user text, which takes
            // the ordinary mapping and lands in Other).
            if mps
                .iter()
                .any(|p| p.data.get("type").and_then(|x| x.as_str()) == Some("compaction"))
            {
                // The summary text is merged into this one CompactSummary event (the §9.1
                // merge rule): a summary is lossy and must never be fed to merge; its text is
                // for display only. The boundary parameters (auto/overflow/tail_start_id) do
                // not enter the IR and are read back from raw via Event.line.
                let boundary_line = mps
                    .iter()
                    .find(|p| p.data.get("type").and_then(|x| x.as_str()) == Some("compaction"))
                    .map(|p| p.lineno)
                    .unwrap_or(m.lineno);
                let summary = msgs.get(i + 1).and_then(|n| {
                    if n.mode.as_deref() != Some("compaction") {
                        return None;
                    }
                    consumed.insert(i + 1);
                    for p in by_msg.get(n.id.as_str()).cloned().unwrap_or_default() {
                        claimed.insert(p.lineno);
                    }
                    let texts: Vec<&str> = by_msg
                        .get(n.id.as_str())?
                        .iter()
                        .filter(|p| p.data.get("type").and_then(|x| x.as_str()) == Some("text"))
                        .filter_map(|p| p.data.get("text").and_then(|x| x.as_str()))
                        .filter(|t| !t.trim().is_empty())
                        .collect();
                    (!texts.is_empty()).then(|| texts.join("\n"))
                });
                let mut e = match summary {
                    Some(t) => {
                        Event::text(EventKind::CompactSummary, t, ms_to_rfc3339(m.time_created))
                    }
                    None => Event {
                        kind: EventKind::CompactSummary,
                        text: None,
                        timestamp: ms_to_rfc3339(m.time_created),
                        paths: vec![],
                        tool: None,
                        line: None,
                    },
                };
                e.line = Some(boundary_line);
                events.push(e);

                // The boundary message's **other** parts, if any, map as usual — part_event
                // already skips the compaction part.
                for p in &mps {
                    if let Some(ev) =
                        part_event(p, &m.role, m.mode.as_deref() == Some("compaction"))
                    {
                        events.push(ev);
                    }
                }
                continue;
            }

            // An ordinary message: parts map one by one; a file part attaches to the paths of
            // the owning UserPrompt / ToolUse (§9.1) and records the filename — the base64 body
            // does not enter the IR (a single 8.1 MB attachment would wreck the transcript
            // page).
            let mut first_prompt: Option<usize> = None;
            let mut first_tool: Option<usize> = None;
            let mut files: Vec<String> = vec![];
            for p in &mps {
                if p.data.get("type").and_then(|x| x.as_str()) == Some("file") {
                    if let Some(f) = p.data.get("filename").and_then(|x| x.as_str()) {
                        files.push(f.to_string());
                    }
                    continue;
                }
                if let Some(ev) = part_event(p, &m.role, m.mode.as_deref() == Some("compaction")) {
                    let idx = events.len();
                    match ev.kind {
                        EventKind::UserPrompt if first_prompt.is_none() => first_prompt = Some(idx),
                        EventKind::ToolUse | EventKind::FileEdit if first_tool.is_none() => {
                            first_tool = Some(idx)
                        }
                        _ => {}
                    }
                    events.push(ev);
                }
            }
            if !files.is_empty() {
                match first_prompt.or(first_tool) {
                    Some(idx) => events[idx].paths.extend(files),
                    // No event to attach to (a message that is nothing but an attachment): an
                    // attachment must not vanish silently, so each one gets its own Other (the
                    // text is the filename, so it can be read back).
                    None => {
                        for f in files {
                            let mut e =
                                Event::text(EventKind::Other, f, ms_to_rfc3339(m.time_created));
                            e.line = Some(m.lineno);
                            events.push(e);
                        }
                    }
                }
            }
        }

        // An orphan part whose host message is missing must not vanish silently — each one
        // counts as Other.
        for p in &parts {
            if claimed.contains(&p.lineno) {
                continue;
            }
            if msg_index.contains_key(p.message_id.as_str()) {
                continue; // Swallowed by a consumed compaction summary message; normal.
            }
            events.push(other_event(part_ts(p)).at_line(p.lineno));
        }

        // Put every event back in line-number order (a stable sort): unknown outer-kind lines
        // are queued while scanning and never reach the message loop; the relative order of
        // several events on one line is preserved by the sort's stability.
        events.sort_by_key(|e| e.line.unwrap_or(usize::MAX));

        Ok(Session {
            id,
            runtime: "opencode".into(),
            cwd,
            events,
        })
    }

    fn render(&self, session: &Session, new_id: &str, cwd: &Path) -> Result<String> {
        self.render_with(session, new_id, cwd, &super::ToolDetails::default())
    }

    fn render_with(
        &self,
        session: &Session,
        new_id: &str,
        cwd: &Path,
        details: &super::ToolDetails,
    ) -> Result<String> {
        Ok(serde_json::to_string_pretty(&render_export(
            session, new_id, cwd, details,
        )?)?)
    }

    fn mint_id(&self) -> String {
        mint_key("ses_")
    }

    /// (1) prepare the export JSON (a full canonical remint, or the rendering unchanged);
    /// (2) write the receipt; (3) run `opencode import` in the target directory; (4) hand back
    /// the resume command per §9.
    ///
    /// import is idempotent (insert-ignore), so installing the same id again is a safe no-op.
    /// But **agit mints a new id every time** — resume continues in place (§8.3), and a user's
    /// existing session is never the target.
    fn install(&self, content: &str, new_id: &str, cwd: &Path) -> Result<Installed> {
        let cli = super::which(self.cli()).context(
            "the `opencode` executable was not found — cannot install.\n  \
             Once the CLI is installed, the receipt file can be `opencode import`-ed manually to complete the content.",
        )?;
        let payload = import_payload(content, new_id, cwd)?;
        let dir = receipt_dir()?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(format!("{new_id}.json"));
        std::fs::write(&path, &payload)
            .with_context(|| format!("cannot write {}", path.display()))?;
        run_import(&mut std::process::Command::new(cli), &path, cwd)?;
        Ok(Installed {
            next: Next::Resume(format!(
                "(cd {} && opencode --session {new_id})",
                cwd.display()
            )),
            path,
        })
    }
}

/// install's testable core: the CLI path and the receipt directory are both parameters; when
/// `shim_log` is set, argv is written there (the test shim uses it to pin that install really
/// invoked the CLI as `import <file>`).
///
/// The trait's `install` only wires this up to `which("opencode")` and [`receipt_dir`] — tests
/// do not call that one, so they never depend on a binary that may not exist in CI.
#[cfg(test)]
fn install_via(
    content: &str,
    new_id: &str,
    cwd: &Path,
    receipts: &Path,
    cli: &Path,
    shim_log: Option<&Path>,
) -> Result<Installed> {
    let payload = import_payload(content, new_id, cwd)?;
    std::fs::create_dir_all(receipts)
        .with_context(|| format!("cannot create {}", receipts.display()))?;
    let path = receipts.join(format!("{new_id}.json"));
    std::fs::write(&path, &payload).with_context(|| format!("cannot write {}", path.display()))?;
    let mut cmd = std::process::Command::new(cli);
    if let Some(log) = shim_log {
        cmd.env("SHIM_LOG", log);
    }
    run_import(&mut cmd, &path, cwd)?;
    Ok(Installed {
        next: Next::Resume(format!(
            "(cd {} && opencode --session {new_id})",
            cwd.display()
        )),
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Real shapes from the probing documentation (§2.1/§2.2), inlined as fixtures ──

    const META: &str = r#"{"id":"ses_aaa111","kind":"opencode.meta","project_id":"global","parent_id":null,"directory":"/repo","time_created":1785832540699,"version":"1.18.13"}"#;

    fn msg(id: &str, tc: i64, data: &str) -> String {
        format!(
            r#"{{"id":"{id}","kind":"message","session_id":"ses_aaa111","time_created":{tc},"data":{data}}}"#
        )
    }

    fn part(id: &str, mid: &str, tc: i64, data: &str) -> String {
        format!(
            r#"{{"id":"{id}","kind":"part","message_id":"{mid}","session_id":"ses_aaa111","time_created":{tc},"data":{data}}}"#
        )
    }

    /// A minimal complete session: one human question + one edit + one reply + the known
    /// structural parts and every mapped shape. The key order is the key order in the
    /// §2.1/§2.2 documentation.
    fn realistic() -> String {
        let mut lines = vec![META.to_string()];
        lines.push(msg(
            "msg_u1",
            1000,
            r#"{"role":"user","time":{"created":1000},"agent":"build","model":{"providerID":"einsia","modelID":"kimi-k3"},"summary":{"diffs":[]}}"#,
        ));
        lines.push(part(
            "prt_u1",
            "msg_u1",
            1001,
            r#"{"type":"text","text":"fix the typos in the README"}"#,
        ));
        // One of the §6 injection shapes: the replay channel.
        lines.push(part(
            "prt_u2",
            "msg_u1",
            1002,
            r#"{"type":"text","synthetic":true,"text":"Called the Read tool with the following input: {\"filePath\":\"/repo/README.md\"}"}"#,
        ));
        // Attachment: attaches to the owning UserPrompt's paths; the base64 body stays out
        // of the IR.
        lines.push(part(
            "prt_u3",
            "msg_u1",
            1003,
            r#"{"type":"file","url":"data:image/png;base64,iVBOR","mime":"image/png","filename":"typo-screenshot.png"}"#,
        ));
        lines.push(msg(
            "msg_a1",
            2000,
            r#"{"parentID":"msg_u1","role":"assistant","mode":"build","agent":"build","path":{"cwd":"/repo","root":"/repo"},"cost":0,"tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}},"modelID":"kimi-k3","providerID":"einsia","time":{"created":2000,"completed":2900},"finish":"stop"}"#,
        ));
        // Plaintext reasoning: counted as Other, never rendered.
        lines.push(part(
            "prt_a1",
            "msg_a1",
            2001,
            r#"{"type":"reasoning","text":"look at the diff first","time":{"start":2001,"end":2010}}"#,
        ));
        lines.push(part(
            "prt_a2",
            "msg_a1",
            2100,
            r#"{"type":"tool","tool":"edit","callID":"call_x1","state":{"status":"completed","input":{"filePath":"/repo/README.md"},"output":"ok","title":"edit /repo/README.md","metadata":{},"time":{"start":2100,"end":2110}}}"#,
        ));
        // A read tool: ToolUse (the allowlist promotes only edit/write).
        lines.push(part(
            "prt_a3",
            "msg_a1",
            2200,
            r#"{"type":"tool","tool":"bash","callID":"call_x2","state":{"status":"completed","input":{"command":"ls"},"output":"...","metadata":{},"time":{"start":2200,"end":2210}}}"#,
        ));
        // A file-change receipt: Other + files[], with no second count for the edit.
        lines.push(part(
            "prt_a4",
            "msg_a1",
            2300,
            r#"{"type":"patch","hash":"deadbeef","files":["/repo/README.md"]}"#,
        ));
        // Known structural parts: no event, and not counted as dropped.
        lines.push(part(
            "prt_a5",
            "msg_a1",
            2400,
            r#"{"type":"step-start","snapshot":"abc"}"#,
        ));
        lines.push(part(
            "prt_a6",
            "msg_a1",
            2500,
            r#"{"type":"step-finish","reason":"stop","tokens":{"input":1},"cost":0}"#,
        ));
        lines.push(part(
            "prt_a7",
            "msg_a1",
            2600,
            r#"{"type":"text","text":"fixed it.","time":{"start":2600,"end":2650}}"#,
        ));
        lines.join("\n") + "\n"
    }

    #[test]
    fn parses_the_documented_part_zoo() {
        let s = OpenCode.parse(&realistic()).unwrap();
        assert_eq!(s.id, "ses_aaa111");
        assert_eq!(s.runtime, "opencode");
        assert_eq!(s.cwd.as_deref(), Some("/repo"));
        let c = s.counts();
        assert_eq!(c.prompts, 1, "a synthetic injection is not a prompt");
        assert_eq!(c.replies, 1);
        assert_eq!(c.tools, 1, "bash is a ToolUse");
        assert_eq!(c.edits, 1, "edit is promoted to FileEdit");
        // reasoning + patch + the synthetic injection are Other; the step parts are not
        // counted.
        assert_eq!(
            c.dropped,
            3,
            "events: {:?}",
            s.events.iter().map(|e| e.kind).collect::<Vec<_>>()
        );
        assert_eq!(c.compactions, 0);
        // The attachment filename hangs on the UserPrompt's paths; the edit's path is there.
        let prompt = s
            .events
            .iter()
            .find(|e| e.kind == EventKind::UserPrompt)
            .unwrap();
        assert_eq!(prompt.paths, vec!["typo-screenshot.png"]);
        let edit = s
            .events
            .iter()
            .find(|e| e.kind == EventKind::FileEdit)
            .unwrap();
        assert_eq!(edit.paths, vec!["/repo/README.md"]);
        // The text part's time.start becomes RFC3339.
        assert_eq!(
            s.events
                .iter()
                .find(|e| e.kind == EventKind::AssistantReply)
                .unwrap()
                .timestamp
                .as_deref(),
            Some("1970-01-01T00:00:02.600Z")
        );
    }

    /// The gist takes the human line and skips the synthetic injection (§6).
    #[test]
    fn synthetic_text_is_not_a_prompt_nor_the_gist() {
        let mut lines = vec![META.to_string()];
        lines.push(msg(
            "msg_u1",
            1000,
            r#"{"role":"user","time":{"created":1000}}"#,
        ));
        lines.push(part(
            "prt_u1",
            "msg_u1",
            1001,
            r#"{"type":"text","synthetic":true,"text":"<system-reminder>Note: The user opened /repo/x.rs</system-reminder>"}"#,
        ));
        lines.push(part(
            "prt_u2",
            "msg_u1",
            1002,
            r#"{"type":"text","text":"question"}"#,
        ));
        let s = OpenCode.parse(&(lines.join("\n") + "\n")).unwrap();
        assert_eq!(s.counts().prompts, 1);
        assert_eq!(s.counts().dropped, 1);
        assert_eq!(s.gist(10).as_deref(), Some("question"));
    }

    /// The §7 triple collapses into one CompactSummary: boundary + merged summary text, with
    /// the continuation injection an ordinary synthetic (landing in Other). The timestamp comes
    /// from the boundary message.
    #[test]
    fn compaction_triple_merges_into_one_summary_event() {
        let mut lines = vec![META.to_string()];
        lines.push(msg(
            "msg_u1",
            1000,
            r#"{"role":"user","time":{"created":1000}}"#,
        ));
        lines.push(part(
            "prt_u1",
            "msg_u1",
            1001,
            r#"{"type":"text","text":"ask"}"#,
        ));
        lines.push(msg(
            "msg_c1",
            5000,
            r#"{"role":"user","time":{"created":5000}}"#,
        ));
        lines.push(part(
            "prt_c1",
            "msg_c1",
            5001,
            r#"{"type":"compaction","auto":true,"overflow":false,"tail_start_id":"msg_s1"}"#,
        ));
        lines.push(msg(
            "msg_s1",
            5100,
            r#"{"role":"assistant","mode":"compaction","agent":"compaction","summary":true,"time":{"created":5100}}"#,
        ));
        lines.push(part(
            "prt_s1",
            "msg_s1",
            5101,
            r###"{"type":"text","text":"## Objective\n- fix the typos"}"###,
        ));
        lines.push(msg(
            "msg_u2",
            5200,
            r#"{"role":"user","time":{"created":5200}}"#,
        ));
        lines.push(part(
            "prt_u2",
            "msg_u2",
            5201,
            r#"{"type":"text","synthetic":true,"text":"Continue if you have next steps…","metadata":{"compaction_continue":true}}"#,
        ));
        let s = OpenCode.parse(&(lines.join("\n") + "\n")).unwrap();
        let c = s.counts();
        assert_eq!(c.compactions, 1, "the triple must be one event");
        assert_eq!(
            c.prompts, 1,
            "neither the summary nor the continuation injection is a prompt"
        );
        assert_eq!(c.dropped, 1, "the continuation injection counts as Other");
        let cs = s
            .events
            .iter()
            .find(|e| e.kind == EventKind::CompactSummary)
            .unwrap();
        assert_eq!(cs.text.as_deref(), Some("## Objective\n- fix the typos"));
        assert!(
            cs.kind.is_lossy_summary(),
            "a lossy summary must never be fed to merge"
        );
        assert_eq!(
            s.gist(10).as_deref(),
            Some("ask"),
            "the gist skips the compact boundary"
        );
    }

    /// An unknown part type and an unknown outer kind both land in dropped, never silently
    /// (subtask/snapshot/agent from the §2.2 documentation have no instance in this database,
    /// and a future type works the same way).
    #[test]
    fn unknown_part_types_are_counted_not_silently_dropped() {
        let mut lines = vec![META.to_string()];
        lines.push(msg(
            "msg_u1",
            1000,
            r#"{"role":"user","time":{"created":1000}}"#,
        ));
        lines.push(part(
            "prt_u1",
            "msg_u1",
            1001,
            r#"{"type":"text","text":"ask"}"#,
        ));
        lines.push(part(
            "prt_x1",
            "msg_u1",
            1002,
            r#"{"type":"subtask","agent":"explore","description":"find it"}"#,
        ));
        lines.push(r#"{"id":"zzz1","kind":"future-thing","time_created":1003}"#.to_string());
        let s = OpenCode.parse(&(lines.join("\n") + "\n")).unwrap();
        let c = s.counts();
        assert_eq!(c.prompts, 1);
        assert_eq!(
            c.dropped, 2,
            "an unknown part type and an unknown kind each count once"
        );
        let others: Vec<usize> = s
            .events
            .iter()
            .filter(|e| e.kind == EventKind::Other)
            .filter_map(|e| e.line)
            .collect();
        assert_eq!(
            others,
            vec![3, 4],
            "the line number must be carried so the raw line can be located"
        );
    }

    /// A corrupt line is skipped instead of failing the whole parse (as in the other three
    /// adapters: a transcript can be truncated).
    #[test]
    fn corrupt_lines_are_skipped_not_fatal() {
        let good = realistic();
        let s = OpenCode
            .parse(&format!("NOT JSON\n{good}{{trunca"))
            .unwrap();
        assert_eq!(s.counts().prompts, 1);
    }

    // ── Database side: reading and materializing from a fixture db ──

    /// Build a temporary database whose schema is the **columns actually used** from the real
    /// tables (§1), with rows in the shapes from the probing documentation.
    fn fixture_db() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("opencode.db");
        let con = Connection::open(&p).unwrap();
        con.execute_batch(
            "CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);
             CREATE TABLE session (
                 id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT,
                 directory TEXT NOT NULL, time_created INTEGER NOT NULL,
                 time_updated INTEGER NOT NULL, version TEXT NOT NULL);
             CREATE TABLE message (
                 id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (
                 id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL, data TEXT NOT NULL);
             INSERT INTO project VALUES ('proj1', '/repo/one'), ('proj2', '/repo/two'), ('global', '/');
             INSERT INTO session VALUES
               ('ses_one', 'proj1', NULL, '/repo/one', 100, 190, '1.18.13'),
               ('ses_two', 'proj2', NULL, '/repo/two', 200, 290, '1.18.13'),
               -- A session started from a subdirectory: directory ≠ worktree, caught by the join
               ('ses_sub', 'proj1', NULL, '/repo/one/sub', 300, 390, '1.18.13'),
               -- A session in the global project is attributed by directory alone (§5)
               ('ses_global', 'global', NULL, '/repo/one', 400, 490, '1.18.13');
             INSERT INTO message VALUES
               ('msg_m1', 'ses_one', 110, '{\"role\":\"user\",\"time\":{\"created\":110}}'),
               ('msg_m2', 'ses_one', 120, '{\"role\":\"assistant\",\"parentID\":\"msg_m1\",\"time\":{\"created\":120},\"finish\":\"stop\"}');
             INSERT INTO part VALUES
               ('prt_p1', 'msg_m1', 'ses_one', 111, '{\"type\":\"text\",\"text\":\"question\"}'),
               ('prt_p2', 'msg_m2', 'ses_one', 121, '{\"type\":\"text\",\"text\":\"answer\"}');",
        )
        .unwrap();
        (d, p)
    }

    /// The §4 canonical shape matches the spec **byte for byte**: the meta line carries
    /// immutable columns only, message comes before part, and data is embedded verbatim without
    /// reserialization.
    #[test]
    fn materialization_matches_the_canonical_spec_byte_for_byte() {
        let (_d, p) = fixture_db();
        let con = open(&p).unwrap();
        assert!(schema_ok(&con));
        let m = materialize(&con, "ses_one").unwrap();
        let expect = concat!(
            "{\"directory\":\"/repo/one\",\"id\":\"ses_one\",\"kind\":\"opencode.meta\",\"parent_id\":null,\"project_id\":\"proj1\",\"time_created\":100,\"version\":\"1.18.13\"}\n",
            "{\"id\":\"msg_m1\",\"kind\":\"message\",\"session_id\":\"ses_one\",\"time_created\":110,\"data\":{\"role\":\"user\",\"time\":{\"created\":110}}}\n",
            "{\"id\":\"prt_p1\",\"kind\":\"part\",\"message_id\":\"msg_m1\",\"session_id\":\"ses_one\",\"time_created\":111,\"data\":{\"type\":\"text\",\"text\":\"question\"}}\n",
            "{\"id\":\"msg_m2\",\"kind\":\"message\",\"session_id\":\"ses_one\",\"time_created\":120,\"data\":{\"role\":\"assistant\",\"parentID\":\"msg_m1\",\"time\":{\"created\":120},\"finish\":\"stop\"}}\n",
            "{\"id\":\"prt_p2\",\"kind\":\"part\",\"message_id\":\"msg_m2\",\"session_id\":\"ses_one\",\"time_created\":121,\"data\":{\"type\":\"text\",\"text\":\"answer\"}}\n",
        );
        assert_eq!(m.text, expect, "canonical bytes must be stable (§4)");
        assert_eq!(m.updated_ms, 190);
        // time_updated must never appear in the materialized result (§3 conclusion 3: a noise
        // column).
        assert!(!m.text.contains("time_updated"));
    }

    /// Two materializations are byte-identical — the property verified in §4, pinned here.
    #[test]
    fn materialization_is_byte_stable_across_rereads() {
        let (_d, p) = fixture_db();
        let con = open(&p).unwrap();
        let a = materialize(&con, "ses_one").unwrap();
        let b = materialize(&con, "ses_one").unwrap();
        assert_eq!(a.text, b.text);
    }

    /// End to end: fixture database → canonical → IR, with mapping and bytes on one chain.
    #[test]
    fn db_to_ir_roundtrip() {
        let (_d, p) = fixture_db();
        let con = open(&p).unwrap();
        let m = materialize(&con, "ses_one").unwrap();
        let s = OpenCode.parse(&m.text).unwrap();
        assert_eq!(s.id, "ses_one");
        assert_eq!(s.cwd.as_deref(), Some("/repo/one"));
        let c = s.counts();
        assert_eq!((c.prompts, c.replies, c.dropped), (1, 1, 0));
    }

    /// §5: the exact directory match and the worktree fallback merge and deduplicate; a
    /// session in the global project is there too.
    #[test]
    fn sessions_for_matches_directory_and_falls_back_to_worktree() {
        let (_d, p) = fixture_db();
        let con = open(&p).unwrap();
        let got = list_for_repo(&con, "/repo/one").unwrap();
        let ids: Vec<&str> = got.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["ses_one", "ses_sub", "ses_global"],
            "ses_sub comes from the worktree fallback, ses_global from directory"
        );
        // Another repo's sessions do not come along.
        let ids: Vec<String> = list_for_repo(&con, "/repo/two")
            .unwrap()
            .into_iter()
            .map(|l| l.id)
            .collect();
        assert_eq!(ids, vec!["ses_two"]);
    }

    #[test]
    fn missing_session_materializes_to_none_not_garbage() {
        let (_d, p) = fixture_db();
        let con = open(&p).unwrap();
        assert!(materialize(&con, "ses_nope").is_none());
    }

    /// A changed schema means wholly unavailable, never guessed (the codex_index rule).
    #[test]
    fn schema_drift_means_gone_not_guessed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("opencode.db");
        Connection::open(&p)
            .unwrap()
            .execute_batch("CREATE TABLE session (id TEXT);")
            .unwrap();
        let con = open(&p).unwrap();
        assert!(!schema_ok(&con));
    }

    // ── id reminting and install preparation ──

    /// The same-family lossless path: all three id layers are replaced, internal references are
    /// rewritten with them, and the structure is preserved.
    #[test]
    fn remint_changes_every_id_and_preserves_the_structure() {
        let canon = realistic();
        let v = canonical_to_export(&canon, "ses_new999", Path::new("/repo")).unwrap();
        let text = serde_json::to_string_pretty(&v).unwrap();

        assert_eq!(v["info"]["id"], "ses_new999");
        // Every row id in all three layers changes (a call id inside a part, such as call_x1,
        // is not a row reference and stays as it is).
        for old in ["ses_aaa111", "msg_u1", "msg_a1", "prt_u1", "prt_a1"] {
            assert!(!text.contains(old), "old id {old} is still present: {text}");
        }
        // The parentID chain points at the new first message; internal references are
        // rewritten with it.
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "the structure is preserved: two messages");
        let u1 = msgs[0]["info"]["id"].as_str().unwrap();
        assert!(u1.starts_with("msg_") && u1 != "msg_u1");
        assert_eq!(msgs[1]["info"]["parentID"].as_str().unwrap(), u1);
        // Parts hang on their host, with brand new ids.
        let parts = msgs[1]["parts"].as_array().unwrap();
        assert_eq!(
            parts.len(),
            7,
            "reasoning/tool×2/patch/step×2/text are all present"
        );
        for pv in parts {
            let pid = pv["id"].as_str().unwrap();
            assert!(pid.starts_with("prt_"), "{pid}");
            assert_eq!(pv["sessionID"], "ses_new999");
        }
        // Content is unchanged: reasoning text, the synthetic flag and patch all pass through.
        let text_part = parts
            .iter()
            .find(|p| p["type"] == "text")
            .expect("the assistant's text part must be present");
        assert_eq!(text_part["text"], "fixed it.");
        assert_eq!(text_part["messageID"], msgs[1]["info"]["id"]);
    }

    /// tail_start_id in a compaction boundary is an internal reference and is reminted with
    /// the rest (§8.2).
    #[test]
    fn remint_rewrites_compaction_tail_references() {
        let mut lines = vec![META.to_string()];
        lines.push(msg(
            "msg_s1",
            2000,
            r#"{"role":"assistant","time":{"created":2000}}"#,
        ));
        lines.push(msg(
            "msg_c1",
            3000,
            r#"{"role":"user","time":{"created":3000}}"#,
        ));
        lines.push(part(
            "prt_c1",
            "msg_c1",
            3001,
            r#"{"type":"compaction","auto":true,"overflow":false,"tail_start_id":"msg_s1"}"#,
        ));
        let canon = lines.join("\n") + "\n";
        let v = canonical_to_export(&canon, "ses_new888", Path::new("/repo")).unwrap();
        let tail = v["messages"][1]["parts"][0]["tail_start_id"]
            .as_str()
            .unwrap();
        assert_eq!(
            tail,
            v["messages"][0]["info"]["id"].as_str().unwrap(),
            "tail_start_id must point at a reminted id that exists"
        );
    }

    /// The discrimination: canonical → remint everything; export JSON → unchanged (rendering
    /// has already minted, and minting again points the resume command at an id that is not in
    /// the database).
    #[test]
    fn import_payload_remints_canonical_but_passes_rendered_export_through() {
        let canon = realistic();
        let a = import_payload(&canon, "ses_k1", Path::new("/repo")).unwrap();
        assert!(a.contains("ses_k1"));
        assert!(!a.contains("ses_aaa111"));

        let ir = OpenCode.parse(&canon).unwrap();
        let rendered = OpenCode.render(&ir, "ses_k2", Path::new("/repo")).unwrap();
        let b = import_payload(&rendered, "ses_k2", Path::new("/repo")).unwrap();
        assert!(b.contains("ses_k2"));
        // Neither canonical nor export → the error comes before anything reaches disk.
        assert!(import_payload("{\"hello\":1}", "ses_x", Path::new("/r")).is_err());
    }

    /// id shape and validation: the `ses_` prefix plus a time prefix, all strings; two mints
    /// are never equal.
    #[test]
    fn minted_ids_are_time_ordered_strings() {
        let a = OpenCode.mint_id();
        let b = OpenCode.mint_id();
        assert_ne!(a, b);
        // ses_ + 12 hex digits of the epoch millisecond + 14 random characters (the observed
        // shape ses_0323b657bffeakH0buzVroXSsV is exactly this structure, §9).
        assert!(a.starts_with("ses_") && a.len() == 30, "{a}");
        assert!(
            a[4..].chars().all(|c| c.is_ascii_alphanumeric()),
            "only alphanumerics, no shell metacharacters: {a}"
        );
    }

    /// The rendered output satisfies the import required-field surface pinned by observation
    /// (info's required keys, a message's model key).
    /// When enrich supplies the details, the tool part carries the real input and real output.
    #[test]
    fn render_with_details_fills_tool_state() {
        use super::super::{Event, EventKind, Session, ToolDetail, ToolDetails};
        let s = Session {
            id: "s".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![Event {
                kind: EventKind::ToolUse,
                text: Some("Bash".into()),
                timestamp: None,
                paths: vec![],
                tool: Some("Bash".into()),
                line: None,
            }],
        };
        let mut details = ToolDetails::default();
        details.insert(
            0,
            ToolDetail {
                input: Some(serde_json::json!({"command": "ls"})),
                output: Some("total 0".into()),
                error: false,
            },
        );
        let out = OpenCode
            .render_with(&s, "ses_x", Path::new("/r"), &details)
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let part = &v["messages"][0]["parts"][0];
        assert_eq!(part["type"], "tool");
        assert_eq!(part["state"]["input"]["command"], "ls");
        assert_eq!(part["state"]["output"], "total 0");
    }

    /// Shaped by role, the tool part's state.metadata, and the key dropped when there is no
    /// parent.
    #[test]
    fn render_satisfies_the_imported_shape() {
        let s = Session {
            id: "src".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![
                Event::text(EventKind::UserPrompt, "fix it", None),
                Event {
                    kind: EventKind::FileEdit,
                    text: Some("edit".into()),
                    timestamp: None,
                    paths: vec!["/repo/a.rs".into()],
                    tool: Some("edit".into()),
                    line: None,
                },
                Event::text(EventKind::AssistantReply, "done", None),
            ],
        };
        let out = OpenCode.render(&s, "ses_r1", Path::new("/repo")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let info = &v["info"];
        for k in ["id", "slug", "title", "directory", "time"] {
            assert!(
                info.get(k).is_some(),
                "info is missing the required key {k}"
            );
        }
        assert_eq!(info["id"], "ses_r1");
        assert!(
            info.get("parentID").is_none(),
            "with no parent the key is dropped, not set to null"
        );
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert!(
            msgs[0]["info"]["model"]["providerID"].is_string(),
            "a user message carries the model object"
        );
        assert!(
            msgs[1]["info"]["modelID"].is_string(),
            "an assistant message carries the flat modelID"
        );
        // FileEdit → a real tool part, with filePath in the input.
        let tp = &msgs[1]["parts"][0];
        assert_eq!(tp["type"], "tool");
        assert_eq!(tp["state"]["input"]["filePath"], "/repo/a.rs");
        assert!(
            tp["state"]["metadata"].is_object(),
            "the metadata key must be present"
        );
        // The parentID chain steps over the message in between.
        assert_eq!(
            msgs[2]["info"]["parentID"].as_str().unwrap(),
            msgs[1]["info"]["id"].as_str().unwrap()
        );
    }

    /// Other / TurnEnd are not replayed (as in the other two adapters).
    #[test]
    fn render_skips_other_and_turn_end() {
        let s = Session {
            id: "src".into(),
            runtime: "codex".into(),
            cwd: None,
            events: vec![
                Event::text(EventKind::UserPrompt, "ask", None),
                Event {
                    kind: EventKind::Other,
                    text: None,
                    timestamp: None,
                    paths: vec![],
                    tool: None,
                    line: None,
                },
                Event {
                    kind: EventKind::TurnEnd,
                    text: None,
                    timestamp: None,
                    paths: vec![],
                    tool: None,
                    line: None,
                },
            ],
        };
        let out = OpenCode.render(&s, "ses_r2", Path::new("/r")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
    }

    /// The whole install chain with the CLI replaced by a shim: the receipt is written (new
    /// identity, no old id), `import` is the command that runs, and a Resume command comes back;
    /// when the shim fails, the error carries stderr.
    #[cfg(unix)]
    #[test]
    fn install_writes_a_fresh_receipt_and_runs_import() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let shim = d.path().join("opencode-shim");
        std::fs::write(&shim, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$SHIM_LOG\"\n").unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

        let receipts = d.path().join("receipts");
        let log = d.path().join("shim.log");
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let inst = install_via(
            &realistic(),
            "ses_inst1",
            &repo,
            &receipts,
            &shim,
            Some(&log),
        )
        .unwrap();

        assert_eq!(inst.path, receipts.join("ses_inst1.json"));
        let body = std::fs::read_to_string(&inst.path).unwrap();
        assert!(body.contains("ses_inst1"));
        assert!(
            !body.contains("ses_aaa111"),
            "the receipt must not carry the old identity"
        );
        let called = std::fs::read_to_string(&log).unwrap();
        assert!(
            called.starts_with("import\n"),
            "import must be the command that runs: {called}"
        );
        let Next::Resume(cmd) = &inst.next else {
            panic!("opencode's next step must be a Resume command");
        };
        assert!(cmd.contains("opencode --session ses_inst1"), "{cmd}");
        assert!(cmd.contains(&repo.display().to_string()), "{cmd}");

        // The failure path: stderr is surfaced and the receipt stays (idempotent, completable
        // by hand).
        let bad = d.path().join("bad-shim");
        std::fs::write(&bad, "#!/bin/sh\necho database is locked >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = install_via(&realistic(), "ses_inst2", &repo, &receipts, &bad, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("database is locked"), "{e}");
        assert!(
            e.contains("ses_inst2"),
            "the receipt the user can complete by hand must be named: {e}"
        );
    }
}
