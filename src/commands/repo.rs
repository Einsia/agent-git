//! `agit repo` — the low-frequency repo governance actions, gathered under one noun command.
//!
//! Two permission levels: read can clone/pull, write can push; no permission is uniformly a 404,
//! so "does not exist" and "no permission" are deliberately indistinguishable. Changing
//! visibility and deleting both require typing the full name to confirm.

use super::CmdResult;
use crate::domain::repo::Repo;
use crate::hub::Client;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Create the repo on the hub together with its pinned local counterpart.
    Create {
        name: String,
        #[arg(long)]
        private: bool,
    },
    /// List repos — local (or --remote for the hub).
    List {
        #[arg(long)]
        remote: bool,
    },
    /// Repo details.
    Info { repo: Option<String> },
    /// Change visibility (full name required; private→public triggers the server-side secret scan).
    Visibility { repo: String, visibility: String },
    /// Manage collaborators.
    Collab {
        #[command(subcommand)]
        action: CollabAction,
    },
    /// Rename.
    Rename { repo: String, new_name: String },
    /// Delete. Deletes the remote by default (full name required); --local removes only the local copy.
    Delete {
        repo: String,
        #[arg(long)]
        local: bool,
    },
    /// Print the local directory: the main checkout, or `<owner/repo>@<branch>` for that session’s worktree.
    Path { repo: Option<String> },
}

#[derive(clap::Subcommand)]
pub enum CollabAction {
    Add {
        repo: String,
        user: String,
        #[arg(long, default_value = "read")]
        role: String,
    },
    Rm {
        repo: String,
        user: String,
    },
    List {
        repo: String,
    },
}

pub fn run(args: Args) -> CmdResult {
    match args.cmd {
        Cmd::Create { name, private } => create(&name, private),
        Cmd::List { remote } => list(remote),
        Cmd::Info { repo } => info(resolve_or_ctx(repo.as_deref())),
        Cmd::Visibility { repo, visibility } => set_visibility(&repo, &visibility),
        Cmd::Collab { action } => collab(action),
        Cmd::Rename { repo, new_name } => rename(&repo, &new_name),
        Cmd::Delete { repo, local } => delete(&repo, local),
        Cmd::Path { repo } => path(resolve_or_ctx(repo.as_deref())),
    }
}

/// With the argument omitted, the repo slug comes from context resolution.
fn resolve_or_ctx(arg: Option<&str>) -> Option<String> {
    match arg {
        Some(s) => Some(s.to_string()),
        None => {
            let cwd = std::env::current_dir().ok()?;
            super::context::resolve(&cwd).ok().map(|c| c.repo)
        }
    }
}

fn create(name: &str, private: bool) -> CmdResult {
    let client = match super::require_login() {
        Ok(c) => c,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Auth);
        }
    };
    // The local check comes before the hub mutation: with a same-name repo
    // already on disk, failing after publish would occupy the remote name
    // and leave the user with both halves broken. The post-publish check in
    // materialize_at stays — this one cannot see a race.
    if let Some(me) = crate::infra::credentials::current_user()
        && let Ok(dir) = crate::infra::config::repo_dir(&me, name)
        && Repo::open(dir.clone()).is_some()
    {
        ui::error(&format!(
            "a local repo already sits at {} — keep it if it is yours, or move it aside first",
            dir.display()
        ));
        ui::hint("nothing was created on the hub");
        return Ok(ExitCode::Precondition);
    }
    match client.publish(&crate::hub::PublishRequest {
        name: name.to_string(),
        owner: None,
        public: !private,
        repo_origins: vec![],
    }) {
        Ok(resp) => {
            ui::success(&format!(
                "created {}/{} ({})",
                resp.owner,
                resp.name,
                if private { "private" } else { "public" }
            ));
            // A hub row without a pinned local repo is a trap: the natural
            // `agit init <name>` next builds an unpinned repo of the same
            // name, and push must then refuse to adopt the remote silently.
            // What "create" promises is the pair — the remote and its pinned
            // local counterpart.
            let dir = match crate::infra::config::repo_dir(&resp.owner, &resp.name) {
                Ok(d) => d,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    return Ok(ExitCode::Precondition);
                }
            };
            if let Err(e) = materialize_at(&dir, client.base(), &resp.agent_id, &resp.push_url) {
                ui::error(&format!("{e:#}"));
                ui::hint(&format!(
                    "the hub repo exists; finish locally with `agit clone {}/{}`",
                    resp.owner, resp.name
                ));
                return Ok(ExitCode::Precondition);
            }
            println!("  next:");
            println!(
                "    cd <your-project> && agit init {}    # lays down the main file line",
                resp.name
            );
            println!("    agit push {}/{} -b main", resp.owner, resp.name);
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            Ok(ExitCode::Network)
        }
    }
}

