//! agit —— A Git-compatible CLI for versioning Agent Context + Environment.
//!
//! 架构（docs/architecture.md）：被版本化的对象是两个 git 库 + 一个配对。
//!
//!   agit <git-args>     = agit -e <git-args>  → 透明 git 作用在 Environment（代码仓库）
//!   agit -a <git-args>                        → 同构操作作用在 Agent Store
//!
//! scope 开关只认紧跟 agit 的第一个 token。子命令之后的 -a 原样交给 git：
//!   agit -a commit   → Agent scope
//!   agit commit -a   → Environment scope，-a 是 git commit 的参数

// 核心逻辑在 lib(crate `agit`),与 agit-hub 共享,避免两个 bin 各写一份解析而漂移。
use agit::scope::Scope;
use agit::{commands, init, passthrough, session};
use std::path::PathBuf;
use std::process::exit;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    exit(dispatch(argv));
}

/// 解析出 (scope, 剩余参数)。scope 只能是紧跟 agit 的第一个 token。
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
        // ── 顶层原生命令（与 scope 无关）──
        "init" => init::run(),
        "clone" => match args.first() {
            Some(url) => commands::clone_agent(url),
            None => {
                eprintln!("用法: agit clone <hub-url>/<name>.git   （把团队 Agent Store 拉到本地并装好驱动）");
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

        // ── 需要 agit 加值的原生动词 ──
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

        // ── session dump 管理（新模型的核心）──
        "sync" => {
            // 位置参数当 runtime 简写：`agit -a sync codex` == `agit -a sync --from codex`。
            // 之前把它解析出来又丢掉，用户以为在同步某个 runtime，实际静默跑了默认的 claude-code。
            let (flag_rt, pos) = parse_runtime_arg(args, "--from");
            let rt = match pos {
                Some(p) => p.to_string_lossy().into_owned(),
                None => flag_rt,
            };
            session::sync(&rt)
        }
        "reconcile" => {
            let (rt, reference, flags) = parse_reconcile(args);
            if reference.is_none() && !flags.abort && !flags.cont {
                eprintln!(
                    "用法: agit -a reconcile <ref> [--dry-run]   （把对面 <ref> 的 session 合进来）\n\
                     \x20     agit -a reconcile --continue        （定稿手动解决的冲突后继续）\n\
                     \x20     agit -a reconcile --abort           （放弃进行中的合并）"
                );
                Ok(2)
            } else {
                session::reconcile(reference.as_deref(), &rt, flags)
            }
        }
        "adapter" => commands::adapter_list(),

        // ── 跨 runtime 转会话(resume 到另一个 CLI)──
        "convert" => match parse_convert(args) {
            Some((src, from, to, cwd, write)) => {
                commands::convert_cmd(&src, from, &to, cwd, write)
            }
            None => {
                eprintln!("用法: agit convert <src-session> --to claude-code|codex [--from RT] [--cwd 路径] [--write]");
                Ok(2)
            }
        },

        // ── 其余一切：透明透传到对应库的 git ──
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


/// 解析 reconcile 参数：`--from <rt>` + `--dry-run/--abort/--continue` + 一个可选位置 <ref>。
fn parse_reconcile(args: &[String]) -> (String, Option<String>, session::ReconcileFlags) {
    let mut rt = "claude-code".to_string();
    let mut reference = None;
    let mut flags = session::ReconcileFlags::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" if i + 1 < args.len() => {
                rt = args[i + 1].clone();
                i += 2;
            }
            "--dry-run" => {
                flags.dry_run = true;
                i += 1;
            }
            "--abort" => {
                flags.abort = true;
                i += 1;
            }
            "--continue" => {
                flags.cont = true;
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
    (rt, reference, flags)
}

/// 解析 `--from/--to <runtime>` + 一个可选位置参数。runtime 默认 claude-code。
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

/// convert 参数:位置参数 src + --to(必需)+ --from/--cwd/--write。
/// 返回 None 表示缺 src 或 --to。
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
agit —— 版本化 agent 的原始 session,让团队协作 Agent Context

  agit init                在代码仓库旁建 Agent Store
  agit -a sync             把本项目的 Claude session dump 镜像进 Agent Store
  agit -a push / pull      和团队同步 session（Agent Store 就是 git 仓库）
  agit -a reconcile <ref>  让 agent 读对面 <ref> 的 session、合成统一上下文,真冲突才问你
  agit clone <url>         一条命令拉取团队 Agent Store
  agit -a scan [--staged]  扫 session dump 里的密钥
  agit workspace [log]     看 Agent↔Environment 的配对
  agit workspace restore [N]  把两个库一起退回某条配对的联合状态
  agit adapter             列出 runtime adapter
  agit convert <src> --to <rt>  把一份 session 转成另一个 runtime 能 resume 的会话(--write 落盘)

  agit <git-args>          在代码仓库（Environment）上透明跑 git
  agit -a <git-args>       在 Agent Store 上跑同构 git

  scope 只认紧跟 agit 的第一个 token：agit -a commit（agent）vs agit commit -a（代码,-a 是 git 参数）";
