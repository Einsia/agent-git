//! `agit doctor --repair-worktrees OWNER/REPO`: move a repository's linked worktrees that sit in
//! another repository's place into its own, relinking each to it.
//!
//! Such a worktree names one repository while its `.git` points to another, so a write through
//! its path reaches the wrong one, or Git can't reach its storage at all. The repair only moves:
//! nothing is rewritten in place, and nothing reaches the Hub.

use super::CmdResult;
use crate::commands::worktree;
use crate::domain::repo::Repo;
use crate::domain::store::Store;
use crate::infra::config;
use crate::{ExitCode, ui};
use std::path::Path;

pub(super) fn repair(slug: &str) -> CmdResult {
    crate::commands::migration::refuse_interrupted()?;
    let (owner, name) = super::super::parse_slug(slug)?;
    let Some(primary) = Repo::open(config::repo_dir(&owner, &name)?) else {
        ui::error(&format!("local repository {slug} is unavailable"));
        return Ok(ExitCode::Ref);
    };
    let store = Store::open_or_init()?;
    let _guard = crate::domain::link::lock_repositories_exclusive(&store, &[slug])?;
    let home = worktree::home_for(&primary)?;
    let registrations = worktree::registrations(&primary.common_dir()?);
    // Everything that could stop a move is checked before any link is touched, so a refusal
    // leaves every worktree exactly as it was.
    let mut moves = Vec::new();
    for holder in primary.worktrees()? {
        if holder.primary {
            continue;
        }
        let Some((_, place)) = worktree::sits_in_place_of(slug, &holder.path) else {
            continue;
        };
        let Ok(rest) = worktree::canonical(&holder.path)
            .strip_prefix(&place)
            .map(Path::to_path_buf)
        else {
            continue;
        };
        let to = home.join(rest);
        let refuse = |why: String| {
            ui::error(&format!(
                "can't move {}: {why}; nothing was moved",
                ui::tilde(&holder.path)
            ));
            Ok(ExitCode::Precondition)
        };
        if !holder.path.exists() {
            return refuse(format!(
                "its folder is missing; `git -C {} worktree prune` forgets it",
                quote_arg(&primary.root().to_string_lossy())
            ));
        }
        let Some(registration) = registrations
            .iter()
            .find(|(_, checkout)| worktree::same_place(checkout, &holder.path))
            .map(|(admin, _)| admin.clone())
        else {
            return refuse(format!("{slug} holds no registration for it"));
        };
        if registration.join("locked").exists() {
            return refuse(format!(
                "it is locked; `git worktree unlock {}` if nothing uses it",
                quote_arg(&holder.path.to_string_lossy())
            ));
        }
        // A worktree whose own link names another repository's live registration of it belongs to
        // that repository now; this repository's record of it is out of date.
        if let Some(other) = owned_elsewhere(&holder.path, &registration) {
            return refuse(format!(
                "it is {other}'s worktree now, and {slug}'s record of it is out of date"
            ));
        }
        // The target and every folder on the way to it: a file standing where a folder belongs
        // would stop the move after this worktree's link changed.
        if let Some(taken) = to
            .ancestors()
            .take_while(|place| place.starts_with(&home))
            .find(|place| place.exists() && (*place == to.as_path() || !place.is_dir()))
        {
            return refuse(format!(
                "something is already there at {}",
                ui::tilde(taken)
            ));
        }
        moves.push((holder.path, to, registration));
    }
    if moves.is_empty() {
        ui::success(&format!("every worktree of {slug} is in its own place"));
        return Ok(ExitCode::Ok);
    }
    let all = worktree::canonical(&config::worktrees_dir()?);
    for (from, to, registration) in &moves {
        // This one worktree is pointed at its registration here just before it moves, and no other
        // worktree is touched. A move that fails puts the link back as it was, so no worktree is
        // left linked to this repository from another repository's place.
        let link = from.join(".git");
        let before = std::fs::read(&link)?;
        let moved = worktree::link_to(from, registration)
            .map_err(anyhow::Error::from)
            .and_then(|()| primary.move_worktree(from, to));
        if let Err(error) = moved {
            std::fs::write(&link, &before)?;
            let said = format!("{error:#}");
            ui::error(&format!(
                "can't move {}: {}; it is as it was",
                ui::tilde(from),
                said.lines().next().unwrap_or("Git refused")
            ));
            return Ok(ExitCode::Failure);
        }
        worktree::remove_empty_parents(from, &all);
        println!("moved {} → {}", ui::tilde(from), ui::tilde(to));
    }
    Ok(ExitCode::Ok)
}

/// The repository a worktree's own `.git` link names, when that is not `ours` and its
/// registration there names this worktree back: then the worktree is that repository's.
fn owned_elsewhere(checkout: &Path, ours: &Path) -> Option<String> {
    let admin = worktree::link_of(checkout)?;
    if worktree::same_place(ours, &admin) {
        return None;
    }
    let back = std::fs::read_to_string(admin.join("gitdir")).ok()?;
    let back = worktree::resolve(&admin, back.trim());
    if !worktree::same_place(back.parent()?, checkout) {
        return None;
    }
    // `<repository>/.git/worktrees/<id>`
    let root = admin.parent()?.parent()?.parent()?;
    Some(
        config::repos_dir()
            .ok()
            .and_then(|repos| {
                let repos = worktree::canonical(&repos);
                worktree::canonical(root)
                    .strip_prefix(&repos)
                    .ok()
                    .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            })
            .unwrap_or_else(|| ui::tilde(root)),
    )
}

#[cfg(windows)]
fn quote_arg(value: &str) -> String {
    crate::ui::quote_powershell_argument(value)
}

#[cfg(not(windows))]
fn quote_arg(value: &str) -> String {
    crate::ui::session::shell_arg(value)
}
