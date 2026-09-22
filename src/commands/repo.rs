//! `agit repo` — the low-frequency repo governance actions, gathered under one noun command.
//!
//! Two permission levels: read can clone/pull, write can push; no permission is uniformly a 404,
//! so "does not exist" and "no permission" are deliberately indistinguishable. Changing
//! visibility and deleting both require typing the full name to confirm.

use super::CmdResult;
use crate::domain::meta;
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
    /// Print an invite link (owners only); `@branch` lands the invitee on that session.
    ///
    /// Whoever opens the link and signs in joins the repository with the chosen role. The link
    /// does not expire and can be used more than once; revoke it in the repository's settings
    /// (Invite by link). With `@branch` or `-b`, the branch's session must already be pushed.
    Invite {
        /// `owner/repo`, `owner/repo@branch` for that session, or `@` for AGIT_SESSION's session.
        #[arg(value_name = "owner/repo[@branch]")]
        repo: String,
        /// Land the invitee on this branch's session; the same as writing `owner/repo@branch`.
        #[arg(short = 'b', long, value_name = "branch")]
        branch: Option<String>,
        /// The role an accepted invitation grants.
        #[arg(long, value_enum, value_name = "role", default_value_t = InviteRole::Read)]
        role: InviteRole,
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

/// The role an invitation link grants; the hub accepts exactly these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum InviteRole {
    /// Clone and pull.
    Read,
    /// Clone, pull and push.
    Write,
    /// Full control, including settings and invite links.
    Owner,
}

impl InviteRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Owner => "owner",
        }
    }

    fn grants(self) -> &'static str {
        match self {
            Self::Read => "clone and pull",
            Self::Write => "clone, pull and push",
            Self::Owner => "full control, including settings and invite links",
        }
    }
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
        Cmd::Invite { repo, branch, role } => invite(&repo, branch.as_deref(), role),
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
    let client = super::require_login()?;
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
    let auto_push = super::config::choose_repo_auto_push()?;
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
            if let Some(value) = auto_push {
                Repo::at(&dir).set_auto_push(Some(value))?;
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
            super::fix::register_terminal_api_error(&e);
            ui::error(&format!("{e:#}"));
            Ok(super::terminal_error_code(&e, ExitCode::Network))
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
        let client = super::require_login()?;
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
                super::fix::register_terminal_api_error(&e);
                ui::error(&format!("{e:#}"));
                return Ok(super::terminal_error_code(&e, ExitCode::Network));
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
    let client = super::require_login()?;
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
            super::fix::register_terminal_api_error(&e);
            ui::error(&format!("{e:#}"));
            Ok(super::terminal_error_code(&e, ExitCode::Network))
        }
    }
}

fn finding_display(value: &str) -> String {
    value
        .chars()
        .take(512)
        .map(|character| {
            if character.is_control()
                || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                ' '
            } else {
                character
            }
        })
        .collect()
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
            super::fix::register_terminal_api_error(&error);
            ui::error(&format!("{error:#}"));
            return Ok(super::terminal_error_code(&error, ExitCode::Network));
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
        for finding in prepared.finding_locations.iter().take(10) {
            println!(
                "  {} line {} [{}] {}",
                finding_display(&finding.file),
                finding.line,
                finding_display(&finding.rule),
                finding_display(&finding.redacted)
            );
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
            super::fix::register_terminal_api_error(&error);
            ui::error(&format!("{error:#}"));
            Ok(super::terminal_error_code(&error, ExitCode::Network))
        }
    }
}

fn collab(action: CollabAction) -> CmdResult {
    let client = super::require_login()?;
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
                    super::fix::register_terminal_api_error(&e);
                    ui::error(&format!("{e:#}"));
                    Ok(super::terminal_error_code(&e, ExitCode::Network))
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
                    super::fix::register_terminal_api_error(&e);
                    ui::error(&format!("{e:#}"));
                    Ok(super::terminal_error_code(&e, ExitCode::Network))
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
                    super::fix::register_terminal_api_error(&e);
                    ui::error(&format!("{e:#}"));
                    Ok(super::terminal_error_code(&e, ExitCode::Network))
                }
            }
        }
    }
}

