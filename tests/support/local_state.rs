//! "Nothing was touched" for local state means no row and no file changed, not that SQLite's
//! files kept their bytes.
//!
//! SQLite keeps committed rows in the database file and its `-wal` sidecar, and whichever
//! connection closes last folds the WAL into the database and deletes the sidecars. An earlier
//! command's background privacy sync can close its connection at any moment, so a byte-level
//! snapshot taken around a later command sees the database rewritten and its sidecars gone
//! although no row changed. Comparing a database by its rows and leaving its sidecars out keeps
//! every real write visible and that close invisible.

use std::io::Read as _;
use std::path::Path;

const HEADER: &[u8; 16] = b"SQLite format 3\0";

fn is_database(path: &Path) -> bool {
    let mut header = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok()
        && &header == HEADER
}

/// A WAL, shared-memory or rollback-journal file of the database beside it.
pub fn is_sidecar(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    ["-wal", "-shm", "-journal"].iter().any(|suffix| {
        name.strip_suffix(suffix)
            .is_some_and(|base| is_database(&path.with_file_name(base)))
    })
}

/// Every table's definition and rows, in a stable order, when `path` is an SQLite database.
pub fn rows(path: &Path) -> Option<Vec<String>> {
    if !is_database(path) {
        return None;
    }
    let db = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    db.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
    let tables: Vec<(String, String)> = db
        .prepare("SELECT name, sql FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut out = vec![];
    for (name, sql) in tables {
        out.push(sql);
        let mut select = db
            .prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))
            .unwrap();
        let columns = select.column_count();
        let mut table: Vec<String> = select
            .query_map([], |row| {
                (0..columns)
                    .map(|column| row.get::<_, rusqlite::types::Value>(column))
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap()
            .map(|row| format!("{name} {:?}", row.unwrap()))
            .collect();
        table.sort();
        out.extend(table);
    }
    Some(out)
}
