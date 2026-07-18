//! The axum Router, the single auth/connection middleware, the Caller extractor, the existence-
//! non-disclosure gate (verbatim), the compiled-in SPA assets, and the git-or-SPA fallback that keeps
//! git smart-http ahead of the SPA. This band replaces the hand-rolled route()/api() dispatch band.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;

use agit::hub::acl::{self, Action, AgentAcl, Caller, Decision, Deny};
use agit::hub::blob::BLOB_MAX;
use agit::hub::net::{self, valid_agent_name};
use agit::hub::store::AgentMeta;
use agit::hub::{audit, auth};

use crate::api::{agent_acl, api};
use crate::cli::repo_path;
use crate::http::{credentials, git_deny_resp, req_from_parts, Resp};
use crate::limits::{API_MAX_BODY, MAX_BODY, MAX_CONN};
use crate::server::Ctx;
use crate::smarthttp::git_http;

// ── Frontend embedded at compile time (hub-ui/dist) ──
pub(crate) const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/index.html"));
pub(crate) const APP_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/app.js"));
pub(crate) const APP_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/app.css"));
pub(crate) const FAVICON: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><text y='13' font-size='13'>◆</text></svg>";

/// Record a denial. A denied anonymous read = "not logged in yet", which is noise; a denied
/// authenticated caller, or a denied write/manage action, is signal.
pub(crate) async fn audit_deny(ctx: &Ctx, actor: &str, agent: Option<&str>, action: Action, d: Deny) {
    if actor != "anonymous" || action != Action::Read {
        audit_append(ctx.root(), actor, audit::DENIED, agent, &format!("{action:?}: {}", d.reason())).await;
    }
}

/// Offload the audit append (a blocking fs open+write) to the blocking pool, and await it so the
/// record is durable before the handler's response returns (a later `GET /api/audit` in the same flow
/// must observe it). Now that `api()` runs inline on the async workers, this keeps the fs write off
/// them.
pub(crate) async fn audit_append(root: &std::path::Path, actor: &str, action: &str, agent: Option<&str>, detail: &str) {
    let (root, actor, action, agent, detail) =
        (root.to_path_buf(), actor.to_string(), action.to_string(), agent.map(|s| s.to_string()), detail.to_string());
    let _ = tokio::task::spawn_blocking(move || audit::append(&root, &actor, &action, agent.as_deref(), &detail)).await;
}

/// Fetch the agent + decide + produce the error response. **Every agent entry point comes through here.**
///
/// Existence is itself a secret: a nonexistent agent is decided as "unowned private", so "doesn't
/// exist" and "you can't see it" give **the same** response — otherwise the difference between
/// 401/403/404 is an interface for enumerating private agent names.
/// Existence is only checked after the decision passes (only the authorized get to know it's absent).
pub(crate) async fn gate(ctx: &Ctx, caller: &Caller, name: &str, action: Action) -> Result<AgentMeta, Resp> {
    // A malformed name → 404. That's not a secret: it could never be a valid agent in the first place.
    if !valid_agent_name(name) {
        return Err(Resp::err(404, "not found"));
    }
    let meta = ctx.store.agent_or_unowned(name).await;
    // Fold the owning org's members in before deciding — decide itself never learns "org" exists.
    let acl = agent_acl(ctx, &meta).await;
    match acl::decide(caller, &acl, action) {
        Decision::Allow => match repo_path(ctx.root(), name).exists() {
            true => Ok(meta),
            false => Err(Resp::err(404, "not found")),
        },
        Decision::Deny(d) => {
            let actor = caller.user.clone().unwrap_or_else(|| "anonymous".into());
            audit_deny(ctx, &actor, Some(name), action, d).await;
            Err(deny_resp(caller, &acl, d))
        }
    }
}