/// The pinned local counterpart of a freshly published repo: an empty repo
/// carrying the immutable remote identity and the push URL, exactly what a
/// clone of the empty remote would leave behind.
fn materialize_at(
    dir: &std::path::Path,
    hub: &str,
    agent_id: &str,
    push_url: &str,
) -> crate::Result<()> {
    use anyhow::Context;
    if Repo::open(dir.to_path_buf()).is_some() {
        anyhow::bail!(
            "a local repo already sits at {} — keep it if it is yours, or move it aside and `agit clone` the new remote",
            dir.display()
        );
    }
    let repo = Repo::init(dir).context("cannot lay down the local repo")?;
    let identity = crate::hub::identity::RemoteIdentity::new(hub, agent_id)?;
    // Pin before URL: a failure part-way leaves at most "pinned, no origin
    // yet", which is safe to retry; the other order leaves a repo that looks
    // usable but has no fencing identity.
    crate::hub::identity::pin(&repo, &identity)?;
    repo.set_remote(push_url)?;
    Ok(())
}

fn list(remote: bool) -> CmdResult {
    if remote {
        let client = match super::require_login() {
            Ok(c) => c,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Auth);
            }
        };
        match client.list_agents() {
            Ok(agents) => {
                if agents.is_empty() {
                    println!("no repos visible to you on the hub.");
                }
                for a in agents {
                    let vis = if a.is_public() { "public " } else { "private" };
                    println!(
                        "{}  [{}]  {} sessions  {}",
                        a.slug(),
                        vis,
                        a.session_count,
                        a.updated_at.as_deref().unwrap_or("")
                    );
                }
            }
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Network);
            }
        }
        return Ok(ExitCode::Ok);
    }
    let root = crate::infra::config::repos_dir()?;
    println!("{}", ui::dim(&format!("  {}", ui::tilde(&root))));
    let mut n = 0;
    if let Ok(owners) = std::fs::read_dir(&root) {
        for o in owners.flatten() {
            let Ok(repos) = std::fs::read_dir(o.path()) else {
                continue;
            };
            for r in repos.flatten() {
                if !r.path().join(".git").exists() {
                    continue;
                }
                n += 1;
                println!(
                    "{}/{}",
                    o.file_name().to_string_lossy(),
                    r.file_name().to_string_lossy()
                );
            }
        }
    }
    if n == 0 {
        println!("no local repos. `agit init <name>` or `agit clone <owner/repo>`.");
    }
    Ok(ExitCode::Ok)
}

fn info(repo: Option<String>) -> CmdResult {
    let Some(slug) = repo else {
        ui::error("can’t resolve which repo to show.");
        ui::hint("`agit repo info <owner/repo>`");
        return Ok(ExitCode::Ref);
    };
    let Some((owner, name)) = super::parse_slug(&slug).ok() else {
        ui::error("expected the form owner/repo.");
        return Ok(ExitCode::Usage);
    };
    let dir = crate::infra::config::repo_dir(&owner, &name)?;
    if let Some(r) = Repo::open(&dir) {
        println!("local     {}", ui::tilde(r.root()));
        println!("branches  {}", r.branches().len());
        if let Some(o) = r.remote_url() {
            println!("origin    {o}");
        }
        if let Some(u) = r.upstream_url() {
            println!("upstream  {u}");
        }
    } else {
        println!("local     (missing)");
    }
    // Remote info: no sign-in required (a public repo needs none).
    let client = Client::from_env();
    match client.get_agent(&owner, &name) {
        Ok(a) => {
            println!("remote    {} [{}]", a.slug(), a.visibility);
            println!("sessions  {}", a.session_count);
            println!("web       {}", config_hub_web(&a));
        }
        Err(e) => {
            ui::warning(&format!("remote info unavailable: {e:#}"));
        }
    }
    Ok(ExitCode::Ok)
}

