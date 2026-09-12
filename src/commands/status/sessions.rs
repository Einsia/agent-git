//! Session rows describe local evidence, never infer process liveness from an adoption link.

use crate::adapter::native_snapshot::{Budget, Unavailable};
use crate::domain::{link, meta, repo::Repo, store::Store};
use crate::infra::{config, local_git::Deadline};

pub(super) const MAX_LINKS: usize = 4_096;
pub(super) const MAX_LINK_BYTES: u64 = 8 * 1024 * 1024;

fn identity(links: &[link::Link]) -> Option<Vec<(String, String)>> {
    links
        .iter()
        .map(|link| Some((link.instance(), link.to_json().ok()?)))
        .collect()
}

fn cell(value: &str) -> String {
    value
        .chars()
        .flat_map(|value| {
            if value.is_control() {
                value.escape_default().collect::<Vec<_>>()
            } else {
                vec![value]
            }
        })
        .collect()
}

fn pending_failure(error: &anyhow::Error) -> &'static str {
    if error
        .downcast_ref::<crate::domain::secret_filter::HydrationBudgetExceeded>()
        .is_some()
    {
        return "unavailable: inspection budget exhausted";
    }
    match error.downcast_ref::<Unavailable>() {
        Some(Unavailable::NotFound) => "unavailable: native session missing",
        Some(Unavailable::BudgetExceeded) => "unavailable: inspection budget exhausted",
        Some(Unavailable::Changed) => "unavailable: native source changed",
        Some(Unavailable::Unsupported) => "unavailable: no read-only native provider",
        Some(Unavailable::Database) => "unavailable: native database evidence is invalid",
        _ => "unavailable: incomplete or inaccessible native evidence",
    }
}

fn branch_head(repo: &Repo, branch: &str, deadline: Deadline) -> crate::Result<String> {
    let reference = format!("refs/heads/{branch}");
    let checked =
        repo.inspection_output_with_deadline(&["check-ref-format", &reference], 1024, deadline)?;
    anyhow::ensure!(
        checked.status.success() && checked.stderr.is_empty(),
        "invalid branch"
    );
    let output = repo.inspection_output_with_deadline(
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
        128,
        deadline,
    )?;
    anyhow::ensure!(
        output.status.success() && output.stderr.is_empty(),
        "branch unavailable"
    );
    let head = std::str::from_utf8(&output.stdout)?.trim();
    anyhow::ensure!(
        matches!(head.len(), 40 | 64) && head.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid commit identity"
    );
    Ok(head.to_owned())
}

fn initial_row(claim: &link::Link) -> Vec<String> {
    let slug = match (&claim.owner, &claim.agent) {
        (Some(owner), Some(agent)) => format!("{owner}/{agent}"),
        _ => "—".into(),
    };
    vec![
        claim.session_id.clone(),
        claim.source.clone(),
        slug,
        claim.branch.clone().unwrap_or_else(|| "—".into()),
        "—".into(),
        "unavailable: incomplete claim".into(),
        "incomplete claim; process unverified".into(),
    ]
}

