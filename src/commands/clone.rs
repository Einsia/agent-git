//! `agit clone` — bring someone else's memory (or your own, from another machine) back here.
//!
//! # This is the product's most central command
//!
//! Every other command is about *this* side (sign in, settle, push), while the whole argument is
//! about **the far side**: my agent, yours to pick up. This command is that.
//!
//! Three scenarios (after the fetch, the sentence is spoken by `agit run` / `agit resume`):
//!
//! - **Team handoff**: a teammate runs `agit clone einsia/payments`, then says "carry on with the
//!   payments module". No re-reading the repo, no scrolling back through chat logs, no stepping
//!   into the same holes again.
//! - **Paper reproduction**: `agit clone lab/repro-tab3`, then "check my environment and run
//!   table 3". One-way publishing — the author does it once, every reader benefits.
//! - **Open-source collaboration**: `agit clone org/project-agent`, then "where do I start on this
//!   issue".
//!
//! # Read-only by default; your own copy has to be asked for
//!
//! ```text
//! agit clone alice/photo           read it, carry on; origin is alice's and push is refused
//! agit clone alice/photo --mine    the hub creates yours; origin is yours, upstream is alice
//! agit clone me/photo              carry on with your own from another machine
//! ```
//!
//! Folding `use` and `fork` into one `clone` loses the read-only tier, and then "just look" also
//! leaves an agent under your name beside someone else's namespace. "Look first, decide later
//! whether to take it on" is by far the more common intent.
//!
//! `--mine` and not `--fork`: the hub endpoint is named `clone`, so picking that word back up in
//! the CLI would leave the product carrying two names for one thing. And what the user decides is
//! **ownership** (whose this is), not mechanism (whether the hub does a bare clone).
//!
//! # `--mine` copies on the hub first, then fetches
//!
//! The hub `clone --bare`s the other side's bare repo into **your** namespace. This step is not
//! optional: it brings the full git history, so every early snapshot the original author recorded
//! is still there in your repo, and what you continue lands **on top of** their history instead of
//! starting a second root. That is how the lineage chain survives. `clone --bare` goes over a
//! local path and git hard-links the objects, so copying a very large agent costs almost no extra
//! space.
//!
//! The read-only tier does not need this step: clone the other side's repo directly, with the same
//! full history.
//!
//! When the source is an agent already under your own name both steps are skipped: there is
//! nothing to copy, and this route is "carry on from another machine".
//!
//! # Where it lands locally is its identity on the hub
//!
//! ```text
//! agit clone alice/photo          → ~/.agit/repos/alice/photo   (a checkout of alice's repo)
//! agit clone alice/photo --mine   → ~/.agit/repos/<you>/photo   (a checkout of your repo)
//! ```
//!
//! So running `--mine` again over an existing read-only checkout is an **in-place promotion**:
//! create the copy, change `origin`, record `upstream`, move the directory under your namespace.
//! Nothing already committed locally is lost — which is why `agit push` can offer that promotion
//! from inside a read-only checkout (see [`super::push`]).
//!
//! # Running it again fetches the source's new work
//!
//! A read-only checkout's `origin` is the source, so a second `agit clone alice/photo` is fetch +
//! fast-forward. That is the route to "pick up the original author's later work": a copy diverges
//! the moment it is cloned, and without this nothing local records where the source is.
//!
//! # With no argument the hub answers by reverse lookup
//!
//! Inside a code repo with no argument, `/api/agents/for-repo` answers "which agents have worked
//! in this repo". This is the counterpart of "no binding file is written into the code repo" —
//! that question moves to the hub.
//!
//! # The version ID is itself the integrity
//!
//! A snapshot ID is a commit SHA. Checking out `refs/tags/agit-<sha>` yields that content itself —
//! git's content addressing makes the tag name and the content the same thing, so a pickup needs
//! no second verification.
//!
//! # Fetch only, never run
//!
//! clone fetches the content, creates the local branches, binds the current directory, and then
//! **stops**. Installing into a runtime and bringing up the harness belongs to `agit run` (fetch +
//! materialize + launch). A clone that also materializes and launches leaves two pairs of session
//! links under one agent out of nowhere, and `agit commit <agent>` then has to pick among four
//! candidates — a pickup must not create that ambiguity.

use super::{CmdResult, parse_slug};
use crate::domain::meta;
use crate::domain::repo::Repo;
use crate::domain::store::Store;
use crate::domain::transcript;
use crate::domain::workspace;
use crate::hub::RemoteAgent;
use crate::hub::identity::{self, RemoteIdentity};
use crate::infra::{config, credentials};
use crate::{ExitCode, ui};
use anyhow::Context;
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};

#[derive(ClapArgs)]
pub struct Args {
    /// `<owner>/<agent>[@<version-or-branch>]`; omittable inside a code repo — the hub answers by reverse lookup
    ///
    /// versions look like `agit-5aa76353`; anything else is a branch name.
    #[arg(value_name = "owner/agent[@ref]")]
    pub target: Option<String>,

    /// Make it yours: the hub creates a copy under your namespace you can `agit push` to
    ///
    /// without it this is a read-only checkout — origin points upstream and push is refused,
    /// but local commits work fine (committing in a clone you can’t push to is normal git).
    #[arg(long)]
    pub mine: bool,

    /// The copy’s name in your namespace (default: same name). Requires --mine
    #[arg(long, value_name = "name")]
    pub name: Option<String>,

    /// Don’t bind the current directory to this repo
    #[arg(long)]
    pub no_bind: bool,

    /// Bind this directory even if it is already bound to another repo
    #[arg(long, conflicts_with = "no_bind")]
    pub rebind: bool,

    /// Explicitly pin an existing legacy checkout to this verified immutable agent ID
    ///
    /// This never infers identity from the current owner/name route. Use only after
    /// verifying the ID shown by the hub; local branches and unpushed commits stay intact.
    #[arg(
        long,
        value_name = "uuid",
        requires = "target",
        conflicts_with_all = ["mine", "name"]
    )]
    pub adopt_legacy_agent_id: Option<String>,

    /// (deprecated) materializing into a runtime moved to `agit run --as <runtime>`
    #[arg(long = "as", value_name = "runtime", hide = true)]
    pub as_runtime: Option<String>,

    /// (deprecated) clone never launches a runtime — kept so older scripts keep working
    #[arg(long, hide = true)]
    pub no_launch: bool,
}

