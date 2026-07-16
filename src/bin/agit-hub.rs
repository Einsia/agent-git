//! agit-hub — AgentGitHub: hosts the team's Agent Stores, readable by people (React SPA) and
//! pullable by agents (JSON API).
//!
//! Shape: one self-contained HTTP service hosting a pile of Agent Stores (bare git repos).
//!   - Registry: scans <name>.git under the hub root; metadata lives in agents.json (owner/visibility/members)
//!   - Sync: git smart-http, so `agit -a push/pull http://host:port/<name>.git` just works
//!   - Identity: `aid` (`agt_<uuid>`) is minted by the client and committed to agent.toml inside the
//!               store — the Hub only reads it, never writes it. A rename does not change identity;
//!               the name on the Hub is just a mutable label.
//!   - Authz: **a separate ACL per agent** (owner + members read/write/admin + public/private).
//!            People use cookie sessions; git/scripts use tokens (bindable to a single agent,
//!            expirable, revocable). Every entry point — git smart-http included — goes through
//!            the same decision: `agit::hub::acl::decide`.
//!   - Frontend: hub-ui (Vite + React + Tailwind + shadcn) compiled into the binary; the SPA
//!               consumes the JSON API below.
//!
//! This layer only parses and shuttles HTTP; "who may do what" lives entirely in `agit::hub`
//! (pure functions + unit tests).
//!
//! Frontend assets are embedded at compile time via include_str! (hub-ui/dist). After changing the
//! frontend, run `cd hub-ui && npm run build` before cargo build.
//!
//! Security defaults (all deliberate — think it through before changing them):
//!   - Listens on 127.0.0.1 only. Going public needs an explicit `--host`, and without TLS it also
//!     needs `--insecure` before it will start.
//!   - New agents are always private. Going public is an explicit act.
//!   - Passwords use argon2id (salted), not sha256. Tokens are stored as sha256 digests only.
//!   - The real IP behind a proxy honours X-Forwarded-For only after an explicit `--trusted-proxy`.

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
/// How many sessions a query may scan at most (stops an unbounded git show). Going over is flagged
/// in the response rather than silently truncated.
const SEARCH_SCAN_CAP: usize = 400;

// ── Frontend embedded at compile time (hub-ui/dist) ──
const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/index.html"));
const APP_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/app.js"));
const APP_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/app.css"));

fn main() {
    std::process::exit(run());
}

/// Returns the process exit code — error paths must be non-zero so scripts/CI can notice the
/// failure (don't just exit 0 everywhere).
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
            eprintln!("unknown subcommand: {other}");
            print_help();
            2
        }
    }
}

fn print_help() {
    println!(
        "agit-hub — AgentGitHub (Registry + Sync)\n\n\
         agit-hub serve [--host 127.0.0.1] [--port 8177] [--root ~/.agit-hub]\n\
                        [--tls] [--insecure] [--trusted-proxy IP,IP]      start the Hub\n\
         agit-hub user add <name> [--admin]                   add a user (asks for the password)\n\
         agit-hub user list                                   list users\n\
         agit-hub add <name> [--owner <user>] [--public]      new Agent Store (private by default)\n\
         agit-hub list                                        list hosted agents\n\
         agit-hub token add <name> [--user <owner>] [--agent <name>]\n\
                            [--read|--write] [--ttl-days N]   issue an access token\n\
         agit-hub token list                                  list tokens (metadata only)\n\
         agit-hub token rm <id>                               revoke a token\n\n\
         First step: agit-hub user add <you> --admin\n\
         Hosted repos are bare git. Publish with: agit -a push http://HOST:PORT/<name>.git (with a write token)\n\n\
         Listens on 127.0.0.1 only by default. Serving the network needs --host 0.0.0.0, and without TLS also --insecure."
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

/// The first positional argument not starting with `--` (skipping the first `skip` tokens).
fn positional(args: &[String], skip: usize) -> Option<&String> {
    args.iter().skip(skip).find(|s| !s.starts_with("--"))
}

// ─────────────────────────── CLI: user ───────────────────────────

fn user_cmd(root: &Path, args: &[String]) -> i32 {
    let store = Store::new(root);
    match args.get(1).map(|s| s.as_str()) {
        Some("add") => {
            let Some(name) = positional(args, 2) else {
                eprintln!("usage: agit-hub user add <name> [--admin]");
                return 2;
            };
            let username = store::normalize_username(name);
            if !store::valid_username(&username) {
                eprintln!("invalid username (2-32 lowercase [a-z0-9._-], no leading dot): {name}");
                return 2;
            }
            if store.user(&username).is_some() {
                eprintln!("user already exists: {username}");
                return 1;
            }
            let is_admin = has_flag(args, "--admin");
            // The password is read from the tty/stdin only, **never from argv** — argv shows up in
            // ps and in shell history.
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
                    eprintln!("no system entropy available, refusing to create the user: {e}");
                    return 1;
                }
            };
            let kdf_id = kdf::current_kdf_id();
            let Some(pw_hash) = kdf::hash_password(&password, &salt, &kdf_id) else {
                eprintln!("password derivation failed (kdf={kdf_id})");
                return 1;
            };
            let user = User { username: username.clone(), pw_hash, salt, kdf: kdf_id, is_admin, created: store::now_iso() };
            if let Err(e) = store.add_user(user) {
                eprintln!("failed to write users.json: {e}");
                return 1;
            }
            audit::append(root, "cli", audit::USER_ADD, None, &format!("{username} admin={is_admin}"));
            println!("created user {username}{}", if is_admin { " (site admin)" } else { "" });
            println!("  The password is derived with argon2id and stored in users.json (0600); the plaintext never hits disk.");
            0
        }
        Some("list") => {
            let users = store.users();
            if users.is_empty() {
                println!("no users yet. `agit-hub user add <you> --admin` creates the first one.");
            }
            for u in users {
                println!("{:<20} {:<8} {}", u.username, if u.is_admin { "admin" } else { "user" }, u.created);
            }
            0
        }
        _ => {
            eprintln!("usage: agit-hub user add <name> [--admin] | agit-hub user list");
            2
        }
    }
}

