//! Codex's session index database.
//!
//! # Why not scan files
//!
//! Codex stores rollouts in per-date directories and the path carries no project information, so
//! "list the sessions belonging to a given repo" otherwise means opening every file to read
//! `session_meta.cwd`. On this machine that is **18745** rollout files — minutes on NFS.
//!
//! The `threads` table in `$CODEX_HOME/state_<N>.sqlite` already has that metadata indexed:
//!
//! ```text
//! threads: 18779 rows
//!   id, rollout_path, cwd, git_origin_url, git_branch, git_sha,
//!   first_user_message, preview, title, tokens_used, archived,
//!   created_at/updated_at(_ms), source, model, thread_source, history_mode
//! ```
//!
//! Observed: looking up one repo's sessions by cwd takes **0.8 ms**, a single lookup by id
//! **0.40 ms** (id is unique).
//!
//! # Two columns that cannot be relied on
//!
//! * `has_user_event` is always 0 (the current Codex version does not fill this column; all
//!   18779 rows are 0)
//! * `history_mode` is always `legacy`
//!
//! Fill rates that suffice: `cwd` 100%, `first_user_message` / `preview` / `title`
//! 18769/18779. `git_origin_url` has a value on only 422 rows — so it is a supplement only,
//! never the source of `repo_origin`.
//!
//! # An unusable database must fall back
//!
//! The database name carries a version number (`state_5.sqlite` here, with `state_1..N`
//! present) and a Codex upgrade swaps in a new one. The schema can change too. So every function
//! in this module returns `None`/empty under any anomaly and the caller falls back to scanning
//! files — **the index is an accelerator, not the only path**.

use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// One row of the `threads` table (only the columns that get used).
#[derive(Debug, Clone)]
pub struct Thread {
    pub id: String,
    pub rollout_path: PathBuf,
    pub cwd: Option<String>,
    /// The opening prompt. `agit log` uses it so the user recognizes "which one was the
    /// payments-module session".
    pub first_user_message: Option<String>,
    /// `user` / `subagent`. Filters out subagent sessions (345 of them on this machine).
    pub thread_source: Option<String>,
    pub updated_at_ms: Option<i64>,
}

/// Find the newest state database.
///
/// The name carries a version number (`state_5.sqlite`); take the highest number. None if there
/// is none.
pub fn index_path() -> Option<PathBuf> {
    let dir = super::codex::codex_home().ok()?;
    let mut best: Option<(u32, PathBuf)> = None;
    for e in std::fs::read_dir(&dir).ok()? {
        let Ok(e) = e else { continue };
        let p = e.path();
        let Some(name) = p.file_name().and_then(|x| x.to_str()) else {
            continue;
        };
        // `state_<N>.sqlite`
        let Some(rest) = name.strip_prefix("state_") else {
            continue;
        };
        let Some(num) = rest.strip_suffix(".sqlite") else {
            continue;
        };
        let Ok(n) = num.parse::<u32>() else { continue };
        if best.as_ref().map(|(bn, _)| n > *bn).unwrap_or(true) {
            best = Some((n, p));
        }
    }
    best.map(|(_, p)| p)
}

