//! `agit hooks` — the entry point for runtime hooks (a hidden subcommand).
//!
//! Two actions, one for each of the harness's two kinds of event:
//!
//! * **`ingest` (SessionStart)**: a session starts or switches. It is the **only authoritative
//!   answer** to "which session is running now", see below.
//! * **`settle` (Stop)**: a turn ended, so settle it.
//!
//! `settle` also starts a Claude Code session's message relay when it needs one
//! ([`crate::rc::claude_inbox`]); `relay` is that detached process, never installed as a hook.
//!
//! Ordinary hook failures stay quiet. Archive settlement errors return failure so lost evidence
//! cannot be acknowledged as saved. `ingest` may return one valid SessionStart JSON response;
//! sessions that are none of agit's business produce no output.
//!
//! # Why SessionStart is authoritative and `AGIT_SESSION` is not
//!
//! `agit new` / `resume` injects `AGIT_SESSION=<owner>/<name>@<branch>` when it starts the runtime.
//! That variable is **process-level**: after the user runs `/resume` or `/clear` inside the claude
//! / codex TUI, the process is the same and so is the variable, while **the actual session has
//! changed**. Settling by it writes session B's content into session A's history — that is data
//! corruption, not a user-experience problem.
//!
//! The two switch paths have different shapes:
//!
//! * `/clear` mints a new session id and a new transcript file on the spot, wholly independent of
//!   the old one;
//! * `/resume` mints no new id: it returns to the id of **the session being resumed** and keeps
//!   appending to that session's original file;
//! * both switches fire SessionStart, with `source` being `clear` / `resume` respectively;
//! * the payload carries the **new** `session_id` and `transcript_path`.
//!
//! So locating always goes through the payload's `session_id` → store link → branch. That path is
//! equally immune to switch paths we did not anticipate, because it asks "which session did the
//! turn just now happen in", not "who was this process started for".
//!
//! # Fixing the in-session environment variable in the same pass
//!
//! claude-code injects `CLAUDE_ENV_FILE` when it runs a SessionStart hook, pointing at
//! `~/.claude/session-env/<session_id>/sessionstart-hook-<n>.sh`; the shell fragment written there
//! is read as the **session environment**, and every tool call after it carries that environment —
//! once a session has been switched, the `AGIT_SESSION` read inside the session is the value
//! written here. Each hook writes its own file and all of them are concatenated, so writing our own
//! does not fight another hook's.
//!
//! So an agent typing `agit commit` by hand inside the session resolves correctly too — leave this
//! half unfixed and the stale value from step 2 keeps lying to the user's face.
//!
//! # Why the action is a subcommand and not an empty argument list
//!
//! `setup --hooks` writes `agit hooks ingest` into settings.json, so the action has to parse as a
//! subcommand: a struct taking no arguments turns every SessionStart into
//! `error: unexpected argument 'ingest'` + exit code 2, silently swallowed by the harness.
//! `ingest` is the default action, so a bare `agit hooks` stays equivalent and an existing
//! settings.json needs no rewrite.

use super::CmdResult;
use crate::ExitCode;
use crate::domain::{link, link::Link, store::Store, workspace};
use clap::{Args as ClapArgs, Subcommand};
use std::io::Read as _;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub action: Option<Action>,

    /// Which runtime is calling (default: inferred from the payload's transcript path).
    #[arg(long, value_name = "runtime", global = true)]
    pub runtime: Option<String>,
}

#[derive(Subcommand)]
pub enum Action {
    /// SessionStart: record which session is running now (reads the hook JSON on stdin).
    Ingest,
    /// Stop: settle the turn that just ended, located by the payload's session_id.
    Settle,
    /// Deliver a Claude Code session's queued workspace messages while that session runs.
    #[command(hide = true)]
    Relay {
        /// The Claude Code session whose messages to deliver.
        #[arg(long, value_name = "id")]
        session: String,
    },
    /// Register the calling native Claude Code process for shared control.
    #[command(hide = true)]
    NativeControl {
        #[arg(long)]
        session: String,
        #[arg(long)]
        generation: String,
    },
    /// Wait for a shared approval without charging the native hook's execution budget.
    #[command(hide = true)]
    NativeApproval {
        #[arg(long)]
        session: String,
        #[arg(long)]
        generation: String,
        #[arg(long)]
        id: String,
    },
    /// PermissionRequest: offer a native Claude Code permission ask to shared control and print
    /// the answer given there; print nothing to leave the ask to the terminal.
    #[command(hide = true)]
    NativePermission,
    /// Spool native lifecycle metadata while its control channel is disconnected.
    #[command(hide = true)]
    NativeEvents {
        #[arg(long)]
        session: String,
        #[arg(long)]
        generation: String,
    },
    /// Retain an exact completion boundary from the calling native writer.
    #[command(hide = true)]
    NativeComplete {
        #[arg(long)]
        session: String,
        #[arg(long)]
        generation: String,
        #[arg(long)]
        turn: String,
        #[arg(long)]
        reason: String,
    },
}

pub fn run(args: Args) -> CmdResult {
    let runtime = args.runtime.clone();
    match args.action.unwrap_or(Action::Ingest) {
        Action::Ingest => ingest(runtime.as_deref()),
        Action::Settle => settle(runtime.as_deref()),
        Action::Relay { session } => {
            // The relay has no audience; it ends quietly when it cannot or need not run.
            let _ = crate::rc::claude_inbox::run_relay(&session);
            Ok(ExitCode::Ok)
        }
        #[cfg(unix)]
        Action::NativeControl {
            session,
            generation,
        } => {
            let registration = crate::rc::native_claude::register(&session, &generation)?;
            println!(
                "{}",
                serde_json::json!({
                    "registration": registration,
                    "descriptor": registration.descriptor,
                })
            );
            Ok(ExitCode::Ok)
        }
        #[cfg(not(unix))]
        Action::NativeControl { .. } => {
            anyhow::bail!("Native Claude Code shared control is not supported on this platform")
        }
        #[cfg(unix)]
        Action::NativeApproval {
            session,
            generation,
            id,
        } => {
            let reply = crate::rc::native_claude::wait_approval(&session, &generation, &id)?;
            println!("{}", serde_json::to_string(&reply)?);
            Ok(ExitCode::Ok)
        }
        #[cfg(not(unix))]
        Action::NativeApproval { .. } => {
            anyhow::bail!("Native Claude Code shared control is not supported on this platform")
        }
        Action::NativePermission => {
            native_permission();
            Ok(ExitCode::Ok)
        }
        #[cfg(unix)]
        Action::NativeEvents {
            session,
            generation,
        } => {
            let reply = crate::rc::native_claude::event_queue::exchange(&session, &generation)?;
            println!("{}", serde_json::to_string(&reply)?);
            Ok(ExitCode::Ok)
        }
        #[cfg(not(unix))]
        Action::NativeEvents { .. } => {
            anyhow::bail!("Native Claude Code shared control is not supported on this platform")
        }
        #[cfg(unix)]
        Action::NativeComplete {
            session,
            generation,
            turn,
            reason,
        } => {
            let receipt = crate::rc::native_claude::completion::record(
                &session,
                &generation,
                &turn,
                &reason,
            )?;
            println!("{}", serde_json::to_string(&receipt)?);
            Ok(ExitCode::Ok)
        }
        #[cfg(not(unix))]
        Action::NativeComplete { .. } => {
            anyhow::bail!("Native Claude Code shared control is not supported on this platform")
        }
    }
}

