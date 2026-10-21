//! Native reads retain the enrolled source and current workspace confinement.

use super::policy::{self, CanonicalRoots};
use super::runtime_sources::{Registry, RuntimeSource};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone)]
pub struct RuntimeContext {
    pub source: RuntimeSource,
    registry: Registry,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogRow {
    pub session_ref: String,
    pub source_id: String,
    pub source_generation: u64,
    pub runtime: String,
    pub native_session_id: String,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub gist: Option<String>,
    pub updated_at_ms: Option<i64>,
    pub writer_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_ref: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub technical: bool,
}

#[derive(Serialize)]
pub struct CatalogPage {
    pub rows: Vec<CatalogRow>,
    pub next_cursor: Option<String>,
    pub reliable: bool,
}

pub struct LocatedThread {
    source_id: String,
    indexed_transcript: PathBuf,
    pub native_session_id: String,
    pub cwd: PathBuf,
    pub transcript: PathBuf,
}

impl RuntimeContext {
    pub fn resolve(registry: &Registry, source_id: &str) -> crate::Result<Self> {
        Ok(Self {
            source: registry.resolve(source_id)?,
            registry: registry.clone(),
        })
    }

    pub(crate) fn validate(&self) -> crate::Result<()> {
        let current = self.registry.resolve(&self.source.source_id)?;
        ensure!(
            current.generation == self.source.generation,
            "runtime source changed; resolve its context again"
        );
        self.source.validate()
    }

    pub fn list_page(
        &self,
        roots: &CanonicalRoots,
        after: Option<&str>,
        limit: usize,
    ) -> crate::Result<CatalogPage> {
        self.validate()?;
        if roots.is_empty() {
            return Ok(CatalogPage {
                rows: Vec::new(),
                next_cursor: None,
                reliable: true,
            });
        }
        let page = if super::runtime_files::cursor(after) {
            super::runtime_files::page(&self.source.home, after, limit)?
        } else {
            match crate::adapter::codex_index::thread_page_in(&self.source.home, after, limit) {
                Ok(page) => super::runtime_files::Page {
                    threads: page.threads,
                    next_cursor: page.next_cursor,
                    reliable: true,
                },
                Err(_) if after.is_none() => {
                    super::runtime_files::page(&self.source.home, None, limit)?
                }
                Err(error) => return Err(error),
            }
        };
        let mut rows = Vec::new();
        for thread in page.threads {
            let Some(cwd) = thread.cwd.as_deref() else {
                continue;
            };
            let Ok(cwd) = policy::require_within(std::path::Path::new(cwd), roots) else {
                continue;
            };
            rows.push(CatalogRow {
                session_ref: self.source.session_ref(&thread.id),
                source_id: self.source.source_id.clone(),
                source_generation: self.source.generation,
                runtime: self.source.runtime.clone(),
                writer_active: self.writer_active(&thread.id),
                parent_session_ref: parent_thread(thread.source.as_deref())
                    .map(|parent| self.source.session_ref(&parent)),
                technical: is_guardian(thread.source.as_deref()),
                native_session_id: thread.id,
                cwd,
                title: thread.title.or(thread.name),
                gist: thread.gist,
                updated_at_ms: thread.updated_at_ms,
            });
        }
        Ok(CatalogPage {
            rows,
            next_cursor: page.next_cursor,
            reliable: page.reliable,
        })
    }

    pub fn locate(&self, native: &str, roots: &CanonicalRoots) -> crate::Result<LocatedThread> {
        self.locate_with(native, roots, true)
    }

    /// For callers holding a lock other conversations need: a cold transcript is restored in the
    /// background and reported as [`crate::adapter::codex_rollout::Restoring`] instead of being
    /// decoded while the lock is held.
    pub(crate) fn locate_ready(
        &self,
        native: &str,
        roots: &CanonicalRoots,
    ) -> crate::Result<LocatedThread> {
        self.locate_with(native, roots, false)
    }

    fn locate_with(
        &self,
        native: &str,
        roots: &CanonicalRoots,
        wait: bool,
    ) -> crate::Result<LocatedThread> {
        self.validate()?;
        let thread = crate::adapter::codex_index::thread_by_id_in(&self.source.home, native)
            .map(Ok)
            .unwrap_or_else(|| super::runtime_files::locate(&self.source.home, native))?;
        ensure!(
            thread.id == native,
            "native index returned a different conversation"
        );
        let cwd = policy::require_within(
            std::path::Path::new(
                thread
                    .cwd
                    .as_deref()
                    .context("native conversation has no project directory")?,
            ),
            roots,
        )?;
        if !wait
            && crate::adapter::codex_rollout::begin_restore(&thread.rollout_path)
                .context("native conversation transcript is unavailable")?
        {
            return Err(crate::adapter::codex_rollout::Restoring.into());
        }
        let transcript = crate::adapter::codex_rollout::restore(&thread.rollout_path)
            .and_then(|path| path.canonicalize())
            .context("native conversation transcript is unavailable")?;
        ensure!(
            transcript.starts_with(&self.source.home),
            "native conversation points outside its runtime source"
        );
        Ok(LocatedThread {
            source_id: self.source.source_id.clone(),
            indexed_transcript: thread.rollout_path,
            native_session_id: thread.id,
            cwd,
            transcript,
        })
    }

