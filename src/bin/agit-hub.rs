//! agit-hub —— AgentGitHub 的第一版：Registry + Sync。
//!
//! PRD：「Hub 提供 Agent Context 首页、Workspace、搜索、revision、diff 和 history。
//! 人可以直接阅读，Agent 可以通过同一接口拉取。第一版是 Registry + Sync，
//! 不运行 Agent，也不保存 secret。」
//!
//! 形态：一个自包含的 HTTP 服务，托管一堆 Agent Store（bare git 仓库）。
//!   - Registry：扫描 hub root 下的 <name>.git
//!   - Sync：git smart-http，`agit -a push/pull http://host:port/<name>.git` 直接可用
//!   - 前端：服务端渲染 HTML，人可读
//!   - API：同样的数据以 JSON 暴露，agent 可拉取
//!
//! 零重量级依赖：std 的 TcpListener（每连接一线程）+ shell out 到 git。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("serve");
    let root = flag(&args, "--root")
        .map(PathBuf::from)
        .unwrap_or_else(default_root);

    match cmd {
        "serve" => {
            let port: u16 = flag(&args, "--port").and_then(|p| p.parse().ok()).unwrap_or(8177);
            serve(&root, port);
        }
        "add" => {
            let name = args.get(1).filter(|s| !s.starts_with("--")).cloned();
            match name {
                Some(n) => add_repo(&root, &n),
                None => eprintln!("用法: agit-hub add <name>"),
            }
        }
        "list" => {
            for a in list_agents(&root) {
                println!("{a}");
            }
        }
        "-h" | "--help" => print_help(),
        other => {
            eprintln!("未知子命令: {other}");
            print_help();
        }
    }
}

fn print_help() {
    println!(
        "agit-hub —— AgentGitHub (Registry + Sync)\n\n\
         agit-hub serve [--port 8177] [--root ~/.agit-hub]   启动 Hub\n\
         agit-hub add <name> [--root ...]                     新建一个 Agent Store 仓库\n\
         agit-hub list [--root ...]                           列出已托管的 agent\n\n\
         托管的仓库是 bare git。发布： agit -a push http://HOST:PORT/<name>.git"
    );
}

fn default_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".agit-hub")
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

// ─────────────────────────── Registry ───────────────────────────

fn add_repo(root: &Path, name: &str) {
    let dir = root.join(format!("{name}.git"));
    if dir.exists() {
        eprintln!("已存在: {}", dir.display());
        return;
    }
    std::fs::create_dir_all(&dir).unwrap();
    let ok = Command::new("git")
        .args(["init", "-q", "--bare", "-b", "main"])
        .arg(&dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        // http-backend 需要它才允许匿名 push/fetch
        let _ = Command::new("git").args(["-C"]).arg(&dir).args(["config", "http.receivepack", "true"]).status();
        println!("已托管 {name}  →  {}", dir.display());
        println!("发布： agit -a remote add origin http://localhost:8177/{name}.git && agit -a push -u origin main");
    } else {
        eprintln!("git init --bare 失败");
    }
}

fn list_agents(root: &Path) -> Vec<String> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(n) = name.strip_suffix(".git") {
                out.push(n.to_string());
            }
        }
    }
    out.sort();
    out
}

/// 合法 agent 名:只允许 [A-Za-z0-9._-],禁止 `..`、前导 `.`、路径分隔符与 NUL。
/// 名字来自 URL,直接拼进文件路径,不校验就是路径穿越（读到 root 之外的 git 仓库）。
fn valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn repo_path(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.git"))
}

// ─────────────────────── git 读取（bare 仓库）───────────────────────

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
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

// ─────────── session dump:列出 + 解析成可读摘要 ───────────

