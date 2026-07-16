//! agit-hub —— AgentGitHub：托管团队的 Agent Store，人可读（React SPA）、agent 可拉（JSON API）。
//!
//! 形态：一个自包含的 HTTP 服务，托管一堆 Agent Store（bare git 仓库）。
//!   - Registry：扫描 hub root 下的 <name>.git，元数据在 agents.json（owner/可见性/成员）
//!   - Sync：git smart-http，`agit -a push/pull http://host:port/<name>.git` 直接可用
//!   - 身份：`aid`（`agt_<uuid>`）由客户端铸造、提交在 store 里的 agent.toml —— Hub 只读不写。
//!           改名不改身份；Hub 上的 name 只是个可变标签。
//!   - 鉴权：**每个 agent 各自的 ACL**（owner + 成员 read/write/admin + public/private），
//!           人走 cookie 会话，git/脚本走 token（可绑定单个 agent、可过期、可撤销）。
//!           所有入口 —— 含 git smart-http —— 都过同一个判定：`agit::hub::acl::decide`。
//!   - 前端：hub-ui（Vite + React + Tailwind + shadcn）编译进二进制，SPA 消费下面的 JSON API。
//!
//! 这一层只做 HTTP 的解析与搬运；"谁能做什么"全在 `agit::hub` 里（纯函数 + 单测）。
//!
//! 前端资源在编译期由 include_str! 嵌入（hub-ui/dist）。改前端后 `cd hub-ui && npm run build` 再 cargo build。
//!
//! 安全默认值（都是刻意的，改之前先想清楚）：
//!   - 只听 127.0.0.1。要对外必须显式 `--host`，且没有 TLS 时还要 `--insecure` 才肯起。
//!   - 新建 agent 一律 private。公开是显式动作。
//!   - 密码用 argon2id（带盐），不是 sha256。token 只存 sha256 摘要。
//!   - 代理后的真实 IP 只在显式 `--trusted-proxy` 之后才认 X-Forwarded-For。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use agit::hub::acl::{self, Action, AgentAcl, Caller, Decision, Deny, Role, Scope, Visibility};
use agit::hub::identity::Identity;
use agit::hub::net::{self, valid_agent_name};
use agit::hub::session::Sessions;
use agit::hub::store::{AgentMeta, Member, Store, TokenRec, User};
use agit::hub::{audit, auth, identity, kdf, session as websession, store};

const PER_PAGE: usize = 20;
/// 带查询时最多扫多少条 session（挡住无界 git show）。超出会在响应里标记，不静默截断。
const SEARCH_SCAN_CAP: usize = 400;

// ── 编译期嵌入的前端（hub-ui/dist）──
const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/index.html"));
const APP_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/app.js"));
const APP_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/app.css"));

fn main() {
    std::process::exit(run());
}

/// 返回进程退出码 —— 错误路径必须非零，脚本/CI 才能感知失败（别一律 exit 0）。
fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("serve");
    let root = flag(&args, "--root").map(PathBuf::from).unwrap_or_else(default_root);

    match cmd {
        "serve" => serve_cmd(&root, &args),
        "add" => add_cmd(&root, &args),
        "list" => list_cmd(&root),
        "token" => token_cmd(&root, &args),
        "user" => user_cmd(&root, &args),
        "-h" | "--help" => {
            print_help();
            0
        }
        other => {
            eprintln!("未知子命令: {other}");
            print_help();
            2
        }
    }
}

fn print_help() {
    println!(
        "agit-hub —— AgentGitHub (Registry + Sync)\n\n\
         agit-hub serve [--host 127.0.0.1] [--port 8177] [--root ~/.agit-hub]\n\
                        [--tls] [--insecure] [--trusted-proxy IP,IP]      启动 Hub\n\
         agit-hub user add <name> [--admin]                   建用户（交互式问密码）\n\
         agit-hub user list                                   列出用户\n\
         agit-hub add <name> [--owner <user>] [--public]      新建 Agent Store（默认 private）\n\
         agit-hub list                                        列出已托管的 agent\n\
         agit-hub token add <name> [--user <owner>] [--agent <name>]\n\
                            [--read|--write] [--ttl-days N]   发一个访问 token\n\
         agit-hub token list                                  列出 token（只显示摘要信息）\n\
         agit-hub token rm <id>                               吊销一个 token\n\n\
         第一步：agit-hub user add <你> --admin\n\
         托管的仓库是 bare git。发布： agit -a push http://HOST:PORT/<name>.git（带写 token）\n\n\
         默认只听 127.0.0.1。对外服务要 --host 0.0.0.0，且没有 TLS 时必须再加 --insecure。"
    );
}

fn default_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".agit-hub")
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// 第一个不以 `--` 开头的位置参数（跳过前 skip 个 token）。
fn positional(args: &[String], skip: usize) -> Option<&String> {
    args.iter().skip(skip).find(|s| !s.starts_with("--"))
}

// ─────────────────────────── CLI: user ───────────────────────────

fn user_cmd(root: &Path, args: &[String]) -> i32 {
    let store = Store::new(root);
    match args.get(1).map(|s| s.as_str()) {
        Some("add") => {
            let Some(name) = positional(args, 2) else {
                eprintln!("用法: agit-hub user add <name> [--admin]");
                return 2;
            };
            let username = store::normalize_username(name);
            if !store::valid_username(&username) {
                eprintln!("非法用户名（2-32 位小写 [a-z0-9._-]，不许前导点）: {name}");
                return 2;
            }
            if store.user(&username).is_some() {
                eprintln!("用户已存在: {username}");
                return 1;
            }
            let is_admin = has_flag(args, "--admin");
            // 密码只从 tty/stdin 读，**绝不从 argv 拿** —— argv 会进 ps、进 shell history。
            let password = match read_new_password() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let salt = match kdf::gen_salt() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("拿不到系统熵，拒绝建用户：{e}");
                    return 1;
                }
            };
            let kdf_id = kdf::current_kdf_id();
            let Some(pw_hash) = kdf::hash_password(&password, &salt, &kdf_id) else {
                eprintln!("口令派生失败（kdf={kdf_id}）");
                return 1;
            };
            let user = User { username: username.clone(), pw_hash, salt, kdf: kdf_id, is_admin, created: store::now_iso() };
            if let Err(e) = store.add_user(user) {
                eprintln!("写 users.json 失败: {e}");
                return 1;
            }
            audit::append(root, "cli", audit::USER_ADD, None, &format!("{username} admin={is_admin}"));
            println!("已建用户 {username}{}", if is_admin { "（站点管理员）" } else { "" });
            println!("  密码用 argon2id 派生后存进 users.json（0600），明文不落盘。");
            0
        }
        Some("list") => {
            let users = store.users();
            if users.is_empty() {
                println!("还没有用户。`agit-hub user add <你> --admin` 建第一个。");
            }
            for u in users {
                println!("{:<20} {:<8} {}", u.username, if u.is_admin { "admin" } else { "user" }, u.created);
            }
            0
        }
        _ => {
            eprintln!("用法: agit-hub user add <name> [--admin] | agit-hub user list");
            2
        }
    }
}

/// 读新密码：tty 上关回显并要求输两遍；stdin 是管道时读一行（脚本化装机）。
fn read_new_password() -> Result<String, String> {
    let (pw, tty) = read_password("为该用户设置密码: ")?;
    if pw.chars().count() < 8 {
        return Err("密码太短（至少 8 个字符）。".into());
    }
    if tty {
        let (again, _) = read_password("再输一次: ")?;
        if again != pw {
            return Err("两次输入不一致。".into());
        }
    }
    Ok(pw)
}

/// 关回显读一行。返回 (密码, 是不是 tty)。
///
/// 不引 rpassword：一个 `stty -echo` 就够，而且 Hub 刻意保持零额外依赖的 CLI 面。
/// stty 失败 = stdin 不是 tty（管道），那本来也不会回显。
fn read_password(prompt: &str) -> Result<(String, bool), String> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let tty = stty(&["-echo"]);
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if tty {
        stty(&["echo"]);
        eprintln!(); // 回显关着，用户敲的回车没显示出来，这里补一个换行
    }
    match read {
        Ok(0) => Err("读不到密码（stdin 已结束）。".into()),
        Ok(_) => Ok((line.trim_end_matches(['\n', '\r']).to_string(), tty)),
        Err(e) => Err(format!("读密码失败: {e}")),
    }
}

