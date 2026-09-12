//! Transaction inspection reports retained intent without acquiring or repairing its lock.

use crate::adapter::native_snapshot::{Limits, read_file_bytes};
use crate::domain::{mergetx, repo::Repo};
use anyhow::{Context, ensure};
use std::path::{Path, PathBuf};

const MAX_TRANSACTION_BYTES: usize = 1024 * 1024;

fn read_optional(path: &Path) -> crate::Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(metadata) => ensure!(
            metadata.is_file(),
            "transaction carrier is not a regular file"
        ),
        Err(error) => return Err(error.into()),
    }
    Ok(Some(read_file_bytes(
        path,
        Limits {
            bytes: MAX_TRANSACTION_BYTES,
            working_bytes: MAX_TRANSACTION_BYTES + 1,
            ..Limits::default()
        },
    )?))
}

fn inspect(repo: &Repo) -> crate::Result<Option<mergetx::Tx>> {
    let repo = repo.clone().exact_root_inspection();
    let path = repo.common_dir()?.join(mergetx::LOCK_FILE);
    let Some(before) = read_optional(&path)? else {
        ensure!(
            read_optional(&path)?.is_none(),
            "transaction appeared during inspection"
        );
        return Ok(None);
    };
    let tx: mergetx::Tx = serde_json::from_slice(&before).context("invalid merge transaction")?;
    ensure!(
        !tx.source.trim().is_empty() && tx.source.len() <= 4096,
        "transaction source is missing or exceeds the inspection budget"
    );
    ensure!(
        tx.target.len() <= 4096,
        "transaction target exceeds the inspection budget"
    );
    let reference = format!("refs/heads/{}", tx.target);
    let checked = repo.inspection_output(&["check-ref-format", &reference], 1024)?;
    ensure!(
        checked.status.success() && checked.stderr.is_empty(),
        "invalid transaction target"
    );
    ensure!(
        [&tx.target_head, &tx.source_head].iter().all(|oid| {
            matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
        }),
        "invalid transaction endpoint"
    );
    ensure!(
        read_optional(&path)?.as_deref() == Some(before.as_slice()),
        "transaction changed during inspection"
    );
    Ok(Some(tx))
}

fn cell(value: &str) -> String {
    let mut result = String::new();
    let mut chars = value.chars();
    for character in chars.by_ref().take(160) {
        if character.is_control() {
            result.extend(character.escape_default());
        } else {
            result.push(character);
        }
    }
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

#[derive(serde::Serialize)]
pub(super) struct Page {
    pub items: Vec<Transaction>,
    pub repositories_omitted: usize,
    pub incomplete: bool,
}

#[derive(serde::Serialize)]
pub(super) struct Transaction {
    repo: String,
    target: Option<String>,
    source: Option<String>,
    target_head: Option<String>,
    source_head: Option<String>,
    picked_count: Option<usize>,
    summary_ready: Option<bool>,
    error: Option<&'static str>,
}

impl Page {
    pub fn rows(&self) -> Vec<Vec<String>> {
        self.items
            .iter()
            .map(
                |tx| match (&tx.target, &tx.source, tx.picked_count, tx.summary_ready) {
                    (Some(target), Some(source), Some(picks), Some(ready)) => vec![
                        cell(&format!("{}@{target}", tx.repo)),
                        cell(source),
                        format!(
                            "open; {picks} picks; summary {}",
                            if ready { "ready" } else { "missing" }
                        ),
                    ],
                    _ => vec![
                        cell(&tx.repo),
                        "—".into(),
                        tx.error.unwrap_or("unavailable").into(),
                    ],
                },
            )
            .collect()
    }
}

pub(super) fn page(agents: &[(String, String, PathBuf)]) -> Page {
    let omitted = agents.len().saturating_sub(128);
    let mut page = Page {
        items: Vec::new(),
        repositories_omitted: omitted,
        incomplete: omitted > 0,
    };
    for (owner, name, path) in agents.iter().take(128) {
        let repo = format!("{owner}/{name}");
        let item = match inspect(&Repo::at(path)) {
            Ok(None) => continue,
            Ok(Some(tx)) => {
                let picked_count = tx.picked_count();
                let summary_ready = tx.has_summary();
                Transaction {
                    repo,
                    target: Some(tx.target),
                    source: Some(tx.source),
                    target_head: Some(tx.target_head),
                    source_head: Some(tx.source_head),
                    picked_count: Some(picked_count),
                    summary_ready: Some(summary_ready),
                    error: None,
                }
            }
            Err(_) => {
                page.incomplete = true;
                Transaction {
                    repo,
                    target: None,
                    source: None,
                    target_head: None,
                    source_head: None,
                    picked_count: None,
                    summary_ready: None,
                    error: Some("unavailable: transaction missing, changed, or unreadable"),
                }
            }
        };
        page.items.push(item);
    }
    page
}
