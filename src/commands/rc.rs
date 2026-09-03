//! `agit rc` — pair this machine, run the daemon, inspect and revoke it.
//!
//! The commands are thin on purpose: everything real lives in [`crate::rc`].
//! This file only parses arguments, talks to the local control socket, and
//! renders. Every error ends in a command the user can run — an error without a
//! next step is a bug (see the CLI conventions in `commands/mod.rs`).

use super::CmdResult;
use crate::rc::{control, identity};
use crate::{ExitCode, infra::config, ui};
use clap::{Args as ClapArgs, Subcommand};

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Subcommand)]
pub enum Action {
    /// Pair this machine (first run) and start the daemon.
    Start(StartArgs),
    /// Connection state, uptime and live sessions.
    Status,
    /// Stop the daemon. Sessions running under it end with it.
    Stop,
    /// Machines registered to your account.
    List,
    /// Revoke a machine — it disconnects at once and cannot re-register.
    Revoke(RevokeArgs),
    /// Print a fresh pairing code for this machine.
    Pair,
    /// (internal) Prepare the local lineage for an RC-born session: repo,
    /// main file line, session branch, store link. Called by the daemon.
    #[command(hide = true)]
    Land(LandArgs),
    /// Let operators of a workspace answer approvals for one command themselves.
    Grant(GrantArgs),
    /// Take a granted command back.
    ///
    /// Not `revoke`: that name already belongs to revoking a machine, and the two consequences
    /// are far apart — one is "this command asks me again", the other is "this machine
    /// disconnects at once and cannot re-register".
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
    /// `owner/name` of the agent repo this session settles into.
    #[arg(long, value_name = "owner/name")]
    pub slug: String,
    /// Immutable identity of the agent repo, negotiated with the hub.
    #[arg(long, value_name = "uuid")]
    pub agent_id: String,
    /// Session branch (allocated by the hub).
    #[arg(long, value_name = "branch")]
    pub branch: String,
    /// Harness runtime (`claude-code` | `codex`).
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
    /// Run in the background instead of holding the terminal.
    #[arg(long)]
    pub detach: bool,
    /// Name shown in the web UI (default: this machine's hostname).
    #[arg(long, value_name = "name")]
    pub name: Option<String>,
}

#[derive(ClapArgs)]
pub struct RevokeArgs {
    /// Connection id from `agit rc list`.
    #[arg(value_name = "connection")]
    pub connection: String,
}

