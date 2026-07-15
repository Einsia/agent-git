//! agit-hub —— AgentGitHub：托管团队的 Agent Store，人可读（React SPA）、agent 可拉（JSON API）。
//!
//! 形态：一个自包含的 HTTP 服务，托管一堆 Agent Store（bare git 仓库）。
//!   - Registry：扫描 hub root 下的 <name>.git
//!   - Sync：git smart-http，`agit -a push/pull http://host:port/<name>.git` 直接可用
//!   - 鉴权：push 必须带**写 token**（关掉"谁都能推、谁都能污染"的口子）；
//!            `serve --private` 时读也要 token。见 `agit-hub token`。
//!   - 前端：hub-ui（Vite + React + Tailwind + shadcn）编译进二进制，SPA 消费下面的 JSON API。
//!   - API：/api/agents、/api/agent/<name>（分页+搜索）、/session/<id>（含 provenance/revision）、/diff。
//!
//! 前端资源在编译期由 include_str! 嵌入（hub-ui/dist）。改前端后 `cd hub-ui && npm run build` 再 cargo build。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PER_PAGE: usize = 20;
/// 带查询时最多扫多少条 session（挡住无界 git show）。超出会在响应里标记，不静默截断。
const SEARCH_SCAN_CAP: usize = 400;

// ── 编译期嵌入的前端（hub-ui/dist）──
const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/index.html"));
const APP_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/app.js"));
const APP_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/app.css"));

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("serve");
    let root = flag(&args, "--root").map(PathBuf::from).unwrap_or_else(default_root);

    match cmd {
        "serve" => {
            let port: u16 = flag(&args, "--port").and_then(|p| p.parse().ok()).unwrap_or(8177);
            let private = args.iter().any(|a| a == "--private");
            serve(&root, port, private);
        }
        "add" => match args.get(1).filter(|s| !s.starts_with("--")) {
            Some(n) => add_repo(&root, n),
            None => eprintln!("用法: agit-hub add <name>"),
        },
        "list" => {
            for a in list_agents(&root) {
                println!("{a}");
            }
        }
        "token" => token_cmd(&root, &args),
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
         agit-hub serve [--port 8177] [--private] [--root ~/.agit-hub]   启动 Hub\n\
         agit-hub add <name> [--root ...]                     新建一个 Agent Store 仓库\n\
         agit-hub list [--root ...]                           列出已托管的 agent\n\
         agit-hub token add <name> [--write|--read]           发一个访问 token（push 必须带写 token）\n\
         agit-hub token list                                  列出 token（只显示名字与权限）\n\n\
         托管的仓库是 bare git。发布： agit -a push http://HOST:PORT/<name>.git（带写 token）"
    );
}

fn default_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".agit-hub")
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

// ─────────────────────────── 鉴权（token） ───────────────────────────

struct Tok {
    name: String,
    secret: String,
    write: bool,
}

fn auth_path(root: &Path) -> PathBuf {
    root.join("auth.json")
}

