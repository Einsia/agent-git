//! agit — A Git-compatible CLI for versioning Agent Context + Environment.
//!
//! Architecture (docs/architecture.md): the versioned objects are two git repos + one pairing.
//!
//!   agit <git-args>        = agit -e <git-args>  → transparent git acting on the Environment (code repository)
//!   agit agent <git-args>  (alias: agit a)       → the isomorphic operation acting on the Agent Store
//!
//! The scope selector is only the first token immediately after agit; anything after the verb belongs to git:
//!   agit a commit    → Agent scope
//!   agit commit -a   → Environment scope, -a is an argument to git commit
//!
//! `agent`/`a` is a subcommand rather than a flag precisely so those two cannot be transposed. `-a` survives
//! as a silent deprecated alias while the docs and demo scripts still say it.


// Pedantic markdown-in-doc-comment lint; the comment style here is deliberate.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
// Core logic lives in the lib (crate `agit`), shared with agit-hub, so the two bins don't each write their own parsing and drift apart.
use agit::scope::{self, Scope};
use agit::{commands, harness, init, passthrough, session, sync, ui, view};
use std::path::PathBuf;
use std::process::exit;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    exit(dispatch(argv));
}

/// Parse out (scope, remaining args). The scope selector is only the first token immediately after agit.
///
/// `agent` (alias `a`) is a SUBCOMMAND, which is the point: a flag before the verb could be transposed,
/// and `agit -a commit` (agent store) vs `agit commit -a` (code repo, git's stage-all) differ by one
/// space. `agit a commit` cannot be written the wrong way round. `-a` / `-e` remain as silent deprecated
/// aliases until the cutover removes them.
fn split_scope(argv: &[String]) -> (Scope, &[String]) {
    match argv.first().map(|s| s.as_str()) {
        Some("agent" | "a" | "-a") => (Scope::Agent, &argv[1..]),
        Some("-e") => (Scope::Environment, &argv[1..]),
        _ => (Scope::Environment, argv),
    }
}

/// Agent-store management verbs — a CLOSED set (design doc §5). Everything outside it is handed to git
/// on the store, so `agit a log`, `agit a commit`, `agit a push` and `agit a diff` all keep working.
///
/// Where a git verb means the same thing on the store it keeps the git name — the agent version is
/// just the smarter one: `clone` (by identity), `init` (mint an identity), `switch` (active agent),
/// `push`/`pull`/`fetch`/`merge`. The verbs here are the ones with no git primitive: `list`, `status`,
/// `info`, `rename`, `rebind` (a repair — "accept a changed identity"). `info` not `show` so `git show`
/// on the store still works. (`log`/`diff` stay git verbs — intercepted only to render the session view,
/// with `--raw` as the passthrough escape hatch.)
const AGENT_MGMT_VERBS: &[&str] = &["list", "status", "switch", "info", "rename", "rebind"];

/// Recognizing the management verbs is what keeps them away from git — `agit a info` must not become
/// `git info`.
fn agent_mgmt(verb: &str, args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;

    // A verb whose argument is mandatory must not read a missing one as "the empty selector": every
    // resolution rung treats blank as absent, so `agit a switch` would silently act on the default.
    let need = |what: &str| -> anyhow::Result<String> {
        match args.first().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(a) => Ok(a.to_string()),
            None => anyhow::bail!("agit agent {verb}: missing <{what}>\n  usage: agit a {verb} <{what}>"),
        }
    };
    let flag = |f: &str| args.iter().any(|a| a == f);

    match verb {
        "list" => {
            let agents = agent::list()?;
            if agents.is_empty() {
                println!("no agents yet — agit a init <name> mints one.");
                return Ok(0);
            }
            let env = scope::env_root().ok();
            let active = env.as_deref().and_then(|e| agent::read_active(e).ok().flatten());
            let default = env
                .as_deref()
                .and_then(|e| agent::Binding::load(e).ok().flatten())
                .and_then(|b| b.default);

            let rows: Vec<Vec<String>> = agents
                .iter()
                .map(|a| {
                    let sessions = commands::store_sessions(&a.store);
                    // "watching", not "running": a live watcher announced itself for this agent, and
                    // nothing here can see whether a human has a live session open.
                    let status = match session::watching_pid(&a.aid) {
                        Some(_) => ui::accent("● watching"),
                        None => ui::dim("·").to_string(),
                    };
                    let last = sessions
                        .iter()
                        .map(|s| s.recency())
                        .max()
                        .map(ui::ago)
                        .unwrap_or_else(|| "—".into());
                    let here = if Some(&a.aid) == active.as_ref() { "  (here)" } else { "" };
                    vec![a.name.clone(), status, sessions.len().to_string(), format!("{last}{here}")]
                })
                .collect();
            println!("{}", ui::table(&["AGENT", "STATUS", "SESSIONS", "LAST"], &rows));
            if let Some(d) = default {
                println!("{}", ui::dim(&format!("default: {d}")));
            }
            Ok(0)
        }
        // `status` is the per-repo overview: which agents this repo works with, which is active, their
        // session counts + last activity + live-watcher, and the active store's unpushed/ahead-behind.
        "status" => commands::agent_status(),
        "info" => {
            let a = agent::info(&need("name|aid")?)?;
            println!("name   {}", a.name);
            println!("aid    {}", a.aid);
            println!("store  {}", a.store.display());
            println!("remote {}", a.remote.as_deref().unwrap_or("— (local only; agit a push records one)"));
            Ok(0)
        }
        // `switch` is the git-native name for "select this worktree's active agent" — the smart
        // version of `git switch` on the store.
        "switch" => {
            let a = agent::switch_agent(&need("name|aid")?)?;
            println!("● {} ({})  — this worktree's agent", a.name, a.aid);
            Ok(0)
        }
        "rename" => {
            let old = need("old-name")?;
            let new = match args.get(1).map(|s| s.trim()).filter(|s| !s.is_empty()) {
                Some(n) => n.to_string(),
                None => anyhow::bail!("agit agent rename: missing <new-name>\n  usage: agit a rename <old> <new>"),
            };
            let a = agent::rename(&old, &new)?;
            println!("renamed {old} → {} ({} — unchanged)", a.name, a.aid);
            Ok(0)
        }
        // Override the integrity check (`--remote <url>`), or give a forked store its own identity
        // (`--new-id`). A repair verb — no git primitive means "accept a changed identity".
        "rebind" => {
            let remote = args.iter().position(|a| a == "--remote").and_then(|i| args.get(i + 1)).cloned();
            let name = args.first().filter(|a| !a.starts_with("--")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            let a = agent::rebind(name.as_deref(), remote.as_deref(), flag("--new-id"))?;
            println!("rebound {} ({})", a.name, a.aid);
            println!("  remote {}", a.remote.as_deref().unwrap_or("—"));
            Ok(0)
        }
        // Unreachable: every verb in AGENT_MGMT_VERBS has an arm above. Kept as a defensive default so
        // adding a name to the closed set without an arm fails loudly here rather than falling to git.
        v => {
            eprintln!("agit agent {v}: no handler — this is a bug (a closed-set verb with no arm).");
            eprintln!("  available: list · status · switch · info · rename · rebind");
            Ok(2)
        }
    }
}

