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


// Pedantic markdown-in-doc-comment lint; the comment style here is deliberate.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use agit::hub::acl::{self, Action, AgentAcl, Caller, Decision, Deny, Lifecycle, Role, Scope, Visibility};
use agit::hub::identity::Identity;
use agit::hub::net::{self, valid_agent_name};
use agit::hub::session::Sessions;
use agit::hub::store::{AgentMeta, Member, Store, TokenRec, User};
use agit::hub::{audit, auth, identity, kdf, mr, session as websession, store};

const PER_PAGE: usize = 20;

/// One line about an agent, for the list. The README is where prose goes; this is a label.
const DESCRIPTION_MAX: usize = 300;

/// The largest page a caller may ask for. Asking for more is not an error worth failing on, but it
/// is not an instruction either.
const PAGE_MAX: usize = 100;

/// What a list request asked for. `limit: None` = everything.
///
/// **Pagination is opt-in.** Without `limit` these endpoints return the whole list exactly as they
/// always have, because the embedded SPA does not know what a cursor is: defaulting to a page would
/// cap its list at 20 with no way for it to ask for the rest, and a silent cap in a UI is worse than
/// an unbounded one in an API. Every response says `has_more` and `next_cursor` either way, so a
/// client can always tell which it got — that is the part that must never be silent.
struct Page {
    limit: usize,
    after: Option<String>,
}

fn page_params(query: &str) -> Result<Page, Resp> {
    let limit = match param(query, "limit") {
        None => usize::MAX,
        Some(s) => match s.parse::<usize>() {
            Ok(n) if n >= 1 => n.min(PAGE_MAX),
            _ => return Err(Resp::err(400, &format!("limit must be a whole number from 1 to {PAGE_MAX}"))),
        },
    };
    let after = match param(query, "cursor") {
        None => None,
        Some(c) => match cursor_decode(&c) {
            Some(x) => Some(x),
            None => return Err(Resp::err(400, "invalid cursor")),
        },
    };
    Ok(Page { limit, after })
}

/// An opaque resume point. Hex, rather than the key itself: what a caller gets back here is not a
/// contract, and a cursor that looks like data is an invitation to build on its shape.
fn cursor_encode(key: &str) -> String {
    key.bytes().map(|b| format!("{b:02x}")).collect()
}

fn cursor_decode(c: &str) -> Option<String> {
    if c.is_empty() || !c.is_ascii() || !c.len().is_multiple_of(2) || c.len() > 512 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..c.len()).step_by(2).map(|i| u8::from_str_radix(&c[i..i + 2], 16).ok()).collect();
    String::from_utf8(bytes?).ok()
}

/// The lifecycle/ownership verbs, so the route table can name them instead of matching strings twice.
#[derive(Clone, Copy)]
enum Verb {
    Fork,
    Transfer,
    Archive,
    Unarchive,
    Restore,
    Star,
}
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
        "pre-receive" => pre_receive_cmd(&root, &args),
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
    // The existence check and the record append run together under the agents.json lock. Split apart,
    // two concurrent creates of one name both pass the check and both append — two records, one name.
    // update_agents holds the lock across the whole closure, so checking inside it is the atomic guard.
    store
        .update_agents(|list| {
            // Covers soft-deleted agents too: their record is still here, and the name is still theirs
            // until someone purges them. Handing it out would leave the restore nowhere to land.
            if let Some(existing) = list.iter().find(|a| a.name == name) {
                return Err(match existing.lifecycle() {
                    Lifecycle::Deleted => format!("a deleted agent still holds this name: restore or purge it first: {name}"),
                    _ => format!("already exists: {name}"),
                });
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
            list.push(AgentMeta::new(name, Some(owner), visibility));
            Ok(!existed)
        })
        .map_err(|e| format!("failed to write agents.json: {e}"))?
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

// ─────────────────── server-side secret scan (pre-receive) ───────────────────
//
// The client-side hook is `agit`'s, and `git push --no-verify` skips it — by design, that flag
// exists precisely to skip local hooks. So a client hook is a **reminder**, not a gate: the only
// place a push can actually be refused is the server, and this is it. A pre-receive hook runs before
// any ref is updated, so a rejected push leaves nothing behind to clean up.
//
// The scanner is the library's (`agit::scan`), so a rule fixed for `agit` is fixed here too.

// Every bound below now **refuses** the push it cannot cover rather than waving it through, so each
// one is an outage if it trips on ordinary work. They are set to bound cost, not to be reached.

/// Blobs scanned per push. A push is a pack of arbitrary size; without a ceiling a single push can
/// keep a core busy for as long as the pusher likes.
const SCAN_MAX_BLOBS: usize = 2_000;
/// Bytes scanned per blob. Generous: a session transcript is routinely megabytes, and the scan is one
/// linear pass, so the old 1MiB ceiling bought little and refused a lot.
const SCAN_MAX_BLOB_BYTES: u64 = 16 * 1024 * 1024;
/// Bytes scanned per push, across all blobs. `cat-file --batch` is buffered whole, so this is a
/// memory ceiling before it is a time one — which is why it does not simply follow the per-blob bound
/// upwards.
const SCAN_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// The operator's escape hatch from fail-closed: paths accepted as unscannable, one per line, read
/// from the **bare repo** on the server. Same placement as the allowlist and for the same reason — a
/// file the pusher controls is not a gate, it is a form to fill in.
const SCAN_SKIP_FILE: &str = ".agit-scan-skip";
/// Bytes of a blob sniffed for NUL before calling it binary. Matches `agit::scan`'s own sniff.
const BINARY_SNIFF_BYTES: usize = 8192;
/// Shortest printable run worth scanning inside a binary blob. A credential has to survive being
/// copied through a config file or an env var, so it is printable and it is long.
const MIN_PRINTABLE_RUN: usize = 6;

/// `agit-hub pre-receive --root <root> --agent <name>` — run by git as the repo's pre-receive hook,
/// with the pushed ref updates on stdin.
///
/// Exit non-zero = the push is refused, and everything on stderr reaches the pusher's terminal.
fn pre_receive_cmd(root: &Path, args: &[String]) -> i32 {
    let Some(agent) = flag(args, "--agent") else {
        eprintln!("pre-receive: --agent is required");
        return 2;
    };
    // git runs the hook with cwd = the bare repo.
    let repo = std::env::current_dir().unwrap_or_else(|_| repo_path(root, &agent));
    let mut news = vec![];
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let mut f = line.split_whitespace();
        let (_old, new) = (f.next().unwrap_or_default(), f.next().unwrap_or_default());
        // All-zero = a deletion: nothing new arrived to scan.
        if new.is_empty() || new.bytes().all(|b| b == b'0') {
            continue;
        }
        news.push(new.to_string());
    }
    if news.is_empty() {
        return 0;
    }

    let report = scan_push(&repo, &news);
    // REMOTE_USER is set for http-backend and inherited all the way down to this hook.
    let actor = std::env::var("REMOTE_USER").unwrap_or_else(|_| "unknown".into());
    if report.findings.is_empty() && !report.incomplete() {
        return 0;
    }

    let mut detail: Vec<String> = report.findings.iter().take(20).map(|f| format!("{} in {}:{}", f.0, f.1, f.2)).collect();
    detail.extend(report.unscanned.iter().take(20).map(|(path, why)| format!("unscanned {path}: {why}")));
    if let Some(e) = &report.errored {
        detail.push(format!("scan failed: {e}"));
    }
    audit::append(
        root,
        &actor,
        audit::GIT_PUSH_REJECTED,
        Some(&agent),
        &format!(
            "secret scan: {} finding(s), {} unscanned blob(s){}; {}",
            report.findings.len(),
            report.unscanned.len(),
            if report.errored.is_some() { ", the scan itself failed" } else { "" },
            detail.join(", "),
        ),
    );

    eprintln!();
    if !report.findings.is_empty() {
        eprintln!("agit-hub: push REFUSED — {} possible secret(s) in the pushed objects.", report.findings.len());
        eprintln!();
        for (rule, path, line, excerpt) in report.findings.iter().take(20) {
            eprintln!("  {rule}  {path}:{line}");
            eprintln!("      {excerpt}");
        }
        if report.findings.len() > 20 {
            eprintln!("  ... and {} more", report.findings.len() - 20);
        }
        eprintln!();
    }
    if report.incomplete() {
        // The reason this refuses instead of warning: a gate that clears what it could not read is
        // worse than no gate, because it is trusted. One NUL byte used to buy exactly that.
        eprintln!("agit-hub: push REFUSED — this push could not be scanned in full.");
        eprintln!();
        if let Some(e) = &report.errored {
            eprintln!("  the scan itself failed: {e}");
            eprintln!("      nothing is known about ANY object in this push.");
        }
        for (path, why) in report.unscanned.iter().take(20) {
            eprintln!("  NOT SCANNED  {path}");
            eprintln!("      {why}");
        }
        if report.unscanned.len() > 20 {
            eprintln!("  ... and {} more", report.unscanned.len() - 20);
        }
        eprintln!();
        eprintln!("A push that could not be read cannot be cleared — that is what this gate is for.");
        eprintln!("If a path above is genuinely fine, add it to {} in the bare repo on the", SCAN_SKIP_FILE);
        eprintln!("server (one path per line) and it will be skipped rather than refused.");
        eprintln!();
    }
    if !report.findings.is_empty() {
        eprintln!("If a finding is wrong: add the line's literal to {} in the bare repo on the server,", agit::scan::ALLOW_FILE);
        eprintln!("or mark the line with the `{}` pragma before committing.", agit::scan::ALLOW_PRAGMA);
        eprintln!("Rewrite the history that carries the secret — and rotate it; a pushed secret is a burnt secret.");
        eprintln!();
    }
    eprintln!("Nothing was written — no ref moved. This gate is on the server, so --no-verify does not reach it.");
    eprintln!();
    1
}

struct ScanReport {
    /// (rule, path, line, excerpt)
    findings: Vec<(String, String, usize, String)>,
    /// Blobs no rule ever ran over: (path, the bound or failure that stopped it). The path is the
    /// actionable half — an operator who cannot tell which limit hit which file cannot act on either.
    unscanned: Vec<(String, String)>,
    /// The scan broke before it could reach any blob. Unlike `unscanned` there is no file to name:
    /// nothing at all is known about the push. A `bool` would do, but the message is the point.
    errored: Option<String>,
}

impl ScanReport {
    /// Anything the scan did not cover. `pre_receive_cmd` refuses on this: "found nothing" and
    /// "looked at nothing" are different claims, and only one of them clears a push.
    fn incomplete(&self) -> bool {
        !self.unscanned.is_empty() || self.errored.is_some()
    }
}