fn load_tokens(root: &Path) -> Vec<Tok> {
    let Ok(text) = std::fs::read_to_string(auth_path(root)) else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return vec![];
    };
    v.get("tokens")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    Some(Tok {
                        name: t.get("name")?.as_str()?.to_string(),
                        secret: t.get("secret")?.as_str()?.to_string(),
                        write: t.get("access").and_then(|a| a.as_str()) == Some("write"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn token_cmd(root: &Path, args: &[String]) {
    match args.get(1).map(|s| s.as_str()) {
        Some("add") => {
            let Some(name) = args.get(2).filter(|s| !s.starts_with("--")) else {
                eprintln!("用法: agit-hub token add <name> [--write|--read]");
                return;
            };
            let write = !args.iter().any(|a| a == "--read");
            let secret = gen_secret();
            let mut toks = load_tokens(root);
            toks.push(Tok { name: name.clone(), secret: secret.clone(), write });
            if let Err(e) = save_tokens(root, &toks) {
                eprintln!("写 auth.json 失败: {e}");
                return;
            }
            println!("已发 token（{}）给 {name}", if write { "写" } else { "只读" });
            println!("  token: {secret}");
            println!("  这串只显示这一次。git 提示输入用户名/密码时，密码填这个 token（用户名随意）。");
        }
        Some("list") => {
            let toks = load_tokens(root);
            if toks.is_empty() {
                println!("还没有 token。`agit-hub token add <name> --write` 发一个。");
            }
            for t in toks {
                println!("{:<20} {}", t.name, if t.write { "write" } else { "read" });
            }
        }
        _ => eprintln!("用法: agit-hub token add <name> [--write|--read] | agit-hub token list"),
    }
}

fn save_tokens(root: &Path, toks: &[Tok]) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let arr: Vec<serde_json::Value> = toks
        .iter()
        .map(|t| serde_json::json!({"name": t.name, "secret": t.secret, "access": if t.write {"write"} else {"read"}}))
        .collect();
    let body = serde_json::to_string_pretty(&serde_json::json!({ "tokens": arr })).unwrap_or("{}".into());
    std::fs::write(auth_path(root), body)
}

/// 32 字节随机 → hex。优先 /dev/urandom；拿不到就退回时间+pid（弱，但至少不是常量）。
fn gen_secret() -> String {
    let mut buf = [0u8; 32];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return hex(&buf);
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hex(&nanos.to_le_bytes())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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

fn authorized(toks: &[Tok], private: bool, req: &Req, need_write: bool) -> bool {
    if !need_write && !private {
        return true;
    }
    let cands = credentials(req);
    toks.iter()
        .any(|t| (!need_write || t.write) && cands.iter().any(|c| c == &t.secret))
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

// ─────────────────────────── Registry ───────────────────────────

fn add_repo(root: &Path, name: &str) {
    if !valid_agent_name(name) {
        eprintln!("非法名字（只允许 [A-Za-z0-9._-]，禁止 .. 与前导点）: {name}");
        return;
    }
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
        let _ = Command::new("git").arg("-C").arg(&dir).args(["config", "http.receivepack", "true"]).status();
        println!("已托管 {name}  →  {}", dir.display());
        println!("发布（需写 token，见 `agit-hub token add`）：");
        println!("  agit -a remote add origin http://localhost:8177/{name}.git");
        println!("  agit -a push -u origin main");
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

/// 合法 agent 名:只允许 [A-Za-z0-9._-]，禁止 `..`、前导 `.`、路径分隔符与 NUL。
fn valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains("..")
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn repo_path(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.git"))
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

struct SessionRef {
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
        let segs: Vec<&str> = path.split('/').collect();
        if segs.len() != 3 || !path.ends_with(".jsonl") {
            continue;
        }
        out.push(SessionRef {
            runtime: segs[1].to_string(),
            id: segs[2].trim_end_matches(".jsonl").to_string(),
            path: path.to_string(),
        });
    }
    out
}

fn load_session(repo: &Path, path: &str, at: Option<&str>) -> Option<String> {
    git(repo, &["show", &format!("{}:{path}", at.unwrap_or("HEAD"))])
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
    let ir = match runtime {
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

// ─────────────────────────── HTTP 服务 ───────────────────────────

fn serve(root: &Path, port: u16, private: bool) {
    std::fs::create_dir_all(root).ok();
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("绑定 {addr} 失败: {e}");
            std::process::exit(1);
        }
    };
    let n_write = load_tokens(root).iter().filter(|t| t.write).count();
    println!("AgentGitHub 运行中");
    println!("  前端:  http://localhost:{port}/");
    println!("  root:  {}", root.display());
    println!("  托管:  {} 个 agent", list_agents(root).len());
    println!("  鉴权:  push 需写 token（{n_write} 个已配）；读{}", if private { "需 token（--private）" } else { "开放" });
    if n_write == 0 {
        println!("  ⚠ 还没有写 token —— 当前谁也不能 push。`agit-hub token add <name> --write` 发一个。");
    }

    for stream in listener.incoming().flatten() {
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            let _ = handle(stream, &root, private);
        });
    }
}

struct Req {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
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
}

const MAX_BODY: usize = 512 * 1024 * 1024;
const MAX_LINE: u64 = 16 * 1024;
const MAX_HEADERS_BYTES: usize = 64 * 1024;

fn read_request(stream: &mut TcpStream) -> Option<Req> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
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
    if content_length > MAX_BODY {
        return None;
    }
    let mut body = Vec::new();
    if content_length > 0 {
        (&mut reader).take(content_length as u64).read_to_end(&mut body).ok()?;
        if body.len() != content_length {
            return None;
        }
    }
    Some(Req { method, target, headers, body })
}

fn handle(mut stream: TcpStream, root: &Path, private: bool) -> std::io::Result<()> {
    let t = Some(std::time::Duration::from_secs(60));
    let _ = stream.set_read_timeout(t);
    let _ = stream.set_write_timeout(t);

    let Some(req) = read_request(&mut stream) else {
        return Ok(());
    };
    let path = req.target.split('?').next().unwrap_or("/").to_string();

    // 路径穿越总闸：任何 `..` 段一律拒绝。
    if path.split('/').any(|seg| seg == "..") {
        return write_response(&mut stream, 400, "text/plain; charset=utf-8", b"bad request");
    }

    let toks = load_tokens(root);

    // git smart-http：鉴权后转交 http-backend。
    if path.contains(".git/") {
        let need_write = path.ends_with("/git-receive-pack") || req.query().contains("service=git-receive-pack");
        if !authorized(&toks, private, &req, need_write) {
            return respond_401(&mut stream, need_write);
        }
        return git_http(&mut stream, root, &req);
    }

    // 前端静态资源（不鉴权也无妨，但 private 下一并要 token 更省事）。
    if private && !authorized(&toks, private, &req, false) {
        return respond_401(&mut stream, false);
    }

    let (status, ctype, body) = route(root, &req, &path);
    write_response(&mut stream, status, ctype, body.as_bytes())
}

fn respond_401(stream: &mut TcpStream, need_write: bool) -> std::io::Result<()> {
    let msg = if need_write {
        "需要写 token 才能 push。管理员用 `agit-hub token add <name> --write` 发放；git 密码处填该 token。"
    } else {
        "这个 Hub 是私有的，需要 token。git 密码处填 token；浏览器会弹出登录框。"
    };
    let head = format!(
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"agit-hub\"\r\n\
         Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        msg.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(msg.as_bytes())?;
    stream.flush()
}

fn route(root: &Path, req: &Req, path: &str) -> (u16, &'static str, String) {
    if req.method != "GET" {
        return (405, "text/plain; charset=utf-8", "method not allowed".into());
    }
    // 前端资源。
    match path {
        "/assets/app.js" => return (200, "application/javascript; charset=utf-8", APP_JS.into()),
        "/assets/app.css" => return (200, "text/css; charset=utf-8", APP_CSS.into()),
        "/favicon.ico" => return (200, "image/svg+xml", FAVICON.into()),
        _ => {}
    }
    // JSON API。
    if let Some(rest) = path.strip_prefix("/api/") {
        return api(root, req, rest);
    }
    // 其余一切 → SPA（前端自己按 URL 渲染 home/agent/session/diff）。
    (200, "text/html; charset=utf-8", INDEX_HTML.into())
}

// ─────────────────────────── JSON API ───────────────────────────

fn json(v: serde_json::Value) -> (u16, &'static str, String) {
    (200, "application/json", serde_json::to_string(&v).unwrap_or("{}".into()))
}

fn json_err(status: u16, msg: &str) -> (u16, &'static str, String) {
    (status, "application/json", serde_json::json!({ "error": msg }).to_string())
}

fn api(root: &Path, req: &Req, rest: &str) -> (u16, &'static str, String) {
    if rest == "agents" {
        return api_agents(root, req);
    }
    let Some(after) = rest.strip_prefix("agent/") else {
        return json_err(404, "not found");
    };
    // agent/<name>[/session/<id>[/diff]]
    if let Some((name, tail)) = after.split_once("/session/") {
        if !valid_agent_name(name) {
            return json_err(404, "not found");
        }
        let repo = repo_path(root, name);
        if !has_head(&repo) {
            return json_err(404, "not found");
        }
        if let Some(id) = tail.strip_suffix("/diff") {
            return api_diff(&repo, id, req.query());
        }
        return api_session(&repo, name, tail, req.query());
    }
    // agent/<name>
    if !valid_agent_name(after) {
        return json_err(404, "not found");
    }
    api_agent(root, after, req.query())
}

fn api_agents(root: &Path, req: &Req) -> (u16, &'static str, String) {
    let items: Vec<serde_json::Value> = list_agents(root)
        .iter()
        .map(|n| {
            let repo = repo_path(root, n);
            let (count, when, subject) = if has_head(&repo) {
                let (w, s) = last_activity(&repo);
                (session_refs(&repo).len(), w, s)
            } else {
                (0, String::new(), String::new())
            };
            serde_json::json!({ "name": n, "sessions": count, "when": when, "subject": subject })
        })
        .collect();
    json(serde_json::json!({ "agents": items, "host": req.host() }))
}

fn api_agent(root: &Path, name: &str, query: &str) -> (u16, &'static str, String) {
    let repo = repo_path(root, name);
    if !repo.exists() || !has_head(&repo) {
        return json_err(404, "not found");
    }
    let search = param(query, "q").map(|q| q.replace('+', " ")).unwrap_or_default();
    let pageno: usize = param(query, "page").and_then(|p| p.parse().ok()).unwrap_or(1).max(1);
    let refs = session_refs(&repo);

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

    json(serde_json::json!({
        "agent": name,
        "git": format!("/{name}.git"),
        "total": total,
        "page": pageno,
        "per_page": PER_PAGE,
        "sessions": sessions,
        "history": history,
    }))
}

fn session_summary(repo: &Path, r: &SessionRef, jsonl: &str) -> serde_json::Value {
    let d = digest(&r.runtime, &r.id, jsonl);
    let p = provenance(repo, &r.path, jsonl);
    serde_json::json!({
        "id": d.id,
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

fn api_session(repo: &Path, name: &str, id: &str, query: &str) -> (u16, &'static str, String) {
    let Some(r) = session_refs(repo).into_iter().find(|r| r.id == id) else {
        return json_err(404, "not found");
    };
    let at = param(query, "at");
    let Some(jsonl) = load_session(repo, &r.path, at.as_deref()) else {
        return json_err(404, "no such revision");
    };
    let d = digest(&r.runtime, &r.id, &jsonl);
    let p = provenance(repo, &r.path, &jsonl);
    let revisions: Vec<serde_json::Value> = session_revisions(repo, &r.path)
        .into_iter()
        .map(|(sha, when, subject)| serde_json::json!({ "sha": sha, "when": when, "subject": subject }))
        .collect();

    let _ = name;
    json(serde_json::json!({
        "id": d.id,
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

fn api_diff(repo: &Path, id: &str, query: &str) -> (u16, &'static str, String) {
    let Some(r) = session_refs(repo).into_iter().find(|r| r.id == id) else {
        return json_err(404, "not found");
    };
    let (Some(from), Some(to)) = (param(query, "from"), param(query, "to")) else {
        return json_err(400, "need from and to");
    };
    let (Some(ja), Some(jb)) = (load_session(repo, &r.path, Some(&from)), load_session(repo, &r.path, Some(&to))) else {
        return json_err(404, "no such revision");
    };
    let a = digest(&r.runtime, id, &ja);
    let b = digest(&r.runtime, id, &jb);
    json(serde_json::json!({
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

fn git_http(stream: &mut TcpStream, root: &Path, req: &Req) -> std::io::Result<()> {
    let (path, query) = match req.target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (req.target.clone(), String::new()),
    };
    let ctype = req.header("content-type").unwrap_or("").to_string();

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

const FAVICON: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><text y='13' font-size='13'>◆</text></svg>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_name_rejects_traversal_and_seps() {
        assert!(valid_agent_name("alice"));
        assert!(valid_agent_name("team-store_2"));
        assert!(valid_agent_name("a.b"));
        assert!(!valid_agent_name(""));
        assert!(!valid_agent_name(".."));
        assert!(!valid_agent_name("../etc/passwd"));
        assert!(!valid_agent_name("a/b"));
        assert!(!valid_agent_name(".hidden"));
        assert!(!valid_agent_name("a..b"));
        assert!(!valid_agent_name("a\0b"));
    }

    #[test]
    fn base64_decodes_basic_credentials() {
        assert_eq!(b64_decode("Z2l0OnNlY3JldDEyMw==").unwrap(), b"git:secret123");
        assert_eq!(b64_decode("YQ").unwrap(), b"a");
        assert_eq!(b64_decode("YWI").unwrap(), b"ab");
    }

    #[test]
    fn write_op_needs_write_token() {
        let toks = vec![
            Tok { name: "r".into(), secret: "readonly".into(), write: false },
            Tok { name: "w".into(), secret: "writekey".into(), write: true },
        ];
        let req = |auth: &str| Req {
            method: "POST".into(),
            target: "/x.git/git-receive-pack".into(),
            headers: vec![("Authorization".into(), auth.into())],
            body: vec![],
        };
        assert!(!authorized(&toks, false, &req("Bearer readonly"), true));
        assert!(authorized(&toks, false, &req("Bearer writekey"), true));
        assert!(authorized(&toks, false, &req(""), false));
        assert!(!authorized(&toks, true, &req(""), false));
        assert!(authorized(&toks, true, &req("Bearer readonly"), false));
    }
}
