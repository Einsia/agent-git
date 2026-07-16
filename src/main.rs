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
use agit::scope::Scope;
use agit::{commands, init, passthrough, session, sync};
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

        // ── snap: mirror the runtime's session dump into the Agent Store (formerly named sync) ──
        "snap" => {
            // A positional argument is shorthand for the runtime: `agit -a snap codex` == `agit -a snap --from codex`.
            let (flag_rt, pos) = parse_runtime_arg(args, "--from");
            let rt = match pos {
                Some(p) => p.to_string_lossy().into_owned(),
                None => flag_rt,
            };
            session::sync(&rt)
        }

        // ── sync: merge two diverged agent branches by dialogue (both sides truly resume, read-only reconciliation, only real conflicts prompt you) ──
        "sync" => {
            let mut rt = "claude-code".to_string();
            let mut reference = None;
            let mut both = false;
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
                    other => {
                        if reference.is_none() && !other.starts_with('-') {
                            reference = Some(other.to_string());
                        }
                        i += 1;
                    }
                }
            }
            match reference {
                Some(r) => sync::run(&r, &rt, both),
                None => {
                    eprintln!("usage: agit -a sync <ref> [--both]   (reconcile this branch's agent with <ref>'s agent by dialogue)");
                    Ok(2)
                }
            }
        }
        "adapter" => commands::adapter_list(),

        // ── Convert a session across runtimes (resume it in another CLI) ──
        "convert" => match parse_convert(args) {
            Some((src, from, to, cwd, write)) => {
                commands::convert_cmd(&src, from, &to, cwd, write)
            }
            None => {
                eprintln!("usage: agit convert <src-session> --to claude-code|codex [--from RT] [--cwd PATH] [--write]");
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


/// Parse `--from/--to <runtime>` + one optional positional argument. The runtime defaults to claude-code.
fn parse_runtime_arg(args: &[String], flag: &str) -> (String, Option<PathBuf>) {
    let mut runtime = "claude-code".to_string();
    let mut positional = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag && i + 1 < args.len() {
            runtime = args[i + 1].clone();
            i += 2;
        } else {
            positional = Some(PathBuf::from(&args[i]));
            i += 1;
        }
    }
    (runtime, positional)
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
  agit -a snap             Mirror this project's Claude session dump into the Agent Store (formerly named sync)
  agit -a push / pull      Sync sessions with the team (the Agent Store is just a git repo)
  agit -a sync <ref>       Merge this branch's agent with <ref>'s agent by dialogue; only real conflicts prompt you
  agit clone <url>         Clone the team Agent Store in one command
  agit -a scan [--staged]  Scan session dumps for secrets
  agit workspace [log]     Show the Agent↔Environment pairing
  agit workspace restore [N]  Roll both repos back together to a pairing's joint state
  agit adapter             List runtime adapters
  agit convert <src> --to <rt>  Convert a session into one another runtime can resume (--write to persist)

  agit <git-args>          Run git transparently on the code repository (Environment)
  agit -a <git-args>       Run isomorphic git on the Agent Store

  scope only recognizes the first token immediately after agit: agit -a commit (agent) vs agit commit -a (code, -a is a git argument)";