/// Split the version/branch part out of `owner/agent@ref`.
///
/// `@` is the design's uniform ref separator (`owner/repo@<ref>`, see [`crate::domain::refs`]).
/// Accepting only the colon makes the `agit clone lab/repro@v1` written in the README parse whole
/// as a repo name and answer "no such agent". The colon form keeps working: it appears in
/// documentation already published, and breaking it suddenly does the user no good.
///
/// Version and branch are told apart by the `agit-` prefix: what starts with it is a snapshot ID,
/// anything else is a branch name. That is the second reason the prefix exists (the other is
/// telling it apart from a git SHA).
fn split_ref(target: &str) -> (&str, Option<Ref>) {
    // Neither a branch name nor a repo name contains `@` (`refs::validate_name` rejects it as an
    // illegal character), so the first `@` is the separator.
    let cut = target
        .split_once('@')
        .map(|(s, r)| (s, r, true))
        .or_else(|| target.rsplit_once(':').map(|(s, r)| (s, r, false)));
    match cut {
        Some((slug, r, _)) if !r.is_empty() && !slug.is_empty() => {
            let rf = if r.starts_with(meta::ID_PREFIX) {
                Ref::Version(r.to_string())
            } else {
                Ref::Branch(r.to_string())
            };
            (slug, Some(rf))
        }
        _ => (target, None),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ref {
    Version(String),
    Branch(String),
}

pub fn run(args: Args) -> CmdResult {
    let client = crate::hub::Client::from_env();
    let s = ui::theme::symbols();

    if args.name.is_some() && !args.mine {
        ui::error(
            "`--name` names the copy under your namespace — a read-only checkout creates no copy.",
        );
        ui::hint("want your own: agit clone <owner>/<agent> --mine --name <name>");
        return Ok(ExitCode::Usage);
    }
    // `--mine` **creates an agent under your namespace on the hub**, so signing in is a hard
    // precondition. A read-only pickup deliberately does not require it — a public agent is
    // readable by anyone, and "just take a look" is the route that has to stay open.
    if args.mine && !client.has_token() {
        ui::error(&format!("not signed in to {}.", client.base()));
        ui::hint("`--mine` creates a copy under your name — sign in first with `agit login`");
        return Ok(ExitCode::Usage);
    }

    // ── 1. Decide which agent and which version to fetch ──
    let (src_owner, src_name, want) = match &args.target {
        Some(t) => {
            let (slug, r) = split_ref(t);
            let (o, n) = parse_slug(slug)?;
            (o, n, r)
        }
        None => match resolve_from_repo(&client)? {
            Some((o, n)) => (o, n, None),
            None => return Ok(ExitCode::Usage),
        },
    };

    // ── 2. Decide where it lands and what the two remotes are ──
    let Some(plan) = plan(&client, &src_owner, &src_name, &args)? else {
        return Ok(ExitCode::Usage);
    };
    let (owner, name) = (plan.owner.clone(), plan.name.clone());
    let clone_url = plan.origin.clone();

    // The in-place promotion is already done by this point: that checkout's session is installed
    // and you may be working in it right now. Running "install into a runtime" again adds a second
    // pair of session links under the same agent, and `agit commit photo` then has to pick among
    // four candidates — an ambiguity created out of nowhere.
    if plan.promoted_in_place {
        println!(
            "{}",
            ui::dim(&format!(
                "  keep working: agit commit {name}, then agit push {name}"
            ))
        );
        return Ok(ExitCode::Ok);
    }

    // ── 3. Fetch it locally ──
    let dest = config::repo_dir(&owner, &name)?;
    let slug = format!("{owner}/{name}");
    // An existing checkout means this run fetches the source's new work, not a first pickup. The
    // two want different things from checkout: a fresh checkout lands on a branch you can carry on
    // from, an existing one must never move you off the branch you are sitting on.
    let existed = dest.join(".git").exists();

    if !existed && args.adopt_legacy_agent_id.is_some() {
        ui::error("`--adopt-legacy-agent-id` only applies to an existing legacy checkout.");
        ui::hint("run the clone again without that flag to create a fresh pinned checkout");
        return Ok(ExitCode::Usage);
    }

    if existed {
        println!("updating {}…", ui::bold(&slug));
        let store = Repo::at(&dest);
        // Align the remotes before fetching: the hub address changes, and a read-only checkout's
        // origin is the very source to fetch from.
        plan.apply_remotes(&store, true, args.adopt_legacy_agent_id.as_deref())?;
        // Fast-forward only. Divergence does no text merge — that interleaves two sessions' lines
        // and breaks the message chain, producing a syntactically valid but semantically corrupt
        // transcript.
        let out = crate::hub::git::run(&store, &["fetch", "origin", "--tags"])?;
        if !out.ok() {
            ui::warning("fetch failed — continuing with the local copy.");
        } else if let Some((ahead, behind)) = store.ahead_behind() {
            if ahead > 0 && behind > 0 {
                ui::warning(&format!(
                    "local and remote have diverged (ahead {ahead}, behind {behind}); leaving local untouched."
                ));
            } else if behind > 0 {
                let branch = store.current_branch().ok_or_else(|| {
                    anyhow::anyhow!("cannot fast-forward a detached existing checkout")
                })?;
                super::pull::fast_forward_to(&store, &branch, "@{upstream}")?;
                println!("  {} fast-forwarded {behind} commits", ui::ok(s.check));
            }
        }
    } else {
        println!(
            "fetching {} from {}…",
            ui::bold(&slug),
            ui::accent(client.base())
        );
        if !crate::hub::git::clone(&clone_url, &dest, &plan.identity)?.ok() {
            ui::error("clone failed.");
            ui::hint(
                "private agents need the owner’s grant — check the account used with `agit login`",
            );
            ui::hint(&format!(
                "remote: {}",
                crate::hub::git::redact_url(&clone_url)
            ));
            return Ok(ExitCode::Failure);
        }
        plan.apply_remotes(&Repo::at(&dest), false, None)?;
    }

    let store = Repo::at(&dest);
    plan.report(&src_owner, &src_name);

    // ── 4. Local branches ──
    //
    // After a clone the repo holds only remote-tracking refs and not one local branch, so
    // `resume` / `run` / `push` all break (recovering takes a manual `git branch X origin/X &&
    // git checkout X`). Every branch of an agent repo is a session that can be carried on, so
    // create all of them, not only the one the remote HEAD points at.
    track_all_remote_branches(&store);

    // ── 5. Check out the version/branch that was asked for ──
    //
    // A version is a tag (`refs/tags/agit-<hash>`), a branch is an ordinary ref. Checking out a
    // version is checking out that tag — which is exactly "go back to that turn". A copy carries
    // the source repo's every tag and branch, so the version named here can be any one the
    // original author recorded.
    match &want {
        Some(Ref::Version(v)) => {
            if store
                .git_opt(&["rev-parse", "--verify", &format!("refs/tags/{v}")])
                .is_none()
            {
                ui::error(&format!("{slug} has no version {v}."));
                ui::hint(&format!("see available versions with `agit log {slug}`"));
                return Ok(ExitCode::Failure);
            }
            let target = format!("refs/tags/{v}");
            checkout_target(&store, &target, None)?;
            println!("  {} version {}", ui::ok(s.check), ui::bold(v));
        }
        Some(Ref::Branch(b)) => {
            let r = format!("origin/{b}");
            if store.git_opt(&["rev-parse", "--verify", &r]).is_none() {
                ui::error(&format!("{slug} has no branch {b}."));
                return Ok(ExitCode::Failure);
            }
            if b != "main" && store.has_ref("refs/heads/main") {
                // Session branches each get their own worktree; the main checkout sits on the
                // main file line.
                if !existed || store.current_branch().is_none() {
                    checkout_target(&store, "refs/heads/main", None)?;
                }
                match sync_explicit_branch(&store, b)? {
                    BranchSync::Created | BranchSync::Current => {}
                    BranchSync::FastForwarded(n) => {
                        println!("  {} fast-forwarded {b} by {n} commits", ui::ok(s.check));
                    }
                    BranchSync::Diverged { ahead, behind } => ui::warning(&format!(
                        "{b} has diverged from origin (ahead {ahead}, behind {behind}); opening the local line as it is"
                    )),
                }
                super::worktree::checkout(&store, b)?;
            } else {
                checkout_target(&store, &r, Some(b))?;
            }
            let _ = store.git(&["branch", &format!("--set-upstream-to=origin/{b}"), b]);
            println!("  {} branch {}", ui::ok(s.check), ui::bold(b));
        }
        None => {
            // Only a first pickup (or a HEAD left dangling) decides where to land.
            if (!existed || store.current_branch().is_none())
                && let Some(b) = default_checkout(&store)
            {
                checkout_target(&store, &format!("refs/heads/{b}"), None)?;
            }
        }
    }

    let heads = local_branches(&store);
    println!(
        "  {} {} branches  {}",
        ui::ok(s.check),
        heads.len(),
        ui::dim(&ui::tilde(&dest))
    );
    if heads.is_empty() {
        // The remote has branches and not one came down locally: the fetch itself went wrong,
        // and it must not read as "this agent is empty".
        ui::warning("no branches came down — nothing to continue from.");
        ui::hint(&format!("check what the hub has: `agit log {slug}`"));
        return Ok(ExitCode::Ok);
    }
    println!("{}", ui::dim(&format!("    {}", heads.join(", "))));

    // ── 6. Bind the current directory ──
    //
    // clone and init are the only two commands that declare a directory binding (the "local
    // layout" design): the point of a fetch is to carry on here, and zero-argument commands rely
    // entirely on this binding to answer "which repo is this".
    if !args.no_bind {
        let here = std::env::current_dir()?;
        workspace::bind(&here, &slug, args.rebind)?;
        println!("{}", ui::dim(&format!("  bound to {}", ui::tilde(&here))));
    }

    // An unreadable session/meta.json means something else (a plain git push) put this repo up.
    // Later commits treat the current branch as one no session has taken yet and claim it under
    // the new layout — say so instead of staying silent.
    if let Err(e) = meta::resolve(&dest) {
        ui::warning(&format!("couldn’t read session metadata: {e:#}"));
        ui::hint("later commits will treat this branch as the start of a new line");
    }

    // ── 7. Fetch only, never run ──
    //
    // Materializing into a runtime and bringing up the harness belongs to `agit run`. This only
    // spells out the next command: which kind of line HEAD sits on, and whether this one can be
    // pushed back.
    if args.as_runtime.is_some() {
        ui::hint("`--as` moved to `agit run --as <runtime>` — clone only fetches");
    }
    println!();
    let head = store.current_branch();
    let on_file_line = head
        .as_deref()
        .is_some_and(|b| meta::is_file_line_at(&store, &format!("refs/heads/{b}")));
    match (&head, plan.writable, on_file_line) {
        // Your own session line: carry straight on.
        (Some(b), true, false) => println!("  {}", ui::accent(&format!("agit resume {b}"))),
        // main is the file line and is never resumed — start a new session off it, inheriting
        // memory/skills.
        (Some(_), true, true) => println!("  {}", ui::accent("agit new -b <name>")),
        // Someone else's: running it necessarily forks off a line you can write to.
        (Some(b), false, _) => println!(
            "  {}",
            ui::accent(&format!("agit run {slug}@{b} -b <name>"))
        ),
        (None, _, _) => println!(
            "  {}",
            ui::accent(&format!("agit run {slug}@{} -b <name>", heads[0]))
        ),
    }
    println!(
        "{}",
        ui::dim(&if plan.writable {
            "  fetched only — that command materializes it into a runtime and continues".to_string()
        } else {
            format!(
                "  fetched only, and read-only: local commits work, publishing needs your own copy
  \
                 agit clone {src_owner}/{src_name} --mine"
            )
        })
    );
    Ok(ExitCode::Ok)
}

/// Change the active checkout only after proving that a v1 target cannot claim user-owned v0
/// paths. `reset_branch` implements clone's explicit `-B <branch> <remote>` form; tags and an
/// already-created local branch use an ordinary detached/branch checkout.
fn checkout_target(repo: &Repo, target: &str, reset_branch: Option<&str>) -> crate::Result<()> {
    super::plumbing::ensure_safe_checkout(repo, target)?;
    match reset_branch {
        Some(branch) => {
            repo.git(&["checkout", "--quiet", "-B", branch, target])?;
        }
        None => {
            repo.git(&["checkout", "--quiet", target])?;
        }
    }
    Ok(())
}

/// Create a same-named local branch with an upstream for every remote branch. Existing ones are
/// left alone.
fn track_all_remote_branches(repo: &Repo) -> Vec<String> {
    let mut made = vec![];
    for b in repo.remote_branches() {
        if repo.has_ref(&format!("refs/heads/{b}")) {
            continue;
        }
        if repo
            .git(&["branch", "--track", &b, &format!("origin/{b}")])
            .is_ok()
        {
            made.push(b);
        }
    }
    made
}

/// The local state of the branch that was explicitly asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchSync {
    /// Not present locally; just created from `origin/<b>`.
    Created,
    /// Identical to `origin/<b>`.
    Current,
    /// Behind the remote; fast-forwarded by this many commits.
    FastForwarded(usize),
    /// Each side has commits the other lacks: leave the local branch alone and hand it to the user.
    Diverged { ahead: usize, behind: usize },
}

/// Align the explicitly requested branch with `origin/<b>`: the main checkout's ahead/behind
/// speaks for main and for no other branch — opening without comparing can hand you the stale
/// session an earlier fetch left behind.
fn sync_explicit_branch(store: &Repo, branch: &str) -> crate::Result<BranchSync> {
    let remote = format!("refs/remotes/origin/{branch}");
    let local = format!("refs/heads/{branch}");
    if !store.has_ref(&local) {
        store.git(&["branch", branch, &remote])?;
        return Ok(BranchSync::Created);
    }
    let counts = store.git(&[
        "rev-list",
        "--left-right",
        "--count",
        &format!("{local}...{remote}"),
    ])?;
    let mut parts = counts.split_whitespace();
    let ahead: usize = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    let behind: usize = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    match (ahead, behind) {
        (_, 0) => Ok(BranchSync::Current),
        (0, behind) => {
            super::pull::fast_forward_to(store, branch, &remote)?;
            Ok(BranchSync::FastForwarded(behind))
        }
        (ahead, behind) => Ok(BranchSync::Diverged { ahead, behind }),
    }
}

/// Where HEAD lands after a clone.
///
/// main (the file line, the trunk of team memory) wins; a pure session agent with no main lands on
/// the **most recently active** branch — "fetch it and carry straight on" is the whole point of
/// this command, and landing on whichever branch happens to sort first is as useless as landing on
/// a detached HEAD.
fn default_checkout(repo: &Repo) -> Option<String> {
    if repo.has_ref("refs/heads/main") {
        return Some("main".into());
    }
    local_branches(repo).into_iter().next()
}

/// Local branches, most recently committed first.
fn local_branches(repo: &Repo) -> Vec<String> {
    repo.git_opt(&[
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(refname:short)",
        "refs/heads/",
    ])
    .unwrap_or_default()
    .lines()
    .map(|l| l.trim().to_string())
    .filter(|l| !l.is_empty())
    .collect()
}

/// Where this pickup lands and what each of the two remotes points at.
///
/// All three tiers are expressed in this one struct, so nothing further down `run` branches on the
/// case again — scattered `if args.mine` is what breeds regressions like "the read-only tier
/// disappeared".
pub struct Plan {
    /// Where it lands locally, which is also its identity on the hub.
    pub owner: String,
    pub name: String,
    /// `origin`: where pushes go, and where a second `agit clone` fetches from.
    pub origin: String,
    /// The immutable remote identity of `origin`.
    pub identity: RemoteIdentity,
    /// `upstream`: the source. Only a copy has one — a read-only checkout's origin is the source.
    pub upstream: Option<String>,
    /// Whether this one can be pushed. A read-only checkout cannot.
    pub writable: bool,
    /// This run only promotes an existing read-only checkout to yours; no new content is fetched.
    pub promoted_in_place: bool,
}

impl Plan {
    fn apply_remotes(
        &self,
        repo: &Repo,
        existed: bool,
        adopt_legacy_agent_id: Option<&str>,
    ) -> crate::Result<()> {
        if existed {
            // A GET on the current slug cannot vouch for a legacy checkout: a name can be
            // deleted and reused. An existing repo must already carry a pin, and that pin must
            // agree with this API answer.
            match (identity::read(repo)?, adopt_legacy_agent_id) {
                (Some(_), Some(_)) => anyhow::bail!(
                    "this checkout already has an immutable remote identity; re-run without `--adopt-legacy-agent-id`"
                ),
                (Some(pinned), None) => {
                    if pinned.hub != self.identity.hub {
                        anyhow::bail!(
                            "this checkout belongs to {}, but the current hub is {}; refusing to send it to a different hub",
                            pinned.hub,
                            self.identity.hub
                        );
                    }
                    if pinned != self.identity {
                        anyhow::bail!(
                            "the remote name now identifies agent {}, but this checkout is pinned to {}; refusing to adopt a reused slug",
                            self.identity.agent_id,
                            pinned.agent_id
                        );
                    }
                }
                (None, None) => anyhow::bail!(
                    "this legacy checkout has no immutable remote identity, so owner/name cannot prove what it belongs to.
  verify the current agent ID, then explicitly preserve this checkout with:
    agit clone {}/{} --adopt-legacy-agent-id {}",
                    self.owner,
                    self.name,
                    self.identity.agent_id
                ),
                (None, Some(supplied)) => {
                    let explicit = RemoteIdentity::new(&self.identity.hub, supplied)
                        .context("invalid --adopt-legacy-agent-id")?;
                    if explicit != self.identity {
                        anyhow::bail!(
                            "--adopt-legacy-agent-id names agent {}, but {}/{} currently identifies {}; refusing to adopt a same-name replacement",
                            explicit.agent_id,
                            self.owner,
                            self.name,
                            self.identity.agent_id
                        );
                    }
                    let origin = repo.remote_url().ok_or_else(|| {
                        anyhow::anyhow!(
                            "this existing checkout has no origin, so it is not eligible for legacy adoption"
                        )
                    })?;
                    if !super::same_hub(&origin, &self.identity.hub)
                        || super::remote_slug(&origin)
                            != Some((self.owner.clone(), self.name.clone()))
                    {
                        anyhow::bail!(
                            "this checkout's origin ({}) is not {}/{} on {}; refusing to attach an unrelated repository",
                            crate::hub::git::redact_url(&origin),
                            self.owner,
                            self.name,
                            self.identity.hub
                        );
                    }
                    identity::pin(repo, &explicit)?;
                }
            }
        } else {
            // Pin before URL: a failure part-way leaves at most "pinned, no origin yet", which
            // is safe to retry; the other order leaves a checkout that looks usable but has no
            // fencing identity.
            identity::pin(repo, &self.identity)?;
        }
        repo.set_remote(&self.origin)?;
        if let Some(u) = &self.upstream {
            repo.set_upstream(u)?;
        }
        Ok(())
    }

    /// Say whose this is and whether it can be pushed, before anything else.
    ///
    /// It has to be explicit: read-only and copy have entirely different next steps, while the
    /// rest of the output on both routes looks the same.
    fn report(&self, src_owner: &str, src_name: &str) {
        let s = ui::theme::symbols();
        if self.writable {
            if self.upstream.is_some() {
                println!(
                    "  {} your copy, sourced from {}",
                    ui::ok(s.check),
                    ui::bold(&format!("{src_owner}/{src_name}"))
                );
            }
            return;
        }
        println!(
            "  {} read-only: origin is {} — you can’t push to it",
            ui::dim(s.node),
            ui::bold(&format!("{src_owner}/{src_name}"))
        );
    }
}

/// Which tier this pickup falls into.
///
/// A function of its own gives "read-only is the default" a **single** test instead of a few
/// `if args.mine` scattered through `plan`. That scattering is how the read-only tier gets lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The source is an agent already under your own name: carry on from another machine, with
    /// nothing to create.
    Own,
    /// A read-only pickup (the default).
    ReadOnly,
    /// Create a copy under your name.
    Copy,
}

