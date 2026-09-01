//! `agit pr` — Pull Requests (hub objects).
//!
//! The key division of work from the PRD: **the merge agent runs before the PR is opened, on
//! the contributor's side**; `pr merge` is approve plus land, and never starts a merge agent.
//! To give the author a one-click merge, run `agit merge <your-branch> --into <the target's
//! branch of the same name>` in your own fork first — that branch in the fork is the same
//! logical session as the author's, and the two-parent merge commit it produces carries the
//! author's current head as its first parent, which is what the PR proposes.
//!
//! The mode `pr merge` lands in is decided server-side from the shape of the source (see the
//! backend `prs` module).

use super::CmdResult;
use crate::domain::repo::Repo;
use crate::hub::Client;
use crate::hub::identity::{self, RemoteIdentity};
use crate::infra::config;
use crate::{ExitCode, ui};
use anyhow::{Context, bail};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Open a PR. source is a branch in your fork, target a branch (or main) of theirs.
    Create {
        /// `<owner/repo>[@<target>]`: the PR's destination
        target: String,
        /// Your branch (the source)
        #[arg(short = 'b', long, value_name = "your-branch")]
        branch: String,
        /// Description
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    /// List PRs.
    List { repo: String },
    /// Show a PR.
    Show { id: u64 },
    /// Fetch a PR’s proposed commits into a local `pr/<id>` ref for show/diff review.
    Fetch { id: u64 },
    /// Approve + land (never starts a merge agent).
    Merge {
        id: u64,
        /// Adopt the contributor's branch whole as a new branch (usual for a rewind-type fix)
        #[arg(long, value_name = "new-branch")]
        adopt: Option<String>,
    },
}

pub fn run(args: Args) -> CmdResult {
    let client = match super::require_login() {
        Ok(c) => c,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Auth);
        }
    };
    match args.cmd {
        Cmd::Create {
            target,
            branch,
            message,
        } => create(&client, &target, &branch, message.as_deref()),
        Cmd::List { repo } => list(&client, &repo),
        Cmd::Show { id } => show(&client, id),
        Cmd::Fetch { id } => fetch(&client, id),
        Cmd::Merge { id, adopt } => merge(&client, id, adopt.as_deref()),
    }
}

