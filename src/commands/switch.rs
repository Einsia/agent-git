//! `agit switch` — pin the current directory to a branch.
//!
//! A pure context operation (PRD): it materializes no runtime, touches no code and moves no
//! repo checkout — to launch an agent, use `agit resume`. A pin is **per-directory** (every
//! terminal in that directory sees the same pin), lives in `~/.agit/workspaces/`, and writes not
//! one byte into the code repo.
//! Inside a session `AGIT_SESSION` always wins over the pin, so parallel sessions are unaffected.

use super::CmdResult;
use crate::domain::{repo::Repo, workspace};
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// The branch to pin.
    pub branch: Option<String>,
    /// Unpin.
    #[arg(long, conflicts_with = "branch")]
    pub unbind: bool,
}

pub fn run(args: Args) -> CmdResult {
    let cwd = std::env::current_dir()?;

    if args.unbind {
        match workspace::pin(&cwd, None) {
            Ok(()) => {
                ui::success(
                    "unpinned — zero-arg commands in this directory no longer point anywhere",
                );
                return Ok(ExitCode::Ok);
            }
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Precondition);
            }
        }
    }

    let Some(branch) = args.branch else {
        ui::error("missing the branch name (did you mean `agit switch --unbind`?).");
        return Ok(ExitCode::Usage);
    };

    // Verify the branch really exists in the repo this directory is bound to before pinning —
    // pinning a name that does not exist makes every later zero-arg command fail inexplicably.
    let Some((repo_slug, _)) = workspace::pinned(&cwd)
        .map(|(r, b)| (r, Some(b)))
        .or_else(|| workspace::read(&cwd).map(|w| (w.repo, None)))
    else {
        ui::error("this directory is bound to no agent repo.");
        ui::hint("run `agit init <name>` or `agit clone <owner/repo>` first");
        return Ok(ExitCode::Precondition);
    };
    let (owner, name) = super::parse_slug(&repo_slug)?;
    let repo_dir = crate::infra::config::repo_dir(&owner, &name)?;
    let Some(repo) = Repo::open(&repo_dir) else {
        ui::error(&format!(
            "the bound repo {} doesn’t exist locally ({}).",
            repo_slug,
            repo_dir.display()
        ));
        ui::hint(&format!("fetch it again: `agit clone {repo_slug}`"));
        return Ok(ExitCode::Precondition);
    };
    if !repo.has_ref(&format!("refs/heads/{branch}")) {
        ui::error(&format!("{repo_slug} has no branch `{branch}`."));
        ui::hint("`agit branch --all` lists what exists");
        return Ok(ExitCode::Ref);
    }

    workspace::pin(&cwd, Some(&branch))?;
    ui::success(&format!("pinned {branch} of {repo_slug}"));
    println!(
        "{}",
        ui::dim(&format!(
            "  zero-arg commands here now point at {branch}; inside an agent session, AGIT_SESSION wins"
        ))
    );
    Ok(ExitCode::Ok)
}
