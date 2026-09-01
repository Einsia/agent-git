//! `agit push [<owner/repo>@<branch>] [--all]` — publish a local repo to the hub.
//!
//! # It is `git push`
//!
//! Recording a version (`agit import -n <agent>` or `agit commit`) has already written the
//! content and the snapshot into `~/.agit/repos/<owner>/<name>/` and made a git commit. So this
//! command has three things left: make sure the remote exists, scan for secrets, push the
//! branches and the tags.
//!
//! **The local repo is the authoritative copy**, which makes re-running `agit push` an
//! idempotent retry. A `drafts/<agent>/` staging area — fetch the remote → reset to its tip →
//! copy the draft in → commit → tag → push, then **delete the draft** — costs two things: the
//! next commit loses its comparison base, and the hint "the tags did not go up, re-run agit
//! push" cannot be followed, because what would be re-pushed has been deleted.
//!
//! # What goes up is the **context branch**, not the branch the checkout sits on
//!
//! The branch comes from [`super::context::resolve`], the same source `commit` uses. Pushing
//! `repo.current_branch()` without ever asking the context publishes the wrong thing the moment
//! the two disagree — a rejected import leaves a ghost branch exactly where the checkout sits,
//! and push sends it to the hub. When the context does not resolve and there is more than one
//! branch, this **errors** instead of guessing (`-b` / `--all` say which one).
//!
//! `--all` publishes only the branches with **something new**: what gets published is selected,
//! the same way memory is, and an experiment branch is not broadcast along the way.
//!
//! # A version ID is a tag
//!
//! `refs/tags/agit-<40hex>` points at a commit. Two tags colliding on a name means the parent,
//! the cwd and the transcript bytes are all identical, which is the same state, so re-pushing is
//! a no-op.
//!
//! Only the tags **reachable from the pushed branches** go up. `--tags` broadcasts the version
//! IDs of every branch that was not selected too (an experiment line, somebody else's line
//! fetched down here), and a version ID is itself evidence that the content exists.
//!
//! # Visibility is settled at first publish
//!
//! On a tty it asks (and says the transcript is a complete work record); a non-interactive run
//! defaults to **private** — publishing a complete work transcript by default is a step that
//! cannot be undone, and in CI there is nobody to nod. After that, push **never changes
//! visibility**; it only reports the current value, and the person who most needs that line is
//! the one who passed nothing. Change it with `agit repo visibility`.
//!
//! # The secret scan is here
//!
//! Because this is the first time the content leaves this machine. The client-side gate can be
//! bypassed (patch the code, run `git push` directly), so the server scans too.
//!
//! **How much the server scans depends on the hub's version**: a newer hub scans only "the
//! moment the content becomes readable by a third party" (a push to a public agent, private
//! turning public, a PR carrying content into a public target); an older one scans on every
//! push. So the user-facing wording is always "the server **may** still refuse" — pinning it to
//! either behavior is wrong on the other hub, and the client and the hub are not guaranteed to
//! ship together.
//!
//! One thing holds on either hub, and it is what the user actually needs to know: what entered
//! history does not go away; it blocks you on the day you want to make this agent public.
//!
//! # In a read-only checkout it offers to promote, instead of hitting a wall
//!
//! The checkout `agit clone alice/photo` leaves behind (without `--mine`) has `origin` pointing
//! at alice's copy, and there is no pushing into it. `agit push` there can mean only one thing:
//! make it yours, then publish. So this **asks** and then does it, rather than raising an error
//! that says "go run another command first" — that command holds no decision for anyone to make.
//!
//! It does have a side effect (creating an agent under your namespace on the server), so:
//!
//! * Interactive: ask "create `<you>/photo` under your name?"; no means nothing happens.
//! * Non-interactive (CI, scripts): **error**, and give that command. A namespace write nobody
//!   nodded at does not belong in automation, and [`ui::prompt::confirm`] returning None off a
//!   tty means exactly that.
//!
//! The promotion itself is [`super::clone::promote`], the same code `agit clone --mine` uses —
//! the repo state the two paths produce must be identical.
//!
//! # The push gate does not verify signatures
//!
//! A signature is a display feature, not a push gate — write access is enough to push, the same
//! as GitHub. The server checks only content integrity (session/meta.json exists) and the
//! secret scan. Signature verification lives on the read path, as the "Verified" badge.

use super::{CmdResult, require_login};
use crate::domain::meta;
use crate::domain::repo::{self, Repo};
use crate::domain::secrets;
use crate::hub::PublishRequest;
use crate::hub::identity::{self, RemoteIdentity};
use crate::infra::{config, credentials};
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::path::Path;

#[derive(ClapArgs)]
pub struct Args {
    /// Target: `owner/repo@branch` (or legacy bare repo / `-b branch`). Omittable when the context resolves.
    ///
    /// The `owner/name` form is what the “several agents named X” error asks for — a bare
    /// name is ambiguous the moment this machine holds both your `payments` and a read-only
    /// checkout of somebody else’s.
    #[arg(value_name = "owner/repo@branch")]
    pub agent: Option<String>,

    /// Branch to publish (repeatable). Default: the context branch.
    #[arg(short = 'b', long, value_name = "branch")]
    pub branch: Vec<String>,

    /// Publish every branch the hub doesn’t already have in full.
    ///
    /// Deliberately not the default: publishing is selected, the same way memory is.
    /// An experiment branch shouldn’t get broadcast just because it was lying around.
    #[arg(long, conflicts_with = "branch")]
    pub all: bool,

    /// First publish only: only you and the authorized can read it.
    ///
    /// This is what a non-interactive run defaults to. Visibility is settled once, at
    /// first publish: `agit push` never changes an existing agent’s visibility —
    /// otherwise one fat-fingered push could flip what a teammate just made public.
    /// Change it later with `agit repo visibility <owner>/<name> <public|private>`.
    #[arg(long, conflicts_with = "public")]
    pub private: bool,

    /// First publish only: anyone can read it — including every transcript in it.
    #[arg(long)]
    pub public: bool,

    /// Run every local check (secret scan included) and print what would go up. Sends nothing.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(args: Args) -> CmdResult {
    if crate::rc::harness::settlement_is_delegated() {
        ui::error(crate::rc::harness::SUPERVISED_SETTLEMENT_MESSAGE);
        ui::hint("finish the turn and let agitd push it under the live identity lease");
        return Ok(ExitCode::Failure);
    }
    let client = require_login()?;
    let s = ui::theme::symbols();

    let Some(me) = credentials::current_user() else {
        ui::error("no account name in the stored credentials.");
        ui::hint("re-run `agit login`");
        return Ok(ExitCode::Failure);
    };

    // The context resolves once: it answers both "which repo" and "which branch", and both
    // answers must come from the same resolution — otherwise "repo from the context, branch
    // from the checkout" is a half-right, half-wrong combination.
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let ctx = super::context::resolve(&cwd).ok();