fn create(client: &Client, target: &str, branch: &str, message: Option<&str>) -> CmdResult {
    let spec = match crate::domain::refs::parse(target) {
        Ok(s) => s,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Usage);
        }
    };
    let (to, tn) = match &spec.repo {
        crate::domain::refs::RepoSel::Slug(o, n) => (o.clone(), n.clone()),
        _ => {
            ui::error("a PR’s destination must be written fully: owner/repo[@target].");
            return Ok(ExitCode::Usage);
        }
    };
    let target_branch = match &spec.base {
        crate::domain::refs::Base::Name(b) => b.clone(),
        _ => "main".to_string(),
    };
    // The source repo is the local current context (your fork).
    let cwd = std::env::current_dir().unwrap_or_default();
    let ctx = match super::context::resolve(&cwd) {
        Ok(c) => c,
        Err(e) => {
            ui::error(&format!("source came from context resolution: {e:#}"));
            return Ok(ExitCode::Ref);
        }
    };
    let (so, sn) = match ctx.owner_name() {
        Ok(v) => v,
        Err(_) => return Ok(ExitCode::Ref),
    };
    let dir = config::repo_dir(&so, &sn)?;
    let Some(repo) = Repo::open(&dir) else {
        ui::error(&format!(
            "source {so}/{sn} does not exist locally; refusing to open a PR without its exact branch head"
        ));
        return Ok(ExitCode::Precondition);
    };
    // Check the repo-local pin before even asking what the reusable slug currently means.
    // The later helper reads it again after the GET, so a concurrent local rebind cannot make
    // the claim use an identity that was not actually pinned when its head was resolved.
    if let Err(e) = identity::require_current(&repo, client.base()) {
        ui::error(&format!("refusing to open a PR from {so}/{sn}: {e:#}"));
        return Ok(ExitCode::Precondition);
    }
    let remote = match client.get_agent(&so, &sn) {
        Ok(remote) => remote,
        Err(e) => {
            let code = api_error_exit(&e);
            ui::error(&format!("cannot verify PR source {so}/{sn}: {e:#}"));
            return Ok(code);
        }
    };
    let source = match source_claim(&repo, client.base(), &so, &sn, branch, &remote.agent_id) {
        Ok(source) => source,
        Err(e) => {
            ui::error(&format!("refusing to open a PR from {so}/{sn}: {e:#}"));
            return Ok(ExitCode::Precondition);
        }
    };
    match client.pr_create(
        &to,
        &tn,
        &so,
        &sn,
        &source.expected_agent_id,
        branch,
        &source.head,
        &target_branch,
        message,
    ) {
        Ok(pr) => {
            ui::success(&format!(
                "PR #{} opened: {}/{}:{} → {}/{}:{}",
                pr.id, so, sn, branch, to, tn, target_branch
            ));
            println!("  {}", ui::dim("for the author to review:"));
            println!(
                "    agit pr fetch {0} && agit show pr/{0} && agit pr merge {0}",
                pr.id
            );
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            let code = api_error_exit(&e);
            ui::error(&format!("{e:#}"));
            if e.downcast_ref::<crate::hub::client::ApiError>()
                .is_some_and(|api| api.status == 409)
            {
                ui::hint(&format!(
                    "push this exact local source branch first: agit push {so}/{sn} -b {branch}"
                ));
            }
            Ok(code)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SourceClaim {
    expected_agent_id: String,
    head: String,
}

/// Bind a PR request to the immutable source object and the exact local branch commit.
///
/// The live slug check catches replacement before POST; the expected ID in the POST is still
/// mandatory because the slug can be replaced in the interval between GET and POST. Likewise,
/// the hub compares `head` with its source ref so an unpushed branch or a concurrently moved
/// remote is rejected instead of opening a PR for different content.
fn source_claim(
    repo: &Repo,
    hub: &str,
    owner: &str,
    name: &str,
    branch: &str,
    live_agent_id: &str,
) -> crate::Result<SourceClaim> {
    let pinned = identity::require_current(repo, hub)?;
    let observed = RemoteIdentity::new(hub, live_agent_id)?;
    if observed != pinned {
        bail!(
            "{owner}/{name} now identifies agent {}, but this checkout is pinned to {}; refusing a reused source name",
            observed.agent_id,
            pinned.agent_id
        );
    }
    if !crate::rc::lineage::valid_branch_name(branch) {
        bail!("`{branch}` is not a usable source branch name");
    }
    let source_ref = format!("refs/heads/{branch}^{{commit}}");
    let head = repo
        .git(&["rev-parse", "--verify", &source_ref])
        .with_context(|| {
            format!(
                "local source branch `{branch}` does not resolve to a commit; commit it before opening a PR"
            )
        })?;
    if head.is_empty() {
        bail!("local source branch `{branch}` resolved to an empty commit id");
    }
    Ok(SourceClaim {
        expected_agent_id: pinned.agent_id,
        head,
    })
}

fn api_error_exit(error: &anyhow::Error) -> ExitCode {
    api_status_exit(
        error
            .downcast_ref::<crate::hub::client::ApiError>()
            .map(|api| api.status),
    )
}

fn api_status_exit(status: Option<u16>) -> ExitCode {
    match status {
        Some(401) => ExitCode::Auth,
        // 409 is an exact-head conflict; 412/428 are immutable-identity preconditions.
        Some(404 | 409 | 412 | 428) => ExitCode::Precondition,
        _ => ExitCode::Network,
    }
}

fn list(client: &Client, repo: &str) -> CmdResult {
    let Some((o, n)) = super::parse_slug(repo).ok() else {
        ui::error("expected the form owner/repo.");
        return Ok(ExitCode::Usage);
    };
    match client.pr_list(&o, &n) {
        Ok(prs) => {
            if prs.is_empty() {
                println!("no open PRs.");
            }
            for p in prs {
                println!(
                    "#{}  {} → {}  [{}]  {}",
                    p.id,
                    p.source,
                    p.target_branch,
                    p.state,
                    p.title.unwrap_or_default()
                );
            }
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            Ok(ExitCode::Network)
        }
    }
}

fn show(client: &Client, id: u64) -> CmdResult {
    match client.pr_get(id) {
        Ok(p) => {
            println!(
                "PR #{}  {} → {}  [{}]",
                p.id, p.source, p.target_branch, p.state
            );
            if let Some(t) = &p.title {
                println!("  {t}");
            }
            if let Some(s) = &p.summary {
                println!("  merge_summary: {s}");
            }
            println!("  review: agit show pr/{id} · agit diff <target>...pr/{id} --view");
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            Ok(ExitCode::Network)
        }
    }
}

fn fetch(client: &Client, id: u64) -> CmdResult {
    let p = match client.pr_get(id) {
        Ok(p) => p,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Network);
        }
    };
    // Fetch pr/<id> into the local target repo (the server publishes the PR head at
    // refs/pr/<id>).
    let cwd = std::env::current_dir().unwrap_or_default();
    let ctx = match super::context::resolve(&cwd) {
        Ok(c) => c,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Ref);
        }
    };
    let (o, n) = match ctx.owner_name() {
        Ok(v) => v,
        Err(_) => return Ok(ExitCode::Ref),
    };
    let dir = crate::infra::config::repo_dir(&o, &n)?;
    let Some(repo) = crate::domain::repo::Repo::open(&dir) else {
        ui::error(&format!("{}/{} doesn’t exist locally.", o, n));
        return Ok(ExitCode::Precondition);
    };
    let identity_client = crate::hub::Client::from_env();
    if let Err(e) = crate::hub::identity::verify_slug(&repo, &identity_client, &o, &n) {
        ui::error(&format!("refusing to fetch pr/{id}: {e:#}"));
        return Ok(ExitCode::Precondition);
    }
    let out = crate::hub::git::run(
        &repo,
        &[
            "fetch",
            "origin",
            &format!("+refs/pr/{id}:refs/agit-pr/{id}"),
        ],
    )?;
    if !out.ok() {
        ui::error(&format!("fetching pr/{id} failed: {}", out.stderr.trim()));
        return Ok(ExitCode::Network);
    }
    let _ = p;
    ui::success(&format!("fetched pr/{id} → local ref refs/agit-pr/{id}"));
    Ok(ExitCode::Ok)
}