/// Bind a freshly-minted agent to this repo and make it the active one.
///
/// Outside a git repo this is a no-op rather than an error: `agit a init` is explicitly allowed there.
fn bind_and_activate(a: &agit::agent::Agent) -> anyhow::Result<()> {
    let Ok(env) = scope::env_root() else { return Ok(()) };
    agit::agent::bind_here(a, &env, false)?;
    agit::agent::write_active(&env, &a.aid)?;
    println!("  bound  .agit.toml (commit it: your team gets this agent on clone)");
    Ok(())
}

fn dispatch(argv: Vec<String>) -> i32 {
    let (scope, rest) = split_scope(&argv);

    let Some(cmd) = rest.first().map(|s| s.as_str()) else {
        eprintln!("{}", USAGE);
        return 2;
    };
    let args = &rest[1..];

    let result = match cmd {
        // ── init (agent scope): mint a new agent — the git-native name for creating a repo, here a
        //    store with its own identity. (Checked before the scope-independent `init` below, which is
        //    the code-repo setup.) ──
        "init" if scope == Scope::Agent => agent_init(args),

        // ── Top-level native commands (independent of scope) ──
        "init" => {
            let agent = args
                .iter()
                .position(|a| a == "--agent")
                .and_then(|i| args.get(i + 1))
                .cloned();
            init::run_named(agent)
        }
        // No `"clone"` arm: `agit clone <url>` is git's clone, on the code repo, like every other
        // unclaimed verb. It used to mean "clone the team's Agent Store into <env>/.agit/agent" — which
        // shadowed git's own verb (the thing `track` not `add` and `info` not `show` exist to avoid) and,
        // after the cutover, built a store at a path nothing resolves. `agit a clone <url>` is the memory.
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(0)
        }
        "--version" => {
            println!("agit {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }

        // ── start: the headline path — launch a real session here, already carrying the agent's
        //    context, with no ids and no file paths typed. ──
        "start" => {
            let (agent, as_rt) = parse_start(args);
            commands::start_cmd(agent.as_deref(), as_rt.as_deref())
        }

        // ── Native verbs that agit adds value to ──
        "scan" => {
            let (staged, paths) = parse_scan(args);
            commands::scan_cmd(scope, staged, &paths)
        }
        "hook-scan" => commands::hook_scan(args.iter().any(|a| a == "--staged")),

        // ── crypt-clean / crypt-smudge: the agit-crypt filter drivers. Scope-INDEPENDENT: git invokes
        //    them as `filter.agit-crypt.{clean,smudge}`, never the user. stdin→stdout binary pipes,
        //    keyed from $AGIT_HOME. Routed here beside hook-scan (also git-invoked). ──
        "crypt-clean" => commands::crypt_clean(),
        "crypt-smudge" => commands::crypt_smudge(),

        // ── crypt: the per-session keybox client verb. `crypt unlock` recovers this machine's content
        //    keys from the committed keybox into the repo-local keyring, so the filter can decrypt.
        //    Scope-independent — it resolves the active agent itself. ──
        "crypt" => commands::crypt_cmd(args),
        "workspace" => match args.first().map(|s| s.as_str()) {
            Some("log") => commands::workspace_log(),
            Some("restore") => commands::workspace_restore(args.get(1).map(|s| s.as_str())),
            _ => commands::workspace_show(),
        },

        // ── snap: mirror the runtime's session dump into the Agent Store (formerly named sync).
        //    --watch runs it continuously (fully automatic snap). ──
        "snap" => {
            let (rt, watch, interval, harness) = parse_snap(args);
            match (watch, rt) {
                // The watcher polls one runtime's dump; unnamed, `agit watch` is the both-runtimes loop.
                (true, Some(rt)) => session::snap_watch_checked(&rt, interval, harness),
                (true, None) => session::watch(interval, false, harness),
                (false, rt) => session::snap(rt.as_deref(), harness),
            }
        }

        // ── merge: reconcile two diverged agent branches by dialogue (the git-term verb; both sides
        //    truly resume, read-only, only real conflicts prompt you). Only the Agent scope is the
        //    semantic dialogue merge — `agit merge` / `git merge` on the code repo passes through to
        //    git untouched. `sync` remains as a back-compat alias, scope-gated to Agent exactly like
        //    `merge` so `agit sync` in the Environment falls through to git rather than running the
        //    dialogue merge. ──
        "merge" if scope == Scope::Agent => merge_cmd(args),
        "sync" if scope == Scope::Agent => merge_cmd(args),

        // ── push (agent scope): a real git push on the store, plus the one thing a bare push can't do
        //    in the agent's context — record the store's origin in the committed binding, so a
        //    teammate's clone can find this agent. `agit push` / `git push` on the code repo is
        //    untouched passthrough. ──
        "push" if scope == Scope::Agent => agent_push(args),

        // ── commit (agent scope): a real git commit on the store, but the secret gate runs on the
        //    staged index BEFORE git — because a bare passthrough fires only git's pre-commit hook,
        //    which `--no-verify` skips, and agit's own commit must not offer that silent exit. ──
        "commit" if scope == Scope::Agent => agent_commit(args),

        // ── pull (agent scope): fast-forward when it safely can, but NEVER let git textually merge
        //    diverged jsonl transcripts (that corrupts exactly the sessions `agit a merge` reconciles
        //    by dialogue). On divergence, refuse and route to merge. ──
        "pull" if scope == Scope::Agent => agent_pull(args),

        // ── fetch (agent scope): a bare fetch only moves remote-tracking refs — nothing to convert
        //    yet — so the session-aware thing it can do is report which sessions arrived, and how to
        //    integrate them. ──
        "fetch" if scope == Scope::Agent => agent_fetch(args),

        // ── clone (agent scope): clone an agent's store by identity — the git-native name for what
        //    used to be `track`. A bare name resolves through the binding; a URL clones that store and
        //    adopts its identity. (Raw `git clone` here would make a nested repo that resolves to
        //    nothing, which is exactly what this replaces.) ──
        "clone" if scope == Scope::Agent => agent_clone(args),

        // ── encrypt (agent scope): opt-in at-rest encryption of the store — a convergent git
        //    clean/smudge filter (git-crypt style). Enables/wires the filter, mints/exports/imports the
        //    symmetric key. Ciphertext is what a push publishes; only coherent for a no-hub setup. ──
        "encrypt" if scope == Scope::Agent => agent_encrypt(args),

        // ── readers / rekey (agent scope): manage a per-session keybox — add/remove readers (add is an
        //    O(1) keybox append; rm is an eager CK rotation) and rotate the content key. ──
        "readers" if scope == Scope::Agent => commands::readers_cmd(args),
        "rekey" if scope == Scope::Agent => commands::rekey_cmd(args),

        // ── log / diff (agent scope): a raw `git log`/`git diff` on the store is a wall of jsonl bytes,
        //    so by default these render the SESSION view — `a log` a timeline of the store's sessions,
        //    `a diff` the prompts + edits ADDED between two refs. `--raw` (or `--git`) drops the flag and
        //    falls back to real git passthrough, so the byte-level view (and every scripted `--format`)
        //    still works. Only the Agent scope is intercepted — `agit log` / `git log` on the code repo is
        //    untouched passthrough. ──
        "log" | "diff" if scope == Scope::Agent => {
            if args.iter().any(|a| a == "--raw" || a == "--git") {
                // Strip the escape-hatch flag; hand the rest to git verbatim (rest[0] is the verb).
                let git_args: Vec<String> =
                    rest.iter().filter(|a| *a != "--raw" && *a != "--git").cloned().collect();
                passthrough::run(scope, &git_args)
            } else if cmd == "log" {
                view::agent_log(args)
            } else {
                view::agent_diff(args)
            }
        }
        "adapter" => commands::adapter_list(),
        "graph" => commands::workspace_graph(),

        // ── provenance: self-verify that a captured session is cryptographically tied to its producer,
        //    or show this machine's signing key. Verification degrades gracefully — an unsigned session
        //    reports "unverified", never blocks. ──
        "provenance" => commands::provenance_cmd(args),

        // ── identity: publish/inspect this machine's public keys in the shared hub identity registry
        //    (encryption-recipients Wave 1). `enroll [--rotate]` derives the ed25519 + X25519 halves,
        //    self-signs, and upserts the caller's own row; `show [<user>]` prints fingerprints. ──
        "identity" => commands::identity_cmd(args),

        // ── hub: hub-side operations beyond per-agent git. `hub team rekey <org>` rotates the org's
        //    Team KEK (gen++, re-seals to members); `hub team sync <org>` seals the current TK to members
        //    who lack an envelope (encryption-recipients Wave 3). ──
        "hub" => commands::hub_cmd(args),

        // ── shadow: route `git` through `agit` in your interactive shell (cross-platform). ──
        "shadow" => agit::shadow::run(args),

        // ── watch: fully hands-off — watch both runtimes' dumps, auto-snap + auto-convert both ways.
        //    --daemon runs it forever in the background; --stop / --status manage it. ──
        "watch" => {
            let (interval, convert, harness, action) = parse_watch(args);
            match action {
                1 => session::watch_daemon(interval, convert, harness),
                2 => session::watch_stop(),
                3 => session::watch_status(),
                _ => session::watch(interval, convert, harness),
            }
        }

        // ── harness: show / apply the captured MCP + skills + config (part of Agent State) ──
        "harness" => {
            let (sub, rt, force, from_env) = parse_harness(args);
            harness_cmd(&sub, rt.as_deref(), force, from_env.as_deref())
        }

        // ── Convert a session across runtimes (resume it in another CLI); --watch = auto-convert worker ──
        "convert" if args.iter().any(|a| a == "--watch") => {
            let interval = args
                .iter()
                .position(|a| a == "--interval")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            commands::convert_watch(interval)
        }
        "convert" => match parse_convert(args) {
            Some((src, from, to, cwd, write)) => {
                commands::convert_cmd(&src, from, &to, cwd, write)
            }
            None => {
                eprintln!("usage: agit convert <src-session> --to claude-code|codex [--from RT] [--cwd PATH] [--write]  (or --watch [--interval N] to auto-convert both ways)");
                Ok(2)
            }
        },

        // ── resume: load a session into a runtime and continue (the universal loader) ──
        "resume" => match parse_resume(args) {
            Some((src, as_rt, cwd, env, exec, relocate)) => {
                commands::resume_cmd(&src, as_rt, cwd, env, exec, relocate)
            }
            None => {
                eprintln!("usage: agit resume <src-session> [--as claude-code|codex] [--env PATH] [--relocate] [--cwd PATH] [--exec]");
                Ok(2)
            }
        },

        // ── Agent-store management verbs, checked before passthrough so they never reach git ──
        v if scope == Scope::Agent && AGENT_MGMT_VERBS.contains(&v) => agent_mgmt(v, args),

        // ── Everything else: transparently pass through to the corresponding repo's git ──
        _ => passthrough::run(scope, rest),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("agit: {e:#}");
            2
        }
    }
}


