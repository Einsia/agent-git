//! Source-qualified storage keys cannot alias another native home's thread.

use super::Link;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBinding {
    pub source: crate::protocol::NativeSourceRef,
    pub thread_id: String,
}

impl NativeBinding {
    pub fn key(&self) -> crate::Result<String> {
        let source_id = self
            .source
            .source_id
            .strip_prefix("src-")
            .context("invalid native source identity")?;
        let source_id =
            uuid::Uuid::parse_str(source_id).context("invalid native source identity")?;
        ensure!(
            self.source.source_id == format!("src-{source_id}"),
            "native source identity is not canonical"
        );
        uuid::Uuid::parse_str(&self.thread_id).context("invalid native thread identity")?;
        ensure!(
            self.source.generation > 0,
            "invalid native source generation"
        );
        Ok(self.source.session_ref(&self.thread_id))
    }
}

impl Link {
    pub fn from_native(binding: NativeBinding, cwd: &Path) -> crate::Result<Self> {
        let mut link = Self::new("codex", &binding.key()?, Some(cwd));
        link.native_binding = Some(binding);
        Ok(link)
    }

    pub fn native_thread_id(&self) -> &str {
        self.native_binding
            .as_ref()
            .map_or(&self.session_id, |binding| &binding.thread_id)
    }

    pub(super) fn validate_native_binding(&self) -> crate::Result<()> {
        if let Some(binding) = &self.native_binding {
            ensure!(
                self.source == "codex" && self.session_id == binding.key()?,
                "native source binding differs from its storage key"
            );
        }
        Ok(())
    }

    #[cfg(feature = "rc")]
    fn source_context(
        &self,
    ) -> crate::Result<(
        crate::rc::runtime_context::RuntimeContext,
        crate::rc::policy::CanonicalRoots,
    )> {
        self.validate_native_binding()?;
        let binding = self
            .native_binding
            .as_ref()
            .context("native source binding is missing")?;
        let registry = crate::rc::runtime_sources::Registry::open()?;
        let context = crate::rc::runtime_context::RuntimeContext::resolve(
            &registry,
            &binding.source.source_id,
        )?;
        ensure!(
            context.source.generation == binding.source.generation,
            "native source changed; refresh the session binding"
        );
        let cwd = self
            .cwd
            .as_deref()
            .context("source-qualified link has no working directory")?;
        let roots = crate::rc::policy::CanonicalRoots::from_untrusted([PathBuf::from(cwd)]);
        Ok((context, roots))
    }

    /// The runtime home this link's session is read from: its registered source at the bound
    /// generation, or `None` for a link without a source binding.
    #[cfg(feature = "rc")]
    pub fn native_home(&self) -> crate::Result<Option<PathBuf>> {
        if self.native_binding.is_none() {
            return Ok(None);
        }
        let (context, _) = self.source_context()?;
        Ok(Some(context.source.home))
    }

    #[cfg(feature = "rc")]
    pub(super) fn resolve_source(&self) -> crate::Result<PathBuf> {
        let (context, roots) = self.source_context()?;
        let thread = context.locate(self.native_thread_id(), &roots)?;
        ensure!(
            Some(thread.cwd.as_path())
                == self
                    .cwd
                    .as_deref()
                    .map(Path::new)
                    .map(Path::canonicalize)
                    .transpose()?
                    .as_deref(),
            "native source directory changed"
        );
        crate::rc::local_goal::validate_header(
            &thread.transcript,
            self.native_thread_id(),
            &thread.cwd,
        )?;
        Ok(thread.transcript)
    }

    #[cfg(feature = "rc")]
    pub(super) fn read_source_bytes(&self) -> crate::Result<Vec<u8>> {
        use crate::adapter::native_snapshot::{Limits, Source, Unavailable};
        let (context, roots) = self.source_context()?;
        let path = self.resolve_source()?;
        let source = Source {
            runtime: "codex",
            session_id: self.native_thread_id().into(),
            path,
            database: false,
        };
        let bytes = crate::adapter::codex::lineage_bytes_with_lookup(
            &source,
            Limits::default(),
            false,
            None,
            |id, limits| {
                let path = context
                    .history_parent(id, &roots, limits)
                    .map_err(|_| Unavailable::Changed)?;
                Ok(Source {
                    runtime: "codex",
                    session_id: id.into(),
                    path,
                    database: false,
                })
            },
        )?;
        context.validate()?;
        Ok(bytes)
    }

    #[cfg(not(feature = "rc"))]
    pub fn native_home(&self) -> crate::Result<Option<PathBuf>> {
        ensure!(
            self.native_binding.is_none(),
            "native source resolution requires runtime support"
        );
        Ok(None)
    }