fn merge(client: &Client, id: u64, adopt: Option<&str>) -> CmdResult {
    match client.pr_merge(id, adopt) {
        Ok(r) => {
            ui::success(&format!("PR #{id} landed ({})", r.mode));
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            ui::hint(
                "common rejection: the author’s branch moved while the PR was open (stale) → contributor pulls upstream, redoes the merge and reopens; or a real fork with no pre-run merge → read the fix guidance first",
            );
            Ok(ExitCode::Precondition)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUB: &str = "https://hub.test";
    const OLD_ID: &str = "00000000-0000-0000-0000-000000000001";
    const NEW_ID: &str = "00000000-0000-0000-0000-000000000002";

    fn commit(repo: &Repo, file: &str, body: &str, message: &str) -> String {
        std::fs::write(repo.root().join(file), body).unwrap();
        repo.add_all().unwrap();
        assert!(repo.commit(message).unwrap());
        repo.git(&["rev-parse", "HEAD"]).unwrap()
    }

    #[test]
    fn stale_or_legacy_source_slug_is_rejected_without_changing_the_checkout() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let head = commit(&repo, "one", "one\n", "one");

        let legacy = source_claim(&repo, HUB, "alice", "photo", "main", OLD_ID)
            .unwrap_err()
            .to_string();
        assert!(legacy.contains("legacy checkout"), "{legacy}");

        let pinned = RemoteIdentity::new(HUB, OLD_ID).unwrap();
        identity::pin(&repo, &pinned).unwrap();
        let stale = source_claim(&repo, HUB, "alice", "photo", "main", NEW_ID)
            .unwrap_err()
            .to_string();
        assert!(stale.contains("reused source name"), "{stale}");
        assert!(stale.contains(OLD_ID), "{stale}");
        assert!(stale.contains(NEW_ID), "{stale}");
        assert_eq!(identity::read(&repo).unwrap(), Some(pinned));
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head);
    }

    #[test]
    fn unpushed_or_moved_remote_never_substitutes_for_the_local_source_head() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let base = commit(&repo, "state", "base\n", "base");
        repo.git(&["checkout", "--quiet", "-b", "topic"]).unwrap();
        let local_topic = commit(&repo, "state", "local topic\n", "local topic");
        repo.git(&["update-ref", "refs/remotes/origin/topic", &base])
            .unwrap();
        identity::pin(&repo, &RemoteIdentity::new(HUB, OLD_ID).unwrap()).unwrap();

        let unpushed = source_claim(&repo, HUB, "alice", "photo", "topic", OLD_ID).unwrap();
        assert_eq!(unpushed.head, local_topic);

        repo.git(&["checkout", "--quiet", "main"]).unwrap();
        let moved_remote = commit(&repo, "remote", "moved\n", "remote moved");
        repo.git(&["update-ref", "refs/remotes/origin/topic", &moved_remote])
            .unwrap();
        let moved = source_claim(&repo, HUB, "alice", "photo", "topic", OLD_ID).unwrap();
        assert_eq!(moved.head, local_topic);
        assert_ne!(moved.head, moved_remote);

        assert_eq!(api_status_exit(Some(409)), ExitCode::Precondition);
    }
}