fn inspect(
    store: &Store,
    claim: &link::Link,
    all: &[link::Link],
    complete: bool,
    budget: &mut Budget,
    deadline: Deadline,
) -> Vec<String> {
    let mut row = initial_row(claim);
    if claim.merge_archive.is_some() && claim.superseded_by.is_none() {
        row[5] = "not inspected: merge exploration".into();
        row[6] = "merge exploration; process unverified".into();
        return row;
    }
    if !claim.is_active() {
        row[5] = "not inspected: superseded instance".into();
        row[6] = "superseded; process unverified".into();
        return row;
    }
    let (Some(owner), Some(agent), Some(branch)) = (&claim.owner, &claim.agent, &claim.branch)
    else {
        return row;
    };
    if [owner, agent]
        .iter()
        .any(|name| crate::domain::repo::valid_name(name).is_err() || name.trim() != name.as_str())
    {
        row[6] = "invalid claim identity; process unverified".into();
        return row;
    }
    let Ok(path) = config::repo_dir(owner, agent) else {
        return row;
    };
    let Some(repo) = Repo::open(path).map(Repo::exact_root_inspection) else {
        row[5] = "unavailable: local repository missing".into();
        row[6] = "repository missing; process unverified".into();
        return row;
    };
    let Ok(head) = branch_head(&repo, branch, deadline) else {
        row[5] = "unavailable: local branch missing or unreadable".into();
        row[6] = "branch unavailable; process unverified".into();
        return row;
    };
    row[4] = meta::short(&meta::id_from_sha(&head));
    if !complete {
        row[5] = "unavailable: claim inventory incomplete".into();
        row[6] = "claim unverified; process unverified".into();
        return row;
    }
    let matching = all
        .iter()
        .filter(|candidate| {
            candidate.is_active()
                && candidate.agent.as_ref() == Some(agent)
                && candidate.branch.as_ref() == Some(branch)
                && candidate
                    .owner
                    .as_ref()
                    .is_none_or(|candidate| candidate == owner)
        })
        .count();
    if matching != 1 {
        row[5] = "unavailable: competing local claims".into();
        row[6] = "conflicting claims; process unverified".into();
        return row;
    }
    match link::claim_update_busy(store, claim) {
        Ok(true) => {
            row[5] = "unavailable: claim update in progress".into();
            row[6] = "busy claim update; process unverified".into();
            return row;
        }
        Err(_) => {
            row[5] = "unavailable: claim lock cannot be inspected".into();
            row[6] = "claim unverified; process unverified".into();
            return row;
        }
        Ok(false) => {}
    }
    if claim.materialized_from.as_ref().is_some_and(|baseline| {
        !matches!(baseline.len(), 40 | 64) || !baseline.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        row[5] = "unavailable: invalid materialized identity".into();
        row[6] = "claim unverified; process unverified".into();
        return row;
    }
    if claim
        .materialized_from
        .as_ref()
        .is_some_and(|baseline| baseline != &head)
    {
        row[5] = "unavailable: materialized baseline differs from branch head".into();
        row[6] = "stale baseline; process unverified".into();
        return row;
    }
    row[6] = "current claim; process unverified".into();
    row[5] =
        match super::super::diff::pending::inspect_status(&repo, claim, &head, budget, deadline) {
            Ok(summary) => summary,
            Err(error) => pending_failure(&error).into(),
        };
    match branch_head(&repo, branch, deadline) {
        Ok(current) if current == head => {}
        checked => {
            row[4] = "—".into();
            row[5] = if checked.is_err() {
                "unavailable: branch could not be rechecked"
            } else {
                "unavailable: branch changed during inspection"
            }
            .into();
            row[6] = "claim unverified; process unverified".into();
        }
    }
    row
}

pub(super) struct Page {
    pub rows: Vec<Vec<String>>,
    pub inventory_complete: bool,
}

pub(super) fn rows(
    store: &Store,
    links: &[link::Link],
    complete: bool,
    offset: usize,
    limit: usize,
) -> Page {
    let before = identity(links);
    let mut budget = Budget::new(256 * 1024 * 1024);
    let deadline = Deadline::new();
    let rows = links
        .iter()
        .skip(offset)
        .take(limit)
        .enumerate()
        .map(|(index, claim)| {
            if index < 8 {
                inspect(store, claim, links, complete, &mut budget, deadline)
            } else {
                let mut row = initial_row(claim);
                row[5] = "unavailable: per-page inspection limit".into();
                row[6] = "not inspected; process unverified".into();
                row
            }
        })
        .collect::<Vec<_>>();
    finish_rows(store, before, complete, rows)
}

// Invalidated inventory cannot authorize a later unadopted-session decision, even on an empty page.
fn finish_rows(
    store: &Store,
    before: Option<Vec<(String, String)>>,
    complete: bool,
    mut rows: Vec<Vec<String>>,
) -> Page {
    let (mut after, issues) = link::list_checked_with_limits(store, MAX_LINKS, MAX_LINK_BYTES);
    after.sort_by_key(|link| !link.is_active());
    let unchanged = before.is_some() && before == identity(&after) && issues.is_empty();
    if !unchanged {
        for row in &mut rows {
            row[4] = "—".into();
            row[5] = "unavailable: claim inventory changed or incomplete".into();
            row[6] = "claim unverified; process unverified".into();
        }
    }
    Page {
        rows: rows
            .into_iter()
            .map(|row| row.iter().map(|value| cell(value)).collect())
            .collect(),
        inventory_complete: complete && unchanged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    type Inventory = BTreeMap<PathBuf, (bool, bool, u32, Vec<u8>)>;

    fn files(root: &Path) -> Inventory {
        fn visit(root: &Path, path: &Path, out: &mut Inventory) {
            let metadata = std::fs::symlink_metadata(path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt as _;
                metadata.permissions().mode()
            };
            #[cfg(not(unix))]
            let mode = 0;
            out.insert(
                path.strip_prefix(root).unwrap().to_owned(),
                (
                    metadata.is_dir(),
                    metadata.permissions().readonly(),
                    mode,
                    if metadata.is_file() {
                        std::fs::read(path).unwrap()
                    } else {
                        Vec::new()
                    },
                ),
            );
            if metadata.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    visit(root, &entry.unwrap().path(), out);
                }
            }
        }
        let mut out = BTreeMap::new();
        visit(root, root, &mut out);
        out
    }

    fn observed(store: &Store) -> Vec<link::Link> {
        let (mut links, issues) = link::list_checked_with_limits(store, MAX_LINKS, MAX_LINK_BYTES);
        assert!(issues.is_empty(), "the initial inventory must be readable");
        links.sort_by_key(|link| !link.is_active());
        links
    }

    fn verified_row(claim: &link::Link) -> Vec<String> {
        let mut row = initial_row(claim);
        row[4] = "agit-01234567".into();
        row[5] = "0 unsettled user turns".into();
        row[6] = "current claim; process unverified".into();
        row
    }

    #[test]
    fn final_inventory_changes_invalidate_details_and_adoption_discovery() {
        for change in ["added", "removed", "rewritten", "malformed"] {
            for populated_page in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let store = Store::at(dir.path());
                let claim = link::Link::new("codex", "first", None);
                let path = link::write(&store, &claim).unwrap();
                let before = observed(&store);
                match change {
                    "added" => {
                        link::write(&store, &link::Link::new("codex", "second", None)).unwrap();
                    }
                    "removed" => std::fs::remove_file(path).unwrap(),
                    "rewritten" => {
                        let mut changed = claim.clone();
                        changed.superseded_by = Some("codex/replacement".into());
                        link::write(&store, &changed).unwrap();
                    }
                    "malformed" => std::fs::write(path, b"{").unwrap(),
                    _ => unreachable!(),
                }
                let disk = files(dir.path());
                let page = if populated_page {
                    finish_rows(&store, identity(&before), true, vec![verified_row(&claim)])
                } else {
                    // An omitted page still participates in the inventory recheck.
                    rows(&store, &before, true, before.len(), 1)
                };
                assert!(!page.inventory_complete, "{change}, page={populated_page}");
                assert_eq!(page.rows.len(), usize::from(populated_page));
                for row in page.rows {
                    assert_eq!(row[4], "—");
                    assert_eq!(row[5], "unavailable: claim inventory changed or incomplete");
                    assert_eq!(row[6], "claim unverified; process unverified");
                }
                let discovery = super::super::uncaptured(&before, page.inventory_complete);
                assert!(discovery.sessions.is_empty());
                assert_eq!(discovery.errors.len(), 1);
                assert_eq!(discovery.errors[0].runtime, "claims");
                assert_eq!(
                    discovery.errors[0].message,
                    "unavailable: claim inventory incomplete; adoption cannot be determined"
                );
                assert_eq!(
                    files(dir.path()),
                    disk,
                    "observation must not modify the store"
                );
            }
        }
    }

    #[test]
    fn an_initially_empty_inventory_cannot_hide_a_new_claim() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        let before = observed(&store);
        assert!(before.is_empty());
        link::write(&store, &link::Link::new("codex", "newly-adopted", None)).unwrap();
        let disk = files(dir.path());
        let page = rows(&store, &before, true, 0, 1);
        assert!(page.rows.is_empty());
        assert!(!page.inventory_complete);
        assert!(
            super::super::uncaptured(&before, page.inventory_complete)
                .sessions
                .is_empty()
        );
        assert_eq!(files(dir.path()), disk);
    }

    #[test]
    fn unchanged_inventory_preserves_details_but_cannot_repair_initial_incompleteness() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        let claim = link::Link::new("codex", "stable", None);
        link::write(&store, &claim).unwrap();
        let before = observed(&store);
        let disk = files(dir.path());
        let detail = verified_row(&claim);
        let page = finish_rows(&store, identity(&before), true, vec![detail.clone()]);
        assert!(page.inventory_complete);
        assert_eq!(page.rows, vec![detail]);
        let empty = rows(&store, &before, true, before.len(), 1);
        assert!(empty.inventory_complete);
        assert!(empty.rows.is_empty());
        assert!(!finish_rows(&store, identity(&before), false, Vec::new()).inventory_complete);
        assert!(!finish_rows(&store, None, true, Vec::new()).inventory_complete);
        assert_eq!(files(dir.path()), disk);
    }
}
