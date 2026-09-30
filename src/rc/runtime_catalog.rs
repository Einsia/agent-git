//! A persistent discovery index is independent of browser attachment and native writers.

use super::policy::CanonicalRoots;
use super::runtime_context::{CatalogRow, RuntimeContext};
use super::runtime_sources::Registry;
use anyhow::ensure;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const SCAN_PAGE: usize = 100;
mod deltas;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListRequest {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub resolve_session: Option<String>,
    #[serde(default)]
    pub after_revision: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl ListRequest {
    pub(crate) fn validate(&self) -> crate::Result<()> {
        ensure!(
            (1..=500).contains(&self.limit),
            "catalog page limit must be between 1 and 500"
        );
        ensure!(
            self.cursor.is_none() || self.resolve_session.is_none(),
            "catalog lookup cannot include a cursor"
        );
        ensure!(
            self.after_revision.is_none()
                || (self.cursor.is_none() && self.resolve_session.is_none()),
            "catalog changes cannot include a snapshot cursor or exact lookup"
        );
        for cursor in [&self.cursor, &self.resolve_session].into_iter().flatten() {
            ensure!(
                cursor.len() == 70
                    && cursor.starts_with("local-")
                    && cursor[6..].bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid catalog cursor"
            );
        }
        Ok(())
    }
}

fn default_limit() -> usize {
    100
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Coverage {
    pub source_id: String,
    pub source_generation: u64,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Page {
    pub rows: Vec<CatalogRow>,
    pub next_cursor: Option<String>,
    pub revision: u64,
    pub coverage: Vec<Coverage>,
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<super::runtime_discovery::Report>,
    #[serde(default)]
    pub changes_cursor: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<deltas::Removed>,
    #[serde(default)]
    pub reset: bool,
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsRequest {
    #[serde(default)]
    pub workspace_id: Option<String>,
    pub session_id: String,
    pub source_id: String,
    pub source_generation: u64,
    pub native_session_id: String,
    #[serde(default)]
    pub expected_cwd: Option<PathBuf>,
}

#[derive(Clone)]
pub(crate) struct Catalog {
    registry: Registry,
    database: PathBuf,
}

impl Catalog {
    pub fn open() -> crate::Result<Self> {
        Self::at(
            Registry::open()?,
            crate::infra::config::agit_home()?.join("runtime-catalog"),
        )
    }

    fn at(registry: Registry, directory: PathBuf) -> crate::Result<Self> {
        crate::infra::config::create_state_dir(&directory)?;
        #[cfg(windows)]
        crate::infra::windows_security::private_directory(&directory)?;
        let database = directory.join("catalog.sqlite");
        let mut options = crate::infra::config::state_file_options();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&database)?;
        ensure!(
            file.metadata()?.is_file(),
            "runtime catalog is not a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
                "runtime catalog must be private to this OS account"
            );
        }
        let catalog = Self { registry, database };
        let connection = catalog.connection()?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            (0..=2).contains(&version),
            "unsupported runtime catalog schema"
        );
        if version == 0 {
            connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS metadata (id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL);
             INSERT OR IGNORE INTO metadata VALUES (1,0);
             CREATE TABLE IF NOT EXISTS sources (
                source_id TEXT PRIMARY KEY, generation INTEGER NOT NULL, scope TEXT NOT NULL,
                epoch INTEGER NOT NULL, cursor TEXT, status TEXT NOT NULL, due INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS sessions (
                session_ref TEXT PRIMARY KEY, source_id TEXT NOT NULL, cwd TEXT NOT NULL,
                epoch INTEGER NOT NULL, row TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS sessions_source ON sessions(source_id);
             CREATE INDEX IF NOT EXISTS sessions_cwd ON sessions(cwd);
             PRAGMA user_version=1;
             COMMIT;"
        )?;
        }
        if version < 2 {
            deltas::initialize(&connection)?;
        }
        Ok(catalog)
    }

    fn connection(&self) -> crate::Result<Connection> {
        let connection = Connection::open_with_flags(
            &self.database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        connection.busy_timeout(std::time::Duration::from_millis(100))?;
        Ok(connection)
    }

    /// A failed or interrupted sweep cannot delete rows that its remaining pages have not seen.
    pub fn reconcile(&self, roots: &CanonicalRoots) -> crate::Result<()> {
        self.reconcile_at(roots, chrono::Utc::now().timestamp_millis())
    }

    fn reconcile_at(&self, roots: &CanonicalRoots, now: i64) -> crate::Result<()> {
        self.registry.retire_missing_temporary()?;
        let enrolled = self.registry.list()?;
        let mut connection = self.connection()?;
        let scope = serde_json::to_string(&**roots)?;
        let transaction = connection.transaction()?;
        let known: Vec<String> = transaction
            .prepare("SELECT source_id FROM sources")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut changed = false;
        for id in known {
            if !enrolled
                .iter()
                .any(|source| source.enabled && source.source_id == id)
            {
                transaction.execute("DELETE FROM sessions WHERE source_id=?1", [&id])?;
                transaction.execute("DELETE FROM sources WHERE source_id=?1", [&id])?;
                changed = true;
            }
        }
        for source in enrolled.iter().filter(|source| source.enabled) {
            changed |= transaction.execute(
                "INSERT INTO sources VALUES (?1,?2,?3,1,NULL,'scanning',0)
                 ON CONFLICT(source_id) DO UPDATE SET generation=excluded.generation,scope=excluded.scope,
                    epoch=sources.epoch+1,cursor=NULL,status='scanning',due=0
                 WHERE sources.generation!=excluded.generation OR sources.scope!=excluded.scope",
                params![source.source_id, source.generation, scope],
            )? > 0;
        }
        if changed {
            bump(&transaction)?;
        }
        deltas::prune(&transaction)?;
        transaction.commit()?;
        let next: Option<(String, u64, i64, Option<String>)> = connection.query_row(
            "SELECT source_id,generation,epoch,cursor FROM sources WHERE due<=?1 ORDER BY due,source_id LIMIT 1",
            [now], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional()?;
        let Some((source_id, generation, epoch, cursor)) = next else {
            return Ok(());
        };
        let page = (|| {
            let context = RuntimeContext::resolve(&self.registry, &source_id)?;
            ensure!(
                context.source.generation == generation,
                "runtime source changed during discovery"
            );
            let page = context.list_page(roots, cursor.as_deref(), SCAN_PAGE)?;
            context.validate()?;
            Ok::<_, anyhow::Error>(page)
        })();
        let transaction = connection.transaction()?;
        match page {
            Ok(page) => {
                let mut changed = false;
                for row in page.rows {
                    let serialized = serde_json::to_string(&row)?;
                    let previous: Option<String> = transaction
                        .query_row(
                            "SELECT row FROM sessions WHERE session_ref=?1",
                            [&row.session_ref],
                            |r| r.get(0),
                        )
                        .optional()?;
                    changed |= previous.as_deref() != Some(&serialized);
                    transaction.execute(
                        "INSERT INTO sessions VALUES (?1,?2,?3,?4,?5) ON CONFLICT(session_ref)
                         DO UPDATE SET cwd=excluded.cwd,epoch=excluded.epoch,row=excluded.row",
                        params![
                            row.session_ref,
                            source_id,
                            row.cwd.to_string_lossy(),
                            epoch,
                            serialized
                        ],
                    )?;
                }
                let complete = page.next_cursor.is_none();
                if complete && page.reliable {
                    changed |= transaction.execute(
                        "DELETE FROM sessions WHERE source_id=?1 AND epoch!=?2",
                        params![source_id, epoch],
                    )? > 0;
                }
                let status = if !complete {
                    "scanning"
                } else if page.reliable {
                    "ready"
                } else {
                    "unavailable"
                };
                let old_status: String = transaction.query_row(
                    "SELECT status FROM sources WHERE source_id=?1",
                    [&source_id],
                    |r| r.get(0),
                )?;
                changed |= old_status != status;
                transaction.execute(
                    "UPDATE sources SET cursor=?2,epoch=?3,status=?4,due=?5 WHERE source_id=?1",
                    params![
                        source_id,
                        page.next_cursor,
                        epoch + i64::from(complete),
                        status,
                        now + if complete { 2000 } else { 1 }
                    ],
                )?;
                if changed {
                    bump(&transaction)?;
                }
            }
            Err(_) => {
                let changed = transaction.execute("UPDATE sources SET status='unavailable',due=?2 WHERE source_id=?1 AND status!='unavailable'", params![source_id, now + 5000])?;
                transaction.execute(
                    "UPDATE sources SET cursor=NULL,epoch=epoch+1,due=?2 WHERE source_id=?1",
                    params![source_id, now + 5000],
                )?;
                if changed > 0 {
                    bump(&transaction)?;
                }
            }
        }
        deltas::prune(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn list(&self, roots: &CanonicalRoots, request: &ListRequest) -> crate::Result<Page> {
        request.validate()?;
        let sources = self.registry.list()?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let revision =
            transaction.query_row("SELECT revision FROM metadata WHERE id=1", [], |r| r.get(0))?;
        let mut coverage = Vec::new();
        let mut valid = std::collections::HashSet::new();
        for source in sources.into_iter().filter(|source| source.enabled) {
            let status: Option<String> = transaction
                .query_row(
                    "SELECT status FROM sources WHERE source_id=?1 AND generation=?2",
                    params![source.source_id, source.generation],
                    |r| r.get(0),
                )
                .optional()?;
            let identity_valid = source.validate().is_ok();
            let status = if identity_valid {
                valid.insert((source.source_id.clone(), source.generation));
                status.unwrap_or_else(|| "scanning".into())
            } else {
                "unavailable".to_owned()
            };
            coverage.push(Coverage {
                source_id: source.source_id,
                source_generation: source.generation,
                status,
            });
        }
        let complete = coverage.iter().all(|source| source.status == "ready");
        if let Some(after) = &request.after_revision {
            let delta = deltas::read(&transaction, roots, &valid, after, request.limit)?;
            return Ok(Page {
                rows: delta.rows,
                next_cursor: None,
                revision,
                complete,
                coverage,
                discovery: self.registry.discovery()?,
                changes_cursor: delta.cursor,
                removed: delta.removed,
                reset: delta.reset,
                has_more: delta.more,
            });
        }
        let changes_cursor = deltas::snapshot(&transaction, roots, &valid)?;
        if roots.is_empty() {
            return Ok(Page {
                rows: vec![],
                next_cursor: None,
                revision,
                complete,
                coverage,
                discovery: self.registry.discovery()?,
                changes_cursor,
                removed: vec![],
                reset: false,
                has_more: false,
            });
        }
        let predicates = (0..roots.len())
            .map(|i| {
                format!(
                    "(cwd=?{} OR substr(cwd,1,length(?{})+1)=?{}||?2)",
                    i + 4,
                    i + 4,
                    i + 4
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        let comparison = if request.resolve_session.is_some() {
            "="
        } else {
            ">"
        };
        let sql = format!(
            "SELECT row FROM sessions WHERE session_ref{comparison}?1 AND ({predicates}) ORDER BY session_ref LIMIT ?3"
        );
        let mut args = vec![
            rusqlite::types::Value::Text(
                request
                    .resolve_session
                    .as_ref()
                    .or(request.cursor.as_ref())
                    .cloned()
                    .unwrap_or_default(),
            ),
            rusqlite::types::Value::Text(std::path::MAIN_SEPARATOR.to_string()),
            rusqlite::types::Value::Integer((request.limit + 1) as i64),
        ];
        args.extend(
            roots
                .iter()
                .map(|root| rusqlite::types::Value::Text(root.to_string_lossy().into())),
        );
        let serialized: Vec<String> = transaction
            .prepare(&sql)?
            .query_map(rusqlite::params_from_iter(args), |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut rows = serialized
            .into_iter()
            .map(|row| serde_json::from_str::<CatalogRow>(&row))
            .collect::<serde_json::Result<Vec<_>>>()?;
        let more = rows.len() > request.limit;
        rows.truncate(request.limit);
        let next_cursor = more
            .then(|| rows.last().map(|row| row.session_ref.clone()))
            .flatten();
        rows.retain(|row| {
            valid.contains(&(row.source_id.clone(), row.source_generation))
                && super::policy::is_within(&row.cwd, roots)
        });
        if request.resolve_session.is_some() {
            for row in &rows {
                let context = RuntimeContext::resolve(&self.registry, &row.source_id)?;
                ensure!(
                    context.source.generation == row.source_generation
                        && context.source.session_ref(&row.native_session_id) == row.session_ref,
                    "conversation source changed during catalog lookup"
                );
                let thread = context.locate(&row.native_session_id, roots)?;
                ensure!(
                    thread.cwd == row.cwd,
                    "conversation directory changed during catalog lookup"
                );
                super::local_goal::validate_header(
                    &thread.transcript,
                    &row.native_session_id,
                    &thread.cwd,
                )?;
            }
        }
        Ok(Page {
            rows,
            next_cursor,
            revision,
            complete,
            coverage,
            discovery: self.registry.discovery()?,
            changes_cursor,
            removed: vec![],
            reset: false,
            has_more: false,
        })
    }

    pub(crate) fn lookup(&self, session_ref: &str) -> crate::Result<Option<CatalogRow>> {
        if session_ref.len() != 70 || !session_ref.starts_with("local-") {
            return Ok(None);
        }
        let serialized: Option<String> = self
            .connection()?
            .query_row(
                "SELECT row FROM sessions WHERE session_ref=?1",
                [session_ref],
                |row| row.get(0),
            )
            .optional()?;
        let Some(serialized) = serialized else {
            return Ok(None);
        };
        let row: CatalogRow = serde_json::from_str(&serialized)?;
        let source = self.registry.resolve(&row.source_id)?;
        ensure!(
            row.session_ref == session_ref
                && source.generation == row.source_generation
                && source.session_ref(&row.native_session_id) == session_ref,
            "cached conversation source changed; refresh the catalog"
        );
        Ok(Some(row))
    }

    pub fn settings(
        &self,
        roots: &CanonicalRoots,
        request: &SettingsRequest,
    ) -> crate::Result<serde_json::Value> {
        let context = RuntimeContext::resolve(&self.registry, &request.source_id)?;
        ensure!(
            context.source.generation == request.source_generation
                && context.source.session_ref(&request.native_session_id) == request.session_id,
            "conversation source identity changed; refresh the catalog"
        );
        let thread = context.locate(&request.native_session_id, roots)?;
        ensure!(
            request
                .expected_cwd
                .as_ref()
                .is_none_or(|cwd| *cwd == thread.cwd),
            "conversation moved outside its catalog authorization; refresh the catalog"
        );
        let settings = context.settings(&thread)?;
        context.validate()?;
        Ok(
            serde_json::json!({"session_id":request.session_id,"source_id":request.source_id,"source_generation":request.source_generation,"model":settings.model(),"permission_mode":settings.permission_mode}),
        )
    }
}

fn bump(connection: &Connection) -> rusqlite::Result<usize> {
    connection.execute("UPDATE metadata SET revision=revision+1 WHERE id=1", [])
}

pub(crate) struct Worker(tokio::task::JoinHandle<()>);

impl Worker {
    pub fn start() -> Self {
        Self(tokio::spawn(async {
            let mut next_discovery = tokio::time::Instant::now();
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let discover = tokio::time::Instant::now() >= next_discovery;
                if discover {
                    next_discovery =
                        tokio::time::Instant::now() + std::time::Duration::from_secs(15);
                }
                let _ = tokio::task::spawn_blocking(move || -> crate::Result<()> {
                    let catalog = Catalog::open()?;
                    catalog.registry.enroll_default()?;
                    let mirror = super::mirror::Mirror::load();
                    let roots = CanonicalRoots::from_verified(
                        mirror
                            .workspaces
                            .keys()
                            .flat_map(|id| mirror.roots(id).to_vec())
                            .collect(),
                    );
                    if discover {
                        let report = super::runtime_discovery::discover(&catalog.registry, &roots)?;
                        catalog.registry.record_discovery(report)?;
                    }
                    catalog.reconcile(&roots)
                })
                .await;
            }
        }))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_changes_survive_restart_and_reset_after_scope_or_journal_loss() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("runtime");
        let project = temporary.path().join("project");
        let outside = temporary.path().join("project-other");
        for directory in [&home, &project, &outside] {
            std::fs::create_dir(directory).unwrap();
        }
        let roots = CanonicalRoots::from_untrusted([project.clone()]);
        let registry = Registry::at(temporary.path().join("registry")).unwrap();
        let source = registry.register(&home, None, None, None).unwrap();
        let path = temporary.path().join("catalog");
        let catalog = Catalog::at(registry.clone(), path.clone()).unwrap();
        let snapshot = catalog.list(&roots, &request(None)).unwrap();
        let mut row = CatalogRow {
            session_ref: source.session_ref("native"),
            source_id: source.source_id.clone(),
            source_generation: source.generation,
            runtime: "codex".into(),
            native_session_id: "native".into(),
            cwd: project.canonicalize().unwrap(),
            title: Some("Discovered externally".into()),
            gist: None,
            updated_at_ms: None,
            writer_active: Some(true),
            parent_session_ref: None,
            technical: false,
        };
        let connection = catalog.connection().unwrap();
        connection
            .execute(
                "INSERT INTO sessions VALUES(?1,?2,?3,1,?4)",
                params![
                    row.session_ref,
                    row.source_id,
                    row.cwd.to_string_lossy(),
                    serde_json::to_string(&row).unwrap()
                ],
            )
            .unwrap();
        let catalog = Catalog::at(registry.clone(), path).unwrap();
        let mut changes = request(None);
        changes.after_revision = Some(snapshot.changes_cursor);
        let inserted = catalog.list(&roots, &changes).unwrap();
        assert!(!inserted.reset);
        assert_eq!(inserted.rows, vec![row.clone()]);
        changes.after_revision = Some(inserted.changes_cursor.clone());
        let quiet = catalog.list(&roots, &changes).unwrap();
        assert!(quiet.rows.is_empty() && quiet.removed.is_empty());
        assert_eq!(quiet.changes_cursor, inserted.changes_cursor);
        row.cwd = outside.canonicalize().unwrap();
        connection
            .execute(
                "UPDATE sessions SET cwd=?1,row=?2 WHERE session_ref=?3",
                params![
                    row.cwd.to_string_lossy(),
                    serde_json::to_string(&row).unwrap(),
                    row.session_ref
                ],
            )
            .unwrap();
        let moved = catalog.list(&roots, &changes).unwrap();
        assert!(moved.rows.is_empty());
        assert_eq!(moved.removed.len(), 1);
        assert_eq!(moved.removed[0].session_ref, row.session_ref);
        let combined = CanonicalRoots::from_untrusted([project, outside]);
        assert!(catalog.list(&combined, &changes).unwrap().reset);
        let sequence: u64 = serde_json::from_str::<serde_json::Value>(&moved.changes_cursor)
            .unwrap()["sequence"]
            .as_u64()
            .unwrap();
        connection
            .execute("UPDATE change_state SET floor=?1", [sequence])
            .unwrap();
        assert!(catalog.list(&roots, &changes).unwrap().reset);
        changes.after_revision = Some(moved.changes_cursor);
        registry.remove(&source.source_id).unwrap();
        assert!(catalog.list(&roots, &changes).unwrap().reset);
    }

    fn native_store(home: &std::path::Path, project: &std::path::Path, model: &str) -> Connection {
        std::fs::create_dir(home).unwrap();
        let transcript = home.join("thread.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let db = Connection::open(home.join("state_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER, model TEXT, reasoning_effort TEXT, approval_mode TEXT, sandbox_policy TEXT)").unwrap();
        for n in 0..125 {
            let cwd = if n % 2 == 0 {
                project.to_path_buf()
            } else {
                project.join("nested")
            };
            db.execute("INSERT INTO threads VALUES (?1,?2,?3,'question','cli',123,0,?4,'high','never','{\"type\":\"read-only\"}')", params![format!("thread-{n:03}"), transcript.to_str().unwrap(), cwd.to_str().unwrap(), model]).unwrap();
        }
        db
    }

    fn request(cursor: Option<String>) -> ListRequest {
        ListRequest {
            workspace_id: None,
            project_id: None,
            cursor,
            resolve_session: None,
            after_revision: None,
            limit: 73,
        }
    }

    #[test]
    fn paged_catalog_survives_restart_and_keeps_copied_ids_and_incomplete_sweeps_separate() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir_all(project.join("nested")).unwrap();
        let roots = CanonicalRoots::from_untrusted([project.clone()]);
        let registry = Registry::at(directory.path().join("registry")).unwrap();
        let first_db = native_store(&directory.path().join("alpha"), &project, "model-alpha");
        let _second_db = native_store(&directory.path().join("beta"), &project, "model-beta");
        let first = registry
            .register(&directory.path().join("alpha"), None, None, None)
            .unwrap();
        let second = registry
            .register(&directory.path().join("beta"), None, None, None)
            .unwrap();
        let cache_path = directory.path().join("cache");
        let cache = Catalog::at(registry.clone(), cache_path.clone()).unwrap();
        cache.reconcile_at(&roots, 0).unwrap();
        drop(cache);
        let cache = Catalog::at(registry.clone(), cache_path).unwrap();
        for now in 1..4 {
            cache.reconcile_at(&roots, now).unwrap();
        }
        let mut rows = Vec::new();
        let mut cursor = None;
        loop {
            let page = cache.list(&roots, &request(cursor)).unwrap();
            assert!(page.rows.len() <= 73);
            assert!(page.coverage.iter().all(|source| source.status == "ready"));
            assert!(page.complete);
            rows.extend(page.rows);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(rows.len(), 250);
        assert_eq!(
            rows.iter()
                .map(|row| &row.session_ref)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            250
        );
        for source in [&first, &second] {
            std::fs::write(source.home.join("thread.jsonl"), format!("{}\n", serde_json::json!({
                "type":"session_meta", "payload":{"id":"thread-000", "cwd":project.canonicalize().unwrap()}
            }))).unwrap();
            let mut exact = request(None);
            exact.resolve_session = Some(source.session_ref("thread-000"));
            let page = cache.list(&roots, &exact).unwrap();
            assert_eq!(page.rows.len(), 1);
            assert_eq!(page.rows[0].source_id, source.source_id);
            assert!(page.next_cursor.is_none());
            assert!(
                cache
                    .list(
                        &CanonicalRoots::from_untrusted([project.join("nested")]),
                        &exact
                    )
                    .unwrap()
                    .rows
                    .is_empty()
            );
            exact.cursor = exact.resolve_session.clone();
            assert!(cache.list(&roots, &exact).is_err());
        }
        let mut exact = request(None);
        exact.resolve_session = Some(first.session_ref("thread-000"));
        first_db
            .execute(
                "UPDATE threads SET cwd=?1 WHERE id='thread-000'",
                [directory.path().to_str()],
            )
            .unwrap();
        assert!(cache.list(&roots, &exact).is_err());
        first_db
            .execute(
                "UPDATE threads SET cwd=?1 WHERE id='thread-000'",
                [project.to_str()],
            )
            .unwrap();
        let mut settings = SettingsRequest {
            workspace_id: None,
            session_id: first.session_ref("thread-000"),
            source_id: first.source_id.clone(),
            source_generation: first.generation,
            native_session_id: "thread-000".into(),
            expected_cwd: Some(project.canonicalize().unwrap()),
        };
        assert_eq!(
            cache.settings(&roots, &settings).unwrap()["model"]["model"],
            "model-alpha"
        );
        settings.source_id = second.source_id.clone();
        assert!(cache.settings(&roots, &settings).is_err());
        settings.session_id = second.session_ref("thread-000");
        assert_eq!(
            cache.settings(&roots, &settings).unwrap()["model"]["model"],
            "model-beta"
        );
        settings.expected_cwd = Some(project.join("nested").canonicalize().unwrap());
        assert!(cache.settings(&roots, &settings).is_err());

        first_db
            .execute("UPDATE threads SET archived=1 WHERE id='thread-000'", [])
            .unwrap();
        cache.reconcile_at(&roots, 3000).unwrap();
        let count: i64 = cache
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 250, "an incomplete sweep must retain unseen rows");
        for now in 3001..3004 {
            cache.reconcile_at(&roots, now).unwrap();
        }
        let count: i64 = cache
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 249);
        assert_eq!(
            cache
                .lookup(&second.session_ref("thread-000"))
                .unwrap()
                .unwrap()
                .source_id,
            second.source_id
        );
        registry.remove(&first.source_id).unwrap();
        assert!(cache.lookup(&first.session_ref("thread-001")).is_err());
        let mut all = request(None);
        all.limit = 500;
        let page = cache.list(&roots, &all).unwrap();
        assert!(
            page.rows
                .iter()
                .all(|row| row.source_id == second.source_id)
        );
        assert_eq!(
            page.rows.len(),
            125,
            "removal takes effect before the next background scan"
        );
    }

    #[test]
    fn expired_temporary_sources_leave_healthy_conversations_complete_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let registry = Registry::at(directory.path().join("registry")).unwrap();
        let healthy_home = directory.path().join("healthy");
        let _db = native_store(&healthy_home, &project, "model");
        let healthy = registry.register(&healthy_home, None, None, None).unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home/.codex");
        std::fs::create_dir_all(&home).unwrap();
        let expired = registry.enroll_observed(&home, None).unwrap();
        drop(temporary);
        let path = directory.path().join("catalog");
        let roots = CanonicalRoots::from_untrusted([project]);
        let catalog = Catalog::at(registry.clone(), path.clone()).unwrap();
        for now in 0..3 {
            catalog.reconcile_at(&roots, now).unwrap();
        }
        drop(catalog);
        let restarted = Catalog::at(registry.clone(), path).unwrap();
        let page = restarted.list(&roots, &request(None)).unwrap();
        assert!(page.complete);
        assert_eq!(page.coverage.len(), 1);
        assert_eq!(page.coverage[0].source_id, healthy.source_id);
        assert_eq!(page.coverage[0].status, "ready");
        assert!(!page.rows.is_empty());
        assert!(
            page.rows
                .iter()
                .all(|row| row.source_id == healthy.source_id)
        );
        let retired = registry
            .list()
            .unwrap()
            .into_iter()
            .find(|source| source.source_id == expired.source_id)
            .unwrap();
        assert!(!retired.enabled);
        assert!(healthy_home.join("state_1.sqlite").exists());
    }

    #[test]
    fn source_failure_and_project_boundaries_are_visible_without_hiding_healthy_sources() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        let other = directory.path().join("project-other");
        std::fs::create_dir_all(project.join("nested")).unwrap();
        std::fs::create_dir_all(other.join("nested")).unwrap();
        let registry = Registry::at(directory.path().join("registry")).unwrap();
        let home = directory.path().join("healthy");
        let db = native_store(&home, &project, "model");
        let healthy = registry.register(&home, None, None, None).unwrap();
        let home = directory.path().join("unavailable");
        std::fs::create_dir(&home).unwrap();
        let failed = registry.register(&home, None, None, None).unwrap();
        let cache = Catalog::at(registry, directory.path().join("cache")).unwrap();
        let roots = CanonicalRoots::from_untrusted([project.clone(), other.clone()]);
        for now in 0..4 {
            cache.reconcile_at(&roots, now).unwrap();
        }
        let page = cache
            .list(
                &CanonicalRoots::from_untrusted([project.join("nested")]),
                &request(None),
            )
            .unwrap();
        assert_eq!(page.rows.len(), 62);
        assert!(
            page.rows
                .iter()
                .all(|row| row.source_id == healthy.source_id)
        );
        assert!(
            page.coverage.iter().any(
                |source| source.source_id == failed.source_id && source.status == "unavailable"
            )
        );
        drop(db);
        std::fs::remove_file(directory.path().join("healthy/state_1.sqlite")).unwrap();
        for now in 6000..6002 {
            cache.reconcile_at(&roots, now).unwrap();
        }
        let retained = cache
            .list(
                &CanonicalRoots::from_untrusted([project.join("nested")]),
                &request(None),
            )
            .unwrap();
        assert_eq!(retained.rows.len(), 62);
        assert!(
            retained
                .coverage
                .iter()
                .all(|source| source.status == "unavailable")
        );
        let page = cache
            .list(&CanonicalRoots::from_untrusted([other]), &request(None))
            .unwrap();
        assert!(page.rows.is_empty());
    }
}