/// start arguments: `--agent <name|aid>` (this invocation only — it does NOT flip the default, or two
/// agents in one repo would fight over one pointer) and `--as <runtime>`.
fn parse_start(args: &[String]) -> (Option<String>, Option<String>) {
    let mut agent = None;
    let mut as_rt = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--agent" if i + 1 < args.len() => {
                agent = Some(args[i + 1].clone());
                i += 2;
            }
            "--as" if i + 1 < args.len() => {
                as_rt = Some(args[i + 1].clone());
                i += 2;
            }
            _ => i += 1,
        }
    }
    (agent, as_rt)
}

/// snap arguments: `--from <rt>` (or a bare positional) names the runtime, else snap captures every
/// runtime with sessions here; `--watch` runs it continuously, `--interval <n>`, `--no-harness`.
/// Returns (runtime, watch, interval, capture-harness).
type SnapArgs = (Option<String>, bool, u64, bool);
fn parse_snap(args: &[String]) -> SnapArgs {
    // No default runtime: `--from` (or a bare positional) names one, otherwise snap captures every
    // runtime that has sessions here. See session::snap.
    let mut rt: Option<String> = None;
    let mut watch = false;
    let mut interval = 5u64;
    let mut harness = true;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" if i + 1 < args.len() => {
                rt = Some(args[i + 1].clone());
                i += 2;
            }
            "--watch" => {
                watch = true;
                i += 1;
            }
            "--interval" if i + 1 < args.len() => {
                interval = args[i + 1].parse().unwrap_or(5);
                i += 2;
            }
            "--no-harness" => {
                harness = false;
                i += 1;
            }
            other => {
                // a bare positional is shorthand for the runtime: `agit a snap codex`
                if !other.starts_with('-') {
                    rt = Some(other.to_string());
                }
                i += 1;
            }
        }
    }
    (rt, watch, interval, harness)
}

/// watch arguments: `--interval <n>`, `--no-convert`, `--no-harness`, and the action selector
/// `--daemon`/`--background` (1), `--stop` (2), `--status` (3); default run (0).
/// Returns (interval, do-convert, capture-harness, action).
type WatchArgs = (u64, bool, bool, u8);
fn parse_watch(args: &[String]) -> WatchArgs {
    let mut interval = 5u64;
    let mut convert = true;
    let mut harness = true;
    let mut action = 0u8; // 0=run 1=daemon 2=stop 3=status
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--interval" if i + 1 < args.len() => {
                interval = args[i + 1].parse().unwrap_or(5);
                i += 2;
            }
            "--no-convert" => {
                convert = false;
                i += 1;
            }
            "--no-harness" => {
                harness = false;
                i += 1;
            }
            "--daemon" | "--background" => {
                action = 1;
                i += 1;
            }
            "--stop" => {
                action = 2;
                i += 1;
            }
            "--status" => {
                action = 3;
                i += 1;
            }
            _ => i += 1,
        }
    }
    (interval, convert, harness, action)
}

