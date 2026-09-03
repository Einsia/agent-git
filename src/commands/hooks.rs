//! `agit hooks` — the entry point for runtime hooks (a hidden subcommand).
//!
//! Two actions, one for each of the harness's two kinds of event:
//!
//! * **`ingest` (SessionStart)**: a session starts or switches. It is the **only authoritative
//!   answer** to "which session is running now", see below.
//! * **`settle` (Stop)**: a turn ended, so settle it.
//!
//! Always exit 0: a hook failure must not disturb the session. `settle` stays silent; `ingest`
//! may return one valid SessionStart JSON response so the runtime can expose the session state.
//! Failures and sessions that are none of agit's business produce no output.
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
}

pub fn run(args: Args) -> CmdResult {
    let runtime = args.runtime.clone();
    match args.action.unwrap_or(Action::Ingest) {
        Action::Ingest => ingest(runtime.as_deref()),
        Action::Settle => settle(runtime.as_deref()),
    }
}

// ── payload ───────────────────────────────────────────────────────────

/// SessionStart's `source`.
///
/// claude-code and codex use the same set of values.
/// Anything unrecognized becomes `Other` — when a value we have never seen appears, the safe
/// behavior is "register without claiming", not guessing startup and claiming a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Startup,
    Resume,
    Clear,
    Compact,
    Other,
}

impl Source {
    fn parse(s: Option<&str>) -> Source {
        match s {
            Some("startup") => Source::Startup,
            Some("resume") => Source::Resume,
            Some("clear") => Source::Clear,
            Some("compact") => Source::Compact,
            _ => Source::Other,
        }
    }
}

/// The few facts the hook gets from stdin.
struct Event {
    session_id: String,
    cwd: Option<String>,
    transcript_path: Option<String>,
    source: Source,
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
    })
}

fn read_event() -> Option<Event> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    parse_event(&buf)
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
/// Only two cases count: **this directory has a repo bound** (the user does agit work here), or
/// **this is a session we started ourselves** (`AGIT_SESSION` is present).
///
/// # Why not "register everything"
///
/// "Registering" sounds harmless — a JSON record of a few dozen bytes. But the store is not
/// write-only: the resolution chain in [`crate::commands::context`] does a **full traversal** with
/// `link::list()` in both `from_harness_env` and `from_cwd`, and that chain is on the path of every
/// zero-argument agit command. Registering everything lets the store grow without bound with "the
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
    let ev = read_event()?;
    let env_session = super::context::from_session_env();

    let dir = ev.cwd.as_deref().map(std::path::Path::new);
    let bound = dir.map(|d| workspace::read(d).is_some()).unwrap_or(false);
    if !concerns_us(bound, env_session.is_some()) {
        return None;
    }

    let rt = runtime_of(runtime, ev.transcript_path.as_deref());
    let store = Store::open_or_init().ok()?;
    let existing = link::get(&store, rt, &ev.session_id);
    let has_binding = existing
        .as_ref()
        .map(|l| l.agent.is_some() && l.branch.is_some())
        .unwrap_or(false);

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
            let _ = link::write(&store, &lk);
            // The value is **the full slug just claimed**, not the bare name sent back through
            // `qualify`: with nobody signed in, `qualify` cannot fill in an owner and writes a
            // binding that was just claimed successfully as a value that does not parse, which
            // the next command then drops as malformed.
            Some(super::context::encode_session_env(&repo, &branch))
        }
        Decision::Register => {
            let _ = link::write(&store, &Link::new(rt, &ev.session_id, dir));
            None
        }
        Decision::Nothing => link::get(&store, rt, &ev.session_id)
            .as_ref()
            .and_then(session_env_value),
    };

    // Whichever branch ran above, the real binding of **this** session is written back to the
    // session environment. With no binding, write `unset` — an empty value sends agit down the
    // full context resolution chain; a stale wrong value makes it confidently do the wrong thing.
    write_session_env(&ev.session_id, env_value.as_deref());
    session_annotation(rt, ev.source, &ev.session_id, env_value.as_deref())
}

/// The SessionStart response the runtime may show or add to the conversation.
///
/// `compact` is deliberately excluded: it happens after the session has already started, when the
/// user may have renamed it. Reapplying our title there would overwrite that explicit choice.
/// Unknown sources are excluded for the same reason — output is safe only when the event is known
/// to represent entering a session. Codex accepts the same response envelope but has no
/// `sessionTitle` field, so a managed Codex session needs no response and an unmanaged one receives
/// only the adoption context.
fn session_annotation(
    runtime: &str,
    source: Source,
    session_id: &str,
    binding: Option<&str>,
) -> Option<serde_json::Value> {
    if !matches!(source, Source::Startup | Source::Resume | Source::Clear) {
        return None;
    }

    let additional_context = binding.is_none().then(|| {
        format!(
            "This session is not under agit version control yet. If the user asks to save, name, or publish it, ask for the target if needed, then run `agit import {} --from {runtime} --into <owner/repo>@<branch>`.",
            sh_quote(session_id)
        )
    });
    if runtime == "codex" && additional_context.is_none() {
        return None;
    }
    if !matches!(runtime, "claude-code" | "codex") {
        return None;
    }

    let mut output = serde_json::Map::new();
    output.insert(
        "hookEventName".into(),
        serde_json::Value::String("SessionStart".into()),
    );
    if runtime == "claude-code" {
        let title = binding
            .map(|binding| format!("agit {binding}"))
            .unwrap_or_else(|| "agit: unnamed".to_string());
        output.insert("sessionTitle".into(), serde_json::Value::String(title));
    }
    if let Some(context) = additional_context {
        output.insert(
            "additionalContext".into(),
            serde_json::Value::String(context),
        );
    }
    Some(serde_json::json!({ "hookSpecificOutput": output }))
}

