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
    // git-parity: Rust's runtime IGNORES SIGPIPE, so the first `println!` into a closed pipe
    // (`agit a log | head`, `| less`, `| grep -m1`) fails and the print macro PANICS
    // ("failed printing to stdout: Broken pipe", exit 101). Reset the disposition to the default
    // BEFORE any output so the process is simply killed by SIGPIPE, exactly like git.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
                println!("no agents yet: agit a init <name> mints one.");
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
                        .unwrap_or_else(|| "-".into());
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
            println!("remote {}", a.remote.as_deref().unwrap_or("(local only; agit a push records one)"));
            Ok(0)
        }
        // `switch` is the git-native name for "select this worktree's active agent" — the smart
        // version of `git switch` on the store.
        "switch" => {
            let a = agent::switch_agent(&need("name|aid")?)?;
            println!("{} {} ({}): this worktree's agent", ui::accent("●"), a.name, a.aid);
            Ok(0)
        }
        "rename" => {
            let old = need("old-name")?;
            let new = match args.get(1).map(|s| s.trim()).filter(|s| !s.is_empty()) {
                Some(n) => n.to_string(),
                None => anyhow::bail!("agit agent rename: missing <new-name>\n  usage: agit a rename <old> <new>"),
            };
            let a = agent::rename(&old, &new)?;
            println!("renamed {old} → {} ({}, unchanged)", a.name, a.aid);
            Ok(0)
        }
        // Override the integrity check (`--remote <url>`), or give a forked store its own identity
        // (`--new-id`). A repair verb — no git primitive means "accept a changed identity".
        "rebind" => {
            let remote = args.iter().position(|a| a == "--remote").and_then(|i| args.get(i + 1)).cloned();
            let name = args.first().filter(|a| !a.starts_with("--")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            let a = agent::rebind(name.as_deref(), remote.as_deref(), flag("--new-id"))?;
            println!("rebound {} ({})", a.name, a.aid);
            println!("  remote {}", a.remote.as_deref().unwrap_or("-"));
            Ok(0)
        }
        // Unreachable: every verb in AGENT_MGMT_VERBS has an arm above. Kept as a defensive default so
        // adding a name to the closed set without an arm fails loudly here rather than falling to git.
        v => {
            eprintln!("agit agent {v}: no handler (bug: closed-set verb with no arm).");
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
        //    `agit init --help` must PRINT usage and touch nothing — it used to EXECUTE init (appending
        //    `.agit/` to .gitignore) because the parser ignored every arg but `--agent`. ──
        "init" => match parse_init(args) {
            Ok(agent) => init::run_named(agent),
            Err(ctl) => Ok(ctl.emit(INIT_USAGE)),
        },
        // ── clone (default/Environment scope): git's clone, but smart about agit-hub AGENT STORES. A raw
        //    `git clone <hub-store-url>` silently makes a nested git repo that resolves to no agent; so a
        //    POSITIVELY-identified agit-hub store URL, or a bare name that is a KNOWN local agent/binding,
        //    is adopted as an agent (agit a clone) instead. `--git` forces the raw passthrough clone, and
        //    any other target is unchanged git passthrough. (Only the default scope: `agit a clone` is the
        //    explicit agent path above, and never reaches here.) ──
        "clone" if scope == Scope::Environment => clone_cmd(rest, args),
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
        "scan" => match parse_scan(args) {
            Ok((staged, paths)) => commands::scan_cmd(scope, staged, &paths),
            Err(ctl) => Ok(ctl.emit(SCAN_USAGE)),
        },
        "hook-scan" => commands::hook_scan(args.iter().any(|a| a == "--staged")),

        // ── crypt-clean / crypt-smudge: the agit-crypt filter drivers. Scope-INDEPENDENT: git invokes
        //    them as `filter.agit-crypt.{clean,smudge}`, never the user. stdin→stdout binary pipes,
        //    keyed from $AGIT_HOME. Routed here beside hook-scan (also git-invoked). ──
        "crypt-clean" => commands::crypt_clean(),
        "crypt-smudge" => commands::crypt_smudge(),
        // The `--index-filter` body of `agit a purge-history`'s filter-branch backend: re-seals sessions/**
        // in the current index. git-invoked (never a human), keyed from $AGIT_HOME + the repo-local keyring.
        "crypt-purge-index" => commands::crypt_purge_index(),

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
        "snap" => match parse_snap(args) {
            Ok((rt, watch, interval, harness)) => match (watch, rt) {
                // The watcher polls one runtime's dump; unnamed, `agit watch` is the both-runtimes loop.
                (true, Some(rt)) => session::snap_watch_checked(&rt, interval, harness),
                (true, None) => session::watch(interval, false, harness),
                (false, rt) => session::snap(rt.as_deref(), harness),
            },
            Err(ctl) => Ok(ctl.emit(SNAP_USAGE)),
        },

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

        // ── purge-history (agent scope): guard-railed history rewrite that re-encrypts every historical
        //    revision of sessions/** so no pre-encryption plaintext survives in any commit. Never
        //    auto-pushes — it prints the exact force-push command(s) to run after review. ──
        "purge-history" if scope == Scope::Agent => commands::purge_history_cmd(args),

        // ── escrow (agent scope): OPT-IN hub-assist key escrow (encryption-recipients Wave 5). `enable`
        //    wraps this session's content key under the hub escrow key and uploads it, but ONLY if the
        //    owning org is in hub-assist mode. Re-trusts the hub; off by default. ──
        "escrow" if scope == Scope::Agent => commands::escrow_cmd(args),

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
        "watch" => match parse_watch(args) {
            Ok((interval, convert, harness, action)) => match action {
                1 => session::watch_daemon(interval, convert, harness),
                2 => session::watch_stop(),
                3 => session::watch_status(),
                _ => session::watch(interval, convert, harness),
            },
            Err(ctl) => Ok(ctl.emit(WATCH_USAGE)),
        },

        // ── harness: show / apply the captured MCP + skills + config (part of Agent State) ──
        "harness" => {
            let (sub, rt, force, from_env) = parse_harness(args);
            harness_cmd(&sub, rt.as_deref(), force, from_env.as_deref())
        }

        // ── Convert a session across runtimes (resume it in another CLI); --watch = auto-convert worker ──
        "convert" if args.iter().any(|a| a == "--watch") => {
            // The auto-convert worker is an infinite, side-effecting loop. `--help`/`-h` must PRINT
            // usage and launch NOTHING; an unknown flag is rejected, not swallowed into the loop
            // (git-parity) — never a silent fall-through into the watcher.
            if args.iter().any(|a| a == "--help" || a == "-h") {
                Ok(ParseCtl::Help.emit(CONVERT_USAGE))
            } else if let Some(bad) = watch_convert_unknown_flag(args) {
                Ok(ParseCtl::Unknown(bad).emit(CONVERT_USAGE))
            } else {
                let interval = args
                    .iter()
                    .position(|a| a == "--interval")
                    .and_then(|i| args.get(i + 1))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(5);
                commands::convert_watch(interval)
            }
        }
        "convert" => match parse_convert(args) {
            Ok((sel, from, to, cwd, write)) => {
                commands::convert_cmd(sel.as_deref(), from, &to, cwd, write)
            }
            Err(ctl) => Ok(ctl.emit(CONVERT_USAGE)),
        },

        // ── resume: load a session into a runtime and continue (the universal loader) ──
        "resume" => match parse_resume(args) {
            Ok((sel, as_rt, cwd, env, exec, relocate)) => {
                commands::resume_cmd(sel.as_deref(), as_rt, cwd, env, exec, relocate)
            }
            Err(ctl) => Ok(ctl.emit(RESUME_USAGE)),
        },

        // ── Agent-store management verbs, checked before passthrough so they never reach git ──
        v if scope == Scope::Agent && AGENT_MGMT_VERBS.contains(&v) => agent_mgmt(v, args),

        // ── url-bearing passthrough verbs (Environment scope): a bare `agit fetch/pull/push <url>` or
        //    `agit remote add <name> <url>` against a POSITIVELY-identified agit-hub store is legitimate
        //    (the code repo can share a git host with a store), so it is NEVER hijacked — but a one-line
        //    hint points at `agit a <verb>`, which operates on the agent. The git passthrough then runs
        //    unchanged. A remote NAME (origin) or a non-hub URL prints nothing. ──
        "fetch" | "pull" | "push" | "remote" if scope == Scope::Environment => {
            hint_if_hub_url(cmd, args);
            passthrough::run(scope, rest)
        }

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


/// How a native argument parser asks dispatch to stop before running the command. git-parity: an
/// agit-native verb must NEVER silently swallow an unknown/misspelled `-flag` (treating it as a target,
/// or ignoring it) — that turns `scan --no-verify` into a scan of a nonexistent file that reports "no
/// secrets found", and `merge --dry-runn` into a REAL merge. Each parser returns `Err(ParseCtl::…)` and
/// dispatch renders it against that command's own usage.
enum ParseCtl {
    /// `--help`/`-h`: print this command's usage to stdout and exit 0 — with ZERO side effects.
    Help,
    /// A required argument is missing or invalid: print usage to stderr and exit 2.
    Usage,
    /// An unrecognized dash flag: print `unknown option '<flag>'` to stderr and exit 2, exactly like git.
    Unknown(String),
}

impl ParseCtl {
    /// Render this control against a command's usage string, returning the process exit code. `Help`
    /// goes to stdout (a requested help text is not an error); the two error forms go to stderr.
    fn emit(self, usage: &str) -> i32 {
        match self {
            ParseCtl::Help => {
                println!("{usage}");
                0
            }
            ParseCtl::Usage => {
                eprintln!("{usage}");
                2
            }
            ParseCtl::Unknown(flag) => {
                eprintln!("unknown option '{flag}'");
                eprintln!("{usage}");
                2
            }
        }
    }
}

const SCAN_USAGE: &str = "usage: agit a scan [--staged] [<file>…]   (scan session dumps for secrets)";
const SNAP_USAGE: &str = "usage: agit a snap [<runtime>] [--from <runtime>] [--watch] [--interval <n>] [--no-harness]   (mirror this project's session dump into the Agent Store)";
const CONVERT_USAGE: &str = "usage: agit convert [<session|agent>] --to claude-code|codex [--from RT] [--cwd PATH] [--write]\n  (no session -> the active agent's latest; an agent name -> that agent's latest; --watch [--interval N] to auto-convert both ways)";
const RESUME_USAGE: &str = "usage: agit resume [<session|agent>] [--as claude-code|codex] [--env PATH] [--relocate] [--cwd PATH] [--exec]\n  (no session -> the active agent's latest; an agent name -> that agent's latest)";
const INIT_USAGE: &str = "usage: agit init [--agent <name>]   (prepare this repo: clone or select its agent, install the secret hooks)";
const WATCH_USAGE: &str = "usage: agit watch [--interval <n>] [--no-convert] [--no-harness] [--daemon|--background] [--stop] [--status]\n  (hands-off: watch both runtimes' dumps, auto-snap + auto-convert both ways; --daemon runs it in the background, --stop/--status manage it)";
const MERGE_USAGE: &str = "usage: agit a merge <target> [--from <runtime>] [--both] [--quick] [--splice] [--dry-run]   (reconcile this agent's memory with <target>'s by dialogue; <target> is an agent name or a ref (--agent X / --ref X disambiguate); --quick shortens the dialogue; --splice skips the model and just combines both sessions' context; --dry-run/--preview shows what the merge would do without running the model)";

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

/// init arguments: `--agent <name>` names the agent to clone/select. Everything else is rejected so
/// `agit init --help` prints usage and runs NOTHING (it used to execute init and edit .gitignore), and a
/// misspelled flag errors instead of being silently ignored. Non-dash positionals are ignored, as before.
fn parse_init(args: &[String]) -> Result<Option<String>, ParseCtl> {
    let mut agent = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--agent" if i + 1 < args.len() => {
                agent = Some(args[i + 1].clone());
                i += 2;
            }
            "--help" | "-h" => return Err(ParseCtl::Help),
            other if other.starts_with('-') => return Err(ParseCtl::Unknown(other.to_string())),
            _ => i += 1,
        }
    }
    Ok(agent)
}