// ── payload ───────────────────────────────────────────────────────────

/// SessionStart's `source`.
///
/// The shared entry reasons cover Claude Code and Codex; Claude also reports native forks.
/// Anything unrecognized becomes `Other` — when a value we have never seen appears, the safe
/// behavior is "register without claiming", not guessing startup and claiming a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Startup,
    Resume,
    Clear,
    Fork,
    Compact,
    Other,
}

impl Source {
    fn parse(s: Option<&str>) -> Source {
        match s {
            Some("startup") => Source::Startup,
            Some("resume") => Source::Resume,
            Some("clear") => Source::Clear,
            Some("fork") => Source::Fork,
            Some("compact") => Source::Compact,
            _ => Source::Other,
        }
    }
}

/// A permission ask's answer from shared control, as the PermissionRequest hook output; nothing
/// when shared control gives none, so a failure never turns into an answer the person did not
/// give. A payload over the bound does not parse, and so decides nothing.
fn native_permission() {
    #[cfg(unix)]
    {
        const PAYLOAD_LIMIT: u64 = 4 * 1024 * 1024;
        let mut buf = String::new();
        if std::io::stdin()
            .take(PAYLOAD_LIMIT)
            .read_to_string(&mut buf)
            .is_ok()
            && let Ok(payload) = serde_json::from_str(&buf)
            && let Some(decision) = crate::rc::native_claude::permission::decide(&payload)
        {
            println!(
                "{}",
                serde_json::json!({"hookSpecificOutput":
                    {"hookEventName":"PermissionRequest","decision":decision}})
            );
        }
    }
}

/// The few facts the hook gets from stdin.
struct Event {
    session_id: String,
    cwd: Option<String>,
    transcript_path: Option<String>,
    source: Source,
    session_title: Option<String>,
    hook_event_name: Option<String>,
    permission_mode: Option<String>,
}

fn parse_event(buf: &str) -> Option<Event> {
    let v: serde_json::Value = serde_json::from_str(buf).ok()?;
    let sid = v.get("session_id")?.as_str()?.to_string();
    if sid.is_empty() {
        return None;
    }
    let get = |k: &str| {
        v.get(k)
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    Some(Event {
        session_id: sid,
        cwd: get("cwd"),
        transcript_path: get("transcript_path"),
        source: Source::parse(v.get("source").and_then(|s| s.as_str())),
        session_title: get("session_title"),
        hook_event_name: get("hook_event_name"),
        permission_mode: get("permission_mode"),
    })
}

/// Whether this hook starts the session's message relay. A Claude Code session receives
/// workspace messages through its own process, and its hook is the only place the session's
/// messaging token is visible. The relay delivers the owner's messages past the approval hold
/// that Claude Code applies only in bypass-permissions mode, so it starts only where the payload
/// reports that live mode: a Stop. A SessionStart payload reports no mode.
fn starts_inbox_relay(runtime: Option<&str>, event: &Event) -> bool {
    runtime_of(runtime, event.transcript_path.as_deref()) == "claude-code"
        && event.hook_event_name.as_deref() == Some("Stop")
        && crate::rc::claude_inbox::relay_serves(event.permission_mode.as_deref())
}

fn read_event(runtime: Option<&str>) -> Option<Event> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    let mut event = parse_event(&buf)?;
    match runtime.and_then(|runtime| crate::adapter::normalize(runtime).ok()) {
        Some("workbuddy") if event.cwd.is_none() => {
            event.cwd = std::env::var("CODEBUDDY_PROJECT_DIR")
                .ok()
                .filter(|cwd| !cwd.is_empty());
        }
        Some("hermes") => {
            let value: serde_json::Value = serde_json::from_str(&buf).ok()?;
            if value["hook_event_name"] == "on_session_start" {
                event.source = Source::Startup;
            }
        }
        _ => {}
    }
    Some(event)
}

/// Which runtime is calling us.
///
/// An explicit `--runtime` wins; otherwise the transcript path decides — it is a fact the payload
/// carries itself, more reliable than every settings.json remembering to spell a flag right. When
/// nothing answers, treat it as claude-code: it is what installs that hook into a settings.json.
fn runtime_of(explicit: Option<&str>, transcript_path: Option<&str>) -> &'static str {
    if let Some(r) = explicit
        && let Ok(n) = crate::adapter::normalize(r)
    {
        return n;
    }
    match transcript_path {
        Some(p) if p.contains("/.codex/") => "codex",
        Some(p) if p.contains("/.claude/") => "claude-code",
        _ => "claude-code",
    }
}

/// The directory the session started in, which decides whether a project captures it.
///
/// A hook's `cwd` follows the session's current directory, so a session started elsewhere that
/// later runs commands inside a project would otherwise be adopted into that project's
/// repository. The transcript's first recorded directory is the start. A session that is only now
/// starting may have no transcript yet, and a runtime that reports no transcript offers nothing
/// better; in those cases the reported directory is the start. Any other event without a recorded
/// directory has no known start and is captured by no project.
fn session_origin(ev: &Event) -> Option<String> {
    match ev.transcript_path.as_deref() {
        Some(path) => crate::tui::screens::selector::recorded_cwd(std::path::Path::new(path))
            .or_else(|| {
                matches!(ev.source, Source::Startup | Source::Clear)
                    .then(|| ev.cwd.clone())
                    .flatten()
            }),
        None => ev.cwd.clone(),
    }
}

// ── ingest (SessionStart) ─────────────────────────────────────────────

/// What this SessionStart does with this session.
///
/// A pure function so that **the decision itself** can be asserted on: it has four branches, and in
/// three of them the cost of being wrong is silent data corruption (claiming the wrong branch /
/// not claiming when it should / not clearing a stale environment when it should).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Claim this session id with the branch `AGIT_SESSION` names.
    Claim,
    /// Record existence only (link-only); the binding is left for the user to give explicitly.
    Register,
    /// Touch nothing.
    Nothing,
}

