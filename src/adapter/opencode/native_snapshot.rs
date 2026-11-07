//! Native inspection reads one database generation without publishing an export cache.
//! SQLite can maintain WAL read-coordination sidecars without modifying application rows.

use super::super::native_snapshot::{
    self as native, Budget, Limits, Result, Snapshot, Source, Unavailable,
};
use super::CanonicalRecord;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OpenFlags, Row};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;

fn open(database: &Path) -> Result<Connection> {
    let metadata = std::fs::symlink_metadata(database).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Unavailable::NotFound
        } else {
            Unavailable::Read
        }
    })?;
    if !database.is_absolute() || !metadata.file_type().is_file() {
        return Err(Unavailable::Read);
    }
    Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| Unavailable::Database)
}

pub(super) fn lookup(database: &Path, id: &str, limits: Limits) -> Result<Source> {
    native::validate_id(id, limits)?;
    if limits.lookup_entries == 0 {
        return Err(Unavailable::BudgetExceeded);
    }
    let mut connection = open(database)?;
    let transaction = connection
        .transaction()
        .map_err(|_| Unavailable::Database)?;
    super::Layout::detect(&transaction)?.route(&transaction, id)?;
    transaction.commit().map_err(|_| Unavailable::Database)?;
    Ok(Source {
        runtime: "opencode",
        session_id: id.to_owned(),
        path: database.to_owned(),
        database: true,
    })
}

pub(super) fn read(source: &Source, limits: Limits) -> Result<Snapshot> {
    if source.runtime != "opencode" || !source.database {
        return Err(Unavailable::Unsupported);
    }
    native::validate_id(&source.session_id, limits)?;
    let mut connection = open(&source.path)?;
    let transaction = connection
        .transaction()
        .map_err(|_| Unavailable::Database)?;
    let bytes = materialize(&transaction, &source.session_id, limits)?;
    transaction.commit().map_err(|_| Unavailable::Database)?;
    native::finish(source.clone(), bytes, limits)
}

#[cfg(feature = "rc")]
pub(crate) fn read_authorized(
    source: &Source,
    limits: Limits,
    cwd: &Path,
) -> crate::Result<Snapshot> {
    if source.runtime != "opencode" || !source.database {
        return Err(Unavailable::Unsupported.into());
    }
    native::validate_id(&source.session_id, limits)?;
    let mut connection = open(&source.path)?;
    let transaction = connection.transaction()?;
    super::validate_rc_connection(&transaction, &source.session_id, cwd)?;
    let bytes = materialize(&transaction, &source.session_id, limits)?;
    transaction.commit()?;
    Ok(native::finish(source.clone(), bytes, limits)?)
}

fn text<'a>(row: &'a Row<'_>, column: usize) -> Result<&'a str> {
    row.get_ref(column)
        .map_err(|_| Unavailable::Database)?
        .as_str()
        .map_err(|_| Unavailable::Database)
}

fn integer(row: &Row<'_>, column: usize) -> Result<i64> {
    row.get_ref(column)
        .map_err(|_| Unavailable::Database)?
        .as_i64()
        .map_err(|_| Unavailable::Database)
}

fn optional_text<'a>(row: &'a Row<'_>, column: usize) -> Result<Option<&'a str>> {
    match row.get_ref(column).map_err(|_| Unavailable::Database)? {
        rusqlite::types::ValueRef::Null => Ok(None),
        value => value.as_str().map(Some).map_err(|_| Unavailable::Database),
    }
}

struct Buffer<'a> {
    bytes: Vec<u8>,
    limit: usize,
    budget: &'a mut Budget,
    failure: Option<Unavailable>,
}

impl Buffer<'_> {
    fn append(&mut self, input: &[u8]) -> Result<()> {
        let next = self
            .bytes
            .len()
            .checked_add(input.len())
            .ok_or(Unavailable::BudgetExceeded)?;
        if next > self.limit {
            return Err(Unavailable::BudgetExceeded);
        }
        if next > self.bytes.capacity() {
            let capacity = next
                .max(self.bytes.capacity().saturating_mul(2))
                .min(self.limit);
            self.budget.reserve(capacity - self.bytes.capacity())?;
            self.bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .map_err(|_| Unavailable::BudgetExceeded)?;
        }
        self.bytes.extend_from_slice(input);
        Ok(())
    }
}

