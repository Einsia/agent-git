//! Native goal inspection does not load a thread or acquire its control channel.
use anyhow::{Context, ensure};
use serde_json::Value;
use std::{
    io::{BufRead, Read},
    path::{Path, PathBuf},
};

pub(crate) fn validate_header(path: &Path, native: &str, cwd: &Path) -> crate::Result<()> {
    validate_header_in(path, native, cwd, |actual| Ok(actual.canonicalize()?))
}

/// For reading existing history: the recorded directory may have been removed since, and is
/// compared by the location the workspace policy judges for it.
pub(crate) fn validate_history_header(path: &Path, native: &str, cwd: &Path) -> crate::Result<()> {
    validate_header_in(path, native, cwd, |actual| {
        super::policy::resolved(actual).context("Native working directory cannot be resolved")
    })
}

fn validate_header_in(
    path: &Path,
    native: &str,
    cwd: &Path,
    resolve: impl FnOnce(&Path) -> crate::Result<PathBuf>,
) -> crate::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut line = String::new();
    std::io::BufReader::new(file.take(1024 * 1024)).read_line(&mut line)?;
    // A header still being written is a passing condition, not a different conversation.
    ensure!(
        line.ends_with('\n'),
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Native session header is incomplete"
        )
    );
    let header: Value = serde_json::from_str(&line)?;
    ensure!(
        header["type"] == "session_meta" && header["payload"]["id"] == native,
        "Native session identity does not match"
    );
    let actual = header["payload"]["cwd"]
        .as_str()
        .context("Native working directory is unavailable")?;
    ensure!(
        resolve(Path::new(actual))? == cwd,
        "Native session belongs to another working directory"
    );
    Ok(())
}

struct Target {
    native: String,
    cwd: PathBuf,
    context: Option<super::runtime_context::RuntimeContext>,
    entry: Option<super::roster::Entry>,
}

impl Target {
    fn validate(&self) -> crate::Result<()> {
        let roots = super::mirror::Mirror::load().roots(super::endpoint::WORKSPACE);
        super::policy::require_within(&self.cwd, &roots)?;
        let path = if let Some(context) = &self.context {
            let thread = context.locate(&self.native, &roots)?;
            ensure!(
                thread.cwd == self.cwd,
                "Native conversation directory changed"
            );
            thread.transcript
        } else {
            crate::adapter::get("codex")?
                .resolve(&self.native, Some(&self.cwd))
                .context("Native transcript is unavailable")?
        };
        validate_header(&path, &self.native, &self.cwd)
    }
}

fn target(params: Value) -> crate::Result<Target> {
    let session = params["session_id"]
        .as_str()
        .context("Session id is required")?;
    let roster = super::roster::Roster::try_load()?;
    let entry = roster.get(session);
    if let Some(entry) = entry {
        ensure!(
            entry.workspace_id == super::endpoint::WORKSPACE && entry.runtime == "codex",
            "Goal inspection requires a Codex session in the local workspace"
        );
    }
    let catalog = if entry.is_none() && session.starts_with("local-") {
        Some(
            super::runtime_catalog::Catalog::open()?
                .lookup(session)?
                .context("Conversation source is unavailable; refresh the catalog")?,
        )
    } else {
        None
    };
    let native = entry
        .map(|e| e.thread_id.as_str())
        .or_else(|| catalog.as_ref().map(|row| row.native_session_id.as_str()))
        .unwrap_or(session);
    ensure!(
        !native.is_empty()
            && native.len() <= 128
            && native
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c)),
        "Invalid native session id"
    );
    let cwd = entry
        .map(|e| e.cwd.as_str())
        .or_else(|| catalog.as_ref().and_then(|row| row.cwd.to_str()))
        .or_else(|| params["cwd"].as_str())
        .context("Working directory is required")?;
    let roots = super::mirror::Mirror::load().roots(super::endpoint::WORKSPACE);
    let cwd = super::policy::require_within(Path::new(cwd), &roots)?;
    // A catalog row is fenced to the generation the client listed it under. A roster row names
    // its store across generations; resolution still refuses a removed or replaced home.
    let saved = entry.and_then(|entry| entry.native_source.clone());
    let fenced = saved.is_none();
    let source = saved.or_else(|| {
        catalog
            .as_ref()
            .map(|row| crate::protocol::NativeSourceRef {
                source_id: row.source_id.clone(),
                generation: row.source_generation,
            })
    });
    let context = source
        .map(|source| {
            let context = super::runtime_context::RuntimeContext::resolve(
                &super::runtime_sources::Registry::open()?,
                &source.source_id,
            )?;
            ensure!(
                !fenced || context.source.generation == source.generation,
                "Runtime source changed"
            );
            Ok::<_, anyhow::Error>(context)
        })
        .transpose()?;
    let target = Target {
        native: native.into(),
        cwd,
        context,
        entry: entry.cloned(),
    };
    target.validate()?;
    Ok(target)
}