/// Scan the objects these refs bring that the repo does not already have.
///
/// `--not --all` is what keeps this proportional to the **push** rather than to the repo: during
/// pre-receive no ref has moved yet, so `--all` is the history already on the server, and the
/// difference is exactly what is arriving. Re-scanning history already accepted would make every
/// push cost the size of the repo.
fn scan_push(repo: &Path, news: &[String]) -> ScanReport {
    // The allowlist is the **server's**, read from the bare repo directory — deliberately not from
    // the pushed tree. An allowlist the pusher controls is not a gate, it is a form to fill in.
    let allow = agit::scan::Allowlist::load(repo);
    let skip = load_scan_skip(repo);
    let mut out = ScanReport { findings: vec![], unscanned: vec![], errored: None };

    let mut list_args: Vec<&str> = vec!["rev-list", "--objects"];
    for n in news {
        list_args.push(n);
    }
    list_args.push("--not");
    list_args.push("--all");
    let Some(listing) = git(repo, &list_args) else {
        out.errored = Some("`git rev-list` could not list the objects this push brings".into());
        return out;
    };

    // "<sha> [path]" — the path is git's best guess at a name for the object, which is what makes a
    // finding reportable to a human.
    let mut want: Vec<(String, String)> = vec![];
    for line in listing.lines() {
        let (sha, path) = match line.split_once(' ') {
            Some((s, p)) => (s, p),
            None => (line, ""),
        };
        if sha.len() < 7 || path.is_empty() {
            continue; // commits/tags have no path here; only blobs (and trees) do
        }
        if skip.iter().any(|s| s == path) {
            continue;
        }
        if want.len() >= SCAN_MAX_BLOBS {
            // One entry, not one per blob: the tail of an oversized push is unbounded, and naming the
            // first object past the bound is what an operator needs to act on it anyway.
            out.unscanned.push((
                path.to_string(),
                format!("this push carries more than {SCAN_MAX_BLOBS} blobs — this one and every blob after it went unscanned"),
            ));
            break;
        }
        want.push((sha.to_string(), path.to_string()));
    }
    // Scan the blob CONTENT (may be empty — a tag or a metadata-only push brings no new blobs; that must
    // NOT skip the metadata scan below, which was the bug that let a tag message through).
    scan_blob_content(repo, &want, &allow, &mut out);
    // Blobs are not the only channel a secret rides in on. A commit MESSAGE, an AUTHOR or COMMITTER
    // name/email, and an annotated TAG message all travel with the push and are readable back off the
    // server — and none of them has a path in `rev-list --objects`, so the loop above never saw them.
    // A gate that advertises "a pushed secret is a burnt secret" but scans only file content is blind to
    // three channels; verified live, an AKIA key in a commit message pushed clean.
    scan_meta(repo, news, &allow, &skip, &mut out);
    out
}

/// Scan the CONTENT of the blobs a push brings. Extracted so `scan_push` can run it and the metadata
/// scan unconditionally: a push with no new blobs (a tag, or a ref that only moves metadata) must not
/// short-circuit before the message/author/tag channels are checked.
fn scan_blob_content(repo: &Path, want: &[(String, String)], allow: &agit::scan::Allowlist, out: &mut ScanReport) {
    if want.is_empty() {
        return;
    }
    // One `cat-file --batch-check` for every candidate: types and sizes in a single process, so the
    // size bound can be applied *before* any content is read.
    let shas: String = want.iter().map(|(s, _)| format!("{s}\n")).collect();
    let Some(check) = git_stdin(repo, &["cat-file", "--batch-check"], shas.as_bytes()) else {
        out.errored.get_or_insert_with(|| "`git cat-file --batch-check` could not size the pushed objects".into());
        return;
    };
    let mut budget = SCAN_MAX_TOTAL_BYTES;
    let mut todo: Vec<(String, String)> = vec![];
    // --batch-check answers every input line with exactly one output line, so this zip stays aligned
    // even for an object git has lost (`<sha> missing`).
    for (line, (sha, path)) in String::from_utf8_lossy(&check).lines().zip(want.iter()) {
        let mut f = line.split_whitespace();
        let (_s, kind, size) = (f.next(), f.next().unwrap_or(""), f.next().unwrap_or("0"));
        if kind == "missing" {
            out.unscanned.push((path.clone(), "git no longer has this object".into()));
            continue;
        }
        if kind != "blob" {
            continue; // a tree has no content of its own to scan
        }
        let size: u64 = size.parse().unwrap_or(0);
        if size > SCAN_MAX_BLOB_BYTES {
            out.unscanned.push((path.clone(), format!("{size} bytes — past the {SCAN_MAX_BLOB_BYTES}-byte per-blob scan bound")));
            continue;
        }
        if size > budget {
            out.unscanned.push((
                path.clone(),
                format!("{size} bytes — past what is left of this push's {SCAN_MAX_TOTAL_BYTES}-byte total scan budget"),
            ));
            continue;
        }
        budget -= size;
        todo.push((sha.clone(), path.clone()));
    }
    if todo.is_empty() {
        return;
    }

    // ...and one `cat-file --batch` for the survivors' contents.
    let shas: String = todo.iter().map(|(s, _)| format!("{s}\n")).collect();
    let Some(blobs) = git_stdin(repo, &["cat-file", "--batch"], shas.as_bytes()) else {
        out.errored.get_or_insert_with(|| "`git cat-file --batch` could not read the pushed blobs".into());
        return;
    };
    // Keyed by sha, never by position: a missing object yields no body, and a positional zip would
    // then pair every later blob's content with the *previous* blob's path — and the path is the whole
    // actionable part of "rewrite the history that carries this secret".
    let bodies = parse_batch(&blobs);
    for (sha, path) in todo.iter() {
        let Some(content) = bodies.get(sha) else {
            out.unscanned.push((path.clone(), "git returned no content for this object".into()));
            continue;
        };
        // A NUL byte used to skip the blob whole and silently: `printf '\000' > f; cat key >> f` was a
        // complete bypass of this gate. Binary holds a key just as well as text does, so scan its
        // printable runs instead — and with the entropy heuristic off, which over the strings of a
        // compressed or compiled file is a false-positive generator, not a rule.
        let binary = content.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0);
        let text = match binary {
            false => String::from_utf8_lossy(content).into_owned(),
            true => printable_runs(content),
        };
        for f in agit::scan::scan_text_allow(&text, !binary, allow) {
            // For a binary blob `line` counts printable runs, not file lines — the rule and the path
            // are what the operator acts on either way.
            out.findings.push((f.rule.to_string(), path.clone(), f.line, clip(&f.excerpt, 120)));
        }
    }
}

/// Scan the metadata the push brings — commit messages, author/committer identity, tag messages — for
/// the same secrets as blob content. Reuses the blob path's batch machinery and bounds.
///
/// Entropy is OFF here on purpose: a raw commit object carries `tree`/`parent` 40-hex lines, which the
/// entropy heuristic would flag as high-entropy strings. The named rules (AKIA, `ghp_…`, and the rest)
/// do not need entropy and do not match a hex sha, so metadata is scanned by rule only.
fn scan_meta(repo: &Path, news: &[String], allow: &agit::scan::Allowlist, skip: &[String], out: &mut ScanReport) {
    // Commits the push introduces, exactly the same range as the blob scan.
    let mut list_args: Vec<&str> = vec!["rev-list"];
    for n in news {
        list_args.push(n);
    }
    list_args.push("--not");
    list_args.push("--all");
    let mut shas: Vec<String> = match git(repo, &list_args) {
        Some(listing) => listing.lines().map(str::to_string).collect(),
        None => {
            out.errored.get_or_insert_with(|| "`git rev-list` could not list the pushed commits".into());
            return;
        }
    };
    // Annotated tags carry their own message/tagger and are not reachable as commits. A ref tip that is
    // itself a tag object gets scanned too.
    for n in news {
        if let Some(check) = git(repo, &["cat-file", "-t", n]) {
            if check.trim() == "tag" {
                shas.push(n.clone());
            }
        }
    }
    if shas.is_empty() {
        return;
    }
    if shas.len() > SCAN_MAX_BLOBS {
        out.unscanned.push((
            format!("commit {}", &shas[SCAN_MAX_BLOBS][..shas[SCAN_MAX_BLOBS].len().min(12)]),
            format!("this push carries more than {SCAN_MAX_BLOBS} commits — this one and every commit after it went unscanned"),
        ));
        shas.truncate(SCAN_MAX_BLOBS);
    }

    let batch: String = shas.iter().map(|s| format!("{s}\n")).collect();
    let Some(raw) = git_stdin(repo, &["cat-file", "--batch"], batch.as_bytes()) else {
        out.errored.get_or_insert_with(|| "`git cat-file --batch` could not read the pushed commits".into());
        return;
    };
    let bodies = parse_batch(&raw);
    for sha in &shas {
        let label = format!("commit {}", &sha[..sha.len().min(12)]);
        if skip.iter().any(|s| s == &label) {
            continue;
        }
        let Some(content) = bodies.get(sha) else {
            out.unscanned.push((label, "git returned no content for this commit".into()));
            continue;
        };
        let text = String::from_utf8_lossy(content);
        for f in agit::scan::scan_text_allow(&text, false, allow) {
            out.findings.push((f.rule.to_string(), label.clone(), f.line, clip(&f.excerpt, 120)));
        }
    }
}

/// The `strings(1)` of a blob: its printable runs, one per line, so the text rules can see them.
///
/// A credential has to survive being copied through a config file, an env var or a header, so it is
/// printable ASCII by construction — the bytes around it cannot hide it.
fn printable_runs(content: &[u8]) -> String {
    let mut out = String::new();
    let mut run: Vec<u8> = vec![];
    // Tab included: an indent does not end a run a human would read as one line.
    let printable = |b: u8| (0x20..0x7f).contains(&b) || b == b'\t';
    for &b in content.iter().chain(std::iter::once(&0)) {
        match printable(b) {
            true => run.push(b),
            false => {
                if run.len() >= MIN_PRINTABLE_RUN {
                    out.push_str(&String::from_utf8_lossy(&run));
                    out.push('\n');
                }
                run.clear();
            }
        }
    }
    out
}

/// Paths the operator has accepted as unscannable. Absent file = empty list, which is the safe
/// direction: fail-closed stays closed until someone says otherwise.
fn load_scan_skip(repo: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(repo.join(SCAN_SKIP_FILE)) else {
        return vec![];
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// `git cat-file --batch` output: `<sha> <type> <size>\n<size bytes>\n`, repeated. Split on the
/// declared size rather than on newlines — blob content contains newlines, and a "missing" line has
/// no size at all.
///
/// Keyed by the sha in the header rather than returned in order: an object git has lost contributes no
/// body, so position is not identity here.
fn parse_batch(raw: &[u8]) -> HashMap<String, Vec<u8>> {
    let mut out = HashMap::new();
    let mut i = 0;
    while i < raw.len() {
        let Some(nl) = raw[i..].iter().position(|&b| b == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&raw[i..i + nl]).to_string();
        i += nl + 1;
        let mut f = header.split_whitespace();
        let Some(sha) = f.next() else {
            continue;
        };
        let Some(size) = f.nth(1).and_then(|s| s.parse::<usize>().ok()) else {
            continue; // "<sha> missing" — no content follows
        };
        let end = (i + size).min(raw.len());
        out.insert(sha.to_string(), raw[i..end].to_vec());
        i = end + 1; // the trailing newline git adds after the content
    }
    out
}

fn git_stdin(repo: &Path, args: &[&str], input: &[u8]) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input).ok()?;
    let out = child.wait_with_output().ok()?;
    Some(out.stdout)
}

/// Install the pre-receive hook into a bare repo, pointing at this very binary.
///
/// The absolute path of the running executable is baked in: the hook runs from git's environment,
/// where PATH is whatever the service inherited, and a hook that cannot find its binary is a gate
/// that silently isn't there.
fn install_pre_receive(repo: &Path, root: &Path, agent: &str) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let hook = repo.join("hooks").join("pre-receive");
    let script = format!(
        "#!/bin/sh\n\
         # Installed by agit-hub. The server-side secret gate: `git push --no-verify` skips the\n\
         # client's hook, not this one. Regenerated on demand — edit agit-hub, not this file.\n\
         exec {} pre-receive --root {} --agent {}\n",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&root.to_string_lossy()),
        shell_quote(agent),
    );
    // Rewrite whenever it differs: the binary may have moved since the repo was created, and a hook
    // pointing at a path that no longer exists fails the push rather than passing it (git treats a
    // hook that cannot execute as a failure) — but silently wrong is still worth correcting.
    if std::fs::read_to_string(&hook).ok().as_deref() == Some(script.as_str()) {
        return;
    }
    let _ = std::fs::create_dir_all(repo.join("hooks"));
    if std::fs::write(&hook, &script).is_ok() {
        let _ = std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700));
    }
}