/// harness arguments: an optional subcommand positional (`show`/`apply`, default `show`), `--from <rt>`,
/// `--from-env <path>`, `--force`. Returns (subcommand, runtime, force, from-env).
type HarnessArgs = (String, Option<String>, bool, Option<String>);
fn parse_harness(args: &[String]) -> HarnessArgs {
    let mut rt: Option<String> = None;
    let mut from_env: Option<String> = None;
    let mut force = false;
    let mut sub = "show".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" if i + 1 < args.len() => {
                rt = Some(args[i + 1].clone());
                i += 2;
            }
            // Which CHECKOUT's harness, when this agent captured one in several. The
            // non-interactive answer to that question, so a script is never left with a
            // prompt it cannot see.
            "--from-env" if i + 1 < args.len() => {
                from_env = Some(args[i + 1].clone());
                i += 2;
            }
            "--force" => {
                force = true;
                i += 1;
            }
            other if !other.starts_with('-') => {
                sub = other.to_string();
                i += 1;
            }
            _ => i += 1,
        }
    }
    (sub, rt, force, from_env)
}

/// convert arguments: positional src + --to (required) + --from/--cwd/--write.
/// Returns None when src or --to is missing.
type ConvertArgs = (PathBuf, Option<String>, String, Option<String>, bool);
fn parse_convert(args: &[String]) -> Option<ConvertArgs> {
    let mut src = None;
    let mut from = None;
    let mut to = None;
    let mut cwd = None;
    let mut write = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => {
                to = args.get(i + 1).cloned();
                i += 2;
            }
            "--from" => {
                from = args.get(i + 1).cloned();
                i += 2;
            }
            "--cwd" => {
                cwd = args.get(i + 1).cloned();
                i += 2;
            }
            "--write" => {
                write = true;
                i += 1;
            }
            other => {
                if src.is_none() && !other.starts_with('-') {
                    src = Some(PathBuf::from(other));
                }
                i += 1;
            }
        }
    }
    Some((src?, from, to?, cwd, write))
}

/// resume arguments: positional src + --as <rt> / --cwd <path> / --env <path> / --exec.
/// Returns None when src is missing.
type ResumeArgs = (PathBuf, Option<String>, Option<String>, Option<String>, bool, bool);
fn parse_resume(args: &[String]) -> Option<ResumeArgs> {
    let mut src = None;
    let mut as_rt = None;
    let mut cwd = None;
    let mut env = None;
    let mut exec = false;
    let mut relocate = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--as" => {
                as_rt = args.get(i + 1).cloned();
                i += 2;
            }
            "--cwd" => {
                cwd = args.get(i + 1).cloned();
                i += 2;
            }
            "--env" => {
                env = args.get(i + 1).cloned();
                i += 2;
            }
            "--relocate" => {
                relocate = true;
                i += 1;
            }
            "--exec" => {
                exec = true;
                i += 1;
            }
            other => {
                if src.is_none() && !other.starts_with('-') {
                    src = Some(PathBuf::from(other));
                }
                i += 1;
            }
        }
    }
    Some((src?, as_rt, cwd, env, exec, relocate))
}

/// `agit harness [show|apply] [--from <rt>]` — which runtime's harness is resolved against what was
/// actually captured, never defaulted.
///
/// The two halves answer ambiguity differently, and deliberately: `show` is read-only, so listing every
/// captured runtime IS the answer; `apply` rewrites the project's own .mcp.json / .claude, so it acts on
/// exactly one runtime and asks rather than guess.
///
/// Both also answer *which checkout's* harness: one agent works in several repos, so `show` reports this
/// checkout's and every other checkout's, while `apply` acts on one and announces it when it comes from
/// another — a config arriving from a repo you did not name is indistinguishable from a bug.
fn harness_cmd(sub: &str, rt: Option<&str>, force: bool, from_env: Option<&str>) -> anyhow::Result<i32> {
    let agent = scope::root_for(Scope::Agent)?;
    let env = scope::env_root()?;
    let captured = harness::captured_runtimes(&agent, &env);
    if sub == "apply" {
        let rt = session::resolve_runtime(rt, &captured, "apply")?;
        return harness::apply(&agent, &env, &rt, force, from_env);
    }
    match rt {
        Some(r) => harness::show(&agent, &env, &session::resolve_runtime(Some(r), &captured, "show")?),
        None if captured.is_empty() => {
            println!(
                "No harness captured for {} yet. Run `agit a snap` to capture it.",
                session::runtime_list()
            );
            Ok(0)
        }
        None => {
            let mut code = 0;
            for rt in captured {
                code = harness::show(&agent, &env, rt)?;
            }
            Ok(code)
        }
    }
}

/// Parse and run the dialogue merge (`agit a merge <target>`, alias `sync`): positional <target> plus
/// --from <rt> / --both / --quick, and --agent/--ref to disambiguate the target in scripts.
///
/// The target is another MEMORY — an agent name or a ref — never a code branch. When a word names both,
/// `--agent X` / `--ref X` say which without a prompt; interactively agit asks.
/// `agit a init <name>` — mint a new agent (a store with its own identity). The git-native name for
/// creating a repo, here one that carries an aid. Minting works outside a repo on purpose (identity
/// precedes any URL), so binding is best-effort.
fn agent_init(args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;
    let Some(name) = args.first().map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        anyhow::bail!("agit a init: name the agent — agit a init <name>");
    };
    let a = agent::init_agent(name)?;
    println!("minted {} ({})", a.name, a.aid);
    println!("  store {}", a.store.display());
    // Without the binding, `agit start` would still say "no agent selected" — the dead end this ends.
    bind_and_activate(&a)?;
    Ok(0)
}

/// `agit a clone <name|url>` — clone an agent's store by identity (the git-native name for `track`). A
/// bare name resolves through the committed binding; a URL clones that store and adopts its identity.
fn agent_clone(args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;
    let Some(target) = args.first().map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        anyhow::bail!("agit a clone: name an agent or a store URL — agit a clone <name|url>");
    };
    let a = agent::clone_agent(target, !args.iter().any(|x| x == "--no-switch"))?;
    println!("cloned {} ({})", a.name, a.aid);
    println!("  store {}", a.store.display());
    Ok(0)
}

/// `agit a push [git-push-args…]` — the agent-context push. It runs the real git push on the store
/// with the caller's args untouched, then, on success, records the store's `origin` in the committed
/// binding so a teammate's clone can find this agent. That binding write is the only thing separating
/// this from bare passthrough; everything about the push itself is git's.
/// `agit a commit [git-commit-args…]` — commit into the Agent Store, but scan the staged INDEX first.
/// A bare passthrough fires only git's pre-commit hook, which `--no-verify` skips; agit owns this entry
/// point, so it gates here and refuses on findings before any commit exists. On pass it delegates
/// through passthrough (so the WorkspaceRevision post-hook still fires) with git's now-redundant hook
/// skipped. The visible AGIT_ALLOW_SECRETS override is disclosed by the gate, never silent.
fn agent_commit(args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;
    let a = agent::resolve(None)?;
    // `git commit -a` and `git commit <pathspec>` stage content AT COMMIT TIME, after a pre-commit index
    // scan runs, and the `--no-verify` below skips git's own hook. So stage that content into the index
    // NOW, before the gate, so the scan sees exactly what the commit will contain. The commit re-stages
    // the same bytes (a no-op), so -a/pathspec semantics are preserved. Without this, `agit a commit -a`
    // could commit a secret the index scan never looked at (it was strictly less safe than bare git).
    stage_commit_inputs(&a.store, args);
    if !commands::secret_gate(&a.store, true, "commit")?.allowed() {
        return Ok(1);
    }
    // The wrapper already gated the index; `--no-verify` skips git's redundant pre-commit hook (kept
    // installed for a raw `git commit` that never went through agit). Keeping --no-verify also preserves
    // the visible AGIT_ALLOW_SECRETS override, which the raw hook deliberately does not honor.
    let mut rest: Vec<String> = vec!["commit".into(), "--no-verify".into()];
    rest.extend(args.iter().cloned());
    passthrough::run(Scope::Agent, &rest)
}