pub async fn read(params: Value) -> crate::Result<Value> {
    let target = tokio::task::spawn_blocking(move || target(params)).await??;
    let result = if let Some(context) = &target.context {
        super::harness::models::shared_codex_goal(&context.source.native()?, &target.native).await?
    } else {
        super::harness::models::codex_goal(target.cwd.clone(), &target.native).await?
    };
    tokio::task::spawn_blocking(move || {
        target.validate()?;
        let redactor = super::local_history::source_redactor(
            "codex",
            &target.native,
            &target.cwd,
            target.context,
            target.entry.as_ref(),
        )?;
        Ok(redactor.try_scrub_json(&result)?.value)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn source_goals_pin_home_and_revalidate_scope_before_returning() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let project = root.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let mut mirror = super::super::mirror::Mirror::default();
            mirror
                .bind(super::super::endpoint::WORKSPACE, "project", &project)
                .unwrap();
            mirror.save().unwrap();
            let registry = super::super::runtime_sources::Registry::open().unwrap();
            let native = uuid::Uuid::new_v4().to_string();
            let mut sources = Vec::new();
            for name in ["alpha", "beta"] {
                let home = root.path().join(name);
                std::fs::create_dir(&home).unwrap();
                let path = home.join("history.jsonl");
                std::fs::write(
                    &path,
                    format!(
                        "{}\n",
                        serde_json::json!({
                            "type":"session_meta", "payload":{"id":native,"cwd":project}
                        })
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
                sources.push(registry.register(&home, None, None, None).unwrap());
            }
            let catalog = super::super::runtime_catalog::Catalog::open().unwrap();
            let roots = mirror.roots(super::super::endpoint::WORKSPACE);
            for _ in &sources {
                catalog.reconcile(&roots).unwrap();
            }
            let read_target = |source: &super::super::runtime_sources::RuntimeSource| {
                target(serde_json::json!({"session_id":source.session_ref(&native),
                    "cwd":"/forged", "source_id":"forged", "native_session_id":"forged"}))
            };
            let first = read_target(&sources[0]).unwrap();
            let second = read_target(&sources[1]).unwrap();
            assert_eq!(first.native, native);
            assert_eq!(first.cwd, project);
            assert_eq!(first.context.as_ref().unwrap().source.home, sources[0].home);
            assert_eq!(
                second.context.as_ref().unwrap().source.home,
                sources[1].home
            );
            registry.remove(&sources[0].source_id).unwrap();
            assert!(first.validate().is_err());
            assert!(read_target(&sources[0]).is_err());
            second.validate().unwrap();
            mirror.unbind(super::super::endpoint::WORKSPACE, "project");
            mirror.save().unwrap();
            assert!(second.validate().is_err());
        });
    }

    #[test]
    fn transcript_identity_and_directory_must_both_match() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let other = tempfile::tempdir().unwrap();
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{"id":"native","cwd":cwd}})
        )
        .unwrap();
        validate_header(file.path(), "native", &cwd).unwrap();
        assert!(validate_header(file.path(), "foreign", &cwd).is_err());
        assert!(validate_header(file.path(), "native", other.path()).is_err());
    }
}