fn mode_of(me: Option<&str>, src_owner: &str, mine: bool) -> Mode {
    match (me == Some(src_owner), mine) {
        (true, _) => Mode::Own,
        (false, true) => Mode::Copy,
        (false, false) => Mode::ReadOnly,
    }
}

/// Decide where this pickup lands and what its remotes are. `None` means the reason is already
/// stated and the caller exits.
///
/// With `--mine` and a read-only checkout already present locally, that checkout is **promoted in
/// place** rather than duplicated: it may already hold commits of yours, and cloning a fresh copy
/// would strand them in a directory nobody looks at again.
fn plan(
    client: &crate::hub::Client,
    src_owner: &str,
    src_name: &str,
    args: &Args,
) -> crate::Result<Option<Plan>> {
    let me = credentials::current_user();
    let mode = mode_of(me.as_deref(), src_owner, args.mine);

    // ── Your own agent: carry on from another machine ──
    if mode == Mode::Own {
        if args.mine {
            ui::error(&format!("{src_owner}/{src_name} is already yours."));
            ui::hint(&format!(
                "fetch it directly: agit clone {src_owner}/{src_name}"
            ));
            return Ok(None);
        }
        let remote = client.get_agent(src_owner, src_name)?;
        let identity = RemoteIdentity::new(client.base(), &remote.agent_id)?;
        return Ok(Some(Plan {
            owner: remote.owner,
            name: remote.name,
            origin: remote.clone_url,
            identity,
            upstream: None,
            writable: true,
            promoted_in_place: false,
        }));
    }

    // Confirm the source exists and is readable first — otherwise the copy request fails with a
    // vaguer error.
    let source = client.get_agent(src_owner, src_name)?;
    let source_identity = RemoteIdentity::new(client.base(), &source.agent_id)?;

    if mode == Mode::ReadOnly {
        return Ok(Some(Plan {
            owner: source.owner,
            name: source.name,
            origin: source.clone_url,
            identity: source_identity,
            upstream: None,
            writable: false,
            promoted_in_place: false,
        }));
    }

    let existing = config::repo_dir(src_owner, src_name)?;
    if existing.join(".git").exists() {
        return promote(client, &existing, &source, args.name.as_deref()).map(Some);
    }

    println!(
        "copying {} ({} sessions) into your namespace…",
        ui::bold(&source.slug()),
        source.session_count
    );
    let resp = client.clone_agent(
        src_owner,
        src_name,
        args.name.as_deref(),
        &source_identity.agent_id,
    )?;
    let copy_identity = validate_copy_response(client.base(), &resp, &source_identity)?;
    println!(
        "{} copied as {}",
        ui::ok(ui::theme::symbols().check),
        ui::bold(&format!("{}/{}", resp.owner, resp.name))
    );
    println!("  {}", ui::accent(&resp.web_url));
    println!(
        "{}",
        ui::dim(&format!(
            "  your own copy now — visibility inherited ({}); publish independently with agit push.",
            visibility_of(&source)
        ))
    );
    Ok(Some(Plan {
        owner: resp.owner,
        name: resp.name,
        origin: resp.push_url,
        identity: copy_identity,
        upstream: Some(source.clone_url),
        writable: true,
        promoted_in_place: false,
    }))
}