/// snap arguments: `--from <rt>` (or a bare positional) names the runtime, else snap captures every
/// runtime with sessions here; `--watch` runs it continuously, `--interval <n>`, `--no-harness`.
/// Returns (runtime, watch, interval, capture-harness).
type SnapArgs = (Option<String>, bool, u64, bool);
fn parse_snap(args: &[String]) -> Result<SnapArgs, ParseCtl> {
    // No default runtime: `--from` (or a bare positional) names one, otherwise snap captures every
    // runtime that has sessions here. See session::snap. An unknown dash flag is REJECTED, not swallowed
    // — `--no-harnes` (a typo of `--no-harness`) must not silently keep the harness on.
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
            "--help" | "-h" => return Err(ParseCtl::Help),
            other if other.starts_with('-') => return Err(ParseCtl::Unknown(other.to_string())),
            // a bare positional is shorthand for the runtime: `agit a snap codex`
            other => {
                rt = Some(other.to_string());
                i += 1;
            }
        }
    }
    Ok((rt, watch, interval, harness))
}

/// watch arguments: `--interval <n>`, `--no-convert`, `--no-harness`, and the action selector
/// `--daemon`/`--background` (1), `--stop` (2), `--status` (3); default run (0).
/// Returns (interval, do-convert, capture-harness, action).
type WatchArgs = (u64, bool, bool, u8);
fn parse_watch(args: &[String]) -> Result<WatchArgs, ParseCtl> {
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
            // `--help`/`-h` must PRINT usage and LAUNCH NOTHING: watch is an infinite, side-effecting
            // loop (it writes auto-snap commits), so a swallowed flag that falls into it is a footgun.
            "--help" | "-h" => return Err(ParseCtl::Help),
            // An unknown dash flag is REJECTED, not swallowed into the watcher (git-parity), exactly
            // like init/snap/scan/convert. A misspelled `--stpo` must not silently start the daemon.
            other if other.starts_with('-') => return Err(ParseCtl::Unknown(other.to_string())),
            // watch takes no positional; ignore a stray bare word, as before.
            _ => i += 1,
        }
    }
    Ok((interval, convert, harness, action))
}