impl Write for Buffer<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Err(error) = self.append(bytes) {
            self.failure = Some(error);
            return Err(std::io::Error::other(error));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct OrderedRecord {
    created: i64,
    kind: u8,
    id: String,
    bytes: Vec<u8>,
}

struct Records {
    entries: Vec<OrderedRecord>,
    source_records: usize,
    output_bytes: usize,
    limits: Limits,
    budget: Budget,
}

impl Records {
    fn new(limits: Limits) -> Self {
        Self {
            entries: Vec::new(),
            source_records: 0,
            output_bytes: 0,
            limits,
            budget: Budget::new(limits.working_bytes),
        }
    }

    fn push(
        &mut self,
        created: i64,
        kind: u8,
        id: &str,
        record: CanonicalRecord<'_>,
    ) -> Result<()> {
        if self.source_records >= self.limits.records {
            return Err(Unavailable::BudgetExceeded);
        }
        self.source_records += 1;
        if self.entries.len() == self.entries.capacity() {
            let capacity = self
                .entries
                .capacity()
                .saturating_mul(2)
                .max(1)
                .min(self.limits.records);
            let reserved = (capacity - self.entries.capacity())
                .checked_mul(std::mem::size_of::<OrderedRecord>())
                .ok_or(Unavailable::BudgetExceeded)?;
            self.budget.reserve(reserved)?;
            self.entries
                .try_reserve_exact(capacity - self.entries.len())
                .map_err(|_| Unavailable::BudgetExceeded)?;
        }
        self.budget.reserve(id.len())?;
        let id = id.to_owned();
        let limit = self
            .limits
            .bytes
            .checked_sub(self.output_bytes)
            .ok_or(Unavailable::BudgetExceeded)?;
        let mut buffer = Buffer {
            bytes: Vec::new(),
            limit,
            budget: &mut self.budget,
            failure: None,
        };
        if record.write_to(&mut buffer).is_err() {
            return Err(buffer.failure.unwrap_or(Unavailable::Database));
        }
        buffer.append(b"\n")?;
        self.output_bytes = self
            .output_bytes
            .checked_add(buffer.bytes.len())
            .ok_or(Unavailable::BudgetExceeded)?;
        self.entries.push(OrderedRecord {
            created,
            kind,
            id,
            bytes: buffer.bytes,
        });
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>> {
        // Equal ordering keys retain their source order, including a native metadata record.
        self.budget.reserve(
            self.entries
                .len()
                .checked_mul(std::mem::size_of::<OrderedRecord>())
                .ok_or(Unavailable::BudgetExceeded)?,
        )?;
        self.entries.sort_by(|a, b| {
            a.created
                .cmp(&b.created)
                .then(a.kind.cmp(&b.kind))
                .then(a.id.cmp(&b.id))
        });
        self.budget.reserve(self.output_bytes)?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(self.output_bytes)
            .map_err(|_| Unavailable::BudgetExceeded)?;
        for record in self.entries {
            output.extend_from_slice(&record.bytes);
        }
        Ok(output)
    }
}

pub(super) fn materialize(connection: &Connection, id: &str, limits: Limits) -> Result<Vec<u8>> {
    let route = super::Layout::detect(connection)?.route(connection, id)?;
    let v2 = route.table == super::storage::SessionTable::V2;
    let mut output = Records::new(limits);
    {
        let mut statement = connection.prepare(&format!("SELECT id, project_id, parent_id, directory, time_created, version FROM {} WHERE id = ?1", route.table.name()))
            .map_err(|_| Unavailable::Database)?;
        let mut rows = statement.query([id]).map_err(|_| Unavailable::Database)?;
        let Some(row) = rows.next().map_err(|_| Unavailable::Database)? else {
            return Err(Unavailable::NotFound);
        };
        let session = text(row, 0)?;
        if session != id {
            return Err(Unavailable::Database);
        }
        let created = integer(row, 4)?;
        output.push(
            if v2 { i64::MIN } else { created },
            0,
            session,
            CanonicalRecord::Meta {
                id: session,
                project_id: text(row, 1)?,
                parent_id: optional_text(row, 2)?,
                directory: text(row, 3)?,
                created,
                version: text(row, 5)?,
            },
        )?;
        if rows.next().map_err(|_| Unavailable::Database)?.is_some() {
            return Err(Unavailable::Ambiguous);
        }
    }
    if !route.projected {
        materialize_parts(connection, id, &mut output)?;
    }
    if route.projected {
        if route.split_parts {
            let orphaned: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_part p WHERE p.session_id = ?1 AND NOT EXISTS (SELECT 1 FROM session_message m WHERE m.id = p.message_id AND m.session_id = p.session_id AND m.type = 'assistant'))",
                [id], |row| row.get(0),
            ).map_err(|_| Unavailable::Database)?;
            if orphaned {
                return Err(Unavailable::Database);
            }
        }
        let mut statement = connection
            .prepare("SELECT id, session_id, type, seq, time_created, data FROM session_message WHERE session_id = ?1 ORDER BY seq")
            .map_err(|_| Unavailable::Database)?;
        let mut rows = statement.query([id]).map_err(|_| Unavailable::Database)?;
        let mut previous = None;
        while let Some(row) = rows.next().map_err(|_| Unavailable::Database)? {
            let message = text(row, 0)?;
            if text(row, 1)? != id {
                return Err(Unavailable::Database);
            }
            let seq = integer(row, 3)?;
            if seq < 0 || previous.is_some_and(|prior| seq <= prior) {
                return Err(Unavailable::Database);
            }
            previous = Some(seq);
            let created = integer(row, 4)?;
            let kind = text(row, 2)?;
            let data = text(row, 5)?;
            let hydrated = if route.split_parts {
                hydrate_parts(connection, id, message, kind, data, &mut output)?
            } else {
                None
            };
            let data = hydrated.as_deref().unwrap_or(data);
            let media = hydrate_media(connection, kind, data, &mut output)?;
            output.push(
                if v2 { seq } else { created },
                super::PROJECTED_ORDER,
                &super::projected_tiebreak(seq),
                CanonicalRecord::Projected {
                    id: message,
                    session: id,
                    kind,
                    seq,
                    created,
                    data: media.as_deref().unwrap_or(data),
                },
            )?;
        }
    }
    output.finish()
}