/// How the copy's visibility reads to a person.
///
/// The hub makes a copy **inherit** the source's visibility (`agents::routes::clone`), so this
/// sentence is not a client-side guess; the moment the two disagree, a user believes a public copy
/// is private.
fn visibility_of(source: &RemoteAgent) -> &'static str {
    if source.is_public() {
        "public"
    } else {
        "private"
    }
}

fn validate_copy_response(
    hub: &str,
    response: &crate::hub::PublishResponse,
    expected_source: &RemoteIdentity,
) -> crate::Result<RemoteIdentity> {
    let forked_from = response.forked_from.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "the hub created a copy without confirming its immutable source; refusing to attach the local checkout"
        )
    })?;
    let confirmed_source = RemoteIdentity::new(hub, forked_from)
        .context("the hub returned an invalid forked_from identity")?;
    if confirmed_source != *expected_source {
        anyhow::bail!(
            "the hub says the copy came from agent {}, but this operation fenced source {}; refusing to attach the wrong copy",
            confirmed_source.agent_id,
            expected_source.agent_id
        );
    }
    let copy = RemoteIdentity::new(hub, &response.agent_id)?;
    if copy == *expected_source {
        anyhow::bail!(
            "the hub returned the source agent itself instead of a distinct copy; refusing to rebind the checkout"
        );
    }
    Ok(copy)
}

/// Promote a read-only checkout into "the one under your name".
///
/// Four things, and the order matters: have the hub create the copy first (a failure there has
/// touched nothing locally), then change origin and upstream, and move the directory last. Moving
/// the directory goes last because it is the only step whose failure needs a person to clean up.
///
/// The directory has to move: `~/.agit/repos/<owner>/<name>` records which repo on the hub this is
/// a checkout of, and after the promotion that agent is yours, so the path following along is what
/// is true — leaving it where it was is a lie.
///
/// The links in the store are renamed with it — their `agent` field is the reverse index for
/// `agit commit <agent>`, and if `--name` changes the name while the links do not follow, the next
/// commit finds nothing (or worse, finds another agent of the same name).
pub fn promote(
    client: &crate::hub::Client,
    checkout: &Path,
    source: &RemoteAgent,
    as_name: Option<&str>,
) -> crate::Result<Plan> {
    let s = ui::theme::symbols();
    let source_identity = RemoteIdentity::new(client.base(), &source.agent_id)?;
    let repo = Repo::at(checkout);
    let pinned = identity::require_current(&repo, client.base())?;
    if pinned != source_identity {
        anyhow::bail!(
            "this checkout is pinned to agent {}, but {}/{} now resolves to {}; refusing to promote a reused name",
            pinned.agent_id,
            source.owner,
            source.name,
            source_identity.agent_id
        );
    }
    println!("copying {} into your namespace…", ui::bold(&source.slug()));
    let resp = client.clone_agent(
        &source.owner,
        &source.name,
        as_name,
        &source_identity.agent_id,
    )?;
    let copy_identity = validate_copy_response(client.base(), &resp, &source_identity)?;

    let dest = config::repo_dir(&resp.owner, &resp.name)?;
    if dest != checkout && dest.join(".git").exists() {
        anyhow::bail!(
            "a checkout of {}/{} already exists locally ({}) — can’t move this one there.
  \
             name the copy differently: agit clone {} --mine --name <other-name>",
            resp.owner,
            resp.name,
            ui::tilde(&dest),
            source.slug()
        );
    }

    identity::rebind(&repo, &source_identity, &copy_identity)?;
    repo.set_remote(&resp.push_url)?;
    repo.set_upstream(&source.clone_url)?;

    if dest != checkout {
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::rename(checkout, &dest)
            .with_context(|| format!("can’t move {} to {}", checkout.display(), dest.display()))?;
    }
    rename_links(&source.name, &resp.name)?;

    println!(
        "{} {} is now yours: {}",
        ui::ok(s.check),
        ui::bold(&source.slug()),
        ui::bold(&format!("{}/{}", resp.owner, resp.name))
    );
    println!("  {}", ui::accent(&resp.web_url));
    println!(
        "{}",
        ui::dim(&format!(
            "  everything you committed locally survives; visibility inherits ({}).",
            visibility_of(source)
        ))
    );

    Ok(Plan {
        owner: resp.owner,
        name: resp.name,
        origin: resp.push_url,
        identity: copy_identity,
        upstream: Some(source.clone_url.clone()),
        writable: true,
        promoted_in_place: true,
    })
}