fn config_hub_web(a: &crate::hub::RemoteAgent) -> String {
    format!(
        "{}/@{}",
        crate::infra::config::hub_url().trim_end_matches('/'),
        a.slug()
    )
}

/// A remote governance write trusts only the local pin, never a live GET on the current slug.
///
/// A live GET takes the new object's id as the expected value once the old agent is deleted and
/// one of the same name is created — which lets the fencing header endorse the very request it
/// protects. With no local checkout, clone first: that both fetches the content and explicitly
/// establishes this immutable root of trust.
fn mutation_identity(
    client: &Client,
    owner: &str,
    name: &str,
) -> crate::Result<crate::hub::identity::RemoteIdentity> {
    let dir = crate::infra::config::repo_dir(owner, name)?;
    let repo = Repo::open(&dir).ok_or_else(|| {
        anyhow::anyhow!(
            "no identity-pinned local checkout for {owner}/{name}; run `agit clone {owner}/{name}` first"
        )
    })?;
    crate::hub::identity::require_current(&repo, client.base())
}

/// Delete the local copy: take down each of its linked worktrees first (they live outside
/// `repos/`), then remove the main checkout.
///
/// Removing the directory alone leaves worktrees pointing at nothing, and they go on to block
/// the slot a new worktree needs when the same name is cloned again.
fn remove_local_checkout(dir: &std::path::Path) -> crate::Result<()> {
    if let Some(primary) = crate::domain::repo::Repo::open(dir) {
        super::worktree::remove_all(&primary)?;
    }
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

fn set_visibility(repo: &str, v: &str) -> CmdResult {
    let Some((owner, name)) = super::parse_slug(repo).ok() else {
        ui::error("expected the form owner/repo.");
        return Ok(ExitCode::Usage);
    };
    let public = match v {
        "public" => true,
        "private" => false,
        _ => {
            ui::error(&format!("visibility is only public / private — got `{v}`."));
            return Ok(ExitCode::Usage);
        }
    };
    let client = match super::require_login() {
        Ok(c) => c,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Auth);
        }
    };
    // The identity is taken **before** the split: in either direction, the object being opened
    // up or locked down is the one the local checkout points at, while `owner/name` can be
    // deleted and rebuilt under the same name. The public direction needs this more — a
    // server-side scan sits in the middle, so its window is longer than the private one's.
    let identity = match mutation_identity(&client, &owner, &name) {
        Ok(identity) => identity,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Precondition);
        }
    };
    if public {
        return set_public_visibility(&client, &owner, &name, &identity.agent_id);
    }

    // Public-to-private remains a single server mutation, but still requires
    // the same exact repository-name confirmation locally.
    match ui::prompt::input(
        &format!("change {owner}/{name} to {v} — type `{owner}/{name}` in full to confirm"),
        None,
    ) {
        Ok(Some(typed)) if typed == format!("{owner}/{name}") => {}
        Err(_) | Ok(None) if !ui::is_tty() => {
            ui::error(
                "changing visibility needs interactive confirmation (typing the full name); refused without a TTY.",
            );
            ui::hint(
                "run it on a real terminal — this is deliberate: one direction (public) can’t be taken back",
            );
            return Ok(ExitCode::Interactive);
        }
        _ => {
            println!("cancelled.");
            return Ok(ExitCode::Ok);
        }
    }
    // Reaching here means going private: public branched off above into
    // [`set_public_visibility`], whose path first passes the server-side pre-publication scan
    // and consumes one confirmation intent.
    match client.set_visibility(&owner, &name, false, &identity.agent_id) {
        Ok(()) => {
            ui::success(&format!("{owner}/{name} is now {v}"));
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            Ok(ExitCode::Network)
        }
    }
}

