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
//! as a silent deprecated alias while the docs, demo scripts and install-shadow.sh still say it.

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
/// The names deliberately avoid shadowing git's namespace: `track` not `add` (`git add` is far too
/// common to lose), `info` not `show` (`git show` is a real verb). `merge` is a management verb too, but
/// is dispatched separately above because it alone is implemented — and it shadows `git merge` on purpose.
const AGENT_MGMT_VERBS: &[&str] = &[
    "list", "use", "new", "track", "info", "rename", "publish", "rebind", "import",
];

/// Recognizing the management verbs is what keeps them away from git — `agit a info` must not become
/// `git info`.
fn agent_mgmt(verb: &str, args: &[String]) -> anyhow::Result<i32> {
    use agit::agent;

    // A verb whose argument is mandatory must not read a missing one as "the empty selector": every
    // resolution rung treats blank as absent, so `agit a use` would silently act on the default.
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
                println!("no agents yet — agit a new <name> mints one.");
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
                    // "watching", not "running": a pidfile proves agit has a watcher on this store, and
                    // nothing here can see whether a human has a live session open.
                    let status = match session::watcher_pid(&a.store) {
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
            println!("remote {}", a.remote.as_deref().unwrap_or("— (local only; agit a publish adds one)"));
            Ok(0)
        }
        "use" => {
            let a = agent::use_agent(&need("name|aid")?)?;
            println!("● {} ({})  — this worktree's agent", a.name, a.aid);
            Ok(0)
        }
        "new" => {
            let a = agent::new_agent(&need("name")?)?;
            println!("minted {} ({})", a.name, a.aid);
            println!("  store {}", a.store.display());
            // Minting works outside a repo on purpose (identity precedes any URL), so binding is
            // best-effort: without it `agit a new` would leave `agit start` still saying "no agent
            // selected", which is the dead end this verb exists to end.
            bind_and_activate(&a)?;
            Ok(0)
        }
        "track" => {
            let a = agent::track(&need("name|url")?, !flag("--no-use"))?;
            println!("tracking {} ({})", a.name, a.aid);
            println!("  store  {}", a.store.display());
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
        // The one-shot adoption of a store minted before identity existed. Optional name: a store that
        // already knows what it is keeps its own label.
        "import" => {
            let a = agent::import(args.first().map(|s| s.trim()).filter(|s| !s.is_empty()))?;
            println!("imported {} ({})", a.name, a.aid);
            println!("  store  {}", a.store.display());
            println!("  bound  {}   (commit it: your team gets this agent on clone)", agent::BINDING_FILE);
            Ok(0)
        }
        // Still design-only. Named individually so the message cannot outlive the gap.
        v => {
            eprintln!("agit agent {v}: not implemented yet.");
            eprintln!("  available: list · new · use · track · info · rename · import");
            Ok(2)
        }
    }
}

/// Bind a freshly-minted agent to this repo and make it the active one.
///
/// Outside a git repo this is a no-op rather than an error: `agit a new` is explicitly allowed there.
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
        // ── Top-level native commands (independent of scope) ──
        "init" => {
            let agent = args
                .iter()
                .position(|a| a == "--agent")
                .and_then(|i| args.get(i + 1))
                .cloned();
            init::run_named(agent)
        }
        "clone" => match args.first() {
            Some(url) => commands::clone_agent(url),
            None => {
                eprintln!("usage: agit clone <hub-url>/<name>.git   (clone the team Agent Store locally and set up the driver)");
                Ok(2)
            }
        },
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
        //    git untouched. `sync` remains as a back-compat alias. ──
        "merge" if scope == Scope::Agent => merge_cmd(args),
        "sync" => merge_cmd(args),
        "adapter" => commands::adapter_list(),
        "graph" => commands::workspace_graph(),

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
            let mut force = false;
            let mut sub = "show".to_string();
            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "--from" if i + 1 < args.len() => {
                        rt = Some(args[i + 1].clone());
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
            harness_cmd(&sub, rt.as_deref(), force)
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
fn harness_cmd(sub: &str, rt: Option<&str>, force: bool) -> anyhow::Result<i32> {
    let agent = scope::root_for(Scope::Agent)?;
    let env = scope::env_root()?;
    let captured = harness::captured_runtimes(&agent);
    if sub == "apply" {
        let rt = session::resolve_runtime(rt, &captured, "apply")?;
        return harness::apply(&agent, &env, &rt, force);
    }
    match rt {
        Some(r) => harness::show(&agent, &session::resolve_runtime(Some(r), &captured, "show")?),
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
                code = harness::show(&agent, rt)?;
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
fn merge_cmd(args: &[String]) -> anyhow::Result<i32> {
    let mut rt: Option<String> = None;
    let mut reference = None;
    let mut both = false;
    let mut quick = false;
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
            sync::run(&r, &rt, both, quick, prefer)
        }
        None => {
            eprintln!("usage: agit a merge <target> [--from <runtime>] [--both] [--quick]   (reconcile this agent's memory with <target>'s by dialogue; <target> is an agent name or a ref — --agent X / --ref X disambiguate; --quick skips the context handoff)");
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

Runtimes are peers and there is no default: claude-code, codex. Commands that read sessions use the one
you name with --from, else the only one present, else they ask.

  agit init [--store P]    Create an Agent Store next to the code repository (--store detaches it to P, so several repos share one agent's history)
  agit a snap [--watch]    Mirror this project's session dump + harness (MCP/skills/config, secrets redacted) into the Agent Store; captures every runtime with sessions here unless --from names one (--watch = auto-snap; --no-harness = sessions only)
  agit a push / pull       Sync sessions with the team (the Agent Store is just a git repo)
  agit start               Launch a session HERE already carrying this agent's latest context, from whatever repo it was last in (--agent <name> picks the agent for this invocation only; --as <rt> switches runtime)
  agit a merge <target>    Merge this agent's memory with <target>'s by dialogue (alias: sync); <target> is an agent name or a ref — never a code branch. Same agent → the histories merge too; a different agent → dialogue only, both stay intact (--agent X / --ref X disambiguate)
  agit clone <url>         Clone the team Agent Store in one command
  agit a scan [--staged]   Scan session dumps for secrets
  agit workspace [log]     Show the Agent↔Environment pairing
  agit workspace restore [N]  Roll both repos back together to a pairing's joint state
  agit watch [--daemon]    Hands-off: watch claude-code, codex, auto-snap + auto-convert both ways; --daemon runs it in the background forever (--stop / --status to manage)
  agit graph               Show the Workspace-State timeline + relation edges
  agit harness [apply]     Show, or apply, the captured harness (MCP/skills/config); apply asks first (--force to skip)
  agit adapter             List runtime adapters
  agit convert <src> --to <rt>  Convert a session into one another runtime can resume (--write to persist; --watch auto-converts both ways in the background)
  agit resume <src>        Load a session into a runtime and continue (--as <rt> to switch runtime; --env <path> to run this agent against a different repo; --relocate if it's the same project moved; --exec to launch)

  agit <git-args>          Run git transparently on the code repository (Environment)
  agit agent <git-args>    Run isomorphic git on the Agent Store — `agit a` is the alias (agit a log · agit a add -A · agit a commit · agit a push)

  agit agent <verb>        Agent management, a closed set: list, use, new, track, info, rename, publish, rebind, import, merge.
                           Anything else after `a` is git, so `agit a add -A` is git-add and `agit a show` is git-show.

  `a` is a subcommand, so it cannot be transposed: agit a commit (agent store) vs agit commit -a (code repo, -a is git's stage-all).
  The old `agit -a <args>` flag still works as a deprecated alias.";