/// Single-quote for /bin/sh. Paths come from the filesystem and the agent name is validated, but a
/// hook script is code — quoting it is not where to save a line.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ─────────────────────── git reads (bare repos) ───────────────────────

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git`, without the lossy UTF-8 conversion. Anything serving a blob's **bytes** has to use this:
/// `from_utf8_lossy` silently rewrites every invalid sequence to U+FFFD, which corrupts the file it
/// claims to be handing over.
fn git_bytes(repo: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    out.status.success().then_some(out.stdout)
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
/// Bytes of README served with the agent detail. Prose, not a book — and it rides along on a route
/// people hit constantly.
const README_MAX: usize = 64 * 1024;

/// The store's README, read out of the default ref. None = there isn't one, which is the common case
/// and not an error.
///
/// Returned as **text for a JSON field**, never as a document: it is pushed content, so it is
/// attacker-authored by definition, and the moment it is served as its own response it needs the same
/// treatment as `api_raw`. The SPA renders it; the SPA must not render it as HTML.
fn readme(repo: &Path) -> Option<String> {
    if !has_head(repo) {
        return None;
    }
    for candidate in ["README.md", "readme.md", "README"] {
        let Some(out) = git_bytes(repo, &["show", &format!("HEAD:{candidate}")]) else {
            continue;
        };
        // A binary blob called README.md is not a README; it is a way to put NULs in a JSON string.
        if out.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0) {
            return None;
        }
        let text = String::from_utf8_lossy(&out).into_owned();
        return Some(clip(&text, README_MAX));
    }
    None
}

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
    let at = at.unwrap_or("HEAD");
    // The rev arrives off the query string and is concatenated into a `<rev>:<path>` **argv slot**,
    // so a leading `-` makes git read the whole thing as an option — and `git show` has options that
    // write files (`--output=`). Checked here rather than at each caller: this is the one place the
    // value reaches git, so it is the one place that cannot be forgotten.
    if !valid_rev(at) {
        return None;
    }
    git(repo, &["show", &format!("{at}:{path}")])
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
    /// Per-token request budget (see TokenBuckets) — charges a runaway robot to its own credential
    /// rather than to whatever address it shares.
    token_rl: Arc<TokenBuckets>,
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
        token_rl: Arc::new(TokenBuckets::new()),
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

/// Per-token request budget — **a different question from the per-IP cap**, which counts *concurrent
/// connections* from one address and exists to stop a slowloris filling the thread pool. This counts
/// *requests over time* from one credential, and exists because a token is a robot: a wedged CI loop
/// or a leaked token hammers the Hub from an address that may be shared (a NAT, a proxy) with people
/// who have done nothing wrong. Keying the budget on the credential charges the right party.
///
/// A token bucket, so a normal `git clone` — a burst of requests, then nothing — is unaffected,
/// while a sustained hammer settles to the refill rate.
struct TokenBuckets {
    inner: Mutex<HashMap<String, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Sustained rate, requests/second/token. A clone or a push is a handful of requests; anything
/// steadily above this is a loop, not a person.
const TOKEN_RATE_PER_SEC: f64 = 4.0;
/// Burst allowance. Deliberately generous: a fetch of a big store fans out into many requests at
/// once, and throttling a legitimate clone would be worse than the problem being solved.
const TOKEN_BURST: f64 = 240.0;

impl TokenBuckets {
    fn new() -> TokenBuckets {
        TokenBuckets { inner: Mutex::new(HashMap::new()) }
    }

    fn allow(&self, id: &str) -> bool {
        self.allow_at(id, Instant::now())
    }

    /// The clock is a parameter so the refill can be tested without sleeping.
    ///
    /// The map only ever grows a key per **authenticated** token id, so it is bounded by the number
    /// of issued tokens — an unauthenticated flood cannot make it allocate.
    fn allow_at(&self, id: &str, now: Instant) -> bool {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let b = m.entry(id.to_string()).or_insert(Bucket { tokens: TOKEN_BURST, last: now });
        // saturating_duration_since, not `-`: Instant subtraction panics if now precedes last, and
        // two threads can read the clock out of order.
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.last = now;
        b.tokens = (b.tokens + elapsed * TOKEN_RATE_PER_SEC).min(TOKEN_BURST);
        if b.tokens < 1.0 {
            return false;
        }
        b.tokens -= 1.0;
        true
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
        // The token's own budget, charged before any work is done for it. Only reached by a token
        // that already authenticated, so an anonymous flood cannot grow the bucket map.
        if !ctx.token_rl.allow(id) {
            return write_response(
                &mut stream,
                &Resp::text(429, "this token is over its request budget; slow down (the limit is per token, not per address)")
                    .with("Retry-After", "1"),
            );
        }
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
        ("GET", "search") => return api_search(ctx, req, caller),
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

    // agent/by-aid/<aid> — identity → current name. Before the name routes, since `by-aid` is not an
    // agent name (a real one could never contain `/`).
    if let Some(aid) = after.strip_prefix("by-aid/") {
        return match m {
            "GET" => api_agent_by_aid(ctx, req, caller, aid),
            _ => Resp::text(405, "method not allowed"),
        };
    }

    // agent/<name>/mrs[/<id>[/comments|/close]]
    if let Some((name, tail)) = after.split_once("/mrs") {
        if tail.is_empty() || tail.starts_with('/') {
            return api_mrs(ctx, caller, name, tail, m, req.query(), body);
        }
    }

    // agent/<name>/raw/<path> and agent/<name>/compare — both read the store's bytes, so both go
    // through the Read gate first, like every other entry point.
    for sep in ["/raw/", "/compare"] {
        let Some((name, tail)) = after.split_once(sep) else {
            continue;
        };
        if sep == "/compare" && !tail.is_empty() {
            continue; // don't let /compareXYZ pass as /compare
        }
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
        return match sep {
            "/raw/" => api_raw(&repo, tail, req.query()),
            _ => api_compare(&repo, req.query()),
        };
    }

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

    // agent/<name>/<verb> — the lifecycle verbs. Each is its own route rather than a PATCH field:
    // they are events with their own audit rows and their own legal predecessors, not attributes.
    for (verb, handler) in [
        ("/fork", Verb::Fork),
        ("/transfer", Verb::Transfer),
        ("/archive", Verb::Archive),
        ("/unarchive", Verb::Unarchive),
        ("/restore", Verb::Restore),
        ("/star", Verb::Star),
    ] {
        if let Some(name) = after.strip_suffix(verb) {
            if m != "POST" {
                return Resp::text(405, "method not allowed");
            }
            return match handler {
                Verb::Fork => api_fork_agent(ctx, req, caller, name, body),
                Verb::Transfer => api_transfer_agent(ctx, caller, name, body),
                Verb::Archive => {
                    set_lifecycle(ctx, caller, name, Lifecycle::Archived, &[Lifecycle::Active], audit::AGENT_ARCHIVE)
                }
                Verb::Unarchive => {
                    set_lifecycle(ctx, caller, name, Lifecycle::Active, &[Lifecycle::Archived], audit::AGENT_UNARCHIVE)
                }
                // Restore lands on Active, not on "whatever it was": an agent coming back from the
                // trash writable is the surprise; coming back and needing one more click is not.
                Verb::Restore => {
                    set_lifecycle(ctx, caller, name, Lifecycle::Active, &[Lifecycle::Deleted], audit::AGENT_RESTORE)
                }
                Verb::Star => api_star_agent(ctx, caller, name, body),
            };
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
        "DELETE" => api_delete_agent(ctx, caller, after, req.query()),
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
    let page = match page_params(req.query()) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // What you can't see doesn't make the list — the list is the first answer to "who may see whose
    // agent", and it is also what makes archived agents show and deleted ones vanish, since both are
    // decided in the same place.
    //
    // Filtered before paging, never after: a page that hides its rejects would hand out short pages
    // and let a caller infer, from the gaps, exactly how many agents they cannot see.
    let visible: Vec<String> = list_agents(ctx.root())
        .into_iter()
        .filter(|n| acl::decide(caller, &ctx.store.agent_or_unowned(n).to_acl(), Action::Read).allowed())
        .filter(|n| page.after.as_deref().is_none_or(|a| n.as_str() > a))
        .collect();
    let has_more = visible.len() > page.limit;
    let window: Vec<String> = visible.into_iter().take(page.limit).collect();
    let next_cursor = has_more.then(|| window.last().map(|n| cursor_encode(n))).flatten();

    let items: Vec<serde_json::Value> = window
        .iter()
        .map(|n| {
            let meta = ctx.store.agent_or_unowned(n);
            let repo = repo_path(ctx.root(), n);
            let (count, when, subject) = if has_head(&repo) {
                let (w, s) = last_activity(&repo);
                (session_refs(&repo).len(), w, s)
            } else {
                (0, String::new(), String::new())
            };
            let (aid, aid_source) = agent_aid(&repo);
            serde_json::json!({
                "name": n,
                "aid": aid,
                "aid_source": aid_source,
                "sessions": count,
                "when": when,
                "subject": subject,
                "visibility": meta.visibility,
                "lifecycle": meta.lifecycle().as_str(),
                "description": meta.description,
                "forked_from": meta.forked_from,
                "stars": meta.stars.len(),
                "starred": caller.user.as_ref().is_some_and(|u| meta.stars.contains(u)),
                "role": effective_role(caller, &meta),
            })
        })
        .collect();
    Resp::json(serde_json::json!({
        "agents": items,
        "host": req.host(),
        "has_more": has_more,
        "next_cursor": next_cursor,
    }))
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
    let hypothetical =
        AgentAcl { name: name.clone(), owner: Some(user.clone()), visibility, lifecycle: Lifecycle::Active, members: vec![] };
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

    let (aid, aid_source, aid_status) = sync_aid(ctx, meta, &caller.user.clone().unwrap_or_else(|| "anonymous".into()));

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
        "aid_status": aid_status,
        "clone_url": clone_url(ctx, req, name),
        "visibility": meta.visibility,
        "lifecycle": meta.lifecycle().as_str(),
        "description": meta.description,
        "forked_from": meta.forked_from,
        "readme": readme(&repo),
        "stars": meta.stars.len(),
        "starred": caller.user.as_ref().is_some_and(|u| meta.stars.contains(u)),
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
        // With a search, `total` counts the hits among the sessions actually scanned — so say how
        // many that was, and whether the cap cut it short. The count alone cannot tell you.
        "scanned": if search.is_empty() { refs.len() } else { refs.len().min(SEARCH_SCAN_CAP) },
        "scan_cap": SEARCH_SCAN_CAP,
        "scan_capped": !search.is_empty() && refs.len() > SEARCH_SCAN_CAP,
        "sessions": sessions,
        "history": history,
    }))
}

/// Read the store's identity, reconcile it with what agents.json has cached, and act on the verdict.
/// Returns `(aid, source, status)` for the response.
///
/// **The store is the authority** — the Hub never mints an aid, it only remembers what it read (the
/// cache exists so `by-aid` and the agent list don't have to `git show` every repo). The reconciling
/// itself is `identity::reconcile`, a pure function with the awkward cases pinned down in tests; all
/// this does is the IO around it.
///
/// `status`: "ok" | "learned" | "replaced" | "conflict".
fn sync_aid(ctx: &Ctx, meta: &AgentMeta, actor: &str) -> (Option<String>, &'static str, &'static str) {
    let repo = repo_path(ctx.root(), &meta.name);
    let (seen, source) = agent_aid(&repo);

    // Nothing to decide and nothing to write: the store said nothing this time, or it said what the
    // cache already holds. Taking the lock on every read of every agent would make a GET a file write.
    if seen.is_none() || seen == meta.aid {
        return match (seen, meta.aid.clone()) {
            (Some(a), _) => (Some(a), source, "ok"),
            // The store didn't say this time (empty repo / unreadable HEAD) — report what the Hub
            // remembers, and label it as the cache rather than passing it off as a fresh read.
            (None, Some(a)) => (Some(a), "cache", "ok"),
            (None, None) => (None, source, "ok"),
        };
    }
    // A fork reads its source's aid on **every** read, forever, since the clone carries the source's
    // agent.toml and `reconcile` rightly refuses to cache an aid someone else holds — so `meta.aid`
    // stays None and the check above can never short-circuit it. Left to fall through, that made a
    // routine read of a routine fork take the lock and write an `agent.aid.conflict` row every time.
    // Mirrors reconcile's lineage rule, which stays the authority; this only avoids taking a lock to
    // be told what cannot have changed (`forked_from_aid` is fixed at fork time).
    if seen.is_some() && seen == meta.forked_from_aid {
        return (seen, source, "inherited");
    }

    // Past here the verdict can write, so reading the cache, looking up the holder and writing must be
    // ONE critical section. Looking the holder up outside the lock was a TOCTOU: two concurrent syncs
    // of two stores carrying the same aid could both see no holder, both Learn, and both write —
    // breaking the invariant `Store::agent_by_aid` leans on, that the first match is the only match.
    let name = meta.name.clone();
    let mut verdict = identity::AidVerdict::Unchanged;
    // Whether this read is the one that *entered* the conflict, as opposed to the millionth to
    // observe it. Only the transition is an event; see `AgentMeta::aid_conflict`.
    let mut newly_conflicted = false;
    // The cache write stays best-effort, as before: the store is the authority, so a verdict whose
    // write failed is still the truth about what was read, and the next sync reconciles again.
    let _ = ctx.store.update_agents(|list| {
        let cached = list.iter().find(|m| m.name == name).and_then(|m| m.aid.clone());
        let holder = seen
            .as_deref()
            .and_then(|a| list.iter().find(|m| m.aid.as_deref() == Some(a)))
            .map(|m| m.name.clone());
        let lineage = list.iter().find(|m| m.name == name).and_then(|m| m.forked_from_aid.clone());
        verdict = identity::reconcile(&name, cached.as_deref(), seen.as_deref(), holder.as_deref(), lineage.as_deref());
        let Some(m) = list.iter_mut().find(|m| m.name == name) else {
            return;
        };
        match &verdict {
            identity::AidVerdict::Learn(a) | identity::AidVerdict::Replaced { now: a, .. } => {
                m.aid = Some(a.clone());
                // Whatever collision was reported is over: this agent now holds an aid of its own,
                // so the next one deserves a fresh alert.
                m.aid_conflict = None;
            }
            identity::AidVerdict::Conflict { aid, .. } => {
                newly_conflicted = m.aid_conflict.as_deref() != Some(aid.as_str());
                m.aid_conflict = Some(aid.clone());
            }
            identity::AidVerdict::Inherited { .. } | identity::AidVerdict::Unchanged => {}
        }
    });

    match verdict {
        // Re-read under the lock, the cache already agreed — the race this section exists to close.
        identity::AidVerdict::Unchanged => match seen {
            Some(a) => (Some(a), source, "ok"),
            None => (None, source, "ok"),
        },
        identity::AidVerdict::Learn(a) => {
            audit::append(ctx.root(), actor, audit::AGENT_AID_LEARNED, Some(&meta.name), &a);
            (Some(a), source, "learned")
        }
        identity::AidVerdict::Replaced { was, now } => {
            // The store is the authority, so the cache follows it — but the response only says
            // "replaced" this once, and the audit log is what makes it still findable tomorrow.
            audit::append(ctx.root(), actor, audit::AGENT_AID_REPLACED, Some(&meta.name), &format!("{was} → {now}"));
            (Some(now), source, "replaced")
        }
        identity::AidVerdict::Conflict { aid, held_by } => {
            // **Only on the transition.** A conflict is a state, re-derived on every read; auditing
            // each observation grew audit.log without bound and buried the one row an operator
            // alerts on under thousands of copies of itself — so polling a conflicted agent became a
            // way to drown out the alert that names you.
            if newly_conflicted {
                audit::append(
                    ctx.root(),
                    actor,
                    audit::AGENT_AID_CONFLICT,
                    Some(&meta.name),
                    &format!("{aid} is already held by {held_by}"),
                );
            }
            // Deliberately does **not** name the other agent in the response: the caller may have no
            // permission to know it exists, and "which name holds this aid" is exactly what the
            // by-aid endpoint gates.
            (Some(aid), source, "conflict")
        }
        // Expected, so it is not an event: a fork carries its source's agent.toml until it is
        // rebound. No audit row, and no cache — the source keeps the aid.
        identity::AidVerdict::Inherited { aid, .. } => (Some(aid), source, "inherited"),
    }
}

/// `GET /api/agent/by-aid/<aid>` — the identity → current name lookup.
///
/// This is what makes a rename safe: a `.agit.toml` records the **aid**, and asks here for whatever
/// name that memory currently answers to. Routes through the normal gate on the resolved agent, so
/// an aid is not an oracle for the existence of agents you cannot read.
fn api_agent_by_aid(ctx: &Ctx, req: &Req, caller: &Caller, aid: &str) -> Resp {
    if !identity::is_aid(aid) {
        return Resp::err(400, "not an aid (want agt_<id>)");
    }
    // Unresolvable and unreadable must look the same, for the same reason gate() hides existence:
    // otherwise this endpoint enumerates the private agents by aid instead of by name.
    let Some(meta) = ctx.store.agent_by_aid(aid) else {
        return Resp::err(404, "not found");
    };
    let meta = match gate(ctx, caller, &meta.name, Action::Read) {
        Ok(x) => x,
        Err(r) => return r,
    };
    Resp::json(serde_json::json!({
        "aid": aid,
        "name": meta.name,
        "clone_url": clone_url(ctx, req, &meta.name),
        "visibility": meta.visibility,
        "owner": meta.owner,
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

    // `{"description": ""}` clears it — an explicit empty string is a real instruction, and the only
    // way to take a description back off.
    if let Some(d) = v.get("description").and_then(|x| x.as_str()) {
        let d = match mr::bounded(d, DESCRIPTION_MAX) {
            Ok(x) => x,
            Err(e) => return Resp::err(400, &format!("description {e}")),
        };
        let r = ctx.store.update_agents(|list| {
            if let Some(m) = list.iter_mut().find(|m| m.name == meta.name) {
                m.description = d.clone();
            }
        });
        if r.is_err() {
            return Resp::err(500, "failed to write agents.json");
        }
        audit::append(ctx.root(), &actor, audit::AGENT_DESCRIBE, Some(&meta.name), d.as_deref().unwrap_or("(cleared)"));
    }

    if let Some(newname) = str_field(&v, "name") {
        if newname != meta.name {
            if !valid_agent_name(&newname) {
                return Resp::err(400, "invalid name ([A-Za-z0-9._-] only, no .. and no leading dot)");
            }
            if name_taken(ctx, &newname) {
                return Resp::err(409, "that name is already taken");
            }
            // Reserve the new name atomically — check and rename the record together under the lock, so
            // two renames to one name can't both land. Done BEFORE moving the repo dir, so a lost race
            // fails before touching the filesystem. (The `name_taken` above is only a fast fail.)
            //
            // A rename is a metadata edit, not a new identity: only the label moves. The aid is
            // deliberately untouched (it lives in the store's agent.toml), so everything keyed on
            // identity survives.
            let reserved = ctx.store.update_agents(|list| {
                if list.iter().any(|m| m.name == newname) {
                    return false;
                }
                if let Some(m) = list.iter_mut().find(|m| m.name == meta.name) {
                    m.name = newname.clone();
                }
                true
            });
            match reserved {
                Ok(true) => {}
                Ok(false) => return Resp::err(409, "that name is already taken"),
                Err(_) => return Resp::err(500, "failed to write agents.json"),
            }
            // Move the repo dir to match the record. On failure, roll the name back so the record and
            // the directory never disagree.
            if std::fs::rename(repo_path(ctx.root(), &meta.name), repo_path(ctx.root(), &newname)).is_err() {
                let _ = ctx.store.update_agents(|list| {
                    if let Some(m) = list.iter_mut().find(|m| m.name == newname) {
                        m.name = meta.name.clone();
                    }
                });
                return Resp::err(500, "rename failed (the repo directory won't move)");
            }
            // Tokens are bound to the **name**. A rename doesn't change identity (the aid lives in
            // the store), so the bindings have to follow — otherwise one rename silently mutes every
            // CI token.
            let _ = ctx.store.update_tokens(|toks| {
                for t in toks.iter_mut().filter(|t| t.agent.as_deref() == Some(meta.name.as_str())) {
                    t.agent = Some(newname.clone());
                }
            });
            // MR endpoints carry both aid and name; the names are labels and have to follow too.
            let _ = ctx.store.rename_in_mrs(&meta.name, &newname);
            audit::append(ctx.root(), &actor, audit::AGENT_RENAME, Some(&newname), &format!("{} → {newname}", meta.name));
            // Echo the aid back: the whole point of the rename being safe is that identity did not
            // move, and a caller should be able to see that rather than take it on faith.
            return Resp::json(serde_json::json!({ "name": newname, "renamed_from": meta.name, "aid": meta.aid }));
        }
    }

    let fresh = ctx.store.agent_or_unowned(&meta.name);
    Resp::json(serde_json::json!({ "name": fresh.name, "visibility": fresh.visibility, "owner": fresh.owner }))
}

/// Is this name spoken for? **Includes soft-deleted agents**, whose whole point is that the name
/// stays theirs: hand it to someone else and the restore has nowhere to land, while every token and
/// `.agit.toml` still pointing at the name silently starts addressing a stranger's agent.
fn name_taken(ctx: &Ctx, name: &str) -> bool {
    ctx.store.agent(name).is_some() || repo_path(ctx.root(), name).exists()
}

/// Move an agent between lifecycle states. The state itself is enforced in `acl::decide` — this only
/// writes it down.
fn set_lifecycle(ctx: &Ctx, caller: &Caller, name: &str, to: Lifecycle, from: &[Lifecycle], action: &'static str) -> Resp {
    let meta = match gate(ctx, caller, name, Action::Manage) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Refusing the no-op transition is what makes each of these verbs mean something: "restore" on a
    // live agent is a caller who thinks it was deleted, and answering 204 would agree with them.
    if !from.contains(&meta.lifecycle()) {
        return Resp::err(409, &format!("this agent is {}", meta.lifecycle().as_str()));
    }
    let r = ctx.store.update_agents(|list| {
        if let Some(m) = list.iter_mut().find(|m| m.name == meta.name) {
            m.lifecycle = to.as_str().to_string();
        }
    });
    if r.is_err() {
        return Resp::err(500, "failed to write agents.json");
    }
    let actor = caller.user.clone().unwrap_or_default();
    audit::append(ctx.root(), &actor, action, Some(&meta.name), &format!("{} → {}", meta.lifecycle().as_str(), to.as_str()));
    Resp::json(serde_json::json!({ "name": meta.name, "lifecycle": to.as_str(), "aid": meta.aid }))
}

/// `DELETE /api/agent/<name>` — **soft**. The repo, the tokens, the MRs and the name all survive; the
/// agent simply stops being findable (`acl::decide` denies everything but Manage on a deleted agent).
///
/// Destroying the bytes is `?purge=true`, and only from here — two steps, because the one-step version
/// of this is how a memory nobody meant to lose gets lost.
fn api_delete_agent(ctx: &Ctx, caller: &Caller, name: &str, query: &str) -> Resp {
    if param(query, "purge").as_deref() == Some("true") {
        return api_purge_agent(ctx, caller, name);
    }
    set_lifecycle(ctx, caller, name, Lifecycle::Deleted, &[Lifecycle::Active, Lifecycle::Archived], audit::AGENT_DELETE)
}

/// The irreversible one: the bytes go. Only reachable for an already soft-deleted agent, so nothing
/// live can be destroyed by a single mistyped verb.
fn api_purge_agent(ctx: &Ctx, caller: &Caller, name: &str) -> Resp {
    let meta = match gate(ctx, caller, name, Action::Manage) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if meta.lifecycle() != Lifecycle::Deleted {
        return Resp::err(409, "purge only empties the trash: delete this agent first, then purge it");
    }
    if std::fs::remove_dir_all(repo_path(ctx.root(), &meta.name)).is_err() {
        return Resp::err(500, "can't remove the repo directory");
    }
    let _ = ctx.store.update_agents(|list| list.retain(|m| m.name != meta.name));
    // Tokens bound to this name must die with it: otherwise, when someone later creates an agent
    // with the same name, the old tokens would **automatically** gain rights on that new agent (the
    // name was recycled, but the token still keys off the name).
    let _ = ctx.store.update_tokens(|toks| toks.retain(|t| t.agent.as_deref() != Some(meta.name.as_str())));
    // Same reasoning for MRs targeting it: a recycled name must not inherit the old agent's reviews.
    let _ = ctx.store.update_mrs(|mrs| mrs.retain(|m| m.target.agent != meta.name));
    audit::append(ctx.root(), &caller.user.clone().unwrap_or_default(), audit::AGENT_PURGE, Some(&meta.name), "");
    Resp::no_content()
}

/// Fork: a new agent, **owned by the caller**, carrying the source's history.
///
/// Two things this deliberately does not do.
///
/// It does not copy the source's members. A fork is not a way to hand your collaborators an agent
/// they were never granted — the fork's ACL starts from the forker alone, and everyone else has to be
/// invited to it the normal way.
///
/// It does not copy the aid into the fork's metadata. The cloned store still *contains* the source's
/// agent.toml, so the fork wears the source's identity until someone rebinds it locally
/// (`agit a rebind`) and pushes — until then `sync_aid` reports it as a conflict and refuses to cache
/// it, which is exactly right: two agents may never share one aid, and the Hub does not mint them.
fn api_fork_agent(ctx: &Ctx, req: &Req, caller: &Caller, name: &str, body: &[u8]) -> Resp {
    // You cannot fork what you cannot read — otherwise fork is an oracle for private agents, and a
    // way to walk off with one.
    let source = match gate(ctx, caller, name, Action::Read) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    // A fork is a write the caller performs, so a read-only token must not get to do it.
    if caller.token.as_ref().is_some_and(|t| t.scope != Scope::Write) {
        audit_deny(ctx, &user, Some(name), Action::Write, Deny::TokenScope);
        return Resp::err(403, Deny::TokenScope.reason());
    }
    let fork = match json_body(body).as_ref().and_then(|v| str_field(v, "name")) {
        Some(n) => n,
        None => format!("{}-fork", source.name),
    };
    if !valid_agent_name(&fork) {
        return Resp::err(400, "invalid name ([A-Za-z0-9._-] only, no .. and no leading dot)");
    }
    if name_taken(ctx, &fork) {
        return Resp::err(409, "that name is already taken");
    }
    let dst = repo_path(ctx.root(), &fork);
    let ok = Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(repo_path(ctx.root(), &source.name))
        .arg(&dst)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_dir_all(&dst);
        return Resp::err(500, "git clone --bare failed");
    }
    let _ = Command::new("git").arg("-C").arg(&dst).args(["config", "http.receivepack", "true"]).status();
    // A bare clone records its origin. The fork is its own agent on its own disk — leaving a remote
    // pointing at the source would make its `--not --all` scan bound, and its pushes routable,
    // through somebody else's repo.
    let _ = Command::new("git").arg("-C").arg(&dst).args(["remote", "remove", "origin"]).status();
    install_pre_receive(&dst, ctx.root(), &fork);

    // The identity the clone carries. Recorded as lineage so `identity::reconcile` can tell this
    // fork's inherited aid from a stolen one — see `AgentMeta::forked_from_aid`. Read from the source
    // repo rather than from `source.aid`, which is only the Hub's cache and may not have been
    // populated yet.
    let (src_aid, _) = agent_aid(&repo_path(ctx.root(), &source.name));
    // Private by default, whatever the source was: forking a public agent is not a decision to
    // publish your copy of it.
    // Authoritative name check, atomic with the insert. The `name_taken` above is only a fast fail; a
    // fork that raced us to this name between there and here must not produce a second record.
    let r = ctx.store.update_agents(|list| {
        if list.iter().any(|a| a.name == fork) {
            return false;
        }
        list.push(AgentMeta {
            forked_from: Some(source.name.clone()),
            forked_from_aid: src_aid.clone(),
            description: source.description.clone(),
            ..AgentMeta::new(&fork, Some(&user), Visibility::Private)
        });
        true
    });
    match r {
        Ok(true) => {}
        Ok(false) => {
            let _ = std::fs::remove_dir_all(&dst);
            return Resp::err(409, "that name is already taken");
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(&dst);
            return Resp::err(500, "failed to write agents.json");
        }
    }
    audit::append(ctx.root(), &user, audit::AGENT_FORK, Some(&fork), &format!("forked from {}", source.name));
    let (aid, aid_source) = agent_aid(&dst);
    Resp::json_status(
        201,
        serde_json::json!({
            "name": fork,
            "forked_from": source.name,
            "owner": user,
            "visibility": Visibility::Private.as_str(),
            "clone_url": clone_url(ctx, req, &fork),
            // The identity the *clone* carries, which is still the source's. Reported, never cached:
            // `by-aid` keeps resolving to the source until this fork is rebound.
            //
            // "inherited", not "conflict": a fork wearing its source's aid is the expected state, and
            // giving it the same word as a real collision is what teaches an operator to ignore the
            // word. An empty source has no aid to inherit, so there is nothing to report.
            "aid": aid,
            "aid_source": aid_source,
            "aid_status": match aid.is_some() {
                true => "inherited",
                false => "ok",
            },
            "note": match aid.is_some() {
                true => Some("this fork carries the source's aid; give it its own identity with `agit a rebind --new-id` locally, then push"),
                false => None,
            },
        }),
    )
}

/// Star / unstar, per user. `{"starred": false}` unstars; the default is to star.
///
/// Gated at Read, not Write: starring is a bookmark the *caller* keeps, and needing write access to
/// bookmark something would make the feature useless for exactly the agents worth bookmarking. It
/// still writes hub state, so it takes an identity and refuses a read-only token, same as an MR
/// comment (see `mutation_actor`).
fn api_star_agent(ctx: &Ctx, caller: &Caller, name: &str, body: &[u8]) -> Resp {
    let meta = match gate(ctx, caller, name, Action::Read) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let actor = match mutation_actor(ctx, caller, name) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let on = json_body(body).and_then(|v| v.get("starred").and_then(|x| x.as_bool())).unwrap_or(true);
    let who = actor.clone();
    let r = ctx.store.update_agents(|list| {
        if let Some(m) = list.iter_mut().find(|m| m.name == meta.name) {
            m.stars.retain(|u| u != &who);
            if on {
                m.stars.push(who.clone());
            }
        }
    });
    if r.is_err() {
        return Resp::err(500, "failed to write agents.json");
    }
    audit::append(ctx.root(), &actor, audit::AGENT_STAR, Some(&meta.name), if on { "starred" } else { "unstarred" });
    let fresh = ctx.store.agent_or_unowned(&meta.name);
    Resp::json(serde_json::json!({ "name": meta.name, "starred": on, "stars": fresh.stars.len() }))
}

/// Transfer ownership. The aid does not move — a transfer is a metadata edit, exactly like a rename:
/// the memory is the same memory, it just answers to someone else now.
///
/// **The previous owner keeps nothing.** No membership row is left behind for them, so on a private
/// agent they lose read access at the same moment, and their name-bound tokens stop working. That is
/// the honest reading of "transfer", and the alternative — quietly leaving the old owner an admin
/// grant — hands the new owner an agent that someone else still controls without saying so. The way
/// back is for the new owner to add them, or for the site admin to step in; both are deliberate acts
/// by someone who still has the rights, which is the property worth keeping.
fn api_transfer_agent(ctx: &Ctx, caller: &Caller, name: &str, body: &[u8]) -> Resp {
    let meta = match gate(ctx, caller, name, Action::Manage) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(to) = json_body(body).as_ref().and_then(|v| str_field(v, "to")) else {
        return Resp::err(400, "want to (the username to transfer ownership to)");
    };
    let to = store::normalize_username(&to);
    // Only a real, existing user — the same rule members follow, and for the same reason: an agent
    // owned by a name nobody holds is an agent whoever registers that name later inherits.
    if ctx.store.user(&to).is_none() {
        return Resp::err(400, &format!("no such user: {to}"));
    }
    if meta.owner.as_deref() == Some(to.as_str()) {
        return Resp::err(409, &format!("{to} already owns this agent"));
    }
    let (from, target) = (meta.owner.clone(), to.clone());
    let r = ctx.store.update_agents(|list| {
        if let Some(m) = list.iter_mut().find(|m| m.name == meta.name) {
            m.owner = Some(target.clone());
            // The new owner's membership row, if any, is now noise at best and a demotion at worst
            // (owner outranks every role) — drop it rather than leave two answers to "what may they
            // do".
            m.members.retain(|x| x.username != target);
        }
    });
    if r.is_err() {
        return Resp::err(500, "failed to write agents.json");
    }
    let actor = caller.user.clone().unwrap_or_default();
    audit::append(
        ctx.root(),
        &actor,
        audit::AGENT_TRANSFER,
        Some(&meta.name),
        &format!("{} → {to}", from.as_deref().unwrap_or("(unowned)")),
    );
    Resp::json(serde_json::json!({
        "name": meta.name,
        "owner": to,
        "previous_owner": from,
        // The point of a transfer being safe is that identity did not move. Say so, rather than
        // leaving the caller to take it on faith.
        "aid": meta.aid,
    }))
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

// ── cross-agent search ──

/// Sessions scanned across the whole query, all agents together. Each one costs a `git show` plus a
/// parse, so this — not the agent count — is the thing worth bounding.
const XSEARCH_SCAN_CAP: usize = 400;
/// Hits returned. Past this the scan stops early: nobody reads hit 200, and the work is real.
const XSEARCH_MAX_HITS: usize = 50;

/// `GET /api/search?q=` — one query across **every agent the caller may read**, over the fields
/// people actually remember: what they asked, what came back, which files were touched.
///
/// The permission is per agent and decided by `acl::decide`, exactly like everywhere else: an agent
/// you cannot read contributes nothing, and cannot even be inferred from a hit count.
fn api_search(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    let q = param(req.query(), "q").map(|q| q.replace('+', " ")).unwrap_or_default();
    let q = q.trim().to_lowercase();
    if q.len() < 2 {
        return Resp::err(400, "want q, at least 2 characters");
    }
    let mut hits: Vec<serde_json::Value> = vec![];
    let mut scanned = 0usize;
    let mut capped = false;

    'agents: for name in list_agents(ctx.root()) {
        let meta = ctx.store.agent_or_unowned(&name);
        if !acl::decide(caller, &meta.to_acl(), Action::Read).allowed() {
            continue;
        }
        let repo = repo_path(ctx.root(), &name);
        if !has_head(&repo) {
            continue;
        }
        for r in session_refs(&repo) {
            if scanned >= XSEARCH_SCAN_CAP || hits.len() >= XSEARCH_MAX_HITS {
                capped = true;
                break 'agents;
            }
            scanned += 1;
            let Some(jsonl) = load_session(&repo, &r.path, None) else {
                continue;
            };
            let d = digest(&r.runtime, &r.id, &jsonl);
            let conclusion = d.texts.last().cloned().unwrap_or_default();
            // Where it matched is worth reporting: "in a prompt" and "in a filename" are different
            // memories, and the UI can say which.
            let mut fields = vec![];
            if d.prompts.iter().any(|p| p.to_lowercase().contains(&q)) {
                fields.push("prompt");
            }
            if conclusion.to_lowercase().contains(&q) {
                fields.push("conclusion");
            }
            if d.files.iter().any(|f| f.to_lowercase().contains(&q)) {
                fields.push("file");
            }
            if fields.is_empty() {
                continue;
            }
            hits.push(serde_json::json!({
                "agent": name,
                "aid": meta.aid,
                "id": d.id,
                "env": r.env,
                "runtime": r.runtime,
                "matched": fields,
                "title": d.prompts.first().map(|s| first_line(s)).unwrap_or_default(),
                "conclusion": clip(&conclusion, 200),
                "files": d.files.iter().filter(|f| f.to_lowercase().contains(&q)).take(5).cloned().collect::<Vec<_>>(),
            }));
        }
    }

    Resp::json(serde_json::json!({
        "q": q,
        "hits": hits,
        // `total` is the number of hits **found**, and `scan_capped` says whether that is the whole
        // story. Reporting a capped count as if it were the total is the lie this flag exists to
        // stop.
        "total": hits.len(),
        "scanned": scanned,
        "scan_capped": capped,
        "scan_cap": XSEARCH_SCAN_CAP,
    }))
}

// ── merge requests ──

/// `/api/agent/<name>/mrs...` — the MR routes, keyed on the **target** agent (that is the memory
/// being changed, so that is the ACL that governs).
///
///   POST   mrs               open one                     [Write on the target]
///   GET    mrs               list                         [Read]
///   GET    mrs/<id>          detail + transcript          [Read]
///   POST   mrs/<id>/comments comment                      [Read on the target + `mutation_actor`]
///   POST   mrs/<id>/close    close / record it as merged  [Write]
///
/// Opening needs Write because an MR is a proposal against that memory; commenting only needs Read,
/// since anyone who may read the review may take part in it. That tier is about *who may join the
/// discussion* — it is not a claim that a comment is not a write, so every POST here additionally
/// clears `mutation_actor`. Nothing here merges anything: see the module docs on `agit::hub::mr`.
fn api_mrs(ctx: &Ctx, caller: &Caller, name: &str, tail: &str, method: &str, query: &str, body: &[u8]) -> Resp {
    // The action this route needs, decided **before** the agent is fetched, so the gate is the first
    // thing that happens on every path.
    let action = match (method, tail) {
        ("GET", _) => Action::Read,
        ("POST", "") => Action::Write,
        ("POST", t) if t.ends_with("/comments") => Action::Read,
        ("POST", t) if t.ends_with("/close") => Action::Write,
        _ => return Resp::text(405, "method not allowed"),
    };
    let meta = match gate(ctx, caller, name, action) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Every POST below mutates hub state, whichever tier `gate` authorized it at.
    let actor = match method {
        "POST" => match mutation_actor(ctx, caller, name) {
            Ok(a) => a,
            Err(r) => return r,
        },
        _ => caller.user.clone().unwrap_or_default(),
    };

    match (method, tail) {
        ("GET", "") => api_mr_list(ctx, caller, &meta, query),
        ("POST", "") => api_mr_open(ctx, caller, &meta, &actor, body),
        _ => {
            // mrs/<id> | mrs/<id>/comments | mrs/<id>/close
            let rest = match tail.strip_prefix('/') {
                Some(r) => r,
                None => return Resp::err(404, "not found"),
            };
            let (id, sub) = match rest.split_once('/') {
                Some((i, s)) => (i, s),
                None => (rest, ""),
            };
            let Ok(id) = id.parse::<usize>() else {
                return Resp::err(404, "not found");
            };
            match (method, sub) {
                ("GET", "") => api_mr_detail(ctx, caller, &meta, id),
                ("POST", "comments") => api_mr_comment(ctx, &meta, id, &actor, body),
                ("POST", "close") => api_mr_close(ctx, caller, &meta, id, &actor, body),
                _ => Resp::text(405, "method not allowed"),
            }
        }
    }
}

/// The identity every MR mutation must have, and the token cap that `gate` could not apply.
///
/// Commenting is deliberately gated at `Action::Read` — anyone who may read a review may take part in
/// it, read-members included — but a comment is still a **write of hub state**, and that carries two
/// requirements the agent tier does not:
///
///   - It must be attributable. Anonymous clears the Read tier on a public agent (acl.rs rule 5), and
///     would otherwise author a comment as the empty string: a mutation attributed to nobody.
///   - A read-only token must never write, whoever holds it. `acl::decide` caps tokens on
///     `Action::Write`, so a route gated at Read never reaches that rule — see acl.rs's
///     `read_token_never_writes_even_for_the_owner`. The cap is an intersection, not a maximum, so it
///     has to be applied where the write actually happens.
fn mutation_actor(ctx: &Ctx, caller: &Caller, name: &str) -> Result<String, Resp> {
    let Some(actor) = caller.user.clone() else {
        audit_deny(ctx, "anonymous", Some(name), Action::Write, Deny::Anonymous);
        return Err(Resp::err(401, "login required"));
    };
    if caller.token.as_ref().is_some_and(|t| t.scope != Scope::Write) {
        audit_deny(ctx, &actor, Some(name), Action::Write, Deny::TokenScope);
        return Err(Resp::err(403, Deny::TokenScope.reason()));
    }
    Ok(actor)
}

/// The list view: no transcripts. They are the big field, and nobody reading an index wants every
/// merge dialogue on the agent shipped along with it.
fn api_mr_list(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, query: &str) -> Resp {
    let page = match page_params(query) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // Ids climb and `mrs_for` sorts by them, so the id of the last row is a resume point that
    // survives an MR being opened or deleted underneath the caller — which an offset would not.
    let after: Option<usize> = match page.after.as_deref().map(|a| a.parse::<usize>()) {
        None => None,
        Some(Ok(n)) => Some(n),
        Some(Err(_)) => return Resp::err(400, "invalid cursor"),
    };
    let all: Vec<mr::Mr> =
        ctx.store.mrs_for(&meta.name).into_iter().filter(|m| after.is_none_or(|a| m.id > a)).collect();
    let has_more = all.len() > page.limit;
    let window: Vec<mr::Mr> = all.into_iter().take(page.limit).collect();
    let next_cursor = has_more.then(|| window.last().map(|m| cursor_encode(&m.id.to_string()))).flatten();

    let items: Vec<serde_json::Value> = window
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "title": m.title,
                "author": m.author,
                "state": m.state,
                "created": m.created,
                "updated": m.updated,
                "source": mr_endpoint_json(ctx, caller, &m.source),
                "target": mr_endpoint_json(ctx, caller, &m.target),
                "comments": m.comments.len(),
                "has_transcript": m.dialogue_transcript.is_some() && can_read_agent(ctx, caller, &m.source.agent),
            })
        })
        .collect();
    Resp::json(serde_json::json!({
        "agent": meta.name,
        "mrs": items,
        "has_more": has_more,
        "next_cursor": next_cursor,
    }))
}

fn can_read_agent(ctx: &Ctx, caller: &Caller, agent: &str) -> bool {
    acl::decide(caller, &ctx.store.agent_or_unowned(agent).to_acl(), Action::Read).allowed()
}

/// Serialize one endpoint **for this reader**, not for the person who opened the MR.
///
/// An MR's source is a different agent with its own ACL, and the opener's permission is not the
/// audience's: alice may open an MR from a private agent into a public one, and from then on everyone
/// who can read the *target* reads the object. Deciding again per reader is what keeps `gate`'s rule —
/// existence is itself a secret — true of the MR views too; checking only the opener leaves the name,
/// aid and ref of a private agent readable by anonymous.
fn mr_endpoint_json(ctx: &Ctx, caller: &Caller, e: &mr::Endpoint) -> serde_json::Value {
    if !can_read_agent(ctx, caller, &e.agent) {
        return serde_json::json!({ "aid": null, "agent": null, "ref": null, "redacted": true });
    }
    serde_json::json!({ "aid": e.aid, "agent": e.agent, "ref": e.git_ref })
}

fn mr_json(ctx: &Ctx, caller: &Caller, m: &mr::Mr) -> serde_json::Value {
    // The transcript is the dialogue `agit a merge` held *between the two sides*, so it quotes the
    // source by construction — a reader who may not know the source exists may not read it either.
    // Withheld whole rather than filtered: there is no reliable way to strip one agent's voice out of
    // free text, and a partial redaction that looks complete is worse than an honest absence.
    let show_source = can_read_agent(ctx, caller, &m.source.agent);
    serde_json::json!({
        "id": m.id,
        "title": m.title,
        "author": m.author,
        "state": m.state,
        "created": m.created,
        "updated": m.updated,
        "source": mr_endpoint_json(ctx, caller, &m.source),
        "target": mr_endpoint_json(ctx, caller, &m.target),
        "dialogue_transcript": if show_source { m.dialogue_transcript.clone() } else { None },
        "transcript_redacted": !show_source && m.dialogue_transcript.is_some(),
        "comments": m.comments.iter().map(|c| serde_json::json!({
            "id": c.id,
            "author": c.author,
            "body": c.body,
            "created": c.created,
        })).collect::<Vec<_>>(),
    })
}

fn api_mr_open(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, actor: &str, body: &[u8]) -> Resp {
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(title) = str_field(&v, "title") else {
        return Resp::err(400, "want title");
    };
    let title = match mr::bounded(&title, mr::TITLE_MAX) {
        Ok(Some(t)) => t,
        Ok(None) => return Resp::err(400, "want title"),
        Err(e) => return Resp::err(400, &format!("title {e}")),
    };
    let Some(source_name) = str_field(&v, "source") else {
        return Resp::err(400, "want source (the agent the change is coming from)");
    };
    // The source is a real agent on this Hub, and **the caller must be able to read it**: an MR
    // carries the source's identity and ref into an object other people will read, so proposing from
    // an agent you cannot see would leak that it exists.
    let source = match gate(ctx, caller, &source_name, Action::Read) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if source.name == meta.name {
        return Resp::err(400, "an agent cannot open a merge request against itself");
    }
    let source_ref = str_field(&v, "source_ref").unwrap_or_else(|| "main".into());
    let target_ref = str_field(&v, "target_ref").unwrap_or_else(|| "main".into());
    for r in [&source_ref, &target_ref] {
        if !valid_ref_name(r) {
            return Resp::err(400, "invalid ref name");
        }
    }
    // The transcript `agit a merge` produced. Optional: an MR may be opened before the dialogue is
    // run, and it can be filled in later by comment. Bounded, and never truncated silently.
    let transcript = match v.get("dialogue_transcript").and_then(|x| x.as_str()) {
        None => None,
        Some(t) => match mr::bounded(t, mr::TRANSCRIPT_MAX) {
            Ok(x) => x,
            Err(e) => return Resp::err(413, &format!("dialogue_transcript {e}")),
        },
    };

    let open_now = ctx.store.mrs_for(&meta.name).iter().filter(|m| m.is_open()).count();
    if open_now >= mr::OPEN_MAX {
        return Resp::err(429, &format!("this agent already has {} open merge requests", mr::OPEN_MAX));
    }

    // Snapshot both identities now. Names get renamed; the aid is what still says, a year later,
    // which two memories this review was actually between.
    let src_aid = sync_aid(ctx, &source, actor).0;
    let tgt_aid = sync_aid(ctx, meta, actor).0;
    let now = store::now_iso();
    let rec = ctx.store.update_mrs(|mrs| {
        let id = mr::next_id(mrs, &meta.name);
        let rec = mr::Mr {
            id,
            source: mr::Endpoint { aid: src_aid.clone(), agent: source.name.clone(), git_ref: source_ref.clone() },
            target: mr::Endpoint { aid: tgt_aid.clone(), agent: meta.name.clone(), git_ref: target_ref.clone() },
            title: title.clone(),
            author: actor.to_string(),
            state: mr::State::Open.as_str().to_string(),
            created: now.clone(),
            updated: now.clone(),
            dialogue_transcript: transcript.clone(),
            comments: vec![],
        };
        mrs.push(rec.clone());
        rec
    });
    let Ok(rec) = rec else {
        return Resp::err(500, "failed to write mrs.json");
    };
    audit::append(
        ctx.root(),
        actor,
        audit::MR_OPEN,
        Some(&meta.name),
        &format!("#{} {} ← {}:{}", rec.id, title, source.name, source_ref),
    );
    Resp::json_status(201, mr_json(ctx, caller, &rec))
}

fn api_mr_detail(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, id: usize) -> Resp {
    match ctx.store.mrs_for(&meta.name).into_iter().find(|m| m.id == id) {
        Some(m) => Resp::json(mr_json(ctx, caller, &m)),
        None => Resp::err(404, "not found"),
    }
}

fn api_mr_comment(ctx: &Ctx, meta: &AgentMeta, id: usize, actor: &str, body: &[u8]) -> Resp {
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(text) = str_field(&v, "body") else {
        return Resp::err(400, "want body");
    };
    let text = match mr::bounded(&text, mr::COMMENT_MAX) {
        Ok(Some(t)) => t,
        Ok(None) => return Resp::err(400, "want body"),
        Err(e) => return Resp::err(400, &format!("body {e}")),
    };
    let target = meta.name.clone();
    let out = ctx.store.update_mrs(|mrs| {
        let Some(m) = mrs.iter_mut().find(|m| m.target.agent == target && m.id == id) else {
            return Err(Resp::err(404, "not found"));
        };
        // A settled MR is a record. Reopening the discussion on it would quietly edit history that
        // someone already acted on.
        if !m.is_open() {
            return Err(Resp::err(409, &format!("this merge request is {}", m.state)));
        }
        if m.comments_full() {
            return Err(Resp::err(429, &format!("this merge request already has {} comments", mr::COMMENTS_MAX)));
        }
        let c = mr::Comment { id: m.next_comment_id(), author: actor.to_string(), body: text.clone(), created: store::now_iso() };
        m.comments.push(c.clone());
        m.updated = store::now_iso();
        Ok(c)
    });
    match out {
        Ok(Ok(c)) => {
            audit::append(ctx.root(), actor, audit::MR_COMMENT, Some(&meta.name), &format!("#{id} comment {}", c.id));
            Resp::json_status(201, serde_json::json!({ "id": c.id, "author": c.author, "body": c.body, "created": c.created }))
        }
        Ok(Err(r)) => r,
        Err(_) => Resp::err(500, "failed to write mrs.json"),
    }
}

/// Close an MR, or record that it was merged.
///
/// `{"state": "merged"}` does **not** merge anything — it records that someone ran `agit a merge`
/// locally and pushed the result. The Hub has no model and no working tree; claiming otherwise here
/// would be the lie that turns this object into a fake engine.
fn api_mr_close(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, id: usize, actor: &str, body: &[u8]) -> Resp {
    let state = match json_body(body).as_ref().and_then(|v| str_field(v, "state")) {
        None => mr::State::Closed,
        Some(s) => match mr::State::parse(&s) {
            Some(x) if !x.is_open() => x,
            // "open" here would be a reopen, which is a different verb on a different route.
            _ => return Resp::err(400, "state must be closed or merged"),
        },
    };
    let target = meta.name.clone();
    let out = ctx.store.update_mrs(|mrs| {
        let Some(m) = mrs.iter_mut().find(|m| m.target.agent == target && m.id == id) else {
            return Err(Resp::err(404, "not found"));
        };
        if !m.is_open() {
            return Err(Resp::err(409, &format!("this merge request is already {}", m.state)));
        }
        m.state = state.as_str().to_string();
        m.updated = store::now_iso();
        Ok(m.clone())
    });
    match out {
        Ok(Ok(m)) => {
            let action = if state == mr::State::Merged { audit::MR_MERGED } else { audit::MR_CLOSE };
            audit::append(ctx.root(), actor, action, Some(&meta.name), &format!("#{id} {}", state.as_str()));
            Resp::json(mr_json(ctx, caller, &m))
        }
        Ok(Err(r)) => r,
        Err(_) => Resp::err(500, "failed to write mrs.json"),
    }
}

/// A revision the caller is allowed to name: a sha, a branch, a tag.
///
/// Same shape as a ref name, and deliberately narrow. Every rev here ends up in a git **argv slot** —
/// `<rev>:<path>`, or `git diff <a> <b>` — where a leading `-` stops being data and becomes an
/// option. That is not hypothetical: `git show --output=<file>` writes to the filesystem, and these
/// values arrive straight off the query string with no decoding in between.
///
/// The cost is that `HEAD~1` and `main^` are not sayable. Shas and branch names are, which is what
/// the UI passes, and "spell it as a sha" is a much better trade than parsing git's rev grammar.
fn valid_rev(r: &str) -> bool {
    valid_ref_name(r)
}

/// A path inside the store, as it arrives in a URL. Rejects the shapes that make `git show
/// <rev>:<path>` mean something other than "read this file", and the control bytes that would break
/// out of a header value further down.
fn valid_repo_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 512
        && !p.starts_with('-')
        && !p.split('/').any(|c| c.is_empty() || c == "." || c == "..")
        && !p.bytes().any(|b| b < 0x20 || b == 0x7f)
}