/// Open the index database read-only.
///
/// Read-only matters: Codex may be writing it, and agit must never hold a write lock.
fn open(path: &Path) -> Option<Connection> {
    Connection::open_with_flags(
        path,
        // `SQLITE_OPEN_READ_ONLY` + no create. URI mode keeps parameters like immutable
        // available later, but immutable stays off — it reads torn pages while Codex is writing.
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()
}

/// Whether the table and every column needed are present.
///
/// When the schema changes (a Codex upgrade), better to fall back wholesale to scanning files
/// than to guess from half a schema — that silently yields an incomplete list.
fn schema_ok(con: &Connection) -> bool {
    // `archived` must be there too: `threads_for_cwd` uses the composite index through it.
    con.prepare(
        "SELECT id, rollout_path, cwd, first_user_message, thread_source, updated_at_ms, \
         archived FROM threads LIMIT 0",
    )
    .is_ok()
}

const SELECT: &str = "SELECT id, rollout_path, cwd, first_user_message, thread_source, \
                      updated_at_ms FROM threads";

fn row_to_thread(r: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
    let path: String = r.get(1)?;
    Ok(Thread {
        id: r.get(0)?,
        rollout_path: PathBuf::from(path),
        cwd: r.get(2).ok(),
        first_user_message: r.get(3).ok(),
        thread_source: r.get(4).ok(),
        updated_at_ms: r.get(5).ok(),
    })
}

/// Sessions under one cwd.
///
/// # `archived = 0` is a semantic requirement and a performance one besides
///
/// In Codex `archived` carries **delete** semantics (sessions the user deleted), so it must not
/// be read in the first place.
///
/// It also happens to be the index's first column — Codex builds the composite index
/// `(archived, cwd, updated_at_ms DESC, id DESC)`. A bare `WHERE cwd = ?` cannot use it and
/// sqlite degrades to scanning `idx_threads_updated_at_ms`, observed at **34.4 ms**; with
/// `archived = 0` it uses the composite index, observed at **0.0 ms**.
///
/// Any anomaly returns None and the caller falls back to scanning files.
pub fn threads_for_cwd(cwd: &str) -> Option<Vec<Thread>> {
    let con = open(&index_path()?)?;
    if !schema_ok(&con) {
        return None;
    }
    let sql = format!(
        "{SELECT} WHERE archived = 0 AND cwd = ?1 AND rollout_path IS NOT NULL \
         ORDER BY updated_at_ms DESC"
    );
    let mut st = con.prepare(&sql).ok()?;
    let rows = st.query_map([cwd], row_to_thread).ok()?;
    Some(rows.filter_map(|r| r.ok()).collect())
}

/// List every session.
///
/// Still orders of magnitude faster than scanning files, but the result can run to tens of
/// thousands of rows — the caller must be prepared for that.
pub fn all_threads() -> Option<Vec<Thread>> {
    let con = open(&index_path()?)?;
    if !schema_ok(&con) {
        return None;
    }
    let sql = format!(
        "{SELECT} WHERE archived = 0 AND rollout_path IS NOT NULL ORDER BY updated_at_ms DESC"
    );
    let mut st = con.prepare(&sql).ok()?;
    let rows = st.query_map([], row_to_thread).ok()?;
    Some(rows.filter_map(|r| r.ok()).collect())
}

/// Exact lookup of one thread by id. Observed at 0.40 ms; id is unique in the table.
///
/// No `archived = 0` here: an explicit id means the user knows which one they want, and
/// filtering on their behalf turns into "it exists, yet it is reported missing". Only the list
/// case needs deleted rows filtered out.
pub fn thread_by_id(id: &str) -> Option<Thread> {
    let con = open(&index_path()?)?;
    if !schema_ok(&con) {
        return None;
    }
    let sql = format!("{SELECT} WHERE id = ?1 AND rollout_path IS NOT NULL LIMIT 1");
    let mut st = con.prepare(&sql).ok()?;
    let mut rows = st.query_map([id], row_to_thread).ok()?;
    rows.next()?.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary database for the query logic, with no dependency on a real Codex home.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state_1.sqlite");
        let con = Connection::open(&p).unwrap();
        con.execute_batch(
            "CREATE TABLE threads (
                 id TEXT, rollout_path TEXT, cwd TEXT, first_user_message TEXT,
                 thread_source TEXT, updated_at_ms INTEGER, archived INTEGER
             );
             INSERT INTO threads VALUES
               ('id-a', '/s/a.jsonl', '/repo/one', 'fix rotation', 'user', 300, 0),
               ('id-b', '/s/b.jsonl', '/repo/one', 'add test',     'user', 200, 0),
               ('id-c', '/s/c.jsonl', '/repo/two', 'other work',   'user', 100, 0),
               -- a NULL rollout_path must be excluded: it points at no file
               ('id-d', NULL,         '/repo/one', 'no path',      'user', 400, 0),
               -- archived=1 means the user deleted it and must not appear in a list
               ('id-e', '/s/e.jsonl', '/repo/one', 'deleted one',  'user', 250, 1);",
        )
        .unwrap();
        (d, p)
    }

    /// Runs the query on a connection directly, bypassing index_path()'s environment dependency.
    /// The SQL stays identical to `threads_for_cwd`.
    fn q_cwd(p: &Path, cwd: &str) -> Vec<Thread> {
        let con = open(p).unwrap();
        assert!(schema_ok(&con));
        let sql = format!(
            "{SELECT} WHERE archived = 0 AND cwd = ?1 AND rollout_path IS NOT NULL \
             ORDER BY updated_at_ms DESC"
        );
        let mut st = con.prepare(&sql).unwrap();
        st.query_map([cwd], row_to_thread)
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    #[test]
    fn filters_by_cwd_and_orders_by_recency() {
        let (_d, p) = fixture();
        let got = q_cwd(&p, "/repo/one");
        let ids: Vec<&str> = got.iter().map(|t| t.id.as_str()).collect();
        // id-d has a NULL rollout_path (points at no file) and id-e is archived=1 (the user
        // deleted it); both are excluded, and what remains is newest-first by updated_at_ms.
        assert_eq!(
            ids,
            vec!["id-a", "id-b"],
            "a NULL path and a deleted row are both excluded, newest first"
        );
    }

    /// `archived = 1` is delete semantics; such a row must not appear in a list.
    #[test]
    fn deleted_sessions_are_excluded() {
        let (_d, p) = fixture();
        let got = q_cwd(&p, "/repo/one");
        assert!(
            !got.iter().any(|t| t.id == "id-e"),
            "archived=1 means the user deleted it; it must not be returned"
        );
    }

    #[test]
    fn other_repos_are_not_returned() {
        let (_d, p) = fixture();
        assert_eq!(q_cwd(&p, "/repo/two").len(), 1);
        assert_eq!(q_cwd(&p, "/repo/nonexistent").len(), 0);
    }

    #[test]
    fn missing_columns_mean_fall_back_not_guess() {
        // When a Codex upgrade changes the schema, better to fall back wholesale to scanning
        // files than to guess from half a schema.
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state_9.sqlite");
        let con = Connection::open(&p).unwrap();
        con.execute_batch("CREATE TABLE threads (id TEXT, rollout_path TEXT);")
            .unwrap();
        drop(con);
        let con = open(&p).unwrap();
        assert!(
            !schema_ok(&con),
            "a missing column must make the schema unusable"
        );
    }

    #[test]
    fn nonexistent_db_is_none_not_error() {
        // With Codex not installed, or the database not yet created, the fallback is silent.
        assert!(open(Path::new("/nonexistent/state_1.sqlite")).is_none());
    }

    #[test]
    fn picks_highest_numbered_state_db() {
        // The database name carries a version number and the newest one must win. Tested as a
        // pure function, leaving the process environment alone.
        fn pick(names: &[&str]) -> Option<String> {
            let mut best: Option<(u32, String)> = None;
            for n in names {
                let Some(rest) = n.strip_prefix("state_") else {
                    continue;
                };
                let Some(num) = rest.strip_suffix(".sqlite") else {
                    continue;
                };
                let Ok(v) = num.parse::<u32>() else { continue };
                if best.as_ref().map(|(b, _)| v > *b).unwrap_or(true) {
                    best = Some((v, n.to_string()));
                }
            }
            best.map(|(_, n)| n)
        }
        assert_eq!(
            pick(&["state_1.sqlite", "state_5.sqlite", "state_2.sqlite"]).as_deref(),
            Some("state_5.sqlite")
        );
        // Numbers compare numerically, not lexicographically: 10 > 9.
        assert_eq!(
            pick(&["state_9.sqlite", "state_10.sqlite"]).as_deref(),
            Some("state_10.sqlite")
        );
        assert_eq!(pick(&["cache.sqlite", "state_x.sqlite"]), None);
    }
}