    // ── 1. Decide which agent to push ──
    //
    // The candidates include read-only checkouts (they sit under the source author's name), so
    // `agit push` has something to say inside such a repo, instead of reporting "no <you>/photo
    // on this machine" and sending the reader the wrong way.
    let (want_owner, agent, target_branch) = match &args.agent {
        Some(a) => match split_publish_target(a) {
            Ok(v) => v,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Usage);
            }
        },
        None => match ctx.as_ref().and_then(|c| c.owner_name().ok()) {
            // When the context speaks, follow it — the same source `agit commit` uses.
            Some((o, n)) => (Some(o), n, None),
            None => match local(&me)?.as_slice() {
                [only] => (Some(only.owner.clone()), only.name.clone(), None),
                [] => {
                    ui::error("no agents on this machine yet.");
                    ui::hint("adopt one first: `agit import <session-id> -n <name>`");
                    return Ok(ExitCode::Usage);
                }
                all => {
                    // Never guess between several — the wrong one publishes a session under
                    // another agent's name.
                    ui::error("several agents locally — name the one to push:");
                    for c in all {
                        println!("  {}", c.slug());
                    }
                    ui::hint("agit push <owner>/<agent>");
                    return Ok(ExitCode::Usage);
                }
            },
        },
    };

    let Some(checkout) = pick_checkout(&me, want_owner.as_deref(), &agent)? else {
        return Ok(ExitCode::Usage);
    };
    let Some(repo) = Repo::open(&checkout.path) else {
        ui::error(&format!("no local repo for {}.", checkout.slug()));
        ui::hint(&format!(
            "record a first version: `agit import <session-id> -n {agent}`"
        ));
        return Ok(ExitCode::Usage);
    };

    let snap = match meta::resolve(repo.root()) {
        Ok(v) => v,
        Err(e) => {
            ui::error(&format!(
                "no readable session metadata in {}: {e:#}",
                checkout.slug()
            ));
            ui::hint(&format!("record one first: `agit commit {agent}`"));
            return Ok(ExitCode::Failure);
        }
    };

    // ── 2. Local preconditions ──
    //
    // Every local judgement runs before the network is touched: with the backend down, a
    // "detached HEAD" hidden behind a connection failure is the hardest class of error to track
    // down.
    //
    // The context branch counts only when the context names **this** repo: running
    // `agit push notes` from a session in `me/payments` names a branch that belongs to another
    // repo.
    if target_branch.is_some() && (!args.branch.is_empty() || args.all) {
        ui::error("a branch in `<owner>/<repo>@<branch>` cannot be combined with `-b` or `--all`.");
        ui::hint("choose one target spelling, e.g. `agit push owner/repo@branch`");
        return Ok(ExitCode::Usage);
    }
    let explicit_branches = target_branch
        .into_iter()
        .chain(args.branch.iter().cloned())
        .collect::<Vec<_>>();
    let ctx_branch = ctx
        .as_ref()
        .filter(|c| c.repo == checkout.slug())
        .map(|c| c.branch.clone());
    let heads = repo.local_branches();
    let branches = match plan_branches(
        &explicit_branches,
        args.all,
        ctx_branch.as_deref(),
        &repo,
        &heads,
    ) {
        Ok(b) => b,
        Err(r) => {
            ui::error(&r.msg);
            for h in &r.hints {
                ui::hint(h);
            }
            return Ok(r.code);
        }
    };
    // A branch that was born but has not settled a single turn is not published. See
    // [`has_settled_turns`].
    //
    // **Only the inferred ones are skipped.** A branch the user named with `-b <branch>` is not
    // skipped quietly: that is the one they meant, and a silent skip that still exits 0 reads as
    // a successful publish. That case says why, and ends in failure.
    let asked_explicitly = !args.branch.is_empty();
    let (branches, unsettled): (Vec<String>, Vec<String>) = branches
        .into_iter()
        .partition(|b| has_settled_turns(&repo, b));
    if !unsettled.is_empty() {
        if asked_explicitly {
            ui::error(&format!(
                "`{}` has no settled turns yet — there is nothing to publish on it.",
                unsettled.join("`, `")
            ));
            ui::hint("`agit commit` records the conversation so far, then push");
            return Ok(ExitCode::Precondition);
        }
        println!(
            "{}",
            ui::dim(&format!(
                "  skipping {} (claimed, nothing settled onto it yet)",
                unsettled.join(", ")
            ))
        );
    }
    if branches.is_empty() {
        println!("nothing to publish yet — no turns have been settled.");
        ui::hint("`agit commit` records the conversation so far, then push");
        return Ok(ExitCode::Ok);
    }

    // ── 3. Secret scan ──
    //
    // What is scanned is exactly "the bytes about to leave this machine". Only the destination
    // answers "which bytes" — see [`super::publish_destination`]. That `ls-remote` is read-only
    // and falls back to a full scan when it fails, so `--dry-run` goes through it too: a
    // rehearsal whose verdict differs from the real push is worth nothing.
    //
    // This does **not** open its own check for "a read-only checkout whose `origin` points at
    // the source author". That check lives in [`super::publish_destination`] (`lands_on`), and
    // can only live there: `agit scan` and the `--dry-run` early return below do not pass
    // through this layer, and three copies of one judgement sooner or later become two.
    //
    // The third field of the destination identity (which agent to publish to) comes from **this
    // checkout**, not from reading `origin` backwards — that is letting the suspect URL vouch
    // for itself. `checkout.name` is the name `ensure_remote` below creates or fetches (a
    // read-only promotion swaps only the owner, see `target` in `promote_if_read_only`), so what
    // is asked here and what is done there are the same destination.
    let asked_url = repo.remote_url();
    let dest = super::publish_destination(&repo, &checkout.name);
    // Whether the destination narrowed the scan surface. Only a narrowed pass has to be redone
    // after the destination changes.
    let narrowed = dest.narrows();
    if let Gate::Blocked(code) = secret_gate(&repo, &secrets::ScanPlan::to(dest))? {
        return Ok(code);
    }

    // ── 4. --dry-run stops here ──
    //
    // "Run every local judgement, send not one **write**": the read-only `get_agent` query is
    // not sent either — it changes nothing, but one failure ends the whole rehearsal in a
    // network error, while everything it would tell you is available locally.
    //
    // The `ls-remote` above is the exception under the same standard, because it **cannot** fail
    // the rehearsal: no answer means falling back to a full scan
    // ([`super::publish_destination`]). Without it, a rehearsal's verdict can differ from the
    // real push's, and that verdict is this command's only purpose.
    //
    // This return sits **before** the read-only promotion in step 5, so the `origin` a rehearsal
    // sees is always the one from before the promotion. In a read-only checkout that is the
    // source author's copy, not this push's destination — so the scan surface falls back to full
    // in such a repo; the test is in [`super::publish_destination`].
    // Reminder before publishing: memory on the session branch that has not been distilled into
    // main does not travel with main.
    for branch in &branches {
        super::memory::remind_pending(&repo, branch);
    }
    if args.dry_run {
        return Ok(dry_run(&repo, &checkout, &me, &branches, &args));
    }

    // ── 5. Read-only checkout: offer to create a copy under your name ──
    //
    // **After** every local judgement: a promotion creates an agent under your namespace on the
    // server, and a repo with no snapshot, or one whose scanned secrets block the push, must
    // never produce that write. **Before** the network steps below: a promotion moves the
    // directory, and every step after it holds the repo's path.
    // The destination's namespace: without a promotion it is the checkout's own (my name, or an
    // organization I belong to); after one it is my name. Remote lookup and creation both follow
    // it — looking for an organization repo under "me" finds nothing.
    let (repo, agent, namespace) = match promote_if_read_only(&client, &me, &checkout, &repo)? {
        Promotion::Ready => (repo, agent, checkout.owner.clone()),
        Promotion::Promoted { repo, name } => (repo, name, me.to_string()),
        Promotion::Refused(code) => return Ok(code),
    };

    // ── 6. Make sure the remote agent exists ──
    let wanted = wanted_visibility(&args, &repo).value();
    let remote = ensure_remote(&client, &namespace, &agent, wanted, &snap, &repo)?;
    let Remote {
        owner,
        name,
        push_url,
        visibility,
        created,
    } = remote;

    // `--private` / `--public` do nothing to an agent that already exists, and that has to be
    // said: passing the flag means believing this push changed who can read it.
    if !created
        && let Some(want) = wanted
        && want != (visibility == "public")
    {
        let asked = if want { "--public" } else { "--private" };
        ui::warning(&format!(
            "{owner}/{name} is already {visibility} on the hub — {asked} did nothing."
        ));
        ui::hint(&format!(
            "visibility is settled at first publish; change it with `agit repo visibility {owner}/{name} {}`",
            if want { "public" } else { "private" }
        ));
    }
    repo.set_remote(&push_url)?;

    // ── 6b. Did the destination change after the scan ──
    //
    // Step 3 asked about the `origin` **as it was then**. Visibility settles only at
    // `ensure_remote`, and that step may create a brand-new empty repo along the way
    // (`created`), or point `origin` somewhere else (`push_url` changed). In both cases what
    // step 3 asked about is not this push's far side, and the difference that pass computed does
    // not count — what goes out this time is the **full history**, so it is rescanned in full.
    //
    // # Why the order cannot be reversed (hoisting `ensure_remote` above the scan)
    //
    // Because it writes: a missing agent gets `publish`ed into your namespace, and the client
    // decides visibility at exactly that moment. A repo whose scan finds secrets and cannot be
    // pushed at all must not first leave a record on the server and ask a "public or private"
    // question along the way. The promotion in step 5 is the same.
    //
    // So the scan stays ahead of the write actions, and the cost is that step 3 can only ask
    // about "the origin as it was then"; this step closes that gap. `narrowed` is the necessary
    // guard: when step 3 scanned in full anyway, a changed destination misses nothing and
    // rescanning only burns time.
    if narrowed && (created || asked_url.as_deref() != Some(push_url.as_str())) {
        ui::warning(&format!(
            "the destination changed while preparing this push ({}) — re-checking the full history.",
            if created {
                "a new agent was created on the hub"
            } else {
                "origin now points somewhere else"
            }
        ));
        if let Gate::Blocked(code) = secret_gate(&repo, &secrets::ScanPlan::full())? {
            return Ok(code);
        }
    }

    // ── 7. Push branches and tags ──
    let mut git_args = vec!["push"];
    if repo.ahead_behind().is_none() {
        git_args.push("-u");
    }
    git_args.push("origin");
    let refs = refs_to_push(&branches, repo.has_ref("refs/heads/main"));
    git_args.extend(refs.iter().map(String::as_str));
    let out = crate::hub::git::run(&repo, &git_args)?;
    if !out.ok() {
        ui::error("pushing the branch failed.");
        for line in diagnose(&out, &owner, &name) {
            ui::hint(&line);
        }
        ui::hint(&format!(
            "remote: {}",
            crate::hub::git::redact_url(&push_url)
        ));
        return Ok(ExitCode::Failure);
    }

    // Tags are pushed separately: `git push <branch>` carries no tags, and a tag is the version
    // ID.
    //
    // **Every** tag reachable from these branches goes up, not just the one on HEAD: several
    // commits and then one push is the natural usage, and pushing only the last one leaves the
    // snapshots in between with no version ID on the hub to point at. A tag name is derived from
    // the content, so "same name, different value" cannot happen and re-pushing is a safe
    // idempotent operation.
    let tags = tags_to_push(&repo, &refs);
    if let Err(out) = push_tags(&repo, &tags) {
        ui::warning("branches pushed, but version tags didn’t go up.");
        for line in diagnose(&out, &owner, &name) {
            ui::hint(&line);
        }
        // The local repo is still where it was, so this hint can be followed.
        ui::hint("re-run `agit push` to retry (the local repo is kept)");
    }

    // ── 8. Report ──
    //
    // The `@` before the owner is not optional: the web interface treats `@<owner>` as a
    // namespace, which is what separates it syntactically from reserved paths like `/login` and
    // `/settings`. One character short is a link that 404s, and this link is precisely what the
    // user sends a teammate.
    let web = format!("{}/@{owner}/{name}", client.base());
    // A version is HEAD's commit SHA (with the `agit-` prefix).
    let id = repo
        .git_opt(&["rev-parse", "HEAD"])
        .map(|sha| meta::id_from_sha(sha.trim()))
        .unwrap_or_default();
    println!(
        "\n{} {} {}",
        ui::ok(s.check),
        ui::bold(&format!("{owner}/{name}")),
        id
    );
    let mut kv: Vec<(&str, String)> = vec![("branches", refs.join(", "))];
    if !tags.is_empty() {
        kv.push(("versions", tags.len().to_string()));
    }
    if let Some(c) = &snap.code {
        kv.push(("code", c.clone()));
    }
    // Every push reports visibility, even when this one changed nothing. The person who most
    // needs this line is the one who passed nothing — they do not know what they published.
    kv.push(("visibility", visibility_label(&visibility).to_string()));
    kv.push(("link", web));
    print!("{}", ui::table::key_values(&kv));

    // Warn only on the push that created it. Shouting it on every push afterwards teaches the
    // reader to stop reading it.
    if created && visibility == "public" {
        ui::warning("this agent is public — anyone can read its full transcripts.");
        ui::hint(&format!(
            "back to private: agit repo visibility {owner}/{name} private"
        ));
    }
    println!(
        "{}",
        ui::dim(&format!("  teammates: agit clone {owner}/{name}"))
    );
    super::upgrade::maybe_nudge();
    Ok(ExitCode::Ok)
}