/// OpenCode prepends ordered split parts to any inline content left on the message.
fn hydrate_parts(
    connection: &Connection,
    session: &str,
    message: &str,
    kind: &str,
    data: &str,
    output: &mut Records,
) -> Result<Option<String>> {
    let mut statement = connection.prepare(
        "SELECT session_id, ordinal, type, data FROM session_part WHERE message_id = ?1 ORDER BY ordinal"
    ).map_err(|_| Unavailable::Database)?;
    let mut rows = statement
        .query([message])
        .map_err(|_| Unavailable::Database)?;
    let mut content = Vec::new();
    let mut previous = None;
    while let Some(row) = rows.next().map_err(|_| Unavailable::Database)? {
        if kind != "assistant" || text(row, 0)? != session {
            return Err(Unavailable::Database);
        }
        let ordinal = integer(row, 1)?;
        if ordinal < 0 || previous.is_some_and(|prior| ordinal <= prior) {
            return Err(Unavailable::Database);
        }
        previous = Some(ordinal);
        if output.source_records >= output.limits.records {
            return Err(Unavailable::BudgetExceeded);
        }
        output.source_records += 1;
        let raw = text(row, 3)?;
        output.budget.reserve(
            raw.len()
                .checked_mul(32)
                .ok_or(Unavailable::BudgetExceeded)?,
        )?;
        let part: serde_json::Value =
            serde_json::from_str(raw).map_err(|_| Unavailable::Database)?;
        if part["type"].as_str() != Some(text(row, 2)?) {
            return Err(Unavailable::Database);
        }
        content.push(part);
    }
    if content.is_empty() {
        return Ok(None);
    }
    output.budget.reserve(
        data.len()
            .checked_mul(32)
            .ok_or(Unavailable::BudgetExceeded)?,
    )?;
    let mut value: serde_json::Value =
        serde_json::from_str(data).map_err(|_| Unavailable::Database)?;
    let fields = value.as_object_mut().ok_or(Unavailable::Database)?;
    if let Some(inline) = fields.remove("content") {
        let serde_json::Value::Array(inline) = inline else {
            return Err(Unavailable::Database);
        };
        content.extend(inline);
    }
    fields.insert("content".into(), serde_json::Value::Array(content));
    serialize_hydrated(&value, output).map(Some)
}