pub(crate) fn deny_resp(caller: &Caller, acl: &AgentAcl, d: Deny) -> Resp {
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

/// Build the axum app: explicit asset routes, the /api dispatcher, the git-or-SPA fallback, wrapped by
/// the connection/auth middleware and the global concurrency cap.
pub(crate) fn build(ctx: Ctx) -> Router {
    Router::new()
        .route("/assets/app.js", get(asset_js))
        .route("/assets/app.css", get(asset_css))
        .route("/favicon.ico", get(favicon))
        .route("/api/{*rest}", any(api_entry))
        .fallback(git_or_spa)
        // Innermost first: identity + connection admission established once per request...
        .layer(middleware::from_fn_with_state(ctx.clone(), gate_conn_and_auth))
        // ...then the global concurrency cap wraps it (replaces the accept-time Semaphore).
        .layer(tower::limit::ConcurrencyLimitLayer::new(MAX_CONN))
        .with_state(ctx)
}

async fn asset_js() -> Response {
    Resp::new(200, "application/javascript; charset=utf-8", APP_JS.as_bytes().to_vec()).into_response()
}
async fn asset_css() -> Response {
    Resp::new(200, "text/css; charset=utf-8", APP_CSS.as_bytes().to_vec()).into_response()
}
async fn favicon() -> Response {
    Resp::new(200, "image/svg+xml", FAVICON.as_bytes().to_vec()).into_response()
}

/// The ONE place identity + connection admission is established. Reproduces the head of the old
/// `handle()` in order: traversal reject, per-IP admission (proxied conns keyed on the real client),
/// authenticate, per-token budget, then insert the Caller and run the inner service.
async fn gate_conn_and_auth(State(ctx): State<Ctx>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let reqo = req_from_parts(parts.method.as_str(), &parts.uri, &parts.headers);

    // (a) Blanket path-traversal gate: any `..` segment → 400, covering git + api + spa uniformly.
    if parts.uri.path().split('/').any(|seg| seg == "..") {
        return Resp::text(400, "bad request").into_response();
    }

    // (b) Connection admission. Direct conns are keyed on the peer; conns from a declared trusted proxy
    // are keyed on the real client (from XFF) so everyone behind the proxy is not lumped into one quota.
    let peer: Option<IpAddr> = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    let proxied = peer.map(|ip| ctx.cfg.trusted_proxies.contains(&ip)).unwrap_or(false);
    let client = peer.map(|p| net::client_ip(p, reqo.header("x-forwarded-for"), &ctx.cfg.trusted_proxies));
    let admit_key = if proxied { client } else { peer };
    let _ipguard = match admit_key {
        Some(ip) => match ctx.limiter.try_acquire(ip) {
            Some(g) => Some(g),
            None => return Resp::text(429, "too many connections").into_response(),
        },
        None => None,
    };

    // (c) Authentication looks only at headers — no body required. The async store is awaited directly.
    let secrets = credentials(&reqo);
    let sid = reqo.sid();
    let authn = auth::authenticate(&ctx.store, &ctx.sessions, sid.as_deref(), &secrets).await;

    // (d) Token bookkeeping + per-token budget (keyed on token id, not IP).
    if let Some(id) = authn.token_id.clone() {
        auth::touch_token(&ctx.store, &id).await;
        if !ctx.token_rl.allow(&id) {
            return Resp::text(
                429,
                "this token is over its request budget; slow down (the limit is per token, not per address)",
            )
            .with("Retry-After", "1")
            .into_response();
        }
    }

    // (e) Hand the Caller to the handlers via extensions, then run the inner service with the IpGuard
    // still held across the await (RAII drop after).
    parts.extensions.insert(authn.caller);
    let req = Request::from_parts(parts, body);
    next.run(req).await
}

/// Pull the Caller the middleware established out of extensions (infallible; middleware always sets it).
fn caller_of(parts: &axum::http::request::Parts) -> Caller {
    parts.extensions.get::<Caller>().cloned().unwrap_or_else(Caller::anonymous)
}

/// Whether `rest` (the path after `/api/`) is the blob-upload endpoint `agent/<name>/blob` — a single
/// agent segment then exactly `/blob`, no trailing digest. Mirrors the handler's PUT arm so the body
/// cap and the route agree. A GET download carries the digest tail (and no body), so it never matches.
fn is_blob_put_path(rest: &str) -> bool {
    rest.strip_prefix("agent/").and_then(|r| r.strip_suffix("/blob")).map(|n| !n.is_empty() && !n.contains('/')).unwrap_or(false)
}

/// `/api/*` dispatcher. Rebuilds the [`Req`] view, enforces the API body cap, then runs the async
/// `api()` string-dispatcher directly on the runtime (its store reads/writes are awaited).
async fn api_entry(State(ctx): State<Ctx>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let caller = caller_of(&parts);
    let method = parts.method.as_str().to_string();
    let reqo = req_from_parts(&method, &parts.uri, &parts.headers);
    let rest = parts.uri.path().strip_prefix("/api/").unwrap_or("").to_string();
    let clen = reqo.content_length;

    // A blob PUT (agent/<name>/blob, no trailing digest) may carry up to BLOB_MAX; every other /api/
    // body keeps the 64 KiB cap. Without this widening every upload over 64 KiB would silently 413.
    let cap = if method == "PUT" && is_blob_put_path(&rest) { BLOB_MAX as usize } else { API_MAX_BODY };

    let body_bytes: Vec<u8> = if method == "GET" || clen == 0 {
        Vec::new()
    } else if clen > cap {
        return Resp::text(413, "payload too large").into_response();
    } else {
        match axum::body::to_bytes(body, cap).await {
            Ok(b) => b.to_vec(),
            Err(_) => return Resp::text(408, "request timeout").into_response(),
        }
    };

    let resp = api(&ctx, &reqo, &rest, &caller, &body_bytes).await;
    resp.into_response()
}

/// The fallback: git smart-http is matched BEFORE the SPA (via net::parse_git_path); everything else
/// GET → the SPA index; any other method → 405. Reproduces the tail of the old `handle()` exactly.
async fn git_or_spa(State(ctx): State<Ctx>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let caller = caller_of(&parts);
    let method = parts.method.as_str().to_string();
    let reqo = req_from_parts(&method, &parts.uri, &parts.headers);
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().unwrap_or("").to_string();
    let actor = caller.user.clone().unwrap_or_else(|| "anonymous".into());

    // ── git smart-http ── decide which agent first, authorize, and only then touch the body.
    if let Some(route) = net::parse_git_path(&path, &query) {
        let action = route.action;
        // A nonexistent agent is decided as "unowned private" — decision first, existence second — so
        // "doesn't exist → 404" and "private → 401" cannot be told apart by an enumerator.
        let meta = ctx.store.agent_or_unowned(&route.agent).await;
        // Fold the owning org's members so org members can clone/push org-owned agents over smart-http.
        let acl = agent_acl(&ctx, &meta).await;
        let decision = acl::decide(&caller, &acl, action);
        let exists = repo_path(ctx.root(), &route.agent).exists();
        match decision {
            Decision::Allow => {
                if !exists {
                    return Resp::text(404, "no such agent").into_response();
                }
            }
            Decision::Deny(d) => {
                audit_deny(&ctx, &actor, Some(&route.agent), action, d).await;
                // A git client only prompts for credentials on 401 + WWW-Authenticate.
                return git_deny_resp(&caller, d).into_response();
            }
        }
        // Authorized already; only now may the body be touched (an unauthorized push got a 401 above and
        // its pack never reached memory — the body-before-auth DoS).
        if reqo.content_length > MAX_BODY {
            return Resp::text(413, "payload too large").into_response();
        }
        audit_append(
            ctx.root(),
            &actor,
            if action == Action::Write { audit::GIT_PUSH } else { audit::GIT_FETCH },
            Some(&route.agent),
            &path,
        )
        .await;
        return git_http(&ctx, &reqo, body, &route.agent, &actor).await;
    }

    // ── Everything else → the SPA (client-side routing renders home/agent/session/diff off the URL). ──
    if method == "GET" {
        return Resp::new(200, "text/html; charset=utf-8", INDEX_HTML.as_bytes().to_vec()).into_response();
    }
    Resp::text(405, "method not allowed").into_response()
}
