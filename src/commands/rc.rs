//! `agit rc` — start the peer executor and manage device access.
//!
//! The commands are thin on purpose: everything real lives in [`crate::rc`].
//! This file only parses arguments, talks to the local control socket, and
//! renders. Every error ends in a command the user can run — an error without a
//! next step is a bug (see the CLI conventions in `commands/mod.rs`).

use super::CmdResult;
use crate::rc::control;
use crate::{ExitCode, ui};
use crate::{infra::config, rc::identity};
use clap::{Args as ClapArgs, Subcommand};

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Subcommand)]
pub enum Action {
    /// Carry transport packets for the supervising daemon.
    #[command(hide = true)]
    Tunnel,
    /// Start agitd and allow your signed-in account to control this device.
    Start(StartArgs),
    /// Owner-only local daemon and SSH stdio bridge, independent of Cloud admission.
    Local(crate::rc::local::Args),
    /// Enroll a cloud peer and manage executor resource permissions.
    Cloud(crate::rc::cloud::Args),
    /// Connection state, uptime and live sessions.
    Status,
    /// Stop the daemon. Sessions running under it end with it.
    Stop,
    /// Machines registered to your account.
    List,
    /// Revoke a Cloud device until its owner explicitly enables it again.
    Revoke(RevokeArgs),
    /// (internal) Prepare the local lineage for an RC-born session: repo,
    /// main file line, session branch, store link. Called by the daemon.
    #[command(hide = true)]
    Land(LandArgs),
    /// Let operators of a workspace answer approvals for one command themselves.
    Grant(GrantArgs),
    /// Require owner approval again for a previously granted command.
    Ungrant(GrantArgs),
    /// What operators of a workspace may currently answer on their own.
    Grants(GrantsArgs),
}

/// `agit rc grant <workspace> <command>`
#[derive(ClapArgs)]
pub struct GrantArgs {
    /// Workspace id (from `agit rc status`, or the URL of its page).
    pub workspace: String,
    /// A **bare command name** — `cargo`, `npm`, `git`. Not a path, not a
    /// command line: granting a command line grants arbitrary code.
    pub command: String,
}

#[derive(ClapArgs)]
pub struct GrantsArgs {
    /// Only this workspace. Omit to list every one.
    pub workspace: Option<String>,
}

#[derive(ClapArgs)]
pub struct LandArgs {
    /// Use pinned local repository authority without contacting a Hub.
    #[arg(long)]
    pub local_owner: bool,
    /// `owner/name` of the agent repo this session settles into.
    #[arg(long, value_name = "owner/name")]
    pub slug: String,
    /// Immutable identity of the agent repo, negotiated with the hub.
    #[arg(long, value_name = "uuid")]
    pub agent_id: String,
    /// Session branch (allocated by the hub).
    #[arg(long, value_name = "branch")]
    pub branch: String,
    /// Harness runtime (`claude-code` | `codex` | `opencode`).
    #[arg(long, value_name = "runtime")]
    pub runtime: String,
    /// The harness-native session/thread id.
    #[arg(long, value_name = "id")]
    pub session: String,
    /// The project working directory.
    #[arg(long, value_name = "dir")]
    pub cwd: String,
}