/// sessions/<runtime>/<id>.jsonl 的 (session_id, 内容)。跳过子 agent 的 jsonl。
fn sessions(repo: &Path) -> Vec<(String, String)> {
    let mut out = vec![];
    let Some(list) = git(repo, &["ls-tree", "-r", "--name-only", "HEAD", "sessions/"]) else {
        return out;
    };
    for path in list.lines() {
        let path = path.trim();
        // 只要顶层会话:sessions/<rt>/<id>.jsonl（正好 3 段）
        let segs: Vec<&str> = path.split('/').collect();
        if segs.len() != 3 || !path.ends_with(".jsonl") {
            continue;
        }
        let id = segs[2].trim_end_matches(".jsonl").to_string();
        if let Some(body) = git(repo, &["show", &format!("HEAD:{path}")]) {
            out.push((id, body));
        }
    }
    out
}

struct SessionDigest {
    id: String,
    branch: String,
    prompts: Vec<String>,
    texts: Vec<String>,
    tools: usize,
    files: Vec<String>,
}

/// 从一条 jsonl 转录里抽出可读摘要（Hub 只读、只解析,不运行 agent）。
/// **不再自带解析** —— 走库里唯一的 `parse_jsonl`,和 `agit` 的 adapter 同一份规则,
/// 于是 prompt 过滤 / isCompactSummary 排除等修复对 Hub UI 也一并生效。
fn parse_session(id: &str, jsonl: &str) -> SessionDigest {
    let ir = agit::adapter::claude_code::parse_jsonl(jsonl, id);
    // 改动文件:Write/Edit 的路径取 basename,按出现顺序去重。
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
        prompts: ir.prompts,
        texts: ir.agent_texts,
        tools: ir.tool_uses,
        files,
    }
}

// ─────────────────────────── HTTP 服务 ───────────────────────────

fn serve(root: &Path, port: u16) {
    std::fs::create_dir_all(root).ok();
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("绑定 {addr} 失败: {e}");
            std::process::exit(1);
        }
    };
    println!("AgentGitHub 运行中");
    println!("  前端:  http://localhost:{port}/");
    println!("  root:  {}", root.display());
    println!("  托管:  {} 个 agent", list_agents(root).len());

    for stream in listener.incoming() {
        if let Ok(s) = stream {
            let root = root.to_path_buf();
            std::thread::spawn(move || {
                let _ = handle(s, &root);
            });
        }
    }
}