    pub(crate) fn history_parent(
        &self,
        physical_id: &str,
        roots: &CanonicalRoots,
        limits: crate::adapter::native_snapshot::Limits,
    ) -> crate::Result<PathBuf> {
        let carrier = self.history_parent_carrier(physical_id, roots, limits)?;
        Ok(crate::adapter::codex_rollout::restore(&carrier)?)
    }

    /// The parent carrier as it is on disk, admitted by its header without restoring it.
    pub(crate) fn history_parent_carrier(
        &self,
        physical_id: &str,
        roots: &CanonicalRoots,
        limits: crate::adapter::native_snapshot::Limits,
    ) -> crate::Result<PathBuf> {
        self.validate()?;
        let carrier = crate::adapter::native_snapshot::codex_carrier_in(
            &self.source.home,
            physical_id,
            limits,
        )?;
        let path = carrier.canonicalize()?;
        ensure!(
            path.starts_with(&self.source.home),
            "native history points outside its runtime source"
        );
        let header = crate::adapter::codex_rollout::first_line(
            &path,
            crate::adapter::codex_rollout::HEADER_LIMIT,
        )?;
        ensure!(
            header.ends_with(b"\n"),
            "native parent header is incomplete"
        );
        let header: serde_json::Value = serde_json::from_slice(&header)?;
        ensure!(
            header["type"] == "session_meta",
            "native parent header is invalid"
        );
        uuid::Uuid::parse_str(
            header["payload"]["id"]
                .as_str()
                .context("native parent has no logical identity")?,
        )?;
        policy::require_within(
            std::path::Path::new(
                header["payload"]["cwd"]
                    .as_str()
                    .context("native parent has no project directory")?,
            ),
            roots,
        )?;
        Ok(path)
    }

    pub fn writer_active(&self, native: &str) -> Option<bool> {
        let native = uuid::Uuid::parse_str(native).ok()?;
        crate::adapter::codex_ownership::writer_active_in(&self.source.home, &native.to_string())
            .ok()
    }