fn serialize_hydrated(value: &serde_json::Value, output: &mut Records) -> Result<String> {
    let mut buffer = Buffer {
        bytes: Vec::new(),
        limit: output.limits.bytes.saturating_sub(output.output_bytes),
        budget: &mut output.budget,
        failure: None,
    };
    if serde_json::to_writer(&mut buffer, value).is_err() {
        return Err(buffer.failure.unwrap_or(Unavailable::Database));
    }
    String::from_utf8(buffer.bytes).map_err(|_| Unavailable::Database)
}

/// Media references are local database keys; portable snapshots must carry their bytes.
fn hydrate_media(
    connection: &Connection,
    kind: &str,
    data: &str,
    output: &mut Records,
) -> Result<Option<String>> {
    if !matches!(kind, "user" | "assistant") || (!data.contains("media:") && !data.contains("\\u"))
    {
        return Ok(None);
    }
    output.budget.reserve(
        data.len()
            .checked_mul(32)
            .ok_or(Unavailable::BudgetExceeded)?,
    )?;
    let mut value: serde_json::Value =
        serde_json::from_str(data).map_err(|_| Unavailable::Database)?;
    let mut changed = false;
    if kind == "user" {
        for file in value
            .get_mut("files")
            .and_then(serde_json::Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            changed |= hydrate_file(connection, file, false, output)?;
        }
    } else {
        for part in value
            .get_mut("content")
            .and_then(serde_json::Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if part["type"] != "tool" {
                continue;
            }
            for item in part
                .get_mut("state")
                .and_then(|state| state.get_mut("content"))
                .and_then(serde_json::Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                if item["type"] == "file" {
                    changed |= hydrate_file(connection, item, true, output)?;
                }
            }
        }
    }
    if changed {
        serialize_hydrated(&value, output).map(Some)
    } else {
        Ok(None)
    }
}

fn hydrate_file(
    connection: &Connection,
    file: &mut serde_json::Value,
    tool: bool,
    output: &mut Records,
) -> Result<bool> {
    let Some(hash) = file["uri"]
        .as_str()
        .and_then(|uri| uri.strip_prefix("media:"))
    else {
        return Ok(false);
    };
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Unavailable::Database);
    }
    if !super::storage::table_exists(connection, "blob")? {
        return Err(Unavailable::Database);
    }
    if output.source_records >= output.limits.records {
        return Err(Unavailable::BudgetExceeded);
    }
    output.source_records += 1;
    let mut statement = connection
        .prepare("SELECT hash, bytes, data FROM blob WHERE hash = ?1")
        .map_err(|_| Unavailable::Database)?;
    let mut rows = statement.query([hash]).map_err(|_| Unavailable::Database)?;
    let row = rows
        .next()
        .map_err(|_| Unavailable::Database)?
        .ok_or(Unavailable::Database)?;
    let raw = row
        .get_ref(2)
        .map_err(|_| Unavailable::Database)?
        .as_blob()
        .map_err(|_| Unavailable::Database)?;
    if text(row, 0)? != hash || usize::try_from(integer(row, 1)?).ok() != Some(raw.len()) {
        return Err(Unavailable::Database);
    }
    let mime = file["mime"].as_str().ok_or(Unavailable::Database)?;
    let prefix = if tool {
        mime.len()
            .checked_add("data:;base64,".len())
            .ok_or(Unavailable::BudgetExceeded)?
    } else {
        0
    };
    let encoded_len = base64::encoded_len(raw.len(), true)
        .and_then(|len| len.checked_add(prefix))
        .ok_or(Unavailable::BudgetExceeded)?;
    if encoded_len > output.limits.bytes.saturating_sub(output.output_bytes) {
        return Err(Unavailable::BudgetExceeded);
    }
    output.budget.reserve(
        raw.len()
            .checked_add(encoded_len)
            .ok_or(Unavailable::BudgetExceeded)?,
    )?;
    if hex::encode(Sha256::digest(raw)) != hash {
        return Err(Unavailable::Database);
    }
    let mut encoded = String::new();
    encoded
        .try_reserve_exact(encoded_len)
        .map_err(|_| Unavailable::BudgetExceeded)?;
    if tool {
        encoded.push_str("data:");
        encoded.push_str(mime);
        encoded.push_str(";base64,");
    }
    STANDARD.encode_string(raw, &mut encoded);
    if rows.next().map_err(|_| Unavailable::Database)?.is_some() {
        return Err(Unavailable::Ambiguous);
    }
    if tool {
        file["uri"] = encoded.into();
    } else {
        file.as_object_mut()
            .ok_or(Unavailable::Database)?
            .remove("uri");
        file["data"] = encoded.into();
    }
    Ok(true)
}