/// Write the binding into the harness's "session environment" file.
///
/// Only claude-code offers this mechanism (`CLAUDE_ENV_FILE`, one directory per session). codex has
/// no equivalent and falls back to the freshness check in [`super::context`].
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
    // On a mismatch, **do not write**. One skipped propagation merely sends agit back down the
    // full context resolution chain (step 3 looks the session up by the harness's session id); one
    // wrong write leaves a persistent false binding.
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

/// The `AGIT_SESSION` value for one link; no binding, no value.
///
/// The namespace comes from the link ([`super::context::slug_of_link`]): a recorded one is used as
/// recorded, an unrecorded one is filled in from the sign-in credentials, and with nobody signed in
/// it is `local/<agent>` — exactly the owner `agit init` gives a repo when nobody is signed in, so
/// the value written out is always complete, parsable, and points at the directory the repo really
/// lives in. A half value (a bare name with no owner) is never written: such a value does not pass
/// [`super::context::decode_session_env`], the next command drops it as malformed, and in between
/// it keeps looking like a valid binding.
fn session_env_value(lk: &Link) -> Option<String> {
    let branch = lk.branch.as_deref()?;
    let slug = super::context::slug_of_link(lk)?;
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
    let _ = settle_inner(runtime);
    Ok(ExitCode::Ok)
}

fn settle_inner(runtime: Option<&str>) -> Option<()> {
    if super::config::get("commit.auto").as_deref() == Some("false") {
        return None; // automatic settlement is explicitly off
    }
    let ev = read_event()?;
    let rt = runtime_of(runtime, ev.transcript_path.as_deref());
    let store = Store::open_or_init().ok()?;

    // **This is where the whole fix lands**: locating goes through the payload's session_id, not
    // through the environment variable.
    let lk = link::get(&store, rt, &ev.session_id)?;

    // An unadopted session is not settled — the premise "adoption is explicit" does not bend for
    // the hook's convenience.
    lk.agent.as_ref()?;
    lk.branch.as_ref()?;

    let _ = super::commit::settle_from_link(&store, lk);
    Some(())
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

    #[test]
    fn source_values_come_from_both_runtimes() {
        // These four values are common to claude-code and codex.
        assert_eq!(Source::parse(Some("startup")), Source::Startup);
        assert_eq!(Source::parse(Some("resume")), Source::Resume);
        assert_eq!(Source::parse(Some("clear")), Source::Clear);
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
        for s in [Source::Resume, Source::Clear] {
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
    /// make an agent attempt to adopt it again.
    #[test]
    fn a_managed_session_is_labeled_with_its_binding() {
        assert_eq!(
            super::session_annotation(
                "claude-code",
                Source::Resume,
                "SID-A",
                Some("einsia/payments@flaky-test"),
            ),
            Some(serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "sessionTitle": "agit einsia/payments@flaky-test",
                }
            }))
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
        assert_eq!(output["sessionTitle"], "agit: unnamed");
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
                    "additionalContext": "This session is not under agit version control yet. If the user asks to save, name, or publish it, ask for the target if needed, then run `agit import 'SID-C' --from codex --into <owner/repo>@<branch>`.",
                }
            }))
        );
    }

    /// Reapplying a title after the user can rename the session loses their explicit choice.
    /// Unknown runtimes also stay silent rather than receiving another runtime's JSON schema.
    #[test]
    fn annotation_is_limited_to_verified_session_entry_events() {
        for source in [Source::Compact, Source::Other] {
            assert!(
                super::session_annotation("claude-code", source, "SID-A", None).is_none(),
                "{source:?}"
            );
        }
        assert!(
            super::session_annotation(
                "codex",
                Source::Startup,
                "SID-A",
                Some("einsia/payments@work"),
            )
            .is_none()
        );
        assert!(super::session_annotation("cursor", Source::Startup, "SID-A", None).is_none());
    }

    /// A value written out must parse back: a recorded namespace is used as recorded, an
    /// unrecorded one is filled in from the signed-in account, and with nobody signed in it is
    /// `local` — all three give a complete `owner/agent@branch`. A half value
    /// (`payments@refund-fix`) does not pass `decode_session_env`, so writing one would leave the
    /// next command something that "looks like a binding but is dropped as malformed"; it never
    /// appears.
    #[test]
    fn a_half_slug_is_never_written_to_the_session_env() {
        let mut lk = crate::domain::link::Link::new("claude-code", "S1", None);
        lk.branch = Some("refund-fix".into());
        lk.agent = Some("payments".into());
        // No recorded namespace: the owner is filled in from the signed-in account, or `local`
        // with nobody signed in — either way the value is complete and parsable, agent and branch
        // unchanged, never a half value.
        let v = super::session_env_value(&lk).expect("a claimed link always has a value");
        let (repo, branch) = crate::commands::context::decode_session_env(&v)
            .unwrap_or_else(|| panic!("a value written out must parse back: {v}"));
        assert_eq!(branch, "refund-fix");
        assert!(
            repo.ends_with("/payments") && !repo.starts_with('/'),
            "the owner half must be filled in, got {repo}"
        );
        // A recorded namespace is written as it stands, without asking the signed-in account.
        lk.owner = Some("einsia".into());
        assert_eq!(
            super::session_env_value(&lk).as_deref(),
            Some("einsia/payments@refund-fix")
        );
        // A link with no binding never has a value.
        let bare = crate::domain::link::Link::new("claude-code", "S2", None);
        assert!(super::session_env_value(&bare).is_none());
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