/// Read a new password: on a tty, turn off echo and ask for it twice; when stdin is a pipe, read
/// one line (scripted provisioning).
fn read_new_password() -> Result<String, String> {
    let (pw, tty) = read_password("set a password for this user: ")?;
    if pw.chars().count() < 8 {
        return Err("password too short (at least 8 characters).".into());
    }
    if tty {
        let (again, _) = read_password("again: ")?;
        if again != pw {
            return Err("the two entries don't match.".into());
        }
    }
    Ok(pw)
}

/// Read one line with echo off. Returns (password, whether it was a tty).
///
/// No rpassword dependency: a single `stty -echo` is enough, and the Hub deliberately keeps its CLI
/// surface free of extra dependencies. A failing stty = stdin isn't a tty (a pipe), which wouldn't
/// have echoed anyway.
fn read_password(prompt: &str) -> Result<(String, bool), String> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let tty = stty(&["-echo"]);
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if tty {
        stty(&["echo"]);
        eprintln!(); // Echo was off, so the user's Enter never showed: emit the newline ourselves
    }
    match read {
        Ok(0) => Err("no password read (stdin ended).".into()),
        Ok(_) => Ok((line.trim_end_matches(['\n', '\r']).to_string(), tty)),
        Err(e) => Err(format!("failed to read the password: {e}")),
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

/// When `--user` / `--owner` is omitted: with exactly one user, default to them; otherwise demand it
/// be spelled out (don't guess).
fn resolve_user(store: &Store, explicit: Option<String>, flag_name: &str) -> Result<User, String> {
    if let Some(name) = explicit {
        let n = store::normalize_username(&name);
        return store.user(&n).ok_or(format!("no such user: {n} (try `agit-hub user list`)"));
    }
    let users = store.users();
    match users.len() {
        0 => Err("no users yet. Start with `agit-hub user add <you> --admin`.".into()),
        1 => Ok(users.into_iter().next().unwrap()),
        _ => Err(format!("there are several users, name one with {flag_name} <user>.")),
    }
}

// ─────────────────────────── CLI: agent ───────────────────────────

fn add_cmd(root: &Path, args: &[String]) -> i32 {
    let Some(name) = positional(args, 1) else {
        eprintln!("usage: agit-hub add <name> [--owner <user>] [--public]");
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
    // Private by default — transcripts are sensitive data, so going public must be an explicit act.
    let visibility = if has_flag(args, "--public") { Visibility::Public } else { Visibility::Private };
    match create_agent(&store, name, &owner.username, visibility) {
        Ok(created) => {
            audit::append(root, "cli", audit::AGENT_CREATE, Some(name), &format!("owner={} visibility={}", owner.username, visibility.as_str()));
            if created {
                println!("now hosting {name}  →  {}", repo_path(root, name).display());
            } else {
                println!("claimed existing repo {name}  →  {}", repo_path(root, name).display());
            }
            println!("  owner:      {}", owner.username);
            println!("  visibility: {}", visibility.as_str());
            if visibility == Visibility::Public {
                println!("  ⚠ public = anyone who can reach this port can read all of its transcripts.");
            }
            println!("Publish (needs a write token, see `agit-hub token add`):");
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

/// Create the repo + record the metadata. Returns true = newly created; false = claimed an existing
/// but unregistered old repo.
///
/// The "claim" path is for migration: root may already hold a `<name>.git` (created by an old Hub,
/// with no owner). Under the new model such a repo is **unowned and private**, invisible to everyone
/// but the site admin — claiming it is the proper way to bring it back.
fn create_agent(store: &Store, name: &str, owner: &str, visibility: Visibility) -> Result<bool, String> {
    if !valid_agent_name(name) {
        return Err(format!("invalid name ([A-Za-z0-9._-] only, no .. and no leading dot): {name}"));
    }
    if store.agent(name).is_some() {
        return Err(format!("already exists: {name}"));
    }
    let dir = store.root().join(format!("{name}.git"));
    let existed = dir.exists();
    if !existed {
        std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create the directory: {e}"))?;
        let ok = Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_dir_all(&dir);
            return Err("git init --bare failed".into());
        }
        let _ = Command::new("git").arg("-C").arg(&dir).args(["config", "http.receivepack", "true"]).status();
    }
    store
        .update_agents(|a| a.push(AgentMeta::new(name, Some(owner), visibility)))
        .map_err(|e| format!("failed to write agents.json: {e}"))?;
    Ok(!existed)
}

fn list_cmd(root: &Path) -> i32 {
    let store = Store::new(root);
    let names = list_agents(root);
    if names.is_empty() {
        println!("no agents yet. `agit-hub add <name>` creates one.");
    }
    for n in names {
        let m = store.agent_or_unowned(&n);
        let owner = m.owner.clone().unwrap_or_else(|| "— (unowned)".into());
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
                // Directory names come from outside: reject anything invalid (don't let the likes of
                // `..git` reach the router).
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
                eprintln!("usage: agit-hub token add <name> [--user <owner>] [--agent <name>] [--read|--write] [--ttl-days N]");
                return 2;
            };
            let owner = match resolve_user(&store, flag(args, "--user"), "--user") {
                Ok(u) => u,
                Err(e) => {
                    eprintln!("{e}");
                    return 2;
                }
            };
            // Read-only by default: when issuing credentials, the only acceptable direction to fail
            // in is "less power". The old CLI defaulted to write, which is backwards.
            let scope = if has_flag(args, "--write") { Scope::Write } else { Scope::Read };
            let agent = flag(args, "--agent");
            if let Some(a) = &agent {
                if !valid_agent_name(a) || store.agent(a).is_none() {
                    eprintln!("no such agent: {a}");
                    return 2;
                }
            }
            let ttl_days: Option<i64> = match flag(args, "--ttl-days") {
                Some(v) => match v.parse::<i64>() {
                    Ok(n) if n > 0 => Some(n),
                    _ => {
                        eprintln!("--ttl-days wants a positive integer");
                        return 2;
                    }
                },
                None => None,
            };
            match issue_token(&store, name, &owner.username, agent.as_deref(), scope, ttl_days) {
                Ok(secret) => {
                    audit::append(root, "cli", audit::TOKEN_CREATE, agent.as_deref(), &format!("name={name} owner={} scope={}", owner.username, scope.as_str()));
                    println!("issued a token to {} ({})", owner.username, scope.as_str());
                    println!("  token: {secret}");
                    println!("  This string is shown once (the server only stores its sha256 digest).");
                    match &agent {
                        Some(a) => println!("  Valid for agent `{a}` only."),
                        None => println!("  Valid for every agent this user can reach — narrow it with --agent <name>."),
                    }
                    match ttl_days {
                        Some(d) => println!("  Expires in {d} days."),
                        None => println!("  ⚠ Never expires — give it a deadline with --ttl-days N."),
                    }
                    println!("  When git asks for a username/password, put this token in the password field (username can be anything).");
                    println!("  Permissions are the **intersection** of the token and its owner: a token can't grant what the owner lacks.");
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
                println!("no tokens yet. `agit-hub token add <name> --write` issues one.");
            }
            let legacy = toks.iter().filter(|t| t.owner.is_none()).count();
            for t in &toks {
                let owner = t.owner.clone().unwrap_or_else(|| "—".into());
                let agent = t.agent.clone().unwrap_or_else(|| "*".into());
                let exp = t.expires.clone().unwrap_or_else(|| "never".into());
                let used = t.last_used.clone().unwrap_or_else(|| "never".into());
                let state = if t.owner.is_none() {
                    " [dead: no owner]"
                } else if t.expired() {
                    " [expired]"
                } else {
                    ""
                };
                println!("{:<18} {:<16} owner={:<12} agent={:<12} {:<6} expires={:<22} last used={}{}", t.id, t.name, owner, agent, t.scope, exp, used, state);
            }
            if legacy > 0 {
                println!();
                println!("⚠ {legacy} **old-format tokens have no owner** and are all dead.");
                println!("  They are leftovers from the old model: one token = a pass to the whole host, which can't be mapped onto \"who may see whose agent\".");
                println!("  Reissue with `agit-hub token add <name> --user <owner> [--agent <a>]`, then `token rm <id>` to drop the old ones.");
            }
            0
        }
        Some("rm") => {
            let Some(id) = positional(args, 2) else {
                eprintln!("usage: agit-hub token rm <id>  (ids come from `agit-hub token list`)");
                return 2;
            };
            match store.update_tokens(|toks| {
                let before = toks.len();
                toks.retain(|t| &t.id != id);
                before != toks.len()
            }) {
                Ok(true) => {
                    audit::append(root, "cli", audit::TOKEN_REVOKE, None, id);
                    println!("revoked {id}");
                    0
                }
                Ok(false) => {
                    eprintln!("no such token: {id}");
                    1
                }
                Err(e) => {
                    eprintln!("failed to write auth.json: {e}");
                    1
                }
            }
        }
        _ => {
            eprintln!("usage: agit-hub token add <name> [--user <owner>] [--agent <a>] [--read|--write] [--ttl-days N]");
            eprintln!("       agit-hub token list | agit-hub token rm <id>");
            2
        }
    }
}

/// Issue a token: generate a 32-byte CSPRNG plaintext and store only its sha256 digest. Returns the
/// plaintext (this once, and never again).
fn issue_token(store: &Store, name: &str, owner: &str, agent: Option<&str>, scope: Scope, ttl_days: Option<i64>) -> Result<String, String> {
    // If the CSPRNG is unavailable, **error out** — never fall back to predictable time values to
    // mint credentials.
    let secret = kdf::gen_secret().map_err(|e| format!("refusing to issue a token: {e}"))?;
    let id = store::new_token_id().map_err(|e| format!("refusing to issue a token: {e}"))?;
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
    store.update_tokens(|t| t.push(rec)).map_err(|e| format!("failed to write auth.json: {e}"))?;
    Ok(secret)
}

// ─────────────────────── git reads (bare repos) ───────────────────────

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

/// Relative time + subject of the last commit, used by the home page (cheap, a single git log).
fn last_activity(repo: &Path) -> (String, String) {
    git(repo, &["log", "-1", "--format=%cr\x1f%s"])
        .and_then(|s| s.trim().split_once('\x1f').map(|(a, b)| (a.to_string(), b.to_string())))
        .unwrap_or_default()
}

/// The agent identity inside the store. **The store itself is the authority** (agent.toml is
/// committed into its history); the Hub never mints an aid.
/// Returns (aid, source). Source values:
///   "agent.toml"   — read it
///   "none"         — the repo is still empty, or has no agent.toml (that's a freshly created repo
///                    nobody has pushed to)
///   "unidentified" — agent.toml exists but carries no agt_ identity (an old store's placeholder id)
fn agent_aid(repo: &Path) -> (Option<String>, &'static str) {
    let Some(text) = git(repo, &["show", "HEAD:agent.toml"]) else {
        return (None, "none");
    };
    match identity::parse_agent_toml(&text) {
        Identity::Aid(a) => (Some(a), "agent.toml"),
        Identity::Unidentified => (None, "unidentified"),
    }
}

/// Where one session lives in the store.
///
/// Both layouts must be recognized (the new one in the design doc carries the environment; old
/// repos don't):
///   sessions/<env>/<runtime>/<id>.jsonl   — new
///   sessions/<runtime>/<id>.jsonl         — old (env = None)
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

/// Session count and last activity per environment. environment = which code repo the session came from.
fn environments(repo: &Path, refs: &[SessionRef]) -> Vec<serde_json::Value> {
    // Keep the order (by first appearance) and note each group's directories, to scope the git log.
    let mut order: Vec<Option<String>> = vec![];
    let mut counts: HashMap<Option<String>, usize> = HashMap::new();
    let mut dirs: HashMap<Option<String>, Vec<String>> = HashMap::new();
    for r in refs {
        if !counts.contains_key(&r.env) {
            order.push(r.env.clone());
        }
        *counts.entry(r.env.clone()).or_insert(0) += 1;
        // The new layout scopes by the env directory; the old one (env=None) has no env directory,
        // so it can only be scoped by the runtime directory.
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
                    // `:(literal)` turns off pathspec globbing — directory names come from repo
                    // content and may contain `*`/`?`.
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

/// Bytes the repo occupies. git count-objects reports KiB.
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

/// Runtimes seen in the store. Alphabetical — claude-code and codex are **peers**, neither is the default.
fn runtimes(refs: &[SessionRef]) -> Vec<String> {
    let mut v: Vec<String> = refs.iter().map(|r| r.runtime.clone()).collect();
    v.sort();
    v.dedup();
    v
}

// ─────────── Session parsing (cross-runtime, through the agit lib) ───────────

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

/// A session's event spine: ordered kinds → a 'p'/'a'/'t'/'e' string (the SPA renders it as a
/// waveform). Cross-runtime via ConversationIR.
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

// ─────────────────────────── Config / context ───────────────────────────

struct Cfg {
    host: IpAddr,
    port: u16,
    /// TLS is terminated in front (reverse proxy). Effects: non-loopback binds are allowed; cookies
    /// get Secure.
    tls: bool,
    /// Listen publicly in full knowledge that there is no TLS.
    insecure: bool,
    /// IPs of trusted reverse proxies — X-Forwarded-For only counts on connections from them.
    trusted_proxies: Vec<IpAddr>,
}

/// All the shared state one request needs.
struct Ctx {
    store: Store,
    cfg: Cfg,
    sessions: Sessions,
    limiter: Arc<ConnLimiter>,
    /// Login concurrency gate: argon2 is **deliberately** slow and memory-hungry (that is exactly
    /// how it stops brute force). Without a cap, a few dozen concurrent logins = a few dozen copies
    /// of 19MiB + every core pegged, i.e. handing out an amplifier.
    login_gate: Arc<Semaphore>,
}

impl Ctx {
    fn root(&self) -> &Path {
        self.store.root()
    }
}

// ─────────────────────────── HTTP service ───────────────────────────

fn serve_cmd(root: &Path, args: &[String]) -> i32 {
    let host: IpAddr = match flag(args, "--host") {
        Some(h) => match h.parse() {
            Ok(ip) => ip,
            Err(_) => {
                eprintln!("--host wants an IP address (e.g. 127.0.0.1 / 0.0.0.0 / ::1), got: {h}");
                return 2;
            }
        },
        // Loopback only by default: the Hub holds the team's entire transcript history, and
        // "installing it exposes it to the office network" cannot be the default.
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
        println!("note: --private is no longer needed — visibility is now a **per-agent** property, and new agents are private by default.");
        println!("      To publish one agent: `agit-hub add <name> --public`, or change it in the UI.");
    }
    serve(root, Cfg { host, port, tls, insecure, trusted_proxies })
}

/// The safety gate before bind (pure function, easy to test).
///
/// A non-loopback address = other people on the network can connect. Without TLS, passwords and
/// tokens cross the wire in **plaintext** and anyone on the path can copy them. So: either --tls
/// (terminated in front) or an explicit --insecure.
fn bind_guard(host: IpAddr, tls: bool, insecure: bool) -> Result<(), String> {
    if host.is_loopback() || tls || insecure {
        return Ok(());
    }
    Err(format!(
        "refusing to listen on {host} in plaintext.\n\
         Other people on this address's network can reach it — and without TLS, login passwords and\n\
         tokens cross the wire in plaintext, so any hop on the path can copy them and then read/push\n\
         your team's entire transcript history.\n\n\
         Pick one:\n\
           - This machine only (the default): drop --host\n\
           - A TLS reverse proxy in front (nginx/caddy terminating HTTPS): add --tls, and use --trusted-proxy <proxy IP>\n\
           - Plaintext on purpose (trusted LAN/quick demo): add --insecure, you know the price now"
    ))
}

fn serve(root: &Path, cfg: Cfg) -> i32 {
    if let Err(e) = store::ensure_root(root) {
        eprintln!("failed to create root {}: {e}", root.display());
        return 1;
    }
    let addr = std::net::SocketAddr::new(cfg.host, cfg.port);
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {addr}: {e}");
            return 1;
        }
    };

    let store = Store::new(root);
    let agents = list_agents(root);
    let unowned = agents.iter().filter(|n| store.agent_or_unowned(n).owner.is_none()).count();
    let users = store.users();
    let legacy_tokens = store.tokens().iter().filter(|t| t.owner.is_none()).count();

    println!("AgentGitHub running");
    println!("  listen:  {addr}{}", if cfg.tls { " (TLS terminated in front)" } else { "" });
    println!("  web:     {}://{}/", if cfg.tls { "https" } else { "http" }, display_host(&cfg));
    println!("  root:    {}", root.display());
    println!("  hosting: {} agents ({} public)", agents.len(), agents.iter().filter(|n| store.agent_or_unowned(n).visibility == "public").count());
    println!("  users:   {} ({} admins)", users.len(), users.iter().filter(|u| u.is_admin).count());
    if !cfg.trusted_proxies.is_empty() {
        println!("  proxy:   trusting X-Forwarded-For from {:?}", cfg.trusted_proxies);
    }
    if cfg.insecure && !cfg.host.is_loopback() && !cfg.tls {
        println!("  ⚠ --insecure: listening publicly in plaintext — passwords and tokens are naked on the wire.");
    }
    if users.is_empty() {
        println!("  ⚠ not a single user — nobody can log in. Start with `agit-hub user add <you> --admin`.");
    }
    if unowned > 0 {
        println!("  ⚠ {unowned} agents have no owner (old repos): they are private, visible only to the site admin.");
        println!("    Claim them: `agit-hub add <name> --owner <user>`");
    }
    if legacy_tokens > 0 {
        println!("  ⚠ {legacy_tokens} old tokens have no owner and are **dead** (the old \"one token = the whole site\" model can't be mapped onto the new ACL).");
        println!("    Reissue: `agit-hub token add <name> --user <owner> [--agent <a>]`; `agit-hub token list` has the details.");
    }

    let ctx = Arc::new(Ctx {
        store,
        cfg,
        sessions: Sessions::new(),
        limiter: Arc::new(ConnLimiter::default()),
        login_gate: Arc::new(Semaphore::new(LOGIN_CONC)),
    });

    // Concurrency cap: a thread per connection, but capped by a semaphore — otherwise N slow
    // connections = N threads/memory, unbounded.
    let sem = Arc::new(Semaphore::new(MAX_CONN));
    for stream in listener.incoming().flatten() {
        let peer = stream.peer_addr().map(|a| a.ip()).ok();
        // Admit by IP first: when full, drop the connection outright (drop closes it) without taking
        // a global slot or spawning.
        //
        // Trusted proxies are the exception: every real user stands behind one, so counting by its
        // IP would push everyone off each other (exactly the disease requirement 10 describes).
        // For those connections the per-IP admission is deferred until XFF has been read (see handle).
        let proxied = peer.map(|ip| ctx.cfg.trusted_proxies.contains(&ip)).unwrap_or(false);
        let ipguard = match (peer, proxied) {
            (Some(ip), false) => match ctx.limiter.try_acquire(ip) {
                Some(g) => Some(g),
                None => continue,
            },
            _ => None,
        };
        let permit = Permit::acquire(sem.clone()); // At the cap this blocks accept here; the excess queues in the kernel backlog.
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // Both held until the thread ends; **even if handle panics they're returned on drop**
            // (no leaked slots/IP counts).
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

/// In-flight connection count per IP.
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

/// One held per-IP slot; decremented on drop (panic-safe).
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

/// A counting semaphore (std has none): caps the number of concurrent handler threads.
struct Semaphore {
    permits: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Self {
        Semaphore { permits: Mutex::new(n), cv: Condvar::new() }
    }
}

/// One held slot; returned on drop (panic-safe — a crashing handle still won't leak a permit).
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
/// Body cap for the JSON API. The API only takes small objects; a 512MB allowance makes no sense.
const API_MAX_BODY: usize = 64 * 1024;
const MAX_LINE: u64 = 16 * 1024;
const MAX_HEADERS_BYTES: usize = 64 * 1024;
/// Cap on concurrent handler threads (stops unbounded thread-per-connection).
const MAX_CONN: usize = 64;
/// Cap on in-flight connections from a single IP (stops one source's slowloris from filling the
/// whole pool). Half the pool, so there are slots left for everyone else.
const PER_IP_MAX: usize = 32;
/// How many argon2 runs may be in flight at once. Each argon2 wants 19MiB + a full core; uncapped =
/// an amplifier.
const LOGIN_CONC: usize = 4;
/// Overall wall-clock cap from accept to the end of the request headers (stops the 1-byte/<60s
/// slowloris drip — that would keep resetting the per-read timeout).
/// The socket read timeout during the header phase is set to it too — so a connection blocked on the
/// **request line** read is cut off here as well (the deadline is only checked at the top of the
/// header loop, which doesn't cover the request-line read).
const HEADER_DEADLINE_SECS: u64 = 20;
/// The body (a git push's pack) gets a longer read timeout: a pack streams in continuously, so this
/// only fires when it truly stalls.
const BODY_TIMEOUT_SECS: u64 = 60;
/// Overall wall-clock cap for reading an API body. 64KB of JSON has no reason to take its time
/// (stops the drip on the body).
const API_BODY_DEADLINE_SECS: u64 = 15;

/// Read the request line + headers only, **never the body**. The body stays in the reader, to be
/// streamed once authz passes and it's actually needed.
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
            return None; // Overall header-read timeout → cut it (with the concurrency cap, a slowloris drip can't hold a thread slot)
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

/// Read the API's small body. Caps both the **size** and the **total duration**.
///
/// Why count the time ourselves: a socket read timeout is **per read**, and a 1-byte-every-19-seconds
/// drip keeps resetting it — that is exactly the slowloris play, and the header read only stops it
/// thanks to an overall deadline too. git's body is a streaming pack (possibly huge and long-lived),
/// so it can only rely on the per-read timeout; an API body is 64KB and has no reason to be slow.
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
            Ok(0) => break, // The peer closed early: go with what arrived and let JSON parsing report the error
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }
    Some(out)
}

fn handle(mut stream: TcpStream, ctx: &Ctx, peer: Option<IpAddr>, proxied: bool) -> std::io::Result<()> {
    // Header phase: a short read timeout, so any blocking read (the request line included) hangs for
    // at most HEADER_DEADLINE_SECS.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(HEADER_DEADLINE_SECS)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(BODY_TIMEOUT_SECS)));

    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(req) = read_head(&mut reader) else {
        return Ok(());
    };
    let path = req.target.split('?').next().unwrap_or("/").to_string();

    // The blanket path-traversal gate: reject any `..` segment.
    if path.split('/').any(|seg| seg == "..") {
        return write_response(&mut stream, &Resp::text(400, "bad request"));
    }

    // The real client IP behind a proxy: XFF counts only when the peer is a **declared** trusted
    // proxy (anyone can forge that header).
    // These connections deliberately skipped the per-IP count at accept (otherwise everyone behind
    // the proxy would share one quota); make it up here.
    let client = peer.map(|p| net::client_ip(p, req.header("x-forwarded-for"), &ctx.cfg.trusted_proxies));
    let _client_guard = match (proxied, client) {
        (true, Some(ip)) => match ctx.limiter.try_acquire(ip) {
            Some(g) => Some(g),
            None => return write_response(&mut stream, &Resp::text(429, "too many connections")),
        },
        _ => None,
    };

    // Authentication only looks at the headers — **no body required**. That's what lets every entry
    // point establish identity first and only then decide whether to read a body at all.
    let secrets = credentials(&req);
    let authn = auth::authenticate(&ctx.store, &ctx.sessions, req.sid().as_deref(), &secrets);
    if let Some(id) = &authn.token_id {
        auth::touch_token(&ctx.store, id);
    }
    let caller = authn.caller;
    let actor = caller.user.clone().unwrap_or_else(|| "anonymous".into());

    // ── git smart-http ──
    // Work out **which agent** first, then authorize, and only then touch the body.
    // The old code here just looked at `path.contains(".git/")`, authorized site-wide in one shot,
    // and left GIT_HTTP_EXPORT_ALL=1 on — so once past the read gate, any repo under root could be
    // pulled. Now every repo is decided on its own.
    if let Some(route) = net::parse_git_path(&path, req.query()) {
        // A nonexistent agent is always decided as "unowned private", **decision first, existence
        // check second** — otherwise the difference between "doesn't exist → 404" and "private →
        // 401" is itself an interface for enumerating private agent names.
        let meta = ctx.store.agent_or_unowned(&route.agent);
        match acl::decide(&caller, &meta.to_acl(), route.action) {
            Decision::Allow => {
                // Only once the decision passes do you get to know whether it exists at all.
                if !repo_path(ctx.root(), &route.agent).exists() {
                    return write_response(&mut stream, &Resp::text(404, "no such agent"));
                }
            }
            Decision::Deny(d) => {
                audit_deny(ctx, &actor, Some(&route.agent), route.action, d);
                // A git client only prompts the user for credentials on 401 + WWW-Authenticate.
                return write_response(&mut stream, &git_deny_resp(&caller, d));
            }
        }
        // An unauthorized push already got a 401 above; its pack never reaches memory
        // (otherwise an anonymous POST with a 512MB body could blow up the process — the
        // body-before-auth memory-exhaustion DoS).
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

    // ── Everything else ──
    // The body cap tightens to API_MAX_BODY: a body here can only be small JSON. Authentication is
    // already done above.
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

/// git's denial response. Anonymous → 401 with a Basic challenge (git will ask the user for
/// credentials); authenticated but unauthorized → 404/403.
fn git_deny_resp(caller: &Caller, d: Deny) -> Resp {
    if d == Deny::Anonymous {
        return Resp::text(401, "credentials required. Put a token (`agit-hub token add`) in git's password field; the username can be anything.")
            .with("WWW-Authenticate", "Basic realm=\"agit-hub\"");
    }
    let _ = caller;
    Resp::text(403, &format!("denied: {}", d.reason()))
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

// ─────────────────────────── Responses ───────────────────────────

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
    // Frontend assets.
    if req.method == "GET" {
        match path {
            "/assets/app.js" => return Resp::new(200, "application/javascript; charset=utf-8", APP_JS.as_bytes().to_vec()),
            "/assets/app.css" => return Resp::new(200, "text/css; charset=utf-8", APP_CSS.as_bytes().to_vec()),
            "/favicon.ico" => return Resp::new(200, "image/svg+xml", FAVICON.as_bytes().to_vec()),
            _ => {}
        }
    }
    // JSON API.
    if let Some(rest) = path.strip_prefix("/api/") {
        return api(ctx, req, rest, caller, body);
    }
    if req.method != "GET" {
        return Resp::text(405, "method not allowed");
    }
    // Everything else → the SPA (the frontend renders home/agent/session/diff off the URL itself).
    // The SPA carries no data — the data all sits behind /api/*, each authorized on its own.
    Resp::new(200, "text/html; charset=utf-8", INDEX_HTML.as_bytes().to_vec())
}

// ─────────────────────────── The authorization gate ───────────────────────────

/// Record a denial. A denied anonymous read = "not logged in yet", which is noise; a denied
/// authenticated caller, or a denied write/manage action, is signal.
fn audit_deny(ctx: &Ctx, actor: &str, agent: Option<&str>, action: Action, d: Deny) {
    if actor != "anonymous" || action != Action::Read {
        audit::append(ctx.root(), actor, audit::DENIED, agent, &format!("{action:?}: {}", d.reason()));
    }
}

/// Fetch the agent + decide + produce the error response. **Every agent entry point comes through here.**
///
/// Existence is itself a secret: a nonexistent agent is decided as "unowned private", so "doesn't
/// exist" and "you can't see it" give **the same** response — otherwise the difference between
/// 401/403/404 is an interface for enumerating private agent names.
/// Existence is only checked after the decision passes (only the authorized get to know it's absent).
fn gate(ctx: &Ctx, caller: &Caller, name: &str, action: Action) -> Result<AgentMeta, Resp> {
    // A malformed name → 404. That's not a secret: it could never be a valid agent in the first place.
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
    // Someone who can read but is denied a write/manage → tell them 403 (they already know this
    // agent exists).
    // Someone who can't even read → 404, not even admitting it exists.
    let can_read = acl::decide(caller, acl, Action::Read).allowed();
    match (d, can_read) {
        (Deny::Anonymous, false) => Resp::err(401, "login required"),
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

    // agent/<name>/members[/<username>] — tail may only be empty or /<username>;
    // don't let /membersXYZ pass as /members.
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

// ── Authentication ──

fn api_login(ctx: &Ctx, req: &Req, body: &[u8]) -> Resp {
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let (Some(username), Some(password)) = (str_field(&v, "username"), str_field(&v, "password")) else {
        return Resp::err(400, "want username and password");
    };
    // argon2 is slow on purpose — leaving its concurrency uncapped hands out a CPU/memory amplifier.
    let verified = {
        let _slot = Permit::acquire(ctx.login_gate.clone());
        auth::verify_login(&ctx.store, &username, &password)
    };
    let Some(user) = verified else {
        audit::append(ctx.root(), &store::normalize_username(&username), audit::LOGIN_FAILED, None, &req.host());
        // Don't say whether the user doesn't exist or the password is wrong — that hands the
        // brute-forcer a username dictionary.
        return Resp::err(401, "wrong username or password");
    };
    let Ok(sid) = ctx.sessions.create(&user.username) else {
        return Resp::err(503, "couldn't create a session, try again shortly");
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
        None => Resp::err(401, "not logged in"),
    }
}

// ── agents ──

fn api_agents(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    let items: Vec<serde_json::Value> = list_agents(ctx.root())
        .iter()
        .filter_map(|n| {
            let meta = ctx.store.agent_or_unowned(n);
            // What you can't see doesn't make the list — the list is the first answer to "who may
            // see whose agent".
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

/// The caller's **effective** role on this agent, for the UI to decide which buttons to show.
/// null = no explicit grant (they can see it only because it's public).
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
        return Resp::err(401, "login required");
    };
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(name) = str_field(&v, "name") else {
        return Resp::err(400, "want name");
    };
    if !valid_agent_name(&name) {
        return Resp::err(400, "invalid name ([A-Za-z0-9._-] only, no .. and no leading dot)");
    }
    // No visibility given means private. **Private by default**; going public takes an explicit word.
    let visibility = match v.get("visibility").and_then(|x| x.as_str()) {
        None => Visibility::Private,
        Some(s) => match Visibility::parse(s) {
            Some(x) => x,
            None => return Resp::err(400, "visibility must be private or public"),
        },
    };
    // Creating a repo goes through the same decision: treat it as "writing to an agent I own" —
    // so a token bound to another agent, or a read-only token, can't create anything.
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
                    // An empty repo has no agent.toml yet — the aid only exists once the client
                    // pushes it. Report null honestly.
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

    // The hit set: no search = page straight through (git show only the current page); with a
    // search = scan the content (capped).
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
    // Once the aid is learned, cache it into agents.json — the authoritative value still lives only
    // in the store; this just saves a git show.
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
        return Resp::err(400, "want a JSON body");
    };
    let actor = caller.user.clone().unwrap_or_default();

    if let Some(vis) = v.get("visibility").and_then(|x| x.as_str()) {
        let Some(vis) = Visibility::parse(vis) else {
            return Resp::err(400, "visibility must be private or public");
        };
        if vis.as_str() != meta.visibility {
            let r = ctx.store.update_agents(|list| {
                if let Some(m) = list.iter_mut().find(|m| m.name == meta.name) {
                    m.visibility = vis.as_str().to_string();
                }
            });
            if r.is_err() {
                return Resp::err(500, "failed to write agents.json");
            }
            audit::append(ctx.root(), &actor, audit::AGENT_VISIBILITY, Some(&meta.name), &format!("{} → {}", meta.visibility, vis.as_str()));
        }
    }

    if let Some(newname) = str_field(&v, "name") {
        if newname != meta.name {
            if !valid_agent_name(&newname) {
                return Resp::err(400, "invalid name ([A-Za-z0-9._-] only, no .. and no leading dot)");
            }
            if repo_path(ctx.root(), &newname).exists() || ctx.store.agent(&newname).is_some() {
                return Resp::err(409, "that name is already taken");
            }
            if std::fs::rename(repo_path(ctx.root(), &meta.name), repo_path(ctx.root(), &newname)).is_err() {
                return Resp::err(500, "rename failed (the repo directory won't move)");
            }
            let r = ctx.store.update_agents(|list| {
                if let Some(m) = list.iter_mut().find(|m| m.name == meta.name) {
                    m.name = newname.clone();
                }
            });
            if r.is_err() {
                return Resp::err(500, "failed to write agents.json");
            }
            // Tokens are bound to the **name**. A rename doesn't change identity (the aid lives in
            // the store), so the bindings have to follow — otherwise one rename silently mutes every
            // CI token.
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
        return Resp::err(500, "can't remove the repo directory");
    }
    let _ = ctx.store.update_agents(|list| list.retain(|m| m.name != meta.name));
    // Tokens bound to this name must die with it: otherwise, when someone later creates an agent
    // with the same name, the old tokens would **automatically** gain rights on that new agent (the
    // name was recycled, but the token still keys off the name).
    let _ = ctx.store.update_tokens(|toks| toks.retain(|t| t.agent.as_deref() != Some(meta.name.as_str())));
    audit::append(ctx.root(), &caller.user.clone().unwrap_or_default(), audit::AGENT_DELETE, Some(&meta.name), "");
    Resp::no_content()
}

// ── members ──

fn api_members(ctx: &Ctx, caller: &Caller, name: &str, tail: &str, method: &str, body: &[u8]) -> Resp {
    let actor = caller.user.clone().unwrap_or_default();
    // GET only needs read (the member list is already shown to readers in the agent detail);
    // adding/removing needs Manage.
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
                return Resp::err(400, "want a JSON body");
            };
            let (Some(username), Some(role)) = (str_field(&v, "username"), str_field(&v, "role")) else {
                return Resp::err(400, "want username and role");
            };
            let username = store::normalize_username(&username);
            let Some(role) = Role::parse(&role) else {
                return Resp::err(400, "role must be read / write / admin");
            };
            // Only real, existing users can be added — otherwise agents.json collects a pile of
            // misspelled names, and whoever really gets that name later **automatically** inherits
            // the grant.
            if ctx.store.user(&username).is_none() {
                return Resp::err(400, "no such user");
            }
            if meta.owner.as_deref() == Some(username.as_str()) {
                return Resp::err(400, "the owner already has every right; no membership needed");
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
                return Resp::err(500, "failed to write agents.json");
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
                return Resp::err(404, "that person isn't a member");
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
        return Resp::err(401, "login required");
    };
    // You only see your own; the site admin sees them all.
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
                // Old ownerless tokens show up here for what they are (they no longer work).
                "usable": t.usable(),
            })
        })
        .collect();
    Resp::json(serde_json::json!(items))
}

fn api_create_token(ctx: &Ctx, caller: &Caller, body: &[u8]) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    // Issuing credentials requires the person's own login session: minting a token from a token
    // turns one leak into a permanent foothold (the old token expires, but the token it spawned
    // lives on).
    if caller.token.is_some() {
        return Resp::err(403, "issuing a token takes a login session; you can't mint a token from a token");
    }
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(name) = str_field(&v, "name") else {
        return Resp::err(400, "want name");
    };
    let Some(scope) = str_field(&v, "scope").and_then(|s| Scope::parse(&s)) else {
        return Resp::err(400, "scope must be read or write");
    };
    let agent = str_field(&v, "agent");
    if let Some(a) = &agent {
        // You can only issue tokens for agents you can see.
        if let Err(r) = gate(ctx, caller, a, Action::Read) {
            return r;
        }
    }
    let ttl_days = match v.get("ttl_days") {
        None | Some(serde_json::Value::Null) => None,
        Some(x) => match x.as_i64() {
            Some(n) if n > 0 && n <= 3650 => Some(n),
            _ => return Resp::err(400, "ttl_days wants an integer in 1..3650"),
        },
    };
    match issue_token(&ctx.store, &name, &user, agent.as_deref(), scope, ttl_days) {
        Ok(secret) => {
            audit::append(ctx.root(), &user, audit::TOKEN_CREATE, agent.as_deref(), &format!("name={name} scope={}", scope.as_str()));
            // The plaintext appears this once — the server keeps only the sha256 digest, which
            // nobody can turn back.
            Resp::json_status(201, serde_json::json!({ "token": secret }))
        }
        Err(e) => Resp::err(500, &e),
    }
}

fn api_revoke_token(ctx: &Ctx, caller: &Caller, id: &str) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let Some(t) = ctx.store.tokens().into_iter().find(|t| t.id == id) else {
        return Resp::err(404, "not found");
    };
    // Your own token, or the site admin.
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
        // One agent's audit log: needs Manage on that agent (owner / member admin / site admin).
        Some(name) => {
            let meta = match gate(ctx, caller, &name, Action::Manage) {
                Ok(x) => x,
                Err(r) => return r,
            };
            Resp::json(serde_json::json!(audit::query(ctx.root(), Some(&meta.name), limit)))
        }
        // The site-wide audit log: site admins only, and only from a login session (tokens can't do
        // manage actions).
        None => {
            if !caller.is_admin || caller.token.is_some() {
                return Resp::err(403, "the site-wide audit log is open to site admins only (and only from a login session)");
            }
            Resp::json(serde_json::json!(audit::query(ctx.root(), None, limit)))
        }
    }
}

