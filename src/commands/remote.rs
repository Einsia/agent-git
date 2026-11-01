//! `agit remote` — the Hubs that hold a repository's publications, as this computer records them.
//!
//! **Zero network**: remotes come from the checkout's pinned identity and Git configuration, and
//! the account from the credentials saved for each remote's Hub. A checkout holds one pin, so it
//! lists at most `origin`, which is also its upstream (where `agit push` and automatic upload go).
//! The repository is the one named, else the session's (`AGIT_SESSION`), else the folder's.
//!
//! The address is shown only when the checkout's `origin` URL lies on the pinned Hub and holds
//! nothing the code-origin sanitizer removes (credentials, a query, a fragment); a URL on
//! another Hub is not where pushes go, and printing it as this remote's address would point a
//! reader at the wrong publication. It is the repository's address there, as people open it, not
//! the Git transport URL.

use super::CmdResult;
use crate::domain::repo::Repo;
use crate::hub::identity;
use crate::infra::{credentials, hub_authority};
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use serde::Serialize;

#[derive(ClapArgs)]
pub struct Args {
    /// Repository (`owner/repo`); defaults to the session's (AGIT_SESSION), else this folder's.
    #[arg(value_name = "owner/repo")]
    pub repo: Option<String>,
    /// Also show each remote's address, the account signed in on its Hub (or how to sign in), and
    /// the upstream.
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct Remote {
    name: &'static str,
    hub: String,
    /// The repository's address on that Hub, when this checkout records one on it.
    address: Option<String>,
    /// The account whose credentials are saved for that Hub.
    account: Option<String>,
    upstream: bool,
}

pub fn run(args: Args) -> CmdResult {
    let slug = match args.repo {
        Some(slug) => slug,
        None => match super::context::from_env()
            .map(|context| context.repo)
            .or_else(folder_repository)
        {
            Some(slug) => slug,
            None => {
                ui::error("this folder is not on, so it has no repository to list remotes for.");
                ui::hint("name one: `agit remote <owner/repo>`");
                return Ok(ExitCode::Ref);
            }
        },
    };
    let Ok((owner, name)) = super::parse_slug(&slug) else {
        ui::error("expected the form owner/repo.");
        return Ok(ExitCode::Usage);
    };
    let dir = crate::infra::config::repo_dir(&owner, &name)?;
    let Some(repo) = Repo::open(&dir) else {
        ui::error(&format!("{slug} doesn’t exist locally."));
        ui::hint(&format!("`agit clone {slug}`"));
        return Ok(ExitCode::Precondition);
    };
    super::migration::check_readonly_repo_startup_local(&repo)?;
    let remotes = remotes(&repo)?;
    if remotes.is_empty() && repo.remote_url().is_some() {
        ui::warning(&format!(
            "{slug} has an origin URL but no pinned Hub identity, so it has no remote agit can push to."
        ));
        ui::hint(&format!(
            "re-run `agit clone {slug}` into a fresh checkout to restore its identity"
        ));
    }
    if super::json::requested() {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "repository": slug,
                "remotes": remotes,
            }))?
        );
        return Ok(ExitCode::Ok);
    }
    for remote in &remotes {
        if args.verbose {
            print_verbose(remote);
        } else {
            println!("{}", remote.name);
        }
    }
    Ok(ExitCode::Ok)
}

/// The repository the current folder saves to, judged offline.
fn folder_repository() -> Option<String> {
    let root = super::front::folder(&std::env::current_dir().ok()?).ok()?;
    super::front::bound(&root)
}

fn remotes(repo: &Repo) -> crate::Result<Vec<Remote>> {
    let Some(pin) = identity::read(repo)? else {
        return Ok(Vec::new());
    };
    let prefix = format!("{}/", pin.hub);
    // An origin the code-origin sanitizer would change carries credentials, a query or a
    // fragment, and none of that may be printed.
    let address = repo
        .remote_url()
        .filter(|url| {
            crate::domain::meta::sanitize_git_origin(url).as_deref() == Some(url.as_str())
                && url.starts_with(&prefix)
                && url.len() > prefix.len()
        })
        .map(|url| url.strip_suffix(".git").map(str::to_owned).unwrap_or(url));
    let account = credentials::load(&pin.hub).map(|credential| credential.username);
    Ok(vec![Remote {
        name: "origin",
        hub: pin.hub,
        address,
        account,
        upstream: true,
    }])
}

fn print_verbose(remote: &Remote) {
    let address = remote.address.clone().unwrap_or_else(|| {
        format!(
            "{} (no repository address recorded)",
            hub_authority::safe_label(&remote.hub)
        )
    });
    let account = match &remote.account {
        Some(account) => format!("signed in as {account}"),
        None => format!("not signed in: agit login --hub {}", remote.hub),
    };
    let upstream = if remote.upstream { " · upstream" } else { "" };
    println!("{}\t{address}\t{account}{upstream}", remote.name);
}