/// `--dry-run`: list what would go up, unchanged, and send not one byte.
fn dry_run(
    repo: &Repo,
    checkout: &super::clone::Checkout,
    me: &str,
    branches: &[String],
    args: &Args,
) -> ExitCode {
    let refs = refs_to_push(branches, repo.has_ref("refs/heads/main"));
    let tags = tags_to_push(repo, &refs);

    println!("\n{}", ui::bold("dry run — nothing left this machine"));
    let mut kv: Vec<(&str, String)> = vec![
        ("repo", checkout.slug()),
        ("branches", refs.join(", ")),
        ("versions", tags.len().to_string()),
    ];
    kv.push((
        "remote",
        repo.remote("origin")
            .map(|u| crate::hub::git::redact_url(&u))
            .unwrap_or_else(|| "none yet (a real push would create it)".into()),
    ));
    kv.push((
        "visibility",
        match wanted_visibility(args, repo) {
            Wanted::Flag(v) => format!("{} (--{0}, first publish only)", visibility_word(v)),
            Wanted::Repo(v) => format!(
                "{} (this repo’s preference from `agit init --private`, first publish only)",
                visibility_word(v)
            ),
            Wanted::Global(v) => format!(
                "{} (config push.visibility, first publish only)",
                visibility_word(v)
            ),
            Wanted::Ask => "asked at first publish; unchanged after that".into(),
        },
    ));
    print!("{}", ui::table::key_values(&kv));

    // A real push from a read-only checkout first asks "create a copy under your name?" — the
    // rehearsal must say so, or the user reads this push as landing on `alice/photo`.
    if is_read_only(me, &checkout.owner, repo.upstream_url().as_deref()) {
        ui::warning(&format!(
            "{} belongs to {} — a real push asks the hub whether you may write to it; if not, it would offer to create {me}/{} under your name.",
            checkout.slug(),
            checkout.owner,
            checkout.name
        ));
    }
    ExitCode::Ok
}

/// Normalize the unified human-facing target.  The old bare repo spelling is
/// still accepted; `@branch` selects exactly one branch for this push.
fn split_publish_target(arg: &str) -> crate::Result<(Option<String>, String, Option<String>)> {
    let parsed = crate::commands::target::parse(arg)?;
    if parsed.tail != crate::domain::refs::Tail::None {
        anyhow::bail!("push accepts a branch target, not a historic selector: `{arg}`");
    }
    // Legacy bare agent name (`agit push payments`) is parsed by refs as a
    // context-local base rather than a repository selector.
    if parsed.repo.is_none()
        && parsed.tail == crate::domain::refs::Tail::None
        && let Some(ref name) = parsed.base
        && name != "@"
    {
        repo::valid_name(name)?;
        return Ok((None, name.clone(), None));
    }
    let (owner, name) = match parsed.repo {
        Some(repo) if repo.contains('/') => {
            let (owner, name) = super::parse_slug(&repo)?;
            super::canonical_owner(&owner)?;
            (Some(owner), name)
        }
        Some(name) => (None, name),
        None => anyhow::bail!("push target must name a repository"),
    };
    repo::valid_name(&name)?;
    let branch = match parsed.base.as_deref() {
        None => None,
        Some("@") => anyhow::bail!("push target must name a branch explicitly"),
        Some(branch) => Some(branch.to_string()),
    };
    Ok((owner, name, branch))
}

/// Where the first-publish visibility comes from. Priority: command-line flag > repo preference
/// (`agit init --private`) > global `push.visibility` > ask at first publish. The closer a
/// statement is to this one push, the more it weighs: a one-off flag beats the preference the
/// repo recorded, and the repo's preference beats a default set for every repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    Flag(bool),
    Repo(bool),
    Global(bool),
    Ask,
}

impl Wanted {
    /// `Some(true)` public, `Some(false)` private, `None` ask at first publish.
    fn value(self) -> Option<bool> {
        match self {
            Wanted::Flag(v) | Wanted::Repo(v) | Wanted::Global(v) => Some(v),
            Wanted::Ask => None,
        }
    }
}

fn wanted_visibility(args: &Args, repo: &Repo) -> Wanted {
    let flag = if args.public {
        Some(true)
    } else if args.private {
        Some(false)
    } else {
        None
    };
    resolve_wanted(
        flag,
        repo.visibility_preference().as_deref(),
        super::config::get("push.visibility").as_deref(),
    )
}