/// The first dash argument in a `convert --watch` invocation that is neither `--watch` nor `--interval`
/// (whose numeric value is skipped). The auto-convert worker honors only those two, so any other flag is
/// an error, not a silent no-op that still enters the infinite loop.
fn watch_convert_unknown_flag(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--interval" => i += 2, // skip the flag and its numeric value
            "--watch" => i += 1,
            a if a.starts_with('-') => return Some(a.to_string()),
            _ => i += 1,
        }
    }
    None
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

/// convert arguments: OPTIONAL positional selector + --to (required) + --from/--cwd/--write.
/// The selector is a session path/id or an agent name (resolved client-side); absent means the active
/// agent's latest session. Returns None only when --to is missing.
type ConvertArgs = (Option<String>, Option<String>, String, Option<String>, bool);
fn parse_convert(args: &[String]) -> Result<ConvertArgs, ParseCtl> {
    let mut sel = None;
    let mut from = None;
    let mut to = None;
    let mut cwd = None;
    let mut write = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" if i + 1 < args.len() => {
                to = Some(args[i + 1].clone());
                i += 2;
            }
            "--from" if i + 1 < args.len() => {
                from = Some(args[i + 1].clone());
                i += 2;
            }
            "--cwd" if i + 1 < args.len() => {
                cwd = Some(args[i + 1].clone());
                i += 2;
            }
            "--write" => {
                write = true;
                i += 1;
            }
            "--help" | "-h" => return Err(ParseCtl::Help),
            // An unknown dash flag is REJECTED, not swallowed — `--wriet` (a typo of `--write`) must not
            // silently DRY-RUN and persist nothing at exit 0.
            other if other.starts_with('-') => return Err(ParseCtl::Unknown(other.to_string())),
            other => {
                if sel.is_none() {
                    sel = Some(other.to_string());
                }
                i += 1;
            }
        }
    }
    // `--to` is required — a convert with no target runtime is a usage error, not a silent no-op.
    let to = to.ok_or(ParseCtl::Usage)?;
    Ok((sel, from, to, cwd, write))
}