    #[cfg(not(feature = "rc"))]
    pub(super) fn resolve_source(&self) -> crate::Result<PathBuf> {
        anyhow::bail!("native source resolution requires runtime support")
    }

    #[cfg(not(feature = "rc"))]
    pub(super) fn read_source_bytes(&self) -> crate::Result<Vec<u8>> {
        anyhow::bail!("native source reads require runtime support")
    }
}

#[cfg(all(test, feature = "cli"))]
mod tests {
    use super::*;
    use crate::domain::{link, store::Store};
    use serde_json::json;

    #[test]
    fn copied_threads_have_independent_links_and_source_confined_parent_bytes() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let cwd = root.path().join("project");
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.canonicalize().unwrap();
            let registry = crate::rc::runtime_sources::Registry::open().unwrap();
            let store = Store::at(root.path().join("store"));
            let parent = "00000000-0000-0000-0000-000000000001";
            let child = "00000000-0000-0000-0000-000000000002";
            let mut links = vec![];
            for name in ["alpha", "beta"] {
                let home = root.path().join(name);
                std::fs::create_dir(&home).unwrap();
                let archive = home.join("archived_sessions");
                std::fs::create_dir(&archive).unwrap();
                let physical_parent = "00000000-0000-0000-0000-000000000003";
                let parent_path = archive.join(format!("rollout-{physical_parent}.jsonl"));
                let child_path = home.join(format!("rollout-{child}.jsonl"));
                let prefix = format!(
                    "{}\n{}\n",
                    json!({"type":"session_meta","ordinal":0,"payload":{"id":parent,"cwd":cwd}}),
                    json!({"type":"event_msg","ordinal":1,"payload":{"type":"user_message","message":format!("{name}-parent")}})
                );
                std::fs::write(&parent_path, &prefix).unwrap();
                std::fs::write(&child_path, format!("{}\n{}\n",
                    json!({"type":"session_meta","ordinal":2,"payload":{"id":child,"cwd":cwd,"history_mode":"paginated","history_base":{"thread_id":physical_parent,"end_byte_offset":prefix.len(),"end_ordinal_exclusive":2}}}),
                    json!({"type":"event_msg","ordinal":3,"payload":{"type":"user_message","message":format!("{name}-child")}}))).unwrap();
                let db = rusqlite::Connection::open(home.join("state_1.sqlite")).unwrap();
                db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER)").unwrap();
                for (id, path) in [(parent, parent_path), (child, child_path)] {
                    db.execute(
                        "INSERT INTO threads VALUES (?1,?2,?3,'question','cli',1,0)",
                        rusqlite::params![id, path.to_str(), cwd.to_str()],
                    )
                    .unwrap();
                }
                let source = registry.register(&home, None, None, None).unwrap();
                let mut link = Link::from_native(
                    NativeBinding {
                        source: crate::protocol::NativeSourceRef {
                            source_id: source.source_id,
                            generation: source.generation,
                        },
                        thread_id: child.into(),
                    },
                    &cwd,
                )
                .unwrap();
                link.owner = Some("owner".into());
                link.agent = Some("project".into());
                link.branch = Some(name.into());
                link::write(&store, &link).unwrap();
                let restored = link::get(&store, "codex", &link.session_id).unwrap();
                assert_eq!(restored.native_thread_id(), child);
                assert_eq!(restored.native_binding, link.native_binding);
                let bytes = String::from_utf8(restored.read_bytes().unwrap()).unwrap();
                assert!(bytes.contains(&format!("{name}-parent")));
                assert!(bytes.contains(&format!("{name}-child")));
                assert!(!bytes.contains(if name == "alpha" {
                    "beta-parent"
                } else {
                    "alpha-parent"
                }));
                assert_eq!(
                    link::archive_claims_for_branch(&store, "owner", "project", name)
                        .unwrap()
                        .len(),
                    1
                );
                links.push(restored);
            }
            assert_ne!(links[0].session_id, links[1].session_id);
            assert!(link::get(&store, "codex", child).is_none());
            assert_eq!(link::list(&store).len(), 2);
            let mut substituted = links[0].clone();
            substituted.session_id = links[1].session_id.clone();
            assert!(link::write(&store, &substituted).is_err());
            assert!(
                Link::from_json(
                    "codex",
                    &links[1].session_id,
                    links[0].to_json().unwrap().as_bytes()
                )
                .is_err()
            );
            registry
                .remove(&links[0].native_binding.as_ref().unwrap().source.source_id)
                .unwrap();
            assert!(links[0].resolve().is_none());
            assert!(links[0].read_bytes().is_err());
            assert!(links[1].read_bytes().is_ok());
        });
    }
}