fn stty(args: &[&str]) -> bool {
    Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 没写 `--user` / `--owner` 时：只有一个用户就默认是他；否则要求写明白（别猜）。
fn resolve_user(store: &Store, explicit: Option<String>, flag_name: &str) -> Result<User, String> {
    if let Some(name) = explicit {
        let n = store::normalize_username(&name);
        return store.user(&n).ok_or(format!("没有这个用户: {n}（`agit-hub user list` 看看）"));
    }
    let users = store.users();
    match users.len() {
        0 => Err("还没有用户。先 `agit-hub user add <你> --admin`。".into()),
        1 => Ok(users.into_iter().next().unwrap()),
        _ => Err(format!("有多个用户，请用 {flag_name} <user> 指明。")),
    }
}

// ─────────────────────────── CLI: agent ───────────────────────────

fn add_cmd(root: &Path, args: &[String]) -> i32 {
    let Some(name) = positional(args, 1) else {
        eprintln!("用法: agit-hub add <name> [--owner <user>] [--public]");
        return 2;
    };
    let store = Store::new(root);
    let owner = match resolve_user(&store, flag(args, "--owner"), "--owner") {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    // 默认 private —— 转录是敏感数据，公开必须是显式动作。
    let visibility = if has_flag(args, "--public") { Visibility::Public } else { Visibility::Private };
    match create_agent(&store, name, &owner.username, visibility) {
        Ok(created) => {
            audit::append(root, "cli", audit::AGENT_CREATE, Some(name), &format!("owner={} visibility={}", owner.username, visibility.as_str()));
            if created {
                println!("已托管 {name}  →  {}", repo_path(root, name).display());
            } else {
                println!("已认领现有仓库 {name}  →  {}", repo_path(root, name).display());
            }
            println!("  owner:      {}", owner.username);
            println!("  可见性:     {}", visibility.as_str());
            if visibility == Visibility::Public {
                println!("  ⚠ public = 任何能连上这个端口的人都能读它的全部转录。");
            }
            println!("发布（需写 token，见 `agit-hub token add`）：");
            println!("  agit -a remote add origin http://localhost:8177/{name}.git");
            println!("  agit -a push -u origin main");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// 建库 + 记元数据。返回 true = 新建；false = 认领了一个已存在但没登记的老仓库。
///
/// "认领"这条路是给迁移用的：root 下可能已经有 `<name>.git`（老 Hub 建的，没有 owner）。
/// 那种库在新模型里是**无主私有**，除了站点管理员谁也看不见 —— 认领是把它接回来的正路。
fn create_agent(store: &Store, name: &str, owner: &str, visibility: Visibility) -> Result<bool, String> {
    if !valid_agent_name(name) {
        return Err(format!("非法名字（只允许 [A-Za-z0-9._-]，禁止 .. 与前导点）: {name}"));
    }
    if store.agent(name).is_some() {
        return Err(format!("已存在: {name}"));
    }
    let dir = store.root().join(format!("{name}.git"));
    let existed = dir.exists();
    if !existed {
        std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败: {e}"))?;
        let ok = Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_dir_all(&dir);
            return Err("git init --bare 失败".into());
        }
        let _ = Command::new("git").arg("-C").arg(&dir).args(["config", "http.receivepack", "true"]).status();
    }
    store
        .update_agents(|a| a.push(AgentMeta::new(name, Some(owner), visibility)))
        .map_err(|e| format!("写 agents.json 失败: {e}"))?;
    Ok(!existed)
}

fn list_cmd(root: &Path) -> i32 {
    let store = Store::new(root);
    let names = list_agents(root);
    if names.is_empty() {
        println!("还没有 agent。`agit-hub add <name>` 建一个。");
    }
    for n in names {
        let m = store.agent_or_unowned(&n);
        let owner = m.owner.clone().unwrap_or_else(|| "—（无主）".into());
        println!("{:<24} {:<8} owner={}", n, m.visibility, owner);
    }
    0
}

fn list_agents(root: &Path) -> Vec<String> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(n) = name.strip_suffix(".git") {
                // 目录名是外面来的：不合法的一律不认（别让 `..git` 之类进到路由里）。
                if valid_agent_name(n) {
                    out.push(n.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

fn repo_path(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.git"))
}

// ─────────────────────────── CLI: token ───────────────────────────

fn token_cmd(root: &Path, args: &[String]) -> i32 {
    let store = Store::new(root);
    match args.get(1).map(|s| s.as_str()) {
        Some("add") => {
            let Some(name) = positional(args, 2) else {
                eprintln!("用法: agit-hub token add <name> [--user <owner>] [--agent <name>] [--read|--write] [--ttl-days N]");
                return 2;
            };
            let owner = match resolve_user(&store, flag(args, "--user"), "--user") {
                Ok(u) => u,
                Err(e) => {
                    eprintln!("{e}");
                    return 2;
                }
            };
            // 默认只读：发凭据时的失败方向只能是"权限更小"。老 CLI 默认是写，那是反的。
            let scope = if has_flag(args, "--write") { Scope::Write } else { Scope::Read };
            let agent = flag(args, "--agent");
            if let Some(a) = &agent {
                if !valid_agent_name(a) || store.agent(a).is_none() {
                    eprintln!("没有这个 agent: {a}");
                    return 2;
                }
            }
            let ttl_days: Option<i64> = match flag(args, "--ttl-days") {
                Some(v) => match v.parse::<i64>() {
                    Ok(n) if n > 0 => Some(n),
                    _ => {
                        eprintln!("--ttl-days 要一个正整数");
                        return 2;
                    }
                },
                None => None,
            };
            match issue_token(&store, name, &owner.username, agent.as_deref(), scope, ttl_days) {
                Ok(secret) => {
                    audit::append(root, "cli", audit::TOKEN_CREATE, agent.as_deref(), &format!("name={name} owner={} scope={}", owner.username, scope.as_str()));
                    println!("已发 token 给 {}（{}）", owner.username, scope.as_str());
                    println!("  token: {secret}");
                    println!("  这串只显示这一次（服务器只存它的 sha256 摘要）。");
                    match &agent {
                        Some(a) => println!("  只对 agent `{a}` 有效。"),
                        None => println!("  对该用户能访问的所有 agent 有效 —— 用 --agent <name> 收窄。"),
                    }
                    match ttl_days {
                        Some(d) => println!("  {d} 天后过期。"),
                        None => println!("  ⚠ 永不过期 —— 用 --ttl-days N 给它一个期限。"),
                    }
                    println!("  git 提示输入用户名/密码时，密码填这个 token（用户名随意）。");
                    println!("  权限是 token 与属主权限的**交集**：属主没有的，token 也给不了。");
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        Some("list") => {
            let toks = store.tokens();
            if toks.is_empty() {
                println!("还没有 token。`agit-hub token add <name> --write` 发一个。");
            }
            let legacy = toks.iter().filter(|t| t.owner.is_none()).count();
            for t in &toks {
                let owner = t.owner.clone().unwrap_or_else(|| "—".into());
                let agent = t.agent.clone().unwrap_or_else(|| "*".into());
                let exp = t.expires.clone().unwrap_or_else(|| "永不".into());
                let used = t.last_used.clone().unwrap_or_else(|| "从未".into());
                let state = if t.owner.is_none() {
                    " [失效：无属主]"
                } else if t.expired() {
                    " [已过期]"
                } else {
                    ""
                };
                println!("{:<18} {:<16} owner={:<12} agent={:<12} {:<6} 过期={:<22} 最近用={}{}", t.id, t.name, owner, agent, t.scope, exp, used, state);
            }
            if legacy > 0 {
                println!();
                println!("⚠ 有 {legacy} 个**老格式 token 没有属主**，已全部失效。");
                println!("  它们是旧模型的遗产：一个 token = 整个 host 的通行证，没法映射到「谁能看谁的 agent」。");
                println!("  用 `agit-hub token add <name> --user <owner> [--agent <a>]` 重发，再 `token rm <id>` 删掉旧的。");
            }
            0
        }
        Some("rm") => {
            let Some(id) = positional(args, 2) else {
                eprintln!("用法: agit-hub token rm <id>（id 见 `agit-hub token list`）");
                return 2;
            };
            match store.update_tokens(|toks| {
                let before = toks.len();
                toks.retain(|t| &t.id != id);
                before != toks.len()
            }) {
                Ok(true) => {
                    audit::append(root, "cli", audit::TOKEN_REVOKE, None, id);
                    println!("已吊销 {id}");
                    0
                }
                Ok(false) => {
                    eprintln!("没有这个 token: {id}");
                    1
                }
                Err(e) => {
                    eprintln!("写 auth.json 失败: {e}");
                    1
                }
            }
        }
        _ => {
            eprintln!("用法: agit-hub token add <name> [--user <owner>] [--agent <a>] [--read|--write] [--ttl-days N]");
            eprintln!("      agit-hub token list | agit-hub token rm <id>");
            2
        }
    }
}

/// 发 token：生成 32 字节 CSPRNG 明文，只存它的 sha256 摘要。返回明文（只此一次）。
fn issue_token(store: &Store, name: &str, owner: &str, agent: Option<&str>, scope: Scope, ttl_days: Option<i64>) -> Result<String, String> {
    // CSPRNG 拿不到就**报错退出**，绝不退回可预测的时间值来发凭据。
    let secret = kdf::gen_secret().map_err(|e| format!("拒绝发 token：{e}"))?;
    let id = store::new_token_id().map_err(|e| format!("拒绝发 token：{e}"))?;
    let expires = ttl_days.map(|d| (chrono::Utc::now() + chrono::Duration::days(d)).format("%Y-%m-%dT%H:%M:%SZ").to_string());
    let rec = TokenRec {
        id,
        name: name.to_string(),
        owner: Some(owner.to_string()),
        agent: agent.map(|s| s.to_string()),
        scope: scope.as_str().to_string(),
        hash: agit::convo::sha256_hex(&secret),
        created: store::now_iso(),
        expires,
        last_used: None,
    };
    store.update_tokens(|t| t.push(rec)).map_err(|e| format!("写 auth.json 失败: {e}"))?;
    Ok(secret)
}

// ─────────────────────── git 读取（bare 仓库）───────────────────────

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn has_head(repo: &Path) -> bool {
    git(repo, &["rev-parse", "HEAD"]).is_some()
}

fn recent_log(repo: &Path, n: usize) -> Vec<(String, String)> {
    git(repo, &["log", &format!("-{n}"), "--format=%h%x09%s"])
        .map(|s| {
            s.lines()
                .filter_map(|l| l.split_once('\t').map(|(a, b)| (a.to_string(), b.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// 最近一次提交的相对时间 + 主题，首页用它（便宜，单次 git log）。
fn last_activity(repo: &Path) -> (String, String) {
    git(repo, &["log", "-1", "--format=%cr\x1f%s"])
        .and_then(|s| s.trim().split_once('\x1f').map(|(a, b)| (a.to_string(), b.to_string())))
        .unwrap_or_default()
}

/// store 里的 agent 身份。**权威来源是 store 自己**（agent.toml 提交在历史里），Hub 不铸造 aid。
/// 返回 (aid, 来源)。来源取值：
///   "agent.toml"   —— 读到了
///   "none"         —— 库还空着，或者没有 agent.toml（新建但没人推过的库就是这样）
///   "unidentified" —— 有 agent.toml，但里面没有 agt_ 身份（老 store 的占位 id）
fn agent_aid(repo: &Path) -> (Option<String>, &'static str) {
    let Some(text) = git(repo, &["show", "HEAD:agent.toml"]) else {
        return (None, "none");
    };
    match identity::parse_agent_toml(&text) {
        Identity::Aid(a) => (Some(a), "agent.toml"),
        Identity::Unidentified => (None, "unidentified"),
    }
}

/// 一条 session 在 store 里的位置。
///
/// 两种布局都要认（设计文档里新的带 environment，老库没有）：
///   sessions/<env>/<runtime>/<id>.jsonl   —— 新
///   sessions/<runtime>/<id>.jsonl         —— 老（env = None）
struct SessionRef {
    env: Option<String>,
    runtime: String,
    id: String,
    path: String,
}

fn session_refs(repo: &Path) -> Vec<SessionRef> {
    let mut out = vec![];
    let Some(list) = git(repo, &["ls-tree", "-r", "--name-only", "HEAD", "sessions/"]) else {
        return out;
    };
    for path in list.lines() {
        let path = path.trim();
        if !path.ends_with(".jsonl") {
            continue;
        }
        let segs: Vec<&str> = path.split('/').collect();
        let (env, runtime, file) = match segs.len() {
            3 => (None, segs[1], segs[2]),
            4 => (Some(segs[1].to_string()), segs[2], segs[3]),
            _ => continue,
        };
        out.push(SessionRef {
            env,
            runtime: runtime.to_string(),
            id: file.trim_end_matches(".jsonl").to_string(),
            path: path.to_string(),
        });
    }
    out
}

fn load_session(repo: &Path, path: &str, at: Option<&str>) -> Option<String> {
    git(repo, &["show", &format!("{}:{path}", at.unwrap_or("HEAD"))])
}

/// 每个 environment 的 session 数与最近活动。environment = session 来自哪个代码仓库。
fn environments(repo: &Path, refs: &[SessionRef]) -> Vec<serde_json::Value> {
    // 保序（按第一次出现），顺带记下每组的目录，供 git log 限定范围。
    let mut order: Vec<Option<String>> = vec![];
    let mut counts: HashMap<Option<String>, usize> = HashMap::new();
    let mut dirs: HashMap<Option<String>, Vec<String>> = HashMap::new();
    for r in refs {
        if !counts.contains_key(&r.env) {
            order.push(r.env.clone());
        }
        *counts.entry(r.env.clone()).or_insert(0) += 1;
        // 新布局按 env 目录限定；老布局（env=None）没有 env 目录，只能按 runtime 目录限定。
        let dir = match &r.env {
            Some(e) => format!("sessions/{e}"),
            None => format!("sessions/{}", r.runtime),
        };
        let d = dirs.entry(r.env.clone()).or_default();
        if !d.contains(&dir) {
            d.push(dir);
        }
    }
    order
        .into_iter()
        .map(|env| {
            let last = dirs
                .get(&env)
                .and_then(|ds| {
                    let mut args: Vec<String> = vec!["log".into(), "-1".into(), "--format=%cr".into(), "--".into()];
                    // `:(literal)` 关掉 pathspec 的通配 —— 目录名来自仓库内容，可能带 `*`/`?`。
                    args.extend(ds.iter().map(|d| format!(":(literal){d}")));
                    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                    git(repo, &argv)
                })
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            serde_json::json!({ "env": env, "sessions": counts.get(&env).copied().unwrap_or(0), "last": last })
        })
        .collect()
}

fn branches(repo: &Path) -> Vec<serde_json::Value> {
    git(repo, &["for-each-ref", "--format=%(refname:short)\x1f%(objectname:short)\x1f%(committerdate:relative)", "refs/heads/"])
        .map(|s| {
            s.lines()
                .filter_map(|l| {
                    let mut it = l.split('\x1f');
                    let name = it.next()?;
                    Some(serde_json::json!({
                        "name": name,
                        "commit": it.next().unwrap_or(""),
                        "when": it.next().unwrap_or(""),
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 仓库占用的字节数。git count-objects 报的是 KiB。
fn size_bytes(repo: &Path) -> u64 {
    let Some(out) = git(repo, &["count-objects", "-v"]) else {
        return 0;
    };
    let mut kib = 0u64;
    for line in out.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if matches!(k.trim(), "size" | "size-pack") {
                kib += v.trim().parse::<u64>().unwrap_or(0);
            }
        }
    }
    kib * 1024
}

/// store 里出现过的 runtime。字母序 —— claude-code 与 codex 是**对等**的，谁也不是默认。
fn runtimes(refs: &[SessionRef]) -> Vec<String> {
    let mut v: Vec<String> = refs.iter().map(|r| r.runtime.clone()).collect();
    v.sort();
    v.dedup();
    v
}

// ─────────── session 解析（跨 runtime，走 agit 库） ───────────

struct SessionDigest {
    id: String,
    branch: String,
    cwd: String,
    prompts: Vec<String>,
    texts: Vec<String>,
    tools: usize,
    files: Vec<String>,
}

fn digest(runtime: &str, id: &str, jsonl: &str) -> SessionDigest {
    let ir = match agit::convo::normalize_runtime(runtime) {
        "codex" => agit::adapter::codex::parse_rollout(jsonl, id),
        _ => agit::adapter::claude_code::parse_jsonl(jsonl, id),
    };
    let mut files = Vec::new();
    for w in &ir.writes {
        let f = w.rsplit('/').next().unwrap_or(w).to_string();
        if !files.contains(&f) {
            files.push(f);
        }
    }
    SessionDigest {
        id: ir.session_id,
        branch: ir.git_branch.unwrap_or_default(),
        cwd: ir.cwd.unwrap_or_default(),
        prompts: ir.prompts,
        texts: ir.agent_texts,
        tools: ir.tool_uses,
        files,
    }
}

struct Provenance {
    author: String,
    when: String,
    commit: String,
    model: String,
}

fn provenance(repo: &Path, path: &str, jsonl: &str) -> Provenance {
    let raw = git(repo, &["log", "-1", "--format=%an\x1f%cr\x1f%H", "--", path]).unwrap_or_default();
    let mut it = raw.trim().split('\x1f');
    Provenance {
        author: it.next().unwrap_or("").to_string(),
        when: it.next().unwrap_or("").to_string(),
        commit: it.next().unwrap_or("").to_string(),
        model: extract_model(jsonl).unwrap_or_default(),
    }
}

fn extract_model(jsonl: &str) -> Option<String> {
    for line in jsonl.lines().take(400) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let candidates = [
            v.get("message").and_then(|m| m.get("model")),
            v.get("payload").and_then(|p| p.get("model")),
            v.get("model"),
        ];
        for c in candidates.into_iter().flatten() {
            if let Some(m) = c.as_str() {
                if !m.is_empty() {
                    return Some(m.to_string());
                }
            }
        }
    }
    None
}

fn session_revisions(repo: &Path, path: &str) -> Vec<(String, String, String)> {
    git(repo, &["log", "--format=%H\x1f%cr\x1f%s", "--", path])
        .map(|s| {
            s.lines()
                .filter_map(|l| {
                    let mut it = l.split('\x1f');
                    Some((it.next()?.to_string(), it.next().unwrap_or("").to_string(), it.next().unwrap_or("").to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// session 的事件脊线：有序 kinds → 'p'/'a'/'t'/'e' 串（SPA 渲染成波形）。跨 runtime 走 ConversationIR。
fn spine_string(runtime: &str, jsonl: &str) -> String {
    use agit::convo::EventKind;
    let Ok(ir) = agit::convo::read_conversation(runtime, jsonl) else {
        return String::new();
    };
    let mut out = String::new();
    for e in &ir.events {
        for k in &e.kinds {
            out.push(match k {
                EventKind::UserPrompt(_) => 'p',
                EventKind::AssistantText(_) => 'a',
                EventKind::ToolCall { .. } | EventKind::ToolResult { .. } => 't',
                EventKind::FileEdit { .. } => 'e',
            });
            if out.len() >= 600 {
                return out;
            }
        }
    }
    out
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

fn clip(s: &str, n: usize) -> String {
    s.trim().chars().take(n).collect()
}

// ─────────────────────────── 配置 / 上下文 ───────────────────────────

struct Cfg {
    host: IpAddr,
    port: u16,
    /// 前面有 TLS 终结（反代）。影响：允许非环回 bind；cookie 打 Secure。
    tls: bool,
    /// 明知没有 TLS 也要对外听。
    insecure: bool,
    /// 可信反代的 IP —— 只有来自它们的连接，X-Forwarded-For 才算数。
    trusted_proxies: Vec<IpAddr>,
}

/// 一个请求要用到的全部共享状态。
struct Ctx {
    store: Store,
    cfg: Cfg,
    sessions: Sessions,
    limiter: Arc<ConnLimiter>,
    /// 登录并发闸：argon2 是**故意**又慢又吃内存的（这正是它挡爆破的方式）。
    /// 不限并发的话，几十个并发登录 = 几十份 19MiB + 满核 CPU，等于送人一个放大器。
    login_gate: Arc<Semaphore>,
}

impl Ctx {
    fn root(&self) -> &Path {
        self.store.root()
    }
}

// ─────────────────────────── HTTP 服务 ───────────────────────────

fn serve_cmd(root: &Path, args: &[String]) -> i32 {
    let host: IpAddr = match flag(args, "--host") {
        Some(h) => match h.parse() {
            Ok(ip) => ip,
            Err(_) => {
                eprintln!("--host 要一个 IP 地址（如 127.0.0.1 / 0.0.0.0 / ::1），拿到的是: {h}");
                return 2;
            }
        },
        // 默认只听环回：Hub 装着团队的全部转录，"装上就暴露在办公网"不能是默认。
        None => IpAddr::from([127, 0, 0, 1]),
    };
    let port: u16 = flag(args, "--port").and_then(|p| p.parse().ok()).unwrap_or(8177);
    let tls = has_flag(args, "--tls");
    let insecure = has_flag(args, "--insecure");
    let trusted_proxies = match flag(args, "--trusted-proxy") {
        Some(s) => match net::parse_trusted_proxies(&s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("--trusted-proxy: {e}");
                return 2;
            }
        },
        None => vec![],
    };
    if let Err(e) = bind_guard(host, tls, insecure) {
        eprintln!("{e}");
        return 2;
    }
    if has_flag(args, "--private") {
        println!("提示：--private 已经不需要了 —— 可见性现在是**每个 agent 各自**的属性，且新建默认 private。");
        println!("      要公开某个 agent：`agit-hub add <name> --public`，或在 UI 里改。");
    }
    serve(root, Cfg { host, port, tls, insecure, trusted_proxies })
}

/// bind 前的安全闸（纯函数，好测）。
///
/// 非环回地址 = 网络上其他人能连。没有 TLS 时，密码与 token 会以**明文**过网线，
/// 任何在路径上的人都能抄走。所以：要么 --tls（前面有终结），要么明确 --insecure。
fn bind_guard(host: IpAddr, tls: bool, insecure: bool) -> Result<(), String> {
    if host.is_loopback() || tls || insecure {
        return Ok(());
    }
    Err(format!(
        "拒绝在 {host} 上明文监听。\n\
         这个地址网络上的其他人能连到 —— 而没有 TLS 时，登录密码和 token 会明文过网线，\n\
         路径上任何一跳都能抄走它们，然后就能读/推你团队的全部转录。\n\n\
         选一个：\n\
           - 只给本机用（默认）：去掉 --host\n\
           - 前面挂 TLS 反代（nginx/caddy 终结 HTTPS）：加 --tls，并用 --trusted-proxy <代理IP>\n\
           - 就是要明文（可信内网/临时演示）：加 --insecure，你已经知道代价了"
    ))
}

fn serve(root: &Path, cfg: Cfg) -> i32 {
    if let Err(e) = store::ensure_root(root) {
        eprintln!("建 root 失败 {}: {e}", root.display());
        return 1;
    }
    let addr = std::net::SocketAddr::new(cfg.host, cfg.port);
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("绑定 {addr} 失败: {e}");
            return 1;
        }
    };

    let store = Store::new(root);
    let agents = list_agents(root);
    let unowned = agents.iter().filter(|n| store.agent_or_unowned(n).owner.is_none()).count();
    let users = store.users();
    let legacy_tokens = store.tokens().iter().filter(|t| t.owner.is_none()).count();

    println!("AgentGitHub 运行中");
    println!("  监听:  {addr}{}", if cfg.tls { "（前面有 TLS 终结）" } else { "" });
    println!("  前端:  {}://{}/", if cfg.tls { "https" } else { "http" }, display_host(&cfg));
    println!("  root:  {}", root.display());
    println!("  托管:  {} 个 agent（{} 个 public）", agents.len(), agents.iter().filter(|n| store.agent_or_unowned(n).visibility == "public").count());
    println!("  用户:  {} 个（{} 个管理员）", users.len(), users.iter().filter(|u| u.is_admin).count());
    if !cfg.trusted_proxies.is_empty() {
        println!("  代理:  信任 {:?} 的 X-Forwarded-For", cfg.trusted_proxies);
    }
    if cfg.insecure && !cfg.host.is_loopback() && !cfg.tls {
        println!("  ⚠ --insecure：明文对外监听 —— 密码与 token 在网线上是裸的。");
    }
    if users.is_empty() {
        println!("  ⚠ 一个用户都没有 —— 没人能登录。先 `agit-hub user add <你> --admin`。");
    }
    if unowned > 0 {
        println!("  ⚠ {unowned} 个 agent 没有 owner（老仓库）：它们是私有的，只有站点管理员看得见。");
        println!("    认领：`agit-hub add <name> --owner <user>`");
    }
    if legacy_tokens > 0 {
        println!("  ⚠ {legacy_tokens} 个老 token 没有属主，**已失效**（旧的「一个 token = 全站」模型没法映射到新 ACL）。");
        println!("    重发：`agit-hub token add <name> --user <owner> [--agent <a>]`；`agit-hub token list` 看详情。");
    }

    let ctx = Arc::new(Ctx {
        store,
        cfg,
        sessions: Sessions::new(),
        limiter: Arc::new(ConnLimiter::default()),
        login_gate: Arc::new(Semaphore::new(LOGIN_CONC)),
    });

    // 并发上限：每连接一线程，但用信号量封顶 —— 否则 N 个慢连接 = N 个线程/内存，unbounded。
    let sem = Arc::new(Semaphore::new(MAX_CONN));
    for stream in listener.incoming().flatten() {
        let peer = stream.peer_addr().map(|a| a.ip()).ok();
        // 先按 IP 准入：满了直接丢连接（drop 关闭），不占全局槽、不 spawn。
        //
        // 可信代理例外：它后面站着所有真实用户，按它的 IP 数会让大家互相挤下线（这正是
        // 需求 10 说的病）。那种连接的 per-IP 准入推迟到读完 XFF 之后（见 handle）。
        let proxied = peer.map(|ip| ctx.cfg.trusted_proxies.contains(&ip)).unwrap_or(false);
        let ipguard = match (peer, proxied) {
            (Some(ip), false) => match ctx.limiter.try_acquire(ip) {
                Some(g) => Some(g),
                None => continue,
            },
            _ => None,
        };
        let permit = Permit::acquire(sem.clone()); // 到顶就在这里挡住 accept，多余连接排在内核 backlog。
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // 都持有到线程结束；**即使 handle panic 也会在 drop 时归还**（不泄漏槽位/IP 计数）。
            let _permit = permit;
            let _ipguard = ipguard;
            let _ = handle(stream, &ctx, peer, proxied);
        });
    }
    0
}

fn display_host(cfg: &Cfg) -> String {
    let h = if cfg.host.is_unspecified() { "localhost".to_string() } else { cfg.host.to_string() };
    match (cfg.tls, cfg.port) {
        (true, 443) | (false, 80) => h,
        _ => format!("{h}:{}", cfg.port),
    }
}

/// 每 IP 的在途连接计数。
#[derive(Default)]
struct ConnLimiter {
    map: Mutex<HashMap<IpAddr, usize>>,
}

impl ConnLimiter {
    fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<IpGuard> {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.entry(ip).or_insert(0);
        if *n >= PER_IP_MAX {
            if *n == 0 {
                m.remove(&ip);
            }
            return None;
        }
        *n += 1;
        Some(IpGuard { limiter: self.clone(), ip })
    }
}

/// 一个占用的 per-IP 名额；drop 时减一（panic 安全）。
struct IpGuard {
    limiter: Arc<ConnLimiter>,
    ip: IpAddr,
}

impl Drop for IpGuard {
    fn drop(&mut self) {
        let mut m = self.limiter.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

/// 计数信号量（std 无内置）：封顶并发处理线程数。
struct Semaphore {
    permits: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Self {
        Semaphore { permits: Mutex::new(n), cv: Condvar::new() }
    }
}

/// 一个占用的槽位；drop 时归还（panic 安全 —— handle 崩了也不会漏掉一个 permit）。
struct Permit(Arc<Semaphore>);

impl Permit {
    fn acquire(sem: Arc<Semaphore>) -> Permit {
        let mut p = sem.permits.lock().unwrap_or_else(|e| e.into_inner());
        while *p == 0 {
            p = sem.cv.wait(p).unwrap_or_else(|e| e.into_inner());
        }
        *p -= 1;
        drop(p);
        Permit(sem)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        *self.0.permits.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.0.cv.notify_one();
    }
}

struct Req {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    content_length: usize,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn host(&self) -> String {
        self.header("host").unwrap_or("localhost:8177").to_string()
    }
    fn query(&self) -> &str {
        self.target.split_once('?').map(|(_, q)| q).unwrap_or("")
    }
    fn sid(&self) -> Option<String> {
        self.header("cookie").and_then(websession::parse_cookie)
    }
}

const MAX_BODY: usize = 512 * 1024 * 1024;
/// JSON API 的 body 上限。API 只收小对象；给它 512MB 的额度毫无道理。
const API_MAX_BODY: usize = 64 * 1024;
const MAX_LINE: u64 = 16 * 1024;
const MAX_HEADERS_BYTES: usize = 64 * 1024;
/// 并发处理线程上限（挡住 unbounded thread-per-connection）。
const MAX_CONN: usize = 64;
/// 单个 IP 的在途连接上限（挡住单一来源 slowloris 占满全池）。给全池的一半，留槽位给别人。
const PER_IP_MAX: usize = 32;
/// 同时最多几个 argon2 在跑。argon2 每次要 19MiB + 满核；不封顶 = 一个放大器。
const LOGIN_CONC: usize = 4;
/// 从 accept 到读完请求头的整体墙钟上限（挡住 1 字节/<60s 的 slowloris 滴灌 —— 那能重置 per-read 超时）。
/// 读头阶段的 socket 读超时也设成它 —— 于是阻塞在**请求行**那一次读上的连接也在此掐断（deadline 只在头循环顶检查，盖不住请求行读）。
const HEADER_DEADLINE_SECS: u64 = 20;
/// body（git push 的 pack）读超时更长：pack 持续流入，只在真卡住时触发。
const BODY_TIMEOUT_SECS: u64 = 60;
/// 读完 API body 的整体墙钟上限。64KB 的 JSON 没有任何理由要慢慢来（挡 body 上的滴灌）。
const API_BODY_DEADLINE_SECS: u64 = 15;

/// 只读请求行 + 头部，**不碰 body**。body 留在 reader 里，等鉴权通过、确实需要时再流式取。
fn read_head(reader: &mut BufReader<TcpStream>) -> Option<Req> {
    let deadline = Instant::now() + Duration::from_secs(HEADER_DEADLINE_SECS);
    let mut line = String::new();
    reader.by_ref().take(MAX_LINE).read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut headers = vec![];
    let mut content_length = 0usize;
    let mut headers_bytes = 0usize;
    loop {
        if Instant::now() > deadline {
            return None; // 整体读头超时 → 掐掉（配合并发上限，slowloris 滴灌撑不住一个线程槽）
        }
        let mut h = String::new();
        reader.by_ref().take(MAX_LINE).read_line(&mut h).ok()?;
        headers_bytes += h.len();
        if headers_bytes > MAX_HEADERS_BYTES {
            return None;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    Some(Req { method, target, headers, content_length })
}

/// 读 API 的小 body。既封顶**大小**，也封顶**总时长**。
///
/// 为什么要自己数时间：socket 上的读超时是**每次读**的，1 字节/19 秒的滴灌能把它一直重置掉 ——
/// 那正是 slowloris 的玩法，读头那里也是靠一个整体 deadline 才挡住的。git 的 body 是流式的
/// pack（可能很大、持续时间长），只能靠 per-read 超时；API 的 body 就 64KB，没有任何理由慢。
fn read_api_body(reader: &mut BufReader<TcpStream>, len: usize) -> Option<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(API_BODY_DEADLINE_SECS);
    let mut out = Vec::with_capacity(len);
    let mut chunk = [0u8; 8192];
    let mut taken = reader.by_ref().take(len as u64);
    while out.len() < len {
        if Instant::now() > deadline {
            return None;
        }
        match taken.read(&mut chunk) {
            Ok(0) => break, // 对面提前关了：按已收到的算，交给 JSON 解析去报错
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }
    Some(out)
}

fn handle(mut stream: TcpStream, ctx: &Ctx, peer: Option<IpAddr>, proxied: bool) -> std::io::Result<()> {
    // 读头阶段：短读超时，任何一次阻塞读（含请求行）最多挂 HEADER_DEADLINE_SECS。
    let _ = stream.set_read_timeout(Some(Duration::from_secs(HEADER_DEADLINE_SECS)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(BODY_TIMEOUT_SECS)));

    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(req) = read_head(&mut reader) else {
        return Ok(());
    };
    let path = req.target.split('?').next().unwrap_or("/").to_string();

    // 路径穿越总闸：任何 `..` 段一律拒绝。
    if path.split('/').any(|seg| seg == "..") {
        return write_response(&mut stream, &Resp::text(400, "bad request"));
    }

    // 代理后的真实客户端 IP：只有 peer 是**声明过的**可信代理时才认 XFF（谁都能伪造它）。
    // 这些连接在 accept 时故意没数 per-IP（否则代理后的所有人共用一个配额），在这里补上。
    let client = peer.map(|p| net::client_ip(p, req.header("x-forwarded-for"), &ctx.cfg.trusted_proxies));
    let _client_guard = match (proxied, client) {
        (true, Some(ip)) => match ctx.limiter.try_acquire(ip) {
            Some(g) => Some(g),
            None => return write_response(&mut stream, &Resp::text(429, "too many connections")),
        },
        _ => None,
    };

    // 认证只看请求头 —— **不需要 body**。于是所有入口都能做到"先认身份，再决定要不要读 body"。
    let secrets = credentials(&req);
    let authn = auth::authenticate(&ctx.store, &ctx.sessions, req.sid().as_deref(), &secrets);
    if let Some(id) = &authn.token_id {
        auth::touch_token(&ctx.store, id);
    }
    let caller = authn.caller;
    let actor = caller.user.clone().unwrap_or_else(|| "anonymous".into());

    // ── git smart-http ──
    // 先解析出**是哪个 agent**，再授权，最后才碰 body。
    // 老代码在这里只看 `path.contains(".git/")`，鉴权是全站一把梭，还开着 GIT_HTTP_EXPORT_ALL=1
    // —— 于是读闸一过，root 下任何库都能拉走。现在每个库各自判定。
    if let Some(route) = net::parse_git_path(&path, req.query()) {
        // 不存在的 agent 一律当「无主私有」来判定，**判定在前、存在性检查在后** ——
        // 否则「不存在→404 / 私有→401」的差别本身就是一个枚举私有 agent 名字的接口。
        let meta = ctx.store.agent_or_unowned(&route.agent);
        match acl::decide(&caller, &meta.to_acl(), route.action) {
            Decision::Allow => {
                // 判定通过了才配知道它到底存不存在。
                if !repo_path(ctx.root(), &route.agent).exists() {
                    return write_response(&mut stream, &Resp::text(404, "no such agent"));
                }
            }
            Decision::Deny(d) => {
                audit_deny(ctx, &actor, Some(&route.agent), route.action, d);
                // git 客户端要靠 401 + WWW-Authenticate 才会去问用户要凭据。
                return write_response(&mut stream, &git_deny_resp(&caller, d));
            }
        }
        // 未授权的 push 直接在上面 401 了，绝不把它的 pack 读进内存
        //（否则匿名 POST 一个 512MB body 就能把进程撑爆 —— body-before-auth 的内存耗尽 DoS）。
        if req.content_length > MAX_BODY {
            return write_response(&mut stream, &Resp::text(413, "payload too large"));
        }
        audit::append(
            ctx.root(),
            &actor,
            if route.action == Action::Write { audit::GIT_PUSH } else { audit::GIT_FETCH },
            Some(&route.agent),
            &path,
        );
        return git_http(&mut stream, &mut reader, ctx, &req, &route.agent, &actor);
    }

    // ── 其余路由 ──
    // body 上限收到 API_MAX_BODY：这里的 body 只可能是小 JSON。认证已经在上面做完了。
    let body = if req.method == "GET" || req.content_length == 0 {
        Vec::new()
    } else if req.content_length > API_MAX_BODY {
        return write_response(&mut stream, &Resp::text(413, "payload too large"));
    } else {
        match read_api_body(&mut reader, req.content_length) {
            Some(b) => b,
            None => return write_response(&mut stream, &Resp::text(408, "request timeout")),
        }
    };

    let resp = route(ctx, &req, &path, &caller, &body);
    write_response(&mut stream, &resp)
}

/// git 的拒绝响应。匿名 → 401 带 Basic 挑战（git 会去问用户要凭据）；已认证但没权限 → 404/403。
fn git_deny_resp(caller: &Caller, d: Deny) -> Resp {
    if d == Deny::Anonymous {
        return Resp::text(401, "需要凭据。git 密码处填 token（`agit-hub token add`），用户名随意。")
            .with("WWW-Authenticate", "Basic realm=\"agit-hub\"");
    }
    let _ = caller;
    Resp::text(403, &format!("拒绝：{}", d.reason()))
}

fn credentials(req: &Req) -> Vec<String> {
    let Some(v) = req.header("authorization") else {
        return vec![];
    };
    let v = v.trim();
    if let Some(b64) = v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic ")) {
        if let Some(dec) = b64_decode(b64.trim()) {
            if let Ok(s) = String::from_utf8(dec) {
                return match s.split_once(':') {
                    Some((u, p)) => vec![p.to_string(), u.to_string()],
                    None => vec![s],
                };
            }
        }
    }
    if let Some(t) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")) {
        return vec![t.trim().to_string()];
    }
    vec![]
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = vec![];
    for chunk in bytes.chunks(4) {
        let mut buf = [0u8; 4];
        let mut n = 0;
        for &c in chunk {
            if c == b'=' {
                break;
            }
            buf[n] = val(c)?;
            n += 1;
        }
        if n >= 2 {
            out.push((buf[0] << 2) | (buf[1] >> 4));
        }
        if n >= 3 {
            out.push((buf[1] << 4) | (buf[2] >> 2));
        }
        if n >= 4 {
            out.push((buf[2] << 6) | buf[3]);
        }
    }
    Some(out)
}

// ─────────────────────────── 响应 ───────────────────────────

struct Resp {
    status: u16,
    ctype: String,
    body: Vec<u8>,
    extra: Vec<(String, String)>,
}

impl Resp {
    fn new(status: u16, ctype: &str, body: Vec<u8>) -> Resp {
        Resp { status, ctype: ctype.to_string(), body, extra: vec![] }
    }
    fn text(status: u16, s: &str) -> Resp {
        Resp::new(status, "text/plain; charset=utf-8", s.as_bytes().to_vec())
    }
    fn json(v: serde_json::Value) -> Resp {
        Resp::json_status(200, v)
    }
    fn json_status(status: u16, v: serde_json::Value) -> Resp {
        Resp::new(status, "application/json", serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec()))
    }
    fn err(status: u16, msg: &str) -> Resp {
        Resp::json_status(status, serde_json::json!({ "error": msg }))
    }
    fn no_content() -> Resp {
        Resp::new(204, "text/plain; charset=utf-8", vec![])
    }
    fn with(mut self, k: &str, v: &str) -> Resp {
        self.extra.push((k.to_string(), v.to_string()));
        self
    }
}

fn write_response(stream: &mut TcpStream, r: &Resp) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n", r.status, reason(r.status), r.ctype, r.body.len());
    for (k, v) in &r.extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&r.body)?;
    stream.flush()
}

fn route(ctx: &Ctx, req: &Req, path: &str, caller: &Caller, body: &[u8]) -> Resp {
    // 前端资源。
    if req.method == "GET" {
        match path {
            "/assets/app.js" => return Resp::new(200, "application/javascript; charset=utf-8", APP_JS.as_bytes().to_vec()),
            "/assets/app.css" => return Resp::new(200, "text/css; charset=utf-8", APP_CSS.as_bytes().to_vec()),
            "/favicon.ico" => return Resp::new(200, "image/svg+xml", FAVICON.as_bytes().to_vec()),
            _ => {}
        }
    }
    // JSON API。
    if let Some(rest) = path.strip_prefix("/api/") {
        return api(ctx, req, rest, caller, body);
    }
    if req.method != "GET" {
        return Resp::text(405, "method not allowed");
    }
    // 其余一切 → SPA（前端自己按 URL 渲染 home/agent/session/diff）。
    // SPA 本身不含数据 —— 数据都在 /api/* 后面，各自鉴权。
    Resp::new(200, "text/html; charset=utf-8", INDEX_HTML.as_bytes().to_vec())
}

// ─────────────────────────── 授权闸 ───────────────────────────

/// 记一条拒绝。匿名读被拒 = "还没登录"，那是噪音；已认证的人被拒、或写/管理动作被拒才是信号。
fn audit_deny(ctx: &Ctx, actor: &str, agent: Option<&str>, action: Action, d: Deny) {
    if actor != "anonymous" || action != Action::Read {
        audit::append(ctx.root(), actor, audit::DENIED, agent, &format!("{action:?}: {}", d.reason()));
    }
}

/// 取 agent + 判定 + 出错响应。**每个 agent 入口都从这里进**。
///
/// 存在性本身也是机密：不存在的 agent 按「无主私有」判定，于是"不存在"与"看不见"
/// 给出**同一个**响应 —— 否则 401/403/404 的差别就是一个枚举私有 agent 名字的接口。
/// 判定通过之后才检查存在性（有权限的人才配知道它不存在）。
fn gate(ctx: &Ctx, caller: &Caller, name: &str, action: Action) -> Result<AgentMeta, Resp> {
    // 名字形状不对 → 404。这不是机密：它压根不可能是个合法 agent。
    if !valid_agent_name(name) {
        return Err(Resp::err(404, "not found"));
    }
    let meta = ctx.store.agent_or_unowned(name);
    let acl = meta.to_acl();
    match acl::decide(caller, &acl, action) {
        Decision::Allow => match repo_path(ctx.root(), name).exists() {
            true => Ok(meta),
            false => Err(Resp::err(404, "not found")),
        },
        Decision::Deny(d) => {
            let actor = caller.user.clone().unwrap_or_else(|| "anonymous".into());
            audit_deny(ctx, &actor, Some(name), action, d);
            Err(deny_resp(caller, &acl, d))
        }
    }
}

fn deny_resp(caller: &Caller, acl: &AgentAcl, d: Deny) -> Resp {
    // 能读的人被拒写/拒管理 → 告诉他 403（他本来就知道这个 agent 存在）。
    // 读都读不了 → 404，连「存在」都不承认。
    let can_read = acl::decide(caller, acl, Action::Read).allowed();
    match (d, can_read) {
        (Deny::Anonymous, false) => Resp::err(401, "需要登录"),
        (_, false) => Resp::err(404, "not found"),
        (_, true) => Resp::err(403, d.reason()),
    }
}

// ─────────────────────────── JSON API ───────────────────────────

fn api(ctx: &Ctx, req: &Req, rest: &str, caller: &Caller, body: &[u8]) -> Resp {
    let m = req.method.as_str();
    match (m, rest) {
        ("POST", "login") => return api_login(ctx, req, body),
        ("POST", "logout") => return api_logout(ctx, req, caller),
        ("GET", "me") => return api_me(caller),
        ("GET", "agents") => return api_agents(ctx, req, caller),
        ("POST", "agents") => return api_create_agent(ctx, req, caller, body),
        ("GET", "tokens") => return api_tokens(ctx, caller),
        ("POST", "tokens") => return api_create_token(ctx, caller, body),
        ("GET", "audit") => return api_audit(ctx, req, caller),
        _ => {}
    }
    if let Some(id) = rest.strip_prefix("tokens/") {
        return match m {
            "DELETE" => api_revoke_token(ctx, caller, id),
            _ => Resp::text(405, "method not allowed"),
        };
    }
    let Some(after) = rest.strip_prefix("agent/") else {
        return Resp::err(404, "not found");
    };

    // agent/<name>/session/<id>[/diff]
    if let Some((name, tail)) = after.split_once("/session/") {
        if m != "GET" {
            return Resp::text(405, "method not allowed");
        }
        let meta = match gate(ctx, caller, name, Action::Read) {
            Ok(x) => x,
            Err(r) => return r,
        };
        let repo = repo_path(ctx.root(), &meta.name);
        if !has_head(&repo) {
            return Resp::err(404, "not found");
        }
        if let Some(id) = tail.strip_suffix("/diff") {
            return api_diff(&repo, id, req.query());
        }
        return api_session(&repo, tail, req.query());
    }

    // agent/<name>/members[/<username>] —— tail 只能是空或 /<username>，
    // 别把 /membersXYZ 也认成 /members。
    if let Some((name, tail)) = after.split_once("/members") {
        if tail.is_empty() || tail.starts_with('/') {
            return api_members(ctx, caller, name, tail, m, body);
        }
    }

    // agent/<name>
    match m {
        "GET" => {
            let meta = match gate(ctx, caller, after, Action::Read) {
                Ok(x) => x,
                Err(r) => return r,
            };
            api_agent(ctx, req, caller, &meta)
        }
        "PATCH" => api_patch_agent(ctx, caller, after, body),
        "DELETE" => api_delete_agent(ctx, caller, after),
        _ => Resp::text(405, "method not allowed"),
    }
}

// ── 认证 ──

fn api_login(ctx: &Ctx, req: &Req, body: &[u8]) -> Resp {
    let Some(v) = json_body(body) else {
        return Resp::err(400, "要 JSON body");
    };
    let (Some(username), Some(password)) = (str_field(&v, "username"), str_field(&v, "password")) else {
        return Resp::err(400, "要 username 与 password");
    };
    // argon2 是故意慢的 —— 不封顶并发就等于给人一个 CPU/内存放大器。
    let verified = {
        let _slot = Permit::acquire(ctx.login_gate.clone());
        auth::verify_login(&ctx.store, &username, &password)
    };
    let Some(user) = verified else {
        audit::append(ctx.root(), &store::normalize_username(&username), audit::LOGIN_FAILED, None, &req.host());
        // 不说"用户不存在"还是"密码错" —— 那是给爆破的人递用户名字典。
        return Resp::err(401, "用户名或密码不对");
    };
    let Ok(sid) = ctx.sessions.create(&user.username) else {
        return Resp::err(503, "会话建不出来，稍后再试");
    };
    audit::append(ctx.root(), &user.username, audit::LOGIN, None, "");
    Resp::json(serde_json::json!({ "username": user.username, "is_admin": user.is_admin }))
        .with("Set-Cookie", &websession::set_cookie(&sid, ctx.cfg.tls))
}

fn api_logout(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    if let Some(sid) = req.sid() {
        ctx.sessions.revoke(&sid);
    }
    if let Some(u) = &caller.user {
        audit::append(ctx.root(), u, audit::LOGOUT, None, "");
    }
    Resp::no_content().with("Set-Cookie", &websession::clear_cookie(ctx.cfg.tls))
}

fn api_me(caller: &Caller) -> Resp {
    match &caller.user {
        Some(u) => Resp::json(serde_json::json!({ "username": u, "is_admin": caller.is_admin })),
        None => Resp::err(401, "未登录"),
    }
}

// ── agents ──

fn api_agents(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    let items: Vec<serde_json::Value> = list_agents(ctx.root())
        .iter()
        .filter_map(|n| {
            let meta = ctx.store.agent_or_unowned(n);
            // 看不见的不进列表 —— 列表本身就是"谁能看谁的 agent"的第一道答案。
            if !acl::decide(caller, &meta.to_acl(), Action::Read).allowed() {
                return None;
            }
            let repo = repo_path(ctx.root(), n);
            let (count, when, subject) = if has_head(&repo) {
                let (w, s) = last_activity(&repo);
                (session_refs(&repo).len(), w, s)
            } else {
                (0, String::new(), String::new())
            };
            let (aid, aid_source) = agent_aid(&repo);
            Some(serde_json::json!({
                "name": n,
                "aid": aid,
                "aid_source": aid_source,
                "sessions": count,
                "when": when,
                "subject": subject,
                "visibility": meta.visibility,
                "role": effective_role(caller, &meta),
            }))
        })
        .collect();
    Resp::json(serde_json::json!({ "agents": items, "host": req.host() }))
}

/// 调用方在这个 agent 上的**有效**角色，给 UI 决定显示哪些按钮。
/// null = 没有显式授权（能看见只是因为它是 public）。
fn effective_role(caller: &Caller, meta: &AgentMeta) -> Option<&'static str> {
    let user = caller.user.as_deref()?;
    if meta.owner.as_deref() == Some(user) {
        return Some("owner");
    }
    if caller.is_admin {
        return Some("admin");
    }
    meta.role_of(user).map(|r| r.as_str())
}

fn api_create_agent(ctx: &Ctx, req: &Req, caller: &Caller, body: &[u8]) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "需要登录");
    };
    let Some(v) = json_body(body) else {
        return Resp::err(400, "要 JSON body");
    };
    let Some(name) = str_field(&v, "name") else {
        return Resp::err(400, "要 name");
    };
    if !valid_agent_name(&name) {
        return Resp::err(400, "非法名字（只允许 [A-Za-z0-9._-]，禁止 .. 与前导点）");
    }
    // 没写 visibility 就是 private。**默认私有**，公开必须是显式的一句话。
    let visibility = match v.get("visibility").and_then(|x| x.as_str()) {
        None => Visibility::Private,
        Some(s) => match Visibility::parse(s) {
            Some(x) => x,
            None => return Resp::err(400, "visibility 只能是 private 或 public"),
        },
    };
    // 建库也过同一个判定：把它当成"在一个我自己是 owner 的 agent 上写" ——
    // 于是绑在别的 agent 上的 token、只读 token 都建不出东西来。
    let hypothetical = AgentAcl { name: name.clone(), owner: Some(user.clone()), visibility, members: vec![] };
    if let Decision::Deny(d) = acl::decide(caller, &hypothetical, Action::Write) {
        audit_deny(ctx, &user, Some(&name), Action::Write, d);
        return Resp::err(403, d.reason());
    }
    match create_agent(&ctx.store, &name, &user, visibility) {
        Ok(_) => {
            audit::append(ctx.root(), &user, audit::AGENT_CREATE, Some(&name), &format!("visibility={}", visibility.as_str()));
            let repo = repo_path(ctx.root(), &name);
            let (aid, aid_source) = agent_aid(&repo);
            Resp::json_status(
                201,
                serde_json::json!({
                    "name": name,
                    // 空库里还没有 agent.toml —— aid 要等客户端把它推上来才有。老实报 null。
                    "aid": aid,
                    "aid_source": aid_source,
                    "clone_url": clone_url(ctx, req, &name),
                    "visibility": visibility.as_str(),
                }),
            )
        }
        Err(e) => Resp::err(409, &e),
    }
}

fn clone_url(ctx: &Ctx, req: &Req, name: &str) -> String {
    format!("{}://{}/{name}.git", if ctx.cfg.tls { "https" } else { "http" }, req.host())
}

fn api_agent(ctx: &Ctx, req: &Req, caller: &Caller, meta: &AgentMeta) -> Resp {
    let name = &meta.name;
    let repo = repo_path(ctx.root(), name);
    let query = req.query();
    let search = param(query, "q").map(|q| q.replace('+', " ")).unwrap_or_default();
    let pageno: usize = param(query, "page").and_then(|p| p.parse().ok()).unwrap_or(1).max(1);
    let refs = if has_head(&repo) { session_refs(&repo) } else { vec![] };

    // 命中集合：无搜索 = 直接分页（只 git show 当页）；有搜索 = 扫内容（有上限）。
    let (window, total): (Vec<&SessionRef>, usize) = if search.is_empty() {
        let start = (pageno - 1) * PER_PAGE;
        (refs.iter().skip(start).take(PER_PAGE).collect(), refs.len())
    } else {
        let mut hits = vec![];
        for r in refs.iter().take(SEARCH_SCAN_CAP) {
            if load_session(&repo, &r.path, None).map(|b| b.contains(&search)).unwrap_or(false) {
                hits.push(r);
            }
        }
        let total = hits.len();
        let start = (pageno - 1) * PER_PAGE;
        (hits.into_iter().skip(start).take(PER_PAGE).collect(), total)
    };

    let sessions: Vec<serde_json::Value> = window
        .iter()
        .filter_map(|r| {
            let jsonl = load_session(&repo, &r.path, None)?;
            Some(session_summary(&repo, r, &jsonl))
        })
        .collect();

    let history: Vec<serde_json::Value> = recent_log(&repo, 10)
        .into_iter()
        .map(|(sha, subject)| serde_json::json!({ "sha": sha, "subject": subject }))
        .collect();

    let (aid, aid_source) = agent_aid(&repo);
    // 学到 aid 就顺手缓存进 agents.json —— 权威值仍然只在 store 里，这只是省一次 git show。
    if let Some(a) = &aid {
        if meta.aid.as_deref() != Some(a.as_str()) {
            let _ = ctx.store.update_agents(|list| {
                if let Some(m) = list.iter_mut().find(|m| &m.name == name) {
                    m.aid = Some(a.clone());
                }
            });
        }
    }

    let members: Vec<serde_json::Value> = meta
        .members
        .iter()
        .map(|m| serde_json::json!({ "username": m.username, "role": m.role }))
        .collect();

    Resp::json(serde_json::json!({
        "agent": name,
        "git": format!("/{name}.git"),
        "aid": aid,
        "aid_source": aid_source,
        "clone_url": clone_url(ctx, req, name),
        "visibility": meta.visibility,
        "owner": meta.owner,
        "members": members,
        "role": effective_role(caller, meta),
        "environments": environments(&repo, &refs),
        "branches": branches(&repo),
        "size_bytes": size_bytes(&repo),
        "runtimes": runtimes(&refs),
        "total": total,
        "page": pageno,
        "per_page": PER_PAGE,
        "scan_capped": !search.is_empty() && refs.len() > SEARCH_SCAN_CAP,
        "sessions": sessions,
        "history": history,
    }))
}

fn api_patch_agent(ctx: &Ctx, caller: &Caller, name: &str, body: &[u8]) -> Resp {
    let meta = match gate(ctx, caller, name, Action::Manage) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(v) = json_body(body) else {
        return Resp::err(400, "要 JSON body");
    };
    let actor = caller.user.clone().unwrap_or_default();

    if let Some(vis) = v.get("visibility").and_then(|x| x.as_str()) {
        let Some(vis) = Visibility::parse(vis) else {
            return Resp::err(400, "visibility 只能是 private 或 public");
        };
        if vis.as_str() != meta.visibility {
            let r = ctx.store.update_agents(|list| {
                if let Some(m) = list.iter_mut().find(|m| m.name == meta.name) {
                    m.visibility = vis.as_str().to_string();
                }
            });
            if r.is_err() {
                return Resp::err(500, "写 agents.json 失败");
            }
            audit::append(ctx.root(), &actor, audit::AGENT_VISIBILITY, Some(&meta.name), &format!("{} → {}", meta.visibility, vis.as_str()));
        }
    }

    if let Some(newname) = str_field(&v, "name") {
        if newname != meta.name {
            if !valid_agent_name(&newname) {
                return Resp::err(400, "非法名字（只允许 [A-Za-z0-9._-]，禁止 .. 与前导点）");
            }
            if repo_path(ctx.root(), &newname).exists() || ctx.store.agent(&newname).is_some() {
                return Resp::err(409, "这个名字已经被占了");
            }
            if std::fs::rename(repo_path(ctx.root(), &meta.name), repo_path(ctx.root(), &newname)).is_err() {
                return Resp::err(500, "改名失败（仓库目录动不了）");
            }
            let r = ctx.store.update_agents(|list| {
                if let Some(m) = list.iter_mut().find(|m| m.name == meta.name) {
                    m.name = newname.clone();
                }
            });
            if r.is_err() {
                return Resp::err(500, "写 agents.json 失败");
            }
            // token 绑的是**名字**。改名不改身份（aid 在 store 里），所以绑定要跟着走 ——
            // 否则改个名就把所有 CI token 悄悄弄哑了。
            let _ = ctx.store.update_tokens(|toks| {
                for t in toks.iter_mut().filter(|t| t.agent.as_deref() == Some(meta.name.as_str())) {
                    t.agent = Some(newname.clone());
                }
            });
            audit::append(ctx.root(), &actor, audit::AGENT_RENAME, Some(&newname), &format!("{} → {newname}", meta.name));
            return Resp::json(serde_json::json!({ "name": newname, "renamed_from": meta.name }));
        }
    }

    let fresh = ctx.store.agent_or_unowned(&meta.name);
    Resp::json(serde_json::json!({ "name": fresh.name, "visibility": fresh.visibility, "owner": fresh.owner }))
}

fn api_delete_agent(ctx: &Ctx, caller: &Caller, name: &str) -> Resp {
    let meta = match gate(ctx, caller, name, Action::Manage) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if std::fs::remove_dir_all(repo_path(ctx.root(), &meta.name)).is_err() {
        return Resp::err(500, "删不掉仓库目录");
    }
    let _ = ctx.store.update_agents(|list| list.retain(|m| m.name != meta.name));
    // 绑在这个名字上的 token 必须一起死：否则将来有人建同名 agent，老 token 会**自动**获得
    // 对那个新 agent 的权限（名字被回收了，token 却还认这个名字）。
    let _ = ctx.store.update_tokens(|toks| toks.retain(|t| t.agent.as_deref() != Some(meta.name.as_str())));
    audit::append(ctx.root(), &caller.user.clone().unwrap_or_default(), audit::AGENT_DELETE, Some(&meta.name), "");
    Resp::no_content()
}

// ── members ──

fn api_members(ctx: &Ctx, caller: &Caller, name: &str, tail: &str, method: &str, body: &[u8]) -> Resp {
    let actor = caller.user.clone().unwrap_or_default();
    // GET 只要读权限（成员表在 agent 详情里本来就给读者看）；增删要 Manage。
    let action = if method == "GET" { Action::Read } else { Action::Manage };
    let meta = match gate(ctx, caller, name, action) {
        Ok(x) => x,
        Err(r) => return r,
    };

    let target = tail.strip_prefix('/').map(|s| s.to_string());
    match (method, target) {
        ("GET", None) => Resp::json(serde_json::json!(meta
            .members
            .iter()
            .map(|m| serde_json::json!({ "username": m.username, "role": m.role }))
            .collect::<Vec<_>>())),
        ("POST", None) => {
            let Some(v) = json_body(body) else {
                return Resp::err(400, "要 JSON body");
            };
            let (Some(username), Some(role)) = (str_field(&v, "username"), str_field(&v, "role")) else {
                return Resp::err(400, "要 username 与 role");
            };
            let username = store::normalize_username(&username);
            let Some(role) = Role::parse(&role) else {
                return Resp::err(400, "role 只能是 read / write / admin");
            };
            // 只能加真实存在的用户 —— 不然 agents.json 里会攒一堆拼错的名字，
            // 而将来真有人叫这个名字时会**自动**继承这份权限。
            if ctx.store.user(&username).is_none() {
                return Resp::err(400, "没有这个用户");
            }
            if meta.owner.as_deref() == Some(username.as_str()) {
                return Resp::err(400, "owner 本来就有全部权限，不用再加成员");
            }
            let r = ctx.store.update_agents(|list| {
                if let Some(m) = list.iter_mut().find(|m| m.name == meta.name) {
                    match m.members.iter_mut().find(|x| x.username == username) {
                        Some(x) => x.role = role.as_str().to_string(),
                        None => m.members.push(Member { username: username.clone(), role: role.as_str().to_string() }),
                    }
                }
            });
            if r.is_err() {
                return Resp::err(500, "写 agents.json 失败");
            }
            audit::append(ctx.root(), &actor, audit::MEMBER_ADD, Some(&meta.name), &format!("{username}={}", role.as_str()));
            let fresh = ctx.store.agent_or_unowned(&meta.name);
            Resp::json(serde_json::json!(fresh
                .members
                .iter()
                .map(|m| serde_json::json!({ "username": m.username, "role": m.role }))
                .collect::<Vec<_>>()))
        }
        ("DELETE", Some(username)) => {
            let username = store::normalize_username(&username);
            let removed = ctx
                .store
                .update_agents(|list| match list.iter_mut().find(|m| m.name == meta.name) {
                    Some(m) => {
                        let before = m.members.len();
                        m.members.retain(|x| x.username != username);
                        before != m.members.len()
                    }
                    None => false,
                })
                .unwrap_or(false);
            if !removed {
                return Resp::err(404, "这个人不是成员");
            }
            audit::append(ctx.root(), &actor, audit::MEMBER_REMOVE, Some(&meta.name), &username);
            Resp::no_content()
        }
        _ => Resp::text(405, "method not allowed"),
    }
}

// ── tokens ──

fn api_tokens(ctx: &Ctx, caller: &Caller) -> Resp {
    let Some(user) = caller.user.as_deref() else {
        return Resp::err(401, "需要登录");
    };
    // 只看自己的；站点管理员看全部。
    let items: Vec<serde_json::Value> = ctx
        .store
        .tokens()
        .iter()
        .filter(|t| caller.is_admin || t.owner.as_deref() == Some(user))
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "name": t.name,
                "owner": t.owner,
                "agent": t.agent,
                "scope": t.scope,
                "created": t.created,
                "expires": t.expires,
                "last_used": t.last_used,
                // 老的无主 token 会在这里现形（它们已经不能用了）。
                "usable": t.usable(),
            })
        })
        .collect();
    Resp::json(serde_json::json!(items))
}

fn api_create_token(ctx: &Ctx, caller: &Caller, body: &[u8]) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "需要登录");
    };
    // 发凭据必须是本人登录会话：拿 token 再发 token 是把一次泄露变成永久立足点
    //（旧 token 到期了，它生的新 token 还活着）。
    if caller.token.is_some() {
        return Resp::err(403, "发 token 要用登录会话，不能拿 token 发 token");
    }
    let Some(v) = json_body(body) else {
        return Resp::err(400, "要 JSON body");
    };
    let Some(name) = str_field(&v, "name") else {
        return Resp::err(400, "要 name");
    };
    let Some(scope) = str_field(&v, "scope").and_then(|s| Scope::parse(&s)) else {
        return Resp::err(400, "scope 只能是 read 或 write");
    };
    let agent = str_field(&v, "agent");
    if let Some(a) = &agent {
        // 只能给自己看得见的 agent 发 token。
        if let Err(r) = gate(ctx, caller, a, Action::Read) {
            return r;
        }
    }
    let ttl_days = match v.get("ttl_days") {
        None | Some(serde_json::Value::Null) => None,
        Some(x) => match x.as_i64() {
            Some(n) if n > 0 && n <= 3650 => Some(n),
            _ => return Resp::err(400, "ttl_days 要 1..3650 的整数"),
        },
    };
    match issue_token(&ctx.store, &name, &user, agent.as_deref(), scope, ttl_days) {
        Ok(secret) => {
            audit::append(ctx.root(), &user, audit::TOKEN_CREATE, agent.as_deref(), &format!("name={name} scope={}", scope.as_str()));
            // 明文只此一次 —— 服务器只留 sha256 摘要，之后谁也变不回来。
            Resp::json_status(201, serde_json::json!({ "token": secret }))
        }
        Err(e) => Resp::err(500, &e),
    }
}

fn api_revoke_token(ctx: &Ctx, caller: &Caller, id: &str) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "需要登录");
    };
    let Some(t) = ctx.store.tokens().into_iter().find(|t| t.id == id) else {
        return Resp::err(404, "not found");
    };
    // 自己的 token，或者站点管理员。
    if !caller.is_admin && t.owner.as_deref() != Some(user.as_str()) {
        return Resp::err(404, "not found");
    }
    let _ = ctx.store.update_tokens(|toks| toks.retain(|x| x.id != id));
    audit::append(ctx.root(), &user, audit::TOKEN_REVOKE, t.agent.as_deref(), &format!("id={id} name={}", t.name));
    Resp::no_content()
}

// ── audit ──

fn api_audit(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    let limit: usize = param(req.query(), "limit").and_then(|s| s.parse().ok()).unwrap_or(100).clamp(1, 1000);
    match param(req.query(), "agent") {
        // 某个 agent 的审计：要这个 agent 的 Manage（owner / 成员 admin / 站点管理员）。
        Some(name) => {
            let meta = match gate(ctx, caller, &name, Action::Manage) {
                Ok(x) => x,
                Err(r) => return r,
            };
            Resp::json(serde_json::json!(audit::query(ctx.root(), Some(&meta.name), limit)))
        }
        // 全站审计：只有站点管理员，且必须是登录会话（token 做不了管理动作）。
        None => {
            if !caller.is_admin || caller.token.is_some() {
                return Resp::err(403, "全站审计只对站点管理员开放（且要用登录会话）");
            }
            Resp::json(serde_json::json!(audit::query(ctx.root(), None, limit)))
        }
    }
}

// ── sessions（读权限已在调用处判完）──

fn session_summary(repo: &Path, r: &SessionRef, jsonl: &str) -> serde_json::Value {
    let d = digest(&r.runtime, &r.id, jsonl);
    let p = provenance(repo, &r.path, jsonl);
    serde_json::json!({
        "id": d.id,
        "env": r.env,
        "runtime": r.runtime,
        "branch": d.branch,
        "model": p.model,
        "author": p.author,
        "when": p.when,
        "commit": p.commit,
        "title": d.prompts.first().map(|s| first_line(s)).unwrap_or_default(),
        "conclusion": d.texts.last().map(|t| clip(t, 280)).unwrap_or_default(),
        "files": d.files,
        "tools": d.tools,
        "n_prompts": d.prompts.len(),
        "n_texts": d.texts.len(),
        "spine": spine_string(&r.runtime, jsonl),
    })
}

fn api_session(repo: &Path, id: &str, query: &str) -> Resp {
    let Some(r) = session_refs(repo).into_iter().find(|r| r.id == id) else {
        return Resp::err(404, "not found");
    };
    let at = param(query, "at");
    let Some(jsonl) = load_session(repo, &r.path, at.as_deref()) else {
        return Resp::err(404, "no such revision");
    };
    let d = digest(&r.runtime, &r.id, &jsonl);
    let p = provenance(repo, &r.path, &jsonl);
    let revisions: Vec<serde_json::Value> = session_revisions(repo, &r.path)
        .into_iter()
        .map(|(sha, when, subject)| serde_json::json!({ "sha": sha, "when": when, "subject": subject }))
        .collect();

    Resp::json(serde_json::json!({
        "id": d.id,
        "env": r.env,
        "runtime": r.runtime,
        "branch": d.branch,
        "cwd": d.cwd,
        "model": p.model,
        "author": p.author,
        "when": p.when,
        "commit": p.commit,
        "prompts": d.prompts.iter().map(|s| first_line(s)).collect::<Vec<_>>(),
        "texts": d.texts.iter().rev().take(8).rev().map(|t| clip(t, 700)).collect::<Vec<_>>(),
        "files": d.files,
        "spine": spine_string(&r.runtime, &jsonl),
        "revisions": revisions,
        "pinned": at,
    }))
}

fn api_diff(repo: &Path, id: &str, query: &str) -> Resp {
    let Some(r) = session_refs(repo).into_iter().find(|r| r.id == id) else {
        return Resp::err(404, "not found");
    };
    let (Some(from), Some(to)) = (param(query, "from"), param(query, "to")) else {
        return Resp::err(400, "need from and to");
    };
    let (Some(ja), Some(jb)) = (load_session(repo, &r.path, Some(&from)), load_session(repo, &r.path, Some(&to))) else {
        return Resp::err(404, "no such revision");
    };
    let a = digest(&r.runtime, id, &ja);
    let b = digest(&r.runtime, id, &jb);
    Resp::json(serde_json::json!({
        "from": from,
        "to": to,
        "added_prompts": diff_list(&b.prompts, &a.prompts),
        "removed_prompts": diff_list(&a.prompts, &b.prompts),
        "added_files": diff_list(&b.files, &a.files),
        "removed_files": diff_list(&a.files, &b.files),
        "conclusion_before": a.texts.last().map(|t| clip(t, 300)).unwrap_or_default(),
        "conclusion_after": b.texts.last().map(|t| clip(t, 300)).unwrap_or_default(),
    }))
}

/// a 里有、b 里没有的元素（保序去重，取首行）。
fn diff_list(a: &[String], b: &[String]) -> Vec<String> {
    let bset: std::collections::HashSet<&String> = b.iter().collect();
    let mut seen = std::collections::HashSet::new();
    a.iter()
        .filter(|x| !bset.contains(*x) && seen.insert((*x).clone()))
        .map(|s| first_line(s))
        .collect()
}

fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| kv.strip_prefix(&format!("{key}="))).map(|v| v.to_string())
}

fn json_body(body: &[u8]) -> Option<serde_json::Value> {
    serde_json::from_slice(body).ok()
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

// ─────────────────────── git smart-http (sync) ───────────────────────

/// **调用前必须已经授权过 `name`**（见 handle）。这里只负责搬运。
fn git_http(stream: &mut TcpStream, reader: &mut BufReader<TcpStream>, ctx: &Ctx, req: &Req, name: &str, actor: &str) -> std::io::Result<()> {
    let (path, query) = match req.target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (req.target.clone(), String::new()),
    };
    let ctype = req.header("content-type").unwrap_or("").to_string();

    // 已鉴权，进入 body 传输：放宽读超时（pack 可能不小、跨网慢，但持续流入）。
    let _ = stream.set_read_timeout(Some(Duration::from_secs(BODY_TIMEOUT_SECS)));

    ensure_exportable(&repo_path(ctx.root(), name));

    let mut child = match Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", ctx.root())
        // GIT_HTTP_EXPORT_ALL **故意不设**：设了它，http-backend 就会服务 root 下任何 *.git，
        // 于是"授权哪个 agent"这件事在它眼里根本不存在。现在只有被 ensure_exportable 标记过的
        // 库（= 刚刚通过了 ACL 的那一个）它才认。真正的闸门是上面的 acl::decide，这只是第二道。
        .env("REQUEST_METHOD", &req.method)
        .env("PATH_INFO", &path)
        .env("QUERY_STRING", &query)
        .env("CONTENT_TYPE", &ctype)
        .env("CONTENT_LENGTH", req.content_length.to_string())
        // 谁推的会进 reflog；也让 http-backend 的错误里带上人。
        .env("REMOTE_USER", actor)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return write_response(stream, &Resp::text(500, "git http-backend 不可用")),
    };

    // 把 body **流式**从 socket 灌进 http-backend stdin(不再整包 read_to_end 进 Vec)。
    let mut stdin = child.stdin.take().unwrap();
    let n = req.content_length.min(MAX_BODY) as u64;
    let _ = std::io::copy(&mut reader.by_ref().take(n), &mut stdin);
    drop(stdin); // 关 stdin 发 EOF，让 http-backend 收尾
    let out = child.wait_with_output()?;

    // CGI 输出 = 头部 + 空行 + 体。规范化头部：拆出 git 的 Status: 作真状态；丢掉它的
    // Content-Length（我们自己算）；每行只补一个 CRLF（别对已是 CRLF 的头再 \n→\r\n 造出 \r\r\n）。
    let raw = out.stdout;
    let sep = find_subslice(&raw, b"\r\n\r\n").map(|i| (i, 4)).or_else(|| find_subslice(&raw, b"\n\n").map(|i| (i, 2)));
    let (raw_headers, body) = match sep {
        Some((i, n)) => (&raw[..i], &raw[i + n..]),
        None => (&b""[..], &raw[..]),
    };
    let mut status = 200u16;
    let mut fwd = String::new();
    for line in String::from_utf8_lossy(raw_headers).split('\n') {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            if key.eq_ignore_ascii_case("status") {
                status = v.trim().split_whitespace().next().and_then(|c| c.parse().ok()).unwrap_or(200);
                continue;
            }
            if key.eq_ignore_ascii_case("content-length") {
                continue; // 我们自己算，避免重复
            }
            fwd.push_str(key);
            fwd.push_str(": ");
            fwd.push_str(v.trim());
            fwd.push_str("\r\n");
        }
    }
    let head = format!(
        "HTTP/1.1 {status} {}\r\n{fwd}Content-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// 给**这一个**库打上 http-backend 的导出标记。
///
/// 没有 GIT_HTTP_EXPORT_ALL 的话，http-backend 只服务带 `git-daemon-export-ok` 的库。
/// 这里在授权通过之后才打标记，顺带把老仓库（`agit-hub add` 之前建的）自动带上来。
/// 注意：标记**不是**安全边界 —— 它只对 http-backend 说"这个库是拿来服务的"，
/// 谁能访问由 acl::decide 决定，而那一步已经在上面跑完了。
fn ensure_exportable(repo: &Path) {
    let marker = repo.join("git-daemon-export-ok");
    if !marker.exists() {
        let _ = std::fs::write(&marker, b"");
    }
}

fn find_subslice(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

const FAVICON: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><text y='13' font-size='13'>◆</text></svg>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes_basic_credentials() {
        assert_eq!(b64_decode("Z2l0OnNlY3JldDEyMw==").unwrap(), b"git:secret123");
        assert_eq!(b64_decode("YQ").unwrap(), b"ab".split_at(1).0);
        assert_eq!(b64_decode("YWI").unwrap(), b"ab");
    }

    #[test]
    fn credentials_come_from_basic_and_bearer() {
        let req = |auth: &str| Req {
            method: "GET".into(),
            target: "/".into(),
            headers: vec![("Authorization".into(), auth.into())],
            content_length: 0,
        };
        // git 把 token 填在密码位 —— 两段都当候选。
        assert_eq!(credentials(&req("Basic Z2l0OnNlY3JldDEyMw==")), vec!["secret123", "git"]);
        assert_eq!(credentials(&req("Bearer abc")), vec!["abc"]);
        assert_eq!(credentials(&req("bearer abc")), vec!["abc"]);
        assert!(credentials(&req("")).is_empty());
        let no_auth = Req { method: "GET".into(), target: "/".into(), headers: vec![], content_length: 0 };
        assert!(credentials(&no_auth).is_empty());
    }

    #[test]
    fn cookie_sid_is_read_from_the_header() {
        let req = Req {
            method: "GET".into(),
            target: "/".into(),
            headers: vec![("Cookie".into(), "a=1; agit_session=deadbeef".into())],
            content_length: 0,
        };
        assert_eq!(req.sid().as_deref(), Some("deadbeef"));
    }

    // ── bind 闸（需求 4）──

    #[test]
    fn loopback_binds_without_ceremony() {
        assert!(bind_guard("127.0.0.1".parse().unwrap(), false, false).is_ok());
        assert!(bind_guard("::1".parse().unwrap(), false, false).is_ok());
    }

    #[test]
    fn public_bind_without_tls_is_refused() {
        let e = bind_guard("0.0.0.0".parse().unwrap(), false, false).unwrap_err();
        // 拒绝要**说清为什么**，不是一句 "refused"。
        assert!(e.contains("明文"), "{e}");
        assert!(e.contains("--insecure"), "{e}");
        assert!(e.contains("--tls"), "{e}");
        assert!(bind_guard("192.168.1.10".parse().unwrap(), false, false).is_err());
    }

    #[test]
    fn public_bind_needs_tls_or_explicit_insecure() {
        assert!(bind_guard("0.0.0.0".parse().unwrap(), true, false).is_ok(), "--tls 放行");
        assert!(bind_guard("0.0.0.0".parse().unwrap(), false, true).is_ok(), "--insecure 放行");
    }

    // ── session 布局：新老两种都要认 ──

    #[test]
    fn runtimes_are_sorted_peers() {
        // claude-code 与 codex 对等 —— 字母序，谁也不是"默认第一个"。
        let refs = vec![
            SessionRef { env: None, runtime: "codex".into(), id: "a".into(), path: "sessions/codex/a.jsonl".into() },
            SessionRef { env: None, runtime: "claude-code".into(), id: "b".into(), path: "sessions/claude-code/b.jsonl".into() },
            SessionRef { env: None, runtime: "codex".into(), id: "c".into(), path: "sessions/codex/c.jsonl".into() },
        ];
        assert_eq!(runtimes(&refs), vec!["claude-code", "codex"]);
    }

    #[test]
    fn param_extracts_query_values() {
        assert_eq!(param("a=1&b=2", "b").as_deref(), Some("2"));
        assert_eq!(param("a=1", "b"), None);
        assert_eq!(param("", "b"), None);
        assert_eq!(param("service=git-receive-pack", "service").as_deref(), Some("git-receive-pack"));
    }

    // ── 拒绝时给什么状态码：这条策略就是"不泄露存在性" ──

    fn private_acl() -> AgentAcl {
        AgentAcl { name: "secret".into(), owner: Some("alice".into()), visibility: Visibility::Private, members: vec![] }
    }

    #[test]
    fn anonymous_denial_is_401_so_the_spa_can_offer_login() {
        let r = deny_resp(&Caller::anonymous(), &private_acl(), Deny::Anonymous);
        assert_eq!(r.status, 401);
    }

    #[test]
    fn denied_stranger_gets_404_not_403() {
        // 403 等于承认"这个 agent 存在" —— 那就是一个枚举私有 agent 名字的接口。
        // 读不了的人一律 404，跟"不存在"长得一模一样。
        let r = deny_resp(&Caller::user("eve"), &private_acl(), Deny::NoGrant);
        assert_eq!(r.status, 404);
    }

    #[test]
    fn reader_denied_a_write_gets_403() {
        // 能读的人本来就知道它存在，没什么可藏的 —— 告诉他真实原因。
        let acl = AgentAcl {
            name: "secret".into(),
            owner: Some("alice".into()),
            visibility: Visibility::Private,
            members: vec![("bob".into(), Role::Read)],
        };
        let r = deny_resp(&Caller::user("bob"), &acl, Deny::NoGrant);
        assert_eq!(r.status, 403);
    }

    #[test]
    fn token_denials_on_a_readable_agent_are_403() {
        let caller = Caller::user("alice").with_token(None, Scope::Read);
        assert_eq!(deny_resp(&caller, &private_acl(), Deny::TokenScope).status, 403);
        assert_eq!(deny_resp(&caller, &private_acl(), Deny::TokenCannotManage).status, 403);
    }

    #[test]
    fn git_anonymous_denial_challenges_so_git_asks_for_credentials() {
        // 没有这个头，`git clone` 不会去问用户要密码，只会直接报错。
        let r = git_deny_resp(&Caller::anonymous(), Deny::Anonymous);
        assert_eq!(r.status, 401);
        assert!(r.extra.iter().any(|(k, v)| k == "WWW-Authenticate" && v.contains("Basic")));
        // 已经认证过的就别再挑战了 —— 再问一遍密码也还是同一个答案。
        let r = git_deny_resp(&Caller::user("eve"), Deny::NoGrant);
        assert_eq!(r.status, 403);
        assert!(r.extra.is_empty());
    }

    #[test]
    fn json_helpers() {
        let v = json_body(br#"{"name":" x ","empty":"","n":3}"#).unwrap();
        assert_eq!(str_field(&v, "name").as_deref(), Some("x"), "两头空白要 trim 掉");
        assert_eq!(str_field(&v, "empty"), None, "空字符串当没给");
        assert_eq!(str_field(&v, "n"), None, "非字符串当没给");
        assert_eq!(str_field(&v, "nope"), None);
        assert!(json_body(b"not json").is_none());
        assert!(json_body(b"").is_none());
    }
}
