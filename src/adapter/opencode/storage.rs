//! Session identity selects one storage generation before directory filters or content reads.

use super::super::native_snapshot::{Result, Unavailable};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionTable {
    Legacy,
    V2,
}

impl SessionTable {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Legacy => "session",
            Self::V2 => "session_v2",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Layout {
    legacy: bool,
    v2: bool,
    parts: bool,
    projected: Option<SessionTable>,
    pub(super) split_parts: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Route {
    pub(super) table: SessionTable,
    pub(super) projected: bool,
    pub(super) split_parts: bool,
}

pub(super) fn db_path() -> Option<PathBuf> {
    let override_path = std::env::var_os("OPENCODE_DB");
    let override_path = override_path.as_deref().map(Path::new);
    if let Some(path) = override_path {
        if path == Path::new(":memory:") {
            return None;
        }
        if path.is_absolute() {
            return Some(path.to_owned());
        }
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| crate::infra::config::user_home().map(|home| home.join(".local/share")))?
        .join("opencode");
    database_path(&data, override_path)
}

fn database_path(data: &Path, override_path: Option<&Path>) -> Option<PathBuf> {
    match override_path.filter(|path| !path.as_os_str().is_empty()) {
        Some(path) if path == Path::new(":memory:") => None,
        Some(path) => Some(data.join(path)),
        None => Some(data.join("opencode.db")),
    }
}

/// Views and malformed recognized tables cannot masquerade as a supported empty store.
pub(super) fn table_exists(con: &Connection, name: &str) -> Result<bool> {
    let kind: Option<(String, i64)> = con
        .query_row(
            "SELECT type, rootpage FROM sqlite_master WHERE name = ?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| Unavailable::Database)?;
    match kind {
        None => Ok(false),
        Some((kind, root)) if kind == "table" && root > 0 => Ok(true),
        _ => Err(Unavailable::Database),
    }
}

impl Layout {
    pub(super) fn detect(con: &Connection) -> Result<Self> {
        let legacy = table_exists(con, "session")?;
        let v2 = table_exists(con, "session_v2")?;
        if !legacy && !v2 {
            return Err(Unavailable::Database);
        }
        for table in [SessionTable::Legacy, SessionTable::V2] {
            if (table == SessionTable::Legacy && legacy) || (table == SessionTable::V2 && v2) {
                con.prepare(&format!(
                    "SELECT id, project_id, parent_id, directory, time_created, version FROM {} LIMIT 0",
                    table.name()
                ))
                .map_err(|_| Unavailable::Database)?;
            }
        }
        let parts = table_exists(con, "message")? && table_exists(con, "part")?;
        if parts {
            con.prepare("SELECT id, session_id, time_created, data FROM message LIMIT 0")
                .map_err(|_| Unavailable::Database)?;
            con.prepare("SELECT id, message_id, session_id, time_created, data FROM part LIMIT 0")
                .map_err(|_| Unavailable::Database)?;
        }
        let projected_present = table_exists(con, "session_message")?;
        let projected = projected_present
            && con.prepare("SELECT id, session_id, type, seq, time_created, data FROM session_message LIMIT 0").is_ok();
        if projected_present && !projected {
            let occupied: bool = con
                .query_row("SELECT EXISTS(SELECT 1 FROM session_message)", [], |row| {
                    row.get(0)
                })
                .map_err(|_| Unavailable::Database)?;
            if occupied {
                return Err(Unavailable::Database);
            }
        }
        if v2 && !projected || !parts && !projected {
            return Err(Unavailable::Database);
        }
        let projected = projected.then_some(if v2 {
            SessionTable::V2
        } else {
            SessionTable::Legacy
        });
        if let Some(owner) = projected {
            let mut statement = con
                .prepare("PRAGMA foreign_key_list(session_message)")
                .map_err(|_| Unavailable::Database)?;
            let mut rows = statement.query([]).map_err(|_| Unavailable::Database)?;
            while let Some(row) = rows.next().map_err(|_| Unavailable::Database)? {
                let from: String = row.get(3).map_err(|_| Unavailable::Database)?;
                if from == "session_id" {
                    let target: String = row.get(2).map_err(|_| Unavailable::Database)?;
                    let column: String = row.get(4).map_err(|_| Unavailable::Database)?;
                    if target != owner.name() || column != "id" {
                        return Err(Unavailable::Database);
                    }
                }
            }
        }
        let split_parts = table_exists(con, "session_part")?;
        if split_parts {
            con.prepare(
                "SELECT message_id, session_id, ordinal, type, data FROM session_part LIMIT 0",
            )
            .map_err(|_| Unavailable::Database)?;
        }
        Ok(Self {
            legacy,
            v2,
            parts,
            projected,
            split_parts,
        })
    }

    pub(super) fn route(self, con: &Connection, id: &str) -> Result<Route> {
        for (table, present) in [
            (SessionTable::V2, self.v2),
            (SessionTable::Legacy, self.legacy),
        ] {
            if !present {
                continue;
            }
            let mut statement = con
                .prepare(&format!("SELECT id FROM {} WHERE id = ?1", table.name()))
                .map_err(|_| Unavailable::Database)?;
            let mut rows = statement.query([id]).map_err(|_| Unavailable::Database)?;
            let Some(row) = rows.next().map_err(|_| Unavailable::Database)? else {
                continue;
            };
            let actual: String = row.get(0).map_err(|_| Unavailable::Database)?;
            if actual != id {
                return Err(Unavailable::Database);
            }
            if rows.next().map_err(|_| Unavailable::Database)?.is_some() {
                return Err(Unavailable::Ambiguous);
            }
            let projected = self.projected == Some(table)
                && (table == SessionTable::V2
                    || !self.parts
                    || con
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM session_message WHERE session_id = ?1)",
                            [id],
                            |row| row.get::<_, bool>(0),
                        )
                        .map_err(|_| Unavailable::Database)?);
            if !projected && !self.parts {
                return Err(Unavailable::Database);
            }
            return Ok(Route {
                table,
                projected,
                split_parts: projected && self.split_parts,
            });
        }
        Err(Unavailable::NotFound)
    }

    /// Select the authoritative identity before applying project or parent filters.
    pub(super) fn sessions(self) -> String {
        let columns = "id, project_id, parent_id, directory, time_created, time_updated";
        match (self.legacy, self.v2) {
            (true, true) => format!(
                "SELECT {columns} FROM session_v2 UNION ALL SELECT {columns} FROM session s WHERE NOT EXISTS (SELECT 1 FROM session_v2 v WHERE v.id = s.id)"
            ),
            (false, true) => format!("SELECT {columns} FROM session_v2"),
            _ => format!("SELECT {columns} FROM session"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_override_uses_opencode_data_directory_not_working_directory() {
        let data = std::env::temp_dir().join("opencode-data");
        assert_eq!(
            database_path(&data, Some(Path::new("custom.db"))),
            Some(data.join("custom.db"))
        );
        let absolute = data.join("elsewhere/db.sqlite");
        assert_eq!(database_path(&data, Some(&absolute)), Some(absolute));
        assert_eq!(database_path(&data, Some(Path::new(":memory:"))), None);
        assert_eq!(database_path(&data, None), Some(data.join("opencode.db")));
    }
}
