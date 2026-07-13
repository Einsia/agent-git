//! agit —— A Git-compatible CLI for versioning Agent Context + Environment.
//!
//! PRD 架构（docs/architecture-v2.md）：被版本化的对象是两个 git 库 + 一个配对。
//!
//!   agit <git-args>     = agit -e <git-args>  → 透明 git 作用在 Environment（代码仓库）
//!   agit -a <git-args>                        → 同构操作作用在 Agent Store
//!
//! scope 开关只认紧跟 agit 的第一个 token。子命令之后的 -a 原样交给 git：
//!   agit -a commit   → Agent scope
//!   agit commit -a   → Environment scope，-a 是 git commit 的参数

#![allow(dead_code)] // v1 领域模块（claim/evidence/merge）正在向 Agent Store 移植

mod adapter;
mod claim;
mod commands;
mod environment;
mod evidence;
mod extract;
mod gitx;
mod init;
mod merge;
mod passthrough;
mod scan;
mod scope;
mod workspace;

use scope::Scope;
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
        "merge-file" => run_merge_driver(args),
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
            _ => commands::workspace_show(),
        },

        // ── Adapter：runtime ↔ AgentState ──
        "adapter" => commands::adapter_list(),
        "import" => {
            let (rt, pos) = parse_runtime_arg(args, "--from");
            commands::import_cmd(&rt, pos)
        }
        "export" => {
            let (rt, pos) = parse_runtime_arg(args, "--to");
            commands::export_cmd(&rt, pos)
        }

        // ── v1 领域动词：迁移中 ──
        "verify" | "why" | "new" | "resolve" => {
            eprintln!(
                "agit {cmd}：正在从 v1 的单库模型移植到 Agent Store（见 docs/architecture-v2.md）。\n\
                 当前可用：agit init / agit [-a] <任意 git 命令> / agit -a scan / agit workspace。"
            );
            Ok(2)
        }

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

/// git 的 merge driver 入口：agit merge-file %O %A %B %P（由 Agent Store 的 config 调用）。
fn run_merge_driver(args: &[String]) -> anyhow::Result<i32> {
    if args.len() < 4 {
        anyhow::bail!("merge-file 需要 %O %A %B %P 四个参数（由 git 调用，不是给人用的）");
    }
    merge::driver(
        std::path::Path::new(&args[0]),
        std::path::Path::new(&args[1]),
        std::path::Path::new(&args[2]),
        &args[3],
    )
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
agit —— 版本化 Agent Context + Environment 的 Git 兼容 CLI

  agit init                初始化 Agent Store 与配对基建
  agit <git-args>          在代码仓库（Environment）上跑 git —— 透明
  agit -a <git-args>       在 Agent Store 上跑同构操作（status/add/commit/log/diff/merge/…）
  agit -a scan [--staged]  扫 AgentState 里的密钥
  agit workspace [log]     看 Agent↔Environment 的配对（WorkspaceRevision）

  scope 只认紧跟 agit 的第一个 token：
    agit -a commit         Agent scope
    agit commit -a         Environment scope（-a 是 git 的参数）";