/// What an invite names: a repository, or one of its session branches as the landing page.
#[derive(Debug, PartialEq, Eq)]
struct InviteTarget {
    owner: String,
    name: String,
    branch: Option<String>,
}

/// `owner/repo` or `owner/repo@branch`. A bare `@` is AGIT_SESSION's and is expanded by the
/// caller; an empty branch is refused rather than read as the repository, because the two print
/// links to different pages.
fn parse_invite_target(raw: &str) -> crate::Result<InviteTarget> {
    let raw = raw.trim();
    let (slug, branch) = match raw.split_once('@') {
        Some((_, "")) => anyhow::bail!(
            "`{raw}` names no branch after `@`; drop the `@` to invite to the whole repository"
        ),
        Some((slug, branch)) => (slug, Some(branch.to_owned())),
        None => (raw, None),
    };
    let (owner, name) = super::parse_slug(slug)?;
    super::canonical_owner(&owner)?;
    Ok(InviteTarget {
        owner,
        name,
        branch,
    })
}

/// JavaScript's `encodeURIComponent`. The web app reads `next` back with `URLSearchParams`, and a
/// link for the same session must be byte-identical whichever side builds it; RFC 3986's
/// unreserved set would also escape `!*'()`.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The session page as a path on the hub: the value an invite link carries in `next`.
fn session_path(owner: &str, name: &str, session_id: &str, branch: &str) -> String {
    format!(
        "/@{owner}/{name}/s/{}?ref={}",
        encode_uri_component(session_id),
        encode_uri_component(branch)
    )
}

/// The token and the landing path ride in the fragment, which browsers never send to the server.
/// The creator's account rides in the query as `share=true&sharer=<account>`, the attribution the
/// hub counts share clicks and share-driven signups by; it names no secret.
fn invite_link(hub: &str, token: &str, next: Option<&str>, sharer: Option<&str>) -> String {
    let mut link = format!("{}/invite", hub.trim_end_matches('/'));
    if let Some(sharer) = sharer {
        link.push_str("?share=true&sharer=");
        link.push_str(&encode_uri_component(sharer));
    }
    link.push_str("#token=");
    link.push_str(token);
    if let Some(next) = next {
        link.push_str("&next=");
        link.push_str(&encode_uri_component(next));
    }
    link
}

/// The web app refuses any other shape, so a link built from it could never be accepted.
fn is_invitation_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Why a branch cannot be the landing page of an invite link.
#[derive(Debug, PartialEq, Eq)]
enum Unlinkable {
    BadName,
    /// Neither a local branch nor a remote-tracking one.
    Missing,
    /// A local branch this checkout has never seen on the hub.
    Unpublished,
    /// The file line carries no session.
    FileLine,
    /// The published head has not settled a turn, so the hub has no session page yet.
    Unsettled,
}

/// The session id the hub serves for `branch`, read at the head this checkout last exchanged
/// with origin.
///
/// The local head may be ahead of it; that is harmless because a branch's session identity is
/// claimed by its first turn and never changes. What must not happen is naming a session from a
/// local-only head: the hub has no page for it, and the invitee lands on a 404.
fn published_session(repo: &Repo, branch: &str) -> crate::Result<Result<String, Unlinkable>> {
    let (status, _, _) =
        repo.git_status_local(&["check-ref-format", &format!("refs/heads/{branch}")])?;
    if status != Some(0) {
        return Ok(Err(Unlinkable::BadName));
    }
    let Some(head) = repo.git_opt(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/remotes/origin/{branch}^{{commit}}"),
    ]) else {
        return Ok(Err(if repo.has_ref(&format!("refs/heads/{branch}")) {
            Unlinkable::Unpublished
        } else {
            Unlinkable::Missing
        }));
    };
    let Some(snapshot) = meta::read_at_ref_result(repo, head.trim())? else {
        return Ok(Err(Unlinkable::Unsettled));
    };
    if snapshot.is_file_line() {
        return Ok(Err(Unlinkable::FileLine));
    }
    if !meta::is_bare_id(&snapshot.session) {
        return Ok(Err(Unlinkable::Unsettled));
    }
    Ok(Ok(snapshot.session))
}