/// Stage into the store's index exactly what a `git commit` with these args would add, so the in-process
/// secret gate scans it before the commit exists. `-a`/`--all` stages tracked modifications; any argument
/// that names an existing path (a pathspec, incl. everything after `--`) is staged too. Option VALUES are
/// skipped (so `-m <msg>` never stages a file), and the existence check makes a misread value harmless.
fn stage_commit_inputs(store: &std::path::Path, args: &[String]) {
    use agit::scope::git_in_status;
    // git-commit flags that consume the following token as their value.
    const TAKES_VALUE: &[&str] = &[
        "-m", "--message", "-F", "--file", "-C", "--reuse-message", "-c", "--reedit-message",
        "--author", "--date", "--fixup", "--squash", "-t", "--template", "--cleanup",
        "--pathspec-from-file", "--trailer",
    ];
    if args.iter().any(|x| x == "-a" || x == "--all") {
        let _ = git_in_status(store, &["add", "-u"]);
    }
    let mut i = 0;
    let mut after_dashdash = false;
    while i < args.len() {
        let x = &args[i];
        if !after_dashdash && x == "--" {
            after_dashdash = true;
            i += 1;
            continue;
        }
        if !after_dashdash && x.starts_with('-') {
            i += if TAKES_VALUE.contains(&x.as_str()) && i + 1 < args.len() { 2 } else { 1 };
            continue;
        }
        // A pathspec. Stage it only if it resolves to a real path in the store, so a misclassified
        // option value (which won't exist as a file) is silently ignored rather than erroring the commit.
        if store.join(x).exists() {
            let _ = git_in_status(store, &["add", "--", x]);
        }
        i += 1;
    }
}

/// The SOURCE ref of a git refspec `[+]<src>[:<dst>]` — the local ref being published. A bare `:<dst>`
/// (a branch delete) has no source and publishes nothing to scan, so it yields None.
fn refspec_source(spec: &str) -> Option<String> {
    let s = spec.strip_prefix('+').unwrap_or(spec);
    let src = s.split(':').next().unwrap_or(s);
    (!src.is_empty()).then(|| src.to_string())
}

fn agent_push(args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;
    let a = agent::resolve(None)?;

    // Split agit-native `--to <name>` out of what git sees, then classify the rest: `flags` start with
    // `-` (--force, --tags), `positionals` are a git-style remote/refspec the user typed.
    let mut to: Option<String> = None;
    let mut flags: Vec<String> = Vec::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let x = &args[i];
        if x == "--to" {
            // A bare `--to` with nothing after it is a usage error, not a silent fall-through to the
            // fan-out (which would push to EVERY remote, the opposite of what `--to` asks for).
            let Some(v) = args.get(i + 1) else {
                anyhow::bail!(
                    "agit a push --to needs a remote name: agit a push --to <name> [refspec...]"
                );
            };
            to = Some(v.clone());
            i += 2;
            continue;
        }
        if let Some(v) = x.strip_prefix("--to=") {
            if v.is_empty() {
                anyhow::bail!(
                    "agit a push --to needs a remote name: agit a push --to <name> [refspec...]"
                );
            }
            to = Some(v.to_string());
        } else if x.starts_with('-') {
            flags.push(x.clone());
        } else {
            positionals.push(x.clone());
        }
        i += 1;
    }

    // The gate runs BEFORE git — a bare `git push` fires only the pre-push hook, which `--no-verify`
    // skips, and agit's own push must not offer that silent exit. Pushing publishes the store to the
    // team, so this is the last line before a secret in a transcript leaves the machine. It scans the
    // COMMIT RANGE about to be published (the session blobs HEAD has that the target remotes don't),
    // NOT the working tree: that catches a secret committed with a raw `git commit --no-verify` and then
    // deleted from the working tree (which a working-tree scan misses), and it won't block on an
    // uncommitted working-tree secret that would never ship. Blocked → refuse without touching the
    // remote; the visible AGIT_ALLOW_SECRETS override is disclosed by the gate.
    let gate_remotes: Vec<String> = if let Some(name) = &to {
        vec![name.clone()]
    } else if let Some(remote) = positionals.first() {
        // git-style `push <remote> [refspec...]`: the first positional is the remote.
        vec![remote.clone()]
    } else {
        // Bare push fans out to every configured remote; the range is the union of what each still lacks.
        agent::store_remotes(&a.store)
            .into_iter()
            .map(|(n, _)| n)
            .collect()
    };
    // The SOURCE refs actually being published decide what range to scan, NOT always HEAD. With `--to`
    // every positional is a refspec (the remote is `to`); git-style, positionals[0] is the remote and the
    // rest are refspecs. `--all`/`--mirror` pushes every branch, so scan every local branch tip. No
    // refspec means the current branch (HEAD). Without this, `agit a push origin leak` (a non-HEAD
    // branch) would ship a secret a HEAD-only scan never looked at.
    let refspecs: &[String] = if to.is_some() {
        &positionals
    } else if positionals.len() > 1 {
        &positionals[1..]
    } else {
        &[]
    };
    let gate_sources: Vec<String> = if flags.iter().any(|f| f == "--all" || f == "--mirror") {
        let (_c, out) = agit::scope::git_in_status(&a.store, &["for-each-ref", "--format=%(refname)", "refs/heads"]);
        out.lines().map(str::to_string).collect()
    } else {
        refspecs.iter().filter_map(|s| refspec_source(s)).collect()
    };
    if !agit::commands::secret_gate_range(&a.store, &gate_sources, &gate_remotes, "push")?.allowed() {
        return Ok(1);
    }

    // Inherited stdio throughout: a push is where credential helpers prompt, and capturing would both
    // swallow git's errors and block the prompt. `--no-verify` skips git's now-redundant pre-push hook
    // (we just gated the tree); the hook stays installed for a raw `git push`.
    let (exit, record) = if let Some(name) = to {
        // (a) Targeted single push — the command's identity anchor. Any positional refspec the user
        // typed (`--to hub main`, `--to hub HEAD:review`) rides along AFTER the remote name, so a
        // targeted push is not silently narrowed to a bare `git push <remote>`.
        let mut pa: Vec<&str> = vec!["push", "--no-verify"];
        pa.extend(flags.iter().map(String::as_str));
        pa.push(&name);
        pa.extend(positionals.iter().map(String::as_str));
        let code = scope::git_in_inherit(&a.store, &pa);
        (code, code == 0)
    } else if !positionals.is_empty() {
        // (b) Git-style passthrough — today's behavior, preserved verbatim.
        let mut pa: Vec<&str> = vec!["push", "--no-verify"];
        pa.extend(args.iter().map(String::as_str));
        let code = scope::git_in_inherit(&a.store, &pa);
        (code, code == 0)
    } else {
        // (c) Bare push — fan out to every configured remote. A per-remote rejection is reported but
        // never fatal; the exit reflects the PRIMARY remote's push (always recorded after the loop).
        (push_fanout(&a, &flags), true)
    };

    // Reconcile ALL of the store's remotes into the binding, only inside an environment — a bare store
    // has nowhere to write one, and that is not an error.
    if record {
        if let Ok(env) = scope::env_root() {
            let summary = agent::sync_remotes_to_binding(&a.aid, &env)?;
            print_sync_summary(&a.name, &summary);
        }
    }
    Ok(exit)
}