/// resume arguments: OPTIONAL positional selector + --as <rt> / --cwd <path> / --env <path> / --exec.
/// The selector is a session path/id or an agent name (resolved client-side); absent means the active
/// agent's latest session. resume has no required positional, so it fails only on an unknown flag or
/// `--help`.
type ResumeArgs = (Option<String>, Option<String>, Option<String>, Option<String>, bool, bool);
fn parse_resume(args: &[String]) -> Result<ResumeArgs, ParseCtl> {
    let mut sel = None;
    let mut as_rt = None;
    let mut cwd = None;
    let mut env = None;
    let mut exec = false;
    let mut relocate = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--as" if i + 1 < args.len() => {
                as_rt = Some(args[i + 1].clone());
                i += 2;
            }
            "--cwd" if i + 1 < args.len() => {
                cwd = Some(args[i + 1].clone());
                i += 2;
            }
            "--env" if i + 1 < args.len() => {
                env = Some(args[i + 1].clone());
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
            "--help" | "-h" => return Err(ParseCtl::Help),
            other if other.starts_with('-') => return Err(ParseCtl::Unknown(other.to_string())),
            other => {
                if sel.is_none() {
                    sel = Some(other.to_string());
                }
                i += 1;
            }
        }
    }
    Ok((sel, as_rt, cwd, env, exec, relocate))
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
        anyhow::bail!("agit a init: name the agent (agit a init <name>)");
    };
    let a = agent::init_agent(name)?;
    println!("minted {} ({})", a.name, a.aid);
    println!("  store {}", a.store.display());
    // Without the binding, `agit start` would still say "no agent selected" — the dead end this ends.
    bind_and_activate(&a)?;
    Ok(0)
}