/// Builds the argv the daemon uses when it invokes `agit rc land` itself.
///
/// **It sits next to `LandArgs` so the two can only change together.** The only consumer of this
/// argv is the clap definition above, and the code that builds it lives in another file
/// (`rc::supervisor::land`): rename one flag, forget the other end, and landing becomes a
/// subprocess call that **always** fails — the only symptom is one log line "will retry next
/// turn", retried every turn, failing every turn, with nothing going red. The paired test feeds
/// this argv through clap for real.
pub fn land_argv(
    slug: &str,
    agent_id: &str,
    branch: &str,
    runtime: &str,
    session: &str,
    cwd: &str,
) -> Vec<String> {
    [
        "rc",
        "land",
        "--slug",
        slug,
        "--agent-id",
        agent_id,
        "--branch",
        branch,
        "--runtime",
        runtime,
        "--session",
        session,
        "--cwd",
        cwd,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[derive(ClapArgs)]
pub struct StartArgs {
    /// Run in the background; wait for local RPC readiness.
    #[arg(long)]
    pub detach: bool,
    /// Name shown in the web UI (default: this machine's hostname).
    #[arg(long, value_name = "name")]
    pub name: Option<String>,
}

#[derive(ClapArgs)]
pub struct RevokeArgs {
    /// Device id from `agit rc list`.
    #[arg(value_name = "device")]
    pub device: String,
}

pub fn run(args: Args) -> CmdResult {
    crate::rc::select_local_authority();
    match args.action {
        Action::Tunnel => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(agit_tunnel::worker::run(
                tokio::io::BufReader::new(tokio::io::stdin()),
                tokio::io::stdout(),
            ))?;
            Ok(ExitCode::Ok)
        }
        Action::Start(a) => start(a),
        Action::Local(a) => crate::rc::local::run(a),
        Action::Cloud(a) => crate::rc::cloud::run(a),
        Action::Status => status(),
        Action::Stop => stop(),
        Action::List => list(),
        Action::Revoke(a) => revoke(a),
        Action::Land(a) => land(a),
        Action::Grant(a) => grant(a),
        Action::Ungrant(a) => revoke_grant(a),
        Action::Grants(a) => grants(a),
    }
}

/// Let the operators of a workspace answer the approval for one command themselves.
///
/// # Why this happens only on the machine
///
/// The approval classifier is fail-closed: it hands a call to an operator only when it can
/// **positively prove** the call is confined to the workspace, and `git status` / `cargo test` /
/// `npm test` prove nothing of the sort (in a repo an agent can write to, `core.pager` /
/// `diff.external` / hooks make a single `git diff` execute arbitrary programs). Without this way
/// out an operator can answer no Bash call at all, and the only alternative is switching the
/// session to bypass — trading a reversible per-command allow for an **irreversible** session-wide
/// surrender.
///
/// Relaxing it stays the owner's decision; it just moves from editing code to typing a command.
/// **It does not go through the hub**: whoever types this command is already in a shell on this
/// machine, and there is no stronger proof of ownership; going through the hub would mean a
/// second authorization scheme for "is this request really from the owner", which is exactly the
/// problem this feature avoids.
fn grant(args: GrantArgs) -> CmdResult {
    if !crate::rc::grants::is_bare_command_name(&args.command) {
        ui::error(&format!(
            "`{}` is not a bare command name — grant `git`, not a path or a command line",
            args.command
        ));
        ui::hint(
            "grant a bare command name like `cargo` — a path or a command line would hand over arbitrary code",
        );
        return Ok(ExitCode::Usage);
    }
    let mut g = match crate::rc::grants::Grants::load_for_update() {
        Ok(grants) => grants,
        Err(error) => {
            ui::error(&format!("cannot read the local command grants: {error:#}"));
            return Ok(ExitCode::Precondition);
        }
    };
    if let Err(error) = g.grant(&args.workspace, &args.command) {
        ui::error(&format!("cannot save the local command grant: {error:#}"));
        return Ok(ExitCode::Precondition);
    }
    ui::success(&format!(
        "operators of {} can now answer `{}` themselves",
        args.workspace, args.command
    ));
    ui::hint("it applies to sessions already running — the daemon re-reads this on every approval");
    Ok(ExitCode::Ok)
}

fn revoke_grant(args: GrantArgs) -> CmdResult {
    let mut g = match crate::rc::grants::Grants::load_for_update() {
        Ok(grants) => grants,
        Err(error) => {
            ui::error(&format!("cannot read the local command grants: {error:#}"));
            return Ok(ExitCode::Precondition);
        }
    };
    match g.revoke(&args.workspace, &args.command) {
        Ok(true) => {
            ui::success(&format!(
                "`{}` goes back to needing you in {}",
                args.command, args.workspace
            ));
            Ok(ExitCode::Ok)
        }
        Ok(false) => {
            ui::success(&format!(
                "`{}` was not granted in {} — nothing to take back",
                args.command, args.workspace
            ));
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            ui::error(&e.to_string());
            Ok(ExitCode::Precondition)
        }
    }
}

fn grants(args: GrantsArgs) -> CmdResult {
    let g = crate::rc::grants::Grants::load();
    let rows: Vec<(&String, &std::collections::BTreeSet<String>)> = g
        .heads
        .iter()
        .filter(|(ws, _)| args.workspace.as_ref().is_none_or(|w| *ws == w))
        .filter(|(_, heads)| !heads.is_empty())
        .collect();
    if rows.is_empty() {
        ui::hint(
            "no commands granted — operators can answer reads and edits inside the workspace, and nothing else",
        );
        ui::hint("`agit rc grant <workspace> cargo` to let them answer `cargo …` too");
        return Ok(ExitCode::Ok);
    }
    for (ws, heads) in rows {
        println!(
            "{ws}  {}",
            heads.iter().cloned().collect::<Vec<_>>().join(" ")
        );
    }
    Ok(ExitCode::Ok)
}

/// The machine-side half of "bind a folder = a private repo; a session = a
/// branch". The hub allocates the slug and the branch; this puts the *local*
/// lineage in place so the ordinary settle path (`agit commit --from-hook`,
/// owned by the daemon's cancellable supervisor) has something to settle into.
/// Without it every RC conversation stays uncommitted: the local repo doesn't
/// exist, the branch was never born, and no link points the branch at the live
/// transcript.
///
/// Idempotent on purpose — the daemon calls it on every session start/resume.
fn land(args: LandArgs) -> CmdResult {
    if args.local_owner {
        return land_local(args);
    }
    // **These two fields come from the hub; this machine did not produce them.**
    //
    // They are joined into `~/.agit/repos/<owner>/<name>` and handed to clone / open_or_init. An
    // owner shaped like `../..` walks that path out of agit's home directory; a branch name is the
    // same — a string that never passed ref validation is taken as an argument to git (the
    // `--upload-pack=...` shape is especially dangerous).
    //
    // The test lives in exactly one place: `rc::lineage::AgitSession`. A second validation here —
    // `domain::repo::valid_name` plus a local `valid_branch_name` — drifts from it, and drift is
    // silent both ways. `domain::repo::valid_name` forbids repo names starting with `agit-` (that
    // rule tells a snapshot id apart in `agit clone x/y:Z` and has nothing to do with path
    // safety), yet the hub creates such names on its own — binding `~/Code/agit-web` yields
    // `alice/agit-web` — so every landing of a session under that directory fails, with no
    // symptom. A local branch check conflates "git says no" with "git could not run".
    let lineage =
        match crate::rc::lineage::AgitSession::new(&args.slug, &args.agent_id, &args.branch) {
            Ok(l) => l,
            Err(e) => {
                ui::error(&format!("hub sent an unusable lineage: {e}"));
                ui::hint("this is a hub bug or a path-traversal attempt; nothing was created");
                return Ok(ExitCode::Usage);
            }
        };
    let runtime = crate::input_argument(crate::adapter::normalize(&args.runtime))?;
    let (owner, name) = (lineage.owner(), lineage.name());
    let dest = lineage.repo_dir()?;
    let client = crate::hub::Client::from_env();
    let expected = crate::input_argument(crate::hub::identity::RemoteIdentity::new(
        client.base(),
        lineage.agent_id(),
    ))?;
    // Resolve the slug on every invocation, including an already-landed
    // checkout. A deleted-and-recreated name must stop before any local commit;
    // relying only on the later push fence would leave an unauthorized local
    // settlement behind.
    let agent = super::remote_request(client.get_agent(owner, name))?;
    let observed = crate::hub::identity::RemoteIdentity::new(client.base(), &agent.agent_id)?;
    if observed != expected {
        ui::error(&format!(
            "{} now identifies agent {}, but this RC session expects {}; refusing a reused name",
            args.slug, observed.agent_id, expected.agent_id
        ));
        return Ok(ExitCode::Precondition);
    }

    let native = crate::domain::merge_archive::RuntimeLinkKey {
        runtime: runtime.into(),
        session_id: args.session.clone(),
    };
    let expected_archive = super::commit::archive::expected_rc_handoff()?;
    if let Some(expected) = &expected_archive {
        anyhow::ensure!(
            expected.native == native
                && expected.role.slug == args.slug
                && expected.role.branch == args.branch,
            "RC landing differs from its retained Archive identity"
        );
    }
    if let Some(store) = crate::domain::store::Store::open()?
        && let Some(link) = super::commit::archive::native_link(&store, &native)?
        && link.merge_archive.is_some()
    {
        anyhow::ensure!(
            expected_archive
                .as_ref()
                .is_none_or(|expected| { link.merge_archive.as_ref() == Some(&expected.role) }),
            "RC Archive role changed after its retained handoff"
        );
        let repo = crate::domain::repo::Repo::open(&dest)
            .ok_or_else(|| anyhow::anyhow!("the RC archive checkout is missing"))?;
        anyhow::ensure!(
            crate::hub::identity::require_current_expected(&repo, client.base())? == expected,
            "RC archive checkout identity differs from the supervisor"
        );
        let handoff = super::commit::archive::rc_handoff(
            &store,
            &repo,
            &link,
            &args.slug,
            &args.branch,
            std::path::Path::new(&args.cwd),
        )?;
        println!(
            "{}{}",
            super::commit::archive::RC_PREFIX,
            serde_json::to_string(&handoff)?
        );
        return Ok(ExitCode::Ok);
    }
    anyhow::ensure!(
        expected_archive.is_none(),
        "the retained RC Archive Link or role is missing; ordinary landing is forbidden"
    );

    let history_update =
        match super::migration::begin_startup_recovery_for_path(&dest, "rc-land-history") {
            Ok(recovery) => recovery,
            Err(error) => {
                ui::error(&format!("cannot prepare RC history recovery: {error:#}"));
                return Ok(super::terminal_error_code(&error, ExitCode::Precondition));
            }
        };

    // Fetch the hub's copy when we don't have one — a rebind of a folder that
    // already has history elsewhere must build on that history, not fork it.
    // Any failure is fatal for this attempt. Falling back to a fresh repo would
    // turn a network error or reused slug into a new local history that a later
    // retry might push to the wrong immutable repository.
    if !dest.join(".git").exists() {
        let cloned = crate::hub::git::clone(&agent.clone_url, &dest, &expected)?;
        if !cloned.ok() {
            ui::error(&format!(
                "could not clone {} for RC settlement: {}",
                args.slug,
                cloned.stderr.trim()
            ));
            return Ok(super::push::branch_failure_code(&cloned));
        }
    }

    let repo = crate::domain::repo::Repo::open_or_init(&dest)?;
    let pinned = crate::hub::identity::require_current_expected(&repo, client.base())?;
    if pinned != expected {
        anyhow::bail!(
            "{} is pinned to agent {}, but this RC session expects {}; refusing to reuse the checkout",
            dest.display(),
            pinned.agent_id,
            expected.agent_id
        );
    }
    let store = crate::domain::store::Store::open_or_init()?;
    let _branch_guard = crate::domain::link::lock_branch(&store, &args.slug, &args.branch)?;
    let _link_guard = crate::domain::link::lock(&store, &args.runtime, &args.session)?;
    let lk = landed_link(&store, &args, name)?;

    if repo.commit_count() == 0 {
        super::import::create_main_file_line(&repo, owner, &lk)?;
    }
    let created = materialize_branch(&repo, &args.branch)?;
    super::migration::finish_external_history_update(&repo, history_update)?;
    if created {
        super::import::declare_session_line(&repo, &args.branch, &lk)?;
    }

    crate::domain::link::write(&store, &lk)?;
    Ok(ExitCode::Ok)
}

fn land_local(args: LandArgs) -> CmdResult {
    crate::rc::select_local_authority();
    let lineage = crate::rc::lineage::AgitSession::new(&args.slug, &args.agent_id, &args.branch)?;
    let repo = crate::rc::local_repository::require(&lineage)?;
    let history_update = super::migration::begin_startup_recovery_for_path(
        &lineage.repo_dir()?,
        "local-land-history",
    )?;
    let store = crate::domain::store::Store::open_or_init()?;
    let _branch_guard = crate::domain::link::lock_branch(&store, &args.slug, &args.branch)?;
    let _link_guard = crate::domain::link::lock(&store, &args.runtime, &args.session)?;
    let link = landed_link(&store, &args, lineage.name())?;
    anyhow::ensure!(
        link.merge_archive.is_none(),
        "local landing cannot replace an Archive role"
    );
    // Existing local branches change only their native link. Without a ref update there is
    // no new external history to migrate; unrelated session branches remain untouched.
    if !repo.has_ref(&format!("refs/heads/{}", args.branch)) {
        if repo.commit_count() == 0 {
            super::import::create_main_file_line(&repo, lineage.owner(), &link)?;
        }
        let created = materialize_branch(&repo, &args.branch)?;
        super::migration::finish_external_history_update(&repo, history_update)?;
        if created {
            super::import::declare_session_line(&repo, &args.branch, &link)?;
        }
    } else if let Some(recovery) = history_update {
        recovery.clear()?;
    }
    crate::domain::link::write(&store, &link)?;
    Ok(ExitCode::Ok)
}

/// Makes the branch the hub allocated exist locally. `true` means this call actually created it
/// (the caller then declares the session line).
///
/// Three cases:
/// - A local head already exists: use it unchanged, touch nothing.
/// - Only `refs/remotes/origin/<b>` exists: in a freshly cloned repo the hub's branch exists only
///   in remote-tracking form. Grow the local branch from it rather than forking off main —
///   otherwise this session is built on an **empty** new line, and every later push after that is
///   judged a divergence by the server. Restoring a published line takes its name as a fait
///   accompli and does not review it.
/// - Neither exists: a brand-new line. This is the only place RC **creates** a branch name, so it
///   follows the same new-branch policy as `new` / `fork` / `import -b` — the `agit-` prefix is
///   reserved for version IDs, and a branch carrying it makes `owner/repo@<b>` resolve from then
///   on to a version that does not exist. Git's own ref shape validation already happened at the
///   protocol layer (`rc::lineage`); only the prefix is reviewed here. The starting point reuses
///   import's decision (the main file line first) rather than being written a second time.
fn materialize_branch(repo: &crate::domain::repo::Repo, branch: &str) -> crate::Result<bool> {
    let head_ref = format!("refs/heads/{branch}");
    if repo.has_ref(&head_ref) {
        return Ok(false);
    }
    let remote = format!("refs/remotes/origin/{branch}");
    if repo.has_ref(&remote) {
        repo.git(&["branch", branch, &remote])?;
        return Ok(true);
    }
    crate::domain::repo::valid_branch_name(branch)?;
    if let Some(base) = super::import::birth_base(repo) {
        repo.git(&["branch", branch, &base])?;
    }
    Ok(true)
}

/// A repeated landing on the same destination retains its materialization baseline because
/// runtime-local transcript identities differ from the committed evidence. A reroute cannot
/// assume that another branch contains that prefix, so it must use native continuity checks.
/// Superseded instances remain historical and require explicit import onto a recovery line.
fn landed_link(
    store: &crate::domain::store::Store,
    args: &LandArgs,
    agent: &str,
) -> crate::Result<crate::domain::link::Link> {
    let mut lk = crate::domain::link::get(store, &args.runtime, &args.session)
        .unwrap_or_else(|| crate::domain::link::Link::new(&args.runtime, &args.session, None));
    anyhow::ensure!(
        lk.is_active(),
        "this runtime session was superseded; resume its active branch or import it onto a separate recovery line"
    );
    let owner = super::parse_slug(&args.slug)?.0;
    let previous_owner = lk
        .owner
        .clone()
        .or_else(crate::infra::credentials::current_user);
    if previous_owner.as_deref() != Some(owner.as_str())
        || !crate::domain::link::claims_branch(&lk, &owner, agent, &args.branch)
    {
        lk.baseline_bytes = None;
        lk.baseline_hash = None;
        lk.materialized_from = None;
    }
    lk.cwd = Some(args.cwd.clone());
    lk.agent = Some(agent.to_string());
    if let Ok((owner, _)) = super::parse_slug(&args.slug) {
        lk.owner = Some(owner);
    }
    lk.branch = Some(args.branch.clone());
    Ok(lk)
}

fn start(args: StartArgs) -> CmdResult {
    crate::rc::select_local_authority();
    let hub = config::hub_url();
    crate::input_argument(crate::infra::hub_authority::HubAuthority::parse(&hub))?;
    if let Err(error) = crate::rc::roster::Roster::try_load() {
        ui::error(&format!("{error:#}"));
        return Ok(ExitCode::Precondition);
    }
    if let Some(name) = &args.name {
        identity::set_display_name(name)?;
    }
    if crate::infra::credentials::load_checked(&hub)?.is_none() {
        ui::hint("Sign in to allow your account to control this device through Cloud.");
        if super::login::login()?.is_none() {
            ui::hint(
                "Run `agit login` to finish Cloud sign-in; local and SSH access remain available.",
            );
        }
    }
    crate::rc::cloud::store::request_inbound(&hub)?;
    ui::section("agit rc");
    println!("  machine   {}", identity::identity()?.display_name);
    println!("  hub       {hub}");
    println!("  runtimes  {}", runtimes_line());
    println!("  inbound   enabled for your account; other users require explicit access grants");
    println!("  cloud     registration and connection retry independently in the background");
    println!(
        "  workspace {}",
        crate::rc::navigation::workspaces_url(&hub)
    );
    ui::hint("`agit rc cloud inbound --hub <hub> --enabled false` disables Cloud inbound access.");
    if control::running_pid().is_some() || args.detach {
        crate::rc::local::ensure_daemon()?;
        ui::success("agitd is ready; Cloud connection status is recorded in the daemon log.");
        return Ok(ExitCode::Ok);
    }
    crate::rc::local::start_foreground()?;
    Ok(ExitCode::Ok)
}

fn status() -> CmdResult {
    crate::rc::select_local_authority();
    match control::ask(&control::Request::Status) {
        Ok(control::Reply::Status(s)) => {
            ui::section("agit rc");
            println!("  pid        {}", s.pid);
            println!("  hub        {}", s.hub);
            println!(
                "  state      {}",
                if s.online {
                    ui::ok("local RPC ready")
                } else {
                    ui::warn_text("local RPC unavailable")
                }
            );
            println!("  uptime     {}", human_secs(s.uptime_secs));
            println!("  version    {}", s.agit_version);
            println!();
            if s.sessions.is_empty() {
                println!("  {}", ui::dim("no live sessions"));
            } else {
                println!("  {} live session(s)", s.sessions.len());
                for l in &s.sessions {
                    println!(
                        "    {:14} {:12} {:8} seq {}",
                        crate::domain::link::short(&l.session_id),
                        l.runtime,
                        l.status,
                        l.last_seq
                    );
                }
            }
            Ok(ExitCode::Ok)
        }
        Ok(control::Reply::Error { message }) => {
            ui::error(&message);
            // A local control error cannot provide the requested daemon status snapshot.
            Ok(ExitCode::Precondition)
        }
        Ok(_) => Ok(ExitCode::Ok),
        Err(_) => {
            let (msg, hint) = unreachable(control::presence());
            ui::error(&msg);
            ui::hint(&hint);
            Ok(ExitCode::Precondition)
        }
    }
}

/// The daemon connection bit used by the TUI status bar.
///
/// A status bar is advisory and must not inherit the command's full wait budget. A missing,
/// offline, busy or unreadable daemon is conservatively shown as offline; `agit rc status` keeps
/// the detailed three-way diagnosis.
pub(crate) fn tui_online() -> bool {
    crate::rc::select_local_authority();
    const BUDGET: std::time::Duration = std::time::Duration::from_millis(150);
    matches!(
        control::ask_with_timeout(&control::Request::Status, BUDGET),
        Ok(control::Reply::Status(control::Status { online: true, .. }))
    )
}

/// What to say when the daemon cannot be asked.
///
/// # Why "definitely absent" and "cannot tell" stay apart here
///
/// Saying "no daemon is running on this machine." whatever the probe found contradicts the other
/// side: when the control socket cannot say, `agit rc start` **refuses to start** (it will not
/// delete a socket that may still belong to someone alive, see [`control::listen`]). The user then
/// holds two contradictory statements — status says there is none, start says there already is
/// one — and neither tells them what to do next.
///
/// So the wording follows [`control::Presence`], and that test is the one `listen` reads:
/// `Absent` is the only "definitely absent".
fn unreachable(p: control::Presence) -> (String, String) {
    match p {
        control::Presence::Absent => (
            "no daemon is running on this machine.".into(),
            "`agit rc start`".into(),
        ),
        control::Presence::Running(pid) => (
            format!("the daemon (pid {pid}) is running but did not answer just now."),
            "retry, or `agit rc stop` if it stays wedged".into(),
        ),
        control::Presence::Unclear(why) => (
            format!("cannot tell whether a daemon is running on this machine ({why})."),
            "retry, or `agit rc stop` if it stays wedged".into(),
        ),
    }
}

/// The **whole verdict** for `agit rc stop` when the daemon cannot be asked: what it says, what it
/// hints, what it exits with.
///
/// # Why this is a pure function
///
/// The three come from **one** probe and must interlock: `Absent` is both "nothing to stop" and a
/// successful exit, everything else is both the plain truth and `Precondition`. Written inside
/// `stop()`, only the `unreachable` half of the wording is testable — no test reaches the exit
/// code, and the exit code is precisely this command's scriptable contract. As its own function
/// the verdict can be asserted on directly.
///
/// # Why `status()` does not share the exit code
///
/// Both sides share the wording (both go through [`unreachable`]; there must not be a second
/// phrasing). The exit codes differ because the two commands ask different questions: `stop` is
/// **idempotent** — with no daemon, "stop it" is already satisfied, so it succeeds; `status` is a
/// query, and finding no state means a precondition does not hold. Making the two exit codes agree
/// destroys the scripting semantics of one of them.
fn stop_verdict(p: control::Presence) -> (String, String, ExitCode) {
    // The test is the one `status()` uses: only a **definitely absent** daemon is reported as
    // absent. A failure from `ask` mixes a connect timeout, a rejection from a full backlog, and a
    // connection that never got an answer — all states where the daemon is **still running, merely
    // wedged or busy** — and reporting those as absent contradicts what `start` says.
    let absent = matches!(p, control::Presence::Absent);
    let (msg, hint) = unreachable(p);
    if absent {
        (msg, "nothing to stop".into(), ExitCode::Ok)
    } else {
        (msg, hint, ExitCode::Precondition)
    }
}

fn stop() -> CmdResult {
    crate::rc::select_local_authority();
    match crate::rc::lifecycle::stop_and_wait() {
        Ok(_) => {
            println!("  {} stopped", ui::ok("✓"));
            Ok(ExitCode::Ok)
        }
        Err(_) => {
            // **Probe once**: the wording, the hint and the exit code share one snapshot.
            // `presence()` is not a memory read — it really connects to the control socket. Probe
            // twice and a daemon that exits or recovers between the two makes the command say
            // "still running but did not answer" while returning success and hinting "nothing to
            // stop", and the worst-case wait doubles.
            //
            // This line is the only `presence()` in this function; `only_one_probe_per_stop` pins
            // that.
            let (msg, hint, code) = stop_verdict(control::presence());
            ui::error(&msg);
            ui::hint(&hint);
            Ok(code)
        }
    }
}

fn list() -> CmdResult {
    crate::rc::select_local_authority();
    let runtime = tokio::runtime::Runtime::new()?;
    let clients = crate::rc::cloud::Clients::default();
    let value = runtime.block_on(crate::rc::cloud::manage(
        &clients,
        crate::rc::cloud::OwnerRequest::Devices {
            hub: config::hub_url(),
            after: None,
        },
    ))?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(ExitCode::Ok)
}

fn revoke(args: RevokeArgs) -> CmdResult {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let hub = config::hub_url();
        let token = crate::rc::cloud::account_token(&hub, false).await?;
        let api = agit_peer::client::Client::new(&hub)?;
        let mut after = None;
        loop {
            let page = api.devices(&token, after.as_deref()).await?;
            if let Some(entry) = page
                .devices
                .iter()
                .find(|entry| entry.device.id == args.device)
            {
                break api.revoke(&token, &entry.device).await;
            }
            after = page.next_cursor;
            anyhow::ensure!(after.is_some(), "no such device");
        }
    })?;
    ui::success("Device Cloud access revoked.");
    Ok(ExitCode::Ok)
}

