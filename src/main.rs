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
use agit::{commands, harness, init, passthrough, session, sync, ui};
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
/// `push`/`pull`/`fetch`/`merge`. The verbs here are the ones with no git primitive: `list`, `info`,
/// `rename`, `rebind` (a repair — "accept a changed identity"). `info` not `show` so `git show` on the
/// store still works.
const AGENT_MGMT_VERBS: &[&str] = &["list", "switch", "info", "rename", "rebind"];

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
            eprintln!("  available: list · switch · info · rename · rebind");
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
        "workspace" => match args.first().map(|s| s.as_str()) {
            Some("log") => commands::workspace_log(),
            Some("restore") => commands::workspace_restore(args.get(1).map(|s| s.as_str())),
            _ => commands::workspace_show(),
        },

        // ── snap: mirror the runtime's session dump into the Agent Store (formerly named sync).
        //    --watch runs it continuously (fully automatic snap). ──
        "snap" => {
            // No default runtime: `--from` (or a bare positional) names one, otherwise snap captures
            // every runtime that has sessions here. See session::snap.
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
        "adapter" => commands::adapter_list(),
        "graph" => commands::workspace_graph(),

        // ── shadow: route `git` through `agit` in your interactive shell (cross-platform). ──
        "shadow" => agit::shadow::run(args),

        // ── watch: fully hands-off — watch both runtimes' dumps, auto-snap + auto-convert both ways.
        //    --daemon runs it forever in the background; --stop / --status manage it. ──
        "watch" => {
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
            match action {
                1 => session::watch_daemon(interval, convert, harness),
                2 => session::watch_stop(),
                3 => session::watch_status(),
                _ => session::watch(interval, convert, harness),
            }
        }

        // ── harness: show / apply the captured MCP + skills + config (part of Agent State) ──
        "harness" => {
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
fn agent_push(args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;
    let a = agent::resolve(None)?;

    // The push, verbatim. Inherited stdio: a push is where credential helpers prompt, and capturing
    // would both swallow git's errors and block the prompt.
    let mut push_args: Vec<&str> = vec!["push"];
    push_args.extend(args.iter().map(String::as_str));
    let code = scope::git_in_inherit(&a.store, &push_args);
    if code != 0 {
        return Ok(code);
    }

    // Reconcile the binding with origin only inside an environment — a bare store has nowhere to write
    // one, and that is not an error.
    if let Ok(env) = scope::env_root() {
        match agent::sync_origin_to_binding(&a.aid, &env)? {
            agent::BindingSync::Recorded { locator, stripped } => {
                println!(
                    "  bound  {} → {locator}   (commit {}: teammates clone the agent from here)",
                    a.name,
                    agent::BINDING_FILE
                );
                if stripped {
                    eprintln!(
                        "  note: credentials stripped from the recorded remote — {} is committed.",
                        agent::BINDING_FILE
                    );
                    eprintln!("        The store keeps the full URL locally; your teammates' git supplies their own.");
                }
            }
            agent::BindingSync::NotShareable(url) => eprintln!(
                "  note: origin ({url}) is not a transport agit will record in {} — binding left unchanged.",
                agent::BINDING_FILE
            ),
            agent::BindingSync::NoOrigin | agent::BindingSync::Unchanged(_) => {}
        }
    }
    Ok(0)
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
    if diverged(&a.store) {
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
    report_incoming(&a.store);
    Ok(0)
}

/// Best-effort summary of the sessions on the tracked upstream that HEAD does not yet have. Silent if
/// there is no upstream to compare against (a fetch with an explicit refspec and no tracking branch).
fn report_incoming(store: &std::path::Path) {
    let (code, out) = scope::git_in_status(
        store,
        &["diff", "--name-only", "--diff-filter=A", "HEAD..@{u}", "--", "sessions"],
    );
    if code != 0 {
        return;
    }
    let new: Vec<&str> = out.lines().filter(|l| l.ends_with(".jsonl")).collect();
    if new.is_empty() {
        println!("  up to date — no new sessions on the remote.");
        return;
    }
    // Break the count down per runtime by asking the registry which runtimes exist, not by naming any
    // — a new adapter shows up here for free.
    let breakdown: Vec<String> = agit::adapter::names()
        .iter()
        .filter_map(|rt| {
            let n = new.iter().filter(|f| f.contains(&format!("/{rt}/"))).count();
            (n > 0).then(|| format!("{rt}: {n}"))
        })
        .collect();
    let suffix = if breakdown.is_empty() { String::new() } else { format!(" ({})", breakdown.join(", ")) };
    println!("  {} new session(s) on the remote{suffix}.", new.len());
    println!("  integrate with: agit a pull");
}

/// True when HEAD and its upstream each hold commits the other does not — the one case a fast-forward
/// cannot cover and a textual merge must not. An absent or unresolvable upstream is not "diverged".
fn diverged(store: &std::path::Path) -> bool {
    let (code, out) = scope::git_in_status(store, &["rev-list", "--left-right", "--count", "@{u}...HEAD"]);
    if code != 0 {
        return false;
    }
    let mut counts = out.split_whitespace().filter_map(|s| s.parse::<u64>().ok());
    let behind = counts.next().unwrap_or(0);
    let ahead = counts.next().unwrap_or(0);
    behind > 0 && ahead > 0
}

fn merge_cmd(args: &[String]) -> anyhow::Result<i32> {
    let mut rt: Option<String> = None;
    let mut reference = None;
    let mut both = false;
    let mut quick = false;
    let mut splice = false;
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
            sync::run(&r, &rt, both, quick, splice, prefer)
        }
        None => {
            eprintln!("usage: agit a merge <target> [--from <runtime>] [--both] [--quick] [--splice]   (reconcile this agent's memory with <target>'s by dialogue; <target> is an agent name or a ref — --agent X / --ref X disambiguate; --quick shortens the dialogue; --splice skips the model and just combines both sessions' context)");
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

  agit <git-args>          Run git transparently on the code repository (Environment)
  agit agent <git-args>    Run isomorphic git on the Agent Store — `agit a` is the alias (agit a log · agit a add -A · agit a commit · agit a push)

  agit agent <verb>        Agent management, a closed set: init, clone, switch, list, info, rename, rebind, merge (push/pull/fetch too).
                           Anything else after `a` is git, so `agit a add -A` is git-add and `agit a show` is git-show.

  `a` is a subcommand, so it cannot be transposed: agit a commit (agent store) vs agit commit -a (code repo, -a is git's stage-all).
  The old `agit -a <args>` flag still works as a deprecated alias.";