// ── sessions (read access was already decided at the call site) ──

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

/// Elements in a but not in b (order-preserving, deduped, first line only).
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

/// **`name` must already be authorized before calling this** (see handle). This only shuttles bytes.
fn git_http(stream: &mut TcpStream, reader: &mut BufReader<TcpStream>, ctx: &Ctx, req: &Req, name: &str, actor: &str) -> std::io::Result<()> {
    let (path, query) = match req.target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (req.target.clone(), String::new()),
    };
    let ctype = req.header("content-type").unwrap_or("").to_string();

    // Authorized, now moving the body: relax the read timeout (a pack can be large and slow across
    // the network, but it keeps streaming in).
    let _ = stream.set_read_timeout(Some(Duration::from_secs(BODY_TIMEOUT_SECS)));

    ensure_exportable(&repo_path(ctx.root(), name));

    let mut child = match Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", ctx.root())
        // GIT_HTTP_EXPORT_ALL is **deliberately unset**: with it, http-backend serves any *.git
        // under root, so "which agent was authorized" simply doesn't exist as far as it's concerned.
        // As it stands it only honours repos marked by ensure_exportable (= the one that just passed
        // the ACL). The real gate is acl::decide above; this is only the second one.
        .env("REQUEST_METHOD", &req.method)
        .env("PATH_INFO", &path)
        .env("QUERY_STRING", &query)
        .env("CONTENT_TYPE", &ctype)
        .env("CONTENT_LENGTH", req.content_length.to_string())
        // Who pushed goes into the reflog; it also puts a person on http-backend's errors.
        .env("REMOTE_USER", actor)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return write_response(stream, &Resp::text(500, "git http-backend unavailable")),
    };

    // **Stream** the body from the socket into http-backend's stdin (no more read_to_end of the
    // whole thing into a Vec).
    let mut stdin = child.stdin.take().unwrap();
    let n = req.content_length.min(MAX_BODY) as u64;
    let _ = std::io::copy(&mut reader.by_ref().take(n), &mut stdin);
    drop(stdin); // Closing stdin sends EOF so http-backend can wrap up
    let out = child.wait_with_output()?;

    // CGI output = headers + blank line + body. Normalize the headers: pull out git's Status: as the
    // real status; drop its Content-Length (we compute our own); append exactly one CRLF per line
    // (don't turn \n→\r\n on a header that's already CRLF and produce \r\r\n).
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
                continue; // We compute our own, so avoid a duplicate
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