fn resolve_wanted(
    flag: Option<bool>,
    repo_pref: Option<&str>,
    global_pref: Option<&str>,
) -> Wanted {
    if let Some(v) = flag {
        return Wanted::Flag(v);
    }
    if let Some(v) = parse_visibility(repo_pref) {
        return Wanted::Repo(v);
    }
    if let Some(v) = parse_visibility(global_pref) {
        return Wanted::Global(v);
    }
    Wanted::Ask
}

/// Anything but `public` / `private` (including `ask`, a typo, unset) means "not said".
fn parse_visibility(pref: Option<&str>) -> Option<bool> {
    match pref.map(str::trim) {
        Some("public") => Some(true),
        Some("private") => Some(false),
        _ => None,
    }
}

fn visibility_word(public: bool) -> &'static str {
    if public { "public" } else { "private" }
}

/// Visibility at first publish. `answer` is the result of the interactive question; `None` means
/// there was nobody to ask.
///
/// A non-interactive run defaults to **private**: publishing a complete work transcript cannot
/// be undone, and in CI there is nobody to nod for it. A default in the other direction
/// (`public: !private`) reads "said nothing" as "publish all my transcripts".
fn first_visibility(want: Option<bool>, answer: Option<bool>) -> bool {
    match (want, answer) {
        (Some(v), _) => v,
        (None, Some(v)) => v,
        (None, None) => false,
    }
}

/// Visibility for a first publish under an organization namespace.
///
/// Organization agents on the hub are always public. `--private`, and a private preference from
/// the repo or the global config, are refused here rather than sent as a creation request bound
/// to be rejected; saying nothing means public, with no question — asking a question that has
/// only one answer wastes the user's time.
fn org_first_visibility(owner: &str, agent: &str, want: Option<bool>) -> crate::Result<bool> {
    match want {
        Some(false) => anyhow::bail!(
            "{owner}/{agent}: organization repos are public on the hub — drop `--private` (or the private preference) to publish it"
        ),
        Some(true) => Ok(true),
        None => {
            println!(
                "{}",
                ui::dim(&format!(
                    "  {owner}/{agent} publishes public: organization repos are public on the hub"
                ))
            );
            Ok(true)
        }
    }
}

/// Ask once at first publish. Off a tty this returns `None`.
fn ask_visibility(agent: &str) -> crate::Result<Option<bool>> {
    println!(
        "{} isn’t on the hub yet — this first push settles who can read it.",
        ui::bold(agent)
    );
    println!(
        "{}",
        ui::dim(
            "  a session is a complete work record: every prompt, every tool call, every file it read."
        )
    );
    ui::prompt::confirm(
        "make it public (anyone can read the full transcripts)?",
        false,
    )
}

/// Give **one next step that can be followed** when the push fails.
///
/// git reports every server-side rejection as exit code 128, and the response body is out of
/// reach (`remote-curl` sets `CURLOPT_FAILONERROR`). So triage runs off the status code git
/// printed, and a 422 takes one more question to the backend before it can say which gate
/// stopped it.
///
/// Unconditional "the remote moved ahead" plus "authentication problem" advice points the wrong
/// way when the real cause is an unregistered public key, and a next step aimed the wrong way is
/// the most expensive thing to hand someone during a first end-to-end run.
fn diagnose(out: &crate::hub::git::Outcome, owner: &str, name: &str) -> Vec<String> {
    let err = &out.stderr;

    match out.http_status() {
        // 422 = the server-side gate refused the content (secret scan, provenance check, a
        // branch swapping sessions). Signature verification is not a gate.
        Some(422) => vec![
            "the server rejected the content (HTTP 422: secret scan or provenance check)".into(),
            "the server-side gate has no bypass — what it stopped would be irreversible inside shared history".into(),
            "ask the hub admin to check the agent.push.rejected audit entries for what matched".into(),
        ],
        // 413 = over quota. git cannot reach the "used this much, the cap is this" line in the
        // response body, so this at least gets the cause right — falling into the backstop gives
        // "git's own words", and git's own words are one status code.
        Some(413) => vec![
            "storage quota exceeded (private agents have a hard cap; public ones don’t)".into(),
            format!("check {owner}/{name}’s usage on the site, or make it public"),
        ],
        Some(409) => vec![
            "the remote history is incompatible with what you’re writing (published refs can’t be rewritten or deleted)".into(),
            format!("fetch first: agit clone {owner}/{name}, then commit"),
        ],
        Some(412) => vec![
            "the remote identity no longer matches this checkout; the agent name may have been deleted and reused".into(),
            format!("nothing was written — clone {owner}/{name} into a fresh checkout to inspect the current agent"),
        ],
        Some(428) => vec![
            "the hub requires an immutable agent identity, but this checkout did not provide one".into(),
            format!("re-clone it with this CLI: agit clone {owner}/{name}"),
        ],
        Some(401) => vec!["credentials expired: agit login".into()],
        Some(403) => vec![format!("no write access: the owner of {owner}/{name} must add you as a write collaborator")],
        Some(404) => vec![format!(
            "can’t find {owner}/{name} — it may not exist, or you lack access (deliberately indistinguishable)"
        )],
        Some(s) if s >= 500 => vec![format!("backend error (HTTP {s}) — try again later")],
        // No status code means a local or protocol-level failure, where git's own line is the
        // most accurate.
        _ if err.contains("fetch first") || err.contains("non-fast-forward") => vec![
            format!("the remote moved ahead: `agit clone {owner}/{name}` to catch up, then continue"),
        ],
        _ => vec![
            "git’s own words above are the best clue".into(),
            "`agit doctor --check-backend` checks connectivity".into(),
        ],
    }
}

/// Every agent checkout on this machine, yours first.
///
/// Read-only checkouts are included (they sit under the source author's name): with the agent
/// name omitted and one read-only checkout on this machine, `agit push` speaks about that one
/// instead of reporting "no agents on this machine yet".
fn local(owner: &str) -> crate::Result<Vec<super::clone::Checkout>> {
    let mut out: Vec<super::clone::Checkout> = super::clone::list_local()?
        .into_iter()
        .map(|(o, n, path)| super::clone::Checkout {
            owner: o,
            name: n,
            path,
        })
        .collect();
    out.sort_by_key(|c| (c.owner != owner, c.slug()));
    Ok(out)
}

/// Pick the checkout by name. `None` means the reason has already been said.
///
/// `want_owner` is the owner spelled out in the positional argument
/// (`agit push alice/payments`) — exactly what the ambiguity hint below asks for, so once it is
/// given the question is not asked a second time.
fn pick_checkout(
    me: &str,
    want_owner: Option<&str>,
    agent: &str,
) -> crate::Result<Option<super::clone::Checkout>> {
    let mut found = super::clone::checkouts_named(me, agent)?;
    if let Some(o) = want_owner {
        found.retain(|c| c.owner == o);
        // None found still falls through: the `Repo::open` error is closer to the facts (record
        // something with `agit import` first), and inventing one here only adds a layer of
        // retelling.
        if found.is_empty() {
            return Ok(Some(super::clone::Checkout {
                owner: o.to_string(),
                name: agent.to_string(),
                path: config::repo_dir(o, agent)?,
            }));
        }
    }
    match found.as_slice() {
        [] => Ok(Some(super::clone::Checkout {
            owner: me.to_string(),
            name: agent.to_string(),
            path: config::repo_dir(me, agent)?,
        })),
        [only] => Ok(Some(only.clone())),
        many => {
            // Your own and a read-only pickup share the name; the wrong one publishes the
            // content under another agent's name.
            ui::error(&format!(
                "this machine has {} agents named `{agent}`:",
                many.len()
            ));
            for c in many {
                println!("  {}  {}", c.slug(), ui::dim(&ui::tilde(&c.path)));
            }
            ui::hint(&format!("name it in full: agit push <owner>/{agent}"));
            Ok(None)
        }
    }
}

/// Is this checkout read-only (nothing can be pushed to it).
///
/// **Both** conditions have to hold. The second is not optional: a copy that was already
/// promoted sits in your namespace with its `upstream` pointing at somebody else's copy — the
/// first condition alone settles that one, while going by "does it hold somebody else's address"
/// alone marks it read-only too, and every push then asks one more question.
pub(crate) fn is_read_only(me: &str, checkout_owner: &str, upstream: Option<&str>) -> bool {
    checkout_owner != me && upstream.is_none()
}