struct Req {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// 请求体上限。git push 的 pack 可以不小,给足余量;但必须有上限 ——
/// 否则伪造的 Content-Length 会让我们预分配任意大小的 Vec,一条请求打崩整个进程。
const MAX_BODY: usize = 512 * 1024 * 1024;
/// 单行(请求行/头行)与头部总量上限,挡住无换行洪泛与 slowloris 的头部膨胀。
const MAX_LINE: u64 = 16 * 1024;
const MAX_HEADERS_BYTES: usize = 64 * 1024;

fn read_request(stream: &mut TcpStream) -> Option<Req> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    // 请求行:限长读,避免无换行的巨流把 String 撑爆内存。
    let mut line = String::new();
    (&mut reader).take(MAX_LINE).read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut headers = vec![];
    let mut content_length = 0usize;
    let mut headers_bytes = 0usize;
    loop {
        let mut h = String::new();
        (&mut reader).take(MAX_LINE).read_line(&mut h).ok()?;
        headers_bytes += h.len();
        if headers_bytes > MAX_HEADERS_BYTES {
            return None; // 头部过大
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

    if content_length > MAX_BODY {
        return None; // 声明的体超上限,拒绝(不预分配)
    }
    // 关键:按**实际到达的字节**增长缓冲,不按声明的长度预分配。
    // 配合 socket 读超时,声明 512MiB 却不发数据的 slowloris 分配不到东西就被踢。
    let mut body = Vec::new();
    if content_length > 0 {
        (&mut reader).take(content_length as u64).read_to_end(&mut body).ok()?;
        if body.len() != content_length {
            return None; // 体不完整(连接早断或超时)
        }
    }
    Some(Req { method, target, headers, body })
}

fn handle(mut stream: TcpStream, root: &Path) -> std::io::Result<()> {
    // 读/写超时:挡住"连上不发/发一半就挂"的 slowloris,每连接一线程时尤其重要。
    let t = Some(std::time::Duration::from_secs(60));
    let _ = stream.set_read_timeout(t);
    let _ = stream.set_write_timeout(t);

    let Some(req) = read_request(&mut stream) else {
        return Ok(());
    };
    let path = req.target.split('?').next().unwrap_or("/").to_string();

    // 路径穿越总闸:任何 `..` 段一律拒绝。既护住 HTML 路由的 repo_path,
    // 也护住 git_http 转给 http-backend 的 PATH_INFO。
    if path.split('/').any(|seg| seg == "..") {
        return write_response(&mut stream, 400, "text/plain; charset=utf-8", b"bad request");
    }

    // git smart-http：/<name>.git/info/refs、/<name>.git/git-(upload|receive)-pack
    if path.contains(".git/") {
        return git_http(&mut stream, root, &req);
    }

    let (status, ctype, body) = route(root, &req.method, &path, &req.target);
    write_response(&mut stream, status, ctype, body.as_bytes())
}

fn route(root: &Path, method: &str, path: &str, target: &str) -> (u16, &'static str, String) {
    if method != "GET" {
        return (405, "text/plain; charset=utf-8", "method not allowed".into());
    }
    match path {
        "/" => (200, "text/html; charset=utf-8", home_page(root)),
        "/favicon.ico" => (200, "image/svg+xml", FAVICON.into()),
        "/api/agents" => (200, "application/json", api_agents(root)),
        p if p.starts_with("/api/agent/") => {
            let name = &p["/api/agent/".len()..];
            api_agent(root, name)
        }
        // 会话摘要（只读渲染;真正合并在本地 reconcile）
        // strip_prefix/suffix 而非手算字节范围:`/agent/digest.md` 会同时匹配前后缀,
        // 旧的 `&p[7..p.len()-10]` 在这种输入上 start>end 直接 panic。
        p if p.starts_with("/agent/") && p.ends_with("/digest.md") => {
            let name = p
                .strip_prefix("/agent/")
                .and_then(|r| r.strip_suffix("/digest.md"))
                .unwrap_or("");
            let repo = repo_path(root, name);
            if !valid_agent_name(name) || !has_head(&repo) {
                (404, "text/plain; charset=utf-8", "no such agent".into())
            } else {
                (200, "text/markdown; charset=utf-8", digest_md(&repo, name))
            }
        }
        p if p.starts_with("/agent/") => {
            let name = p.strip_prefix("/agent/").unwrap_or("");
            if !valid_agent_name(name) {
                return (404, "text/plain; charset=utf-8", "no such agent".into());
            }
            let q = target.split_once('?').map(|(_, q)| q).unwrap_or("");
            (200, "text/html; charset=utf-8", agent_page(root, name, q))
        }
        _ => (404, "text/html; charset=utf-8", page("404", "<p>Not found. <a href=\"/\">← 首页</a></p>".into())),
    }
}

fn write_response(stream: &mut TcpStream, status: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

// ─────────────────────── git smart-http (sync) ───────────────────────
//
// 把请求转交 `git http-backend`（CGI），使 Hub 成为真正的 git 远端：
//   agit -a push/pull http://host:port/<name>.git 直接可用。

fn git_http(stream: &mut TcpStream, root: &Path, req: &Req) -> std::io::Result<()> {
    let (path, query) = match req.target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (req.target.clone(), String::new()),
    };
    let ctype = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();

    let mut child = match Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", &req.method)
        .env("PATH_INFO", &path)
        .env("QUERY_STRING", &query)
        .env("CONTENT_TYPE", &ctype)
        .env("CONTENT_LENGTH", req.body.len().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return write_response(stream, 500, "text/plain", "git http-backend 不可用".as_bytes()),
    };
    child.stdin.take().unwrap().write_all(&req.body).ok();
    let out = child.wait_with_output()?;

    // CGI 输出 = 头部 + 空行 + 体。原样透出，只补状态行。
    let raw = out.stdout;
    let sep = find_subslice(&raw, b"\r\n\r\n").map(|i| (i, 4)).or_else(|| find_subslice(&raw, b"\n\n").map(|i| (i, 2)));
    let (headers, body) = match sep {
        Some((i, n)) => (&raw[..i], &raw[i + n..]),
        None => (&b""[..], &raw[..]),
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\n{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        String::from_utf8_lossy(headers).replace('\n', "\r\n").trim_end(),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn find_subslice(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

// ─────────────────────────── 页面 ───────────────────────────

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn home_page(root: &Path) -> String {
    let agents = list_agents(root);
    let mut cards = String::new();
    if agents.is_empty() {
        cards.push_str("<p class=dim>还没有托管的 agent。<code>agit-hub add &lt;name&gt;</code> 新建一个。</p>");
    }
    for name in &agents {
        let repo = repo_path(root, name);
        let (n, last) = if has_head(&repo) {
            let n = sessions(&repo).len();
            let last = recent_log(&repo, 1).into_iter().next().map(|(_, s)| s).unwrap_or_default();
            (n, last)
        } else {
            (0, "（空）".into())
        };
        cards.push_str(&format!(
            "<a class=card href=\"/agent/{n}\"><div class=name>{n}</div>\
             <div class=goal>{l}</div><div class=meta>{c} 条 session</div></a>",
            n = esc(name),
            l = esc(&last),
            c = n,
        ));
    }
    let body = format!(
        "<p class=lead>面向 Agent Context 的协作平台。每个 agent 托管它的原始会话(session)。</p>\
         <div class=grid>{cards}</div>\
         <p class=dim style=\"margin-top:2rem\">API： <a href=\"/api/agents\">/api/agents</a> · \
         发布： <code>agit -a push http://HOST:PORT/&lt;name&gt;.git</code></p>"
    );
    page("AgentGitHub", body)
}

fn agent_page(root: &Path, name: &str, query: &str) -> String {
    let repo = repo_path(root, name);
    if !repo.exists() {
        return page("404", format!("<p>没有 agent <b>{}</b>。<a href=\"/\">← 首页</a></p>", esc(name)));
    }
    if !has_head(&repo) {
        return page(name, format!("<h1>{}</h1><p class=dim>还没有 session（尚未 push）。</p>", esc(name)));
    }

    let search = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("q="))
        .map(|q| q.replace('+', " "))
        .unwrap_or_default();

    let mut html = String::new();
    let mut shown = 0;
    for (id, jsonl) in sessions(&repo) {
        if !search.is_empty() && !jsonl.contains(&search) {
            continue;
        }
        shown += 1;
        let d = parse_session(&id, &jsonl);
        let mut prompts = String::new();
        for p in d.prompts.iter().take(6) {
            prompts.push_str(&format!("<li>{}</li>", esc(p.lines().next().unwrap_or(""))));
        }
        let concl = d.texts.last().map(|t| {
            let one: String = t.trim().chars().take(400).collect();
            esc(&one)
        }).unwrap_or_default();
        let files = if d.files.is_empty() { String::new() } else {
            format!("<div class=meta>改动文件：{}</div>", esc(&d.files.join(", ")))
        };
        html.push_str(&format!(
            "<div class=fact><div class=subject>会话 {id}{br}</div>\
             <div class=body>{concl}</div>\
             <ul class=evidence>{prompts}</ul>\
             {files}\
             <div class=meta>{np} 条指令 · {nt} 段回复 · {tools} 次工具调用</div></div>",
            id = esc(&d.id),
            br = if d.branch.is_empty() { String::new() } else { format!("（分支 {}）", esc(&d.branch)) },
            np = d.prompts.len(),
            nt = d.texts.len(),
            tools = d.tools,
        ));
    }
    if html.is_empty() {
        html = "<p class=dim>没有匹配的 session。</p>".into();
    }

    let mut hist = String::new();
    for (h, s) in recent_log(&repo, 12) {
        hist.push_str(&format!("<li><code>{}</code> {}</li>", esc(&h), esc(&s)));
    }

    let body = format!(
        "<div class=crumb><a href=\"/\">AgentGitHub</a> / <b>{name}</b></div>\
         <h1>{name}</h1>\
         <form class=search><input name=q placeholder=\"搜会话内容…\" value=\"{sv}\"><button>搜</button></form>\
         <div class=cols>\
           <div class=main>\
             <h2>会话 <span class=dim>({shown})</span></h2>{html}\
           </div>\
           <div class=side>\
             <h3>历史</h3><ul class=hist>{hist}</ul>\
             <h3>拉取并合并</h3><pre>agit clone \\\n  http://HOST:PORT/{name}.git\nagit -a reconcile origin/main</pre>\
             <p class=dim><a href=\"/agent/{name}/digest.md\">↓ 会话摘要</a> · <a href=\"/api/agent/{name}\">JSON</a></p>\
           </div>\
         </div>",
        name = esc(name),
        sv = esc(&search),
    );
    page(name, body)
}

/// 会话摘要 markdown（Hub 只做只读渲染，不运行 agent、不做语义合并）。
/// 真正的合并靠消费者本地 `agit -a reconcile`。
fn digest_md(repo: &Path, name: &str) -> String {
    let mut md = format!(
        "# {name} 的会话摘要（AgentGitHub · 只读渲染）\n\n\
         > 这是团队 agent 会话的摘要，供快速了解。**真正的合并请在本地：**\n\
         > `agit clone …/{name}.git && agit -a reconcile origin/main`（由 agent 读全量会话、语义合并）。\n\n"
    );
    for (id, jsonl) in sessions(repo) {
        let d = parse_session(&id, &jsonl);
        md.push_str(&format!("## 会话 {}{}\n\n", d.id, if d.branch.is_empty() { String::new() } else { format!("（{}）", d.branch) }));
        if !d.prompts.is_empty() {
            md.push_str("要它做的：\n");
            for p in d.prompts.iter().take(8) {
                md.push_str(&format!("- {}\n", p.lines().next().unwrap_or("")));
            }
        }
        if let Some(t) = d.texts.last() {
            let one: String = t.trim().chars().take(500).collect();
            md.push_str(&format!("\n结论/进展：{one}\n"));
        }
        if !d.files.is_empty() {
            md.push_str(&format!("\n改动文件：{}\n", d.files.join(", ")));
        }
        md.push('\n');
    }
    md
}

fn api_agents(root: &Path) -> String {
    let items: Vec<serde_json::Value> = list_agents(root)
        .iter()
        .map(|n| {
            let repo = repo_path(root, n);
            serde_json::json!({
                "name": n,
                "sessions": if has_head(&repo) { sessions(&repo).len() } else { 0 },
                "state_url": format!("/api/agent/{n}"),
                "git": format!("/{n}.git"),
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({ "agents": items })).unwrap_or("{}".into())
}

fn api_agent(root: &Path, name: &str) -> (u16, &'static str, String) {
    let repo = repo_path(root, name);
    if !repo.exists() {
        return (404, "application/json", "{\"error\":\"not found\"}".into());
    }
    let sess_json: Vec<serde_json::Value> = sessions(&repo)
        .iter()
        .map(|(id, jsonl)| {
            let d = parse_session(id, jsonl);
            serde_json::json!({
                "id": d.id, "branch": d.branch,
                "prompts": d.prompts, "conclusion": d.texts.last(),
                "tools": d.tools, "files": d.files,
            })
        })
        .collect();
    let v = serde_json::json!({
        "agent": name,
        "sessions": sess_json,
        "git": format!("/{name}.git"),
        "hint": "真正的合并在本地 agit -a reconcile;Hub 只做只读渲染。",
    });
    (200, "application/json", serde_json::to_string_pretty(&v).unwrap_or("{}".into()))
}

// ─────────────────────────── 外壳 / 样式 ───────────────────────────

fn page(title: &str, body: String) -> String {
    format!(
        "<!doctype html><html lang=zh><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{t} · AgentGitHub</title><link rel=icon href=/favicon.ico><style>{css}</style></head>\
         <body><header><a href=\"/\" class=logo>◆ AgentGitHub</a></header><main>{b}</main></body></html>",
        t = esc(title),
        css = CSS,
        b = body,
    )
}

const CSS: &str = "\
*{box-sizing:border-box}body{margin:0;font:15px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#1a1d24;background:#f6f7f9}\
@media(prefers-color-scheme:dark){body{color:#e6e8ec;background:#0f1115}.card,.fact{background:#181b21!important;border-color:#262b34!important}pre,code{background:#181b21!important}header{background:#0f1115!important;border-color:#262b34!important}input{background:#181b21!important;color:#e6e8ec!important;border-color:#333!important}}\
header{border-bottom:1px solid #e4e7eb;background:#fff;padding:.8rem 1.2rem}.logo{font-weight:700;color:inherit;text-decoration:none;font-size:1.05rem}\
main{max-width:1000px;margin:0 auto;padding:1.5rem 1.2rem}\
h1{font-size:1.6rem;margin:.2rem 0 1rem}h2{font-size:1.1rem;margin:1.5rem 0 .8rem}h3{font-size:.8rem;text-transform:uppercase;letter-spacing:.05em;color:#8a919c;margin:1.4rem 0 .5rem}\
a{color:#2f6fed}.dim{color:#8a919c}.lead{font-size:1.1rem;color:#5a616c}.crumb{font-size:.85rem;color:#8a919c;margin-bottom:.5rem}\
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(230px,1fr));gap:.8rem;margin-top:1rem}\
.card{display:block;padding:1rem;border:1px solid #e4e7eb;border-radius:10px;background:#fff;text-decoration:none;color:inherit;transition:.1s}\
.card:hover{border-color:#2f6fed;transform:translateY(-1px)}.card .name{font-weight:600;font-size:1.05rem}.card .goal{color:#5a616c;font-size:.9rem;margin:.3rem 0;min-height:1.2em;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.card .meta{color:#8a919c;font-size:.8rem}\
.cols{display:grid;grid-template-columns:1fr 300px;gap:2rem}@media(max-width:760px){.cols{grid-template-columns:1fr}}\
.fact{border:1px solid #e4e7eb;border-radius:10px;background:#fff;padding:1rem;margin-bottom:.8rem}\
.subject{font-family:ui-monospace,monospace;font-size:.85rem;color:#8a919c}.fact .body{font-size:1.05rem;margin:.3rem 0 .6rem}\
.evidence{list-style:none;margin:0;padding:0}.evidence li{font-size:.82rem;margin:.15rem 0}\
.tag{display:inline-block;font-size:.7rem;padding:.05rem .4rem;border-radius:4px;background:#eef1f5;color:#5a616c;margin-right:.3rem}\
.ev-code .tag{background:#e3f0e8;color:#1f7a44}.ev-doc .tag{background:#fbeecf;color:#8a5a00}.ev-human .tag{background:#e8e3f5;color:#5a3f9a}\
.fact .meta{color:#8a919c;font-size:.78rem;margin-top:.5rem}\
pre{background:#f0f2f5;padding:.7rem;border-radius:8px;overflow:auto;font-size:.82rem;white-space:pre-wrap}code{background:#f0f2f5;padding:.1rem .3rem;border-radius:4px;font-size:.85em}\
.search{margin:.5rem 0 1rem;display:flex;gap:.4rem}.search input{flex:1;padding:.5rem .7rem;border:1px solid #d5d9e0;border-radius:8px;font-size:.9rem}.search button{padding:.5rem 1rem;border:0;border-radius:8px;background:#2f6fed;color:#fff;cursor:pointer}\
.hist{list-style:none;padding:0;margin:0;font-size:.82rem}.hist li{margin:.2rem 0}";

const FAVICON: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><text y='13' font-size='13'>◆</text></svg>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_name_rejects_traversal_and_seps() {
        assert!(valid_agent_name("alice"));
        assert!(valid_agent_name("team-store_2"));
        assert!(valid_agent_name("a.b")); // 单个点合法
        assert!(!valid_agent_name(""));
        assert!(!valid_agent_name(".."));
        assert!(!valid_agent_name("../etc/passwd"));
        assert!(!valid_agent_name("a/b"));
        assert!(!valid_agent_name(".hidden")); // 前导点
        assert!(!valid_agent_name("a..b")); // 内嵌 ..
        assert!(!valid_agent_name("a\0b"));
    }
}
