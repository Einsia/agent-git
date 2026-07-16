//! agit — A Git-compatible CLI for versioning Agent Context + Environment.
//!
//! Architecture (docs/architecture.md): the versioned objects are two git repos + one pairing.
//!
//!   agit <git-args>     = agit -e <git-args>  → transparent git acting on the Environment (code repository)
//!   agit -a <git-args>                        → the isomorphic operation acting on the Agent Store
//!
//! The scope switch only recognizes the first token immediately after agit. Any -a after the subcommand is passed to git as-is:
//!   agit -a commit   → Agent scope
//!   agit commit -a   → Environment scope, -a is an argument to git commit

// Core logic lives in the lib (crate `agit`), shared with agit-hub, so the two bins don't each write their own parsing and drift apart.
use agit::scope::{self, Scope};
use agit::{commands, harness, init, passthrough, session, sync};
use std::path::PathBuf;
use std::process::exit;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    exit(dispatch(argv));
}

/// Parse out (scope, remaining args). The scope can only be the first token immediately after agit.
fn split_scope(argv: &[String]) -> (Scope, &[String]) {
    match argv.first().map(|s| s.as_str()) {
        Some("-a") => (Scope::Agent, &argv[1..]),
        Some("-e") => (Scope::Environment, &argv[1..]),
        _ => (Scope::Environment, argv),
    }
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
        "init" => init::run(),
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
            let mut rt = "claude-code".to_string();
            let mut watch = false;
            let mut interval = 5u64;
            let mut harness = true;
            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "--from" if i + 1 < args.len() => {
                        rt = args[i + 1].clone();
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
                        // a bare positional is shorthand for the runtime: `agit -a snap codex`
                        if !other.starts_with('-') {
                            rt = other.to_string();
                        }
                        i += 1;
                    }
                }
            }
            if watch {
                session::snap_watch(&rt, interval)
            } else {
                session::sync(&rt, harness)
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
            let mut rt = "claude-code".to_string();
            let mut force = false;
            let mut sub = "show".to_string();
            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "--from" if i + 1 < args.len() => {
                        rt = args[i + 1].clone();
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
            let roots = scope::root_for(Scope::Agent)
                .and_then(|agent| scope::env_root().map(|env| (agent, env)));
            match roots {
                Ok((agent, env)) if sub == "apply" => harness::apply(&agent, &env, &rt, force),
                Ok((agent, _)) => harness::show(&agent, &rt),
                Err(e) => Err(e),
            }
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
            Some((src, as_rt, cwd, env, exec)) => commands::resume_cmd(&src, as_rt, cwd, env, exec),
            None => {
                eprintln!("usage: agit resume <src-session> [--as claude-code|codex] [--env PATH] [--cwd PATH] [--exec]");
                Ok(2)
            }
        },

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
type ResumeArgs = (PathBuf, Option<String>, Option<String>, Option<String>, bool);
fn parse_resume(args: &[String]) -> Option<ResumeArgs> {
    let mut src = None;
    let mut as_rt = None;
    let mut cwd = None;
    let mut env = None;
    let mut exec = false;
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
    Some((src?, as_rt, cwd, env, exec))
}

/// Parse and run the dialogue merge (`agit -a merge <ref>`, alias `sync`): positional <ref> plus
/// --from <rt> / --both / --quick.
fn merge_cmd(args: &[String]) -> anyhow::Result<i32> {
    let mut rt = "claude-code".to_string();
    let mut reference = None;
    let mut both = false;
    let mut quick = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" if i + 1 < args.len() => {
                rt = args[i + 1].clone();
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
        Some(r) => sync::run(&r, &rt, both, quick),
        None => {
            eprintln!("usage: agit -a merge <ref> [--both] [--quick]   (reconcile this branch's agent with <ref>'s by dialogue; --quick skips the context handoff)");
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

  agit init                Create an Agent Store next to the code repository
  agit -a snap [--watch]   Mirror this project's session dump + harness (MCP/skills/config, secrets redacted) into the Agent Store (--watch = auto-snap; --no-harness = sessions only)
  agit -a push / pull      Sync sessions with the team (the Agent Store is just a git repo)
  agit -a merge <ref>      Merge this branch's agent with <ref>'s by dialogue (alias: sync); only real conflicts prompt you
  agit clone <url>         Clone the team Agent Store in one command
  agit -a scan [--staged]  Scan session dumps for secrets
  agit workspace [log]     Show the Agent↔Environment pairing
  agit workspace restore [N]  Roll both repos back together to a pairing's joint state
  agit watch [--daemon]    Hands-off: watch both runtimes, auto-snap + auto-convert both ways; --daemon runs it in the background forever (--stop / --status to manage)
  agit graph               Show the Workspace-State timeline + relation edges
  agit harness [apply]     Show, or apply, the captured harness (MCP/skills/config); apply asks first (--force to skip)
  agit adapter             List runtime adapters
  agit convert <src> --to <rt>  Convert a session into one another runtime can resume (--write to persist; --watch auto-converts both ways in the background)
  agit resume <src>        Load a session into a runtime and continue (--as <rt> to switch runtime, --env <path> to run it against a different checkout, --exec to launch)

  agit <git-args>          Run git transparently on the code repository (Environment)
  agit -a <git-args>       Run isomorphic git on the Agent Store

  scope only recognizes the first token immediately after agit: agit -a commit (agent) vs agit commit -a (code, -a is a git argument)";