/// `expected_agent_id` is carried the whole way: the scan and the publication that follows must
/// land on the **same** immutable identity. A server-side scan and a human confirmation sit in
/// between, and `owner/name` can be deleted and rebuilt under the same name in that window; a
/// single GET precheck at the start does not stop it — the test has to travel with every request
/// into the server's own lock.
fn set_public_visibility(
    client: &crate::hub::Client,
    owner: &str,
    name: &str,
    expected_agent_id: &str,
) -> CmdResult {
    let prepared = match client.prepare_public_visibility(owner, name, expected_agent_id) {
        Ok(prepared) => prepared,
        Err(error) => {
            ui::error(&format!("{error:#}"));
            return Ok(ExitCode::Network);
        }
    };
    if !prepared.findings.complete {
        ui::error("the server did not complete the publication scan; visibility was not changed");
        return Ok(ExitCode::Policy);
    }

    ui::warning(&prepared.warning);
    if prepared.findings.suspected_secrets == 0 {
        println!("  server scan: complete, no suspected secrets reported");
    } else {
        let qualifier = if prepared.findings.truncated {
            "at least "
        } else {
            ""
        };
        ui::warning(&format!(
            "server scan reported {qualifier}{} suspected secret occurrence(s)",
            prepared.findings.suspected_secrets
        ));
        for rule in &prepared.findings.rules {
            println!("  {}: {}", rule.id, rule.count);
        }
        ui::hint(
            "findings are warnings for this owner-confirmed transition; incomplete scans still cannot be overridden",
        );
    }
    println!("  confirmation expires at {}", prepared.expires_at);

    match ui::prompt::input(
        &format!(
            "make {owner}/{name} public — type `{}` in full to confirm",
            prepared.confirmation_phrase
        ),
        None,
    ) {
        Ok(Some(typed)) if typed == prepared.confirmation_phrase => {}
        Err(_) | Ok(None) if !ui::is_tty() => {
            ui::error(
                "making a repository public needs interactive confirmation; refused without a TTY.",
            );
            return Ok(ExitCode::Interactive);
        }
        _ => {
            println!("cancelled.");
            return Ok(ExitCode::Ok);
        }
    }

    let accept_secret_findings = if prepared.findings.suspected_secrets > 0 {
        match ui::prompt::confirm(
            "I reviewed the secret findings and still want to publish this repository",
            false,
        )? {
            Some(true) => true,
            Some(false) => {
                println!("cancelled.");
                return Ok(ExitCode::Ok);
            }
            None => {
                ui::error("a separate interactive acceptance is required for secret findings");
                return Ok(ExitCode::Interactive);
            }
        }
    } else {
        false
    };

    match client.confirm_public_visibility(
        owner,
        name,
        &prepared.intent_id,
        &prepared.confirmation_phrase,
        accept_secret_findings,
        expected_agent_id,
    ) {
        Ok(_) => {
            ui::success(&format!("{owner}/{name} is now public"));
            Ok(ExitCode::Ok)
        }
        Err(error) => {
            ui::error(&format!("{error:#}"));
            Ok(ExitCode::Network)
        }
    }
}