/// Put http-backend's export marker on **this one** repo.
///
/// Without GIT_HTTP_EXPORT_ALL, http-backend only serves repos carrying `git-daemon-export-ok`.
/// The marker is written only after authorization passes, which also brings old repos (created
/// before `agit-hub add`) along automatically.
/// Note: the marker is **not** a security boundary — it only tells http-backend "this repo is meant
/// to be served". Who may access it is decided by acl::decide, and that step already ran above.
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
        // git puts the token in the password field — treat both halves as candidates.
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

    // ── The bind gate (requirement 4) ──

    #[test]
    fn loopback_binds_without_ceremony() {
        assert!(bind_guard("127.0.0.1".parse().unwrap(), false, false).is_ok());
        assert!(bind_guard("::1".parse().unwrap(), false, false).is_ok());
    }

    #[test]
    fn public_bind_without_tls_is_refused() {
        let e = bind_guard("0.0.0.0".parse().unwrap(), false, false).unwrap_err();
        // A refusal must **say why**, not just "refused".
        assert!(e.contains("plaintext"), "{e}");
        assert!(e.contains("--insecure"), "{e}");
        assert!(e.contains("--tls"), "{e}");
        assert!(bind_guard("192.168.1.10".parse().unwrap(), false, false).is_err());
    }

    #[test]
    fn public_bind_needs_tls_or_explicit_insecure() {
        assert!(bind_guard("0.0.0.0".parse().unwrap(), true, false).is_ok(), "--tls lets it through");
        assert!(bind_guard("0.0.0.0".parse().unwrap(), false, true).is_ok(), "--insecure lets it through");
    }

    // ── Session layouts: both the new and the old must be recognized ──

    #[test]
    fn runtimes_are_sorted_peers() {
        // claude-code and codex are peers — alphabetical, neither is "first by default".
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

    // ── Which status a denial gets: the policy here is "don't leak existence" ──

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
        // A 403 admits "this agent exists" — that's an interface for enumerating private agent names.
        // Anyone who can't read gets a 404, identical to "doesn't exist".
        let r = deny_resp(&Caller::user("eve"), &private_acl(), Deny::NoGrant);
        assert_eq!(r.status, 404);
    }

    #[test]
    fn reader_denied_a_write_gets_403() {
        // Someone who can read already knows it exists; nothing left to hide — give them the real reason.
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
        // Without this header, `git clone` won't ask the user for a password, it just errors out.
        let r = git_deny_resp(&Caller::anonymous(), Deny::Anonymous);
        assert_eq!(r.status, 401);
        assert!(r.extra.iter().any(|(k, v)| k == "WWW-Authenticate" && v.contains("Basic")));
        // Don't challenge someone already authenticated — asking for the password again yields the same answer.
        let r = git_deny_resp(&Caller::user("eve"), Deny::NoGrant);
        assert_eq!(r.status, 403);
        assert!(r.extra.is_empty());
    }

    #[test]
    fn json_helpers() {
        let v = json_body(br#"{"name":" x ","empty":"","n":3}"#).unwrap();
        assert_eq!(str_field(&v, "name").as_deref(), Some("x"), "whitespace on both ends is trimmed");
        assert_eq!(str_field(&v, "empty"), None, "an empty string counts as absent");
        assert_eq!(str_field(&v, "n"), None, "a non-string counts as absent");
        assert_eq!(str_field(&v, "nope"), None);
        assert!(json_body(b"not json").is_none());
        assert!(json_body(b"").is_none());
    }
}
