//! SQLite snapshots keep record order stable and publish caches only after a complete read.

use super::native_snapshot::{self as native, Limits, Snapshot, Source, Unavailable};
use crate::Result;
use anyhow::{Context, ensure};
use rusqlite::{Connection, OpenFlags, Row, types::ValueRef};
use serde_json::{Map, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub(super) fn open(path: &Path, limits: Limits) -> Result<Connection> {
    ensure!(
        path.is_absolute() && std::fs::symlink_metadata(path)?.file_type().is_file(),
        "native database must be an absolute regular file"
    );
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(1))?;
    conn.set_limit(
        rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
        limits
            .bytes
            .min(limits.working_bytes / 8)
            .min(i32::MAX as usize) as i32,
    );
    Ok(conn)
}

pub(super) fn row(row: &Row<'_>, limits: Limits) -> Result<Value> {
    let mut map = Map::new();
    let mut size = 0usize;
    for (index, name) in row.as_ref().column_names().into_iter().enumerate() {
        let raw = row.get_ref(index)?;
        size = size
            .checked_add(match raw {
                ValueRef::Text(v) | ValueRef::Blob(v) => v.len(),
                _ => 16,
            })
            .context("native row is too large")?;
        ensure!(
            size <= limits.bytes.min(limits.working_bytes / 8),
            "native row exceeds the inspection budget"
        );
        let value = match raw {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(v) => Value::from(v),
            ValueRef::Real(v) => serde_json::Number::from_f64(v)
                .map(Value::Number)
                .context("non-finite native number")?,
            ValueRef::Text(v) => Value::String(std::str::from_utf8(v)?.into()),
            ValueRef::Blob(v) => serde_json::json!({"$sqlite_blob_hex": hex::encode(v)}),
        };
        map.insert(name.into(), value);
    }
    Ok(Value::Object(map))
}

pub(super) fn push(
    bytes: &mut Vec<u8>,
    value: &Value,
    records: &mut usize,
    limits: Limits,
) -> Result<()> {
    *records += 1;
    ensure!(*records <= limits.records, "native record budget exceeded");
    let line = serde_json::to_vec(value)?;
    ensure!(
        bytes.len().saturating_add(line.len()).saturating_add(1)
            <= limits.bytes.min(limits.working_bytes / 8),
        "native byte budget exceeded"
    );
    bytes.extend_from_slice(&line);
    bytes.push(b'\n');
    Ok(())
}

pub(super) fn source(
    runtime: &'static str,
    id: &str,
    path: PathBuf,
    limits: Limits,
) -> native::Result<Source> {
    native::validate_id(id, limits)?;
    if limits.lookup_entries == 0 {
        return Err(Unavailable::BudgetExceeded);
    }
    Ok(Source {
        runtime,
        session_id: id.into(),
        path,
        database: true,
    })
}

pub(super) fn cache_path(runtime: &str, id: &str) -> Result<PathBuf> {
    native::validate_id(id, Limits::default())?;
    Ok(crate::infra::config::agit_home()?
        .join("cache")
        .join(runtime)
        .join(format!("{id}.jsonl")))
}

pub(super) fn cache(snapshot: Snapshot) -> Result<PathBuf> {
    let path = cache_path(snapshot.source.runtime, &snapshot.source.session_id)?;
    let root = path.parent().context("native cache has no parent")?;
    std::fs::create_dir_all(root)?;
    let mut staged = tempfile::NamedTempFile::new_in(root)?;
    staged.write_all(&snapshot.bytes)?;
    staged.persist(&path).map_err(|error| error.error)?;
    Ok(path)
}