fn materialize_parts(connection: &Connection, id: &str, output: &mut Records) -> Result<()> {
    {
        let mut statement = connection
            .prepare("SELECT id, session_id, time_created, data FROM message WHERE session_id = ?1")
            .map_err(|_| Unavailable::Database)?;
        let mut rows = statement.query([id]).map_err(|_| Unavailable::Database)?;
        while let Some(row) = rows.next().map_err(|_| Unavailable::Database)? {
            let message = text(row, 0)?;
            if text(row, 1)? != id {
                return Err(Unavailable::Database);
            }
            let created = integer(row, 2)?;
            output.push(
                created,
                0,
                message,
                CanonicalRecord::Message {
                    id: message,
                    session: id,
                    created,
                    data: text(row, 3)?,
                },
            )?;
        }
    }
    {
        let mut statement = connection
            .prepare("SELECT id, session_id, message_id, time_created, data FROM part WHERE session_id = ?1")
            .map_err(|_| Unavailable::Database)?;
        let mut rows = statement.query([id]).map_err(|_| Unavailable::Database)?;
        while let Some(row) = rows.next().map_err(|_| Unavailable::Database)? {
            let part = text(row, 0)?;
            if text(row, 1)? != id {
                return Err(Unavailable::Database);
            }
            let created = integer(row, 3)?;
            output.push(
                created,
                1,
                part,
                CanonicalRecord::Part {
                    id: part,
                    message: text(row, 2)?,
                    session: id,
                    created,
                    data: text(row, 4)?,
                },
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT, time_created INTEGER, time_updated INTEGER, version TEXT);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
            CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, data TEXT);
            INSERT INTO session VALUES ('ses_selected', 'project', NULL, '/explicit', 1, 10, 'v');
            INSERT INTO message VALUES ('message', 'ses_selected', 2, '{\"role\":\"user\"}');
            INSERT INTO part VALUES ('part', 'ses_selected', 'message', 3, '{ \"type\": \"text\", \"text\": \"original\" }');").unwrap();
        drop(connection);
        (directory, path)
    }

    /// Query affinity cannot authorize relabeling a native row as another selected session.
    #[test]
    fn readonly_snapshot_requires_exact_text_identity_on_every_native_row() {
        for table in ["message", "part"] {
            for (column, selected, foreign) in [
                ("TEXT COLLATE NOCASE", "ses_selected", "SES_SELECTED"),
                ("INTEGER", "7", "7"),
            ] {
                let (_directory, database) = fixture();
                let connection = Connection::open(&database).unwrap();
                for table in ["session", "message", "part"] {
                    let key = if table == "session" {
                        "id"
                    } else {
                        "session_id"
                    };
                    connection
                        .execute(&format!("UPDATE {table} SET {key} = ?1"), [selected])
                        .unwrap();
                }
                let source = lookup(&database, selected, Limits::default()).unwrap();
                let expected = read(&source, Limits::default()).unwrap().bytes;
                let replace = |column: &str| {
                    let rest = if table == "message" {
                        "time_created INTEGER, data TEXT"
                    } else {
                        "message_id TEXT, time_created INTEGER, data TEXT"
                    };
                    connection.execute_batch(&format!(
                        "ALTER TABLE {table} RENAME TO prior_rows; CREATE TABLE {table} (id TEXT PRIMARY KEY, session_id {column}, {rest}); INSERT INTO {table} SELECT * FROM prior_rows; DROP TABLE prior_rows;"
                    )).unwrap();
                };
                replace(column);
                connection
                    .execute(&format!("UPDATE {table} SET session_id = ?1"), [foreign])
                    .unwrap();
                let before = std::fs::read(&database).unwrap();
                assert!(
                    matches!(read(&source, Limits::default()), Err(Unavailable::Database)),
                    "{table}: {column}"
                );
                assert_eq!(std::fs::read(&database).unwrap(), before);
                replace("TEXT");
                connection
                    .execute(&format!("UPDATE {table} SET session_id = ?1"), [selected])
                    .unwrap();
                assert_eq!(read(&source, Limits::default()).unwrap().bytes, expected);
            }
        }
    }

    #[test]
    fn readonly_snapshot_matches_canonical_bytes_without_an_export_cache() {
        let (directory, database) = fixture();
        let before = std::fs::read(&database).unwrap();
        let source = lookup(&database, "ses_selected", Limits::default()).unwrap();
        let snapshot = read(&source, Limits::default()).unwrap();
        let connection = open(&database).unwrap();
        assert_eq!(
            snapshot.bytes,
            super::super::materialize(&connection, "ses_selected")
                .unwrap()
                .text
                .as_bytes()
        );
        assert_eq!(snapshot.records, 3);
        assert!(
            std::str::from_utf8(&snapshot.bytes)
                .unwrap()
                .contains("{ \"type\": \"text\", \"text\": \"original\" }")
        );
        drop(connection);
        assert_eq!(std::fs::read(&database).unwrap(), before);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert_eq!(
            lookup(&database, "ses_select", Limits::default()).unwrap_err(),
            Unavailable::NotFound
        );
    }

    #[test]
    fn native_budgets_and_bad_rows_never_yield_partial_snapshots() {
        let (_directory, database) = fixture();
        let source = lookup(&database, "ses_selected", Limits::default()).unwrap();
        let expected = read(&source, Limits::default()).unwrap().bytes;
        for limits in [
            Limits {
                bytes: expected.len() - 1,
                ..Limits::default()
            },
            Limits {
                records: 2,
                ..Limits::default()
            },
            Limits {
                working_bytes: 1,
                ..Limits::default()
            },
        ] {
            assert_eq!(
                read(&source, limits).unwrap_err(),
                Unavailable::BudgetExceeded
            );
        }
        assert_eq!(
            read(
                &source,
                Limits {
                    bytes: expected.len(),
                    ..Limits::default()
                }
            )
            .unwrap()
            .bytes,
            expected
        );
        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES ('bad', 'ses_selected', 4, x'010203')",
                [],
            )
            .unwrap();
        assert_eq!(
            read(&source, Limits::default()).unwrap_err(),
            Unavailable::Database
        );
        connection
            .execute("DELETE FROM message WHERE id='bad'", [])
            .unwrap();
        connection
            .execute("UPDATE part SET data='{\"type\":\"a\",\"type\":\"b\"}'", [])
            .unwrap();
        let malformed = read(&source, Limits::default()).unwrap();
        assert!(
            std::str::from_utf8(&malformed.bytes)
                .unwrap()
                .contains("{\"type\":\"a\",\"type\":\"b\"}")
        );
        connection.execute("DROP TABLE part", []).unwrap();
        assert_eq!(
            read(&source, Limits::default()).unwrap_err(),
            Unavailable::Database
        );
    }

    #[test]
    fn a_read_transaction_cannot_mix_native_database_generations() {
        let (_directory, database) = fixture();
        let writer = Connection::open(&database).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        let source = lookup(&database, "ses_selected", Limits::default()).unwrap();
        let original = read(&source, Limits::default()).unwrap().bytes;
        let mut connection = open(&database).unwrap();
        let transaction = connection.transaction().unwrap();
        let _: String = transaction
            .query_row("SELECT id FROM session", [], |row| row.get(0))
            .unwrap();
        writer
            .execute(
                "UPDATE part SET data='{\"type\":\"text\",\"text\":\"changed\"}'",
                [],
            )
            .unwrap();
        assert_eq!(
            materialize(&transaction, "ses_selected", Limits::default()).unwrap(),
            original
        );
        transaction.commit().unwrap();
        assert_ne!(read(&source, Limits::default()).unwrap().bytes, original);
    }
}
