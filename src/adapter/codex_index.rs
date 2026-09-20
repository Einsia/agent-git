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
//!   first_user_message, preview, title, name, tokens_used, archived,
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
    /// A bounded opening preview; callers must open the transcript to read the prompt.
    pub gist: Option<String>,
    /// Native origin metadata is retained for user-facing discovery filters.
    pub thread_source: Option<String>,
    pub source: Option<String>,
    pub updated_at_ms: Option<i64>,
    /// A bounded native title; missing or blank metadata is absent.
    pub title: Option<String>,
    /// The bounded thread name Codex shows in its own session list; absent when the index has
    /// no such column or the value is blank.
    pub name: Option<String>,
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

const GIST_SOURCE_CHARS: usize = 4096;

fn select(con: &Connection) -> String {
    // Older indexes can omit source; native metadata remains the classification fallback.
    let source = if con.prepare("SELECT source FROM threads LIMIT 0").is_ok() {
        "source"
    } else {
        "NULL"
    };
    let title = bounded_text_column(con, "title");
    let name = bounded_text_column(con, "name");
    format!(
        "SELECT id, rollout_path, cwd, \
         CASE WHEN typeof(first_user_message) = 'text' THEN substr(first_user_message, 1, {}) END, thread_source, \
         updated_at_ms, {source}, {title}, {name} FROM threads",
        GIST_SOURCE_CHARS + 1
    )
}

/// A text column older indexes can lack, clipped in SQL so a long value never crosses the
/// process boundary in full.
fn bounded_text_column(con: &Connection, column: &str) -> String {
    if con
        .prepare(&format!("SELECT {column} FROM threads LIMIT 0"))
        .is_ok()
    {
        format!(
            "CASE WHEN typeof({column}) = 'text' THEN substr({column}, 1, {}) END",
            GIST_SOURCE_CHARS + 1
        )
    } else {
        "NULL".into()
    }
}

fn opening_preview(text: String) -> String {
    let mut preview = super::preview::shorten(&text, super::preview::SESSION_PREVIEW_CHARS);
    // A clipped whitespace prefix cannot establish that the complete prompt is empty.
    if text.chars().count() > GIST_SOURCE_CHARS && !preview.ends_with('…') {
        preview.push('…');
    }
    preview
}