/// After the copy is renamed, move the store links that point at the old name over to it.
fn rename_links(from: &str, to: &str) -> crate::Result<()> {
    if from == to {
        return Ok(());
    }
    let Some(store) = Store::open()? else {
        return Ok(());
    };
    for mut lk in crate::domain::link::list(&store) {
        if lk.agent.as_deref() == Some(from) {
            lk.agent = Some(to.to_string());
            crate::domain::link::write(&store, &lk)?;
        }
    }
    Ok(())
}

/// With no argument: have the hub reverse-look-up which agents this repo has.
fn resolve_from_repo(client: &crate::hub::Client) -> crate::Result<Option<(String, String)>> {
    let Some(origin) = config::repo_origin() else {
        ui::error("not inside a code repository, or this repo has no origin.");
        ui::hint("name what to copy: agit clone <owner>/<agent>");
        return Ok(None);
    };

    let sp = ui::spinner("asking the hub which agents worked here…");
    let candidates = client.agents_for_repo(&origin);
    sp.finish_and_clear();

    let candidates = match candidates {
        Ok(c) => c,
        Err(e) => {
            ui::error(&format!("reverse lookup failed: {e:#}"));
            ui::hint("name it: agit clone <owner>/<agent>");
            return Ok(None);
        }
    };

    match candidates.len() {
        0 => {
            println!("no agent has worked on this repo (or none are visible to you).");
            ui::hint("list what you can see: agit clone <owner>/<agent>");
            Ok(None)
        }
        1 => {
            let a = &candidates[0];
            println!(
                "{} {}",
                ui::dim("agents on this repo:"),
                ui::bold(&a.slug())
            );
            Ok(Some((a.owner.clone(), a.name.clone())))
        }
        _ => pick(&candidates),
    }
}

/// Let the user pick when there are several candidates.
fn pick(candidates: &[RemoteAgent]) -> crate::Result<Option<(String, String)>> {
    let labels: Vec<String> = candidates
        .iter()
        .map(|a| {
            let gist = a
                .last_gist
                .as_deref()
                .map(|g| format!("  \"{}\"", ui::truncate(g, 40)))
                .unwrap_or_default();
            format!("{} ({} sessions){gist}", a.slug(), a.session_count)
        })
        .collect();
    let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();

    match ui::prompt::select("several agents worked here — copy which?", &refs)? {
        Some(i) => Ok(Some((
            candidates[i].owner.clone(),
            candidates[i].name.clone(),
        ))),
        None => {
            // A non-interactive environment cannot be asked — list the candidates and make the
            // user name one; never guess.
            ui::error("multiple candidates and nothing interactive to ask with.");
            for a in candidates {
                println!("  {}", a.slug());
            }
            ui::hint("be explicit: agit clone <owner>/<agent>");
            Ok(None)
        }
    }
}

/// Read the text to install into a runtime from the repo root: `session/VIEW` unwrapped back to
/// raw lines.
///
/// Why the VIEW and not the transcript itself: `session/log.jsonl` is the full history (the one for
/// people, the one `agit show` reads), while `session/VIEW` is the resume view — the slice from the
/// last compact boundary (inclusive) to the end, rewritten whole at every commit (see
/// [`crate::domain::transcript`]). So on a settled session line it is necessarily present and
/// unwrappable; missing, or unwrapping to nothing, is an incomplete checkout (deleted by hand, a
/// half-finished fetch) — refuse, point at re-running clone/fetch, and never silently degrade into
/// installing the full history.
///
/// Unwrapping goes through [`transcript::unwrap_lossy`]: one corrupt line does not sink the whole
/// pickup, but the count is declared.
pub fn view_text_for_install(repo_root: &Path) -> crate::Result<String> {
    let p = repo_root.join(meta::VIEW_FILE);
    let raw =
        crate::domain::storage::materialize_worktree(repo_root, meta::VIEW_FILE).map_err(|e| {
            anyhow::anyhow!(
                "couldn’t read {} ({e}).
  \
             this checkout is incomplete — re-run `agit clone` (or `agit fetch`) to bring {} back",
                p.display(),
                meta::VIEW_FILE
            )
        })?;
    let (text, skipped) = transcript::unwrap_lossy(&raw);
    if text.trim().is_empty() {
        anyhow::bail!(
            "no raw lines could be recovered from {}.
  \
             this checkout is incomplete — re-run `agit clone` (or `agit fetch`) to bring {} back",
            p.display(),
            meta::VIEW_FILE
        );
    }
    if skipped > 0 {
        ui::warning(&format!(
            "{} had {skipped} lines that aren’t valid envelopes — skipped in the install.",
            meta::VIEW_FILE
        ));
    }
    let hydrated = crate::domain::secret_filter::RepositoryDictionary::open(repo_root)?
        .hydrate_jsonl(&text)?;
    if hydrated.unresolved > 0 {
        ui::warning(&format!(
            "{} repository secret placeholder(s) have no local dictionary entry and were left unchanged.",
            hydrated.unresolved
        ));
        ui::hint(
            "repository secret dictionaries are device-local and are never fetched from the hub",
        );
    }
    Ok(hydrated.text)
}

/// The path of an agent on this machine (reused by `agit log --agent` and friends).
pub fn local_path(owner: &str, name: &str) -> crate::Result<PathBuf> {
    config::repo_dir(owner, name)
}

/// The store of an agent on this machine (`None` when it does not exist).
pub fn local_store(owner: &str, name: &str) -> crate::Result<Option<Repo>> {
    let p = local_path(owner, name)?;
    Ok(if p.join(".git").exists() {
        Some(Repo::at(p))
    } else {
        None
    })
}

/// List every agent on this machine.
pub fn list_local() -> crate::Result<Vec<(String, String, PathBuf)>> {
    let root = config::repos_dir()?;
    let mut out = vec![];
    if !root.exists() {
        return Ok(out);
    }
    for owner_entry in std::fs::read_dir(&root)? {
        let Ok(oe) = owner_entry else { continue };
        if !oe.path().is_dir() {
            continue;
        }
        let owner = oe.file_name().to_string_lossy().to_string();
        for name_entry in std::fs::read_dir(oe.path())? {
            let Ok(ne) = name_entry else { continue };
            let p = ne.path();
            if !p.join(".git").exists() {
                continue;
            }
            out.push((
                owner.clone(),
                ne.file_name().to_string_lossy().to_string(),
                p,
            ));
        }
    }
    out.sort();
    Ok(out)
}

/// Whether a path sits under an agent directory.
pub fn is_local_agent(p: &Path) -> bool {
    config::repos_dir()
        .map(|root| p.starts_with(root))
        .unwrap_or(false)
}

/// A checkout of an agent name on this machine.
///
/// One name can have two checkouts: your own `<you>/photo`, and the `alice/photo` a read-only
/// pickup left behind. So "agent name → repo" is not a function: either it is unique, or a person
/// has to say which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub owner: String,
    pub name: String,
    pub path: PathBuf,
}

