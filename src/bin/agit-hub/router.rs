//! The axum Router, the single auth/connection middleware, the Caller extractor, the existence-
//! non-disclosure gate (verbatim), the compiled-in SPA assets, and the git-or-SPA fallback that keeps
//! git smart-http ahead of the SPA. This band replaces the hand-rolled route()/api() dispatch band.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use tower_http::trace::TraceLayer;

use agit::hub::acl::{self, Action, AgentAcl, Caller, Decision, Deny};
use agit::hub::metrics::AuthResult;
use agit::hub::blob::BLOB_MAX;
use agit::hub::net::{self, valid_agent_name};
use agit::hub::store::{valid_username, AgentMeta};
use agit::hub::{audit, auth};

use crate::api::{agent_acl, api};
use crate::cli::repo_path;
use crate::http::{authz_denied_msg, credentials, git_deny_resp, req_from_parts, Resp};
use crate::limits::{API_MAX_BODY, MAX_BODY, MAX_CONN};
use crate::server::Ctx;
use crate::smarthttp::git_http;

/// The real client IP the connection admission keyed on, stashed in request extensions by
/// `gate_conn_and_auth` so path-specific rate limits (registration) charge the same address without
/// re-deriving it. A newtype so it can't collide with any other `IpAddr` in extensions.
#[derive(Clone, Copy)]
pub(crate) struct ClientIp(pub(crate) IpAddr);

/// The per-request correlation id (`X-Request-Id`), minted once by [`observe`] and stashed in request
/// extensions so any inner handler can log it alongside its own events. A newtype so it can't collide
/// with any other `String` in extensions.
#[derive(Clone)]
pub(crate) struct RequestId(pub(crate) String);