/// Resolve the landing session before anything is minted: a link, once printed, cannot be
/// retargeted, and a refused lookup must not leave an unused invitation behind.
fn landing_session(
    owner: &str,
    name: &str,
    branch: &str,
    hub: &str,
) -> crate::Result<Result<String, ExitCode>> {
    let slug = format!("{owner}/{name}");
    let dir = crate::infra::config::repo_dir(owner, name)?;
    let repo = Repo::open(&dir).ok_or_else(|| anyhow::anyhow!("{slug} has no local checkout"))?;
    if super::show::pinned_origin(&repo)
        != Some((hub.to_owned(), owner.to_owned(), name.to_owned()))
    {
        ui::error(&format!(
            "the origin of {} is not {hub}/{slug}, so its branches cannot name a session there",
            ui::tilde(&dir)
        ));
        ui::hint(&format!(
            "invite to the repository with `agit repo invite {slug}`, or re-clone it with `agit clone {slug}`"
        ));
        return Ok(Err(ExitCode::Precondition));
    }
    let refusal = match published_session(&repo, branch)? {
        Ok(session) => return Ok(Ok(session)),
        Err(refusal) => refusal,
    };
    Ok(Err(match refusal {
        Unlinkable::BadName => {
            ui::error(&format!("`{branch}` is not a valid branch name."));
            ExitCode::Usage
        }
        Unlinkable::Missing => {
            ui::error(&format!("{slug} has no branch `{branch}`."));
            ui::hint(&format!("`agit branch --repo {slug}` lists its branches"));
            ExitCode::Ref
        }
        Unlinkable::Unpublished => {
            ui::error(&format!(
                "{slug}@{branch} is not on the hub as far as this checkout knows, so there is no session page to land on."
            ));
            ui::hint(&format!(
                "publish it with `agit push {slug}@{branch}` (or `agit fetch {slug}` if it was pushed from elsewhere), then retry"
            ));
            ExitCode::Precondition
        }
        Unlinkable::FileLine => {
            ui::error(&format!(
                "`{branch}` is the file line of {slug}; it carries no session."
            ));
            ui::hint(&format!(
                "invite to the repository instead: `agit repo invite {slug}`"
            ));
            ExitCode::Precondition
        }
        Unlinkable::Unsettled => {
            ui::error(&format!(
                "the published head of {slug}@{branch} has no settled session yet."
            ));
            ui::hint(&format!(
                "settle and publish a turn first: `agit commit {slug}@{branch}`, then `agit push {slug}@{branch}`"
            ));
            ExitCode::Precondition
        }
    }))
}