/// `agit clone <target>` (default scope) — git's clone, made smart about agit-hub AGENT STORES.
///
/// A raw `git clone <hub-store-url>` silently makes a nested git repo that resolves to no agent, so a
/// target that is (a) a POSITIVELY-identified agit-hub store URL or (b) a bare name that is a KNOWN local
/// agent/binding is adopted via the agent path instead, with a one-line note. `--git` forces the raw
/// passthrough clone; any other target is unchanged git passthrough. Detection is fail-safe: an
/// unprobeable/offline host, or anything not positively an agit-hub, falls through to git untouched.
fn clone_cmd(rest: &[String], args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;
    // `--git` is the escape hatch: strip it and run the raw git clone verbatim, no probing at all.
    if args.iter().any(|a| a == "--git") {
        let git_args: Vec<String> = rest.iter().filter(|a| *a != "--git").cloned().collect();
        return passthrough::run(Scope::Environment, &git_args);
    }
    // No identifiable repository argument → nothing to classify; let git handle it (e.g. `clone --help`).
    let Some(target) = clone_target(args) else {
        return passthrough::run(Scope::Environment, rest);
    };
    // (b) a bare KNOWN agent/binding name (cheap, network-free), then (a) a positively-identified hub
    // store URL (a short, bounded probe). Anything else stays a raw git clone.
    let is_agent = agent::is_known_local_agent(&target) || agit::hubapi::is_hub_store_url(&target);
    if !is_agent {
        return passthrough::run(Scope::Environment, rest);
    }
    eprintln!(
        "detected an agit agent store - adopting it as an agent (agit a clone); use --git for a raw git clone"
    );
    let a = agent::clone_agent(&target, !args.iter().any(|x| x == "--no-switch"), false)?;
    println!("cloned {} ({})", a.name, a.aid);
    println!("  store {}", a.store.display());
    Ok(0)
}

/// The repository argument of a git-clone-style invocation — the first positional that is not an option
/// (nor an option's value). Value-taking clone options are skipped so their value is never mistaken for
/// the repo, and agit-native flags (`--git`, `--no-switch`) are ignored. `None` when there is no
/// positional (e.g. `agit clone --help`). A misread here only ever costs a redirect (the result is not a
/// known agent nor a hub URL → plain passthrough), never a wrong hijack.
fn clone_target(args: &[String]) -> Option<String> {
    // git-clone options that consume the following token as their value.
    const TAKES_VALUE: &[&str] = &[
        "-b", "--branch", "-o", "--origin", "-u", "--upload-pack", "--depth", "--reference",
        "--reference-if-able", "--separate-git-dir", "-c", "--config", "-j", "--jobs",
        "--shallow-since", "--shallow-exclude", "--template", "--filter", "--bundle-uri", "--server-option",
    ];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            // Everything after `--` is positional; the first is the repository.
            return args.get(i + 1).cloned();
        }
        if a == "--git" || a == "--no-switch" {
            i += 1; // agit-native, not git's — skip.
            continue;
        }
        if a.starts_with("--") && a.contains('=') {
            i += 1; // `--opt=value` is one token.
            continue;
        }
        if a.starts_with('-') {
            i += if TAKES_VALUE.contains(&a.as_str()) && i + 1 < args.len() { 2 } else { 1 };
            continue;
        }
        return Some(a.clone());
    }
    None
}

/// Print the hub-store hint for a url-bearing passthrough verb (`fetch`/`pull`/`push`/`remote add`) when
/// its target is a POSITIVELY-identified agit-hub store URL. Only a URL the user typed literally is
/// probed — a remote NAME (`origin`) is not a URL, so it never triggers a network call. Never hijacks:
/// the caller runs the git passthrough regardless.
fn hint_if_hub_url(cmd: &str, args: &[String]) {
    let Some(url) = hub_url_candidate(cmd, args) else {
        return;
    };
    if agit::hubapi::is_hub_store_url(&url) {
        let shown = agit::hubapi::redact_url(&url);
        eprintln!("{shown} is an agit agent store - `agit a {cmd}` operates on the agent");
    }
}

/// The URL a url-bearing passthrough verb points at, if any. For `remote` it is the store URL of a
/// `remote add <name> <url>` (the SECOND positional after `add`); for `fetch`/`pull`/`push` it is the
/// first positional (the `<repository>`). Returns `None` when there is no positional URL to consider.
fn hub_url_candidate(cmd: &str, args: &[String]) -> Option<String> {
    let positionals: Vec<String> = args.iter().filter(|a| !a.starts_with('-')).cloned().collect();
    if cmd == "remote" {
        // Only `remote add <name> <url>` carries a store URL; `remote -v`, `remove`, etc. do not.
        if args.first().map(String::as_str) != Some("add") {
            return None;
        }
        // Positionals are `add`, `<name>`, `<url>`: the store URL is the third.
        return positionals.get(2).cloned();
    }
    // fetch/pull/push <repository>: the first positional (a remote name is not a URL and won't probe).
    positionals.into_iter().next()
}