pub fn run(args: Args) -> CmdResult {
    match args.action {
        Action::Start(a) => start(a),
        Action::Status => status(),
        Action::Stop => stop(),
        Action::List => list(),
        Action::Revoke(a) => revoke(a),
        Action::Pair => pair(),
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
    let mut g = crate::rc::grants::Grants::load();
    if let Err(e) = g.grant(&args.workspace, &args.command) {
        ui::error(&e.to_string());
        ui::hint(
            "grant a bare command name like `cargo` — a path or a command line would hand over arbitrary code",
        );
        return Ok(ExitCode::Usage);
    }
    ui::success(&format!(
        "operators of {} can now answer `{}` themselves",
        args.workspace, args.command
    ));
    ui::hint("it applies to sessions already running — the daemon re-reads this on every approval");
    Ok(ExitCode::Ok)
}

fn revoke_grant(args: GrantArgs) -> CmdResult {
    let mut g = crate::rc::grants::Grants::load();
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
    let (owner, name) = (lineage.owner(), lineage.name());
    let dest = lineage.repo_dir()?;
    let client = crate::hub::Client::from_env();
    let expected = crate::hub::identity::RemoteIdentity::new(client.base(), lineage.agent_id())?;
    // Resolve the slug on every invocation, including an already-landed
    // checkout. A deleted-and-recreated name must stop before any local commit;
    // relying only on the later push fence would leave an unauthorized local
    // settlement behind.
    let agent = client.get_agent(owner, name)?;
    let observed = crate::hub::identity::RemoteIdentity::new(client.base(), &agent.agent_id)?;
    if observed != expected {
        anyhow::bail!(
            "{} now identifies agent {}, but this RC session expects {}; refusing a reused name",
            args.slug,
            observed.agent_id,
            expected.agent_id
        );
    }

    // Fetch the hub's copy when we don't have one — a rebind of a folder that
    // already has history elsewhere must build on that history, not fork it.
    // Any failure is fatal for this attempt. Falling back to a fresh repo would
    // turn a network error or reused slug into a new local history that a later
    // retry might push to the wrong immutable repository.
    if !dest.join(".git").exists() {
        let cloned = crate::hub::git::clone(&agent.clone_url, &dest, &expected)?;
        if !cloned.ok() {
            anyhow::bail!(
                "could not clone {} for RC settlement: {}",
                args.slug,
                cloned.stderr.trim()
            );
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
    let lk = landed_link(&store, &args, name);

    if repo.commit_count() == 0 {
        super::import::create_main_file_line(&repo, owner, &lk)?;
    }
    if materialize_branch(&repo, &args.branch)? {
        super::import::declare_session_line(&repo, &args.branch, &lk)?;
    }

    crate::domain::link::write(&store, &lk)?;
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

/// What this session's store link must look like **after landing**: it adds to the **existing**
/// one instead of creating a new one.
///
/// land is idempotent and runs on every session start/resume (and again before every turn's
/// settlement), yet several fields on the link are not its output at all — `baseline_bytes` /
/// `baseline_hash` are the **settlement baseline** recorded the moment `agit resume`'s slow path
/// (and fork) materializes the VIEW into the runtime. `link::write` overwrites the whole file and
/// does not merge, so writing back a brand-new `Link::new` here every time erases that baseline
/// permanently.
///
/// Erasing it costs far more than one field: it alone decides `commit::settle_bytes`'s mode. With
/// no baseline the path is "native continuation", comparing the committed LOG against the live
/// transcript byte for byte for continuity — but materialized content ids are recast by
/// `domain::install` and can never be a prefix of the LOG, so every turn is judged
/// `already claimed by another session` and exits Policy. Not one line of the remotely driven
/// conversation lands in the repo, and nothing goes red: settlement runs in the supervisor's
/// subprocess, and a failure leaves one line in the log.
///
/// Reading the store **stays inside this function** and is not passed in by the caller: a caller
/// can always pass an empty one, which is that same failure in its original shape — only now no
/// test can see it. For the same reason the starting point is the whole old link rather than a
/// field-by-field pick: whoever adds a field to `Link` later need not know this place exists.
///
/// The other way round, cwd / agent / branch always come from this landing: the first is the
/// directory the daemon actually runs this session in right now, the other two are the agent repo
/// and branch the hub just allocated. The old record in the store has no say over these three;
/// copying it makes settlement advance the wrong branch.
fn landed_link(
    store: &crate::domain::store::Store,
    args: &LandArgs,
    agent: &str,
) -> crate::domain::link::Link {
    let mut lk = crate::domain::link::get(store, &args.runtime, &args.session)
        .unwrap_or_else(|| crate::domain::link::Link::new(&args.runtime, &args.session, None));
    lk.cwd = Some(args.cwd.clone());
    lk.agent = Some(agent.to_string());
    if let Ok((owner, _)) = super::parse_slug(&args.slug) {
        lk.owner = Some(owner);
    }
    lk.branch = Some(args.branch.clone());
    lk
}

fn start(args: StartArgs) -> CmdResult {
    if let Some(pid) = control::running_pid() {
        ui::error(&format!(
            "a daemon is already running on this machine (pid {pid})."
        ));
        ui::hint("`agit rc status` to see it, `agit rc stop` to replace it");
        return Ok(ExitCode::Precondition);
    }
    if let Some(n) = &args.name {
        identity::set_display_name(n)?;
    }

    let hub = config::hub_url();
    let conn = match identity::connection(&hub) {
        Some(c) => c,
        None => {
            // First run: pair through the existing device-code flow rather than
            // inventing a second credential system.
            match pair_interactive(&hub)? {
                Some(c) => c,
                None => return Ok(ExitCode::Auth),
            }
        }
    };

    let id = identity::identity()?;
    ui::section("agit rc");
    println!("  machine   {}", ui::accent(&id.display_name));
    println!("  hub       {hub}");
    println!("  runtimes  {}", runtimes_line());
    println!();

    if args.detach {
        // Re-exec ourselves without --detach, fully detached from this terminal.
        let exe = std::env::current_exe()?;
        let child = std::process::Command::new(exe)
            .args(["rc", "start"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        println!(
            "  {} daemon running in the background (pid {})",
            ui::ok("✓"),
            child.id()
        );
        println!();
        println!(
            "  {}",
            ui::dim("`agit rc status` to check it, `agit rc stop` to stop it")
        );
        return Ok(ExitCode::Ok);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let opts = crate::rc::daemon::Options {
        hub,
        token: conn.token,
        connection_id: Some(conn.connection_id),
    };
    match rt.block_on(crate::rc::daemon::Daemon::run(opts)) {
        Ok(()) => Ok(ExitCode::Ok),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            Ok(ExitCode::Failure)
        }
    }
}

fn status() -> CmdResult {
    match control::ask(&control::Request::Status) {
        Ok(control::Reply::Status(s)) => {
            ui::section("agit rc");
            println!("  pid        {}", s.pid);
            println!("  hub        {}", s.hub);
            println!(
                "  state      {}",
                if s.online {
                    ui::ok("connected")
                } else {
                    ui::warn_text("offline (retrying)")
                }
            );
            if let Some(c) = &s.connection_id {
                println!("  connection {c}");
            }
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
            Ok(ExitCode::Failure)
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
    match control::ask(&control::Request::Stop) {
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
    let c = crate::hub::Client::from_env();
    if !c.has_token() {
        ui::error(&format!("sign in to {} first.", c.base()));
        ui::hint("`agit login`");
        return Ok(ExitCode::Auth);
    }
    match c.rc_connections() {
        Ok(rows) if rows.is_empty() => {
            println!("  {}", ui::dim("no machines paired yet"));
            println!("  {}", ui::dim("`agit rc start` on a machine to pair it"));
            Ok(ExitCode::Ok)
        }
        Ok(rows) => {
            ui::section("machines");
            for r in &rows {
                let state = if r.online {
                    ui::ok("online")
                } else {
                    ui::dim(r.last_seen_at.as_deref().unwrap_or("never seen"))
                };
                println!("  {:36} {:20} {}", r.id, r.display_name, state);
            }
            println!();
            println!(
                "  {}",
                ui::dim("`agit rc revoke <connection>` to remove one")
            );
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            ui::error(&format!("{e}"));
            Ok(ExitCode::Network)
        }
    }
}

fn revoke(args: RevokeArgs) -> CmdResult {
    let c = crate::hub::Client::from_env();
    if !c.has_token() {
        ui::error(&format!("sign in to {} first.", c.base()));
        ui::hint("`agit login`");
        return Ok(ExitCode::Auth);
    }
    match c.rc_revoke(&args.connection) {
        Ok(()) => {
            println!("  {} revoked {}", ui::ok("✓"), args.connection);
            println!(
                "  {}",
                ui::dim("its workspaces are kept and marked disconnected; nothing was deleted")
            );
            Ok(ExitCode::Ok)
        }
        Err(e) => {
            ui::error(&format!("{e}"));
            Ok(ExitCode::Network)
        }
    }
}

fn pair() -> CmdResult {
    let hub = config::hub_url();
    match pair_interactive(&hub)? {
        Some(_) => Ok(ExitCode::Ok),
        None => Ok(ExitCode::Auth),
    }
}

/// Pair by exchanging the user's session token for an RC-scoped one.
///
/// Reuses the existing login rather than inventing a credential system: if the
/// user is not signed in we send them through `agit login`, then ask the hub for
/// a connection token bound to this machine's fingerprint. The RC token is
/// separate from the API token so it can be revoked on its own.
fn pair_interactive(hub: &str) -> crate::Result<Option<identity::Connection>> {
    let c = crate::hub::Client::from_env();
    if !c.has_token() {
        ui::error(&format!(
            "sign in to {hub} first — pairing a machine needs an account."
        ));
        ui::hint("`agit login`");
        return Ok(None);
    }
    let id = identity::identity()?;
    let res = match c.rc_pair(
        &id.machine_fingerprint,
        &id.display_name,
        &crate::rc::platform(),
    ) {
        Ok(r) => r,
        Err(e) => {
            ui::error(&format!("{e}"));
            return Ok(None);
        }
    };
    let conn = identity::Connection {
        connection_id: res.connection_id,
        token: res.token,
        hub: hub.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    identity::save_connection(&conn)?;
    println!(
        "  {} paired as {}",
        ui::ok("✓"),
        ui::accent(&id.display_name)
    );
    Ok(Some(conn))
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
            .split_once("\nfn stop() -> CmdResult {")
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
            vec!["x", "pair"],
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
    fn landing_keeps_the_materialization_baseline_settlement_reads_from() {
        use crate::domain::link::{self, Link};
        use crate::domain::store::Store;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("store"));

        // The link `agit resume`'s slow path registers right after materializing the VIEW.
        let mut resumed = Link::new(
            "claude-code",
            "thread-1",
            Some(std::path::Path::new("/home/alice/code/photo")),
        );
        resumed.agent = Some("photo".into());
        resumed.branch = Some("s-earlier".into());
        resumed.baseline_bytes = Some(4096);
        resumed.baseline_hash = Some("f00d".into());
        link::write(&store, &resumed).unwrap();

        // The daemon takes over the same session by thread id and lands this lineage. The argv
        // comes from the real construction.
        let full = super::land_argv(
            "alice/photo",
            AGENT_ID,
            "s-202608220101-abcd",
            "claude-code",
            "thread-1",
            "/home/alice/code/photo",
        );
        let mut argv = vec!["x".to_string()];
        argv.extend(full.into_iter().skip(1));
        let super::Action::Land(args) = Probe::try_parse_from(&argv).unwrap().cmd else {
            panic!("parsed a different subcommand");
        };
        let landed = super::landed_link(&store, &args, "photo");
        link::write(&store, &landed).unwrap();

        let back = link::get(&store, "claude-code", "thread-1").expect("the link must still exist");
        assert_eq!(
            back.baseline_bytes,
            Some(4096),
            "landing must keep the materialization baseline; without it settlement switches to \
             native continuation and judges every turn already claimed, so this remote session \
             can never commit — land adds to the existing link instead of a fresh Link::new"
        );
        assert_eq!(
            back.baseline_hash.as_deref(),
            Some("f00d"),
            "the baseline hash must survive too; doctor uses it to spot a non-append write to \
             the live transcript below the baseline"
        );
        // The reverse: what the hub says now overrides the old record in the store; otherwise
        // settlement advances the wrong branch.
        assert_eq!(
            back.branch.as_deref(),
            Some("s-202608220101-abcd"),
            "the hub allocates the branch, and landing must write the one for this turn"
        );
        assert_eq!(back.agent.as_deref(), Some("photo"));
        assert_eq!(back.cwd.as_deref(), Some("/home/alice/code/photo"));

        // A first landing (no such link in the store yet) has no baseline to keep — that is a
        // native session, and settlement runs the continuity check. A baseline invented out of
        // nowhere would make it skip that check.
        let empty = Store::at(dir.path().join("never-landed"));
        let fresh = super::landed_link(&empty, &args, "photo");
        assert_eq!(
            fresh.baseline_bytes, None,
            "a native session has no materialization baseline, and landing must not invent one; \
             that would make settlement skip the continuity check"
        );
        assert_eq!(fresh.branch.as_deref(), Some("s-202608220101-abcd"));
        assert_eq!(fresh.cwd.as_deref(), Some("/home/alice/code/photo"));
    }
}