/// What happened to a read-only checkout.
enum Promotion {
    /// This one is pushable as it stands.
    Ready,
    /// Just promoted to yours; the repo moved to a new location.
    Promoted { repo: Repo, name: String },
    /// The reason has already been said; exit with this code.
    Refused(ExitCode),
}

/// In a read-only checkout, offer to promote it to a copy under your name.
///
/// The test is that **both** hold: the agent `origin` names is not yours, and there is no
/// `upstream`. The second is not optional — a copy that was already promoted has its upstream
/// pointing at somebody else's copy, and going by "does it hold somebody else's address" alone
/// marks it read-only too.
fn promote_if_read_only(
    client: &crate::hub::Client,
    owner: &str,
    checkout: &super::clone::Checkout,
    repo: &Repo,
) -> crate::Result<Promotion> {
    if !is_read_only(owner, &checkout.owner, repo.upstream_url().as_deref()) {
        return Ok(Promotion::Ready);
    }
    // Somebody else's name: ask the hub's write-access gate (an organization owner, a team
    // member granted access to this agent). Write access pushes straight through with no
    // promoted copy; no answer is an error — treating a timeout as a refusal offers a copy.
    if matches!(
        super::writability(owner, &checkout.owner, &checkout.name)?,
        super::Writability::Granted | super::Writability::Creatable
    ) {
        return Ok(Promotion::Ready);
    }

    let src = format!("{}/{}", checkout.owner, checkout.name);
    let target = format!("{owner}/{}", checkout.name);
    println!(
        "{} belongs to {} — you can’t push to it.",
        ui::bold(&src),
        ui::bold(&checkout.owner)
    );

    match ui::prompt::confirm(
        &format!("create {target} under your name and publish it?"),
        true,
    )? {
        Some(true) => {}
        Some(false) => {
            println!(
                "{}",
                ui::dim("  nothing published; every local version is still here.")
            );
            ui::hint(&format!("when you mean it: agit clone {src} --mine"));
            return Ok(Promotion::Refused(ExitCode::Ok));
        }
        // A non-interactive run touches nobody's namespace, not even your own — it hands over
        // the command to run.
        None => {
            ui::error(&format!(
                "can’t push to {src}, and there’s no TTY here to ask about making your own copy."
            ));
            ui::hint(&format!(
                "agit clone {src} --mine   # make the copy, wire origin/upstream"
            ));
            ui::hint("then publish with `agit push`");
            return Ok(Promotion::Refused(ExitCode::Usage));
        }
    }

    let source = client.get_agent(&checkout.owner, &checkout.name)?;
    let plan = super::clone::promote(client, &checkout.path, &source, None)?;
    Ok(Promotion::Promoted {
        repo: Repo::at(config::repo_dir(&plan.owner, &plan.name)?),
        name: plan.name,
    })
}

/// Where the remote agent lands.
struct Remote {
    owner: String,
    name: String,
    push_url: String,
    /// Visibility as the server records it: `public` or `private`.
    visibility: String,
    /// Created by this call. The client decides visibility only at that moment, so "was it just
    /// created" decides whether `--private` took effect and whether the public warning prints.
    created: bool,
}

/// Make sure the remote has this agent.
fn ensure_remote(
    client: &crate::hub::Client,
    owner: &str,
    agent: &str,
    want: Option<bool>,
    snap: &meta::Meta,
    repo: &Repo,
) -> crate::Result<Remote> {
    let pinned = identity::read(repo)?;
    if pinned.is_some() {
        // Refuse an `AGIT_HUB_URL` site swap first, then query by slug. An existing checkout's
        // pin is the only trustworthy identity; the current slug proves only who it routed to,
        // and cannot rewrite the pin the other way.
        let pinned = identity::require_current_expected(repo, client.base())?;
        let remote = client.get_agent(owner, agent).map_err(|e| {
            anyhow::anyhow!(
                "the checkout is pinned to agent {}, but {owner}/{agent} is unavailable; refusing to create a replacement at the same name: {e:#}",
                pinned.agent_id
            )
        })?;
        let observed = RemoteIdentity::new(client.base(), &remote.agent_id)?;
        if observed != pinned {
            anyhow::bail!(
                "{owner}/{agent} now identifies agent {}, but this checkout is pinned to {}; the name was reused, so nothing was pushed",
                observed.agent_id,
                pinned.agent_id
            );
        }
        return Ok(Remote {
            owner: remote.owner,
            name: remote.name,
            push_url: remote.clone_url,
            visibility: remote.visibility,
            created: false,
        });
    }

    // A legacy checkout leaves only an origin URL. It cannot prove the URL still points at the
    // object that was cloned then, so an `agent_id` is never adopted silently from one current
    // GET.
    if repo.remote_url().is_some() {
        match client.get_agent(owner, agent) {
            Ok(remote) => {
                let observed = RemoteIdentity::new(client.base(), &remote.agent_id)?;
                anyhow::bail!(
                    "this legacy checkout has no immutable remote identity, so {owner}/{agent} cannot be adopted from its reusable name.
  verify the current agent ID, then preserve every local branch and unpushed commit with:
    agit clone {owner}/{agent} --adopt-legacy-agent-id {}",
                    observed.agent_id
                );
            }
            Err(e)
                if e.downcast_ref::<crate::hub::client::ApiError>()
                    .is_some_and(|api| api.status == 404) =>
            {
                anyhow::bail!(
                    "this legacy checkout has no immutable remote identity and {owner}/{agent} no longer exists; refusing to create or adopt a same-name replacement"
                );
            }
            Err(e) => return Err(e),
        }
    }

    // When a remote of the same name already exists, nothing local proves this repo belongs to
    // it. Only an explicit 404 enters creation; a network error, a 5xx, a 401 all fail
    // unchanged, and must never be read as "does not exist" and turned into a POST.
    match client.get_agent(owner, agent) {
        Ok(remote) => anyhow::bail!(
            "{owner}/{agent} already exists as agent {}, but this local repo has no identity pin; clone that remote instead of silently adopting it.
  an unpinned repo of this name usually means `agit init` ran against an already-created remote — move the local repo aside, then `agit clone {owner}/{agent}`",
            remote.agent_id
        ),
        Err(e)
            if e.downcast_ref::<crate::hub::client::ApiError>()
                .is_some_and(|api| api.status == 404) => {}
        Err(e) => return Err(e),
    }

    // Create it when it does not exist. Visibility settles at this moment, so this is the only
    // moment it is asked. A repo under an organization is not asked: the hub makes them public.
    let mine = crate::infra::credentials::current_user().as_deref() == Some(owner);
    let public = if mine {
        match want {
            Some(v) => v,
            None => first_visibility(None, ask_visibility(agent)?),
        }
    } else {
        org_first_visibility(owner, agent, want)?
    };
    // The repo origin lets the server look up "which agents have worked in this repo".
    let origins: Vec<String> = snap
        .code
        .as_deref()
        .and_then(code_origin)
        .into_iter()
        .collect();

    println!(
        "publishing {} to {}…",
        ui::bold(agent),
        ui::accent(client.base())
    );
    // An organization repo names the organization to create it under; your own passes nothing,
    // and the server defaults to the caller.
    let resp = client.publish(&PublishRequest {
        name: agent.to_string(),
        owner: (!mine).then(|| owner.to_string()),
        public,
        repo_origins: origins,
    })?;
    let remote_identity = RemoteIdentity::new(client.base(), &resp.agent_id)?;
    identity::pin(repo, &remote_identity)?;
    Ok(Remote {
        owner: resp.owner,
        name: resp.name,
        push_url: resp.push_url,
        visibility: if public { "public" } else { "private" }.to_string(),
        created: true,
    })
}

/// The human-facing spelling of visibility. Another level from the server (organization-visible,
/// say) passes through unchanged rather than being guessed at.
fn visibility_label(visibility: &str) -> &str {
    visibility
}

/// Branches with something new: the ones the remote does not have, or the ones local is ahead
/// on.
///
/// The scope of `--all`. The difference between "every branch" and "every branch with something
/// new" shows up on a re-run: the first reports a pile of up-to-date refs to the server every
/// time, the second says only what is true.
fn updated_branches(repo: &Repo, heads: &[String]) -> Vec<String> {
    heads
        .iter()
        .filter(|b| {
            if !repo.has_ref(&format!("refs/remotes/origin/{b}")) {
                return true;
            }
            repo.git_opt(&["rev-list", "--count", &format!("origin/{b}..{b}")])
                .and_then(|s| s.trim().parse::<usize>().ok())
                .is_none_or(|n| n > 0)
        })
        .cloned()
        .collect()
}

