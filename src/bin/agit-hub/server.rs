//! Server config, shared context, and the axum/tokio boot. serve_cmd/bind_guard/display_host stay
//! sync and verbatim; serve() is rewritten onto a tokio runtime + axum.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::net::{IpAddr, SocketAddr};
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;

use agit::hub::net;
use agit::hub::session::Sessions;
use agit::hub::store::{self, Store};

use crate::cli::list_agents;
use crate::limits::{ConnLimiter, TokenBuckets, LOGIN_CONC};
use crate::{flag, has_flag};

pub(crate) struct Cfg {
    pub(crate) host: IpAddr,
    pub(crate) port: u16,
    /// TLS is terminated in front (reverse proxy). Effects: non-loopback binds are allowed; cookies
    /// get Secure.
    pub(crate) tls: bool,
    /// Listen publicly in full knowledge that there is no TLS.
    pub(crate) insecure: bool,
    /// IPs of trusted reverse proxies — X-Forwarded-For only counts on connections from them.
    pub(crate) trusted_proxies: Vec<IpAddr>,
}

/// All the shared state one request needs. Wrapped in a single Arc so [`Ctx`] is cheap to clone into
/// every axum handler as `State<Ctx>`.
pub(crate) struct CtxInner {
    pub(crate) store: Store,
    pub(crate) cfg: Cfg,
    pub(crate) sessions: Sessions,
    pub(crate) limiter: Arc<ConnLimiter>,
    /// Login concurrency gate: argon2 is **deliberately** slow and memory-hungry. Without a cap, a few
    /// dozen concurrent logins = a few dozen copies of 19MiB + every core pegged. An async semaphore,
    /// so the login handler yields (rather than blocking a worker thread) while it waits for a slot.
    pub(crate) login_gate: Arc<tokio::sync::Semaphore>,
    /// Per-token request budget (see TokenBuckets).
    pub(crate) token_rl: Arc<TokenBuckets>,
}

#[derive(Clone)]
pub(crate) struct Ctx(pub(crate) Arc<CtxInner>);

impl Deref for Ctx {
    type Target = CtxInner;
    fn deref(&self) -> &CtxInner {
        &self.0
    }
}

impl Ctx {
    pub(crate) fn root(&self) -> &Path {
        self.store.root()
    }
}

pub(crate) fn serve_cmd(root: &Path, args: &[String]) -> i32 {
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
pub(crate) fn bind_guard(host: IpAddr, tls: bool, insecure: bool) -> Result<(), String> {
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

pub(crate) fn display_host(cfg: &Cfg) -> String {
    let h = if cfg.host.is_unspecified() { "localhost".to_string() } else { cfg.host.to_string() };
    match (cfg.tls, cfg.port) {
        (true, 443) | (false, 80) => h,
        _ => format!("{h}:{}", cfg.port),
    }
}

/// Print the startup banner (verbatim text from the monolith), computed off the live store.
async fn startup_banner(ctx: &Ctx, addr: SocketAddr) {
    let cfg = &ctx.cfg;
    let root = ctx.root();
    let store = &ctx.store;
    let agents = list_agents(root);
    // One pass over the agents: async store reads can't sit inside an iterator's `.filter` closure.
    let mut unowned = 0usize;
    let mut public = 0usize;
    for n in &agents {
        let m = store.agent_or_unowned(n).await;
        if m.owner.is_none() {
            unowned += 1;
        }
        if m.visibility == "public" {
            public += 1;
        }
    }
    let users = store.users().await;
    let legacy_tokens = store.tokens().await.iter().filter(|t| t.owner.is_none()).count();

    println!("AgentGitHub running");
    println!("  listen:  {addr}{}", if cfg.tls { " (TLS terminated in front)" } else { "" });
    println!("  web:     {}://{}/", if cfg.tls { "https" } else { "http" }, display_host(cfg));
    println!("  root:    {}", root.display());
    println!("  hosting: {} agents ({public} public)", agents.len());
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
}

/// Graceful shutdown on Ctrl-C (SIGINT).
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Rewritten boot: build the shared context, a multi-thread tokio runtime, and the axum app. The CLI
/// path stays sync (this is called from `run()`), so the runtime is built here rather than via
/// `#[tokio::main]`.
pub(crate) fn serve(root: &Path, cfg: Cfg) -> i32 {
    if let Err(e) = store::ensure_root(root) {
        eprintln!("failed to create root {}: {e}", root.display());
        return 1;
    }

    let addr = SocketAddr::new(cfg.host, cfg.port);

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("failed to start the async runtime: {e}");
            return 1;
        }
    };

    rt.block_on(async move {
        // Build the pool + run migrations here, on the runtime that will serve requests. A bad
        // AGIT_HUB_DB (or an unreachable Postgres) surfaces as a clear error at boot rather than on
        // the first request.
        let store = match Store::open(root).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("failed to open the metadata database: {e}");
                return 1;
            }
        };
        let ctx = Ctx(Arc::new(CtxInner {
            store,
            cfg,
            sessions: Sessions::new(),
            limiter: Arc::new(ConnLimiter::default()),
            login_gate: Arc::new(tokio::sync::Semaphore::new(LOGIN_CONC)),
            token_rl: Arc::new(TokenBuckets::new()),
        }));
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("failed to bind {addr}: {e}");
                return 1;
            }
        };
        startup_banner(&ctx, addr).await;
        let app = crate::router::build(ctx);
        match axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(shutdown_signal())
            .await
        {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("server error: {e}");
                1
            }
        }
    })
}