/// `agit a clone <name|url>` — clone an agent's store by identity (the git-native name for `track`). A
/// bare name resolves through the committed binding; a URL clones that store and adopts its identity.
///
/// `--init` is the empty-store path: when `<url>` is a store created but never pushed to, mint a fresh
/// agent into it and push, so the store becomes adoptable. `--no-switch` opts out of activating it here.
fn agent_clone(args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;
    // The target is the first positional; `--init` / `--no-switch` are agit-native flags, not a target.
    let Some(target) = args
        .iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty() && !s.starts_with("--"))
    else {
        anyhow::bail!("agit a clone: name an agent or store URL (agit a clone [--init] <name|url>)");
    };
    let init = args.iter().any(|x| x == "--init");
    let a = agent::clone_agent(target, !args.iter().any(|x| x == "--no-switch"), init)?;
    if init {
        // The store was empty; a fresh agent was minted into it and pushed.
        println!("minted {} ({}) into the empty store and pushed it", a.name, a.aid);
    } else {
        println!("cloned {} ({})", a.name, a.aid);
    }
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
        if code != 0 {
            hub_auth_hint(&a.store, &name);
        }
        (code, code == 0)
    } else if !positionals.is_empty() {
        // (b) Git-style `push <remote> [refspec...]`. When the user named ONLY a remote/url and NO
        // refspec, default the refspec to the current branch: a fresh branch with no upstream would
        // otherwise die with git's 'The current branch has no upstream branch'. With `-u` (already in
        // `flags`) this also sets the upstream. A refspec the user DID type (`origin main`,
        // `origin HEAD:x`) is passed through verbatim — untouched.
        let default_refspec: Option<String> =
            (positionals.len() == 1).then(|| current_branch(&a.store));
        let mut pa: Vec<&str> = vec!["push", "--no-verify"];
        pa.extend(flags.iter().map(String::as_str));
        pa.extend(positionals.iter().map(String::as_str));
        if let Some(spec) = default_refspec.as_deref() {
            pa.push(spec);
        }
        let code = scope::git_in_inherit(&a.store, &pa);
        if code != 0 {
            hub_auth_hint(&a.store, &positionals[0]);
        }
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

/// HEAD's branch, for defaulting a bare `push <remote>`'s refspec. `symbolic-ref --short HEAD` names the
/// checked-out branch; a detached HEAD (or any failure) falls back to the literal `HEAD`, which git
/// resolves to the current commit so the push still names a source.
fn current_branch(store: &std::path::Path) -> String {
    let (code, out) = scope::git_in_status(store, &["symbolic-ref", "--short", "HEAD"]);
    let b = out.trim();
    if code == 0 && !b.is_empty() {
        b.to_string()
    } else {
        "HEAD".to_string()
    }
}

/// A push `target` (a configured remote NAME or a literal URL) resolved to its URL, for the hub probe.
/// A configured remote wins (its stored URL carries the credential); otherwise an explicit `scheme://`
/// URL is taken as-is. A bare name that is no configured remote yields None — nothing to probe.
fn resolve_remote_url(store: &std::path::Path, target: &str) -> Option<String> {
    if let Some((_, url)) = agit::agent::store_remotes(store).into_iter().find(|(n, _)| n == target) {
        return Some(url);
    }
    target.contains("://").then(|| target.to_string())
}

/// After a FAILED push to `target`, print a one-line client-facing hint IFF the target is a
/// positively-identified agit-hub that REJECTED the credential (an authentication failure). git's own
/// error already reached the terminal (inherited stdio); this only appends guidance git cannot give —
/// the hub's server-side `remote:` message points at `agit-hub token add`, which a client user cannot
/// run. The web `/tokens` page is where they actually mint a write token. Non-http targets (local paths,
/// ssh) are a fast network-free no-op; the probe never fires except on a real hub-push failure.
fn hub_auth_hint(store: &std::path::Path, target: &str) {
    let Some(url) = resolve_remote_url(store, target) else {
        return;
    };
    hub_auth_hint_url(&url);
}

/// [`hub_auth_hint`] for a call site that already holds the target URL (the fan-out).
fn hub_auth_hint_url(url: &str) {
    if agit::hubapi::is_hub_store_url(url) && !agit::hubapi::hub_credential_accepted(url) {
        if let Some(base) = agit::hubapi::hub_web_base(url) {
            eprintln!(
                "hint: this hub needs a WRITE TOKEN in the password field -- create one at {base}/tokens (username can be anything)"
            );
        }
    }
}

/// Fan a bare `agit a push` out to every git remote the store has: an explicit `<name> <branch>`
/// refspec per remote (a freshly `remote add`-ed hub has no upstream, so a bare `git push <name>` under
/// `push.default=simple` would error). Each remote's TARGET is announced (credentials redacted) before
/// the push and its success/failure printed after; a per-remote failure is non-fatal. The returned exit
/// is the PRIMARY remote's push code — the command's identity anchor.
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
    for (name, url) in &remotes {
        // Announce the target BEFORE the push so the multi-remote fan-out is never a surprise; the URL
        // is redacted so a credential in it never reaches the terminal or CI logs.
        println!("→ pushing to {name} ({})", agit::hubapi::redact_url(url));
        let mut pa: Vec<&str> = vec!["push", "--no-verify"];
        pa.extend(flags.iter().map(String::as_str));
        pa.push(name.as_str());
        pa.push(&branch);
        let code = scope::git_in_inherit(&a.store, &pa);
        if code == 0 {
            println!("  pushed {name}");
        } else {
            // A non-owner's push to a personal hub returns non-zero from the ACL 403 — shown, not fatal.
            eprintln!("  push to {name} rejected (exit {code}); reported, not fatal");
            hub_auth_hint_url(url);
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
    format!("  note: remote {name} is not a transport agit records in {bf}; left unchanged.")
}

/// Print what `sync_remotes_to_binding` recorded/skipped. Recorded lines only appear on a change (an
/// idempotent re-push is silent); a skipped remote is a standing problem worth repeating every push.
fn print_sync_summary(agent_name: &str, s: &agit::agent::RemoteSyncSummary) {
    let bf = agit::agent::BINDING_FILE;
    if s.changed {
        for r in &s.recorded {
            let tag = if r.primary { "  (primary; clone/pull anchor)" } else { "" };
            println!("  bound  {agent_name} → {}{tag}", r.locator);
        }
        if !s.recorded.is_empty() {
            println!("         (commit {bf}: teammates clone the agent from here)");
        }
        if s.recorded.iter().any(|r| r.stripped) {
            eprintln!("  note: credentials stripped from a recorded remote; {bf} is committed.");
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
            "{prompt}\n  refusing without confirmation (re-run with --yes to proceed non-interactively)"
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
            "agit-crypt is not enabled on this machine: run `agit a encrypt` first, then --rotate"
        );
    }
    let a = agent::resolve(None)?;
    let store = a.store.clone();

    eprintln!("agit encrypt --rotate: mint a new key and re-encrypt this store's working tree.");
    eprintln!(
        "  Retired keys are kept LOCALLY so history / not-yet-renormalized blobs still decrypt; only\n\
         \x20     new writes use the new key. Afterwards you must `agit a push` and re-share the key\n\
         \x20     (`agit a encrypt --export <file>`) so teammates can decrypt going-forward blobs."
    );
    if !confirm_or_yes("Rotate the crypt key and re-encrypt this store now?", yes)? {
        eprintln!("aborted: key not rotated");
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

/// `agit a encrypt --purge-history [--yes]` — back-compat alias for the first-class `agit a purge-history`
/// command. The guard-railed rewrite (per-session precondition, clean-tree gate, filter-repo/filter-branch
/// backend, exact force-push instructions) lives in `commands::purge_history_cmd`; this just forwards.
fn agent_encrypt_purge_history(args: &[String]) -> anyhow::Result<i32> {
    commands::purge_history_cmd(args)
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
        eprintln!("diverged: local and remote both added sessions (a text merge corrupts transcripts)");
        // Suggest the diverged UPSTREAM ref (e.g. `origin/main`), never the agent's own name: `agit a
        // merge <self-name>` resolves back to this agent and dead-ends with "no local session to
        // represent it". `@{u}` is the generic fallback when the tracking ref can't be named — still a
        // ref, still not the agent name.
        let target = commands::upstream_ref(&a.store).unwrap_or_else(|| "@{u}".to_string());
        eprintln!("  agit a merge {target}   reconcile by dialogue");
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
            "--help" | "-h" => {
                println!("{MERGE_USAGE}");
                return Ok(0);
            }
            // An unknown dash flag is REJECTED, not swallowed — `--dry-runn` (a typo of `--dry-run`)
            // must not slip through and run a REAL merge where the user asked for a preview.
            other if other.starts_with('-') => {
                eprintln!("unknown option '{other}'");
                eprintln!("{MERGE_USAGE}");
                return Ok(2);
            }
            other => {
                if reference.is_none() {
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
            eprintln!("{MERGE_USAGE}");
            Ok(2)
        }
    }
}

/// SECURITY: a dash flag agit does not recognize is REJECTED, never turned into a scan target. The old
/// behaviour turned `agit a scan --no-verify` (and `--bogus`) into a scan of a nonexistent "file", which
/// found nothing and printed "no secrets found" at exit 0 — HIDING real secrets in the tree. Non-dash
/// args stay as explicit file targets.
fn parse_scan(args: &[String]) -> Result<(bool, Vec<PathBuf>), ParseCtl> {
    let mut staged = false;
    let mut paths = Vec::new();
    for a in args {
        match a.as_str() {
            "--staged" => staged = true,
            "--help" | "-h" => return Err(ParseCtl::Help),
            other if other.starts_with('-') => return Err(ParseCtl::Unknown(other.to_string())),
            other => paths.push(PathBuf::from(other)),
        }
    }
    Ok((staged, paths))
}

const USAGE: &str = "\
agit: version an agent's raw session so teams can collaborate on Agent Context

Works with claude-code and codex. Commands that read sessions use the one you name with --from, else
the only one present, else they ask.

  agit init [--agent N]    Prepare this repo: clone or select its agent, install the secret hooks (--agent names one)
  agit a snap [--watch]    Mirror this project's session dump + harness (MCP/skills/config, secrets redacted) into the Agent Store; captures every runtime with sessions here unless --from names one (--watch = auto-snap; --no-harness = sessions only)
  agit a push / pull       Push your sessions to, and pull the team's back from, the shared store (the Agent Store is just a git repo)
  agit start               Launch a session HERE already carrying this agent's latest context, from whatever repo it was last in (--agent <name> picks the agent for this invocation only; --as <rt> switches runtime)
  agit a merge <target>    Merge this agent's memory with <target>'s by dialogue (alias: sync); <target> is an agent name or a ref (never a code branch). Same agent → the histories merge too; a different agent → dialogue only, both stay intact (--agent X / --ref X disambiguate)
  agit a clone <url>       Clone an agent published on a hub (its memory, by identity); --init mints a fresh agent into an EMPTY store and pushes it
  agit a scan [--staged]   Scan session dumps for secrets
  agit workspace [log]     Show the Agent↔Environment pairing
  agit workspace restore [N]  Roll both repos back together to a pairing's joint state
  agit watch [--daemon]    Hands-off: watch claude-code, codex, auto-snap + auto-convert both ways; --daemon runs it in the background forever (--stop / --status to manage)
  agit graph               Show the Workspace-State timeline + relation edges
  agit harness [apply]     Show, or apply, the captured harness (MCP/skills/config); apply asks first (--force to skip)
  agit adapter             List runtime adapters
  agit shadow [install]    Route `git` through `agit` in your shell so every git command versions agent context (uninstall / status to manage)
  agit convert [<session|agent>] --to <rt>  Convert a session into one another runtime can resume (default: the active agent's latest; an agent name picks that agent's latest; --write to persist; --watch auto-converts both ways in the background)
  agit resume [<session|agent>]  Load a session into a runtime and continue (default: the active agent's latest; an agent name picks that agent's latest; --as <rt> to switch runtime; --env <path> to run this agent against a different repo; --relocate if it's the same project moved; --exec to launch). `agit start` launches a fresh runtime here carrying that latest context instead
  agit provenance verify [<session|agent>]  Check a captured session's signature against its recorded key (default: the active agent's latest; an agent name verifies ALL its sessions; unsigned → unverified, never blocks); `agit provenance key` shows this machine's public key

  agit <git-args>          Run git transparently on the code repository (Environment). `agit clone <target>` also adopts an agit agent store (a positively-identified hub URL, or a known agent name) as an agent; --git forces the raw git clone
  agit clone --git <url>   Force a raw git clone (never adopt it as an agent)
  agit agent <git-args>    Run isomorphic git on the Agent Store; `agit a` is the alias (agit a log · agit a add -A · agit a commit · agit a push)

  agit a status            Overview of this repo: its agents, which is active, each one's sessions + last activity + live-watcher, and the active store's unpushed/ahead-behind
  agit a log / diff        The SESSION view of the store (a timeline of sessions; the prompts + edits added between two refs); pass --raw for the byte-level git log/diff

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