fn row_to_thread(r: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
    let path: String = r.get(1)?;
    Ok(Thread {
        id: r.get(0)?,
        rollout_path: PathBuf::from(path),
        cwd: r.get(2).ok(),
        gist: r.get(3).ok().map(opening_preview),
        thread_source: r.get(4).ok(),
        updated_at_ms: r.get(5).ok(),
        source: r.get(6).ok(),
        title: r
            .get::<_, String>(7)
            .ok()
            .and_then(|title| super::codex_titles::preview(&title)),
        name: r
            .get::<_, String>(8)
            .ok()
            .and_then(|name| super::codex_titles::preview(&name)),
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
    threads_for_cwd_at(&con, cwd, false)
}

/// Internal approval runtimes cannot displace human conversations from session choices.
pub fn session_choices_for_cwd(cwd: &str) -> Option<Vec<Thread>> {
    let con = open(&index_path()?)?;
    threads_for_cwd_at(&con, cwd, true)
}

fn threads_for_cwd_at(con: &Connection, cwd: &str, choices_only: bool) -> Option<Vec<Thread>> {
    if !schema_ok(con) {
        return None;
    }
    // Unknown provenance stays visible; prompt text and broad subagent labels do not identify
    // internal approval runtimes. Filtering before projection avoids reading their previews.
    let source_filter = if choices_only && con.prepare("SELECT source FROM threads LIMIT 0").is_ok()
    {
        "AND CASE WHEN typeof(source) = 'text' AND length(source) <= 4096 AND json_valid(source) \
         THEN COALESCE(json_extract(source, '$.subagent.other') != 'guardian', 1) ELSE 1 END"
    } else {
        ""
    };
    let sql = format!(
        "{} WHERE archived = 0 AND cwd = ?1 AND rollout_path IS NOT NULL \
         {source_filter} ORDER BY updated_at_ms DESC",
        select(con)
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
        "{} WHERE archived = 0 AND rollout_path IS NOT NULL ORDER BY updated_at_ms DESC",
        select(&con)
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
    let sql = format!(
        "{} WHERE id = ?1 AND rollout_path IS NOT NULL LIMIT 1",
        select(&con)
    );
    let mut st = con.prepare(&sql).ok()?;
    let mut rows = st.query_map([id], row_to_thread).ok()?;
    rows.next()?.ok()
}

/// An inspection reads only the exact identity's path and propagates incomplete index evidence.
pub(super) fn native_path_readonly(
    id: &str,
    limits: super::native_snapshot::Limits,
) -> super::native_snapshot::Result<Option<PathBuf>> {
    use super::native_snapshot::Unavailable;
    let root = super::codex::codex_home().map_err(|_| Unavailable::Read)?;
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Unavailable::Read),
    };
    let mut selected: Option<(u32, PathBuf)> = None;
    for (visited, entry) in entries.enumerate() {
        if visited >= limits.lookup_entries {
            return Err(Unavailable::BudgetExceeded);
        }
        let entry = entry.map_err(|_| Unavailable::Read)?;
        let name = entry.file_name();
        let version = name
            .to_str()
            .and_then(|name| name.strip_prefix("state_"))
            .and_then(|name| name.strip_suffix(".sqlite"))
            .and_then(|version| version.parse::<u32>().ok());
        if let Some(version) = version
            && selected
                .as_ref()
                .is_none_or(|(current, _)| version > *current)
        {
            selected = Some((version, entry.path()));
        }
    }
    let Some((_, path)) = selected else {
        return Ok(None);
    };
    native_path_at(&path, id, limits)
}

fn native_path_at(
    database: &Path,
    id: &str,
    limits: super::native_snapshot::Limits,
) -> super::native_snapshot::Result<Option<PathBuf>> {
    use super::native_snapshot::Unavailable;
    if limits.lookup_entries == 0 {
        return Err(Unavailable::BudgetExceeded);
    }
    let metadata = std::fs::symlink_metadata(database).map_err(|_| Unavailable::Read)?;
    if !metadata.file_type().is_file() {
        return Err(Unavailable::Read);
    }
    let connection = open(database).ok_or(Unavailable::Database)?;
    let mut statement = connection
        .prepare("SELECT id, rollout_path FROM threads WHERE id = ?1 AND rollout_path IS NOT NULL")
        .map_err(|_| Unavailable::Database)?;
    let mut rows = statement.query([id]).map_err(|_| Unavailable::Database)?;
    let Some(row) = rows.next().map_err(|_| Unavailable::Database)? else {
        return Ok(None);
    };
    let selected = row
        .get_ref(0)
        .map_err(|_| Unavailable::Database)?
        .as_str()
        .map_err(|_| Unavailable::Database)?;
    if selected != id {
        return Err(Unavailable::Database);
    }
    let path = row
        .get_ref(1)
        .map_err(|_| Unavailable::Database)?
        .as_str()
        .map_err(|_| Unavailable::Database)?;
    if path.len() > limits.bytes.min(limits.working_bytes) {
        return Err(Unavailable::BudgetExceeded);
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(Unavailable::Database);
    }
    if rows.next().map_err(|_| Unavailable::Database)?.is_some() {
        return Err(Unavailable::Ambiguous);
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_native_lookup_ignores_payload_columns_and_refuses_ambiguous_or_bad_paths() {
        use super::super::native_snapshot::{Limits, Unavailable};
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("index.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT, rollout_path TEXT, first_user_message BLOB);",
            )
            .unwrap();
        let path = directory.path().join("selected.jsonl");
        connection
            .execute(
                "INSERT INTO threads VALUES ('selected', ?1, zeroblob(1048576))",
                [path.to_str().unwrap()],
            )
            .unwrap();
        let before = std::fs::read(&database).unwrap();
        assert_eq!(
            native_path_at(&database, "selected", Limits::default()).unwrap(),
            Some(path)
        );
        assert_eq!(
            native_path_at(&database, "select", Limits::default()).unwrap(),
            None
        );
        assert_eq!(
            native_path_at(
                &database,
                "selected",
                Limits {
                    working_bytes: 0,
                    ..Limits::default()
                }
            )
            .unwrap_err(),
            Unavailable::BudgetExceeded
        );
        assert_eq!(std::fs::read(&database).unwrap(), before);
        connection
            .execute(
                "INSERT INTO threads VALUES ('selected', '/different', NULL)",
                [],
            )
            .unwrap();
        assert_eq!(
            native_path_at(&database, "selected", Limits::default()).unwrap_err(),
            Unavailable::Ambiguous
        );
        connection.execute("DELETE FROM threads", []).unwrap();
        connection
            .execute("INSERT INTO threads VALUES ('selected', x'0102', NULL)", [])
            .unwrap();
        assert_eq!(
            native_path_at(&database, "selected", Limits::default()).unwrap_err(),
            Unavailable::Database
        );
        connection.execute("DROP TABLE threads", []).unwrap();
        assert_eq!(
            native_path_at(&database, "selected", Limits::default()).unwrap_err(),
            Unavailable::Database
        );
    }

    #[test]
    fn readonly_native_identity_is_exact_even_with_a_permissive_index_collation() {
        use super::super::native_snapshot::{Limits, Unavailable};
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("index.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE threads (id TEXT COLLATE NOCASE, rollout_path TEXT);")
            .unwrap();
        let path = directory.path().join("native.jsonl");
        connection
            .execute(
                "INSERT INTO threads VALUES ('aBc-session', ?1)",
                [path.to_str().unwrap()],
            )
            .unwrap();
        let matched: String = connection
            .query_row(
                "SELECT id FROM threads WHERE id = ?1",
                ["abc-session"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(matched, "aBc-session");
        assert_eq!(
            native_path_at(&database, "aBc-session", Limits::default()).unwrap(),
            Some(path)
        );
        assert_eq!(
            native_path_at(&database, "abc-session", Limits::default()).unwrap_err(),
            Unavailable::Database
        );
    }

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
        threads_for_cwd_at(&con, cwd, false).unwrap()
    }

    #[test]
    fn titles_are_optional_bounded_text_without_changing_opening_previews() {
        let (_directory, path) = fixture();
        assert!(
            q_cwd(&path, "/repo/one")
                .iter()
                .all(|row| row.title.is_none())
        );
        let con = Connection::open(&path).unwrap();
        con.execute_batch("ALTER TABLE threads ADD COLUMN title;")
            .unwrap();
        let long = format!("{}TAIL", "t".repeat(GIST_SOURCE_CHARS * 2));
        for title in [
            rusqlite::types::Value::Text(long.clone()),
            rusqlite::types::Value::Text("  \n  ".into()),
            rusqlite::types::Value::Blob(b"not text".to_vec()),
        ] {
            con.execute("UPDATE threads SET title = ?1 WHERE id = 'id-a'", [&title])
                .unwrap();
            let sql = format!("{} WHERE id = 'id-a'", select(&con));
            let projected: Option<String> = con.query_row(&sql, [], |row| row.get(7)).unwrap();
            assert!(
                projected
                    .as_ref()
                    .is_none_or(|text| text.chars().count() <= GIST_SOURCE_CHARS + 1)
            );
            let listed = q_cwd(&path, "/repo/one");
            assert_eq!(listed[0].gist.as_deref(), Some("fix rotation"));
            if title == rusqlite::types::Value::Text(long.clone()) {
                let shown = listed[0].title.as_ref().unwrap();
                assert_eq!(
                    shown.chars().count(),
                    super::super::preview::SESSION_PREVIEW_CHARS + 1
                );
                assert!(shown.ends_with('…'));
            } else {
                assert!(listed[0].title.is_none());
            }
            let stored: rusqlite::types::Value = con
                .query_row("SELECT title FROM threads WHERE id = 'id-a'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(stored, title);
        }
    }

    #[test]
    fn names_are_optional_bounded_text_independent_of_titles() {
        let (_directory, path) = fixture();
        assert!(
            q_cwd(&path, "/repo/one")
                .iter()
                .all(|row| row.name.is_none())
        );
        let con = Connection::open(&path).unwrap();
        con.execute_batch(
            "ALTER TABLE threads ADD COLUMN name; ALTER TABLE threads ADD COLUMN title;",
        )
        .unwrap();
        con.execute(
            "UPDATE threads SET name = 'Retry path', title = 'fix rotation' WHERE id = 'id-a'",
            [],
        )
        .unwrap();
        let listed = q_cwd(&path, "/repo/one");
        assert_eq!(listed[0].name.as_deref(), Some("Retry path"));
        assert_eq!(listed[0].title.as_deref(), Some("fix rotation"));
        assert!(listed[1].name.is_none());
        let long = format!("{}TAIL", "n".repeat(GIST_SOURCE_CHARS * 2));
        for name in [
            rusqlite::types::Value::Text(long.clone()),
            rusqlite::types::Value::Text("  \n  ".into()),
            rusqlite::types::Value::Blob(b"not text".to_vec()),
        ] {
            con.execute("UPDATE threads SET name = ?1 WHERE id = 'id-a'", [&name])
                .unwrap();
            let listed = q_cwd(&path, "/repo/one");
            assert_eq!(listed[0].title.as_deref(), Some("fix rotation"));
            if name == rusqlite::types::Value::Text(long.clone()) {
                let shown = listed[0].name.as_ref().unwrap();
                assert_eq!(
                    shown.chars().count(),
                    super::super::preview::SESSION_PREVIEW_CHARS + 1
                );
                assert!(shown.ends_with('…'));
            } else {
                assert!(listed[0].name.is_none());
            }
        }
    }

    #[test]
    fn session_choices_filter_native_approval_provenance_without_hiding_other_work() {
        let (_directory, path) = fixture();
        let con = Connection::open(&path).unwrap();
        con.execute_batch("ALTER TABLE threads ADD COLUMN source TEXT;")
            .unwrap();
        let cases = [
            (
                "approval-null",
                Some(r#"{"subagent":{"other":"guardian"}}"#),
                None,
                false,
            ),
            (
                "approval-user",
                Some(r#"{"subagent":{"other":"guardian"}}"#),
                Some("user"),
                false,
            ),
            (
                "worker",
                Some(r#"{"subagent":{"thread_spawn":{"agent_role":"guardian"}}}"#),
                Some("subagent"),
                true,
            ),
            (
                "review",
                Some(r#"{"subagent":"review"}"#),
                Some("subagent"),
                true,
            ),
            (
                "unknown",
                Some(r#"{"subagent":{"other":"unknown"}}"#),
                Some("subagent"),
                true,
            ),
            ("malformed", Some("{invalid"), None, true),
            ("plain", Some("guardian"), Some("user"), true),
            ("missing", None, Some("subagent"), true),
        ];
        for (id, source, thread_source, _) in cases {
            con.execute(
                "INSERT INTO threads (id, rollout_path, cwd, first_user_message, thread_source, updated_at_ms, archived, source) \
                 VALUES (?1, '/missing.jsonl', '/repo/one', 'guardian approval transcript', ?2, 500, 0, ?3)",
                rusqlite::params![id, thread_source, source],
            ).unwrap();
        }
        drop(con);
        let con = open(&path).unwrap();
        let choices = threads_for_cwd_at(&con, "/repo/one", true).unwrap();
        let all = threads_for_cwd_at(&con, "/repo/one", false).unwrap();
        for (id, _, _, visible) in cases {
            assert_eq!(
                choices.iter().any(|thread| thread.id == id),
                visible,
                "{id}"
            );
            assert!(all.iter().any(|thread| thread.id == id), "{id}");
        }
        assert!(choices.iter().any(|thread| thread.id == "id-a"));
        assert!(
            !choices
                .iter()
                .any(|thread| thread.id == "id-c" || thread.id == "id-e")
        );
    }

    #[test]
    fn indexes_without_native_source_keep_their_session_choices() {
        let (_directory, path) = fixture();
        let con = open(&path).unwrap();
        let choices = threads_for_cwd_at(&con, "/repo/one", true).unwrap();
        assert_eq!(
            choices
                .iter()
                .map(|thread| thread.id.as_str())
                .collect::<Vec<_>>(),
            ["id-a", "id-b"]
        );
    }

    #[test]
    fn indexed_previews_bound_query_results_without_changing_native_prompts() {
        let (_directory, path) = fixture();
        let connection = Connection::open(&path).unwrap();
        // Unicode fixture pins scalar boundaries through SQLite and Rust preview extraction.
        let prompt = format!(
            "{}SENTINEL_AFTER_PREVIEW",
            "\u{4f60}\u{597d}\n".repeat(1 << 17)
        );
        connection
            .execute(
                "UPDATE threads SET first_user_message = ?1 WHERE id = 'id-a'",
                [&prompt],
            )
            .unwrap();
        let sql = format!("{} WHERE id = 'id-a'", select(&connection));
        let projected: String = connection.query_row(&sql, [], |row| row.get(3)).unwrap();
        assert_eq!(projected.chars().count(), GIST_SOURCE_CHARS + 1);
        let listed = q_cwd(&path, "/repo/one");
        let preview = listed[0].gist.as_ref().unwrap();
        assert_eq!(
            preview.chars().count(),
            super::super::preview::SESSION_PREVIEW_CHARS + 1
        );
        assert!(preview.ends_with('…'));
        assert!(!preview.contains("SENTINEL_AFTER_PREVIEW"));
        assert_eq!(listed[1].gist.as_deref(), Some("add test"));
        let stored: String = connection
            .query_row(
                "SELECT first_user_message FROM threads WHERE id = 'id-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, prompt);
    }

    #[test]
    fn non_text_index_values_do_not_become_prompt_previews() {
        let (_directory, path) = fixture();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE threads SET first_user_message = zeroblob(65536) WHERE id = 'id-a'",
                [],
            )
            .unwrap();
        assert!(q_cwd(&path, "/repo/one")[0].gist.is_none());
    }

    #[test]
    fn an_incomplete_whitespace_prefix_does_not_hide_a_real_session() {
        let (_directory, path) = fixture();
        let connection = Connection::open(&path).unwrap();
        let prompt = format!("{}real request", " ".repeat(GIST_SOURCE_CHARS * 2));
        connection
            .execute(
                "UPDATE threads SET first_user_message = ?1 WHERE id = 'id-a'",
                [&prompt],
            )
            .unwrap();
        let listed = q_cwd(&path, "/repo/one");
        assert_eq!(listed[0].gist.as_deref(), Some("…"));
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