/// An invite link is a governance write like adding a collaborator: it is fenced by the local
/// pin, never by a live lookup of the name.
fn invite(raw: &str, branch: Option<&str>, role: InviteRole) -> CmdResult {
    let raw = match (branch, raw.trim().contains('@')) {
        (Some(_), true) => {
            ui::error(&format!(
                "`{}` already names a branch; give it either after `@` or with -b, not both",
                raw.trim()
            ));
            return Ok(ExitCode::Usage);
        }
        (Some(branch), false) => format!("{}@{branch}", raw.trim()),
        (None, _) => raw.to_owned(),
    };
    let raw = raw.as_str();
    let target = if raw.trim() == "@" {
        match super::context::at_context() {
            Ok(context) => parse_invite_target(&format!("{}@{}", context.repo, context.branch)),
            Err(error) => {
                ui::error(&format!("{error:#}"));
                return Ok(ExitCode::Ref);
            }
        }
    } else {
        parse_invite_target(raw)
    };
    let target = match target {
        Ok(target) => target,
        Err(error) => {
            ui::error(&format!("{error:#}"));
            return Ok(ExitCode::Usage);
        }
    };
    let (owner, name) = (target.owner.as_str(), target.name.as_str());
    let slug = format!("{owner}/{name}");
    let client = super::require_login()?;
    let identity = match mutation_identity(&client, owner, name) {
        Ok(identity) => identity,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Precondition);
        }
    };
    let hub = identity.hub.as_str();
    let session = match target.branch.as_deref() {
        None => None,
        Some(branch) => match landing_session(owner, name, branch, hub)? {
            Ok(session) => Some((branch, session)),
            Err(code) => return Ok(code),
        },
    };
    let settings = format!("{hub}/@{slug}/settings");

    let created = match client.create_invitation(owner, name, role.as_str(), &identity.agent_id) {
        Ok(created) => created,
        Err(error) => {
            super::fix::register_terminal_api_error(&error);
            match error
                .downcast_ref::<crate::hub::client::ApiError>()
                .map(|api| api.status)
            {
                Some(404) => {
                    ui::error(&format!(
                        "only repository owners can create invite links: {slug} does not exist on {hub}, or you are not one of its owners."
                    ));
                    ui::hint(
                        "ask an owner to run `agit repo invite`, or check the signed-in account with `agit whoami`",
                    );
                }
                Some(400) => {
                    ui::error(&format!("{error:#}"));
                    ui::hint(&format!(
                        "existing invite links are listed and revocable in {settings} (Invite by link)"
                    ));
                }
                Some(409) => {
                    ui::error(&format!("{error:#}"));
                    ui::hint(
                        "the repository changed while the link was being created; run the same command again",
                    );
                }
                _ => ui::error(&format!("{error:#}")),
            }
            return Ok(super::terminal_error_code(&error, ExitCode::Network));
        }
    };
    if !is_invitation_token(&created.token) {
        ui::error("the hub answered with a malformed invitation token; no link was printed.");
        ui::hint(&format!(
            "revoke the unusable invitation in {settings} (Invite by link)"
        ));
        return Ok(ExitCode::Network);
    }

    let next = session
        .as_ref()
        .map(|(branch, id)| session_path(owner, name, id, branch));
    let sharer = crate::infra::credentials::load(hub).map(|credential| credential.username);
    let url = invite_link(hub, &created.token, next.as_deref(), sharer.as_deref());
    let session_url = next.as_ref().map(|path| format!("{hub}{path}"));

    if super::json::requested() {
        let mut value = serde_json::json!({
            "schema_version": 1,
            "operation": "invite",
            "repository": slug,
            "url": url,
            "role": role.as_str(),
            "invitation_id": created.invitation.id,
            "created_at": created.invitation.created_at,
            "expires_at": null,
            "settings_url": settings,
        });
        if let (Some((branch, id)), Some(session_url)) = (&session, &session_url) {
            value["branch"] = serde_json::json!(branch);
            value["session_id"] = serde_json::json!(id);
            value["session_url"] = serde_json::json!(session_url);
        }
        println!("{value}");
        return Ok(ExitCode::Ok);
    }

    ui::success(&format!(
        "invite link created for {}",
        match &session {
            Some((branch, _)) => format!("{slug}@{branch}"),
            None => slug.clone(),
        }
    ));
    let mut rows = vec![
        ("invite link", ui::accent(&url)),
        ("repository", slug.clone()),
    ];
    if let Some(session_url) = &session_url {
        rows.push(("session", session_url.clone()));
    }
    rows.push(("role", format!("{} ({})", role.as_str(), role.grants())));
    rows.push((
        "expires",
        "never; anyone holding the link can accept it until it is revoked".into(),
    ));
    print!("{}", ui::table::key_values(&rows));
    if role == InviteRole::Owner {
        ui::warning(&format!(
            "an owner link hands out full control of {slug}; share it only with people you would make owners"
        ));
    }
    ui::hint(&format!("revoke it in {settings} (Invite by link)"));
    Ok(ExitCode::Ok)
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
    let client = super::require_login()?;
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
            super::fix::register_terminal_api_error(&e);
            ui::error(&format!("remote rename failed: {e:#}"));
            return Ok(super::terminal_error_code(&e, ExitCode::Network));
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

    let client = super::require_login()?;
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
            super::fix::register_terminal_api_error(&e);
            ui::error(&format!("{e:#}"));
            Ok(super::terminal_error_code(&e, ExitCode::Network))
        }
    }
}

