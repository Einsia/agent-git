//! `agit doctor --repair-objects OWNER/REPO`: restore what the Hub has of a repository whose loose
//! objects a hard stop left empty, and say plainly what only this machine had.

use super::CmdResult;
use crate::domain::repo::Repo;
use crate::domain::repo::integrity::{self, SET_ASIDE};
use crate::hub::identity;
use crate::infra::config;
use crate::{ExitCode, ui};
use anyhow::Context as _;

pub(super) fn repair(slug: &str) -> CmdResult {
    let (owner, name) = super::super::parse_slug(slug)?;
    let Some(repo) = Repo::open(config::repo_dir(&owner, &name)?) else {
        ui::error(&format!("local repository {slug} is unavailable"));
        return Ok(ExitCode::Ref);
    };
    let Some(damage) = integrity::assess(&repo)? else {
        ui::success(&format!("{slug}: no damaged Git objects"));
        return Ok(ExitCode::Ok);
    };
    println!(
        "{slug}: {}; branches that reach them: {}",
        integrity::damaged_objects_phrase(damage.objects.len()),
        listed(&damage.branches)
    );
    // Recorded first: once the files are gone the objects are only missing, and a repair that
    // stops below is found and resumed by the next check through this record.
    integrity::record(&repo, &damage.objects)?;
    // An empty object file holds nothing to keep, and while it is there Git takes the object as
    // present and never asks for it again.
    let store = repo.git_path("objects")?;
    for object in &damage.objects {
        let file = store.join(&object[..2]).join(&object[2..]);
        if std::fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_file() && meta.len() == 0) {
            std::fs::remove_file(&file)
                .with_context(|| format!("cannot remove {}", file.display()))?;
        }
    }
    if let Err(error) = refetch(&repo, &owner, &name) {
        ui::error(&format!(
            "the damaged objects are recorded, but refetching from the Hub failed: {error:#}"
        ));
        ui::hint(&format!(
            "connect and run `agit doctor --repair-objects {slug}` again; nothing else was changed"
        ));
        return Ok(ExitCode::Network);
    }
    let mut unreadable = Vec::new();
    for branch in &damage.branches {
        let reference = format!("refs/heads/{branch}");
        if !integrity::missing_objects(&repo, &reference).is_empty() {
            unreadable.push(branch.clone());
        }
    }
    if unreadable.is_empty() {
        return settle(
            &repo,
            slug,
            "restored from the Hub; every branch reads again",
        );
    }
    for branch in &unreadable {
        report_lost(&repo, branch);
    }
    set_aside(&repo, slug, &unreadable)
}

/// Fetch every object the Hub's refs reach. A plain fetch sends only what lies beyond the commits
/// this store claims to have, and the damaged objects are behind those commits.
fn refetch(repo: &Repo, owner: &str, name: &str) -> anyhow::Result<()> {
    let client = crate::hub::Client::from_env();
    let identity = identity::resolve_transport_target(repo, &client, owner, name)?;
    let out = crate::hub::git::run_for_remote(
        repo,
        &["fetch", "--refetch", "--prune", "origin", "--tags"],
        &identity,
    )?;
    anyhow::ensure!(out.ok(), "{}", out.stderr.trim());
    Ok(())
}

/// Name the turns of `branch` that only this machine had: they are not on the Hub, and part of
/// them can no longer be read.
fn report_lost(repo: &Repo, branch: &str) {
    let local = readable_state(repo, branch)
        .and_then(|start| {
            repo.git_opt(&["log", "--format=%h %s", &start, "--not", "--remotes=origin"])
        })
        .unwrap_or_default();
    ui::warning(&format!(
        "{branch}: part of its history existed only on this machine and cannot be read"
    ));
    if local.trim().is_empty() {
        println!("  the commits that were not yet on the Hub cannot be listed");
    } else {
        println!("  commits that were not yet on the Hub:");
        for line in local.lines() {
            println!("    {line}");
        }
    }
}