/// A git ref name, conservatively. Not `git check-ref-format` — this only has to be a safe, boring
/// label to store and echo back, and refusing an exotic-but-legal ref costs nothing here.
fn valid_ref_name(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 200
        && !r.starts_with('-')
        && !r.starts_with('/')
        && !r.contains("..")
        && !r.contains("//")
        && !r.ends_with('/')
        && r.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
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

/// Bytes served by the raw route in one response. A store holds transcripts, not releases.
const RAW_MAX: u64 = 8 * 1024 * 1024;
/// Rows of `compare` output. A diff between two distant points is unbounded; the answer to "what
/// changed across 40,000 files" is not a JSON array.
const COMPARE_MAX: usize = 500;

/// `GET /api/agent/<name>/raw/<path>?at=<rev>` — a file out of the store, as bytes.
///
/// **This is the one route whose response headers are the security control.** Everything it serves is
/// pushed content, so it is attacker-authored by definition: a session file can hold `<script>` as
/// easily as it holds JSON, and it is served from the Hub's own origin — the origin the session
/// cookie belongs to. So the content-type is never guessed from the extension, and never negotiated:
///
///   - `application/octet-stream` — a guessed `text/html` here is stored XSS, full stop.
///   - `attachment` — a browser following the link downloads it instead of rendering it.
///   - `nosniff` — without it a browser will content-sniff its way back to `text/html` whatever the
///     header said, which is exactly the bug the header was supposed to prevent.
///   - `sandbox` + a null CSP — defence in depth: if something does render it, it renders inert.
///
/// The SPA reads this with fetch() and decides how to display it. That is the right place for the
/// decision, because the SPA knows it is showing a transcript rather than a document.
fn api_raw(repo: &Path, path: &str, query: &str) -> Resp {
    if !valid_repo_path(path) {
        return Resp::err(400, "invalid path");
    }
    let at = param(query, "at").unwrap_or_else(|| "HEAD".into());
    if !valid_rev(&at) {
        return Resp::err(400, "invalid revision");
    }
    let spec = format!("{at}:{path}");
    // Size first, from the object header, so an enormous blob is refused before it is read into
    // memory rather than after.
    let size: u64 = match git(repo, &["cat-file", "-s", &spec]).and_then(|s| s.trim().parse().ok()) {
        Some(n) => n,
        None => return Resp::err(404, "not found"),
    };
    if size > RAW_MAX {
        return Resp::err(413, &format!("this file is {size} bytes; the raw view stops at {RAW_MAX}. Clone the store for it."));
    }
    let Some(body) = git_bytes(repo, &["cat-file", "blob", &spec]) else {
        return Resp::err(404, "not found");
    };
    Resp::new(200, "application/octet-stream", body)
        .with("Content-Disposition", &format!("attachment; filename=\"{}\"", safe_filename(path)))
        .with("X-Content-Type-Options", "nosniff")
        .with("Content-Security-Policy", "default-src 'none'; sandbox")
}

/// The basename, reduced to bytes that cannot break out of a quoted header value.
///
/// `Resp::with` writes headers verbatim, and this string comes from a URL: a `"` would end the value
/// early and a CR/LF would start a header of the attacker's choosing. Filtering rather than escaping,
/// because the only thing a filename has to do here is name the file.
fn safe_filename(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or("file");
    let s: String = base.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')).take(80).collect();
    match s.trim_matches('.').is_empty() {
        true => "file".into(),
        false => s,
    }
}

/// `GET /api/agent/<name>/compare?from=<rev>&to=<rev>` — what changed between two points of the
/// store, across the whole tree rather than within one session (that is `session/<id>/diff`).
fn api_compare(repo: &Path, query: &str) -> Resp {
    let (Some(from), Some(to)) = (param(query, "from"), param(query, "to")) else {
        return Resp::err(400, "need from and to");
    };
    if !valid_rev(&from) || !valid_rev(&to) {
        return Resp::err(400, "invalid revision");
    }
    // Resolve both before diffing: an unknown rev is a 404, not an empty diff that reads like "these
    // two points are identical".
    let (Some(fsha), Some(tsha)) = (rev_sha(repo, &from), rev_sha(repo, &to)) else {
        return Resp::err(404, "no such revision");
    };

    let raw = git(repo, &["diff", "--numstat", &fsha, &tsha, "--"]).unwrap_or_default();
    let mut files: Vec<serde_json::Value> = vec![];
    let mut truncated = false;
    for line in raw.lines() {
        if files.len() >= COMPARE_MAX {
            truncated = true;
            break;
        }
        let mut f = line.split('\t');
        let (added, deleted, path) = (f.next().unwrap_or("-"), f.next().unwrap_or("-"), f.next().unwrap_or(""));
        if path.is_empty() {
            continue;
        }
        // numstat prints "-" for a binary file rather than a count. Report null, not 0: "no lines
        // changed" and "lines are not the unit here" are different answers.
        files.push(serde_json::json!({
            "path": path,
            "added": added.parse::<u64>().ok(),
            "deleted": deleted.parse::<u64>().ok(),
            "binary": added == "-",
        }));
    }

    let commits: Vec<serde_json::Value> = git(repo, &["log", "--format=%H\x1f%s", &format!("{fsha}..{tsha}")])
        .unwrap_or_default()
        .lines()
        .take(COMPARE_MAX)
        .filter_map(|l| {
            let (sha, subject) = l.split_once('\x1f')?;
            Some(serde_json::json!({ "sha": sha, "subject": subject }))
        })
        .collect();

    Resp::json(serde_json::json!({
        "from": from,
        "to": to,
        // What the names resolved to, so a moving branch can be told from a fixed point later.
        "resolved": { "from": fsha, "to": tsha },
        "commits": commits,
        "files": files,
        "truncated": truncated,
    }))
}

/// Resolve a rev to a commit sha. None = it does not name a commit here.
fn rev_sha(repo: &Path, rev: &str) -> Option<String> {
    if !valid_rev(rev) {
        return None;
    }
    let out = git(repo, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])?;
    let s = out.trim().to_string();
    (s.len() == 40).then_some(s)
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

    prepare_repo(&repo_path(ctx.root(), name), ctx.root(), name);

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
                status = v.split_whitespace().next().and_then(|c| c.parse().ok()).unwrap_or(200);
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

/// Make a repo ready to be served: the export marker, and the server-side secret gate.
///
/// Both are done here, right before http-backend runs, rather than only at create time — that is
/// what brings repos made by an older agit-hub (or `git init --bare` by hand) under the same rules
/// instead of leaving them as quiet exceptions.
fn prepare_repo(repo: &Path, root: &Path, agent: &str) {
    ensure_exportable(repo);
    install_pre_receive(repo, root, agent);
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
        AgentAcl {
            name: "secret".into(),
            owner: Some("alice".into()),
            visibility: Visibility::Private,
            lifecycle: Lifecycle::Active,
            members: vec![],
        }
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
            lifecycle: Lifecycle::Active,
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

    // ── the per-token budget ──

    #[test]
    fn a_token_spends_its_burst_and_is_then_refused() {
        let rl = TokenBuckets::new();
        let t0 = Instant::now();
        for i in 0..TOKEN_BURST as usize {
            assert!(rl.allow_at("tok_a", t0), "request {i} is still inside the burst");
        }
        assert!(!rl.allow_at("tok_a", t0), "the burst is spent — the next one must be refused");
    }

    #[test]
    fn the_budget_refills_over_time() {
        let rl = TokenBuckets::new();
        let t0 = Instant::now();
        for _ in 0..TOKEN_BURST as usize {
            rl.allow_at("tok_a", t0);
        }
        assert!(!rl.allow_at("tok_a", t0));
        // One second later there is a second's worth of refill and no more.
        let t1 = t0 + Duration::from_secs(1);
        for i in 0..TOKEN_RATE_PER_SEC as usize {
            assert!(rl.allow_at("tok_a", t1), "refilled request {i}");
        }
        assert!(!rl.allow_at("tok_a", t1), "the refill is the rate, not a fresh burst");
    }

    #[test]
    fn the_refill_never_exceeds_the_burst() {
        // An idle token comes back with a full bucket, not an unbounded one — otherwise a token
        // left alone for a day would bank a day's worth of requests.
        let rl = TokenBuckets::new();
        let t0 = Instant::now();
        rl.allow_at("tok_a", t0);
        let later = t0 + Duration::from_secs(86_400);
        for _ in 0..TOKEN_BURST as usize {
            assert!(rl.allow_at("tok_a", later));
        }
        assert!(!rl.allow_at("tok_a", later), "a day idle must not bank a day of requests");
    }

    #[test]
    fn one_tokens_budget_is_not_anothers() {
        // The whole point of charging the credential: a wedged CI token must not lock out the token
        // next to it (which the per-IP cap would, when both sit behind one NAT).
        let rl = TokenBuckets::new();
        let t0 = Instant::now();
        for _ in 0..TOKEN_BURST as usize {
            rl.allow_at("tok_a", t0);
        }
        assert!(!rl.allow_at("tok_a", t0));
        assert!(rl.allow_at("tok_b", t0), "a different token has its own budget");
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_panic() {
        // Instant subtraction panics on a negative delta, and two threads can read the clock out of
        // order. Refusing to crash here matters more than the arithmetic being exact.
        let rl = TokenBuckets::new();
        let t1 = Instant::now() + Duration::from_secs(10);
        assert!(rl.allow_at("tok_a", t1));
        assert!(rl.allow_at("tok_a", t1 - Duration::from_secs(5)));
    }

    // ── the pre-receive secret gate ──

    #[test]
    fn batch_output_splits_on_the_declared_size_not_on_newlines() {
        // Blob content contains newlines; splitting on them would cut a blob into pieces and scan
        // the header line as if it were content. git's shape is exactly: `<sha> <type> <size>\n`,
        // then <size> bytes, then a separator newline of its own.
        let raw = b"aaa blob 11\nline1\nline2\nbbb blob 3\nxyz\n";
        let out = parse_batch(raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out["aaa"], b"line1\nline2");
        assert_eq!(out["bbb"], b"xyz", "the separator newline must not be eaten out of the next header");
    }

    #[test]
    fn a_missing_object_does_not_shift_every_later_blob_onto_the_wrong_path() {
        // The bug this keys on: "<sha> missing" yields no body, so the old positional zip paired
        // "hi" with the MISSING object's path and every blob after it with its predecessor's. The
        // rejection then named the wrong file — and the path is the whole actionable half of it.
        let raw = b"deadbeef missing\naaa blob 2\nhi\nbbb blob 3\nkey\n";
        let out = parse_batch(raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out["aaa"], b"hi");
        assert_eq!(out["bbb"], b"key");
        assert!(!out.contains_key("deadbeef"), "a missing object must contribute no body at all");
        assert!(parse_batch(b"").is_empty());
    }

    #[test]
    fn printable_runs_find_a_key_a_nul_byte_used_to_hide() {
        // One NUL used to skip the blob whole and silently: this is the bypass that made the gate a
        // liar. The strings pass has to still see the key.
        let mut blob = vec![0u8, 1, 2];
        blob.extend_from_slice(b"aws_access_key_id = AKIAIOSFODNN7EXAMPLE");
        blob.extend_from_slice(&[0u8, 0xff]);
        let runs = printable_runs(&blob);
        assert!(runs.contains("AKIAIOSFODNN7EXAMPLE"), "{runs:?}");
        // entropy off, as scan_push runs it for binary: the named rule must not depend on it.
        let hits = agit::scan::scan_text_allow(&runs, false, &agit::scan::Allowlist::empty());
        let rules: Vec<&str> = hits.iter().map(|f| f.rule).collect();
        assert!(rules.contains(&"aws-access-key-id"), "{rules:?}");
    }

    #[test]
    fn printable_runs_drop_the_noise_between_them() {
        // Runs shorter than the minimum are the incidental bytes of any binary — keeping them would
        // hand the rules a haystack made of chaff.
        assert_eq!(printable_runs(&[0, b'a', b'b', 0, 0xff]), "");
        assert_eq!(printable_runs(b"hello world"), "hello world\n");
        assert_eq!(printable_runs(&[b'l', b'o', b'n', b'g', b'e', b'r', 0, b'x']), "longer\n");
        // A run ending at the very end of the blob is still a run.
        assert_eq!(printable_runs(&[0, b'l', b'o', b'n', b'g', b'e', b'r']), "longer\n");
    }

    #[test]
    fn an_unscanned_blob_makes_the_report_incomplete() {
        // `incomplete()` is what `pre_receive_cmd` refuses on. "Found nothing" and "looked at
        // nothing" must never be the same value.
        let mut r = ScanReport { findings: vec![], unscanned: vec![], errored: None };
        assert!(!r.incomplete(), "a clean, complete scan clears the push");
        r.unscanned.push(("big.bin".into(), "past the per-blob bound".into()));
        assert!(r.incomplete());

        let errored = ScanReport { findings: vec![], unscanned: vec![], errored: Some("git failed".into()) };
        assert!(errored.incomplete(), "an IO failure is not a clean scan");
    }

    // ── pagination ──

    #[test]
    fn a_cursor_roundtrips_and_refuses_junk() {
        for key in ["payments", "1", "a.b-c_d", "42"] {
            assert_eq!(cursor_decode(&cursor_encode(key)).as_deref(), Some(key));
        }
        // Opaque means opaque: it must not read as the key it encodes.
        assert_ne!(cursor_encode("payments"), "payments");
        for bad in ["", "zz", "abc", "payments", "的的"] {
            assert_eq!(cursor_decode(bad), None, "{bad:?} must not decode");
        }
        // A cursor is a resume point, not a place to post a novel.
        assert_eq!(cursor_decode(&"61".repeat(300)), None);
    }

    /// `Resp` is deliberately not Debug (it carries response bodies), so unwrap it by hand.
    fn page_of(query: &str) -> Option<Page> {
        page_params(query).ok()
    }

    #[test]
    fn no_limit_means_everything_not_a_default_page() {
        // The embedded SPA does not know what a cursor is. A default page would cap its list with no
        // way for it to ask for the rest — a silent cap in a UI, which is the thing being avoided.
        let p = page_of("").expect("no params is a valid request");
        assert_eq!(p.limit, usize::MAX);
        assert!(p.after.is_none());
    }

    #[test]
    fn a_limit_is_clamped_and_junk_is_refused_rather_than_ignored() {
        assert_eq!(page_of("limit=5").map(|p| p.limit), Some(5));
        // Over the ceiling is clamped, not an error: asking for too much is not an instruction.
        assert_eq!(page_of("limit=99999").map(|p| p.limit), Some(PAGE_MAX));
        // ...but nonsense is refused, never silently treated as "everything".
        for bad in ["limit=0", "limit=-1", "limit=abc", "limit="] {
            assert!(page_of(bad).is_none(), "{bad:?} must be refused");
        }
        assert!(page_of("cursor=nothex").is_none());
        assert_eq!(page_of(&format!("cursor={}", cursor_encode("payments"))).and_then(|p| p.after).as_deref(), Some("payments"));
    }

    // ── the values that reach a git argv slot ──

    #[test]
    fn a_rev_that_could_become_a_git_option_is_refused() {
        // `git show --output=<file>` WRITES A FILE. The rev is concatenated into `<rev>:<path>`, so a
        // leading `-` turns the whole argument into an option — and this value arrives straight off
        // the query string.
        assert!(!valid_rev("--output=/tmp/pwned"));
        assert!(!valid_rev("-o"));
        assert!(!valid_rev("--upload-pack=evil"));
        // ...while the things a caller legitimately says still work.
        assert!(valid_rev("HEAD"));
        assert!(valid_rev("main"));
        assert!(valid_rev("refs/heads/topic"));
        assert!(valid_rev("d43585c9e0f8a1b2c3d4e5f60718293a4b5c6d7e"));
        // Range syntax is not a rev: `from..to` is built here, from two revs checked separately.
        assert!(!valid_rev("a..b"));
        assert!(!valid_rev(""));
    }

    #[test]
    fn a_repo_path_cannot_climb_out_or_break_a_header() {
        for bad in ["../../../etc/passwd", "sessions/../../../etc/passwd", "a/../b", "a//b", "./x", "x/./y", "/etc/passwd", "-x", ""] {
            assert!(!valid_repo_path(bad), "{bad:?} must be refused");
        }
        // Control bytes never reach a header value, quoted or not.
        assert!(!valid_repo_path("a\r\nX-Evil: 1"));
        assert!(!valid_repo_path("a\nb"));
        assert!(!valid_repo_path("a\0b"));
        for ok in ["tracked.txt", "sessions/claude-code/s1.jsonl", "a-b_c.2.json"] {
            assert!(valid_repo_path(ok), "{ok:?} must be allowed");
        }
    }

    #[test]
    fn a_filename_cannot_break_out_of_the_content_disposition_header() {
        // Resp::with writes headers verbatim: a quote ends the value early, a CRLF starts a header of
        // the attacker's choosing. Filtered, not escaped — a filename only has to name the file.
        assert_eq!(safe_filename("sessions/x/s1.jsonl"), "s1.jsonl");
        assert_eq!(safe_filename(r#"a".txt"#), "a.txt");
        assert_eq!(safe_filename("a\r\nX-Evil: 1.txt"), "aX-Evil1.txt");
        assert_eq!(safe_filename("a b;c.txt"), "abc.txt");
        // Nothing usable left → a name, not an empty quoted value.
        assert_eq!(safe_filename("的"), "file");
        assert_eq!(safe_filename("..."), "file");
        assert_eq!(safe_filename(""), "file");
    }

    #[test]
    fn hook_paths_are_shell_quoted() {
        // The hook is a shell script; a path with a space or a quote in it must not become code.
        assert_eq!(shell_quote("/tmp/x"), "'/tmp/x'");
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
        assert_eq!(shell_quote("a';rm -rf /;'"), r"'a'\'';rm -rf /;'\'''");
    }

    // ── MR refs ──

    #[test]
    fn ref_names_reject_traversal_and_option_injection() {
        assert!(valid_ref_name("main"));
        assert!(valid_ref_name("feat/hub"));
        assert!(valid_ref_name("v1.2.3"));
        assert!(!valid_ref_name(""));
        assert!(!valid_ref_name("--upload-pack=evil"), "a leading dash could be read as an option");
        assert!(!valid_ref_name("../etc"));
        assert!(!valid_ref_name("/abs"));
        assert!(!valid_ref_name("a//b"));
        assert!(!valid_ref_name("trailing/"));
        assert!(!valid_ref_name("has space"));
        assert!(!valid_ref_name(&"x".repeat(201)));
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