fn path(repo: Option<String>) -> CmdResult {
    let Some(raw) = repo else {
        ui::error("can’t resolve the repo.");
        ui::hint("use `agit repo path <owner/repo>`, or set AGIT_SESSION");
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
    #[test]
    fn finding_locations_cannot_inject_terminal_controls_or_unbounded_text() {
        assert_eq!(
            finding_display("file\n\u{1b}[31m\u{202e}txt"),
            "file  [31m txt"
        );
        assert_eq!(finding_display(&"界".repeat(600)).chars().count(), 512);
    }

    #[test]
    fn publication_location_details_are_optional_for_older_servers() {
        let response = serde_json::json!({
            "intent_id": "intent", "expires_at": "2099-01-01", "confirmation_phrase": "alice/notes",
            "snapshot": {"refs_digest": "refs", "ruleset_digest": "rules"},
            "findings": {"suspected_secrets": 0, "complete": true}, "warning": "Public history"
        });
        let parsed: crate::hub::PreparePublicResponse =
            serde_json::from_value(response.clone()).unwrap();
        assert!(parsed.finding_locations.is_empty());
        let mut located = response;
        located["finding_locations"] = serde_json::json!([{"rule": "example-rule", "file": "session.jsonl", "line": 7, "redacted": "redacted"}]);
        let parsed: crate::hub::PreparePublicResponse = serde_json::from_value(located).unwrap();
        assert_eq!(parsed.finding_locations[0].line, 7);
    }

    use super::*;

    /// The role defaults to read and accepts only the hub's roles; the target keeps a branch
    /// with slashes whole and refuses an empty one instead of widening it to the repository.
    #[test]
    fn invite_arguments_default_to_read_and_keep_the_branch_whole() {
        use clap::Parser as _;
        use clap::error::ErrorKind;
        let parse = |argv: &[&str]| match crate::commands::Cli::try_parse_from(argv) {
            Ok(crate::commands::Cli {
                command:
                    Some(crate::commands::Commands::Repo(Args {
                        cmd: Cmd::Invite { repo, branch, role },
                    })),
                ..
            }) => Ok((
                match branch {
                    Some(branch) => format!("{repo} -b {branch}"),
                    None => repo,
                },
                role,
            )),
            Ok(_) => panic!("not an invite: {argv:?}"),
            Err(error) => Err(error.kind()),
        };
        assert_eq!(
            parse(&["agit", "repo", "invite", "alice/notes"]),
            Ok(("alice/notes".into(), InviteRole::Read))
        );
        assert_eq!(
            parse(&[
                "agit",
                "repo",
                "invite",
                "alice/notes@a",
                "--role",
                "owner",
                "--json"
            ]),
            Ok(("alice/notes@a".into(), InviteRole::Owner))
        );
        assert_eq!(
            parse(&["agit", "repo", "invite", "alice/notes", "-b", "feat/cache"]),
            Ok(("alice/notes -b feat/cache".into(), InviteRole::Read))
        );
        assert_eq!(
            parse(&["agit", "repo", "invite", "alice/notes", "--role", "admin"]),
            Err(ErrorKind::InvalidValue)
        );
        assert_eq!(
            parse(&["agit", "repo", "invite"]),
            Err(ErrorKind::MissingRequiredArgument)
        );

        assert_eq!(
            parse_invite_target("alice/notes").unwrap(),
            InviteTarget {
                owner: "alice".into(),
                name: "notes".into(),
                branch: None
            }
        );
        assert_eq!(
            parse_invite_target("alice/notes@feat/cache")
                .unwrap()
                .branch
                .as_deref(),
            Some("feat/cache")
        );
        for refused in ["alice/notes@", "notes", "Alice/notes", "alice/notes/x@y"] {
            assert!(parse_invite_target(refused).is_err(), "{refused}");
        }
    }

    /// The session page travels in the fragment as one `encodeURIComponent` value, so a branch's
    /// `/`, `&` or `+` can neither split it nor decode differently in `URLSearchParams`. Leaving
    /// `/` raw, escaping with the RFC 3986 set, or encoding the path only once changes these bytes.
    #[test]
    fn a_session_invite_carries_its_page_as_one_encoded_fragment_value() {
        let token = "0f".repeat(32);
        let session = format!("agit-{}", "a".repeat(40));
        let hub = "https://hub.example.test";
        assert_eq!(
            invite_link(&format!("{hub}/"), &token, None, None),
            format!("{hub}/invite#token={token}")
        );
        assert_eq!(
            invite_link(hub, &token, None, Some("alice")),
            format!("{hub}/invite?share=true&sharer=alice#token={token}")
        );

        let path = session_path("alice", "notes", &session, "feat/a&b+c");
        assert_eq!(
            path,
            format!("/@alice/notes/s/{session}?ref=feat%2Fa%26b%2Bc")
        );
        assert_eq!(
            invite_link(hub, &token, Some(&path), None),
            format!(
                "{hub}/invite#token={token}&next=%2F%40alice%2Fnotes%2Fs%2F{session}%3Fref%3Dfeat%252Fa%2526b%252Bc"
            )
        );
        assert_eq!(encode_uri_component("Az09-_.!~*'()"), "Az09-_.!~*'()");
        assert_eq!(
            encode_uri_component("@/?&=+# %é"),
            "%40%2F%3F%26%3D%2B%23%20%25%C3%A9"
        );

        assert!(is_invitation_token(&token));
        assert!(!is_invitation_token(&token[1..]));
        assert!(!is_invitation_token(&format!("zz{}", &token[2..])));
    }

    /// A session link names only a session the hub already has. Reading the local head instead
    /// would print, for a branch that was never pushed, a link whose landing page is a 404.
    #[test]
    fn only_a_published_session_branch_can_be_a_landing_page() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("repo")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("file line").unwrap();

        repo.git(&["checkout", "-b", "born"]).unwrap();
        meta::write(
            repo.root(),
            &meta::Meta::new_session_line("claude-code".into(), "/work".into()),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("birth").unwrap();

        repo.git(&["checkout", "-b", "work"]).unwrap();
        let session = format!("{}{}", meta::ID_PREFIX, "c".repeat(meta::ID_HEX_LEN));
        meta::write(
            repo.root(),
            &meta::Meta::new(session.clone(), "claude-code".into(), "/work".into()),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("turn").unwrap();
        repo.git(&["branch", "local-only"]).unwrap();
        for published in ["main", "born", "work"] {
            repo.git(&[
                "update-ref",
                &format!("refs/remotes/origin/{published}"),
                published,
            ])
            .unwrap();
        }

        assert_eq!(published_session(&repo, "work").unwrap(), Ok(session));
        for (branch, refusal) in [
            ("local-only", Unlinkable::Unpublished),
            ("absent", Unlinkable::Missing),
            ("main", Unlinkable::FileLine),
            ("born", Unlinkable::Unsettled),
            ("work~1", Unlinkable::BadName),
        ] {
            assert_eq!(published_session(&repo, branch).unwrap(), Err(refusal));
        }
    }

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
