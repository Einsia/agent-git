//! Native goal inspection does not load a thread or acquire its control channel.
use anyhow::{Context, ensure};
use serde_json::Value;
use std::{
    io::{BufRead, Read},
    path::{Path, PathBuf},
};

fn validate_header(path: &Path, native: &str, cwd: &Path) -> crate::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut line = String::new();
    std::io::BufReader::new(file.take(1024 * 1024)).read_line(&mut line)?;
    ensure!(line.ends_with('\n'), "Native session header is incomplete");
    let header: Value = serde_json::from_str(&line)?;
    ensure!(
        header["type"] == "session_meta" && header["payload"]["id"] == native,
        "Native session identity does not match"
    );
    let actual = header["payload"]["cwd"]
        .as_str()
        .context("Native working directory is unavailable")?;
    ensure!(
        Path::new(actual).canonicalize()? == cwd,
        "Native session belongs to another working directory"
    );
    Ok(())
}

fn target(params: Value) -> crate::Result<(String, PathBuf, bool)> {
    let session = params["session_id"]
        .as_str()
        .context("Session id is required")?;
    let roster = super::roster::Roster::try_load()?;
    let entry = roster.get(session);
    if let Some(entry) = entry {
        ensure!(
            entry.workspace_id == super::local::WORKSPACE && entry.runtime == "codex",
            "Goal inspection requires a Codex session in the local workspace"
        );
    }
    let native = entry.map(|e| e.thread_id.as_str()).unwrap_or(session);
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
        .or_else(|| params["cwd"].as_str())
        .context("Working directory is required")?;
    let roots = super::mirror::Mirror::load().roots(super::local::WORKSPACE);
    let cwd = super::policy::require_within(Path::new(cwd), &roots)?;
    let empty = source_is_empty(native, &cwd)?;
    Ok((native.to_string(), cwd, empty))
}

fn source_is_empty(native: &str, cwd: &Path) -> crate::Result<bool> {
    if super::harness::codex::fresh::is_empty(native, cwd) {
        return Ok(true);
    }
    let path = crate::adapter::get("codex")?
        .resolve(native, Some(cwd))
        .context("Native transcript is unavailable")?;
    validate_header(&path, native, cwd)?;
    Ok(false)
}

pub async fn read(params: Value) -> crate::Result<Value> {
    let (native, cwd, empty) = tokio::task::spawn_blocking(move || target(params)).await??;
    let result = if empty {
        serde_json::json!({"goal": null})
    } else {
        super::harness::models::codex_goal(cwd.clone(), &native).await?
    };
    let roots = super::mirror::Mirror::load().roots(super::local::WORKSPACE);
    super::policy::require_within(&cwd, &roots)?;
    let redactor = crate::domain::redact::Redactor::try_this_machine()?;
    Ok(redactor.scrub_json(&result).value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn empty_goal_requires_a_live_creator_for_the_same_directory() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let creator = super::super::harness::codex::fresh::FreshCodex::new(
            &native,
            root.path(),
            root.path().join("absent.jsonl"),
        )
        .unwrap();
        assert!(source_is_empty(&native, root.path()).unwrap());
        assert!(source_is_empty(&native, other.path()).is_err());
        drop(creator);
        assert!(source_is_empty(&native, root.path()).is_err());
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
