//! CLI subcommands (sync): user / agent / token administration. Verbatim from the monolith.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::io::Write as _;

use agit::hub::acl::{Lifecycle, Scope, Visibility};
use agit::hub::net::valid_agent_name;
use agit::hub::store::{AgentMeta, Store, TokenRec, User};
use agit::hub::{audit, kdf, store};

use crate::{flag, has_flag, positional};

/// Bridge a sync CLI subcommand to the async store: spin up a short-lived tokio runtime and drive the
/// future to completion. Only `serve` needs the full server runtime; the admin subcommands are
/// one-shot, so a fresh runtime per invocation is fine. `Store::open` honors `AGIT_HUB_DB`, so an
/// admin command run against a Postgres-backed hub (e.g. `docker exec … user add`) hits the same DB
/// the server does.
pub(crate) fn run_async<F: std::future::Future<Output = i32>>(fut: F) -> i32 {
    match tokio::runtime::Runtime::new() {
        Ok(rt) => rt.block_on(fut),
        Err(e) => {
            eprintln!("failed to start the async runtime: {e}");
            1
        }
    }
}

/// Open the configured backend, mapping any error to a printed message + exit code 1.
async fn open_store(root: &Path) -> Result<Store, i32> {
    Store::open(root).await.map_err(|e| {
        eprintln!("failed to open the metadata database: {e}");
        1
    })
}

// ─────────────────────────── CLI: user ───────────────────────────

pub(crate) fn user_cmd(root: &Path, args: &[String]) -> i32 {
    run_async(user_cmd_async(root, args))
}

async fn user_cmd_async(root: &Path, args: &[String]) -> i32 {
    let store = match open_store(root).await {
        Ok(s) => s,
        Err(code) => return code,
    };
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
            if store.user(&username).await.is_some() {
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
            if let Err(e) = store.add_user(user).await {
                eprintln!("failed to persist the user to {}: {e}", store.backend());
                return 1;
            }
            audit::append(root, "cli", audit::USER_ADD, None, &format!("{username} admin={is_admin}"));
            println!("created user {username}{}", if is_admin { " (site admin)" } else { "" });
            println!("  The password is derived with argon2id and stored in {}; the plaintext never hits disk.", store.describe());
            0
        }
        Some("list") => {
            let users = store.users().await;
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
pub(crate) fn read_new_password() -> Result<String, String> {
    let (pw, tty) = read_password("set a password for this user: ")?;
    if pw.chars().count() < store::MIN_PASSWORD_LEN {
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
pub(crate) fn read_password(prompt: &str) -> Result<(String, bool), String> {
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

pub(crate) fn stty(args: &[&str]) -> bool {
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
pub(crate) async fn resolve_user(store: &Store, explicit: Option<String>, flag_name: &str) -> Result<User, String> {
    if let Some(name) = explicit {
        let n = store::normalize_username(&name);
        return store.user(&n).await.ok_or(format!("no such user: {n} (try `agit-hub user list`)"));
    }
    let users = store.users().await;
    match users.len() {
        0 => Err("no users yet. Start with `agit-hub user add <you> --admin`.".into()),
        1 => Ok(users.into_iter().next().unwrap()),
        _ => Err(format!("there are several users, name one with {flag_name} <user>.")),
    }
}

// ─────────────────────────── CLI: agent ───────────────────────────

pub(crate) fn add_cmd(root: &Path, args: &[String]) -> i32 {
    run_async(add_cmd_async(root, args))
}

async fn add_cmd_async(root: &Path, args: &[String]) -> i32 {
    let Some(name) = positional(args, 1) else {
        eprintln!("usage: agit-hub add <name> [--owner <user>] [--public]");
        return 2;
    };
    let store = match open_store(root).await {
        Ok(s) => s,
        Err(code) => return code,
    };
    let owner = match resolve_user(&store, flag(args, "--owner"), "--owner").await {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    // Private by default — transcripts are sensitive data, so going public must be an explicit act.
    let visibility = if has_flag(args, "--public") { Visibility::Public } else { Visibility::Private };
    match create_agent(&store, name, &owner.username, visibility).await {
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
pub(crate) async fn create_agent(store: &Store, name: &str, owner: &str, visibility: Visibility) -> Result<bool, String> {
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
        .await
        .map_err(|e| format!("failed to persist the agent: {e}"))?
}

pub(crate) fn list_cmd(root: &Path) -> i32 {
    run_async(list_cmd_async(root))
}

async fn list_cmd_async(root: &Path) -> i32 {
    let store = match open_store(root).await {
        Ok(s) => s,
        Err(code) => return code,
    };
    let names = list_agents(root);
    if names.is_empty() {
        println!("no agents yet. `agit-hub add <name>` creates one.");
    }
    for n in names {
        let m = store.agent_or_unowned(&n).await;
        let owner = m.owner.clone().unwrap_or_else(|| "— (unowned)".into());
        println!("{:<24} {:<8} owner={}", n, m.visibility, owner);
    }
    0
}

pub(crate) fn list_agents(root: &Path) -> Vec<String> {
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

pub(crate) fn repo_path(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.git"))
}

// ─────────────────────────── CLI: token ───────────────────────────

pub(crate) fn token_cmd(root: &Path, args: &[String]) -> i32 {
    run_async(token_cmd_async(root, args))
}

async fn token_cmd_async(root: &Path, args: &[String]) -> i32 {
    let store = match open_store(root).await {
        Ok(s) => s,
        Err(code) => return code,
    };
    match args.get(1).map(|s| s.as_str()) {
        Some("add") => {
            let Some(name) = positional(args, 2) else {
                eprintln!("usage: agit-hub token add <name> [--user <owner>] [--agent <name>] [--read|--write] [--ttl-days N]");
                return 2;
            };
            let owner = match resolve_user(&store, flag(args, "--user"), "--user").await {
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
                if !valid_agent_name(a) || store.agent(a).await.is_none() {
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
            match issue_token(&store, name, &owner.username, agent.as_deref(), scope, ttl_days).await {
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
            let toks = store.tokens().await;
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
            match store
                .update_tokens(|toks| {
                    let before = toks.len();
                    toks.retain(|t| &t.id != id);
                    before != toks.len()
                })
                .await
            {
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
                    eprintln!("failed to persist the token change: {e}");
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
pub(crate) async fn issue_token(store: &Store, name: &str, owner: &str, agent: Option<&str>, scope: Scope, ttl_days: Option<i64>) -> Result<String, String> {
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
    store.update_tokens(|t| t.push(rec)).await.map_err(|e| format!("failed to persist the token: {e}"))?;
    Ok(secret)
}