/// What to tell the user when this cannot go on.
#[derive(Debug)]
struct Refusal {
    msg: String,
    hints: Vec<String>,
    code: ExitCode,
}

/// Whether anything on this branch has been settled.
///
/// The branch and its `agit: claim session line` commit are created when the session is
/// **born**, while the identity (`session`) is claimed only at the first settlement — two things
/// that happen at different moments. So a legitimate intermediate state exists: a session
/// interrupted before it finished a sentence (or opened and closed again) leaves a branch
/// holding only that claim commit, declaring itself a session line while having no identity yet.
///
/// The server's provenance check (correctly) refuses such a tip, and it returns HTTP 422 — a
/// line that reads like "your content is wrong" while it only means "there is nothing here
/// yet". So it is filtered out locally: having nothing to publish is not an error.
///
/// The file line (main) is always pushable — it never claims a session, and the test does not
/// apply to it.
fn has_settled_turns(repo: &Repo, branch: &str) -> bool {
    match meta::read_at_ref(repo, &format!("refs/heads/{branch}")) {
        Some(m) => !m.is_session_line() || meta::is_bare_id(&m.session),
        // A branch with no readable meta is left to the server — nothing local judges for it.
        None => true,
    }
}

/// Decide which branches to push.
///
/// The order is the priority: explicit `-b` → `--all` → the context branch → the only branch in
/// the repo. Past that last rung it **does not guess**: with no resolvable context and several
/// branches, any way of picking can publish an experiment line (or a ghost branch left behind by
/// a rejected import) to the hub.
fn plan_branches(
    explicit: &[String],
    all: bool,
    ctx_branch: Option<&str>,
    repo: &Repo,
    heads: &[String],
) -> std::result::Result<Vec<String>, Refusal> {
    if !explicit.is_empty() {
        let unknown: Vec<&str> = explicit
            .iter()
            .filter(|b| !heads.iter().any(|h| h == *b))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(Refusal {
                msg: format!("no local branch named `{}`.", unknown.join("`, `")),
                hints: vec![format!("this repo has: {}", heads.join(", "))],
                code: ExitCode::Ref,
            });
        }
        let mut out: Vec<String> = Vec::new();
        for b in explicit {
            if !out.contains(b) {
                out.push(b.clone());
            }
        }
        return Ok(out);
    }

    if all {
        let updated = updated_branches(repo, heads);
        if updated.is_empty() {
            return Err(Refusal {
                msg: "every branch is already on the hub — nothing to push.".into(),
                hints: vec!["record new turns first: `agit commit`".into()],
                code: ExitCode::Ok,
            });
        }
        return Ok(updated);
    }

    if let Some(b) = ctx_branch {
        if heads.iter().any(|h| h == b) {
            return Ok(vec![b.to_string()]);
        }
        return Err(Refusal {
            msg: format!("the context branch `{b}` doesn’t exist in this repo."),
            hints: vec![
                format!("this repo has: {}", heads.join(", ")),
                "settle it first (`agit commit`), or name a branch with -b".into(),
            ],
            code: ExitCode::Ref,
        });
    }

    match heads {
        [] => Err(Refusal {
            msg: "this repo has no branches yet — there is nothing to publish.".into(),
            hints: vec!["record a first version: `agit commit`".into()],
            code: ExitCode::Precondition,
        }),
        [only] => Ok(vec![only.clone()]),
        many => Err(Refusal {
            msg: format!(
                "no session context here and this repo has {} branches — I won’t guess which one to publish:",
                many.len()
            ),
            hints: vec![
                format!("pick one: agit push -b {}", many[0]),
                "or publish everything with new turns: agit push --all".into(),
            ],
            code: ExitCode::Ref,
        }),
    }
}

/// Publish the main file line alongside the session branches. Bare repositories default HEAD to
/// `main`; omitting it leaves an otherwise healthy clone with a misleading "remote HEAD refers to
/// nonexistent ref" warning.
fn refs_to_push(branches: &[String], has_main: bool) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    for b in branches {
        if !refs.contains(b) {
            refs.push(b.clone());
        }
    }
    if has_main && !refs.iter().any(|b| b == "main") {
        refs.push("main".to_string());
    }
    refs
}

/// Tags pointing at commits reachable from the pushed branches.
///
/// `git push --tags` sends up **every** tag in the repo, including the ones on branches that
/// were not selected. A version ID is a name derived from the content, so pushing it announces
/// that the content exists — a branch outside `--all` must not be exposed along the way.
fn tags_to_push(repo: &Repo, branches: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for b in branches {
        let Some(list) = repo.git_opt(&["tag", "--merged", b]) else {
            continue;
        };
        for t in list.lines().map(str::trim).filter(|t| !t.is_empty()) {
            if !out.iter().any(|x| x == t) {
                out.push(t.to_string());
            }
        }
    }
    out
}

/// Push tags. Batched because an agent gets a version ID every turn, and a command line has a
/// length limit.
fn push_tags(repo: &Repo, tags: &[String]) -> std::result::Result<(), crate::hub::git::Outcome> {
    for chunk in tags.chunks(100) {
        let specs: Vec<String> = chunk.iter().map(|t| format!("refs/tags/{t}")).collect();
        let mut args: Vec<&str> = vec!["push", "origin"];
        args.extend(specs.iter().map(String::as_str));
        match crate::hub::git::run(repo, &args) {
            Ok(out) if out.ok() => {}
            Ok(out) => return Err(out),
            Err(e) => {
                return Err(crate::hub::git::Outcome {
                    code: 1,
                    stderr: format!("{e:#}"),
                });
            }
        }
    }
    Ok(())
}

/// Recover the origin from `<origin>@<short-sha>`.
///
/// Split from the right: an origin holds an `@` itself (`git@github.com:o/r.git` is the most
/// common form).
fn code_origin(code: &str) -> Option<String> {
    let (origin, _) = code.rsplit_once('@')?;
    (!origin.is_empty()).then(|| origin.to_string())
}

/// Where a hit is shown — **by carrier**, not the file name for everything.
///
/// A workspace file shows its basename: the leading path helps little in locating it, and a
/// narrower table reads better.
///
/// Every other carrier is shown whole. Their label is `<type> object <sha8>[/<path>]`, and the
/// oid inside it is the handle those remedies use (`git cat-file blob <oid>`, `git log --all
/// --find-object=<oid>`) — cut down to a basename, it is gone. The blob case especially: when
/// the same file is both in the workspace and in a blob in history, the two hits share rule,
/// line number and redacted excerpt, and taking the basename for both turns them into two
/// **identical** rows that read like a bug in the report, while they are two things to handle
/// separately.
fn where_column(h: &secrets::Hit) -> String {
    let at = h.file.as_deref().unwrap_or_default();
    match h.source {
        secrets::Source::File => Path::new(at)
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default(),
        _ => at.to_string(),
    }
}

/// `truncated`: the scan filled its cap, and hits beyond it went unreported.
///
/// Left unsaid, this report looks exactly like "that is all of it": the user fixes the rows one
/// by one, pushes again, and is stopped again — a loop with no way out.
/// One gate's verdict.
enum Gate {
    /// Nothing to stop (or the user allowed it explicitly).
    Pass,
    /// The reason has already been said; exit with this code.
    Blocked(ExitCode),
}