/// Mint a fresh 16-hex-char (8-byte) correlation id from OS randomness. Per-request and random — never a
/// clock/pid alone — so two requests in the same microsecond, or across a fork, still get distinct ids.
/// A caller-supplied `X-Request-Id` is deliberately NOT honored: minting server-side keeps an attacker
/// from forging or colliding ids in the logs (log injection), and a fresh id is all correlation needs.
pub(crate) fn new_request_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    // OsRng is infallible here in practice; on the vanishingly unlikely gather failure fall back to a
    // still-unique-per-process nanosecond stamp so a request is never left without an id.
    if rand::rngs::OsRng.try_fill_bytes(&mut b).is_err() {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        b.copy_from_slice(&(n as u64).to_be_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Set the `X-Request-Id` header on a response, and — for a JSON error body (`>= 400`) — fold the id
/// into the JSON object as a `request_id` field so the client and web UI can surface it. Only small JSON
/// error bodies are buffered/rewritten; success responses (git packs, blob downloads) are never touched.
/// No request body is ever read or logged here — correlation is method/route/status/id only.
async fn stamp_request_id(mut resp: Response, rid: &str) -> Response {
    if let Ok(hv) = HeaderValue::from_str(rid) {
        resp.headers_mut().insert(HeaderName::from_static("x-request-id"), hv);
    }
    if resp.status().as_u16() < 400 {
        return resp;
    }
    let is_json = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|c| c.starts_with("application/json"))
        .unwrap_or(false);
    if !is_json {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    // Error bodies are tiny; cap the buffering so a mislabeled large body can't blow up here.
    let bytes = match axum::body::to_bytes(body, 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return Response::from_parts(parts, Body::empty()),
    };
    let new_body = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(serde_json::Value::Object(mut m)) => {
            m.insert("request_id".to_string(), serde_json::Value::String(rid.to_string()));
            serde_json::to_vec(&serde_json::Value::Object(m)).unwrap_or_else(|_| bytes.to_vec())
        }
        _ => bytes.to_vec(),
    };
    // The body length changed; drop the stale Content-Length so the server recomputes it.
    parts.headers.remove(CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(new_body))
}

// ── Frontend embedded at compile time (hub-ui/dist) ──
//
// The SPA is CODE-SPLIT (see hub-ui/vite.config.ts): the entry is `app.js`, plus a small set of
// deterministically-named chunks. The heavy, lazily-used chunks (Session/Diff/MrDetail and the
// markdown/virtualizer vendors) are fetched on demand by the browser at their `/assets/<name>.js` URL,
// so every emitted chunk must be embedded and served here. Names are hash-free and pinned by the vite
// config, so this table stays stable across rebuilds; adding a new lazy chunk means adding a row here.
pub(crate) const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/index.html"));
pub(crate) const APP_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/app.css"));
pub(crate) const FAVICON: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><text y='13' font-size='13'>◆</text></svg>";

/// One embedded static asset: (basename under /assets/, content-type, body). The whole code-split bundle
/// lives here so the single `/assets/{file}` route can serve any chunk the SPA lazily imports.
type Asset = (&'static str, &'static str, &'static str);
const JS: &str = "application/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
pub(crate) const ASSETS: &[Asset] = &[
    ("app.js", JS, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/app.js"))),
    ("app.css", CSS, APP_CSS),
    ("react-vendor.js", JS, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/react-vendor.js"))),
    ("markdown-vendor.js", JS, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/markdown-vendor.js"))),
    ("virtual-vendor.js", JS, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/virtual-vendor.js"))),
    ("Session.js", JS, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/Session.js"))),
    ("Diff.js", JS, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/Diff.js"))),
    ("MrDetail.js", JS, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hub-ui/dist/assets/MrDetail.js"))),
];

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
pub(crate) async fn gate(ctx: &Ctx, caller: &Caller, owner: &str, name: &str, action: Action) -> Result<AgentMeta, Resp> {
    // A malformed owner OR name → 404. That's not a secret: neither could ever address a real agent,
    // exactly like today's malformed-name 404 — no "no such owner" vs "no such agent" oracle.
    if !valid_username(owner) || !valid_agent_name(name) {
        return Err(Resp::err(404, "not found"));
    }
    // The fail-safe unowned/private meta is returned when the (owner, name) row is absent — identical
    // whether the OWNER account is missing, the AGENT is missing, or it exists-but-invisible, so all
    // three collapse to the same outcome for a fixed caller.
    let meta = ctx.store.agent_or_unowned(owner, name).await;
    // Fold the owning org's members in before deciding — decide itself never learns "org" exists.
    let acl = agent_acl(ctx, &meta).await;
    match acl::decide(caller, &acl, action) {
        // Repo existence is checked ONLY after Allow (post-decision), so an empty namespace still 404s
        // for an authorized caller without ever being an existence oracle for the unauthorized.
        Decision::Allow => match repo_path(ctx.root(), owner, name).exists() {
            true => Ok(meta),
            false => Err(Resp::err(404, "not found")),
        },
        Decision::Deny(d) => {
            let actor = caller.user.clone().unwrap_or_else(|| "anonymous".into());
            audit_deny(ctx, &actor, Some(&format!("{owner}/{name}")), action, d).await;
            Err(deny_resp(caller, &acl, owner, name, action, d))
        }
    }
}

pub(crate) fn deny_resp(caller: &Caller, acl: &AgentAcl, owner: &str, name: &str, action: Action, d: Deny) -> Resp {
    // Someone who can read but is denied a write/manage → tell them 403 (they already know this
    // agent exists).
    // Someone who can't even read → 404, not even admitting it exists.
    let can_read = acl::decide(caller, acl, Action::Read).allowed();
    match (d, can_read) {
        // The existence-masking arms stay BYTE-IDENTICAL: a 401/404 here must not vary by who is
        // asking, or the difference becomes an enumeration oracle for private agent names.
        (Deny::Anonymous, false) => Resp::err(401, "login required"),
        (_, false) => Resp::err(404, "not found"),
        // They can already see the agent, so nothing is left to hide — name who they are, the agent,
        // and the remedy instead of the bare reason.
        (_, true) => Resp::err(403, &authz_denied_msg(caller, owner, name, action, d)),
    }
}

/// Build the axum app: explicit asset routes, the /api dispatcher, the git-or-SPA fallback, wrapped by
/// the connection/auth middleware and the global concurrency cap.
pub(crate) fn build(ctx: Ctx) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/assets/{file}", get(asset))
        .route("/favicon.ico", get(favicon))
        .route("/api/{*rest}", any(api_entry))
        .fallback(git_or_spa)
        // Innermost first: identity + connection admission established once per request...
        .layer(middleware::from_fn_with_state(ctx.clone(), gate_conn_and_auth))
        // ...the global concurrency cap wraps it (replaces the accept-time Semaphore)...
        .layer(tower::limit::ConcurrencyLimitLayer::new(MAX_CONN))
        // ...structured request tracing sits JUST INSIDE `observe`: because `observe` runs first (it is
        // outermost) it has already minted the X-Request-Id into the request extensions, so the span this
        // layer opens reads that SAME id — every event under the span greps to the exact id the response
        // carries. It records method/path/route on open and status/latency on response (see `on_response`).
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(make_request_span)
                .on_response(|resp: &Response, latency: Duration, span: &tracing::Span| {
                    let status = resp.status().as_u16();
                    let latency_ms = latency.as_secs_f64() * 1_000.0;
                    // Fill the fields left `Empty` at span open, then emit one INFO event *inside* the span
                    // so it inherits `request_id`/`method`/`path` — this is the line that actually prints
                    // under the default `info` filter (a span alone emits nothing without span events).
                    span.record("status", status);
                    span.record("latency_ms", latency_ms);
                    tracing::info!(parent: span, status, latency_ms, "http response");
                }),
        )
        // ...and the observability layer is outermost, so it times and counts every request that
        // arrives (including any rejected by the layers within) and always runs.
        .layer(middleware::from_fn_with_state(ctx.clone(), observe))
        .with_state(ctx)
}

/// A coarse, **bounded** route label for logs. The real path carries unbounded owner/agent segments;
/// this folds it to one of a fixed handful of buckets so a log pipeline (and any future
/// route-labelled metric) can never be flooded with per-agent cardinality.
fn route_label(path: &str) -> &'static str {
    if path == "/metrics" {
        "/metrics"
    } else if path == "/favicon.ico" {
        "/favicon.ico"
    } else if path.starts_with("/assets/") {
        "/assets"
    } else if path == "/api" || path.starts_with("/api/") {
        "/api"
    } else if path == "/" {
        "/"
    } else {
        "git-or-spa"
    }
}

/// The fields lifted off a request to seed its tracing span, pulled EXACTLY as [`make_request_span`]
/// does — factored out so a unit test can assert the span is seeded from the same `RequestId`
/// [`observe`] minted (the one the response carries) WITHOUT reaching into global subscriber state.
pub(crate) struct SpanFields {
    pub(crate) request_id: String,
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) route: &'static str,
}

/// Read the span fields off a request. `request_id` is the id `observe` stashed in extensions upstream
/// (empty only if this layer were ever wired OUTSIDE `observe`); `route` is the bounded label, never the
/// unbounded path, matching `observe`'s cardinality discipline (the raw `path` is a span field, not a
/// metric label, so it carries no flood risk).
pub(crate) fn span_fields(req: &Request) -> SpanFields {
    SpanFields {
        request_id: req.extensions().get::<RequestId>().map(|r| r.0.clone()).unwrap_or_default(),
        method: req.method().as_str().to_string(),
        path: req.uri().path().to_string(),
        route: route_label(req.uri().path()),
    }
}

/// Build the structured span for one request: method, path, route, and the correlated `request_id`,
/// with `status`/`latency_ms` left `Empty` for `on_response` to fill. Named (not an inline closure) so
/// it is unit-testable via [`span_fields`].
fn make_request_span(req: &Request) -> tracing::Span {
    let f = span_fields(req);
    tracing::info_span!(
        "http_request",
        method = %f.method,
        path = %f.path,
        route = f.route,
        request_id = %f.request_id,
        status = tracing::field::Empty,
        latency_ms = tracing::field::Empty,
    )
}

/// Per-request observability: time the request, record `http_requests_total{method,status}` + the
/// latency histogram, and emit a structured start/end pair. Labels are bounded (method folded to a
/// closed set, status folded to its class inside `Metrics`); the request path is **not** used as a
/// metric label — only as a coarse `route` field on the log line — so no request can grow the metric
/// families. No secrets/PII are logged: method, coarse route, status, and latency only.
async fn observe(State(ctx): State<Ctx>, mut req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_string();
    let route = route_label(req.uri().path());
    // One correlation id per request: stashed in extensions (so inner handlers can log it) and stamped
    // onto every response below — a maintainer greps this single id to find the request end-to-end.
    let rid = new_request_id();
    req.extensions_mut().insert(RequestId(rid.clone()));
    let start = Instant::now();
    tracing::debug!(%method, route, request_id = %rid, "request start");
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    // Attach X-Request-Id to EVERY response (2xx and error alike), and fold it into JSON error bodies.
    let resp = stamp_request_id(resp, &rid).await;
    let secs = start.elapsed().as_secs_f64();
    ctx.metrics.record_request(&method, status, secs);
    let latency_ms = secs * 1_000.0;
    if status >= 500 {
        tracing::error!(%method, route, status, request_id = %rid, latency_ms, "request end");
    } else {
        tracing::info!(%method, route, status, request_id = %rid, latency_ms, "request end");
    }
    resp
}

/// `GET /metrics` — Prometheus text exposition, **admin-gated** through the same auth every other
/// route uses (the `Caller` the middleware already established). This is deliberately *not*
/// world-readable: request counts, auth-failure rates and latency are operational intelligence, so a
/// non-admin caller gets the same `404` a missing route would (non-disclosure — `/metrics` is not even
/// advertised as existing). A Prometheus scraper authenticates with an **admin user's API token**
/// (`agit-hub token add --user <admin>`), which carries `is_admin`, so no separate metrics secret or
/// extra bind is introduced.
async fn metrics_handler(State(ctx): State<Ctx>, req: Request) -> Response {
    let (parts, _body) = req.into_parts();
    let caller = caller_of(&parts);
    if !caller.is_admin {
        return Resp::err(404, "not found").into_response();
    }
    Resp::new(200, "text/plain; version=0.0.4; charset=utf-8", ctx.metrics.render().into_bytes()).into_response()
}

/// Serve one compiled-in SPA asset by basename from the [`ASSETS`] table. An unknown name is a 404 (it
/// never falls through to the SPA's index.html — a `.js` request answered with HTML would break the
/// module load). The set is closed and hash-free, so this cannot serve anything but the built bundle.
async fn asset(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    match ASSETS.iter().find(|(name, ..)| *name == file) {
        Some((_, ctype, body)) => Resp::new(200, ctype, body.as_bytes().to_vec()).into_response(),
        None => Resp::err(404, "not found").into_response(),
    }
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
    // The correlation id `observe` minted (it runs outside this layer), so the auth log lines below
    // carry the SAME id as the request-end span — one grep ties an auth event to its request.
    let rid = parts.extensions.get::<RequestId>().map(|r| r.0.clone()).unwrap_or_default();

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
        // A token that matched a real, usable grant. `id` is the token's *identifier* (e.g. `tok_1`),
        // never the plaintext secret, so it is safe to log.
        ctx.metrics.record_auth(AuthResult::TokenOk);
        tracing::info!(token_id = %id, request_id = %rid, "token accepted");
        auth::touch_token(&ctx.store, &id).await;
        if !ctx.token_rl.allow(&id) {
            return Resp::text(
                429,
                "this token is over its request budget; slow down (the limit is per token, not per address)",
            )
            .with("Retry-After", "1")
            .into_response();
        }
    } else if !secrets.is_empty() && authn.caller.user.is_none() {
        // Credentials were presented in the Authorization header but resolved to nobody: an
        // unusable/expired/unknown token (a live session would have won and set `caller.user`). Count
        // and log the denial — without the header contents, which may carry the secret.
        ctx.metrics.record_auth(AuthResult::TokenDenied);
        tracing::warn!(request_id = %rid, "token denied");
    }

    // (e) Hand the Caller to the handlers via extensions, then run the inner service with the IpGuard
    // still held across the await (RAII drop after). The resolved client IP rides along too, so the
    // registration rate limit charges the same address the connection limiter did.
    parts.extensions.insert(authn.caller);
    if let Some(ip) = client {
        parts.extensions.insert(ClientIp(ip));
    }
    let req = Request::from_parts(parts, body);
    next.run(req).await
}

/// Pull the Caller the middleware established out of extensions (infallible; middleware always sets it).
fn caller_of(parts: &axum::http::request::Parts) -> Caller {
    parts.extensions.get::<Caller>().cloned().unwrap_or_else(Caller::anonymous)
}

/// Whether `rest` (the path after `/api/`) is the blob-upload endpoint `agent/<owner>/<name>/blob` —
/// an owner then a name segment, then exactly `/blob`, no trailing digest. Mirrors the handler's PUT
/// arm so the body cap and the route agree. A GET download carries the digest tail (and no body), so
/// it never matches.
fn is_blob_put_path(rest: &str) -> bool {
    let Some(mid) = rest.strip_prefix("agent/").and_then(|r| r.strip_suffix("/blob")) else {
        return false;
    };
    matches!(mid.split_once('/'), Some((o, n)) if !o.is_empty() && !n.is_empty() && !n.contains('/'))
}

/// `/api/*` dispatcher. Rebuilds the [`Req`] view, enforces the API body cap, then runs the async
/// `api()` string-dispatcher directly on the runtime (its store reads/writes are awaited).
async fn api_entry(State(ctx): State<Ctx>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let caller = caller_of(&parts);
    let client_ip = parts.extensions.get::<ClientIp>().map(|c| c.0);
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

    let resp = api(&ctx, &reqo, &rest, &caller, client_ip, &body_bytes).await;
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
        let (owner, name) = (route.owner.as_str(), route.name.as_str());
        let scoped = format!("{owner}/{name}");
        // A nonexistent agent is decided as "unowned private" — decision first, existence second — so
        // "doesn't exist → 404" and "private → 401" cannot be told apart by an enumerator, and neither
        // can a missing owner from a missing agent (the fail-safe is identical for all three).
        let meta = ctx.store.agent_or_unowned(owner, name).await;
        // Fold the owning org's members so org members can clone/push org-owned agents over smart-http.
        let acl = agent_acl(&ctx, &meta).await;
        let decision = acl::decide(&caller, &acl, action);
        let exists = repo_path(ctx.root(), owner, name).exists();
        match decision {
            Decision::Allow => {
                if !exists {
                    return Resp::text(404, "no such agent").into_response();
                }
            }
            Decision::Deny(d) => {
                audit_deny(&ctx, &actor, Some(&scoped), action, d).await;
                // A push (receive-pack) refused at the authorization gate. Counted by outcome only —
                // `scoped`/`actor` go to the structured log (bounded metric labels, richer logs).
                if action == Action::Write {
                    ctx.metrics.record_git_push(false);
                    tracing::warn!(agent = %scoped, actor = %actor, reason = %d.reason(), "git push rejected");
                }
                // A git client only prompts for credentials on 401 + WWW-Authenticate.
                return git_deny_resp(&caller, owner, name, action, d).into_response();
            }
        }
        // Authorized already; only now may the body be touched (an unauthorized push got a 401 above and
        // its pack never reached memory — the body-before-auth DoS).
        if reqo.content_length > MAX_BODY {
            return Resp::text(413, "payload too large").into_response();
        }
        // Push authorized (the secret scan still runs later in the out-of-process pre-receive hook;
        // this counts the authorization outcome, which is the in-process signal we have).
        if action == Action::Write {
            ctx.metrics.record_git_push(true);
            tracing::info!(agent = %scoped, actor = %actor, "git push accepted");
        }
        audit_append(
            ctx.root(),
            &actor,
            if action == Action::Write { audit::GIT_PUSH } else { audit::GIT_FETCH },
            Some(&scoped),
            &path,
        )
        .await;
        return git_http(&ctx, &reqo, body, owner, name, &actor).await;
    }

    // ── Everything else → the SPA (client-side routing renders home/agent/session/diff off the URL). ──
    if method == "GET" {
        return Resp::new(200, "text/html; charset=utf-8", INDEX_HTML.as_bytes().to_vec()).into_response();
    }
    Resp::text(405, "method not allowed").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_span_reuses_the_request_id_observe_minted() {
        // The TraceLayer span must be seeded from the SAME RequestId `observe` stashes in the request
        // extensions, so any log line under the span greps to the exact X-Request-Id the response carries.
        // Asserting the builder wiring here proves the correlation deterministically, without reading
        // global subscriber state.
        let mut req = Request::builder().method("POST").uri("/api/agents/x").body(Body::empty()).unwrap();
        req.extensions_mut().insert(RequestId("deadbeefcafe0123".into()));
        let f = span_fields(&req);
        assert_eq!(f.request_id, "deadbeefcafe0123", "the span must reuse observe's request id");
        assert_eq!(f.method, "POST");
        assert_eq!(f.path, "/api/agents/x");
        assert_eq!(f.route, "/api", "the unbounded path folds to a bounded route label");
        // The named builder actually constructs a span (not a disabled one) from those fields.
        let span = make_request_span(&req);
        assert!(!span.is_disabled(), "make_request_span must produce a live span");
    }

    #[test]
    fn request_span_degrades_without_a_request_id() {
        // If this layer were ever wired OUTSIDE `observe` (no RequestId in extensions), the builder must
        // degrade to an empty id rather than panic — the span still forms, just uncorrelated.
        let bare = Request::builder().uri("/").body(Body::empty()).unwrap();
        let f = span_fields(&bare);
        assert_eq!(f.request_id, "", "no minted id yet → empty, never a panic");
        assert_eq!(f.route, "/");
    }
}