/// Move unreadable branches under [`SET_ASIDE`] once the person confirms. A branch checked out in a
/// worktree stays where it is, since moving it would leave that worktree on no branch. The record
/// of damaged objects is forgotten only when every branch is handled; until then the next check
/// still finds the damage.
fn set_aside(repo: &Repo, slug: &str, branches: &[String]) -> CmdResult {
    let checked_out = repo
        .git_opt(&["worktree", "list", "--porcelain"])
        .unwrap_or_default();
    let movable: Vec<&String> = branches
        .iter()
        .filter(|branch| {
            let held = checked_out
                .lines()
                .any(|line| line == format!("branch refs/heads/{branch}"));
            if held {
                ui::hint(&format!(
                    "{branch} is checked out in a worktree; switch it away and run this again to set the branch aside"
                ));
            }
            !held
        })
        .collect();
    let mut kept_back = movable.len() != branches.len();
    if movable.is_empty() {
        return Ok(ExitCode::Precondition);
    }
    let prompt = format!(
        "Set aside {} under {SET_ASIDE}, so this machine continues from what the Hub has?",
        listed(
            &movable
                .iter()
                .map(|branch| (*branch).clone())
                .collect::<Vec<_>>()
        )
    );
    let confirmed = std::env::var_os("AGIT_YES").is_some()
        || ui::prompt::confirm(&prompt, false)?.unwrap_or(false);
    if !confirmed {
        ui::hint(&format!(
            "nothing was set aside; run `agit --yes doctor --repair-objects {slug}` to do it"
        ));
        return Ok(ExitCode::Precondition);
    }
    let mut kept_refs = Vec::new();
    for branch in movable {
        let from = format!("refs/heads/{branch}");
        let tip = repo.git(&["rev-parse", "--verify", "--quiet", &from])?;
        let Some(state) = readable_state(repo, branch) else {
            // Deleting the branch also deletes its reflog, the last pointer to any earlier state;
            // that is safe only because none of those states reads either.
            repo.git(&["update-ref", "-d", &from, &tip])?;
            ui::success(&format!(
                "{branch} removed: neither its tip {tip} nor any earlier state of it can be read"
            ));
            continue;
        };
        // Named by the state it keeps, so a branch set aside again later never meets an earlier
        // one. The branch goes only once that ref exists.
        let to = format!("{SET_ASIDE}{branch}-{}", &state[..12]);
        let kept = repo.git(&["update-ref", &to, &state, ""]).is_ok()
            || repo
                .git_opt(&["rev-parse", "--verify", "--quiet", &to])
                .is_some_and(|value| value == state);
        if !kept {
            ui::error(&format!(
                "{branch} stays where it is: its last readable state could not be kept as {to}"
            ));
            kept_back = true;
            continue;
        }
        repo.git(&["update-ref", "-d", &from, &tip])?;
        if state == tip {
            ui::success(&format!("{branch} set aside as {to}"));
        } else {
            ui::success(&format!(
                "{branch} set aside as {to}: its tip {tip} cannot be read, so its newest readable \
                 state is kept"
            ));
        }
        kept_refs.push(to);
    }
    for kept in &kept_refs {
        ui::hint(&format!(
            "part of {kept}'s history cannot be read, so Git's own cleanup (`git gc`) cannot finish \
             while it exists; once you have taken what you need, delete it with \
             `git update-ref -d {kept}`"
        ));
    }
    if kept_back {
        return Ok(ExitCode::Precondition);
    }
    settle(repo, slug, "every branch reads again")
}

/// Finish a repair whose branches all read again. The record is forgotten only when no ref outside
/// [`SET_ASIDE`] still reaches a damaged object (see [`integrity::assess`]); a tag or a
/// remote-tracking ref that still does keeps it, so doctor and the commit guard keep finding the
/// damage, and is named here.
fn settle(repo: &Repo, slug: &str, done: &str) -> CmdResult {
    let Some(damage) = integrity::assess(repo)? else {
        ui::success(&format!("{slug}: {done}"));
        return Ok(ExitCode::Ok);
    };
    let holders = integrity::other_refs_reaching(repo, &damage.objects);
    ui::error(&format!(
        "{slug}: {} still reached by {}",
        integrity::damaged_objects_phrase(damage.objects.len()),
        if holders.is_empty() {
            "a version tag".to_owned()
        } else {
            holders.join(", ")
        }
    ));
    ui::hint(&format!(
        "delete those refs, or drop remote-tracking refs the Hub no longer has with \
         `git fetch --prune origin`, then run `agit doctor --repair-objects {slug}` again"
    ));
    Ok(ExitCode::Precondition)
}

/// The newest state of `branch` whose commit reads: its tip, or else the newest entry of its
/// reflog that reads. None when no state reads, or when the ref store keeps no reflog file.
fn readable_state(repo: &Repo, branch: &str) -> Option<String> {
    let tip = repo.git_opt(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}"),
    ])?;
    if integrity::readable(repo, &tip) {
        return Some(tip);
    }
    let reflog = repo.git_path(&format!("logs/refs/heads/{branch}")).ok()?;
    let entries = std::fs::read_to_string(reflog).ok()?;
    entries
        .lines()
        .rev()
        .filter_map(|line| line.split(' ').nth(1))
        .filter(|oid| matches!(oid.len(), 40 | 64) && oid.bytes().any(|byte| byte != b'0'))
        .find(|oid| *oid != tip && integrity::readable(repo, oid))
        .map(str::to_owned)
}

fn listed(branches: &[String]) -> String {
    if branches.is_empty() {
        "none".into()
    } else {
        branches.join(", ")
    }
}