/// Fan a bare `agit a push` out to every git remote the store has: an explicit `<name> <branch>`
/// refspec per remote (a freshly `remote add`-ed hub has no upstream, so a bare `git push <name>` under
/// `push.default=simple` would error). Each remote's success/failure is printed and non-fatal; the
/// returned exit is the PRIMARY remote's push code — the command's identity anchor.
fn push_fanout(a: &agit::agent::Agent, flags: &[String]) -> i32 {
    use agit::agent;
    let remotes = agent::store_remotes(&a.store);
    // No remotes: fall back to today's plain push (whatever upstream the store has, or git's own error).
    if remotes.is_empty() {
        let mut pa: Vec<&str> = vec!["push", "--no-verify"];
        pa.extend(flags.iter().map(String::as_str));
        return scope::git_in_inherit(&a.store, &pa);
    }
    let (bcode, branch) = scope::git_in_status(&a.store, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let branch = branch.trim().to_string();
    // A detached HEAD (or a failed rev-parse) has no branch to fan out; fall back to a plain push.
    if bcode != 0 || branch.is_empty() || branch == "HEAD" {
        let mut pa: Vec<&str> = vec!["push", "--no-verify"];
        pa.extend(flags.iter().map(String::as_str));
        return scope::git_in_inherit(&a.store, &pa);
    }
    let primary = agent::primary_remote_name(&a.store);
    let mut anchor = 0;
    for (name, _url) in &remotes {
        let mut pa: Vec<&str> = vec!["push", "--no-verify"];
        pa.extend(flags.iter().map(String::as_str));
        pa.push(name.as_str());
        pa.push(&branch);
        let code = scope::git_in_inherit(&a.store, &pa);
        if code == 0 {
            println!("  pushed {name}");
        } else {
            // A non-owner's push to a personal hub returns non-zero from the ACL 403 — shown, not fatal.
            eprintln!("  push to {name} rejected (exit {code}) — reported, not fatal");
        }
        if primary.as_deref() == Some(name.as_str()) {
            anchor = code;
        }
    }
    anchor
}

/// The "skipped remote" note, BY NAME ONLY. A remote is often skipped because its URL carries a secret
/// credential-stripping can't remove (e.g. a `?private_token=` query), and this note prints on every
/// push, so it must never echo the raw URL or it leaks the secret to the terminal and CI logs.
fn skipped_note(name: &str, bf: &str) -> String {
    format!("  note: remote {name} is not a transport agit will record in {bf} — left unchanged.")
}

/// Print what `sync_remotes_to_binding` recorded/skipped. Recorded lines only appear on a change (an
/// idempotent re-push is silent); a skipped remote is a standing problem worth repeating every push.
fn print_sync_summary(agent_name: &str, s: &agit::agent::RemoteSyncSummary) {
    let bf = agit::agent::BINDING_FILE;
    if s.changed {
        for r in &s.recorded {
            let tag = if r.primary { "  (primary — clone/pull anchor)" } else { "" };
            println!("  bound  {agent_name} → {}{tag}", r.locator);
        }
        if !s.recorded.is_empty() {
            println!("         (commit {bf}: teammates clone the agent from here)");
        }
        if s.recorded.iter().any(|r| r.stripped) {
            eprintln!("  note: credentials stripped from a recorded remote — {bf} is committed.");
            eprintln!("        The store keeps the full URL locally; your teammates' git supplies their own.");
        }
    }
    for (name, _url) in &s.skipped {
        eprintln!("{}", skipped_note(name, bf));
    }
}

/// `agit a encrypt […]` — the encryption lifecycle verb. `--rotate` and `--purge-history` are the two
/// lifecycle operations handled here; everything else (enable/wire, --export/--import) delegates to the
/// base implementation unchanged.
fn agent_encrypt(args: &[String]) -> anyhow::Result<i32> {
    if args.iter().any(|a| a == "--rotate") {
        return agent_encrypt_rotate(args);
    }
    if args.iter().any(|a| a == "--purge-history") {
        return agent_encrypt_purge_history(args);
    }
    commands::agent_encrypt(args)
}

/// A yes/no gate honoured by `--yes`/`-y` non-interactively; refuses (never hangs) when it cannot ask.
fn confirm_or_yes(prompt: &str, yes: bool) -> anyhow::Result<bool> {
    use std::io::Write;
    if yes {
        return Ok(true);
    }
    if !ui::interactive() {
        anyhow::bail!(
            "{prompt}\n  refusing without confirmation — re-run with --yes to proceed non-interactively"
        );
    }
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// Commit whatever is staged in `store` under `message`, via `-F <tempfile>` (never `-m`, per the repo's
/// commit-message hygiene). The caller stages + checks there is something to commit.
fn commit_store(store: &std::path::Path, message: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut msg = tempfile::NamedTempFile::new()?;
    msg.write_all(message.as_bytes())?;
    msg.flush()?;
    scope::git_in(store, &["commit", "--no-verify", "-F", msg.path().to_string_lossy().as_ref()])?;
    Ok(())
}

/// `agit a encrypt --rotate [--yes]` — mint a NEW current key (retiring the old for decrypt-only) and
/// re-encrypt this store's working tree under it via `git add --renormalize`. Going-forward blobs use the
/// new key; every blob a retired key sealed (history, or a not-yet-renormalized clone) still decrypts.
fn agent_encrypt_rotate(args: &[String]) -> anyhow::Result<i32> {
    use agit::{agent, crypt};
    let home = scope::agit_home()?;
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");

    // Rotation only makes sense once encryption is enabled — there is otherwise no key to rotate.
    if crypt::read_master(&home)?.is_none() {
        anyhow::bail!(
            "agit-crypt is not enabled on this machine — run `agit a encrypt` first, then --rotate"
        );
    }
    let a = agent::resolve(None)?;
    let store = a.store.clone();

    eprintln!("agit encrypt --rotate — mint a new key and re-encrypt this store's working tree.");
    eprintln!(
        "  Retired keys are kept LOCALLY so history / not-yet-renormalized blobs still decrypt; only\n\
         \x20     new writes use the new key. Afterwards you must `agit a push` and re-share the key\n\
         \x20     (`agit a encrypt --export <file>`) so teammates can decrypt going-forward blobs."
    );
    if !confirm_or_yes("Rotate the crypt key and re-encrypt this store now?", yes)? {
        eprintln!("aborted — key not rotated");
        return Ok(1);
    }

    let _lock = session::lock_store(&store)?;
    let new_id = crypt::rotate_key(&home)?;
    println!("  minted key-id {new_id} (now current); every prior key retained for decrypt only.");

    // Re-encrypt the tracked working tree under the new current key: --renormalize pushes every tracked
    // sessions blob back through the clean filter, which now seals under the new key.
    let _ = scope::git_in_status(&store, &["add", "--renormalize", "--", "sessions"]);
    if scope::git_in_status(&store, &["diff", "--cached", "--quiet"]).0 != 0 {
        commit_store(
            &store,
            "chore(crypt): rotate key and re-encrypt sessions\n\nThe new current key seals going-forward blobs; retired keys are kept locally to decrypt prior and history blobs.",
        )?;
        println!("  re-encrypted tracked sessions under key-id {new_id} (git add --renormalize)");
    } else {
        println!("  no tracked sessions to re-encrypt.");
    }
    eprintln!(
        "  ⚠ BACK UP and re-share the new key now: `agit a encrypt --export <file>`, then `agit a push`.\n\
         \x20     git HISTORY still holds blobs under the retired key; `agit a encrypt --purge-history`\n\
         \x20     rewrites them out if you need pre-rotation plaintext/ciphertext gone."
    );
    Ok(0)
}

/// `agit a encrypt --purge-history [--yes]` — rewrite git history to remove pre-encryption plaintext
/// (or blobs under a retired key) via `git filter-repo`. Rewrites every commit and needs a force-push, so
/// it demands an explicit confirmation (`--yes` non-interactively) and prints install instructions when
/// `git filter-repo` is absent.
fn agent_encrypt_purge_history(args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let a = agent::resolve(None)?;
    let store = a.store.clone();

    // `git filter-repo` is a separate install; without it we cannot rewrite history — print how to get it.
    if !filter_repo_available() {
        eprintln!("git-filter-repo is not installed — it is required to rewrite history.");
        eprintln!("  Install it, then re-run:");
        eprintln!("    pip install git-filter-repo         # or");
        eprintln!("    brew install git-filter-repo         # macOS");
        eprintln!("    apt-get install git-filter-repo      # Debian/Ubuntu");
        eprintln!("  See https://github.com/newren/git-filter-repo for other platforms.");
        return Ok(1);
    }

    eprintln!("agit encrypt --purge-history — DESTRUCTIVE: this REWRITES this store's ENTIRE git history.");
    eprintln!(
        "  It re-runs every commit through the encryption clean filter so pre-encryption plaintext (and\n\
         \x20     blobs under retired keys) no longer exist in ANY commit. Consequences:\n\
         \x20       • every commit SHA changes — this is a history rewrite;\n\
         \x20       • you MUST `agit a push --force` afterwards, and every teammate must re-clone;\n\
         \x20       • it cannot be undone except from a backup. BACK UP THE STORE FIRST."
    );
    if !confirm_or_yes("Rewrite history and purge pre-encryption plaintext now?", yes)? {
        eprintln!("aborted — history not rewritten");
        return Ok(1);
    }

    // Re-run every historical blob through agit's own clean filter via a filter-repo blob-callback.
    // git-filter-repo does not re-apply gitattributes filters, so we seal explicitly. The callback is
    // path-safe by content: it leaves anything already ciphertext (starts with the AGITCRYPT magic) and
    // anything that is not session JSONL (session files begin with `{`) untouched, so non-session blobs
    // (.gitattributes, README, .agit.toml) are never encrypted. `agit crypt-clean` seals under the
    // CURRENT key, keyed from $AGIT_HOME regardless of cwd. `--force` because filter-repo refuses on a
    // non-fresh clone; this is an explicit, confirmed rewrite.
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("could not locate agit's own path: {e}"))?;
    let callback = "import os, subprocess\n\
        MAGIC = b\"AGITCRYPT\\x00\"\n\
        d = blob.data\n\
        if (not d.startswith(MAGIC)) and d[:1] == b\"{\":\n\
        \x20   blob.data = subprocess.run([os.environ[\"AGIT_SELF\"], \"crypt-clean\"], input=d, stdout=subprocess.PIPE, check=True).stdout\n";
    let code = {
        use std::process::Command;
        Command::new("git")
            .arg("-C")
            .arg(&store)
            .args(["filter-repo", "--force", "--blob-callback", callback])
            .env("AGIT_SELF", &exe)
            .status()
            .map(|s| s.code().unwrap_or(1))
            .unwrap_or(1)
    };
    if code != 0 {
        eprintln!("git filter-repo exited {code} — history was NOT rewritten.");
        return Ok(code);
    }
    println!("  history rewritten: every commit re-encrypted through the clean filter.");
    eprintln!(
        "  ⚠ NEXT: `agit a push --force` to publish the rewritten history, and tell every teammate to\n\
         \x20     re-clone — their old clones still hold the pre-purge plaintext."
    );
    Ok(0)
}

/// Is `git filter-repo` runnable? (`git filter-repo --version` exits 0.)
fn filter_repo_available() -> bool {
    std::process::Command::new("git")
        .args(["filter-repo", "--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `agit a pull [git-pull-args…]` — the agent-context pull. A bare `git pull` on the store would do a
/// TEXTUAL merge of the jsonl transcripts on divergence, corrupting exactly the sessions `agit a merge`
/// reconciles by dialogue. So this fast-forwards when it safely can (`--ff-only`), and on divergence
/// refuses and routes to merge rather than splice transcripts line by line.
fn agent_pull(args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;
    let a = agent::resolve(None)?;

    // `--ff-only`: git integrates by fast-forward or not at all — never a textual merge. The caller's
    // args (an explicit remote/branch) ride along. Inherited stdio so credential helpers can prompt.
    let mut pull_args: Vec<&str> = vec!["pull", "--ff-only"];
    pull_args.extend(args.iter().map(String::as_str));
    let code = scope::git_in_inherit(&a.store, &pull_args);
    if code == 0 {
        return Ok(0);
    }

    // The pull failed. If it failed because the two sides DIVERGED (each holds commits the other
    // lacks), that is the case dialogue-merge exists for — say so, with the command to run. Any other
    // failure (no upstream, network) is git's own and already printed above.
    if commands::diverged(&a.store) {
        eprintln!();
        eprintln!("your sessions and the remote's have diverged — a textual git merge would corrupt the");
        eprintln!("transcripts. Reconcile them by dialogue instead:");
        eprintln!("  agit a merge {}", a.name);
    }
    Ok(code)
}

/// `agit a fetch [git-fetch-args…]` — the agent-context fetch. A bare git fetch only advances
/// remote-tracking refs, so nothing lands in the working tree to convert; what it *can* do that plain
/// git won't is report the incoming work in session terms — how many sessions arrived, from which
/// runtime — and how to integrate them.
fn agent_fetch(args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;
    let a = agent::resolve(None)?;

    let mut fetch_args: Vec<&str> = vec!["fetch"];
    fetch_args.extend(args.iter().map(String::as_str));
    let code = scope::git_in_inherit(&a.store, &fetch_args);
    if code != 0 {
        return Ok(code);
    }
    commands::report_incoming(&a.store);
    Ok(0)
}

fn merge_cmd(args: &[String]) -> anyhow::Result<i32> {
    let mut rt: Option<String> = None;
    let mut reference = None;
    let mut both = false;
    let mut quick = false;
    let mut splice = false;
    let mut dry_run = false;
    let mut prefer = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" if i + 1 < args.len() => {
                rt = Some(args[i + 1].clone());
                i += 2;
            }
            // `--agent X` / `--ref X` both name the target AND disambiguate it.
            "--agent" if i + 1 < args.len() => {
                reference = Some(args[i + 1].clone());
                prefer = Some(sync::Prefer::Agent);
                i += 2;
            }
            "--ref" if i + 1 < args.len() => {
                reference = Some(args[i + 1].clone());
                prefer = Some(sync::Prefer::Ref);
                i += 2;
            }
            "--both" => {
                both = true;
                i += 1;
            }
            "--quick" => {
                quick = true;
                i += 1;
            }
            // The no-model merge: combine both sessions' context into one, skip the dialogue entirely.
            "--splice" => {
                splice = true;
                i += 1;
            }
            // Preview only: show what the merge WOULD do — no model, no install, no worktrees, no fuse.
            "--dry-run" | "--preview" => {
                dry_run = true;
                i += 1;
            }
            other => {
                if reference.is_none() && !other.starts_with('-') {
                    reference = Some(other.to_string());
                }
                i += 1;
            }
        }
    }
    match reference {
        Some(r) => {
            // The dialogue merge is one conversation, so it acts on exactly one runtime. Resolve it
            // against the sessions actually in the store — running two LLM dialogues is not a
            // meaningful "do both", so genuine ambiguity asks instead.
            //
            // The RESOLVED agent's store, matching sync::run: reading the legacy `<env>/.agit/agent`
            // here made merge demand a store nothing writes to any more, so it died with "run `agit
            // init` first" in a repo that already had an agent selected.
            let agent = agit::agent::resolve(None)?.store;
            let rt = session::resolve_runtime(rt.as_deref(), &session::store_runtimes(&agent), "merge")?;
            sync::run(&r, &rt, both, quick, splice, dry_run, prefer)
        }
        None => {
            eprintln!("usage: agit a merge <target> [--from <runtime>] [--both] [--quick] [--splice] [--dry-run]   (reconcile this agent's memory with <target>'s by dialogue; <target> is an agent name or a ref — --agent X / --ref X disambiguate; --quick shortens the dialogue; --splice skips the model and just combines both sessions' context; --dry-run/--preview shows what the merge would do without running the model)");
            Ok(2)
        }
    }
}

fn parse_scan(args: &[String]) -> (bool, Vec<PathBuf>) {
    let mut staged = false;
    let mut paths = Vec::new();
    for a in args {
        if a == "--staged" {
            staged = true;
        } else {
            paths.push(PathBuf::from(a));
        }
    }
    (staged, paths)
}

const USAGE: &str = "\
agit — version an agent's raw session so teams can collaborate on Agent Context

Works with claude-code and codex. Commands that read sessions use the one you name with --from, else
the only one present, else they ask.

  agit init [--agent N]    Prepare this repo: clone or select its agent, install the secret hooks (--agent names one)
  agit a snap [--watch]    Mirror this project's session dump + harness (MCP/skills/config, secrets redacted) into the Agent Store; captures every runtime with sessions here unless --from names one (--watch = auto-snap; --no-harness = sessions only)
  agit a push / pull       Push your sessions to, and pull the team's back from, the shared store (the Agent Store is just a git repo)
  agit start               Launch a session HERE already carrying this agent's latest context, from whatever repo it was last in (--agent <name> picks the agent for this invocation only; --as <rt> switches runtime)
  agit a merge <target>    Merge this agent's memory with <target>'s by dialogue (alias: sync); <target> is an agent name or a ref — never a code branch. Same agent → the histories merge too; a different agent → dialogue only, both stay intact (--agent X / --ref X disambiguate)
  agit a clone <url>       Clone an agent published on a hub (its memory, by identity)
  agit a scan [--staged]   Scan session dumps for secrets
  agit workspace [log]     Show the Agent↔Environment pairing
  agit workspace restore [N]  Roll both repos back together to a pairing's joint state
  agit watch [--daemon]    Hands-off: watch claude-code, codex, auto-snap + auto-convert both ways; --daemon runs it in the background forever (--stop / --status to manage)
  agit graph               Show the Workspace-State timeline + relation edges
  agit harness [apply]     Show, or apply, the captured harness (MCP/skills/config); apply asks first (--force to skip)
  agit adapter             List runtime adapters
  agit shadow [install]    Route `git` through `agit` in your shell so every git command versions agent context (uninstall / status to manage)
  agit convert <src> --to <rt>  Convert a session into one another runtime can resume (--write to persist; --watch auto-converts both ways in the background)
  agit resume <src>        Load a session into a runtime and continue (--as <rt> to switch runtime; --env <path> to run this agent against a different repo; --relocate if it's the same project moved; --exec to launch)
  agit provenance verify <session>  Check a captured session's signature against its recorded key (unsigned → unverified, never blocks); `agit provenance key` shows this machine's public key

  agit <git-args>          Run git transparently on the code repository (Environment)
  agit agent <git-args>    Run isomorphic git on the Agent Store — `agit a` is the alias (agit a log · agit a add -A · agit a commit · agit a push)

  agit a status            Overview of this repo: its agents, which is active, each one's sessions + last activity + live-watcher, and the active store's unpushed/ahead-behind
  agit a log / diff        The SESSION view of the store (a timeline of sessions; the prompts + edits added between two refs) — pass --raw for the byte-level git log/diff

  agit agent <verb>        Agent management, a closed set: init, clone, switch, list, status, info, rename, rebind, merge (push/pull/fetch too).
                           Anything else after `a` is git, so `agit a add -A` is git-add and `agit a show` is git-show.

  `a` is a subcommand, so it cannot be transposed: agit a commit (agent store) vs agit commit -a (code repo, -a is git's stage-all).
  The old `agit -a <args>` flag still works as a deprecated alias.";

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a skipped remote note must NEVER echo the remote URL. A remote is often skipped
    /// because its URL carries a secret credential-stripping can't remove, and the note prints on every
    /// push, so echoing the URL would leak the secret to the terminal and CI logs.
    #[test]
    fn skipped_note_never_echoes_the_remote_url() {
        let note = skipped_note("hub", ".agit.toml");
        assert!(note.contains("hub"), "names the remote: {note}");
        assert!(!note.contains("://"), "must not contain a URL scheme: {note}");
        assert!(
            !note.to_lowercase().contains("token") && !note.contains('@') && !note.contains('?'),
            "must not leak credential-bearing URL parts: {note}"
        );
    }
}
