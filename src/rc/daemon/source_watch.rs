//! A source watch reads one enrolled native transcript without acquiring its writer.

use crate::rc::{policy::CanonicalRoots, runtime_context::RuntimeContext};
use std::path::PathBuf;

#[derive(Clone)]
pub(super) struct SourceWatch {
    pub context: RuntimeContext,
    pub native_id: String,
    pub cwd: PathBuf,
    pub path: PathBuf,
}

impl SourceWatch {
    pub fn resolve(reference: &str, roots: &CanonicalRoots) -> crate::Result<Option<Self>> {
        if !reference.starts_with("local-") {
            return Ok(None);
        }
        let row = crate::rc::runtime_catalog::Catalog::open()?
            .lookup(reference)?
            .ok_or_else(|| {
                anyhow::anyhow!("source conversation is unavailable; refresh its catalog")
            })?;
        let context = RuntimeContext::resolve(
            &crate::rc::runtime_sources::Registry::open()?,
            &row.source_id,
        )?;
        anyhow::ensure!(
            context.source.generation == row.source_generation,
            "source generation changed"
        );
        let thread = context.locate(&row.native_session_id, roots)?;
        anyhow::ensure!(
            thread.cwd == row.cwd,
            "conversation directory changed since discovery"
        );
        let watch = Self {
            context,
            native_id: row.native_session_id,
            cwd: thread.cwd,
            path: thread.transcript,
        };
        watch.validate(roots)?;
        Ok(Some(watch))
    }

    pub fn validate(&self, roots: &CanonicalRoots) -> crate::Result<()> {
        self.current(roots).map(drop)
    }

    /// [`Self::validate`], moving the watch to the segment Codex continued the thread in.
    /// Returns whether the transcript moved.
    pub fn follow(&mut self, roots: &CanonicalRoots) -> crate::Result<bool> {
        let current = self.current(roots)?;
        let moved = current != self.path;
        self.path = current;
        Ok(moved)
    }

    /// The transcript is either the one being watched or the segment that continues it; any
    /// other change revokes the watch.
    fn current(&self, roots: &CanonicalRoots) -> crate::Result<PathBuf> {
        let current = self.context.locate(&self.native_id, roots)?;
        anyhow::ensure!(
            current.cwd == self.cwd
                && (current.transcript == self.path
                    || crate::adapter::codex_rollout::continues(&current.transcript, &self.path)),
            "native watch source changed"
        );
        crate::rc::local_goal::validate_history_header(
            &current.transcript,
            &self.native_id,
            &self.cwd,
        )?;
        Ok(current.transcript)
    }

    pub fn identity(&self) -> crate::protocol::NativeSourceRef {
        crate::protocol::NativeSourceRef {
            source_id: self.context.source.source_id.clone(),
            generation: self.context.source.generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_watch_rechecks_directory_and_registration_before_publication() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let project = root.path().join("project");
            let home = root.path().join("custom-profile");
            std::fs::create_dir(&project).unwrap();
            std::fs::create_dir(&home).unwrap();
            let project = project.canonicalize().unwrap();
            let native = uuid::Uuid::new_v4().to_string();
            let path = home.join(format!("rollout-2026-10-12T00-00-00-{native}.jsonl"));
            std::fs::write(
                &path,
                format!(
                    "{}\n",
                    serde_json::json!({"type":"session_meta","payload":{"id":native,"cwd":project}})
                ),
            )
            .unwrap();
            let db = rusqlite::Connection::open(home.join("state_1.sqlite")).unwrap();
            db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER)").unwrap();
            db.execute(
                "INSERT INTO threads VALUES (?1,?2,?3,'Preview','cli',1,0)",
                rusqlite::params![native, path.to_str(), project.to_str()],
            )
            .unwrap();
            let registry = crate::rc::runtime_sources::Registry::open().unwrap();
            let source = registry.register(&home, None, None, None).unwrap();
            let roots = CanonicalRoots::from_untrusted([project.clone()]);
            let catalog = crate::rc::runtime_catalog::Catalog::open().unwrap();
            for _ in registry.list().unwrap() {
                catalog.reconcile(&roots).unwrap();
            }
            let watch = SourceWatch::resolve(&source.session_ref(&native), &roots)
                .unwrap()
                .unwrap();
            assert_eq!(watch.native_id, native);
            assert_eq!(watch.identity().source_id, source.source_id);
            watch.validate(&roots).unwrap();
            // A segment that continues the watched carrier is followed; any other file revokes.
            let rollover = |name: &str, base: serde_json::Value| {
                let next = home.join(name);
                std::fs::write(
                    &next,
                    format!(
                        "{}\n",
                        serde_json::json!({"type":"session_meta","payload":{
                    "id":native,"cwd":project,"history_mode":"paginated","history_base":base}})
                    ),
                )
                .unwrap();
                db.execute("UPDATE threads SET rollout_path=?1", [next.to_str()])
                    .unwrap();
                next.canonicalize().unwrap()
            };
            let segment = uuid::Uuid::new_v4();
            rollover(
                &format!(
                    "rollout-2026-10-12T00-00-02-{native}_{}.jsonl",
                    uuid::Uuid::new_v4()
                ),
                serde_json::json!({"thread_id":segment.to_string(),"end_byte_offset":1,"end_ordinal_exclusive":1}),
            );
            assert!(watch.validate(&roots).is_err());
            let next = rollover(
                &format!("rollout-2026-10-12T00-00-01-{native}_{segment}.jsonl"),
                serde_json::json!({"thread_id":native,"end_byte_offset":1,"end_ordinal_exclusive":1}),
            );
            let mut watch = watch;
            assert!(watch.follow(&roots).unwrap());
            assert_eq!(watch.path, next);
            assert!(!watch.follow(&roots).unwrap());
            db.execute("UPDATE threads SET cwd=?1", [root.path().to_str()])
                .unwrap();
            assert!(watch.validate(&roots).is_err());
            db.execute("UPDATE threads SET cwd=?1", [project.to_str()])
                .unwrap();
            registry.remove(&source.source_id).unwrap();
            assert!(watch.validate(&roots).is_err());
            assert!(SourceWatch::resolve(&source.session_ref(&native), &roots).is_err());
        });
    }
}
