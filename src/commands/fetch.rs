//! `agit fetch` — objects and remote refs only.
//!
//! Local branches never move; the workspace and the runtime are never touched (the PRD's
//! "Remote" section: push, pull and clone carry no side effects). With an `<owner/repo>` argument
//! any readable repo comes down as a peer — cross-repo merge / cherry-pick take their material
//! this way.
//!
//! # A repo this machine does not have is fetched anyway
//!
//! Restricting "pull as a peer" to repos already on this machine splits one command into two: the
//! caller gets "doesn't exist locally" and is sent to `agit clone`, and clone does another job —
//! it creates a workspace, claims a session, loads the VIEW into the runtime — while someone
//! taking material only wants objects and refs. The job becomes "clone a full checkout first,
//! then pick out of it".
//!
//! A repo that is not here gets an empty repo created in place, `origin` attached, and one fetch.
//! What lands is objects and `refs/remotes/origin/*`: no local branch, no checkout, no link, no
//! binding to any directory. Reference resolution falls back to remote-tracking refs
//! ([`crate::domain::refs`]), so `agit cherry-pick alice/notes@exp#3` works right after.

use super::CmdResult;
use crate::domain::repo::Repo;
use crate::hub::identity::{self, RemoteIdentity};
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::path::Path;

#[derive(ClapArgs)]
pub struct Args {
    /// Peer repo to pull (owner/repo). Default: the context repo’s origin.
    pub repo: Option<String>,
    /// Fetch every bound repo.
    #[arg(long, conflicts_with = "repo")]
    pub all: bool,
}

pub fn run(args: Args) -> CmdResult {
    if args.all {
        return fetch_all();
    }
    match args.repo {
        Some(slug) => fetch_slug(&slug),
        None => fetch_context(),
    }
}

fn fetch_all() -> CmdResult {
    let root = crate::infra::config::repos_dir()?;
    let mut n = 0;
    if let Ok(owners) = std::fs::read_dir(&root) {
        for o in owners.flatten() {
            let Ok(repos) = std::fs::read_dir(o.path()) else {
                continue;
            };
            for r in repos.flatten() {
                let Some(repo) = Repo::open(r.path()) else {
                    continue;
                };
                if repo.remote("origin").is_none() {
                    continue;
                }
                let slug = format!(
                    "{}/{}",
                    o.file_name().to_string_lossy(),
                    r.file_name().to_string_lossy()
                );
                let owner = o.file_name().to_string_lossy().to_string();
                let name = r.file_name().to_string_lossy().to_string();
                match do_fetch(&repo, &owner, &name) {
                    Ok(()) => {
                        n += 1;
                        println!("{slug} fetched");
                    }
                    Err(e) => ui::warning(&format!("{slug}: fetch failed: {e:#}")),
                }
            }
        }
    }
    ui::success(&format!("fetched {n} repos"));
    Ok(ExitCode::Ok)
}

fn fetch_slug(slug: &str) -> CmdResult {
    let (owner, name) = match super::parse_slug(slug) {
        Ok(v) => v,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Usage);
        }
    };
    let dir = crate::infra::config::repo_dir(&owner, &name).unwrap_or_default();
    let existing = Repo::open(&dir);
    // Not on this machine yet: pull it down as a peer right now (objects and refs only).
    let fresh = existing.is_none();
    let repo = match existing {
        Some(r) => r,
        None => match adopt_peer(&owner, &name, &dir) {
            Ok(r) => r,
            Err(e) => {
                ui::error(&format!("can’t fetch {slug}: {e:#}"));
                ui::hint(
                    "a private agent needs the owner’s grant — check who you are with `agit whoami`",
                );
                ui::hint(&format!(
                    "want a working checkout instead? `agit clone {slug}`"
                ));
                return Ok(ExitCode::Network);
            }
        },
    };
    match do_fetch(&repo, &owner, &name) {
        Ok(()) => {
            ui::success(&format!(
                "{slug} fetched (objects and remote refs; local branches untouched)"
            ));
            if fresh {
                // Say what shape it is, or "where is the workspace?" is the guaranteed next
                // question.
                println!(
                    "{}",
                    ui::dim(&format!(
                        "  a peer, not a checkout: objects and refs only. Take material with `agit cherry-pick {slug}@<branch>#<n>`"
                    ))
                );
            }
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            ui::error(&format!("fetch failed: {e:#}"));
            if fresh {
                // A fetch that never lands leaves nothing but an empty repo. Keeping it puts an
                // entry with nothing in it into `agit repo list`, and the next fetch does not
                // need it.
                let _ = std::fs::remove_dir_all(&dir);
            }
            Ok(ExitCode::Network)
        }
    }
}