/// * `env_session`: whether `AGIT_SESSION` is present (only a session we started has it).
/// * `known`: whether the store already holds a link for this session id.
/// * `bound`: whether that existing link already has a binding (agent + branch).
pub fn decide(source: Source, env_session: bool, known: bool, bound: bool) -> Decision {
    match source {
        // compact is the same session with its context compacted. The binding does not change —
        // the compact boundary is handled by the VIEW logic at settlement, which already knows it.
        Source::Compact => Decision::Nothing,

        // The session we just started: `AGIT_SESSION` names exactly it, so claim it.
        //
        // **Claim only on startup.** On resume, `AGIT_SESSION` names the session current at
        // launch, while the id in the payload is the **other** session the user just switched to —
        // claiming from it binds two sessions to the same branch.
        Source::Startup if env_session && !bound => Decision::Claim,

        // Already bound (both the fast and the slow resume path write the link before launch):
        // do nothing.
        _ if bound => Decision::Nothing,

        // Everything else — a claude the user started themselves, a new session out of `/clear`,
        // a `/resume` into a session that was never adopted — is registered as awaiting a name.
        // This does not break "adoption is explicit": link-only is precisely the "marked, but not
        // yet under version control" state, and real adoption still takes a name from the user.
        _ if !known => Decision::Register,

        _ => Decision::Nothing,
    }
}

/// Whether this session is any of agit's business.
///
/// A bound directory or injected identity permits registration of a new session. Ingest also
/// checks the exact link for an already adopted session, regardless of its current directory.
///
/// # Why not "register everything"
///
/// "Registering" sounds harmless — a JSON record of a few dozen bytes. But the store is not
/// write-only: stale-environment checks in [`crate::commands::context`] traverse registered
/// links. Registering everything lets the store grow without bound with "the
/// total number of sessions ever started in any directory on this machine", so every command slows
/// down with it and never comes back down.
///
/// The cost grows with the number of links, and under "register everything" that number is the
/// total number of sessions ever started on this machine — it only goes up, never down. So this is
/// not "a bit slower", it is a permanent rise in the floor of every command.
///
/// This is exactly the shape `docs/07_tui.md` §4.1 forbids with "cost must not scale with count".
///
/// # How a session in an unbound directory is seen
///
/// The TUI scans the runtime index itself (`sessions_for(cwd)` in `tui::screens::sessions`); that
/// path does not care about binding, and its answer is complete. Registering fewer links on the
/// hook side costs the user nothing they can see — **discovery** is the index's job, not the
/// store's. The store records only "what the user selected".
pub fn concerns_us(bound: bool, env_session: bool) -> bool {
    bound || env_session
}

fn ingest(runtime: Option<&str>) -> CmdResult {
    // Any failure exits 0 silently. The only stdout allowed here is one complete response the
    // hook harness understands; diagnostics would corrupt that protocol.
    if let Some(response) = ingest_inner(runtime) {
        println!("{response}");
    }
    Ok(ExitCode::Ok)
}

fn ingest_inner(runtime: Option<&str>) -> Option<serde_json::Value> {
    // The supervisor lands its exact native identity. Runtime helper processes
    // inherit its environment but cannot claim its conversation branch.
    if crate::rc::harness::settlement_is_delegated() {
        return None;
    }
    let ev = read_event(runtime)?;
    let rt = runtime_of(runtime, ev.transcript_path.as_deref());
    let restored = restored_title(rt, &ev);
    let response = session_start(rt, &ev);
    let Some(title) = restored else {
        return response;
    };
    let mut response = response.unwrap_or_else(
        || serde_json::json!({"hookSpecificOutput": {"hookEventName": "SessionStart"}}),
    );
    response["hookSpecificOutput"]["sessionTitle"] = serde_json::Value::String(title);
    Some(response)
}

/// Claude Code's own title for a session that an earlier agit renamed with a placeholder: a name
/// chosen before the placeholder, the title Claude Code generated, its summary, or else the label
/// Claude Code lists an untitled session by. Hook input names the current title, so any other
/// title is left alone, whether or not agit manages the session now.
fn restored_title(rt: &str, ev: &Event) -> Option<String> {
    if rt != "claude-code"
        || !matches!(ev.source, Source::Startup | Source::Resume | Source::Fork)
        || !ev
            .session_title
            .as_deref()
            .is_some_and(crate::adapter::native_title::agit_placeholder)
    {
        return None;
    }
    let transcript = std::fs::read(ev.transcript_path.as_deref()?).ok()?;
    let transcript = String::from_utf8_lossy(&transcript);
    crate::adapter::native_title::native_title(rt, &ev.session_id, &transcript, None).or_else(
        || {
            Some(crate::adapter::native_title::claude_untitled_label(
                &transcript,
                crate::adapter::native_title::claude_user_config().as_deref(),
            ))
        },
    )
}