/// Scan once, say what came out, and give the verdict.
///
/// # Why this is one function
///
/// This command uses it **twice** (step 3, and step 6b after the destination changed), and both
/// times the wording, the allow switch and the exit code must be identical. The cost of a second
/// copy is "one repo judged by two sets of rules inside one push".
///
/// # "Not scanned" is stopped exactly like "found"
///
/// [`secrets::Unscanned`] records the part that was **never read at all** (a whole history over
/// budget, a single object over the line). A verdict given off an empty hit list is fail open: a
/// gate allowing the input it could not reach, and saying nothing. The server refuses an
/// over-the-line object rather than skipping it, so the two sides agree.
fn secret_gate(repo: &Repo, plan: &secrets::ScanPlan) -> crate::Result<Gate> {
    let sp = ui::spinner("scanning for secrets…");
    let scan = secrets::scan_agent_repo(repo, plan)?;
    sp.finish_and_clear();

    let hits = scan.hits;
    let unscanned = scan.unscanned;
    if hits.is_empty() && unscanned.is_empty() {
        return Ok(Gate::Pass);
    }
    if !hits.is_empty() {
        report_hits(&hits, scan.truncated);
    }
    if !unscanned.is_empty() {
        super::report_unscanned(&unscanned);
    }

    if config::allow_secrets() {
        // An allow must be visible. A silent bypass is the same as no gate.
        if !hits.is_empty() {
            ui::warning(&format!(
                "AGIT_ALLOW_SECRETS is set — proceeding past {} suspected secrets.",
                hits.len()
            ));
        }
        if !unscanned.is_empty() {
            ui::warning(
                "AGIT_ALLOW_SECRETS is set — proceeding past content that was not scanned.",
            );
        }
        // The wording has to hold **on both sides of a hub deployment**.
        //
        // "Scan only at exposure" is what a newer hub does, and this CLI can ship ahead of it —
        // until then the server still scans every push, and "pushing private will not be
        // refused" is false. So the first half says "may still refuse" (true in both worlds),
        // and the second says the thing that holds either way and that the user actually needs
        // to know: a secret that entered history does not go away, and it blocks you on the day
        // you make this public.
        ui::warning(
            "note: the server may still refuse this, and it will block making this agent public later.",
        );
        return Ok(Gate::Pass);
    }

    if hits.is_empty() {
        // With nothing scanned there is no saying "N secrets found" — that is false, and the
        // action it points at is wrong (there is no hit to fix). `report_unscanned` has already
        // said the reason and the next step.
        ui::error("publish blocked: part of what would be sent was not scanned.");
    } else {
        ui::error(&format!(
            "{} suspected secrets found — publish blocked.",
            hits.len()
        ));
        // A registered secret deliberately does not accept the allowlist; this way out is
        // offered only when a built-in heuristic actually hit.
        if hits.iter().any(|h| h.rule != "registered-secret") {
            ui::hint(&format!(
                "· false positive? add the string to {}",
                ui::tilde(&config::agit_home()?.join(secrets::ALLOWLIST_FILE))
            ));
        }
    }
    ui::hint("· to proceed anyway: AGIT_ALLOW_SECRETS=1 agit push");
    // The remedies that branch by carrier go through the **shared** implementation
    // (`agit scan --secrets` calls the same one). An unconditional promise of "annotate that
    // line with agit:allow-secret" is wrong for a hit inside a blob / commit / tag object: that
    // line is not in the workspace, there is nowhere to write it, and the one way out that works
    // goes unmentioned — same repo, scan right, push wrong.
    super::hint_secret_hit_remedies(hits.iter());
    Ok(Gate::Blocked(ExitCode::Policy))
}