/// Pull a repo this machine does not have down as a peer: create the repo, attach `origin`,
/// nothing else.
///
/// Deliberately **not** `git clone`: clone checks out a workspace, and fetch promises "objects
/// and remote refs only". After init + fetch there is no local branch at all and
/// `refs/remotes/origin/*` is complete — the shape taking material needs, and what makes a
/// repeated `agit fetch` idempotent.
fn adopt_peer(owner: &str, name: &str, dir: &Path) -> crate::Result<Repo> {
    // Only the server knows `clone_url` (the hub address moves), so ask first. That ask is also
    // the read-permission check: without permission it fails here, while nothing is on disk yet.
    let client = crate::hub::Client::from_env();
    let remote = client.get_agent(owner, name)?;
    println!(
        "fetching {} from {}…",
        ui::bold(&format!("{owner}/{name}")),
        ui::accent(client.base())
    );
    let identity = RemoteIdentity::new(client.base(), &remote.agent_id)?;
    init_peer(dir, &remote.clone_url, &identity)
}

/// The shape a peer repo lands in: an empty repo plus an `origin`, nothing else.
fn init_peer(dir: &Path, clone_url: &str, remote_identity: &RemoteIdentity) -> crate::Result<Repo> {
    let repo = Repo::init(dir)?;
    identity::pin(&repo, remote_identity)?;
    repo.set_remote(clone_url)?;
    Ok(repo)
}

fn fetch_context() -> CmdResult {
    let cwd = std::env::current_dir()?;
    let ctx = match super::context::resolve(&cwd) {
        Ok(c) => c,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            ui::hint("or name the repo: `agit fetch <owner/repo>`");
            return Ok(ExitCode::Ref);
        }
    };
    println!(
        "{}",
        ui::dim(&format!("  target: {} ({})", ctx.repo, ctx.via))
    );
    let (owner, name) = ctx.owner_name()?;
    let dir = crate::infra::config::repo_dir(&owner, &name)?;
    let Some(repo) = Repo::open(&dir) else {
        ui::error(&format!("{} doesn’t exist locally.", ctx.repo));
        ui::hint(&format!("fetch it first: `agit clone {}`", ctx.repo));
        return Ok(ExitCode::Precondition);
    };
    // A read-only checkout (origin under someone else's name): anonymous / read-only fetching of
    // a read-only repo must still work — the server's upload-pack goes through authorize_read,
    // and a public repo needs no sign-in.
    match do_fetch(&repo, &owner, &name) {
        Ok(()) => {
            ui::success("fetched (objects and remote refs; local branches untouched)");
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            ui::error(&format!("fetch failed: {e:#}"));
            Ok(ExitCode::Network)
        }
    }
}

fn do_fetch(repo: &Repo, owner: &str, name: &str) -> crate::Result<()> {
    let client = crate::hub::Client::from_env();
    identity::verify_slug(repo, &client, owner, name)?;
    let out = crate::hub::git::run(repo, &["fetch", "origin", "--prune", "--tags"])?;
    if !out.ok() {
        anyhow::bail!("{}", out.stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer comes down as "objects and refs", not as a checkout.
    ///
    /// This pins "no materialization": fetch creates the repo when this machine does not have it,
    /// and what it creates must carry no local branch and no workspace file — otherwise it is
    /// the same command as `agit clone`, and someone taking material does not want a checkout
    /// that `commit` treats as a live repo.
    #[test]
    fn a_fresh_peer_has_a_remote_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!(
            "agit-fetch-peer-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let identity =
            RemoteIdentity::new("http://hub.test", "00000000-0000-0000-0000-000000000001").unwrap();
        let repo = init_peer(&dir, "http://hub.test/alice/notes.git", &identity).unwrap();
        assert_eq!(
            repo.remote("origin").as_deref(),
            Some("http://hub.test/alice/notes.git")
        );
        assert_eq!(identity::read(&repo).unwrap(), Some(identity));
        // No local branch at all — fetch never moves a local branch, including "creating the
        // first one".
        let heads = repo
            .git_opt(&["for-each-ref", "--format=%(refname)", "refs/heads/"])
            .unwrap_or_default();
        assert!(
            heads.trim().is_empty(),
            "a peer must have no local branch: {heads}"
        );
        // No checked-out files either (nothing outside `.git`).
        let visible: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != ".git")
            .collect();
        assert!(
            visible.is_empty(),
            "a peer must materialize no file: {visible:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