/// Record which session started and describe its state to the agent.
fn session_start(rt: &'static str, ev: &Event) -> Option<serde_json::Value> {
    let env_session = super::context::from_session_env();

    let dir = ev.cwd.as_deref().map(std::path::Path::new);
    let origin = session_origin(ev);
    let origin = origin.as_deref().map(std::path::Path::new);
    let bound = origin
        .map(|d| workspace::read(d).is_some() || super::project::registration_scope(d))
        .unwrap_or(false);
    if rt == "codex" && ev.source != Source::Compact {
        let _ = crate::rc::runtime_sources::Registry::open()
            .and_then(|registry| registry.enroll_default());
    }
    crate::telemetry::runtime(rt);
    crate::telemetry::observe(crate::telemetry::Observation::WorkspaceBound(bound));
    let existing = Store::open()
        .ok()
        .flatten()
        .and_then(|store| link::get(&store, rt, &ev.session_id));
    let has_binding = existing
        .as_ref()
        .map(|l| l.agent.is_some() && l.branch.is_some())
        .unwrap_or(false);
    if !has_binding && !concerns_us(bound, env_session.is_some()) {
        return None;
    }

    let store = Store::open_or_init().ok()?;
    let _branch_guard = if ev.source == Source::Startup {
        env_session
            .as_ref()
            .map(|(slug, branch)| link::lock_branch(&store, slug, branch))
            .transpose()
            .ok()?
    } else {
        None
    };
    let _link_guard = link::lock(&store, rt, &ev.session_id).ok()?;
    // The initial lookup only decides whether to enter. Ownership must be reread under the
    // branch and link locks so concurrent imports cannot be overwritten by a stale claim.
    let existing = link::get(&store, rt, &ev.session_id);
    let has_binding = existing
        .as_ref()
        .is_some_and(|link| link.agent.is_some() && link.branch.is_some());
    if !has_binding && !concerns_us(bound, env_session.is_some()) {
        return None;
    }

    let env_value = match decide(
        ev.source,
        env_session.is_some(),
        existing.is_some(),
        has_binding,
    ) {
        Decision::Claim => {
            let (repo, branch) = env_session.clone()?;
            let mut lk = existing.unwrap_or_else(|| Link::new(rt, &ev.session_id, dir));
            if lk.cwd.is_none() {
                lk.cwd = ev.cwd.clone();
            }
            // The agent name and the namespace are recorded apart: the slug in `AGIT_SESSION` is
            // complete, and splitting it keeps later Stop settlements and SessionStarts from
            // filling an organization's repo in as the signed-in account's.
            match super::parse_slug(&repo) {
                Ok((owner, agent)) => {
                    lk.owner = Some(owner);
                    lk.agent = Some(agent);
                }
                Err(_) => lk.agent = Some(repo.clone()),
            }
            lk.branch = Some(branch.clone());
            link::write(&store, &lk)
                .ok()
                .and_then(|_| session_env_value(&lk))
        }
        Decision::Register => {
            let _ = link::write(&store, &Link::new(rt, &ev.session_id, origin));
            None
        }
        Decision::Nothing => link::get(&store, rt, &ev.session_id)
            .as_ref()
            .and_then(session_env_value),
    };

    // Only this session's complete claim may propagate. An incomplete claim clears any
    // inherited AGIT_SESSION instead of making the next command inherit another identity.
    write_session_env(&ev.session_id, env_value.as_deref());
    let mut response = session_annotation(rt, ev.source, &ev.session_id, env_value.as_deref())?;
    if matches!(ev.source, Source::Startup | Source::Resume)
        && let (Some(binding), Some(cwd)) = (env_value.as_deref(), dir)
        && let Some(context) = recorded_environment_context(binding, cwd)
        && let Some(serde_json::Value::String(existing)) =
            response["hookSpecificOutput"].get_mut("additionalContext")
    {
        existing.push_str("\n\n");
        existing.push_str(&context);
    }
    if announces_upload(rt, ev.source, has_binding)
        && let Some(upload) = origin.and_then(|origin| {
            super::project::automatic_upload(rt, &ev.session_id, origin, UPLOAD_VISIBILITY_WAIT)
        })
    {
        response["systemMessage"] = serde_json::Value::String(upload_notice(&upload));
        let automatic = format!(
            "This folder uploads this conversation to {} at the end of each turn. If the user does not want it uploaded, run `agit project drop` before the turn ends.",
            upload.repository
        );
        // An unclaimed conversation is told how to import itself; one the project uploads must not
        // be imported a second time, so that advice is replaced rather than contradicted.
        let context = match env_value.as_deref() {
            Some(_) => response["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .map(|existing| format!("{existing}\n\n{automatic}")),
            None => Some(format!(
                "This {rt} session ({}) is saved automatically; do not import it. {automatic}",
                sh_quote(&ev.session_id)
            )),
        };
        if let Some(context) = context {
            response["hookSpecificOutput"]["additionalContext"] =
                serde_json::Value::String(context);
        }
    }
    Some(response)
}

/// How long a SessionStart waits for the Hub to confirm who can see the destination.
const UPLOAD_VISIBILITY_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The person learns that a conversation will be uploaded before its first turn ends: when it
/// starts, fresh or as a fork of another (a fork is a new conversation the project uploads on its
/// own), and when it is resumed before anything was uploaded. Both runtimes show a hook's
/// `systemMessage` to the person rather than to the agent.
fn announces_upload(runtime: &str, source: Source, uploading: bool) -> bool {
    matches!(runtime, "claude-code" | "codex")
        && match source {
            Source::Startup | Source::Clear | Source::Fork => true,
            Source::Resume => !uploading,
            _ => false,
        }
}

/// One line naming the destination, who can see it, and both ways to stop the upload.
fn upload_notice(upload: &super::project::AutomaticUpload) -> String {
    let owner = upload.repository.split('/').next().unwrap_or_default();
    let audience = match (upload.visibility.as_deref(), upload.org) {
        (Some("public"), _) => "public: anyone can read it".to_owned(),
        (Some("private"), Some(true)) if upload.members_read == Some(true) => {
            format!("private to {owner}: every member of {owner} can read it")
        }
        (Some("private"), Some(true)) => {
            format!("private to {owner}: its owners and the people granted access can read it")
        }
        (Some("private"), Some(false)) => {
            "private: only you and people you add can read it".to_owned()
        }
        (Some(visibility), _) => visibility.to_owned(),
        (None, _) => "visibility not confirmed".to_owned(),
    };
    format!(
        "agit: this conversation is uploaded to {} ({audience}) at the end of each turn. Not this one? Say so, or run `agit project drop`.",
        upload.repository
    )
}

/// The SessionStart response the runtime may show or add to the conversation.
///
/// It never names the session: the runtime's own title, generated or chosen by the user, is the
/// session's name, and agit reads it from the runtime. Claude Code, Codex and WorkBuddy accept
/// binding context. The context names the native session id: a runtime that exports no
/// session variable to its tool shell leaves the agent no other way to learn which conversation
/// it is, and an agent that cannot tell imports whichever session looks unsaved.
fn session_annotation(
    runtime: &str,
    source: Source,
    session_id: &str,
    binding: Option<&str>,
) -> Option<serde_json::Value> {
    if !matches!(
        source,
        Source::Startup | Source::Resume | Source::Clear | Source::Fork
    ) {
        return None;
    }

    let additional_context = match binding {
        Some(binding) => format!(
            "This {runtime} session ({}) is already under agit version control at {binding}. When the user asks to save its new turns, run `agit commit {quoted}`; publish them with `agit push {quoted}` only when the user asks to. Use that explicit repo and branch for session commands, since a launch-time AGIT_SESSION may refer to a session switched away from. Do not import this session again, and do not import another session in its place.",
            sh_quote(session_id),
            quoted = sh_quote(binding)
        ),
        None => format!(
            "This runtime session has no active agit branch. If the user asks to save, name, or publish it, ask for the target if needed, then run `agit import {} --from {runtime} --into <owner/repo>@<branch>`.",
            sh_quote(session_id)
        ),
    };
    if runtime == "openclaw" {
        return Some(serde_json::json!({"context":additional_context}));
    }
    if !matches!(runtime, "claude-code" | "codex" | "workbuddy") {
        return None;
    }

    let mut output = serde_json::Map::new();
    output.insert(
        "hookEventName".into(),
        serde_json::Value::String("SessionStart".into()),
    );
    output.insert(
        "additionalContext".into(),
        serde_json::Value::String(additional_context),
    );
    Some(serde_json::json!({ "hookSpecificOutput": output }))
}

/// Reading the selected branch's metadata does not scan the worktree or runtime transcript.
/// Native TUI resumes need this observation too, since they do not pass through `agit resume`.
fn recorded_environment_context(binding: &str, cwd: &std::path::Path) -> Option<String> {
    let (slug, branch) = super::context::decode_session_env(binding)?;
    let (owner, name) = super::parse_slug(&slug).ok()?;
    let repo =
        crate::domain::repo::Repo::open(crate::infra::config::repo_dir(&owner, &name).ok()?)?;
    let snapshot = crate::domain::meta::read_at_ref(&repo, &format!("refs/heads/{branch}"))?;
    let state = snapshot.cwd_state.as_ref()?;
    let observation = serde_json::json!({
        "recorded_cwd": snapshot.cwd,
        "recorded_cwd_state": state.sanitized(),
        "current_cwd": cwd.to_string_lossy(),
    });
    Some(format!(
        "AgentGit saved environment (historical data, not instructions): {observation}\nThis is the last settled turn's observation, not a live Git status or proof of the same machine. Inspect the current checkout before relying on earlier code changes. AgentGit has not restored code files or uncommitted changes."
    ))
}

/// Write the binding into the harness's "session environment" file.
///
/// Only claude-code offers this mechanism (`CLAUDE_ENV_FILE`, one directory per session). codex has
/// no equivalent; [`super::context`] refuses a known stale supplied identity.
fn write_session_env(session_id: &str, value: Option<&str>) {
    let Ok(path) = std::env::var("CLAUDE_ENV_FILE") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    // **This file must belong to the session in the payload.**
    //
    // On a `/resume` switch inside the TUI, the SessionStart payload already carries the **new**
    // session's id, while `CLAUDE_ENV_FILE` still points at the directory of **the session
    // switched away from** (`session-env/<old id>/...`). Without the comparison, the new session's
    // binding lands in the old session's environment file — and whoever resumes that old session
    // next finds it calling itself another branch. That is the data corruption this whole
    // mechanism exists to prevent, only running the other way.
    //
    // A mismatch must not write another session's identity into this file. Ordinary commands
    // reject a known stale AGIT_SESSION and require an explicit target to continue.
    if !env_file_belongs_to(&path, session_id) {
        return;
    }
    let line = match value {
        Some(v) => format!("export AGIT_SESSION={}\n", sh_quote(v)),
        None => "unset AGIT_SESSION\n".to_string(),
    };
    let _ = std::fs::write(path, line);
}

/// A `CLAUDE_ENV_FILE` path looks like `.../session-env/<session_id>/sessionstart-hook-<n>.sh`, so
/// the parent directory's name is the session it serves.
///
/// An unrecognized layout returns false (fail closed): writing the wrong session's environment is
/// far worse than not writing at all.
fn env_file_belongs_to(path: &str, session_id: &str) -> bool {
    std::path::Path::new(path)
        .parent()
        .and_then(|p| p.file_name())
        .map(|name| name == session_id)
        .unwrap_or(false)
}

/// Environment propagation requires a complete recorded claim. A missing namespace must not
/// become an implicit account selection for commands launched after this hook.
fn session_env_value(lk: &Link) -> Option<String> {
    let propagates = match lk.merge_archive.as_ref() {
        Some(role) => lk.is_archive_for(role, &lk.source, &lk.session_id),
        None => lk.is_active(),
    };
    if !propagates {
        return None;
    }
    let branch = lk.branch.as_deref()?;
    let slug = format!("{}/{}", lk.owner.as_deref()?, lk.agent.as_deref()?);
    let v = super::context::encode_session_env(&slug, branch);
    super::context::decode_session_env(&v).map(|_| v)
}

/// Close in single quotes, escaping embedded ones. The harness sources this line as a shell
/// fragment.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ── settle (Stop) ─────────────────────────────────────────────────────

fn settle(runtime: Option<&str>) -> CmdResult {
    // The gate sits before reading the payload and opening the store: inside a process tree
    // delegated to the supervisor, the hook touches no local state at all — the branch moves only
    // within the supervisor's lease.
    if super::commit::delegated_settlement(true)?.is_some() {
        return Ok(ExitCode::Ok);
    }
    match settle_inner(runtime) {
        Err(error) if super::commit::archive::is_failure(&error) => {
            crate::ui::error(&format!("{error:#}"));
            Ok(super::terminal_error_code(&error, ExitCode::Failure))
        }
        _ => Ok(ExitCode::Ok),
    }
}

fn settle_inner(runtime: Option<&str>) -> crate::Result<()> {
    let event = read_event(runtime);
    if let Some(ev) = &event
        && starts_inbox_relay(runtime, ev)
    {
        crate::rc::claude_inbox::start_relay(&ev.session_id);
    }
    #[cfg(unix)]
    if let Some(ev) = &event
        && let Some(mode) = &ev.permission_mode
        && runtime_of(runtime, ev.transcript_path.as_deref()) == "claude-code"
    {
        crate::rc::native_claude::permission::report_mode(&ev.session_id, mode);
    }
    if super::config::get("commit.auto").as_deref() == Some("false") {
        return Ok(());
    }
    let Some(ev) = event else {
        super::commit::archive::require_hook_identity(false)?;
        return Ok(());
    };
    let rt = runtime_of(runtime, ev.transcript_path.as_deref());
    let (store, archive_required) = match super::commit::archive::process_store()? {
        Some(store) => (store, true),
        None => (Store::open_or_init()?, false),
    };
    if !archive_required
        && super::project::capture(rt, &ev.session_id, session_origin(&ev).as_deref())?
    {
        return Ok(());
    }
    let Some(lk) = super::commit::archive::native_link(
        &store,
        &crate::domain::merge_archive::RuntimeLinkKey {
            runtime: rt.into(),
            session_id: ev.session_id,
        },
    )?
    else {
        if archive_required {
            return super::commit::archive::checked(Err(anyhow::anyhow!(
                "the selected Archive hook native Link is missing"
            )));
        }
        return Ok(());
    };
    super::commit::settle_from_link(&store, lk)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Decision, Source, decide};
    use clap::Parser as _;

    /// Every form the harness runs must parse. A form that does not parse is
    /// `unexpected argument 'ingest'` + exit code 2 on every trigger, and hook failures are
    /// silent, so "register awaiting adoption" never works at all.
    #[test]
    fn every_installed_form_parses() {
        for argv in [
            vec!["agit", "hooks", "ingest"],
            vec!["agit", "hooks", "settle"],
            vec!["agit", "hooks"],
            vec!["agit", "hooks", "ingest", "--runtime", "codex"],
            vec!["agit", "hooks", "settle", "--runtime", "codex"],
            // Spawned by the Stop hook rather than installed, but parsed the same way.
            vec![
                "agit",
                "hooks",
                "relay",
                "--session",
                "8e1d7c3a-4f0b-4a51-9a8e-2f6b1c0d9e47",
            ],
        ] {
            crate::commands::Cli::try_parse_from(&argv)
                .unwrap_or_else(|e| panic!("`{}` must parse: {e}", argv.join(" ")));
        }
        // An unknown action is rejected outright, never silently taken as ingest.
        assert!(crate::commands::Cli::try_parse_from(["agit", "hooks", "nope"]).is_err());
    }

    /// A session in an unbound directory that we did not start — not one byte is written.
    ///
    /// This pins the size of the store, not the hook's own latency: the store is traversed in full
    /// on the path of every command, and once it grows with the total number of sessions on this
    /// machine it never comes back down.
    #[test]
    fn a_session_that_is_none_of_our_business_is_not_recorded() {
        assert!(!super::concerns_us(false, false));
        // A bound directory: the user does agit work here.
        assert!(super::concerns_us(true, false));
        // A session we started ourselves, even in a directory that was never bound.
        assert!(super::concerns_us(false, true));
        assert!(super::concerns_us(true, true));
    }

    /// A project captures sessions started inside it, so the hook judges a session by the
    /// directory its transcript first records. An implementation that read the hook's `cwd`
    /// would hand a session that merely moved into the project to that project's repository, and
    /// one that fell back to the hook's `cwd` whenever the transcript records no directory would
    /// do the same for a later event.
    #[test]
    fn a_session_is_placed_by_the_directory_it_started_in() {
        let temp = tempfile::tempdir().unwrap();
        let transcript = temp.path().join("session.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"summary","summary":"x"}"#,
                "\n",
                r#"{"type":"user","cwd":"/home/u"}"#,
                "\n",
                r#"{"type":"user","cwd":"/home/u/work/project"}"#,
                "\n"
            ),
        )
        .unwrap();
        let event = |transcript: &std::path::Path, source: &str| {
            super::parse_event(
                &serde_json::json!({
                    "session_id": "4faae39f-0000-4000-8000-000000000000",
                    "transcript_path": transcript,
                    "cwd": "/home/u/work/project",
                    "hook_event_name": "Stop",
                    "source": source,
                })
                .to_string(),
            )
            .unwrap()
        };
        assert_eq!(
            super::session_origin(&event(&transcript, "")).as_deref(),
            Some("/home/u")
        );
        let unwritten = temp.path().join("unwritten.jsonl");
        assert_eq!(super::session_origin(&event(&unwritten, "")), None);
        assert_eq!(
            super::session_origin(&event(&unwritten, "startup")).as_deref(),
            Some("/home/u/work/project")
        );
    }

    #[test]
    fn source_values_come_from_both_runtimes() {
        // These four values are common to claude-code and codex.
        assert_eq!(Source::parse(Some("startup")), Source::Startup);
        assert_eq!(Source::parse(Some("resume")), Source::Resume);
        assert_eq!(Source::parse(Some("clear")), Source::Clear);
        assert_eq!(Source::parse(Some("fork")), Source::Fork);
        assert_eq!(Source::parse(Some("compact")), Source::Compact);
        // An unseen value must not be guessed as startup — that would claim a branch.
        assert_eq!(Source::parse(Some("teleport")), Source::Other);
        assert_eq!(Source::parse(None), Source::Other);
    }

    /// The parser accepts the fields emitted by Codex hooks and ignores event-specific additions.
    /// SessionStart and Stop carry different optional fields, so requiring their full objects to
    /// match would make settlement disappear when Codex adds metadata.
    #[test]
    fn live_codex_payload_shapes_parse_to_the_same_session() {
        let start = super::parse_event(
            r#"{
                "session_id":"01a062f9-e70d-78b1-b14e-c48fa1cea69b",
                "transcript_path":"/tmp/codex/sessions/rollout.jsonl",
                "cwd":"/tmp/work",
                "hook_event_name":"SessionStart",
                "model":"gpt-5.6-sol",
                "permission_mode":"bypassPermissions",
                "source":"startup"
            }"#,
        )
        .expect("SessionStart payload must parse");
        let stop = super::parse_event(
            r#"{
                "session_id":"01a062f9-e70d-78b1-b14e-c48fa1cea69b",
                "transcript_path":"/tmp/codex/sessions/rollout.jsonl",
                "cwd":"/tmp/work",
                "hook_event_name":"Stop",
                "last_assistant_message":"CODEX_HOOK_PROBE_OK",
                "model":"gpt-5.6-sol",
                "permission_mode":"bypassPermissions",
                "stop_hook_active":false,
                "turn_id":"01a062f9-e760-7362-b0c0-de319632e620"
            }"#,
        )
        .expect("Stop payload must parse");

        assert_eq!(start.session_id, stop.session_id);
        assert_eq!(start.cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(stop.cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(
            start.transcript_path.as_deref(),
            Some("/tmp/codex/sessions/rollout.jsonl")
        );
        assert_eq!(start.transcript_path, stop.transcript_path);
        assert_eq!(start.source, Source::Startup);
        assert_eq!(stop.source, Source::Other);
    }

    /// The message relay delivers the owner's messages as the session itself, past the hold
    /// Claude Code applies in bypass-permissions mode, so only a Claude Code Stop that reports
    /// that live mode starts one. A relay started from SessionStart, from a Stop in another or an
    /// unreported mode, or for another runtime fails here.
    #[test]
    fn only_a_bypass_permissions_claude_stop_starts_the_inbox_relay() {
        let event = |hook: &str, mode: Option<&str>| {
            super::parse_event(
                &serde_json::json!({
                    "session_id": "8e1d7c3a-4f0b-4a51-9a8e-2f6b1c0d9e47",
                    "transcript_path": "/home/me/.claude/projects/-work/8e1d7c3a-4f0b-4a51-9a8e-2f6b1c0d9e47.jsonl",
                    "cwd": "/work",
                    "hook_event_name": hook,
                    "permission_mode": mode,
                })
                .to_string(),
            )
            .unwrap()
        };
        assert!(super::starts_inbox_relay(
            None,
            &event("Stop", Some("bypassPermissions"))
        ));
        for (runtime, hook, mode) in [
            (None, "SessionStart", Some("bypassPermissions")),
            (None, "SessionStart", None),
            (None, "Stop", Some("default")),
            (None, "Stop", Some("acceptEdits")),
            (None, "Stop", None),
            (Some("codex"), "Stop", Some("bypassPermissions")),
        ] {
            assert!(
                !super::starts_inbox_relay(runtime, &event(hook, mode)),
                "{runtime:?} {hook} {mode:?} must not start a relay"
            );
        }
    }

    /// Claiming happens only on startup.
    ///
    /// On `/resume`, `AGIT_SESSION` names the session current **at launch**, while the id in the
    /// payload is the other session the user just switched to. Claiming here binds two sessions to
    /// the same branch — the data corruption this pins against.
    #[test]
    fn only_startup_claims_the_branch_from_the_environment() {
        assert_eq!(
            decide(Source::Startup, true, false, false),
            Decision::Claim,
            "a session we started ourselves is claimed"
        );
        for s in [Source::Resume, Source::Clear, Source::Fork] {
            assert_eq!(
                decide(s, true, false, false),
                Decision::Register,
                "{s:?}: AGIT_SESSION names another session, so only register"
            );
        }
    }

    /// A session that already has a binding is untouched, whatever the source.
    ///
    /// Both the fast and the slow path of `agit resume` write agent/branch into the link **before**
    /// launch, and SessionStart arrives right behind them with `source=resume`; overwriting here
    /// loses the materialization baseline the slow path has just recorded.
    #[test]
    fn an_already_bound_session_is_never_touched() {
        for s in [
            Source::Startup,
            Source::Resume,
            Source::Clear,
            Source::Fork,
            Source::Compact,
            Source::Other,
        ] {
            assert_eq!(decide(s, true, true, true), Decision::Nothing, "{s:?}");
        }
    }

    /// compact is not a session switch; it is the same session compacted.
    #[test]
    fn compact_changes_no_binding() {
        assert_eq!(
            decide(Source::Compact, true, false, false),
            Decision::Nothing
        );
        assert_eq!(
            decide(Source::Compact, false, false, false),
            Decision::Nothing
        );
    }

    /// A foreign session (no `AGIT_SESSION`) is registered once and not written again.
    #[test]
    fn a_foreign_session_is_registered_once() {
        assert_eq!(
            decide(Source::Startup, false, false, false),
            Decision::Register
        );
        assert_eq!(
            decide(Source::Startup, false, true, false),
            Decision::Nothing
        );
    }

    #[test]
    fn runtime_comes_from_the_transcript_path_when_not_told() {
        let cc = "/home/me/.claude/projects/x/abc.jsonl";
        let cx = "/home/me/.codex/sessions/2026/08/26/rollout-x.jsonl";
        assert_eq!(super::runtime_of(None, Some(cc)), "claude-code");
        assert_eq!(super::runtime_of(None, Some(cx)), "codex");
        // An explicit flag beats inference.
        assert_eq!(super::runtime_of(Some("codex"), Some(cc)), "codex");
        // With nothing to go on, fall back to the runtime that installs this hook into
        // settings.json.
        assert_eq!(super::runtime_of(None, None), "claude-code");
    }

    /// A managed session exposes its exact destination without adding instructions that could
    /// make an agent attempt to adopt it again, or save or publish it without being asked: an
    /// agent told to publish pushes mid-turn without being asked.
    #[test]
    fn a_managed_session_learns_its_binding() {
        let response = super::session_annotation(
            "claude-code",
            Source::Resume,
            "SID-A",
            Some("einsia/payments@flaky-test"),
        )
        .unwrap();
        let context = response["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.contains("'einsia/payments@flaky-test'"));
        assert!(context.contains("Do not import this session again"));
        assert!(
            context.contains("When the user asks to save its new turns")
                && context.contains("only when the user asks to"),
            "{context}"
        );
    }

    /// An unmanaged session gets both the visible state and a command containing its real runtime
    /// identity. Quoting the id keeps the suggested shell command one argument even if a future
    /// runtime widens its id alphabet.
    #[test]
    fn an_unmanaged_session_gets_actionable_adoption_context() {
        let response =
            super::session_annotation("claude-code", Source::Clear, "sid with ' quote", None)
                .unwrap();
        let output = &response["hookSpecificOutput"];
        assert_eq!(output["hookEventName"], "SessionStart");
        assert!(output.get("sessionTitle").is_none());
        let context = output["additionalContext"].as_str().unwrap();
        assert!(
            context.contains(
                "agit import 'sid with '\\'' quote' --from claude-code --into <owner/repo>@<branch>"
            ),
            "{context}"
        );
    }

    /// Codex accepts the same hook envelope and adoption context, but its SessionStart schema has
    /// no title field. Adding Claude's field makes the whole response invalid instead of merely
    /// ignoring the field, so the two payloads must differ exactly here.
    #[test]
    fn an_unmanaged_codex_session_gets_context_without_a_title() {
        assert_eq!(
            super::session_annotation("codex", Source::Startup, "SID-C", None),
            Some(serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": "This runtime session has no active agit branch. If the user asks to save, name, or publish it, ask for the target if needed, then run `agit import 'SID-C' --from codex --into <owner/repo>@<branch>`.",
                }
            }))
        );
    }

    /// Compacting continues the same conversation, so it gets no context again. Unknown runtimes
    /// also stay silent rather than receiving another runtime's JSON schema.
    #[test]
    fn annotation_is_limited_to_verified_session_entry_events() {
        for source in [Source::Compact, Source::Other] {
            assert!(
                super::session_annotation("claude-code", source, "SID-A", None).is_none(),
                "{source:?}"
            );
        }
        let response = super::session_annotation(
            "codex",
            Source::Startup,
            "SID-A",
            Some("einsia/payments@work"),
        )
        .unwrap();
        assert!(
            response["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("einsia/payments@work")
        );
        assert!(super::session_annotation("cursor", Source::Startup, "SID-A", None).is_none());
    }

    /// WorkBuddy consumes the Claude-compatible `hookSpecificOutput` envelope. Its context must
    /// name the native session id and the saved target's commands, and carry no title field;
    /// leaving WorkBuddy out of the envelope, or dropping the id, fails here.
    #[test]
    fn a_workbuddy_session_learns_its_native_id_and_target() {
        let response = super::session_annotation(
            "workbuddy",
            Source::Startup,
            "SID-W",
            Some("einsia/payments@work"),
        )
        .unwrap();
        let output = &response["hookSpecificOutput"];
        assert_eq!(output["hookEventName"], "SessionStart");
        assert!(output.get("sessionTitle").is_none());
        let context = output["additionalContext"].as_str().unwrap();
        assert!(context.contains("'SID-W'"), "{context}");
        assert!(
            context.contains("agit commit 'einsia/payments@work'"),
            "{context}"
        );
    }

    /// The runtime names the conversation; a title in this response would replace the one Claude
    /// Code generates and stop it from generating one. Every entry event of every runtime, with
    /// or without a binding, carries context and no title.
    #[test]
    fn no_session_start_names_the_conversation() {
        for runtime in ["claude-code", "codex", "workbuddy"] {
            for source in [Source::Startup, Source::Resume, Source::Clear, Source::Fork] {
                for binding in [None, Some("einsia/payments@work")] {
                    let response =
                        super::session_annotation(runtime, source, "SID-A", binding).unwrap();
                    let output = &response["hookSpecificOutput"];
                    assert!(output.get("sessionTitle").is_none(), "{runtime} {source:?}");
                    assert!(output.get("additionalContext").is_some());
                }
            }
        }
    }

    /// A session an earlier agit renamed gets Claude Code's own title back, or the label Claude
    /// Code lists it by when it never had one, and only that session: a title the user chose is
    /// never replaced, and Codex is never sent a title.
    #[test]
    fn only_an_agit_placeholder_gives_way_to_claude_codes_own_title() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("SID.jsonl");
        let event = |runtime_title: &str, source: &str| {
            super::parse_event(
                &serde_json::json!({
                    "session_id": "SID",
                    "source": source,
                    "session_title": runtime_title,
                    "transcript_path": transcript,
                })
                .to_string(),
            )
            .unwrap()
        };
        std::fs::write(
            &transcript,
            [
                serde_json::json!({"type":"ai-title","aiTitle":"Fix the login page"}),
                serde_json::json!({"type":"custom-title","customTitle":"agit alice/app@work"}),
            ]
            .map(|record| format!("{record}\n"))
            .concat(),
        )
        .unwrap();
        for source in ["startup", "resume", "fork"] {
            assert_eq!(
                super::restored_title("claude-code", &event("agit alice/app@work", source))
                    .as_deref(),
                Some("Fix the login page"),
                "{source}"
            );
        }
        assert!(super::restored_title("claude-code", &event("Login page", "resume")).is_none());
        assert!(super::restored_title("codex", &event("agit: unnamed", "resume")).is_none());
        std::fs::write(
            &transcript,
            [
                serde_json::json!({"type":"custom-title","customTitle":"agit: unnamed"}),
                serde_json::json!({"type":"user","message":{"content":"Add a dark mode"}}),
            ]
            .map(|record| format!("{record}\n"))
            .concat(),
        )
        .unwrap();
        assert_eq!(
            super::restored_title("claude-code", &event("agit: unnamed", "resume")).as_deref(),
            Some("Add a dark mode")
        );
    }

    /// The person is told before the first upload, once: a new conversation (a fork included) and
    /// a resumed one that has not uploaded yet are announced, an uploading one is not announced
    /// again, and a runtime that shows no hook message gets none. The line names who can see the repository
    /// without guessing, every member of the organization when its base permission reads, and both
    /// ways to stop.
    #[test]
    fn an_automatic_upload_is_announced_before_it_happens() {
        for runtime in ["claude-code", "codex"] {
            assert!(super::announces_upload(runtime, Source::Startup, false));
            assert!(super::announces_upload(runtime, Source::Clear, false));
            assert!(super::announces_upload(runtime, Source::Fork, false));
            assert!(super::announces_upload(runtime, Source::Resume, false));
            assert!(!super::announces_upload(runtime, Source::Resume, true));
            assert!(!super::announces_upload(runtime, Source::Compact, false));
        }
        assert!(!super::announces_upload(
            "workbuddy",
            Source::Startup,
            false
        ));
        let notice_reading = |visibility: Option<&str>, org: Option<bool>, members_read| {
            super::upload_notice(&crate::commands::project::AutomaticUpload {
                repository: "acme/widget".into(),
                visibility: visibility.map(str::to_owned),
                org,
                members_read,
            })
        };
        let notice =
            |visibility: Option<&str>, org: Option<bool>| notice_reading(visibility, org, None);
        assert!(
            notice_reading(Some("private"), Some(true), Some(true))
                .contains("(private to acme: every member of acme can read it)")
        );
        assert_eq!(
            notice(Some("private"), Some(true)),
            "agit: this conversation is uploaded to acme/widget (private to acme: its owners and the people granted access can read it) at the end of each turn. Not this one? Say so, or run `agit project drop`."
        );
        assert!(
            notice(Some("private"), Some(false))
                .contains("(private: only you and people you add can read it)")
        );
        assert!(notice(Some("private"), None).contains("(private)"));
        assert!(notice(Some("public"), Some(true)).contains("(public: anyone can read it)"));
        assert!(notice(None, None).contains("(visibility not confirmed)"));
    }

    #[test]
    fn a_half_slug_is_never_written_to_the_session_env() {
        let mut lk = crate::domain::link::Link::new("claude-code", "S1", None);
        lk.branch = Some("refund-fix".into());
        lk.agent = Some("payments".into());
        assert!(super::session_env_value(&lk).is_none());
        lk.owner = Some("einsia".into());
        assert_eq!(
            super::session_env_value(&lk).as_deref(),
            Some("einsia/payments@refund-fix")
        );
        lk.superseded_by = Some("codex/replacement".into());
        assert!(super::session_env_value(&lk).is_none());
        let bare = crate::domain::link::Link::new("claude-code", "S2", None);
        assert!(super::session_env_value(&bare).is_none());
    }

    #[test]
    fn archive_environment_requires_its_exact_complete_role() {
        use crate::domain::merge_archive::MergeArchiveRole;
        let mut link = crate::domain::link::Link::new(
            "claude-code",
            "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa",
            None,
        );
        link.owner = Some("alice".into());
        link.agent = Some("target".into());
        link.branch = Some("work".into());
        link.merge_archive = Some(MergeArchiveRole {
            generation: "019e8308-072a-739c-a75b-604d0848ba6f".into(),
            slug: "alice/target".into(),
            branch: "work".into(),
            origin_head: "a".repeat(40),
            logical_session: format!("agit-{}", "b".repeat(40)),
        });
        assert!(!link.is_active());
        assert_eq!(
            super::session_env_value(&link).as_deref(),
            Some("alice/target@work")
        );
        for change in [
            "superseded",
            "owner",
            "agent",
            "branch",
            "runtime",
            "native",
            "generation",
            "origin",
            "logical-session",
        ] {
            let mut invalid = link.clone();
            match change {
                "superseded" => invalid.superseded_by = Some("codex/other".into()),
                "owner" => invalid.owner = None,
                "agent" => invalid.agent = Some("other".into()),
                "branch" => invalid.branch = Some("other".into()),
                "runtime" => invalid.source = "unknown-runtime".into(),
                "native" => invalid.session_id.clear(),
                "generation" => invalid.merge_archive.as_mut().unwrap().generation.clear(),
                "origin" => invalid.merge_archive.as_mut().unwrap().origin_head = "z".repeat(40),
                "logical-session" => invalid
                    .merge_archive
                    .as_mut()
                    .unwrap()
                    .logical_session
                    .clear(),
                _ => unreachable!(),
            }
            assert!(super::session_env_value(&invalid).is_none(), "{change}");
        }
    }

    /// Only **this session's own** environment file is written.
    ///
    /// On `/resume` the payload already carries the new session's id, while `CLAUDE_ENV_FILE`
    /// still points at the directory of the session switched away from. Without the comparison,
    /// A's binding lands in B's environment file — resuming B next, it calls itself A's branch,
    /// exactly the corruption this mechanism exists to prevent.
    #[test]
    fn the_session_env_file_must_belong_to_this_session() {
        let base = "/h/.claude/session-env";
        assert!(super::env_file_belongs_to(
            &format!("{base}/SID-A/sessionstart-hook-2.sh"),
            "SID-A"
        ));
        // The real shape of /resume: the payload is A while the file is B's.
        assert!(!super::env_file_belongs_to(
            &format!("{base}/SID-B/sessionstart-hook-2.sh"),
            "SID-A"
        ));
        // An unrecognized layout is not written (fail closed).
        assert!(!super::env_file_belongs_to("SID-A", "SID-A"));
        assert!(!super::env_file_belongs_to("", "SID-A"));
    }

    /// The line written into the session environment must be valid shell, with quoting that holds.
    #[test]
    fn the_session_env_line_survives_shell_parsing() {
        let q = super::sh_quote("me/re'po@bra nch");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("set -- {q}; printf '%s' \"$1\""))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "me/re'po@bra nch");
    }
}
