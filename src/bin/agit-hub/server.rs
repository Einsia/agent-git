//! Server config, shared context, and the axum/tokio boot. serve_cmd/bind_guard/display_host stay
//! sync and verbatim; serve() is rewritten onto a tokio runtime + axum.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::net::{IpAddr, SocketAddr};
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;

use agit::hub::blob::Blobs;
use agit::hub::metrics::Metrics;
use agit::hub::net;
use agit::hub::session::Sessions;
use agit::hub::store::{self, Store};

use crate::cli::list_agents;
use crate::limits::{ConnLimiter, TokenBuckets, LOGIN_CONC, REGISTER_BURST, REGISTER_RATE_PER_SEC};
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
    /// Self-service registration (POST /api/register). **Disabled by default** (invite-only): the Hub
    /// is conservative at every other axis (loopback-only bind, private-by-default agents, admin-only
    /// user creation), and opening account creation to anyone who can reach the port must be a
    /// deliberate opt-in. Enabled via `AGIT_HUB_REGISTRATION` or `--open-registration`.
    pub(crate) registration: bool,
    /// This hub's own canonical base URL (`scheme://host[:port]`, no path), when the operator has
    /// configured one via `AGIT_HUB_PUBLIC_URL`. It is the AUTHORITATIVE audience the key-auth handler
    /// (`POST /api/auth/key`) checks a signed assertion against: a configured value is server-controlled
    /// and CANNOT be spoofed by a request header, so a signature captured for this hub can never be
    /// replayed to a different hub. When unset the handler falls back to the request `Host` (see
    /// `keyauth::canonical_audience`), which is best-effort only — pin this on any multi-hub deployment.
    pub(crate) public_url: Option<String>,
}

/// All the shared state one request needs. Wrapped in a single Arc so [`Ctx`] is cheap to clone into
/// every axum handler as `State<Ctx>`.
pub(crate) struct CtxInner {
    pub(crate) store: Store,
    /// Content-addressed blob storage (fs by default, S3/Garage when configured). Held by value like
    /// `store`, since `CtxInner` is already behind one `Arc`; every handler reads `ctx.blobs`.
    pub(crate) blobs: Blobs,
    pub(crate) cfg: Cfg,
    pub(crate) sessions: Sessions,
    pub(crate) limiter: Arc<ConnLimiter>,
    /// Login concurrency gate: argon2 is **deliberately** slow and memory-hungry. Without a cap, a few
    /// dozen concurrent logins = a few dozen copies of 19MiB + every core pegged. An async semaphore,
    /// so the login handler yields (rather than blocking a worker thread) while it waits for a slot.
    pub(crate) login_gate: Arc<tokio::sync::Semaphore>,
    /// Per-token request budget (see TokenBuckets).
    pub(crate) token_rl: Arc<TokenBuckets>,
    /// Per-IP registration budget. The shared connection limiter caps *concurrent* connections and the
    /// per-token budget only bites *authenticated* callers, so self-service signup — unauthenticated and
    /// cheap to retry — is otherwise an unbounded account-spam / username-enumeration surface on an
    /// `--open-registration` hub. A tight token bucket keyed on the client IP throttles a sweep to a
    /// trickle without troubling a real person who signs up once.
    pub(crate) register_rl: Arc<TokenBuckets>,
    /// Process-wide observability counters (Prometheus text exposition at `/metrics`). Behind an
    /// `Arc` so the per-request middleware and every handler that records share one instance; all its
    /// fields are atomics, so no lock is taken on the hot path.
    pub(crate) metrics: Arc<Metrics>,
    /// The per-hub escrow X25519 keypair (encryption-recipients Wave 5, hub-assist escrow). Generated
    /// once at boot and persisted under the data dir (`escrow_x25519`, `0600`). The PUBLIC half is served
    /// at `GET /api/escrow/pubkey` so a hub-assist client can seal its content key TO the hub; the PRIVATE
    /// half NEVER leaves the process and is the only key that can open an escrowed CK for release. At-rest
    /// protection of this secret is OUT OF SCOPE (same posture as the TOTP secret and password material):
    /// a hub with hub-assist escrow ENABLED for an org has, by design, chosen to trust the hub with the
    /// ability to release those sessions' keys under the ACL gate.
    pub(crate) escrow: EscrowKeypair,
    /// The in-memory, single-use challenge nonces for KEY-BASED auth (`GET /api/auth/challenge` +
    /// `POST /api/auth/key`). Deliberately NOT a table/column: nonces live ~60s and must never touch the
    /// schema-migration path (the known live-Postgres crash-loop risk). Behind an `Arc` so every handler
    /// shares one store; pruned lazily on access.
    pub(crate) auth_nonces: Arc<crate::keyauth::AuthNonces>,
}