fn report_hits(hits: &[secrets::Hit], truncated: bool) {
    ui::section("suspected secrets");
    let rows: Vec<Vec<String>> = hits
        .iter()
        .take(20)
        .map(|h| {
            vec![
                h.rule.clone(),
                where_column(h),
                h.line.to_string(),
                // Only the redacted excerpt is shown — this output goes into CI logs.
                h.redacted.clone(),
            ]
        })
        .collect();
    println!(
        "{}",
        ui::table::render(&["rule", "at", "line", "excerpt (redacted)"], &rows)
    );
    if hits.len() > 20 {
        println!("{}", ui::dim(&format!("… {} more", hits.len() - 20)));
    }
    if truncated {
        ui::hint(&format!(
            "this list is incomplete: it shows {} findings and stops there, more remain — fix these, then run `agit push` again to see the rest",
            hits.len()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a repo with two branches off `main`, each carrying a tag.
    fn fixture(tag: &str) -> (std::path::PathBuf, Repo) {
        let dir = std::env::temp_dir().join(format!(
            "agit-push-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = Repo::init(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-m", "one"]).unwrap();
        repo.git(&["tag", "agit-main-one"]).unwrap();
        // Fork a session line off main; each side then takes one more step.
        repo.git(&["checkout", "--quiet", "-b", "refund-fix"])
            .unwrap();
        std::fs::write(dir.join("b.txt"), "two").unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-m", "two"]).unwrap();
        repo.git(&["tag", "agit-refund-two"]).unwrap();
        repo.git(&["checkout", "--quiet", "-b", "ghost", "main"])
            .unwrap();
        std::fs::write(dir.join("c.txt"), "three").unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-m", "three"]).unwrap();
        repo.git(&["tag", "agit-ghost-three"]).unwrap();
        (dir, repo)
    }

    /// With nobody to ask in a non-interactive run (CI, scripts), the default is **private**.
    ///
    /// This pins the direction of the default: `public: !private` reads "said nothing" as
    /// "publish my complete work transcript", and that step cannot be undone.
    #[test]
    fn private_by_default_when_non_interactive() {
        assert!(
            !first_visibility(None, None),
            "a non-interactive run must default to private"
        );
        // On a tty the answer stands as given.
        assert!(first_visibility(None, Some(true)));
        assert!(!first_visibility(None, Some(false)));
        // A flag is not asked about, and no answer overrides it.
        assert!(first_visibility(Some(true), None));
        assert!(!first_visibility(Some(false), Some(true)));
    }

    #[test]
    fn visibility_flags_are_read_from_the_args() {
        let base = || Args {
            agent: None,
            branch: vec![],
            all: false,
            private: false,
            public: false,
            dry_run: false,
        };
        let (_d, repo) = {
            let d = tempfile::tempdir().unwrap();
            let r = Repo::init(&d.path().join("agents/alice/photo")).unwrap();
            (d, r)
        };
        assert_eq!(wanted_visibility(&base(), &repo), Wanted::Ask);
        assert_eq!(
            wanted_visibility(
                &Args {
                    private: true,
                    ..base()
                },
                &repo
            ),
            Wanted::Flag(false)
        );
        assert_eq!(
            wanted_visibility(
                &Args {
                    public: true,
                    ..base()
                },
                &repo
            ),
            Wanted::Flag(true)
        );
        // The preference `agit init --private` records in the repo is read by push.
        repo.set_visibility_preference("private").unwrap();
        assert_eq!(wanted_visibility(&base(), &repo), Wanted::Repo(false));
    }

    /// The flag beats the repo preference, which beats the global default; `ask`,
    /// typos and unset all mean "not said". An implementation that let the global
    /// default win over the repo preference would publish a repo created with
    /// `--private` publicly because of a setting made for other repos.
    #[test]
    fn visibility_preference_precedence() {
        assert_eq!(resolve_wanted(None, None, None), Wanted::Ask);
        assert_eq!(resolve_wanted(None, None, Some("ask")), Wanted::Ask);
        assert_eq!(resolve_wanted(None, None, Some("secret")), Wanted::Ask);
        assert_eq!(
            resolve_wanted(None, None, Some("public")),
            Wanted::Global(true)
        );
        assert_eq!(
            resolve_wanted(None, None, Some("private")),
            Wanted::Global(false)
        );
        assert_eq!(
            resolve_wanted(None, Some("private"), Some("public")),
            Wanted::Repo(false)
        );
        assert_eq!(
            resolve_wanted(Some(true), Some("private"), Some("private")),
            Wanted::Flag(true)
        );
        assert_eq!(Wanted::Ask.value(), None);
        assert_eq!(Wanted::Repo(false).value(), Some(false));
    }

    #[test]
    fn visibility_labels_are_not_guessed() {
        assert_eq!(visibility_label("public"), "public");
        assert_eq!(visibility_label("private"), "private");
        // Another level from the server passes through unchanged instead of being folded into
        // "private".
        assert_eq!(visibility_label("internal"), "internal");
    }

    #[test]
    fn first_session_push_also_publishes_the_main_file_line() {
        assert_eq!(refs_to_push(&["e2e".into()], true), ["e2e", "main"]);
        assert_eq!(refs_to_push(&["main".into()], true), ["main"]);
        assert_eq!(refs_to_push(&["e2e".into()], false), ["e2e"]);
        // The same branch written twice (`-b a -b a`) must not appear twice on the command line.
        assert_eq!(refs_to_push(&["a".into(), "a".into()], false), ["a"]);
    }

    /// The positional argument accepts the spelling its own ambiguity hint asks for.
    #[test]
    fn the_positional_takes_both_a_bare_name_and_owner_slash_name() {
        let (owner, name, branch) = split_publish_target("payments").unwrap();
        assert_eq!((owner, name, branch), (None, "payments".into(), None));
        assert_eq!(
            split_publish_target("alice/payments").unwrap(),
            (Some("alice".into()), "payments".into(), None)
        );
        assert_eq!(
            split_publish_target("alice/payments@refund").unwrap(),
            (
                Some("alice".into()),
                "payments".into(),
                Some("refund".into())
            )
        );
        // The owner must use the hub's lowercase spelling, or the local directory and the remote
        // path each use their own.
        assert!(split_publish_target("Einsia/agent-git-dev").is_err());
        // A repo name does not occupy the ref namespace: the `agit-` prefix is unambiguous in
        // the `owner/<name>` position.
        assert_eq!(
            split_publish_target("alice/agit-dev").unwrap(),
            (Some("alice".into()), "agit-dev".into(), None)
        );
        assert!(split_publish_target("a/b/c").is_err());
    }

    /// A first publish under an organization has one visibility: an explicit request for private
    /// is refused (the hub refuses it; this says so first), and saying nothing means public, with
    /// no question. An implementation carrying over the personal-repo "non-interactive defaults
    /// to private" sends a creation bound to be rejected.
    #[test]
    fn an_org_first_publish_is_public_or_refused() {
        assert!(org_first_visibility("einsia", "qa", Some(false)).is_err());
        assert!(org_first_visibility("einsia", "qa", Some(true)).unwrap());
        assert!(org_first_visibility("einsia", "qa", None).unwrap());
    }

    /// What goes up is the context branch, not the one the checkout sits on.
    ///
    /// A rejected import leaves HEAD on `ghost` while the context says `refund-fix`; an
    /// implementation that pushes the checked-out branch publishes content nobody claimed.
    #[test]
    fn the_context_branch_wins_over_the_checked_out_one() {
        let (dir, repo) = fixture("ctx");
        assert_eq!(repo.current_branch().as_deref(), Some("ghost"));
        let heads = repo.local_branches();
        let got = plan_branches(&[], false, Some("refund-fix"), &repo, &heads).unwrap();
        assert_eq!(got, ["refund-fix"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no resolvable context and several branches, this errors instead of picking one.
    #[test]
    fn several_branches_without_a_context_are_refused_not_guessed() {
        let (dir, repo) = fixture("ambig");
        let heads = repo.local_branches();
        let err = plan_branches(&[], false, None, &repo, &heads).unwrap_err();
        assert_eq!(err.code, ExitCode::Ref);
        assert!(
            err.hints.iter().any(|h| h.contains("--all")),
            "{:?}",
            err.hints
        );
        // With only one there is nothing to ask.
        let one = ["solo".to_string()];
        assert_eq!(
            plan_branches(&[], false, None, &repo, &one).unwrap(),
            ["solo"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `-b` pushes only the branches named; naming one that does not exist is said out loud.
    #[test]
    fn explicit_branches_are_taken_verbatim() {
        let (dir, repo) = fixture("explicit");
        let heads = repo.local_branches();
        let got = plan_branches(
            &["refund-fix".into(), "main".into()],
            false,
            Some("ghost"),
            &repo,
            &heads,
        )
        .unwrap();
        assert_eq!(got, ["refund-fix", "main"]);
        let err = plan_branches(&["nope".into()], false, None, &repo, &heads).unwrap_err();
        assert_eq!(err.code, ExitCode::Ref);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The scope of `--all` is "branches with something new", not "every branch".
    #[test]
    fn all_covers_the_branches_with_something_new() {
        let (dir, repo) = fixture("all");
        let heads = repo.local_branches();
        // No origin yet: all three are new.
        let mut got = plan_branches(&[], true, None, &repo, &heads).unwrap();
        got.sort();
        assert_eq!(got, ["ghost", "main", "refund-fix"]);
        // Mark main as "the remote already has it" — it drops out of the push scope.
        repo.git(&["update-ref", "refs/remotes/origin/main", "main"])
            .unwrap();
        let mut got = plan_branches(&[], true, None, &repo, &heads).unwrap();
        got.sort();
        assert_eq!(got, ["ghost", "refund-fix"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only the tags reachable from the pushed branches go up.
    ///
    /// `--tags` sends up the version IDs of the `ghost` experiment line too, and a version ID is
    /// a name derived from the content — pushing it announces that the content exists.
    #[test]
    fn only_tags_reachable_from_the_pushed_branches_go_up() {
        let (dir, repo) = fixture("tags");
        let got = tags_to_push(&repo, &["refund-fix".to_string()]);
        assert!(got.contains(&"agit-refund-two".to_string()), "{got:?}");
        assert!(
            got.contains(&"agit-main-one".to_string()),
            "an ancestor's version ID goes up too: {got:?}"
        );
        assert!(
            !got.contains(&"agit-ghost-three".to_string()),
            "another branch must not be broadcast along the way: {got:?}"
        );
        // Pushing main + ghost reverses it.
        let got = tags_to_push(&repo, &["main".to_string(), "ghost".to_string()]);
        assert!(got.contains(&"agit-ghost-three".to_string()), "{got:?}");
        assert!(!got.contains(&"agit-refund-two".to_string()), "{got:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn code_origin_splits_from_the_right() {
        // An ssh-form origin holds an `@` itself; splitting from the left yields "git".
        assert_eq!(
            code_origin("git@github.com:nana/OpenPad.git@1839e61").as_deref(),
            Some("git@github.com:nana/OpenPad.git")
        );
        assert_eq!(
            code_origin("https://github.com/nana/x.git@abc1234").as_deref(),
            Some("https://github.com/nana/x.git")
        );
        assert!(code_origin("no-at-sign").is_none());
    }

    /// The test for "can this be pushed at all".
    #[test]
    fn a_checkout_is_read_only_only_when_it_is_someone_elses_and_has_no_upstream() {
        // `agit clone alice/photo`: lands under alice's name, with no upstream.
        assert!(is_read_only("me", "alice", None));
        // Your own agent is never read-only.
        assert!(!is_read_only("me", "me", None));
        assert!(!is_read_only("me", "me", Some("http://h/alice/photo.git")));
        // A promoted copy lands under your name, upstream pointing at the source — pushable.
        assert!(!is_read_only("me", "me", Some("http://h/alice/photo.git")));
    }

    /// A promoted copy is not asked about again.
    ///
    /// This is why the upstream test exists: going by "does it point at somebody else's address"
    /// alone marks a perfectly good copy read-only, and every `agit push` then asks once more
    /// whether to create another one.
    #[test]
    fn an_already_promoted_copy_is_not_asked_again() {
        assert!(!is_read_only("me", "me", Some("http://h/alice/photo.git")));
        // Edge case: a collaborator keeps the repo under somebody else's name but has an
        // upstream configured — either it was promoted, or the user wired the remote up
        // themselves, and in neither case does push create another one for them.
        assert!(!is_read_only("me", "alice", Some("http://h/bob/photo.git")));
    }

    /// Every 422 hint says "the server refused it" first, or the user goes looking locally.
    #[test]
    fn a_422_is_explained_as_a_server_side_rejection() {
        let out = crate::hub::git::Outcome {
            code: 128,
            stderr: "error: RPC failed; HTTP 422 curl 22 The requested URL returned error: 422"
                .into(),
        };
        let advice = diagnose(&out, "me", "photo").join("\n");
        assert!(
            advice.contains("the server rejected"),
            "must say the server rejected it: {advice}"
        );
        assert!(
            !advice.contains("the remote moved ahead"),
            "must not point the wrong way: {advice}"
        );
    }

    #[test]
    fn a_stale_identity_is_not_diagnosed_as_non_fast_forward() {
        let out = crate::hub::git::Outcome {
            code: 128,
            stderr: "fatal: unable to access remote: The requested URL returned error: 412".into(),
        };
        let advice = diagnose(&out, "me", "photo").join("\n");
        assert!(advice.contains("identity"), "{advice}");
        assert!(advice.contains("deleted and reused"), "{advice}");
        assert!(!advice.contains("fetch first"), "{advice}");
    }
}