impl Checkout {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// The checkouts on this machine with this name, yours first.
pub fn checkouts_named(me: &str, name: &str) -> crate::Result<Vec<Checkout>> {
    let mut out: Vec<Checkout> = list_local()?
        .into_iter()
        .filter(|(_, n, _)| n == name)
        .map(|(owner, name, path)| Checkout { owner, name, path })
        .collect();
    // Yours wins: the "exactly one" tests below all read this order.
    out.sort_by_key(|c| c.owner != me);
    Ok(out)
}

/// Which repo this agent name records versions into.
///
/// Three tiers:
///
/// * Exactly one checkout → that one. The one a read-only pickup left behind (under the source
///   author's name) takes this route too: committing into a clone you cannot push to is ordinary
///   git, while opening a fresh empty repo under your name drops the original author's entire
///   history and nothing fast-forwards afterwards.
/// * None → the path under your name, where `agit commit` creates it.
/// * Both → **error out**. "photo" then means either your own or the one just fetched; the two
///   readings point at different lineages, and picking one for the user is the worst thing to do.
pub fn checkout_for_recording(me: &str, name: &str) -> crate::Result<PathBuf> {
    match choose_for_recording(name, &checkouts_named(me, name)?)? {
        Some(p) => Ok(p),
        None => config::repo_dir(me, name),
    }
}

/// The body of the three-tier test above. `None` = no checkout at all, and the caller creates one
/// at the path under your name.
fn choose_for_recording(name: &str, found: &[Checkout]) -> crate::Result<Option<PathBuf>> {
    match found {
        [] => Ok(None),
        [only] => Ok(Some(only.path.clone())),
        many => {
            let list: Vec<String> = many
                .iter()
                .map(|c| format!("    {}  {}", c.slug(), ui::tilde(&c.path)))
                .collect();
            anyhow::bail!(
                "this machine has {} agents named `{name}` — can’t tell which repo to settle into:\n{}\n  \
                 rename the checkout: agit clone <owner>/{name} --mine --name <other>",
                many.len(),
                list.join("\n")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Args, Checkout, Mode, Plan, Ref, checkout_target, choose_for_recording, mode_of, split_ref,
        validate_copy_response,
    };
    use crate::hub::identity::{self, RemoteIdentity};
    use clap::Parser;
    use std::path::PathBuf;

    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        a: Args,
    }

    /// clone fetches and stops there, and binds the current directory by default.
    ///
    /// The whole "fetch + install + launch" chain belongs to `agit run`; when clone materializes
    /// as well, two pairs of session links appear under one agent out of nowhere and
    /// `agit commit <agent>` has to pick among four candidates. The binding is the **only** pair
    /// of directory declarations the design gives to clone and init — without it, no zero-argument
    /// command knows which repo it is in after a fetch.
    #[test]
    fn cloning_fetches_binds_and_stops_there() {
        let a = W::parse_from(["x", "alice/photo"]).a;
        assert!(!a.no_bind, "binding the current directory is the default");
        assert!(W::parse_from(["x", "alice/photo", "--no-bind"]).a.no_bind);
        // The materialization flags are compatibility shells only and no longer appear in help.
        assert!(a.as_runtime.is_none() && !a.no_launch);
        assert!(
            W::try_parse_from(["x", "alice/photo", "--no-launch"]).is_ok(),
            "older scripts must keep parsing"
        );
    }

    /// `--name` is a flag that is actually read, not decoration.
    ///
    /// A flag that is declared but never used makes `--name` silently ineffective — the user
    /// believes the copy was renamed when it was not. This pins that a declared flag is passed
    /// through.
    #[test]
    fn the_copy_can_be_renamed() {
        let w = W::parse_from(["x", "alice/photo", "--mine", "--name", "photo-mine"]);
        assert_eq!(w.a.name.as_deref(), Some("photo-mine"));
        assert_eq!(
            W::parse_from(["x", "alice/photo"]).a.name,
            None,
            "omitting it keeps the original name"
        );
    }

    #[test]
    fn legacy_adoption_requires_a_target_id_and_cannot_hide_inside_copying() {
        let id = "00000000-0000-0000-0000-000000000002";
        let parsed = W::parse_from(["x", "alice/photo", "--adopt-legacy-agent-id", id]);
        assert_eq!(parsed.a.adopt_legacy_agent_id.as_deref(), Some(id));
        assert!(
            W::try_parse_from(["x", "--adopt-legacy-agent-id", id]).is_err(),
            "adoption must name the route whose live immutable ID was verified"
        );
        assert!(
            W::try_parse_from(["x", "alice/photo", "--mine", "--adopt-legacy-agent-id", id,])
                .is_err(),
            "adopting an old checkout and creating a copy are separate explicit operations"
        );
    }

    /// A pickup is read-only by default; your own copy has to be asked for.
    ///
    /// This is the command's product decision: with `use` and `fork` folded together, "just look"
    /// also creates an agent under your name, while "look first, decide later whether to take it
    /// on" is by far the more common intent.
    #[test]
    fn taking_ownership_is_opt_in() {
        assert!(
            !W::parse_from(["x", "alice/photo"]).a.mine,
            "read-only is the default"
        );
        assert!(W::parse_from(["x", "alice/photo", "--mine"]).a.mine);
    }

    /// The `agit-` prefix is the only test separating a version from a branch name.
    ///
    /// That is also why a branch name and an agent name may not start with `agit-` (see
    /// [`crate::domain::repo::valid_name`]) — otherwise there is nothing here to disambiguate on.
    #[test]
    fn version_ids_are_told_apart_from_branch_names() {
        let id = "agit-21fc4fdc111ed596a78771f54f45f7a6004d9d5d";
        assert_eq!(
            split_ref(&format!("alice/photo:{id}")),
            ("alice/photo", Some(Ref::Version(id.to_string())))
        );
        assert_eq!(
            split_ref("alice/photo:exif"),
            ("alice/photo", Some(Ref::Branch("exif".into())))
        );
        assert_eq!(split_ref("alice/photo"), ("alice/photo", None));
        // A trailing colon must not yield an empty ref.
        assert_eq!(split_ref("alice/photo:"), ("alice/photo:", None));
    }

    /// The separator in the design is `@` (`owner/repo@<ref>`). Accepting only the colon makes
    /// the `agit clone lab/repro@v1` written in the README parse whole as a repo name and answer
    /// "no such agent".
    #[test]
    fn the_designed_at_syntax_is_what_users_actually_type() {
        let id = "agit-21fc4fdc111ed596a78771f54f45f7a6004d9d5d";
        assert_eq!(
            split_ref("lab/repro@v1"),
            ("lab/repro", Some(Ref::Branch("v1".into())))
        );
        assert_eq!(
            split_ref(&format!("lab/repro@{id}")),
            ("lab/repro", Some(Ref::Version(id.to_string())))
        );
        // An empty ref does not count; a bare slug comes back unchanged.
        assert_eq!(split_ref("lab/repro@"), ("lab/repro@", None));
        assert_eq!(split_ref("lab/repro"), ("lab/repro", None));
    }

    // ──────────── Three pickup modes ────────────

    /// Fetching someone else's thing creates **nothing** under your name by default.
    ///
    /// This is the core promise. It has one test (`mode_of`) instead of a few scattered
    /// `if args.mine`, because that scattering is how the read-only tier gets lost: fold `use` and
    /// `fork` together and the "create no copy" branch evaporates in the refactor with no test
    /// watching it.
    #[test]
    fn cloning_someone_elses_agent_creates_nothing_by_default() {
        assert_eq!(mode_of(Some("me"), "alice", false), Mode::ReadOnly);
        assert_eq!(mode_of(Some("me"), "alice", true), Mode::Copy);
        // A read-only pickup works without signing in — a public agent is readable by anyone.
        assert_eq!(mode_of(None, "alice", false), Mode::ReadOnly);
    }

    /// Your own agent is always `Own`; `--mine` means nothing in that tier (`plan` errors out).
    #[test]
    fn your_own_agent_is_never_a_copy() {
        assert_eq!(mode_of(Some("me"), "me", false), Mode::Own);
        assert_eq!(mode_of(Some("me"), "me", true), Mode::Own);
    }

    #[test]
    fn explicit_legacy_adoption_preserves_unpushed_commits_and_rejects_replacements() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(repo.root().join("local-only.txt"), "not pushed\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("local work").unwrap();
        let local_head = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.set_remote("https://hub.test/me/photo.git").unwrap();
        let current =
            RemoteIdentity::new("https://hub.test", "00000000-0000-0000-0000-000000000002")
                .unwrap();
        let plan = Plan {
            owner: "me".into(),
            name: "photo".into(),
            origin: "https://hub.test/me/photo.git".into(),
            identity: current.clone(),
            upstream: None,
            writable: true,
            promoted_in_place: false,
        };

        let ordinary = plan
            .apply_remotes(&repo, true, None)
            .unwrap_err()
            .to_string();
        assert!(ordinary.contains("--adopt-legacy-agent-id"), "{ordinary}");
        assert!(ordinary.contains(&current.agent_id), "{ordinary}");
        assert_eq!(identity::read(&repo).unwrap(), None);
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), local_head);

        let old = RemoteIdentity::new("https://hub.test", "00000000-0000-0000-0000-000000000001")
            .unwrap();
        let replacement = plan
            .apply_remotes(&repo, true, Some(&old.agent_id))
            .unwrap_err()
            .to_string();
        assert!(
            replacement.contains("same-name replacement"),
            "{replacement}"
        );
        assert_eq!(identity::read(&repo).unwrap(), None);
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), local_head);

        plan.apply_remotes(&repo, true, Some(&current.agent_id))
            .unwrap();
        assert_eq!(identity::read(&repo).unwrap(), Some(current));
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), local_head);
        assert_eq!(
            std::fs::read_to_string(repo.root().join("local-only.txt")).unwrap(),
            "not pushed\n"
        );
    }

    #[test]
    fn a_copy_response_must_confirm_the_exact_fenced_source() {
        let hub = "https://hub.test";
        let source = RemoteIdentity::new(hub, "00000000-0000-0000-0000-000000000001").unwrap();
        let response = |forked_from: Option<&str>, agent_id: &str| crate::hub::PublishResponse {
            agent_id: agent_id.into(),
            forked_from: forked_from.map(str::to_string),
            owner: "me".into(),
            name: "photo".into(),
            push_url: "https://hub.test/me/photo.git".into(),
            web_url: "https://hub.test/@me/photo".into(),
        };

        let copy = validate_copy_response(
            hub,
            &response(
                Some(&source.agent_id),
                "00000000-0000-0000-0000-000000000002",
            ),
            &source,
        )
        .unwrap();
        assert_eq!(copy.agent_id, "00000000-0000-0000-0000-000000000002");
        assert!(
            validate_copy_response(
                hub,
                &response(None, "00000000-0000-0000-0000-000000000002"),
                &source,
            )
            .is_err(),
            "a legacy/malformed response cannot authorize local pinning"
        );
        assert!(
            validate_copy_response(
                hub,
                &response(
                    Some("00000000-0000-0000-0000-000000000003"),
                    "00000000-0000-0000-0000-000000000002",
                ),
                &source,
            )
            .is_err(),
            "a slug replacement between GET and POST must not attach its copy"
        );
        assert!(
            validate_copy_response(
                hub,
                &response(Some(&source.agent_id), &source.agent_id),
                &source
            )
            .is_err(),
            "the response must name a distinct copied Agent"
        );
    }

    // ──── Into the runtime: unwrap the VIEW, not the history ────

    use crate::domain::meta::{self, Meta};
    use crate::domain::repo::Repo;
    use crate::domain::transcript;

    fn claim() -> String {
        format!("{}{}", meta::ID_PREFIX, "c".repeat(meta::ID_HEX_LEN))
    }

    fn claude_user(text: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"s1\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n"
        )
    }

    fn claude_compact(text: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"isCompactSummary\":true,\"parentUuid\":null,\"sessionId\":\"s1\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n"
        )
    }

    /// What goes into the runtime is the VIEW unwrapped back to raw lines: not envelopes, and not
    /// the history from before the compact.
    ///
    /// This pins S5's core promise on the pickup side — what is installed back into the runtime
    /// matches the context the runtime resumes on its own: from the last compact boundary, as raw
    /// lines and not envelopes.
    #[test]
    fn whats_installed_is_the_view_unwrapped_back_to_raw_lines() {
        let sid = claim();
        let history = format!(
            "{}{}{}",
            claude_user("PRE-COMPACT"),
            claude_compact("SUMMARY"),
            claude_user("POST-COMPACT")
        );
        let view_slice = format!(
            "{}{}",
            claude_compact("SUMMARY"),
            claude_user("POST-COMPACT")
        );

        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        meta::ensure_session_dir(r.root()).unwrap();
        crate::domain::storage::write_snapshot(
            r.root(),
            &transcript::wrap_lines(&history, "claude-code", &sid),
            &transcript::wrap_lines(&view_slice, "claude-code", &sid),
        )
        .unwrap();
        meta::write(r.root(), &Meta::new(sid, "claude-code".into(), "/r".into())).unwrap();

        let text = super::view_text_for_install(r.root()).unwrap();
        assert!(
            !text.contains("_object_hash") && !text.contains("_session_id"),
            "installed content must not carry envelope keys: {text}"
        );
        assert!(
            !text.contains("PRE-COMPACT"),
            "pre-compact history must not enter the runtime: {text}"
        );
        assert!(
            text.contains("SUMMARY"),
            "the boundary (inclusive) belongs to the view: {text}"
        );
        assert!(
            text.contains("POST-COMPACT"),
            "lines after the boundary must be present: {text}"
        );
    }

    /// A missing VIEW, or one that unwraps to nothing, is an incomplete checkout: refuse to
    /// install and point at re-running clone/fetch.
    #[test]
    fn a_checkout_without_a_readable_view_is_refused_with_a_rerun_hint() {
        let d = tempfile::tempdir().unwrap();
        let e = super::view_text_for_install(d.path())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(meta::VIEW_FILE),
            "must name the missing file: {e}"
        );
        assert!(
            e.contains("clone") && e.contains("fetch"),
            "must offer a way out: {e}"
        );

        let d2 = tempfile::tempdir().unwrap();
        meta::ensure_session_dir(d2.path()).unwrap();
        std::fs::write(d2.path().join(meta::VIEW_FILE), "not an envelope at all\n").unwrap();
        let e2 = super::view_text_for_install(d2.path())
            .unwrap_err()
            .to_string();
        assert!(
            e2.contains("clone") && e2.contains("fetch"),
            "must offer a way out: {e2}"
        );
    }

    // ─────── The fetch must yield a usable checkout ───────

    fn commit_at(repo: &Repo, msg: &str, date: &str) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.root())
            // Tests run with no tty; if the host machine sets commit.gpgsign, signing always
            // fails.
            .args(["-c", "commit.gpgsign=false"])
            .args(["commit", "--allow-empty", "--quiet", "-m", msg])
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_DATE", date)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A "remote": the main file line plus two session branches.
    fn upstream_with_three_branches(tmp: &std::path::Path) -> PathBuf {
        let bare = tmp.join("upstream.git");
        std::process::Command::new("git")
            .args(["init", "--bare", "--quiet", "--initial-branch=main"])
            .arg(&bare)
            .output()
            .unwrap();
        let work = Repo::init(&tmp.join("work")).unwrap();
        meta::write(work.root(), &Meta::new_file_line()).unwrap();
        work.add_all().unwrap();
        commit_at(&work, "main", "2024-01-01T00:00:00Z");
        for (b, date) in [
            ("older", "2024-02-01T00:00:00Z"),
            ("newest", "2024-03-01T00:00:00Z"),
        ] {
            work.git(&["checkout", "--quiet", "-b", b, "main"]).unwrap();
            commit_at(&work, b, date);
        }
        work.git(&["push", "--quiet", &bare.to_string_lossy(), "--all"])
            .unwrap();
        bare
    }

    /// After a clone there must be **local** branches, each with an upstream.
    ///
    /// Without this the repo holds only remote-tracking refs and not one local branch, so
    /// `resume` / `run` / `push` all break and recovery means a manual `git branch X origin/X &&
    /// git checkout X`.
    #[test]
    fn every_remote_branch_becomes_a_local_branch_with_an_upstream() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = upstream_with_three_branches(tmp.path());
        let dest = tmp.path().join("clone");
        std::process::Command::new("git")
            .args(["clone", "--quiet"])
            .arg(&bare)
            .arg(&dest)
            .output()
            .unwrap();
        let repo = Repo::at(&dest);

        super::track_all_remote_branches(&repo);
        let mut heads = super::local_branches(&repo);
        heads.sort();
        assert_eq!(
            heads,
            ["main", "newest", "older"],
            "every remote branch must have a local counterpart"
        );
        for b in ["newest", "older"] {
            let up = repo
                .git(&["rev-parse", "--abbrev-ref", &format!("{b}@{{upstream}}")])
                .unwrap();
            assert_eq!(
                up.trim(),
                format!("origin/{b}"),
                "a local branch must track its remote"
            );
        }
        // Idempotent: a second run creates nothing again and does not error.
        assert!(super::track_all_remote_branches(&repo).is_empty());
    }

    /// Re-cloning with an explicit session branch: main is not behind while that branch is, so it
    /// is fast-forwarded before it opens; on divergence the local branch is left alone.
    #[test]
    fn an_explicit_session_branch_is_aligned_with_origin_before_it_opens() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = upstream_with_three_branches(tmp.path());
        let dest = tmp.path().join("clone");
        std::process::Command::new("git")
            .args(["clone", "--quiet"])
            .arg(&bare)
            .arg(&dest)
            .output()
            .unwrap();
        let repo = Repo::at(&dest);
        super::track_all_remote_branches(&repo);
        assert_eq!(
            super::sync_explicit_branch(&repo, "older").unwrap(),
            super::BranchSync::Current
        );

        // The remote's `older` moved on by one turn; main stays put.
        let work = Repo::at(tmp.path().join("work"));
        work.git(&["checkout", "--quiet", "older"]).unwrap();
        commit_at(&work, "older-2", "2024-04-01T00:00:00Z");
        work.git(&["push", "--quiet", &bare.to_string_lossy(), "older"])
            .unwrap();
        repo.git(&["fetch", "--quiet", "origin"]).unwrap();
        assert_eq!(repo.ahead_behind(), Some((0, 0)), "main itself is current");

        assert_eq!(
            super::sync_explicit_branch(&repo, "older").unwrap(),
            super::BranchSync::FastForwarded(1)
        );
        assert_eq!(
            repo.git(&["rev-parse", "refs/heads/older"]).unwrap(),
            repo.git(&["rev-parse", "refs/remotes/origin/older"])
                .unwrap()
        );

        // Each side grew one more commit: diverged, and the local branch stays put. A checkout
        // from `git clone` carries no committer identity, so supply one — a CI runner has no
        // global git identity.
        repo.ensure_committer().unwrap();
        repo.git(&["checkout", "--quiet", "older"]).unwrap();
        commit_at(&repo, "local-only", "2024-05-01T00:00:00Z");
        let local_tip = repo.git(&["rev-parse", "refs/heads/older"]).unwrap();
        commit_at(&work, "older-3", "2024-06-01T00:00:00Z");
        work.git(&["push", "--quiet", &bare.to_string_lossy(), "older"])
            .unwrap();
        repo.git(&["fetch", "--quiet", "origin"]).unwrap();
        assert_eq!(
            super::sync_explicit_branch(&repo, "older").unwrap(),
            super::BranchSync::Diverged {
                ahead: 1,
                behind: 1
            }
        );
        assert_eq!(
            repo.git(&["rev-parse", "refs/heads/older"]).unwrap(),
            local_tip
        );
    }

    /// HEAD lands on main (the trunk of team memory); a pure session agent with no main lands on
    /// the **most recently active** line — landing on whichever branch happens to sort first is as
    /// useless as a detached HEAD.
    #[test]
    fn head_lands_on_main_or_on_the_most_recently_active_line() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = upstream_with_three_branches(tmp.path());
        let dest = tmp.path().join("clone");
        std::process::Command::new("git")
            .args(["clone", "--quiet"])
            .arg(&bare)
            .arg(&dest)
            .output()
            .unwrap();
        let repo = Repo::at(&dest);
        super::track_all_remote_branches(&repo);

        assert_eq!(super::default_checkout(&repo).as_deref(), Some("main"));
        // A pure session agent (no file line): land on the most recently committed branch.
        repo.git(&["checkout", "--quiet", "newest"]).unwrap();
        repo.git(&["branch", "-D", "main"]).unwrap();
        assert_eq!(super::default_checkout(&repo).as_deref(), Some("newest"));
    }

    #[test]
    fn explicit_branch_or_tag_checkout_cannot_claim_ignored_v0_storage_names() {
        use crate::domain::meta::{self, LayoutVersion, Meta};
        use crate::domain::repo::Repo;

        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init(&tmp.path().join("repo")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut old_meta = Meta::new_file_line();
        old_meta.layout = LayoutVersion::V0;
        meta::write(repo.root(), &old_meta).unwrap();
        std::fs::write(repo.root().join(".gitignore"), "/LOG\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("v0 main").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let born = Meta::new_session_line("codex".into(), "/work".into());
        let tree = crate::commands::new::fresh_session_tree(
            &repo,
            old.trim(),
            &meta::to_text(&born).unwrap(),
        )
        .unwrap();
        let target =
            crate::commands::plumbing::commit_tree(&repo, &tree, &[old.trim()], "v1 target")
                .unwrap();
        repo.git(&["update-ref", "refs/remotes/origin/session", &target])
            .unwrap();
        repo.git(&["tag", "v1-point", &target]).unwrap();
        let malformed_tree = crate::commands::plumbing::tree_apply_owned(
            &repo,
            &target,
            vec![(meta::FILE.to_string(), None)],
        )
        .unwrap();
        let malformed = crate::commands::plumbing::commit_tree(
            &repo,
            &malformed_tree,
            &[old.trim()],
            "v1 namespace without meta",
        )
        .unwrap();
        repo.git(&["update-ref", "refs/remotes/origin/malformed", &malformed])
            .unwrap();

        let user_bytes = b"ignored user LOG\n";
        std::fs::write(repo.root().join(meta::LOG_FILE), user_bytes).unwrap();
        for (target_ref, reset) in [
            ("refs/remotes/origin/session", Some("session")),
            ("refs/tags/v1-point", None),
            ("refs/remotes/origin/malformed", Some("malformed")),
        ] {
            let error = checkout_target(&repo, target_ref, reset).unwrap_err();
            assert!(
                error.to_string().contains("user data") || error.to_string().contains("user-owned"),
                "{error:#}"
            );
            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
            assert_eq!(repo.current_branch().as_deref(), Some("main"));
            assert_eq!(
                std::fs::read(repo.root().join(meta::LOG_FILE)).unwrap(),
                user_bytes
            );
        }
    }

    // ─────────── One name and two checkouts ───────────

    fn co(owner: &str, name: &str) -> Checkout {
        Checkout {
            owner: owner.into(),
            name: name.into(),
            path: PathBuf::from(format!("/h/repos/{owner}/{name}")),
        }
    }

    /// A read-only checkout records versions into **its own** repo.
    ///
    /// Opening a separate empty repo under your name drops the original author's entire history:
    /// what push then sends up is a new root with no common ancestor, and every early snapshot the
    /// clone worked to bring down was fetched for nothing.
    #[test]
    fn recording_lands_in_the_only_checkout_even_when_it_is_someone_elses() {
        let ro = [co("alice", "photo")];
        assert_eq!(
            choose_for_recording("photo", &ro).unwrap(),
            Some(PathBuf::from("/h/repos/alice/photo"))
        );
        // None at all → the caller creates one under your name.
        assert_eq!(choose_for_recording("photo", &[]).unwrap(), None);
    }

    /// When your own and a read-only pickup share a name this **errors out** and never picks one.
    ///
    /// The two readings point at different lineages, and picking wrong records a stretch of work
    /// into the other chain with no symptom at the time — matching how every other ambiguity in
    /// this codebase is handled.
    #[test]
    fn two_checkouts_with_one_name_are_refused_not_guessed() {
        let both = [co("me", "photo"), co("alice", "photo")];
        let e = choose_for_recording("photo", &both)
            .unwrap_err()
            .to_string();
        assert!(e.contains("2 agents named `photo`"), "{e}");
        assert!(
            e.contains("me/photo") && e.contains("alice/photo"),
            "must list both: {e}"
        );
        assert!(e.contains("--name"), "must offer a way out: {e}");
    }
}