fn collab(action: CollabAction) -> CmdResult {
    let client = match super::require_login() {
        Ok(c) => c,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Auth);
        }
    };
    match action {
        CollabAction::Add { repo, user, role } => {
            let Some((o, n)) = super::parse_slug(&repo).ok() else {
                return Ok(ExitCode::Usage);
            };
            if !matches!(role.as_str(), "read" | "write") {
                ui::error("--role is read (clone/pull) or write (push), nothing else.");
                return Ok(ExitCode::Usage);
            }
            let identity = match mutation_identity(&client, &o, &n) {
                Ok(identity) => identity,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    return Ok(ExitCode::Precondition);
                }
            };
            match client.add_collaborator(&o, &n, &user, &role, &identity.agent_id) {
                Ok(()) => {
                    ui::success(&format!("{user} is now a {role} collaborator on {repo}"));
                    Ok(ExitCode::Ok)
                }
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    Ok(ExitCode::Network)
                }
            }
        }
        CollabAction::Rm { repo, user } => {
            let Some((o, n)) = super::parse_slug(&repo).ok() else {
                return Ok(ExitCode::Usage);
            };
            let identity = match mutation_identity(&client, &o, &n) {
                Ok(identity) => identity,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    return Ok(ExitCode::Precondition);
                }
            };
            match client.remove_collaborator(&o, &n, &user, &identity.agent_id) {
                Ok(()) => {
                    ui::success(&format!("removed {user}"));
                    Ok(ExitCode::Ok)
                }
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    Ok(ExitCode::Network)
                }
            }
        }
        CollabAction::List { repo } => {
            let Some((o, n)) = super::parse_slug(&repo).ok() else {
                return Ok(ExitCode::Usage);
            };
            match client.list_collaborators(&o, &n) {
                Ok(cs) => {
                    if cs.is_empty() {
                        println!("no collaborators.");
                    }
                    for (u, r) in cs {
                        println!("{u:<24} {r}");
                    }
                    Ok(ExitCode::Ok)
                }
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    Ok(ExitCode::Network)
                }
            }
        }
    }
}

fn rename(repo: &str, new_name: &str) -> CmdResult {
    let Some((owner, name)) = super::parse_slug(repo).ok() else {
        ui::error("expected the form owner/repo.");
        return Ok(ExitCode::Usage);
    };
    if let Err(e) = crate::domain::repo::valid_name(new_name) {
        ui::error(&format!("{e:#}"));
        return Ok(ExitCode::Usage);
    }
    let old_dir = crate::infra::config::repo_dir(&owner, &name)?;
    let new_dir = crate::infra::config::repo_dir(&owner, new_name)?;
    if new_dir.exists() {
        ui::error(&format!(
            "the local target already exists: {}. Deal with it first.",
            new_dir.display()
        ));
        return Ok(ExitCode::Precondition);
    }
    let client = match super::require_login() {
        Ok(c) => c,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Auth);
        }
    };
    let identity = match mutation_identity(&client, &owner, &name) {
        Ok(identity) => identity,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Precondition);
        }
    };
    match client.rename_agent(&owner, &name, new_name, &identity.agent_id) {
        Ok(()) => ui::success(&format!("remote renamed: {repo} → {owner}/{new_name}")),
        Err(e) => {
            ui::error(&format!("remote rename failed: {e:#}"));
            return Ok(ExitCode::Network);
        }
    }
    // The local directory follows.
    if Repo::open(&old_dir).is_some() {
        std::fs::rename(&old_dir, &new_dir)?;
        if let Some(r) = Repo::open(&new_dir)
            && let Some(url) = r.remote_url()
        {
            let new_url = url.replace(&format!("/{name}.git"), &format!("/{new_name}.git"));
            if new_url != url {
                let _ = r.set_remote(&new_url);
            }
        }
        ui::success(&format!(
            "local directory moved: {} → {}",
            ui::tilde(&old_dir),
            ui::tilde(&new_dir)
        ));
    }
    Ok(ExitCode::Ok)
}