/// The hub's escrow keypair. `secret` is a raw 32-byte X25519 scalar (clamped on use via `mul_clamped`);
/// `public` is its X25519 public. Both are plain bytes so a handler can seal/open with the `keybox`
/// primitives without holding a lock.
#[derive(Clone)]
pub(crate) struct EscrowKeypair {
    pub(crate) secret: [u8; 32],
    pub(crate) public: [u8; 32],
}

/// Load the per-hub escrow X25519 keypair from `<root>/escrow_x25519`, or generate + persist a fresh one
/// (`0600`) on first boot. The file holds the 32-byte secret as hex. A corrupt/short file is a hard boot
/// error rather than a silent re-generation (re-generating would strand every already-escrowed CK).
fn load_or_create_escrow_keypair(root: &Path) -> std::io::Result<EscrowKeypair> {
    let path = root.join("escrow_x25519");
    let secret: [u8; 32] = match std::fs::read_to_string(&path) {
        Ok(t) => {
            let raw = hex::decode(t.trim()).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: not valid hex: {e}", path.display()))
            })?;
            raw.as_slice().try_into().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: escrow key is not 32 bytes", path.display()))
            })?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let s = agit::crypt::random_master().map_err(|e| std::io::Error::other(e.to_string()))?;
            // Write the secret hex at 0600 (owner-only), the same at-rest guarantee the DB gets.
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, format!("{}\n", hex::encode(s)))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            }
            std::fs::rename(&tmp, &path)?;
            s
        }
        Err(e) => return Err(e),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let public = agit::agent::x25519_public_from_secret(&secret);
    Ok(EscrowKeypair { secret, public })
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
    // Self-service signup is OFF unless explicitly turned on (env or flag), mirroring the env-driven
    // AGIT_HUB_DB wiring. Default invite-only: safest default, opt in explicitly.
    let registration = std::env::var("AGIT_HUB_REGISTRATION")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "open" | "yes"))
        .unwrap_or(false)
        || has_flag(args, "--open-registration");
    // The hub's own canonical base URL, if the operator pins one. It anchors the key-auth audience check
    // (see `Cfg::public_url`). A trailing slash is trimmed so it compares equal to a client's path-free base.
    let public_url = flag(args, "--public-url")
        .or_else(|| std::env::var("AGIT_HUB_PUBLIC_URL").ok())
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());
    if let Err(e) = bind_guard(host, tls, insecure) {
        eprintln!("{e}");
        return 2;
    }
    if has_flag(args, "--private") {
        println!("note: --private is no longer needed — visibility is now a **per-agent** property, and new agents are private by default.");
        println!("      To publish one agent: `agit-hub add <name> --public`, or change it in the UI.");
    }
    serve(root, Cfg { host, port, tls, insecure, trusted_proxies, registration, public_url })
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
    for (seg, n) in &agents {
        let m = store.agent_or_unowned(seg, n).await;
        // Legacy null-owner repos were re-homed to the reserved `_unclaimed` account at migration.
        if m.owner_ns() == Some(agit::hub::store::UNCLAIMED) {
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
    println!("  store:   {}", store.describe());
    println!("  blobs:   {}", ctx.blobs.describe());
    println!("  hosting: {} agents ({public} public)", agents.len());
    println!("  users:   {} ({} admins)", users.len(), users.iter().filter(|u| u.is_admin).count());
    println!("  signup:  {}", if cfg.registration { "open (self-service)" } else { "invite-only" });
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
        println!("  ⚠ {unowned} agents are unclaimed (old repos, re-homed to `_unclaimed`): private, visible only to the site admin.");
        println!("    Claim them: `agit-hub add <name> --owner <user>` (they answer at /_unclaimed/<name>.git meanwhile).");
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

/// Install the process-wide `tracing` subscriber, exactly once.
///
/// Configuration is env-only so it needs no flags and no config file:
///   - `AGIT_HUB_LOG` — the filter directive (default `info`); any `tracing_subscriber` `EnvFilter`
///     syntax works, e.g. `AGIT_HUB_LOG=agit_hub=debug,info`.
///   - `AGIT_HUB_LOG_FORMAT` — `pretty` (human, default) or `json` (one JSON object per line, for a
///     log pipeline).
///
/// `try_init` is used rather than `init` so a second call (a test that boots two contexts, say) is a
/// harmless no-op instead of a panic — hence this never returns an error and never aborts a boot.
/// Logs go to **stderr** so the human-readable startup banner (`println!`) on stdout is untouched.
pub(crate) fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("AGIT_HUB_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("AGIT_HUB_LOG_FORMAT")
        .map(|v| v.trim().eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    let builder = fmt().with_env_filter(filter).with_writer(std::io::stderr);
    // Two separate `try_init` arms: the JSON and pretty formatters are different concrete types, so
    // they cannot share one `builder` binding.
    let _ = if json { builder.json().try_init() } else { builder.try_init() };
}

/// Rewritten boot: build the shared context, a multi-thread tokio runtime, and the axum app. The CLI
/// path stays sync (this is called from `run()`), so the runtime is built here rather than via
/// `#[tokio::main]`.
pub(crate) fn serve(root: &Path, cfg: Cfg) -> i32 {
    // Structured logging comes up first thing, so every event below (boot failures included) is
    // captured in the configured format.
    init_tracing();
    if let Err(e) = store::ensure_root(root) {
        tracing::error!(root = %root.display(), error = %e, "failed to create root");
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
                tracing::error!(error = %e, "failed to open the metadata database");
                eprintln!("failed to open the metadata database: {e}");
                return 1;
            }
        };
        let blobs = match Blobs::open(root).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "failed to open blob storage");
                eprintln!("failed to open blob storage: {e}");
                return 1;
            }
        };
        let escrow = match load_or_create_escrow_keypair(root) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(error = %e, "failed to load the hub escrow key");
                eprintln!("failed to load the hub escrow key: {e}");
                return 1;
            }
        };
        let ctx = Ctx(Arc::new(CtxInner {
            store,
            blobs,
            cfg,
            sessions: Sessions::new(),
            limiter: Arc::new(ConnLimiter::default()),
            login_gate: Arc::new(tokio::sync::Semaphore::new(LOGIN_CONC)),
            token_rl: Arc::new(TokenBuckets::new()),
            register_rl: Arc::new(TokenBuckets::with_rate(REGISTER_RATE_PER_SEC, REGISTER_BURST)),
            metrics: Arc::new(Metrics::new()),
            escrow,
            auth_nonces: Arc::new(crate::keyauth::AuthNonces::new()),
        }));
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(%addr, error = %e, "failed to bind");
                eprintln!("failed to bind {addr}: {e}");
                return 1;
            }
        };
        startup_banner(&ctx, addr).await;
        tracing::info!(
            %addr,
            tls = ctx.cfg.tls,
            registration = ctx.cfg.registration,
            "agit-hub serving"
        );
        let app = crate::router::build(ctx);
        match axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(shutdown_signal())
            .await
        {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!(error = %e, "server error");
                eprintln!("server error: {e}");
                1
            }
        }
    })
}