fn runtimes_line() -> String {
    crate::rc::harness::drivable()
        .into_iter()
        .map(|c| {
            if c.available {
                format!("{} {}", c.runtime, ui::ok("✓"))
            } else {
                format!("{} {}", c.runtime, ui::dim("(not installed)"))
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn human_secs(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    const AGENT_ID: &str = "00000000-0000-0000-0000-000000000001";

    /// `agit rc land`'s slug and branch name **come from the hub**; this machine does not produce
    /// them.
    ///
    /// They are joined into `~/.agit/repos/<owner>/<name>` and handed to clone and
    /// `open_or_init`: an owner shaped like `../..` walks that path out of agit's home directory,
    /// and then a git repo gets created, read and modified at an arbitrary location. A branch name
    /// is the same — a string that never passed ref validation is taken as an argument to git, and
    /// the `--upload-pack=...` option shape is especially dangerous.
    ///
    /// This pins the two validations themselves, not `land`'s shell: `land` really touches the
    /// filesystem, and validation must stop these values before it does.
    #[test]
    fn a_hub_that_sends_a_traversing_slug_or_a_hostile_branch_gets_refused() {
        use crate::rc::lineage::AgitSession;
        for bad in ["../..", "..", ".", "a/b/c", "a b", "a.b", "a\\b"] {
            assert!(
                AgitSession::new(bad, AGENT_ID, "main").is_err(),
                "`{bad}` must not be usable as a repo slug"
            );
        }
        for ok in ["acme/payments", "acme/agent_git", "a-b/c-d", "x/y1"] {
            assert!(AgitSession::new(ok, AGENT_ID, "main").is_ok(), "`{ok}`");
        }
        // **A repo name starting with `agit-` must be usable.** The hub creates them on its own
        // (binding `~/Code/agit-web` yields `alice/agit-web`). `domain::repo::valid_name` forbids
        // that prefix, so applying that test here makes every landing of a session under that
        // directory fail with no symptom.
        assert!(AgitSession::new("alice/agit-web", AGENT_ID, "main").is_ok());
        for bad in ["--upload-pack=touch /tmp/pwn", "-x", "a..b", "a b", ""] {
            assert!(
                !crate::rc::lineage::valid_branch_name(bad),
                "`{bad}` must not be usable as a branch"
            );
        }
        assert!(crate::rc::lineage::valid_branch_name(
            "s-202608202307-9f3a1c07b25e4d8a"
        ));
    }

    /// `agit rc stop`'s wording, hint and exit code agree.
    ///
    /// This pins the verdict itself: `Absent` ⟺ "nothing to stop" + a **successful exit**;
    /// everything else (`Running` / `Unclear`) ⟺ the plain truth + `Precondition`.
    ///
    /// # Why this mapping matters
    ///
    /// The exit code is this command's only scriptable contract: a script reads `0` as "it really
    /// is stopped now" and moves on to the next step (change the port, delete the socket, restart).
    /// Map `Unclear` to `0` and the script keeps acting on a daemon that may still be alive; map
    /// `Absent` to non-zero and a situation whose demand is already satisfied fails the whole
    /// script. Neither side can fall back on the wording — wording is for people, the exit code is
    /// for machines, and machines do not read wording.
    #[test]
    fn the_stop_wording_and_exit_code_agree() {
        use crate::ExitCode;
        use crate::rc::control::Presence;

        // Definitely absent: stopping a nonexistent daemon is already satisfied — exit success.
        let (msg, hint, code) = super::stop_verdict(Presence::Absent);
        assert!(msg.contains("no daemon is running"), "{msg}");
        assert_eq!(hint, "nothing to stop", "{hint}");
        assert_eq!(
            code,
            ExitCode::Ok,
            "with no daemon `agit rc stop` must exit success; a script reads this exit code as \
             \"it really is stopped now\""
        );

        // Still running / cannot tell: neither one is stopped, and neither may be called absent.
        for p in [Presence::Running(1234), Presence::Unclear("busy".into())] {
            let label = format!("{p:?}");
            let (msg, hint, code) = super::stop_verdict(p);
            assert!(
                !msg.contains("no daemon is running"),
                "{label}: cannot tell / still running must not be reported as absent: {msg}"
            );
            assert_ne!(hint, "nothing to stop", "{label}: it may still be running");
            assert!(!hint.is_empty(), "{label}: an error must give a next step");
            assert_eq!(
                code,
                ExitCode::Precondition,
                "{label}: a failed stop must not report success; a script takes the daemon as gone"
            );
        }
    }

    /// `stop()` probes once.
    ///
    /// `presence()` is not a memory read — it really connects to the control socket. Take it
    /// twice and a daemon that exits or recovers between the two makes the command contradict
    /// itself: it says "still running but did not answer" while returning success and hinting
    /// "nothing to stop" (and the reverse holds too). On a busy socket the worst-case wait also
    /// doubles.
    ///
    /// A return value cannot pin this property — it is "one snapshot feeds three outputs", not the
    /// value of any one output. With the verdict living in [`stop_verdict`], all `stop()` still has
    /// to hold is that `presence()` appears once, so this pins the source text.
    #[test]
    fn only_one_probe_per_stop() {
        let src = include_str!("rc.rs");
        let body = src
            .split_once(
                "\nfn stop() -> CmdResult {
    crate::rc::select_local_authority();",
            )
            .expect("stop() not found; this test no longer pins anything")
            .1
            .split_once("\n}\n")
            .expect("stop() has no closing brace at column zero")
            .0;
        // Comments mention `presence()` too (just above), so strip comment lines before counting.
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            code.matches("presence()").count(),
            1,
            "stop() must probe the daemon once; wording / hint / exit code share it:\n{code}"
        );
    }
    use clap::Parser as _;

    #[derive(clap::Parser)]
    struct Probe {
        #[command(subcommand)]
        cmd: super::Action,
    }

    /// `agit rc status` **must not** say "no daemon" when it cannot tell.
    ///
    /// When the control socket cannot say, `agit rc start` refuses to start (see
    /// `control::listen`). If status still says "no daemon is running", the user holds two
    /// contradictory statements, and neither tells them what to do next.
    #[test]
    fn an_unanswerable_probe_is_not_reported_as_no_daemon() {
        use crate::rc::control::Presence;

        let (msg, hint) = super::unreachable(Presence::Unclear("the socket is wedged".into()));
        assert!(
            !msg.contains("no daemon is running"),
            "cannot tell is not the same as absent: {msg}"
        );
        assert!(!hint.is_empty(), "an error must give a next step");

        let (msg, _) = super::unreachable(Presence::Running(7));
        assert!(!msg.contains("no daemon is running"), "it answered: {msg}");

        // The reverse: definitely absent still says so plainly and points at `agit rc start`.
        let (msg, hint) = super::unreachable(Presence::Absent);
        assert!(msg.contains("no daemon is running"), "{msg}");
        assert!(hint.contains("agit rc start"), "{hint}");
    }

    /// Every subcommand in the PRD's CLI surface must actually parse. This is
    /// the same class of bug the hooks test guards: a command documented in the
    /// README that exits 2 on arg parsing.
    #[test]
    fn the_documented_subcommands_all_parse() {
        for argv in [
            vec!["x", "start"],
            vec!["x", "start", "--detach"],
            vec!["x", "start", "--name", "laptop"],
            vec!["x", "status"],
            vec!["x", "stop"],
            vec!["x", "list"],
            vec!["x", "revoke", "conn-123"],
        ] {
            assert!(
                Probe::try_parse_from(&argv).is_ok(),
                "failed to parse {argv:?}"
            );
        }
    }

    /// **The `rc land` argv the daemon builds must actually parse.**
    ///
    /// Its construction and the clap definition live in two files; rename one flag, forget the
    /// other end, and landing becomes a subprocess call that **always** fails, with one log line
    /// "will retry next turn" as the only symptom — retried every turn, failing every turn, with
    /// nothing going red. This feeds the real argv (not a hand-copied duplicate) to the real
    /// parser.
    #[test]
    fn the_argv_the_daemon_builds_for_land_actually_parses() {
        let full = super::land_argv(
            "alice/payments",
            AGENT_ID,
            "s-202608220101-abcd",
            "claude-code",
            "thread-1",
            "/home/alice/code/payments",
        );
        // In production this argv goes to `agit` itself, so the first word is the subcommand group
        // `rc`; `Probe` here wraps the enum **inside** the group, so strip the group name before
        // feeding it. Failing to strip it goes red on the spot — the construction got even the
        // group name wrong.
        assert_eq!(
            full[0], "rc",
            "argv must start with the subcommand group name"
        );
        let mut argv = vec!["x".to_string()];
        argv.extend(full.into_iter().skip(1));
        let parsed = Probe::try_parse_from(&argv).expect("the argv the daemon builds must parse");
        let super::Action::Land(a) = parsed.cmd else {
            panic!("parsed a different subcommand");
        };
        assert_eq!(a.slug, "alice/payments");
        assert_eq!(a.agent_id, AGENT_ID);
        assert_eq!(a.branch, "s-202608220101-abcd");
        assert_eq!(a.runtime, "claude-code");
        assert_eq!(a.session, "thread-1");
        assert_eq!(a.cwd, "/home/alice/code/payments");
    }

    /// **Landing must not erase the materialization baseline.**
    ///
    /// `agit rc land` runs on every session start/resume (`rc::supervisor::land`, and again before
    /// every turn's settlement), and the link's `baseline_bytes` / `baseline_hash` are not written
    /// by it: they are recorded the moment `agit resume`'s slow path materializes the VIEW into the
    /// runtime, and settlement counts only the bytes appended after the baseline. Lose the baseline
    /// and `agit commit` switches to "native continuation", comparing the committed LOG against the
    /// live transcript byte for byte for continuity — materialized content ids are recast and can
    /// never be a prefix of the LOG, so every turn is judged "this branch is already claimed by
    /// another session" and exits Policy: not one line of the remotely driven conversation lands in
    /// the repo, and nothing goes red (settlement runs in the supervisor's subprocess, and a
    /// failure leaves one line in the log).
    ///
    /// This takes the real persistence path: `link::write` overwrites the whole file and does not
    /// merge, and "the struct in memory still looks intact" proves nothing about what reads back
    /// next time.
    /// When RC creates a line for the first time, the `agit-` prefix is refused exactly as in
    /// `new` / `fork` / `import -b`; restoring a line the hub already has (locally only a
    /// remote-tracking ref) takes the name as a fait accompli and grows the local branch as usual.
    /// An implementation that only checks Git's ref shape really creates `agit-foo` in the first
    /// case.
    #[test]
    fn landing_refuses_to_create_a_reserved_prefix_line_but_restores_an_existing_one() {
        use crate::domain::repo::Repo;

        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("repos/alice/photo")).unwrap();
        super::super::init::scaffold(repo.root()).unwrap();
        repo.add_all().unwrap();
        repo.commit("agit: init (main file line)").unwrap();

        let refused = super::materialize_branch(&repo, "agit-foo");
        assert!(refused.is_err(), "a brand-new `agit-` line must be refused");
        assert!(
            !repo.has_ref("refs/heads/agit-foo"),
            "and nothing may be created"
        );

        assert!(super::materialize_branch(&repo, "s-202608220101-abcd").unwrap());
        assert!(repo.has_ref("refs/heads/s-202608220101-abcd"));
        assert!(
            !super::materialize_branch(&repo, "s-202608220101-abcd").unwrap(),
            "an existing head is reused, not rebuilt"
        );

        // A line the hub already has: locally only a remote-tracking ref.
        let main = repo.git(&["rev-parse", "refs/heads/main"]).unwrap();
        repo.git(&["update-ref", "refs/remotes/origin/agit-legacy", &main])
            .unwrap();
        assert!(super::materialize_branch(&repo, "agit-legacy").unwrap());
        assert!(
            repo.has_ref("refs/heads/agit-legacy"),
            "restoring a published line keeps its name, whatever it is"
        );
    }

    #[test]
    fn landing_keeps_a_baseline_only_for_the_same_destination() {
        use crate::domain::link::{self, Link};
        use crate::domain::store::Store;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("store"));
        let mut resumed = Link::new("claude-code", "thread", None);
        resumed.owner = Some("alice".into());
        resumed.agent = Some("photo".into());
        resumed.branch = Some("work".into());
        resumed.baseline_bytes = Some(4096);
        resumed.baseline_hash = Some("f00d".into());
        resumed.materialized_from = Some("a".repeat(40));
        link::write(&store, &resumed).unwrap();
        let mut args = super::LandArgs {
            local_owner: false,
            slug: "alice/photo".into(),
            agent_id: AGENT_ID.into(),
            branch: "work".into(),
            runtime: "claude-code".into(),
            session: "thread".into(),
            cwd: "/home/alice/code/photo".into(),
        };
        let repeated = super::landed_link(&store, &args, "photo").unwrap();
        assert_eq!(repeated.baseline_bytes, resumed.baseline_bytes);
        assert_eq!(repeated.baseline_hash, resumed.baseline_hash);
        assert_eq!(repeated.materialized_from, resumed.materialized_from);
        assert_eq!(repeated.cwd.as_deref(), Some(args.cwd.as_str()));

        args.branch = "recovery".into();
        let rerouted = super::landed_link(&store, &args, "photo").unwrap();
        assert!(rerouted.baseline_bytes.is_none());
        assert!(rerouted.baseline_hash.is_none());
        assert!(rerouted.materialized_from.is_none());
        assert_eq!(rerouted.branch.as_deref(), Some("recovery"));

        resumed.superseded_by = Some("claude-code/successor".into());
        link::write(&store, &resumed).unwrap();
        assert!(super::landed_link(&store, &args, "photo").is_err());
        let empty = Store::at(dir.path().join("empty"));
        let fresh = super::landed_link(&empty, &args, "photo").unwrap();
        assert!(fresh.baseline_bytes.is_none());
        assert!(fresh.baseline_hash.is_none());
        assert!(fresh.materialized_from.is_none());
    }
}