fn delete(repo: &str, local_only: bool) -> CmdResult {
    let Some((owner, name)) = super::parse_slug(repo).ok() else {
        ui::error("expected the form owner/repo.");
        return Ok(ExitCode::Usage);
    };
    // Type the full name to confirm (both modes require it).
    match ui::prompt::input(
        &format!("delete {owner}/{name} — type `{owner}/{name}` in full to confirm"),
        None,
    ) {
        Ok(Some(typed)) if typed == format!("{owner}/{name}") => {}
        Ok(None) => {
            ui::error(
                "deletion needs interactive confirmation (typing the full name); refused without a TTY.",
            );
            return Ok(ExitCode::Interactive);
        }
        _ => {
            println!("cancelled.");
            return Ok(ExitCode::Ok);
        }
    }

    if local_only {
        let dir = crate::infra::config::repo_dir(&owner, &name)?;
        if !dir.exists() {
            println!("the local copy doesn’t exist anyway.");
            return Ok(ExitCode::Ok);
        }
        remove_local_checkout(&dir)?;
        ui::success(&format!(
            "local copy deleted ({owner}/{name} on the hub is untouched)"
        ));
        return Ok(ExitCode::Ok);
    }

    let client = match super::require_login() {
        Ok(c) => c,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Auth);
        }
    };
    let identity = match mutation_identity(&client, &owner, &name) {
        Ok(identity) => identity,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Precondition);
        }
    };
    match client.delete_agent(&owner, &name, &identity.agent_id) {
        Ok(()) => {
            ui::success(&format!("remote {owner}/{name} deleted"));
            let dir = crate::infra::config::repo_dir(&owner, &name)?;
            if dir.exists() {
                match ui::prompt::confirm("delete the local copy too?", false) {
                    Ok(Some(true)) => {
                        remove_local_checkout(&dir)?;
                        ui::success("local copy deleted");
                    }
                    _ => println!("local copy kept."),
                }
            }
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            Ok(ExitCode::Network)
        }
    }
}

fn path(repo: Option<String>) -> CmdResult {
    let Some(raw) = repo else {
        ui::error("can’t resolve the repo.");
        ui::hint("use `agit repo path <owner/repo>`, or pin one with `agit switch` first");
        return Ok(ExitCode::Ref);
    };
    // `<owner/repo>@<branch>` names that session branch's worktree; a bare `@` is the current
    // session.
    let (slug, branch) = if raw == "@" {
        match super::context::from_env() {
            Some(ctx) => (ctx.repo, Some(ctx.branch)),
            None => {
                ui::error("`@` requires the session environment (AGIT_SESSION).");
                return Ok(ExitCode::Ref);
            }
        }
    } else {
        match raw.split_once('@') {
            Some((slug, branch)) if !branch.is_empty() => {
                (slug.to_string(), Some(branch.to_string()))
            }
            _ => (raw, None),
        }
    };
    let Some((owner, name)) = super::parse_slug(&slug).ok() else {
        ui::error("expected the form owner/repo.");
        return Ok(ExitCode::Usage);
    };
    let dir = crate::infra::config::repo_dir(&owner, &name)?;
    if !dir.join(".git").exists() {
        ui::error(&format!("{slug} doesn’t exist locally."));
        ui::hint(&format!("`agit clone {slug}`"));
        return Ok(ExitCode::Precondition);
    }
    let dir = match branch {
        None => dir,
        Some(branch) => {
            let primary = crate::domain::repo::Repo::at(&dir);
            if !primary.has_ref(&format!("refs/heads/{branch}")) {
                ui::error(&format!("{slug} has no branch `{branch}`."));
                return Ok(ExitCode::Ref);
            }
            super::worktree::checkout(&primary, &branch)?
                .root()
                .to_path_buf()
        }
    };
    // Print the path and nothing else (this feeds `cd $(agit repo path ...)`).
    println!("{}", dir.display());
    Ok(ExitCode::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialize_leaves_a_pinned_repo_with_an_origin() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("owner").join("name");
        let hub = "https://hub.example.test";
        let agent_id = "01a05c78-4273-7110-9d90-6cc202250000";
        let url = "https://hub.example.test/owner/name.git";
        materialize_at(&root, hub, agent_id, url).unwrap();

        let repo = Repo::open(root.clone()).expect("the repo must exist");
        let pin = repo
            .git_opt(&["config", "agit.remoteidentity"])
            .expect("the pin must exist");
        assert!(pin.contains(agent_id), "{pin}");
        assert_eq!(repo.remote_url().as_deref(), Some(url));

        let again = materialize_at(&root, hub, agent_id, url).unwrap_err();
        assert!(
            again.to_string().contains("already sits"),
            "an existing repo must never be overwritten: {again}"
        );
    }
}