    pub(crate) fn settings(
        &self,
        thread: &LocatedThread,
    ) -> crate::Result<super::native_settings::Settings> {
        self.validate()?;
        ensure!(
            thread.source_id == self.source.source_id,
            "conversation belongs to a different runtime source"
        );
        ensure!(
            thread.indexed_transcript.canonicalize()? == thread.transcript,
            "native transcript path changed; locate the conversation again"
        );
        Ok(super::native_settings::read_codex_in(
            &self.source.home,
            &thread.indexed_transcript,
            &thread.native_session_id,
        ))
    }
}

fn is_guardian(source: Option<&str>) -> bool {
    source
        .and_then(|source| serde_json::from_str::<serde_json::Value>(source).ok())
        .is_some_and(|source| {
            source
                .pointer("/subagent/other")
                .and_then(serde_json::Value::as_str)
                == Some("guardian")
        })
}

fn parent_thread(source: Option<&str>) -> Option<String> {
    let source = source.filter(|source| source.len() <= 4096)?;
    let origin: serde_json::Value = serde_json::from_str(source).ok()?;
    let parent = origin
        .pointer("/subagent/thread_spawn/parent_thread_id")?
        .as_str()?;
    Some(uuid::Uuid::parse_str(parent).ok()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only native provenance classifies internal work; a role or prompt cannot hide a task.
    #[test]
    fn child_provenance_preserves_source_identity_without_hiding_user_tasks() {
        let parent = uuid::Uuid::new_v4().to_string();
        let source = serde_json::json!({"subagent":{"thread_spawn":{
            "parent_thread_id":parent,"agent_role":"guardian"}}})
        .to_string();
        assert_eq!(parent_thread(Some(&source)), Some(parent));
        assert!(!is_guardian(Some(&source)));
        assert!(is_guardian(Some(r#"{"subagent":{"other":"guardian"}}"#)));
        assert!(!is_guardian(Some(r#"{"subagent":"review"}"#)));
        assert_eq!(
            parent_thread(Some(
                r#"{"subagent":{"thread_spawn":{"parent_thread_id":"../outside"}}}"#
            )),
            None
        );
    }

    fn native_store(home: &std::path::Path, project: &std::path::Path, model: &str) {
        std::fs::create_dir(home).unwrap();
        let transcript = home.join("thread.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let db = rusqlite::Connection::open(home.join("state_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER, model TEXT, reasoning_effort TEXT, approval_mode TEXT, sandbox_policy TEXT)").unwrap();
        for (id, cwd, archived) in [
            ("a", project.with_file_name("project-other"), 0),
            ("b", project.join("nested"), 0),
            ("c", project.to_path_buf(), 1),
            ("d", project.to_path_buf(), 0),
        ] {
            db.execute("INSERT INTO threads VALUES (?1,?2,?3,'question','cli',123,?4,?5,'high','never','{\"type\":\"read-only\"}')",
                rusqlite::params![id, transcript.to_str().unwrap(), cwd.to_str().unwrap(), archived, model]).unwrap();
        }
    }

    #[test]
    fn bounded_discovery_retains_nested_projects_and_advances_past_unrelated_rows() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join("nested")).unwrap();
        std::fs::create_dir(root.path().join("project-other")).unwrap();
        let home = root.path().join("custom-home");
        native_store(&home, &project, "model-a");
        let parent = uuid::Uuid::new_v4().to_string();
        let database = rusqlite::Connection::open(home.join("state_1.sqlite")).unwrap();
        database
            .execute("ALTER TABLE threads ADD COLUMN source TEXT", [])
            .unwrap();
        database
            .execute(
                "UPDATE threads SET source=?1 WHERE id='b'",
                [
                    serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":parent}}})
                        .to_string(),
                ],
            )
            .unwrap();
        database
            .execute(
                "UPDATE threads SET source=?1 WHERE id='d'",
                [r#"{"subagent":{"other":"guardian"}}"#],
            )
            .unwrap();
        let registry = Registry::at(root.path().join("registry")).unwrap();
        let source = registry.register(&home, None, None, None).unwrap();
        let context = RuntimeContext::resolve(&registry, &source.source_id).unwrap();
        let roots = CanonicalRoots::from_untrusted([project]);
        let first = context.list_page(&roots, None, 1).unwrap();
        assert!(first.rows.is_empty());
        assert_eq!(first.next_cursor.as_deref(), Some("a"));
        let second = context
            .list_page(&roots, first.next_cursor.as_deref(), 1)
            .unwrap();
        assert_eq!(second.rows[0].native_session_id, "b");
        assert_eq!(
            second.rows[0].parent_session_ref,
            Some(source.session_ref(&parent))
        );
        assert!(!second.rows[0].technical);
        assert_eq!(second.next_cursor.as_deref(), Some("b"));
        let third = context
            .list_page(&roots, second.next_cursor.as_deref(), 1)
            .unwrap();
        assert_eq!(third.rows[0].native_session_id, "d");
        assert!(third.rows[0].technical);
        assert!(third.next_cursor.is_none());
        assert!(context.locate("a", &roots).is_err());
    }

    /// Codex compresses a rollout that has been idle for long enough and keeps naming its plain
    /// path in the state database. Resolving only the plain file makes every dormant managed
    /// conversation unreadable and impossible to resume.
    #[test]
    fn dormant_conversations_resolve_after_codex_compresses_their_rollout() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join("nested")).unwrap();
        let home = root.path().join("home");
        native_store(&home, &project, "model-a");
        let transcript = home.join("thread.jsonl");
        let header = format!(
            "{}\n",
            serde_json::json!({"type":"session_meta","payload":{"id":"b","cwd":project.join("nested")}})
        );
        std::fs::write(
            home.join("thread.jsonl.zst"),
            ruzstd::encoding::compress_to_vec(
                header.as_bytes(),
                ruzstd::encoding::CompressionLevel::Fastest,
            ),
        )
        .unwrap();
        std::fs::remove_file(&transcript).unwrap();
        let registry = Registry::at(root.path().join("registry")).unwrap();
        let source = registry.register(&home, None, None, None).unwrap();
        let context = RuntimeContext::resolve(&registry, &source.source_id).unwrap();
        let roots = CanonicalRoots::from_untrusted([project]);

        let thread = context.locate("b", &roots).unwrap();
        assert_eq!(thread.transcript, transcript.canonicalize().unwrap());
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), header);
        assert_eq!(
            context.settings(&thread).unwrap().model()["model"],
            "model-a"
        );
    }

    #[test]
    fn settings_are_source_scoped_and_removal_revokes_a_captured_context() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join("nested")).unwrap();
        let registry = Registry::at(root.path().join("registry")).unwrap();
        let roots = CanonicalRoots::from_untrusted([project.clone()]);
        let mut contexts = Vec::new();
        for name in ["alpha", "beta"] {
            let home = root.path().join(name);
            native_store(&home, &project, name);
            let source = registry.register(&home, None, None, None).unwrap();
            contexts.push(RuntimeContext::resolve(&registry, &source.source_id).unwrap());
        }
        let first = contexts[0].locate("b", &roots).unwrap();
        let second = contexts[1].locate("b", &roots).unwrap();
        assert_eq!(
            contexts[0].settings(&first).unwrap().model()["model"],
            "alpha"
        );
        assert_eq!(
            contexts[1].settings(&second).unwrap().model()["model"],
            "beta"
        );
        assert!(contexts[1].settings(&first).is_err());
        registry.remove(&contexts[0].source.source_id).unwrap();
        assert!(contexts[0].settings(&first).is_err());
        assert!(contexts[0].list_page(&roots, None, 10).is_err());
        assert!(contexts[1].list_page(&roots, None, 10).is_ok());
    }
}
