//! An isolated observation worker cannot initialize stores, nudge upgrades, or emit native text.

use crate::adapter::{
    native_snapshot::{Limits, Unavailable},
    opencode::status_snapshot,
};
use crate::domain::{link::Link, repo::Repo};
use crate::infra::local_git::Deadline;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;

const WORKER: &str = "__status-opencode-observe-v1";
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
pub(super) const RESERVATION: usize = 128 * 1024 * 1024;

pub(super) fn limits() -> Limits {
    Limits {
        bytes: 2 * 1024 * 1024,
        working_bytes: 8 * 1024 * 1024,
        records: 16_384,
        lookup_entries: 4_096,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    repo: PathBuf,
    session: String,
    claim: String,
    tip: String,
    log: String,
}

pub(super) fn inspect(
    repo: &Repo,
    claim: &Link,
    tip: &str,
    log: &str,
    deadline: Deadline,
) -> crate::Result<String> {
    let input = serde_json::to_vec(&Request {
        repo: repo.root().to_owned(),
        session: claim.session_id.clone(),
        claim: claim.to_json()?,
        tip: tip.to_owned(),
        log: log.to_owned(),
    })?;
    if input.len() > MAX_REQUEST_BYTES {
        return Err(Unavailable::BudgetExceeded.into());
    }
    let mut command = Command::new(std::env::current_exe()?);
    command.arg(WORKER);
    let output = deadline
        .output(command, Some(&input), MAX_RESPONSE_BYTES)
        .map_err(|_| Unavailable::BudgetExceeded)?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(Unavailable::Read.into());
    }
    let response: Result<String, Unavailable> =
        serde_json::from_slice(&output.stdout).map_err(|_| Unavailable::Read)?;
    response.map_err(Into::into)
}

/// This entry is checked before argument parsing or any ordinary startup side effect.
pub(crate) fn worker(args: &[OsString]) -> Option<i32> {
    if args.len() != 2 || args[1] != WORKER {
        return None;
    }
    // SQLite's heap limit is process-global; only the isolated worker changes it.
    unsafe {
        rusqlite::ffi::sqlite3_hard_heap_limit64(status_snapshot::SQLITE_HEAP_BYTES);
    }
    let result = execute();
    let written = serde_json::to_writer(std::io::stdout().lock(), &result)
        .and_then(|()| std::io::stdout().flush().map_err(serde_json::Error::io));
    Some(if written.is_ok() { 0 } else { 4 })
}

fn execute() -> Result<String, Unavailable> {
    let mut input = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut input)
        .map_err(|_| Unavailable::Read)?;
    if input.len() > MAX_REQUEST_BYTES {
        return Err(Unavailable::BudgetExceeded);
    }
    let request: Request = serde_json::from_slice(&input).map_err(|_| Unavailable::Read)?;
    let limit = limits();
    if !request.repo.is_absolute()
        || request.session.len() > 4_096
        || request.claim.len() > 64 * 1024
        || request.log.len() > limit.bytes
        || !matches!(request.tip.len(), 40 | 64)
        || !request.tip.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(Unavailable::Read);
    }
    let claim = Link::from_json("opencode", &request.session, request.claim.as_bytes())
        .map_err(|_| Unavailable::Read)?;
    if !claim.is_active()
        || claim.merge_archive.is_some()
        || claim.owner.is_none()
        || claim.agent.is_none()
        || claim.branch.is_none()
    {
        return Err(Unavailable::Read);
    }
    let repo = Repo::open(&request.repo)
        .ok_or(Unavailable::Read)?
        .local_objects_only();
    if repo.root() != request.repo.as_path() {
        return Err(Unavailable::Read);
    }
    let bytes = status_snapshot::read(&request.session, limit)?;
    super::opencode::compare_snapshot(
        &repo,
        &claim,
        &request.tip,
        &request.log,
        &bytes,
        limit,
        true,
    )
    .map_err(|error| {
        error
            .downcast_ref::<Unavailable>()
            .copied()
            .unwrap_or(Unavailable::Incomplete)
    })
}
