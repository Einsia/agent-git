//! JSON API handlers + shared helpers (sync bodies returning Resp). Verbatim from the monolith.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::io;
use std::process::Command;

use agit::hub::acl::{self, Action, AgentAcl, Caller, Decision, Deny, Lifecycle, Role, Scope, Visibility};
use agit::hub::blob::{self, BLOB_MAX};
use agit::hub::net::valid_agent_name;
use agit::hub::store::{AgentMeta, Member, Org, OrgMember, User};
use agit::hub::{audit, auth, identity, kdf, mr, session as websession, store};

use crate::cli::{create_agent, issue_token, list_agents, repo_path};
use crate::gitplumb::*;
use crate::content::{api_compare, api_diff, api_raw, api_session, session_summary};
use crate::http::{Req, Resp};
use crate::router::{audit_append, audit_deny, gate};
use crate::scan::install_pre_receive;
use crate::server::Ctx;

pub(crate) const PER_PAGE: usize = 20;

/// One line about an agent, for the list. The README is where prose goes; this is a label.
pub(crate) const DESCRIPTION_MAX: usize = 300;

/// The largest page a caller may ask for. Asking for more is not an error worth failing on, but it
/// is not an instruction either.
pub(crate) const PAGE_MAX: usize = 100;

pub(crate) struct Page {
    pub(crate) limit: usize,
    pub(crate) after: Option<String>,
}

pub(crate) fn page_params(query: &str) -> Result<Page, Resp> {
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
pub(crate) fn cursor_encode(key: &str) -> String {
    key.bytes().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn cursor_decode(c: &str) -> Option<String> {
    if c.is_empty() || !c.is_ascii() || !c.len().is_multiple_of(2) || c.len() > 512 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..c.len()).step_by(2).map(|i| u8::from_str_radix(&c[i..i + 2], 16).ok()).collect();
    String::from_utf8(bytes?).ok()
}

/// The lifecycle/ownership verbs, so the route table can name them instead of matching strings twice.
#[derive(Clone, Copy)]
pub(crate) enum Verb {
    Fork,
    Transfer,
    Archive,
    Unarchive,
    Restore,
    Star,
}
/// How many sessions a query may scan at most (stops an unbounded git show). Going over is flagged
/// in the response rather than silently truncated.
pub(crate) const SEARCH_SCAN_CAP: usize = 400;

pub(crate) async fn api(ctx: &Ctx, req: &Req, rest: &str, caller: &Caller, body: &[u8]) -> Resp {
    let m = req.method.as_str();
    match (m, rest) {
        ("POST", "login") => return api_login(ctx, req, body).await,
        ("POST", "register") => return api_register(ctx, body).await,
        ("POST", "logout") => return api_logout(ctx, req, caller).await,
        ("GET", "me") => return api_me(caller),
        ("GET", "agents") => return api_agents(ctx, req, caller).await,
        ("POST", "agents") => return api_create_agent(ctx, req, caller, body).await,
        ("GET", "tokens") => return api_tokens(ctx, caller).await,
        ("POST", "tokens") => return api_create_token(ctx, caller, body).await,
        ("GET", "audit") => return api_audit(ctx, req, caller).await,
        ("GET", "search") => return api_search(ctx, req, caller).await,
        ("GET", "orgs") => return api_orgs_list(ctx, caller).await,
        ("POST", "orgs") => return api_orgs_create(ctx, caller, body).await,
        _ => {}
    }
    // orgs/<name> and orgs/<name>/members[/<username>] — before the agent/ block, since an org name
    // is not an agent name.
    if let Some(after) = rest.strip_prefix("orgs/") {
        if let Some((name, tail)) = after.split_once("/members") {
            if tail.is_empty() || tail.starts_with('/') {
                return api_org_members(ctx, caller, name, tail, m, body).await;
            }
        }
        return match m {
            "GET" => api_org_get(ctx, caller, after).await,
            _ => Resp::text(405, "method not allowed"),
        };
    }
    if let Some(id) = rest.strip_prefix("tokens/") {
        return match m {
            "DELETE" => api_revoke_token(ctx, caller, id).await,
            _ => Resp::text(405, "method not allowed"),
        };
    }
    let Some(after) = rest.strip_prefix("agent/") else {
        return Resp::err(404, "not found");
    };

    // agent/by-aid/<aid> — identity → current name. Before the owner peel + name routes, since
    // `by-aid` is owner-less (a real owner segment is always followed by a name).
    if let Some(aid) = after.strip_prefix("by-aid/") {
        return match m {
            "GET" => api_agent_by_aid(ctx, req, caller, aid).await,
            _ => Resp::text(405, "method not allowed"),
        };
    }

    // Peel the owner namespace segment: everything past here is `<owner>/<name>...`. Identity is
    // (owner, name), so (owner, name) is threaded into `gate` at every sub-route below.
    let Some((owner, after)) = after.split_once('/') else {
        return Resp::err(404, "not found");
    };

    // agent/<owner>/<name>/mrs[/<id>[/comments|/close]]
    if let Some((name, tail)) = after.split_once("/mrs") {
        if tail.is_empty() || tail.starts_with('/') {
            return api_mrs(ctx, caller, owner, name, tail, m, req.query(), body).await;
        }
    }

    // agent/<owner>/<name>/raw/<path> and agent/<owner>/<name>/compare — both read the store's bytes,
    // so both go through the Read gate first, like every other entry point.
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
        let meta = match gate(ctx, caller, owner, name, Action::Read).await {
            Ok(x) => x,
            Err(r) => return r,
        };
        // has_head + the content endpoint each shell out to git; run them on the blocking pool.
        let (root, seg, name, tail, query, is_raw) =
            (ctx.root().to_path_buf(), meta.seg().to_string(), meta.name.clone(), tail.to_string(), req.query().to_string(), sep == "/raw/");
        return tokio::task::spawn_blocking(move || {
            let repo = repo_path(&root, &seg, &name);
            if !has_head(&repo) {
                return Resp::err(404, "not found");
            }
            if is_raw {
                api_raw(&repo, &tail, &query)
            } else {
                api_compare(&repo, &query)
            }
        })
        .await
        .unwrap();
    }

    // agent/<owner>/<name>/session/<id>[/diff]
    if let Some((name, tail)) = after.split_once("/session/") {
        if m != "GET" {
            return Resp::text(405, "method not allowed");
        }
        let meta = match gate(ctx, caller, owner, name, Action::Read).await {
            Ok(x) => x,
            Err(r) => return r,
        };
        let (root, seg, name, tail, query) =
            (ctx.root().to_path_buf(), meta.seg().to_string(), meta.name.clone(), tail.to_string(), req.query().to_string());
        return tokio::task::spawn_blocking(move || {
            let repo = repo_path(&root, &seg, &name);
            if !has_head(&repo) {
                return Resp::err(404, "not found");
            }
            match tail.strip_suffix("/diff") {
                Some(id) => api_diff(&repo, id, &query),
                None => api_session(&repo, &tail, &query),
            }
        })
        .await
        .unwrap();
    }

    // agent/<owner>/<name>/blob            (PUT) — content-addressed upload, Write-gated.
    // agent/<owner>/<name>/blob/<digest>   (GET) — content-addressed download, Read-gated.
    // Same "read/write the store's bytes → through gate()" band as /raw/ + /compare above; placed after
    // /session/ so a session literally named "blob" can never be shadowed. The tail is either empty
    // (PUT) or `/`+digest (GET); a `/blobXYZ` falls through, unmatched.
    if let Some((name, tail)) = after.split_once("/blob") {
        if tail.is_empty() {
            return match m {
                "PUT" => api_blob_put(ctx, req, caller, owner, name, body).await,
                "GET" => Resp::text(405, "method not allowed"), // GET needs a /<digest>
                _ => Resp::text(405, "method not allowed"),
            };
        }
        if let Some(digest) = tail.strip_prefix('/') {
            if !digest.contains('/') {
                return match m {
                    "GET" => api_blob_get(ctx, caller, owner, name, digest).await,
                    _ => Resp::text(405, "method not allowed"),
                };
            }
        }
    }

    // agent/<owner>/<name>/members[/<username>] — tail may only be empty or /<username>;
    // don't let /membersXYZ pass as /members.
    if let Some((name, tail)) = after.split_once("/members") {
        if tail.is_empty() || tail.starts_with('/') {
            return api_members(ctx, caller, owner, name, tail, m, body).await;
        }
    }

    // agent/<owner>/<name>/<verb> — the lifecycle verbs. Each is its own route rather than a PATCH
    // field: they are events with their own audit rows and their own legal predecessors, not attributes.
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
                Verb::Fork => api_fork_agent(ctx, req, caller, owner, name, body).await,
                Verb::Transfer => api_transfer_agent(ctx, caller, owner, name, body).await,
                Verb::Archive => {
                    set_lifecycle(ctx, caller, owner, name, Lifecycle::Archived, &[Lifecycle::Active], audit::AGENT_ARCHIVE).await
                }
                Verb::Unarchive => {
                    set_lifecycle(ctx, caller, owner, name, Lifecycle::Active, &[Lifecycle::Archived], audit::AGENT_UNARCHIVE).await
                }
                // Restore lands on Active, not on "whatever it was": an agent coming back from the
                // trash writable is the surprise; coming back and needing one more click is not.
                Verb::Restore => {
                    set_lifecycle(ctx, caller, owner, name, Lifecycle::Active, &[Lifecycle::Deleted], audit::AGENT_RESTORE).await
                }
                Verb::Star => api_star_agent(ctx, caller, owner, name, body).await,
            };
        }
    }

    // agent/<owner>/<name>
    match m {
        "GET" => {
            let meta = match gate(ctx, caller, owner, after, Action::Read).await {
                Ok(x) => x,
                Err(r) => return r,
            };
            api_agent(ctx, req, caller, &meta).await
        }
        "PATCH" => api_patch_agent(ctx, caller, owner, after, body).await,
        "DELETE" => api_delete_agent(ctx, caller, owner, after, req.query()).await,
        _ => Resp::text(405, "method not allowed"),
    }
}

// ── Authentication ──

pub(crate) async fn api_login(ctx: &Ctx, req: &Req, body: &[u8]) -> Resp {
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let (Some(username), Some(password)) = (str_field(&v, "username"), str_field(&v, "password")) else {
        return Resp::err(400, "want username and password");
    };
    // argon2 is slow on purpose — leaving its concurrency uncapped hands out a CPU/memory amplifier.
    // Hold an async permit across the verify (which runs the KDF on the blocking pool): the wait for a
    // slot yields the worker rather than blocking it.
    let verified = {
        let _slot = ctx.login_gate.acquire().await.expect("login gate semaphore is never closed");
        auth::verify_login(&ctx.store, &username, &password).await
    };
    let Some(user) = verified else {
        audit_append(ctx.root(), &store::normalize_username(&username), audit::LOGIN_FAILED, None, &req.host()).await;
        // Don't say whether the user doesn't exist or the password is wrong — that hands the
        // brute-forcer a username dictionary.
        return Resp::err(401, "wrong username or password");
    };
    let Ok(sid) = ctx.sessions.create(&user.username) else {
        return Resp::err(503, "couldn't create a session, try again shortly");
    };
    audit_append(ctx.root(), &user.username, audit::LOGIN, None, "").await;
    Resp::json(serde_json::json!({ "username": user.username, "is_admin": user.is_admin }))
        .with("Set-Cookie", &websession::set_cookie(&sid, ctx.cfg.tls))
}

pub(crate) async fn api_logout(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    if let Some(sid) = req.sid() {
        ctx.sessions.revoke(&sid);
    }
    if let Some(u) = &caller.user {
        audit_append(ctx.root(), u, audit::LOGOUT, None, "").await;
    }
    Resp::no_content().with("Set-Cookie", &websession::clear_cookie(ctx.cfg.tls))
}

pub(crate) fn api_me(caller: &Caller) -> Resp {
    match &caller.user {
        Some(u) => Resp::json(serde_json::json!({ "username": u, "is_admin": caller.is_admin })),
        None => Resp::err(401, "not logged in"),
    }
}

/// `POST /api/register` — self-service signup. Creates a **normal, non-admin** user and logs them in
/// with a session cookie, mirroring the CLI's `user add` crypto (cli.rs) + `api_login`'s session
/// issuance. Off unless the operator enabled it (`ctx.cfg.registration`).
///
/// **Security invariant**: `is_admin` is hardcoded `false`, and no admin field is ever read from the
/// body — registration can never grant admin. Admin stays CLI-only (`agit-hub user add --admin`).
pub(crate) async fn api_register(ctx: &Ctx, body: &[u8]) -> Resp {
    // Config gate FIRST — before any crypto — so a disabled hub spends nothing on a signup attempt.
    if !ctx.cfg.registration {
        return Resp::err(403, "self-service registration is disabled on this hub");
    }
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let (Some(raw), Some(password)) = (str_field(&v, "username"), str_field(&v, "password")) else {
        return Resp::err(400, "want username and password");
    };
    let username = store::normalize_username(&raw);
    if !store::valid_username(&username) {
        return Resp::err(400, "invalid username (2-32 lowercase [a-z0-9._-], no leading dot)");
    }
    if store::is_reserved_account(&username) {
        return Resp::err(400, "that name is reserved");
    }
    // Unified account namespace: a username and an org name may never share a bare string.
    if ctx.store.org(&username).await.is_some() {
        return Resp::err(409, "that username is taken");
    }
    // Same minimum as the CLI's read_new_password, via the one shared constant.
    if password.chars().count() < store::MIN_PASSWORD_LEN {
        return Resp::err(400, "password too short (at least 8 characters)");
    }
    let Ok(salt) = kdf::gen_salt() else {
        return Resp::err(500, "no system entropy available, try again shortly");
    };
    let kdf_id = kdf::current_kdf_id();
    // Run the argon2 hash under the login gate + blocking pool, reusing the login handler's CPU/memory
    // amplifier defense (argon2 is deliberately slow; uncapped concurrency is a DoS lever).
    let (pw, salt2, kdf2) = (password.clone(), salt.clone(), kdf_id.clone());
    let hashed = {
        let _slot = ctx.login_gate.acquire().await.expect("login gate semaphore is never closed");
        tokio::task::spawn_blocking(move || kdf::hash_password(&pw, &salt2, &kdf2)).await.unwrap()
    };
    let Some(pw_hash) = hashed else {
        return Resp::err(500, "password derivation failed");
    };
    // is_admin: false is the security invariant — never read any admin field from the body.
    let user = User { username: username.clone(), pw_hash, salt, kdf: kdf_id, is_admin: false, created: store::now_iso() };
    match ctx.store.add_user(user).await {
        Ok(()) => {}
        // The username PRIMARY KEY makes this race-safe: a duplicate is always a clean AlreadyExists,
        // never a 500.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Resp::err(409, "that username is taken"),
        Err(_) => return Resp::err(500, "couldn't create the account"),
    }
    let Ok(sid) = ctx.sessions.create(&username) else {
        return Resp::err(503, "couldn't create a session, try again shortly");
    };
    audit_append(ctx.root(), &username, audit::USER_REGISTER, None, "").await;
    Resp::json(serde_json::json!({ "username": username, "is_admin": false }))
        .with("Set-Cookie", &websession::set_cookie(&sid, ctx.cfg.tls))
}

/// Resolve an agent's effective ACL, folding in the owning org's members when it is org-owned. **This
/// is the one place org membership is expanded**, before `acl::decide` runs — so decide stays pure and
/// never learns "org" exists. Fail-closed: a missing/unreadable org folds nobody in.
pub(crate) async fn agent_acl(ctx: &Ctx, meta: &AgentMeta) -> AgentAcl {
    let org = match meta.org_owner() {
        Some(n) => ctx.store.org(n).await,
        None => None,
    };
    meta.to_acl_with_org(org.as_ref())
}

// ── agents ──

pub(crate) async fn api_agents(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    let page = match page_params(req.query()) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // What you can't see doesn't make the list — the list is the first answer to "who may see whose
    // agent", and it is also what makes archived agents show and deleted ones vanish, since both are
    // decided in the same place.
    //
    // Filtered before paging, never after: a page that hides its rejects would hand out short pages
    // and let a caller infer, from the gaps, exactly how many agents they cannot see. (A loop, not an
    // iterator chain: the ACL read is an async store call.)
    // The ACL filter needs each agent's metadata (an async store read), so collect (name, meta) as we
    // go; the meta is reused when the row is built.
    // The list is keyed on `full_name` = `<owner_ns>/<name>`, which IS unique (a name is unique only
    // within an owner), so paging and the cursor stay stable when two owners share a name.
    let mut visible: Vec<(String, String, AgentMeta)> = Vec::new();
    for (seg, n) in list_agents(ctx.root()) {
        let meta = ctx.store.agent_or_unowned(&seg, &n).await;
        if !acl::decide(caller, &agent_acl(ctx, &meta).await, Action::Read).allowed() {
            continue;
        }
        let full = format!("{seg}/{n}");
        if page.after.as_deref().is_none_or(|a| full.as_str() > a) {
            visible.push((full, seg, meta));
        }
    }
    let has_more = visible.len() > page.limit;
    let window: Vec<(String, String, AgentMeta)> = visible.into_iter().take(page.limit).collect();
    let next_cursor = has_more.then(|| window.last().map(|(full, _, _)| cursor_encode(full))).flatten();

    // The per-agent git reads (has_head / last_activity / session_refs / agent_aid) each shell out, so
    // one request over N agents is ~3N subprocesses. Run the whole fan-out on the blocking pool and
    // hand back the collected fields; the store reads above already ran async.
    let root = ctx.root().to_path_buf();
    let paths: Vec<(String, String)> = window.iter().map(|(_, seg, m)| (seg.clone(), m.name.clone())).collect();
    let git_info: Vec<(usize, String, String, Option<String>, &'static str)> = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|(seg, n)| {
                let repo = repo_path(&root, &seg, &n);
                let (count, when, subject) = if has_head(&repo) {
                    let (w, s) = last_activity(&repo);
                    (session_refs(&repo).len(), w, s)
                } else {
                    (0, String::new(), String::new())
                };
                let (aid, aid_source) = agent_aid(&repo);
                (count, when, subject, aid, aid_source)
            })
            .collect()
    })
    .await
    .unwrap();

    let mut items: Vec<serde_json::Value> = Vec::with_capacity(window.len());
    for ((full, _seg, meta), (count, when, subject, aid, aid_source)) in window.iter().zip(git_info) {
        items.push(serde_json::json!({
            "name": meta.name,
            "owner": meta.owner,
            "full_name": full,
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
            "role": effective_role(caller, meta),
        }));
    }
    Resp::json(serde_json::json!({
        "agents": items,
        "host": req.host(),
        "has_more": has_more,
        "next_cursor": next_cursor,
    }))
}

/// The caller's **effective** role on this agent, for the UI to decide which buttons to show.
/// null = no explicit grant (they can see it only because it's public).
pub(crate) fn effective_role(caller: &Caller, meta: &AgentMeta) -> Option<&'static str> {
    let user = caller.user.as_deref()?;
    if meta.owner.as_deref() == Some(user) {
        return Some("owner");
    }
    if caller.is_admin {
        return Some("admin");
    }
    meta.role_of(user).map(|r| r.as_str())
}

pub(crate) async fn api_create_agent(ctx: &Ctx, req: &Req, caller: &Caller, body: &[u8]) -> Resp {
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
    // Optional org owner: when present, the agent is owned by "org:<name>" and only an org admin may
    // create it. Absent → the caller owns it, exactly as before.
    let org = match str_field(&v, "org") {
        Some(orgname) => match ctx.store.org(&orgname).await {
            // Existence non-disclosure (mirrors api_org_get / api_org_members): a missing org and one
            // the caller can't see (not a member, not a site admin) both return the SAME 404, so create
            // can't be used to probe which orgs exist. A member who merely lacks org-admin still gets a
            // distinct 403 below — they already know the org exists, so that leaks nothing.
            Some(o) if caller.is_admin || o.is_member(&user) => Some(o),
            _ => return Resp::err(404, "not found"),
        },
        None => None,
    };
    if let Some(o) = &org {
        if !o.is_admin(&user) {
            return Resp::err(403, "must be an org admin to create agents under it");
        }
    }
    let owner = match &org {
        Some(o) => format!("org:{}", o.name),
        None => user.clone(),
    };
    // Creating a repo goes through the same decision: treat it as "writing to an agent I own" —
    // so a token bound to another agent, or a read-only token, can't create anything. Under an org the
    // hypothetical folds the org members, so the caller passes via their folded Admin role (keeping the
    // single-gate pattern); decide never sees the "org:" owner directly.
    let hypothetical = match &org {
        Some(o) => AgentMeta::new(&name, Some(&owner), visibility).to_acl_with_org(Some(o)),
        None => AgentAcl { name: name.clone(), owner: Some(user.clone()), visibility, lifecycle: Lifecycle::Active, members: vec![] },
    };
    if let Decision::Deny(d) = acl::decide(caller, &hypothetical, Action::Write) {
        audit_deny(ctx, &user, Some(&name), Action::Write, d).await;
        return Resp::err(403, d.reason());
    }
    let seg = store::owner_ns(&owner).to_string();
    match create_agent(&ctx.store, &name, &owner, visibility).await {
        Ok(_) => {
            audit_append(ctx.root(), &user, audit::AGENT_CREATE, Some(&format!("{seg}/{name}")), &format!("visibility={} owner={owner}", visibility.as_str())).await;
            let repo = repo_path(ctx.root(), &seg, &name);
            let (aid, aid_source) = tokio::task::spawn_blocking(move || agent_aid(&repo)).await.unwrap();
            Resp::json_status(
                201,
                serde_json::json!({
                    "name": name,
                    "owner": owner,
                    "full_name": format!("{seg}/{name}"),
                    // An empty repo has no agent.toml yet — the aid only exists once the client
                    // pushes it. Report null honestly.
                    "aid": aid,
                    "aid_source": aid_source,
                    "clone_url": clone_url(ctx, req, &seg, &name),
                    "visibility": visibility.as_str(),
                }),
            )
        }
        Err(e) => Resp::err(409, &e),
    }
}

pub(crate) fn clone_url(ctx: &Ctx, req: &Req, owner_ns: &str, name: &str) -> String {
    format!("{}://{}/{owner_ns}/{name}.git", if ctx.cfg.tls { "https" } else { "http" }, req.host())
}

/// Everything the agent-detail response reads out of the repo (git + fs). Collected inside one
/// `spawn_blocking` so the whole subprocess fan-out stays off the async workers.
struct AgentView {
    sessions: Vec<serde_json::Value>,
    history: Vec<serde_json::Value>,
    readme: Option<String>,
    environments: Vec<serde_json::Value>,
    branches: Vec<serde_json::Value>,
    size_bytes: u64,
    runtimes: Vec<String>,
    total: usize,
    scanned: usize,
    scan_capped: bool,
}

pub(crate) async fn api_agent(ctx: &Ctx, req: &Req, caller: &Caller, meta: &AgentMeta) -> Resp {
    let name = &meta.name;
    let query = req.query();
    let search = param(query, "q").map(|q| q.replace('+', " ")).unwrap_or_default();
    let pageno: usize = param(query, "page").and_then(|p| p.parse().ok()).unwrap_or(1).max(1);

    // The aid reconcile is a store read+write, so it runs async (it offloads its own git read). Its
    // result is independent of the session/history reads below, so ordering it first is fine.
    let (aid, aid_source, aid_status) = sync_aid(ctx, meta, &caller.user.clone().unwrap_or_else(|| "anonymous".into())).await;

    // All the per-repo git/fs reads (has_head, session_refs, load_session, session_summary,
    // recent_log, readme, environments, branches, size_bytes, runtimes) shell out — one bounded
    // spawn_blocking does the lot and returns the rendered pieces.
    let root = ctx.root().to_path_buf();
    let seg_for_git = meta.seg().to_string();
    let name_for_git = meta.name.clone();
    let search2 = search.clone();
    let v: AgentView = tokio::task::spawn_blocking(move || {
        let repo = repo_path(&root, &seg_for_git, &name_for_git);
        let refs = if has_head(&repo) { session_refs(&repo) } else { vec![] };
        // The hit set: no search = page straight through (git show only the current page); with a
        // search = scan the content (capped).
        let (window, total): (Vec<&SessionRef>, usize) = if search2.is_empty() {
            let start = (pageno - 1) * PER_PAGE;
            (refs.iter().skip(start).take(PER_PAGE).collect(), refs.len())
        } else {
            let mut hits = vec![];
            for r in refs.iter().take(SEARCH_SCAN_CAP) {
                if load_session(&repo, &r.path, None).map(|b| b.contains(&search2)).unwrap_or(false) {
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
        AgentView {
            sessions,
            history,
            readme: readme(&repo),
            environments: environments(&repo, &refs),
            branches: branches(&repo),
            size_bytes: size_bytes(&repo),
            runtimes: runtimes(&refs),
            total,
            // With a search, `total` counts the hits among the sessions actually scanned — so say how
            // many that was, and whether the cap cut it short. The count alone cannot tell you.
            scanned: if search2.is_empty() { refs.len() } else { refs.len().min(SEARCH_SCAN_CAP) },
            scan_capped: !search2.is_empty() && refs.len() > SEARCH_SCAN_CAP,
        }
    })
    .await
    .unwrap();

    let members: Vec<serde_json::Value> = meta
        .members
        .iter()
        .map(|m| serde_json::json!({ "username": m.username, "role": m.role }))
        .collect();

    Resp::json(serde_json::json!({
        "agent": name,
        "full_name": meta.scoped(),
        "git": format!("/{}/{name}.git", meta.seg()),
        "aid": aid,
        "aid_source": aid_source,
        "aid_status": aid_status,
        "clone_url": clone_url(ctx, req, meta.seg(), name),
        "visibility": meta.visibility,
        "lifecycle": meta.lifecycle().as_str(),
        "description": meta.description,
        "forked_from": meta.forked_from,
        "readme": v.readme,
        "stars": meta.stars.len(),
        "starred": caller.user.as_ref().is_some_and(|u| meta.stars.contains(u)),
        "owner": meta.owner,
        "members": members,
        "role": effective_role(caller, meta),
        "environments": v.environments,
        "branches": v.branches,
        "size_bytes": v.size_bytes,
        "runtimes": v.runtimes,
        "total": v.total,
        "page": pageno,
        "per_page": PER_PAGE,
        "scanned": v.scanned,
        "scan_cap": SEARCH_SCAN_CAP,
        "scan_capped": v.scan_capped,
        "sessions": v.sessions,
        "history": v.history,
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
pub(crate) async fn sync_aid(ctx: &Ctx, meta: &AgentMeta, actor: &str) -> (Option<String>, &'static str, &'static str) {
    let seg = meta.seg().to_string();
    let name = meta.name.clone();
    // The agent's scoped identity `<owner_ns>/<name>` — reconcile compares identities as strings, so
    // passing the scoped id (self AND holder) keeps two same-named agents under different owners from
    // being mistaken for one another, without reconcile needing to know about owners.
    let self_id = meta.scoped();
    let repo = repo_path(ctx.root(), &seg, &name);
    // agent_aid shells out to `git show HEAD:agent.toml`; keep it off the async worker.
    let (seen, source) = tokio::task::spawn_blocking(move || agent_aid(&repo)).await.unwrap();

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
    let mut verdict = identity::AidVerdict::Unchanged;
    // Whether this read is the one that *entered* the conflict, as opposed to the millionth to
    // observe it. Only the transition is an event; see `AgentMeta::aid_conflict`.
    let mut newly_conflicted = false;
    // The cache write stays best-effort, as before: the store is the authority, so a verdict whose
    // write failed is still the truth about what was read, and the next sync reconciles again.
    let _ = ctx
        .store
        .update_agents(|list| {
            let cached = list.iter().find(|m| m.matches(&seg, &name)).and_then(|m| m.aid.clone());
            // The holder is looked up by aid (globally unique) and identified by its scoped id, so a
            // same-named agent under a different owner is not mistaken for "this agent already holds it".
            let holder = seen
                .as_deref()
                .and_then(|a| list.iter().find(|m| m.aid.as_deref() == Some(a)))
                .map(|m| m.scoped());
            let lineage = list.iter().find(|m| m.matches(&seg, &name)).and_then(|m| m.forked_from_aid.clone());
            verdict = identity::reconcile(&self_id, cached.as_deref(), seen.as_deref(), holder.as_deref(), lineage.as_deref());
            let Some(m) = list.iter_mut().find(|m| m.matches(&seg, &name)) else {
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
        })
        .await;

    match verdict {
        // Re-read under the lock, the cache already agreed — the race this section exists to close.
        identity::AidVerdict::Unchanged => match seen {
            Some(a) => (Some(a), source, "ok"),
            None => (None, source, "ok"),
        },
        identity::AidVerdict::Learn(a) => {
            audit_append(ctx.root(), actor, audit::AGENT_AID_LEARNED, Some(&self_id), &a).await;
            (Some(a), source, "learned")
        }
        identity::AidVerdict::Replaced { was, now } => {
            // The store is the authority, so the cache follows it — but the response only says
            // "replaced" this once, and the audit log is what makes it still findable tomorrow.
            audit_append(ctx.root(), actor, audit::AGENT_AID_REPLACED, Some(&self_id), &format!("{was} → {now}")).await;
            (Some(now), source, "replaced")
        }
        identity::AidVerdict::Conflict { aid, held_by } => {
            // **Only on the transition.** A conflict is a state, re-derived on every read; auditing
            // each observation grew audit.log without bound and buried the one row an operator
            // alerts on under thousands of copies of itself — so polling a conflicted agent became a
            // way to drown out the alert that names you.
            if newly_conflicted {
                audit_append(
                    ctx.root(),
                    actor,
                    audit::AGENT_AID_CONFLICT,
                    Some(&self_id),
                    &format!("{aid} is already held by {held_by}"),
                )
                .await;
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
pub(crate) async fn api_agent_by_aid(ctx: &Ctx, req: &Req, caller: &Caller, aid: &str) -> Resp {
    if !identity::is_aid(aid) {
        return Resp::err(400, "not an aid (want agt_<id>)");
    }
    // Unresolvable and unreadable must look the same, for the same reason gate() hides existence:
    // otherwise this endpoint enumerates the private agents by aid instead of by name.
    let Some(meta) = ctx.store.agent_by_aid(aid).await else {
        return Resp::err(404, "not found");
    };
    let seg = meta.seg().to_string();
    let meta = match gate(ctx, caller, &seg, &meta.name, Action::Read).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    Resp::json(serde_json::json!({
        "aid": aid,
        "name": meta.name,
        "owner": meta.owner,
        "full_name": meta.scoped(),
        "clone_url": clone_url(ctx, req, meta.seg(), &meta.name),
        "visibility": meta.visibility,
    }))
}

pub(crate) async fn api_patch_agent(ctx: &Ctx, caller: &Caller, owner: &str, name: &str, body: &[u8]) -> Resp {
    let meta = match gate(ctx, caller, owner, name, Action::Manage).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let seg = meta.seg().to_string(); // the namespace a rename stays within
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let actor = caller.user.clone().unwrap_or_default();

    if let Some(vis) = v.get("visibility").and_then(|x| x.as_str()) {
        let Some(vis) = Visibility::parse(vis) else {
            return Resp::err(400, "visibility must be private or public");
        };
        if vis.as_str() != meta.visibility {
            if let Err(resp) = edit_agent(ctx, &seg, &meta.name, |m| m.visibility = vis.as_str().to_string()).await {
                return resp;
            }
            audit_append(ctx.root(), &actor, audit::AGENT_VISIBILITY, Some(&meta.scoped()), &format!("{} → {}", meta.visibility, vis.as_str())).await;
        }
    }

    // `{"description": ""}` clears it — an explicit empty string is a real instruction, and the only
    // way to take a description back off.
    if let Some(d) = v.get("description").and_then(|x| x.as_str()) {
        let d = match mr::bounded(d, DESCRIPTION_MAX) {
            Ok(x) => x,
            Err(e) => return Resp::err(400, &format!("description {e}")),
        };
        if let Err(resp) = edit_agent(ctx, &seg, &meta.name, |m| m.description = d.clone()).await {
            return resp;
        }
        audit_append(ctx.root(), &actor, audit::AGENT_DESCRIBE, Some(&meta.scoped()), d.as_deref().unwrap_or("(cleared)")).await;
    }

    if let Some(newname) = str_field(&v, "name") {
        if newname != meta.name {
            if !valid_agent_name(&newname) {
                return Resp::err(400, "invalid name ([A-Za-z0-9._-] only, no .. and no leading dot)");
            }
            // A rename stays within the same owner namespace — only the name half moves.
            if name_taken(ctx, &seg, &newname).await {
                return Resp::err(409, "that name is already taken");
            }
            // Reserve the new name atomically — check and rename the record together under the lock, so
            // two renames to one name can't both land. Done BEFORE moving the repo dir, so a lost race
            // fails before touching the filesystem. (The `name_taken` above is only a fast fail.)
            //
            // A rename is a metadata edit, not a new identity: only the label moves. The aid is
            // deliberately untouched (it lives in the store's agent.toml), so everything keyed on
            // identity survives.
            let reserved = ctx
                .store
                .update_agents(|list| {
                    if list.iter().any(|m| m.matches(&seg, &newname)) {
                        return false;
                    }
                    if let Some(m) = list.iter_mut().find(|m| m.matches(&seg, &meta.name)) {
                        m.name = newname.clone();
                    }
                    true
                })
                .await;
            match reserved {
                Ok(true) => {}
                Ok(false) => return Resp::err(409, "that name is already taken"),
                Err(_) => return Resp::err(500, "failed to persist the agent"),
            }
            // Move the repo dir to match the record (a blocking fs op, off the async worker). On
            // failure, roll the name back so the record and the directory never disagree.
            let from = repo_path(ctx.root(), &seg, &meta.name);
            let to = repo_path(ctx.root(), &seg, &newname);
            let moved = tokio::task::spawn_blocking(move || std::fs::rename(from, to).is_ok()).await.unwrap_or(false);
            if !moved {
                let _ = ctx
                    .store
                    .update_agents(|list| {
                        if let Some(m) = list.iter_mut().find(|m| m.matches(&seg, &newname)) {
                            m.name = meta.name.clone();
                        }
                    })
                    .await;
                return Resp::err(500, "rename failed (the repo directory won't move)");
            }
            // Blobs are keyed by (owner_ns, name), so they must follow the rename or they are stranded
            // under the old name and unreachable. Mirror the repo-dir move exactly: on failure, undo the
            // repo move and roll the name back, so record, repo dir and blobs never disagree.
            if let Err(e) = ctx.blobs.rename_agent((&seg, &meta.name), (&seg, &newname)).await {
                eprintln!("blob rename failed for {seg}/{} → {seg}/{newname}: {e}", meta.name);
                let (undo_from, undo_to) = (repo_path(ctx.root(), &seg, &newname), repo_path(ctx.root(), &seg, &meta.name));
                let _ = tokio::task::spawn_blocking(move || std::fs::rename(undo_from, undo_to)).await;
                let _ = ctx
                    .store
                    .update_agents(|list| {
                        if let Some(m) = list.iter_mut().find(|m| m.matches(&seg, &newname)) {
                            m.name = meta.name.clone();
                        }
                    })
                    .await;
                return Resp::err(500, "rename failed (couldn't move the agent's blobs)");
            }
            // Tokens are bound to the **scoped id** `<owner_ns>/<name>`. A rename doesn't change identity
            // (the aid lives in the store), so the bindings have to follow — otherwise one rename
            // silently mutes every CI token.
            let (old_scoped, new_scoped) = (format!("{seg}/{}", meta.name), format!("{seg}/{newname}"));
            let _ = ctx
                .store
                .update_tokens(|toks| {
                    for t in toks.iter_mut().filter(|t| t.agent.as_deref() == Some(old_scoped.as_str())) {
                        t.agent = Some(new_scoped.clone());
                    }
                })
                .await;
            // MR endpoints carry owner + name; the name is a label within the namespace and has to follow.
            let _ = ctx.store.rename_in_mrs(&seg, &meta.name, &newname).await;
            audit_append(ctx.root(), &actor, audit::AGENT_RENAME, Some(&new_scoped), &format!("{} → {newname}", meta.name)).await;
            // Echo the aid back: the whole point of the rename being safe is that identity did not
            // move, and a caller should be able to see that rather than take it on faith.
            return Resp::json(serde_json::json!({ "name": newname, "renamed_from": meta.name, "aid": meta.aid }));
        }
    }

    let fresh = ctx.store.agent_or_unowned(&seg, &meta.name).await;
    Resp::json(serde_json::json!({ "name": fresh.name, "visibility": fresh.visibility, "owner": fresh.owner, "full_name": fresh.scoped() }))
}

/// Is this name spoken for? **Includes soft-deleted agents**, whose whole point is that the name
/// stays theirs: hand it to someone else and the restore has nowhere to land, while every token and
/// `.agit.toml` still pointing at the name silently starts addressing a stranger's agent.
pub(crate) async fn name_taken(ctx: &Ctx, seg: &str, name: &str) -> bool {
    ctx.store.agent_scoped(seg, name).await.is_some() || repo_path(ctx.root(), seg, name).exists()
}

/// Mutate the record for the agent `(seg, name)` under the lock, mapping a write failure to a 500. The
/// find / err-to-500 boilerplate that every field-editing handler otherwise repeats.
pub(crate) async fn edit_agent(ctx: &Ctx, seg: &str, name: &str, f: impl FnOnce(&mut AgentMeta)) -> Result<(), Resp> {
    ctx.store
        .update_agents(|list| {
            if let Some(m) = list.iter_mut().find(|m| m.matches(seg, name)) {
                f(m);
            }
        })
        .await
        .map_err(|_| Resp::err(500, "failed to persist the agent"))
}

/// Move an agent between lifecycle states. The state itself is enforced in `acl::decide` — this only
/// writes it down.
pub(crate) async fn set_lifecycle(ctx: &Ctx, caller: &Caller, owner: &str, name: &str, to: Lifecycle, from: &[Lifecycle], action: &'static str) -> Resp {
    let meta = match gate(ctx, caller, owner, name, Action::Manage).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Refusing the no-op transition is what makes each of these verbs mean something: "restore" on a
    // live agent is a caller who thinks it was deleted, and answering 204 would agree with them.
    if !from.contains(&meta.lifecycle()) {
        return Resp::err(409, &format!("this agent is {}", meta.lifecycle().as_str()));
    }
    if let Err(resp) = edit_agent(ctx, meta.seg(), &meta.name, |m| m.lifecycle = to.as_str().to_string()).await {
        return resp;
    }
    let actor = caller.user.clone().unwrap_or_default();
    audit_append(ctx.root(), &actor, action, Some(&meta.scoped()), &format!("{} → {}", meta.lifecycle().as_str(), to.as_str())).await;
    Resp::json(serde_json::json!({ "name": meta.name, "full_name": meta.scoped(), "lifecycle": to.as_str(), "aid": meta.aid }))
}

/// `DELETE /api/agent/<name>` — **soft**. The repo, the tokens, the MRs and the name all survive; the
/// agent simply stops being findable (`acl::decide` denies everything but Manage on a deleted agent).
///
/// Destroying the bytes is `?purge=true`, and only from here — two steps, because the one-step version
/// of this is how a memory nobody meant to lose gets lost.
pub(crate) async fn api_delete_agent(ctx: &Ctx, caller: &Caller, owner: &str, name: &str, query: &str) -> Resp {
    if param(query, "purge").as_deref() == Some("true") {
        return api_purge_agent(ctx, caller, owner, name).await;
    }
    set_lifecycle(ctx, caller, owner, name, Lifecycle::Deleted, &[Lifecycle::Active, Lifecycle::Archived], audit::AGENT_DELETE).await
}

/// The irreversible one: the bytes go. Only reachable for an already soft-deleted agent, so nothing
/// live can be destroyed by a single mistyped verb.
pub(crate) async fn api_purge_agent(ctx: &Ctx, caller: &Caller, owner: &str, name: &str) -> Resp {
    let meta = match gate(ctx, caller, owner, name, Action::Manage).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if meta.lifecycle() != Lifecycle::Deleted {
        return Resp::err(409, "purge only empties the trash: delete this agent first, then purge it");
    }
    let seg = meta.seg().to_string();
    let scoped = meta.scoped();
    let dir = repo_path(ctx.root(), &seg, &meta.name);
    let removed = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir).is_ok()).await.unwrap_or(false);
    if !removed {
        return Resp::err(500, "can't remove the repo directory");
    }
    // Blobs are keyed by (owner_ns, name). Destroy them with the repo, and BEFORE the record is dropped:
    // leaving them behind is the recycled-name leak — a NEW agent later created under this same
    // (owner, name) would pass the gate and read the previous owner's PRIVATE blobs. Surfacing a failure
    // as a 500 while the record still stands means the name can't be recycled yet, so the leak never
    // opens — the same recycled-name reasoning the tokens/MRs cleanup documents.
    if let Err(e) = ctx.blobs.delete_agent(&seg, &meta.name).await {
        eprintln!("blob purge failed for {scoped}: {e}");
        return Resp::err(500, "can't remove the agent's blobs");
    }
    let _ = ctx.store.update_agents(|list| list.retain(|m| !m.matches(&seg, &meta.name))).await;
    // Tokens bound to this SCOPED id must die with it: otherwise a recycled (owner, name) would let old
    // tokens automatically gain rights on the new agent.
    let _ = ctx.store.update_tokens(|toks| toks.retain(|t| t.agent.as_deref() != Some(scoped.as_str()))).await;
    // Same reasoning for MRs targeting it: a recycled name must not inherit the old agent's reviews.
    let _ = ctx.store.update_mrs(|mrs| mrs.retain(|m| !(m.target.owner == seg && m.target.agent == meta.name))).await;
    audit_append(ctx.root(), &caller.user.clone().unwrap_or_default(), audit::AGENT_PURGE, Some(&scoped), "").await;
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
pub(crate) async fn api_fork_agent(ctx: &Ctx, req: &Req, caller: &Caller, owner: &str, name: &str, body: &[u8]) -> Resp {
    // You cannot fork what you cannot read — otherwise fork is an oracle for private agents, and a
    // way to walk off with one.
    let source = match gate(ctx, caller, owner, name, Action::Read).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    // A fork is a write the caller performs, so a read-only token must not get to do it.
    if caller.token.as_ref().is_some_and(|t| t.scope != Scope::Write) {
        audit_deny(ctx, &user, Some(&source.scoped()), Action::Write, Deny::TokenScope).await;
        return Resp::err(403, Deny::TokenScope.reason());
    }
    // The fork is owned by (and namespaced under) the caller — its segment is the caller's username.
    let fork_seg = user.clone();
    let fork = match json_body(body).as_ref().and_then(|v| str_field(v, "name")) {
        Some(n) => n,
        None => format!("{}-fork", source.name),
    };
    if !valid_agent_name(&fork) {
        return Resp::err(400, "invalid name ([A-Za-z0-9._-] only, no .. and no leading dot)");
    }
    if name_taken(ctx, &fork_seg, &fork).await {
        return Resp::err(409, "that name is already taken");
    }
    let dst = repo_path(ctx.root(), &fork_seg, &fork);
    // `git clone --bare` + the two `git config`/`remote` calls + install_pre_receive + reading the
    // source's aid all shell out or touch the fs. Run the whole lot on the blocking pool: it returns
    // None if the clone failed (already cleaned up), else the source's aid (the fork's lineage).
    let clone_out: Option<Option<String>> = {
        let src = repo_path(ctx.root(), source.seg(), &source.name);
        let dst = dst.clone();
        let root = ctx.root().to_path_buf();
        let (fork_seg, fork) = (fork_seg.clone(), fork.clone());
        tokio::task::spawn_blocking(move || {
            let ok = Command::new("git")
                .args(["clone", "-q", "--bare"])
                .arg(&src)
                .arg(&dst)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                let _ = std::fs::remove_dir_all(&dst);
                return None;
            }
            let _ = Command::new("git").arg("-C").arg(&dst).args(["config", "http.receivepack", "true"]).status();
            // A bare clone records its origin. The fork is its own agent on its own disk — leaving a
            // remote pointing at the source would make its `--not --all` scan bound, and its pushes
            // routable, through somebody else's repo.
            let _ = Command::new("git").arg("-C").arg(&dst).args(["remote", "remove", "origin"]).status();
            install_pre_receive(&dst, &root, &fork_seg, &fork);
            // The identity the clone carries. Recorded as lineage so `identity::reconcile` can tell
            // this fork's inherited aid from a stolen one — see `AgentMeta::forked_from_aid`. Read from
            // the source repo rather than from `source.aid`, which is only the Hub's cache.
            let (src_aid, _) = agent_aid(&src);
            Some(src_aid)
        })
        .await
        .unwrap()
    };
    let Some(src_aid) = clone_out else {
        return Resp::err(500, "git clone --bare failed");
    };
    // Private by default, whatever the source was: forking a public agent is not a decision to
    // publish your copy of it.
    // Authoritative name check, atomic with the insert. The `name_taken` above is only a fast fail; a
    // fork that raced us to this name between there and here must not produce a second record.
    let r = ctx
        .store
        .update_agents(|list| {
            if list.iter().any(|a| a.matches(&fork_seg, &fork)) {
                return false;
            }
            list.push(AgentMeta {
                forked_from: Some(source.name.clone()),
                forked_from_aid: src_aid.clone(),
                description: source.description.clone(),
                ..AgentMeta::new(&fork, Some(&user), Visibility::Private)
            });
            true
        })
        .await;
    match r {
        Ok(true) => {}
        Ok(false) => {
            let d = dst.clone();
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(d)).await;
            return Resp::err(409, "that name is already taken");
        }
        Err(_) => {
            let d = dst.clone();
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(d)).await;
            return Resp::err(500, "failed to persist the agent");
        }
    }
    audit_append(ctx.root(), &user, audit::AGENT_FORK, Some(&format!("{fork_seg}/{fork}")), &format!("forked from {}", source.scoped())).await;
    let (aid, aid_source) = tokio::task::spawn_blocking(move || agent_aid(&dst)).await.unwrap();
    Resp::json_status(
        201,
        serde_json::json!({
            "name": fork,
            "forked_from": source.name,
            "owner": user,
            "full_name": format!("{fork_seg}/{fork}"),
            "visibility": Visibility::Private.as_str(),
            "clone_url": clone_url(ctx, req, &fork_seg, &fork),
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
pub(crate) async fn api_star_agent(ctx: &Ctx, caller: &Caller, owner: &str, name: &str, body: &[u8]) -> Resp {
    let meta = match gate(ctx, caller, owner, name, Action::Read).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let actor = match mutation_actor(ctx, caller, &meta.scoped()).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let on = json_body(body).and_then(|v| v.get("starred").and_then(|x| x.as_bool())).unwrap_or(true);
    let who = actor.clone();
    if let Err(resp) = edit_agent(ctx, meta.seg(), &meta.name, |m| {
        m.stars.retain(|u| u != &who);
        if on {
            m.stars.push(who.clone());
        }
    })
    .await
    {
        return resp;
    }
    audit_append(ctx.root(), &actor, audit::AGENT_STAR, Some(&meta.scoped()), if on { "starred" } else { "unstarred" }).await;
    let fresh = ctx.store.agent_or_unowned(meta.seg(), &meta.name).await;
    Resp::json(serde_json::json!({ "name": meta.name, "full_name": meta.scoped(), "starred": on, "stars": fresh.stars.len() }))
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
/// Move an agent's on-disk storage (repo dir + blobs) from `old_seg` to `new_seg`, keeping the same
/// `name`. Storage-first with the same rollback discipline as the rename in `api_patch_agent`: on a
/// blob-move failure the repo dir is moved back, so the two never disagree. A no-op when the segment
/// is unchanged.
async fn move_agent_storage(ctx: &Ctx, old_seg: &str, new_seg: &str, name: &str) -> Result<(), Resp> {
    if old_seg == new_seg {
        return Ok(());
    }
    let from = repo_path(ctx.root(), old_seg, name);
    let to = repo_path(ctx.root(), new_seg, name);
    let to2 = to.clone();
    let moved = tokio::task::spawn_blocking(move || {
        if let Some(p) = to2.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        std::fs::rename(&from, &to2).is_ok()
    })
    .await
    .unwrap_or(false);
    if !moved {
        return Err(Resp::err(500, "transfer failed (the repo directory won't move)"));
    }
    if let Err(e) = ctx.blobs.rename_agent((old_seg, name), (new_seg, name)).await {
        eprintln!("blob transfer failed for {old_seg}/{name} → {new_seg}/{name}: {e}");
        let (uf, ut) = (repo_path(ctx.root(), new_seg, name), repo_path(ctx.root(), old_seg, name));
        let _ = tokio::task::spawn_blocking(move || std::fs::rename(uf, ut)).await;
        return Err(Resp::err(500, "transfer failed (couldn't move the agent's blobs)"));
    }
    Ok(())
}

/// Carry an agent's MR endpoints across an ownership change (the name is unchanged; only the namespace
/// segment moves). Mirrors `rename_in_mrs`, but for the owner half.
async fn retarget_mrs_owner(ctx: &Ctx, old_seg: &str, new_seg: &str, name: &str) {
    let (old_seg, new_seg, name) = (old_seg.to_string(), new_seg.to_string(), name.to_string());
    let _ = ctx
        .store
        .update_mrs(|mrs| {
            for m in mrs.iter_mut() {
                if m.target.agent == name && m.target.owner == old_seg {
                    m.target.owner = new_seg.clone();
                }
                if m.source.agent == name && m.source.owner == old_seg {
                    m.source.owner = new_seg.clone();
                }
            }
        })
        .await;
}

pub(crate) async fn api_transfer_agent(ctx: &Ctx, caller: &Caller, owner: &str, name: &str, body: &[u8]) -> Resp {
    let meta = match gate(ctx, caller, owner, name, Action::Manage).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let old_seg = meta.seg().to_string();
    let actor = caller.user.clone().unwrap_or_default();
    let bodyv = json_body(body);
    // Transfer to an org (mutually exclusive with `to`): the owner becomes "org:<name>" and access
    // flows to the org's members via folding. The caller must belong to the target org, so an agent
    // can't be dumped onto an org you have no part in.
    if let Some(orgname) = bodyv.as_ref().and_then(|v| str_field(v, "org")) {
        // Existence non-disclosure (mirrors api_org_get): membership IS the permission to transfer here,
        // so a missing org and one the caller isn't a member of collapse to the SAME 404 — transfer
        // can't be used to probe which orgs exist. The successful (member/site-admin) path is unchanged.
        let org = match ctx.store.org(&orgname).await {
            Some(o) if caller.is_admin || o.is_member(&actor) => o,
            _ => return Resp::err(404, "not found"),
        };
        let new_owner = format!("org:{}", org.name);
        let new_seg = org.name.clone(); // owner_ns of "org:acme" is "acme"
        if meta.owner.as_deref() == Some(new_owner.as_str()) {
            return Resp::err(409, &format!("this agent is already owned by org:{}", org.name));
        }
        if name_taken(ctx, &new_seg, name).await {
            return Resp::err(409, &format!("org:{} already has an agent named {name}", org.name));
        }
        let from = meta.owner.clone();
        // An owner change now moves the storage namespace: repo dir + blobs first, then the metadata.
        if let Err(r) = move_agent_storage(ctx, &old_seg, &new_seg, name).await {
            return r;
        }
        if let Err(resp) = edit_agent(ctx, &old_seg, name, |m| {
            m.owner = Some(new_owner.clone());
            // Drop any stale membership row that shares the org's bare name — the org grant supersedes.
            m.members.retain(|x| x.username != org.name);
        })
        .await
        {
            // Roll the storage back so record and disk never disagree.
            let _ = move_agent_storage(ctx, &new_seg, &old_seg, name).await;
            return resp;
        }
        retarget_mrs_owner(ctx, &old_seg, &new_seg, name).await;
        audit_append(
            ctx.root(),
            &actor,
            audit::AGENT_TRANSFER,
            Some(&format!("{new_seg}/{name}")),
            &format!("{}/{name} → {new_owner}", old_seg),
        )
        .await;
        return Resp::json(serde_json::json!({
            "name": name,
            "owner": new_owner,
            "full_name": format!("{new_seg}/{name}"),
            "previous_owner": from,
            "aid": meta.aid,
        }));
    }
    let Some(to) = bodyv.as_ref().and_then(|v| str_field(v, "to")) else {
        return Resp::err(400, "want to (the username to transfer ownership to) or org (an org name)");
    };
    let to = store::normalize_username(&to);
    // Only a real, existing user — the same rule members follow, and for the same reason: an agent
    // owned by a name nobody holds is an agent whoever registers that name later inherits.
    if ctx.store.user(&to).await.is_none() {
        return Resp::err(400, &format!("no such user: {to}"));
    }
    if meta.owner.as_deref() == Some(to.as_str()) {
        return Resp::err(409, &format!("{to} already owns this agent"));
    }
    // The new owner's namespace segment is their bare username.
    if name_taken(ctx, &to, name).await {
        return Resp::err(409, &format!("{to} already has an agent named {name}"));
    }
    let (from, target) = (meta.owner.clone(), to.clone());
    if let Err(r) = move_agent_storage(ctx, &old_seg, &to, name).await {
        return r;
    }
    if let Err(resp) = edit_agent(ctx, &old_seg, name, |m| {
        m.owner = Some(target.clone());
        // The new owner's membership row, if any, is now noise at best and a demotion at worst (owner
        // outranks every role) — drop it rather than leave two answers to "what may they do".
        m.members.retain(|x| x.username != target);
    })
    .await
    {
        let _ = move_agent_storage(ctx, &to, &old_seg, name).await;
        return resp;
    }
    retarget_mrs_owner(ctx, &old_seg, &to, name).await;
    audit_append(
        ctx.root(),
        &actor,
        audit::AGENT_TRANSFER,
        Some(&format!("{to}/{name}")),
        &format!("{old_seg}/{name} → {to}"),
    )
    .await;
    Resp::json(serde_json::json!({
        "name": name,
        "owner": to,
        "full_name": format!("{to}/{name}"),
        "previous_owner": from,
        // The point of a transfer being safe is that identity did not move. Say so, rather than
        // leaving the caller to take it on faith.
        "aid": meta.aid,
    }))
}

// ── members ──

pub(crate) async fn api_members(ctx: &Ctx, caller: &Caller, owner: &str, name: &str, tail: &str, method: &str, body: &[u8]) -> Resp {
    let actor = caller.user.clone().unwrap_or_default();
    // GET only needs read (the member list is already shown to readers in the agent detail);
    // adding/removing needs Manage.
    let action = if method == "GET" { Action::Read } else { Action::Manage };
    let meta = match gate(ctx, caller, owner, name, action).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let seg = meta.seg().to_string();

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
            if ctx.store.user(&username).await.is_none() {
                return Resp::err(400, "no such user");
            }
            if meta.owner.as_deref() == Some(username.as_str()) {
                return Resp::err(400, "the owner already has every right; no membership needed");
            }
            if let Err(resp) = edit_agent(ctx, &seg, &meta.name, |m| {
                match m.members.iter_mut().find(|x| x.username == username) {
                    Some(x) => x.role = role.as_str().to_string(),
                    None => m.members.push(Member { username: username.clone(), role: role.as_str().to_string() }),
                }
            })
            .await
            {
                return resp;
            }
            audit_append(ctx.root(), &actor, audit::MEMBER_ADD, Some(&meta.scoped()), &format!("{username}={}", role.as_str())).await;
            let fresh = ctx.store.agent_or_unowned(&seg, &meta.name).await;
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
                .update_agents(|list| match list.iter_mut().find(|m| m.matches(&seg, &meta.name)) {
                    Some(m) => {
                        let before = m.members.len();
                        m.members.retain(|x| x.username != username);
                        before != m.members.len()
                    }
                    None => false,
                })
                .await
                .unwrap_or(false);
            if !removed {
                return Resp::err(404, "that person isn't a member");
            }
            audit_append(ctx.root(), &actor, audit::MEMBER_REMOVE, Some(&meta.scoped()), &username).await;
            Resp::no_content()
        }
        _ => Resp::text(405, "method not allowed"),
    }
}

// ── organizations ──

/// Serialize an org's member list for the API.
fn org_members_json(org: &Org) -> serde_json::Value {
    serde_json::json!(org
        .members
        .iter()
        .map(|m| serde_json::json!({ "username": m.username, "role": m.role }))
        .collect::<Vec<_>>())
}

/// `GET /api/orgs` — the orgs the caller belongs to (site admin sees all). You only see orgs you are a
/// member of, which prevents enumerating org membership.
pub(crate) async fn api_orgs_list(ctx: &Ctx, caller: &Caller) -> Resp {
    let Some(user) = caller.user.as_deref() else {
        return Resp::err(401, "login required");
    };
    let items: Vec<serde_json::Value> = ctx
        .store
        .orgs()
        .await
        .into_iter()
        .filter(|o| caller.is_admin || o.is_member(user))
        .map(|o| serde_json::json!({ "name": o.name, "created": o.created, "members": org_members_json(&o) }))
        .collect();
    Resp::json(serde_json::json!(items))
}

/// `POST /api/orgs` — create an org. The creator becomes its first (and only) admin. Org names use the
/// same rules as usernames, keeping them clean and echo-safe.
pub(crate) async fn api_orgs_create(ctx: &Ctx, caller: &Caller, body: &[u8]) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(name) = str_field(&v, "name") else {
        return Resp::err(400, "want name");
    };
    let name = store::normalize_username(&name);
    if !store::valid_username(&name) {
        return Resp::err(400, "invalid org name (2-32 lowercase [a-z0-9._-], no leading dot)");
    }
    if store::is_reserved_account(&name) {
        return Resp::err(400, "that name is reserved");
    }
    // Unified account namespace (GitHub-style): an org and a user may never share a bare name, so the
    // `owner_ns` segment resolves to exactly one account.
    if ctx.store.user(&name).await.is_some() {
        return Resp::err(409, "that name is already taken by a user");
    }
    // Atomic create: the existence check and the push run together under the orgs lock, so two racing
    // creates of one name can't both land.
    let created = ctx
        .store
        .update_orgs(|list| {
            if list.iter().any(|o| o.name == name) {
                return false;
            }
            list.push(Org {
                name: name.clone(),
                members: vec![OrgMember { username: user.clone(), role: "admin".into() }],
                created: store::now_iso(),
            });
            true
        })
        .await;
    match created {
        Ok(true) => {
            audit_append(ctx.root(), &user, audit::ORG_CREATE, None, &format!("org={name}")).await;
            Resp::json_status(
                201,
                serde_json::json!({ "name": name, "members": [{ "username": user, "role": "admin" }] }),
            )
        }
        Ok(false) => Resp::err(409, "that org name is taken"),
        Err(_) => Resp::err(500, "couldn't create the org"),
    }
}

/// `GET /api/orgs/<name>` — org detail. Existence non-disclosure, the same shape as the agent gate: a
/// missing org and one the caller may not see both answer 404, so org names cannot be enumerated.
pub(crate) async fn api_org_get(ctx: &Ctx, caller: &Caller, name: &str) -> Resp {
    let org = ctx.store.org(name).await;
    let visible = |o: &Org| caller.is_admin || caller.user.as_deref().is_some_and(|u| o.is_member(u));
    match org {
        Some(o) if visible(&o) => {
            Resp::json(serde_json::json!({ "name": o.name, "created": o.created, "members": org_members_json(&o) }))
        }
        _ => Resp::err(404, "not found"),
    }
}

/// `/api/orgs/<name>/members[/<username>]` — the org membership routes. Authorization here is an ORG
/// gate (`is_admin` on the org), NOT `acl::decide` — decide stays agent-only. Managing members needs
/// org-admin (or site admin); listing needs only membership.
pub(crate) async fn api_org_members(ctx: &Ctx, caller: &Caller, name: &str, tail: &str, method: &str, body: &[u8]) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    // 404 for a missing org OR one the caller can't see — existence non-disclosure, as above.
    let Some(org) = ctx.store.org(name).await else {
        return Resp::err(404, "not found");
    };
    if !(caller.is_admin || org.is_member(&user)) {
        return Resp::err(404, "not found");
    }
    let can_manage = caller.is_admin || org.is_admin(&user);
    let target = tail.strip_prefix('/').map(|s| s.to_string());
    match (method, target) {
        // Any member (or site admin) may see the roster.
        ("GET", None) => Resp::json(org_members_json(&org)),
        ("POST", None) => {
            if !can_manage {
                return Resp::err(403, "must be an org admin to manage members");
            }
            let Some(v) = json_body(body) else {
                return Resp::err(400, "want a JSON body");
            };
            let (Some(username), Some(role)) = (str_field(&v, "username"), str_field(&v, "role")) else {
                return Resp::err(400, "want username and role");
            };
            let username = store::normalize_username(&username);
            if role != "member" && role != "admin" {
                return Resp::err(400, "role must be member or admin");
            }
            // Only real, existing users — the same rule agent members follow, so an org can't collect
            // misspelled names that whoever registers them later inherits.
            if ctx.store.user(&username).await.is_none() {
                return Resp::err(400, "no such user");
            }
            let orgname = org.name.clone();
            if let Err(_e) = ctx
                .store
                .update_orgs(|list| {
                    if let Some(o) = list.iter_mut().find(|o| o.name == orgname) {
                        match o.members.iter_mut().find(|m| m.username == username) {
                            Some(m) => m.role = role.clone(),
                            None => o.members.push(OrgMember { username: username.clone(), role: role.clone() }),
                        }
                    }
                })
                .await
            {
                return Resp::err(500, "couldn't update the org");
            }
            audit_append(ctx.root(), &user, audit::ORG_MEMBER_ADD, None, &format!("org={} {username}={role}", org.name)).await;
            let fresh = ctx.store.org(&org.name).await.unwrap_or(org);
            Resp::json(org_members_json(&fresh))
        }
        ("DELETE", Some(target_user)) => {
            if !can_manage {
                return Resp::err(403, "must be an org admin to manage members");
            }
            let target_user = store::normalize_username(&target_user);
            let orgname = org.name.clone();
            // Guard against orphaning the org: refuse removing its last admin. Checked inside the lock,
            // so it can't race another concurrent demotion.
            let outcome = ctx
                .store
                .update_orgs(|list| {
                    let Some(o) = list.iter_mut().find(|o| o.name == orgname) else {
                        return RmOutcome::NotMember;
                    };
                    if !o.members.iter().any(|m| m.username == target_user) {
                        return RmOutcome::NotMember;
                    }
                    let admins = o.members.iter().filter(|m| m.role == "admin").count();
                    let target_is_admin = o.members.iter().any(|m| m.username == target_user && m.role == "admin");
                    if admins == 1 && target_is_admin {
                        return RmOutcome::LastAdmin;
                    }
                    o.members.retain(|m| m.username != target_user);
                    RmOutcome::Removed
                })
                .await
                .unwrap_or(RmOutcome::NotMember);
            match outcome {
                RmOutcome::Removed => {
                    audit_append(ctx.root(), &user, audit::ORG_MEMBER_REMOVE, None, &format!("org={} {target_user}", org.name)).await;
                    Resp::no_content()
                }
                RmOutcome::LastAdmin => Resp::err(409, "an org must keep at least one admin"),
                RmOutcome::NotMember => Resp::err(404, "that person isn't a member"),
            }
        }
        _ => Resp::text(405, "method not allowed"),
    }
}

/// The result of an org member removal, so the last-admin guard and the not-a-member case can be told
/// apart after the atomic `update_orgs`.
enum RmOutcome {
    Removed,
    LastAdmin,
    NotMember,
}

// ── cross-agent search ──

/// Sessions scanned across the whole query, all agents together. Each one costs a `git show` plus a
/// parse, so this — not the agent count — is the thing worth bounding.
pub(crate) const XSEARCH_SCAN_CAP: usize = 400;
/// Hits returned. Past this the scan stops early: nobody reads hit 200, and the work is real.
pub(crate) const XSEARCH_MAX_HITS: usize = 50;

/// `GET /api/search?q=` — one query across **every agent the caller may read**, over the fields
/// people actually remember: what they asked, what came back, which files were touched.
///
/// The permission is per agent and decided by `acl::decide`, exactly like everywhere else: an agent
/// you cannot read contributes nothing, and cannot even be inferred from a hit count.
pub(crate) async fn api_search(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    let q = param(req.query(), "q").map(|q| q.replace('+', " ")).unwrap_or_default();
    let q = q.trim().to_lowercase();
    if q.len() < 2 {
        return Resp::err(400, "want q, at least 2 characters");
    }
    // Decide readability async (each per-agent ACL is a store read), then hand the whole
    // subprocess-heavy scan (has_head + session_refs + load_session + digest, capped) to the blocking
    // pool in one shot.
    let mut readable: Vec<(String, String, Option<String>)> = Vec::new();
    for (seg, name) in list_agents(ctx.root()) {
        let meta = ctx.store.agent_or_unowned(&seg, &name).await;
        if acl::decide(caller, &agent_acl(ctx, &meta).await, Action::Read).allowed() {
            readable.push((seg, name, meta.aid.clone()));
        }
    }
    let root = ctx.root().to_path_buf();
    let qc = q.clone();
    let (hits, scanned, capped): (Vec<serde_json::Value>, usize, bool) = tokio::task::spawn_blocking(move || {
        let mut hits: Vec<serde_json::Value> = vec![];
        let mut scanned = 0usize;
        let mut capped = false;
        'agents: for (seg, name, aid) in &readable {
            let repo = repo_path(&root, seg, name);
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
                if d.prompts.iter().any(|p| p.to_lowercase().contains(&qc)) {
                    fields.push("prompt");
                }
                if conclusion.to_lowercase().contains(&qc) {
                    fields.push("conclusion");
                }
                if d.files.iter().any(|f| f.to_lowercase().contains(&qc)) {
                    fields.push("file");
                }
                if fields.is_empty() {
                    continue;
                }
                hits.push(serde_json::json!({
                    "agent": name,
                    "owner": seg,
                    "full_name": format!("{seg}/{name}"),
                    "aid": aid,
                    "id": d.id,
                    "env": r.env,
                    "runtime": r.runtime,
                    "matched": fields,
                    "title": d.prompts.first().map(|s| first_line(s)).unwrap_or_default(),
                    "conclusion": clip(&conclusion, 200),
                    "files": d.files.iter().filter(|f| f.to_lowercase().contains(&qc)).take(5).cloned().collect::<Vec<_>>(),
                }));
            }
        }
        (hits, scanned, capped)
    })
    .await
    .unwrap();

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
#[allow(clippy::too_many_arguments)] // owner+name is the identity now; both must thread through
pub(crate) async fn api_mrs(ctx: &Ctx, caller: &Caller, owner: &str, name: &str, tail: &str, method: &str, query: &str, body: &[u8]) -> Resp {
    // The action this route needs, decided **before** the agent is fetched, so the gate is the first
    // thing that happens on every path.
    let action = match (method, tail) {
        ("GET", _) => Action::Read,
        ("POST", "") => Action::Write,
        ("POST", t) if t.ends_with("/comments") => Action::Read,
        ("POST", t) if t.ends_with("/close") => Action::Write,
        _ => return Resp::text(405, "method not allowed"),
    };
    let meta = match gate(ctx, caller, owner, name, action).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Every POST below mutates hub state, whichever tier `gate` authorized it at.
    let actor = match method {
        "POST" => match mutation_actor(ctx, caller, &meta.scoped()).await {
            Ok(a) => a,
            Err(r) => return r,
        },
        _ => caller.user.clone().unwrap_or_default(),
    };

    match (method, tail) {
        ("GET", "") => api_mr_list(ctx, caller, &meta, query).await,
        ("POST", "") => api_mr_open(ctx, caller, &meta, &actor, body).await,
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
                ("GET", "") => api_mr_detail(ctx, caller, &meta, id).await,
                ("POST", "comments") => api_mr_comment(ctx, &meta, id, &actor, body).await,
                ("POST", "close") => api_mr_close(ctx, caller, &meta, id, &actor, body).await,
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
pub(crate) async fn mutation_actor(ctx: &Ctx, caller: &Caller, scoped: &str) -> Result<String, Resp> {
    let Some(actor) = caller.user.clone() else {
        audit_deny(ctx, "anonymous", Some(scoped), Action::Write, Deny::Anonymous).await;
        return Err(Resp::err(401, "login required"));
    };
    if caller.token.as_ref().is_some_and(|t| t.scope != Scope::Write) {
        audit_deny(ctx, &actor, Some(scoped), Action::Write, Deny::TokenScope).await;
        return Err(Resp::err(403, Deny::TokenScope.reason()));
    }
    Ok(actor)
}

/// The list view: no transcripts. They are the big field, and nobody reading an index wants every
/// merge dialogue on the agent shipped along with it.
pub(crate) async fn api_mr_list(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, query: &str) -> Resp {
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
        ctx.store.mrs_for(meta.seg(), &meta.name).await.into_iter().filter(|m| after.is_none_or(|a| m.id > a)).collect();
    let has_more = all.len() > page.limit;
    let window: Vec<mr::Mr> = all.into_iter().take(page.limit).collect();
    let next_cursor = has_more.then(|| window.last().map(|m| cursor_encode(&m.id.to_string()))).flatten();

    // A loop, not an iterator chain: each row's per-reader redaction is an async store read.
    let mut items: Vec<serde_json::Value> = Vec::with_capacity(window.len());
    for m in &window {
        let source = mr_endpoint_json(ctx, caller, &m.source).await;
        let target = mr_endpoint_json(ctx, caller, &m.target).await;
        let has_transcript = m.dialogue_transcript.is_some() && can_read_agent(ctx, caller, &m.source.owner, &m.source.agent).await;
        items.push(serde_json::json!({
            "id": m.id,
            "title": m.title,
            "author": m.author,
            "state": m.state,
            "created": m.created,
            "updated": m.updated,
            "source": source,
            "target": target,
            "comments": m.comments.len(),
            "has_transcript": has_transcript,
        }));
    }
    Resp::json(serde_json::json!({
        "agent": meta.name,
        "mrs": items,
        "has_more": has_more,
        "next_cursor": next_cursor,
    }))
}

pub(crate) async fn can_read_agent(ctx: &Ctx, caller: &Caller, seg: &str, name: &str) -> bool {
    let meta = ctx.store.agent_or_unowned(seg, name).await;
    acl::decide(caller, &agent_acl(ctx, &meta).await, Action::Read).allowed()
}

/// Serialize one endpoint **for this reader**, not for the person who opened the MR.
///
/// An MR's source is a different agent with its own ACL, and the opener's permission is not the
/// audience's: alice may open an MR from a private agent into a public one, and from then on everyone
/// who can read the *target* reads the object. Deciding again per reader is what keeps `gate`'s rule —
/// existence is itself a secret — true of the MR views too; checking only the opener leaves the name,
/// aid and ref of a private agent readable by anonymous.
pub(crate) async fn mr_endpoint_json(ctx: &Ctx, caller: &Caller, e: &mr::Endpoint) -> serde_json::Value {
    if !can_read_agent(ctx, caller, &e.owner, &e.agent).await {
        return serde_json::json!({ "aid": null, "owner": null, "agent": null, "full_name": null, "ref": null, "redacted": true });
    }
    serde_json::json!({ "aid": e.aid, "owner": e.owner, "agent": e.agent, "full_name": format!("{}/{}", e.owner, e.agent), "ref": e.git_ref })
}

pub(crate) async fn mr_json(ctx: &Ctx, caller: &Caller, m: &mr::Mr) -> serde_json::Value {
    // The transcript is the dialogue `agit a merge` held *between the two sides*, so it quotes the
    // source by construction — a reader who may not know the source exists may not read it either.
    // Withheld whole rather than filtered: there is no reliable way to strip one agent's voice out of
    // free text, and a partial redaction that looks complete is worse than an honest absence.
    let show_source = can_read_agent(ctx, caller, &m.source.owner, &m.source.agent).await;
    let source = mr_endpoint_json(ctx, caller, &m.source).await;
    let target = mr_endpoint_json(ctx, caller, &m.target).await;
    serde_json::json!({
        "id": m.id,
        "title": m.title,
        "author": m.author,
        "state": m.state,
        "created": m.created,
        "updated": m.updated,
        "source": source,
        "target": target,
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

pub(crate) async fn api_mr_open(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, actor: &str, body: &[u8]) -> Resp {
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
    let Some(source_spec) = str_field(&v, "source") else {
        return Resp::err(400, "want source (the agent the change is coming from, as owner/name)");
    };
    // Identity is (owner, name), so the source is addressed as `owner/name`.
    let Some((source_owner, source_name)) = source_spec.split_once('/') else {
        return Resp::err(400, "source must be owner/name (e.g. daru/frontend)");
    };
    // The source is a real agent on this Hub, and **the caller must be able to read it**: an MR
    // carries the source's identity and ref into an object other people will read, so proposing from
    // an agent you cannot see would leak that it exists.
    let source = match gate(ctx, caller, source_owner, source_name, Action::Read).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if source.scoped() == meta.scoped() {
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

    let open_now = ctx.store.mrs_for(meta.seg(), &meta.name).await.iter().filter(|m| m.is_open()).count();
    if open_now >= mr::OPEN_MAX {
        return Resp::err(429, &format!("this agent already has {} open merge requests", mr::OPEN_MAX));
    }

    // Snapshot both identities now. Names get renamed; the aid is what still says, a year later,
    // which two memories this review was actually between.
    let src_aid = sync_aid(ctx, &source, actor).await.0;
    let tgt_aid = sync_aid(ctx, meta, actor).await.0;
    let (src_seg, tgt_seg) = (source.seg().to_string(), meta.seg().to_string());
    let now = store::now_iso();
    let rec = ctx.store.update_mrs(|mrs| {
        let id = mr::next_id(mrs, &tgt_seg, &meta.name);
        let rec = mr::Mr {
            id,
            source: mr::Endpoint { aid: src_aid.clone(), owner: src_seg.clone(), agent: source.name.clone(), git_ref: source_ref.clone() },
            target: mr::Endpoint { aid: tgt_aid.clone(), owner: tgt_seg.clone(), agent: meta.name.clone(), git_ref: target_ref.clone() },
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
    }).await;
    let Ok(rec) = rec else {
        return Resp::err(500, "failed to write mrs.json");
    };
    audit_append(
        ctx.root(),
        actor,
        audit::MR_OPEN,
        Some(&meta.scoped()),
        &format!("#{} {} ← {}:{}", rec.id, title, source.scoped(), source_ref),
    )
    .await;
    Resp::json_status(201, mr_json(ctx, caller, &rec).await)
}

pub(crate) async fn api_mr_detail(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, id: usize) -> Resp {
    match ctx.store.mrs_for(meta.seg(), &meta.name).await.into_iter().find(|m| m.id == id) {
        Some(m) => Resp::json(mr_json(ctx, caller, &m).await),
        None => Resp::err(404, "not found"),
    }
}

pub(crate) async fn api_mr_comment(ctx: &Ctx, meta: &AgentMeta, id: usize, actor: &str, body: &[u8]) -> Resp {
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
    let (tseg, target) = (meta.seg().to_string(), meta.name.clone());
    let out = ctx.store.update_mrs(|mrs| {
        let Some(m) = mrs.iter_mut().find(|m| m.target.owner == tseg && m.target.agent == target && m.id == id) else {
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
    }).await;
    match out {
        Ok(Ok(c)) => {
            audit_append(ctx.root(), actor, audit::MR_COMMENT, Some(&meta.scoped()), &format!("#{id} comment {}", c.id)).await;
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
pub(crate) async fn api_mr_close(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, id: usize, actor: &str, body: &[u8]) -> Resp {
    let state = match json_body(body).as_ref().and_then(|v| str_field(v, "state")) {
        None => mr::State::Closed,
        Some(s) => match mr::State::parse(&s) {
            Some(x) if !x.is_open() => x,
            // "open" here would be a reopen, which is a different verb on a different route.
            _ => return Resp::err(400, "state must be closed or merged"),
        },
    };
    let (tseg, target) = (meta.seg().to_string(), meta.name.clone());
    let out = ctx.store.update_mrs(|mrs| {
        let Some(m) = mrs.iter_mut().find(|m| m.target.owner == tseg && m.target.agent == target && m.id == id) else {
            return Err(Resp::err(404, "not found"));
        };
        if !m.is_open() {
            return Err(Resp::err(409, &format!("this merge request is already {}", m.state)));
        }
        m.state = state.as_str().to_string();
        m.updated = store::now_iso();
        Ok(m.clone())
    }).await;
    match out {
        Ok(Ok(m)) => {
            let action = if state == mr::State::Merged { audit::MR_MERGED } else { audit::MR_CLOSE };
            audit_append(ctx.root(), actor, action, Some(&meta.scoped()), &format!("#{id} {}", state.as_str())).await;
            Resp::json(mr_json(ctx, caller, &m).await)
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
pub(crate) fn valid_rev(r: &str) -> bool {
    valid_ref_name(r)
}

/// A path inside the store, as it arrives in a URL. Rejects the shapes that make `git show
/// <rev>:<path>` mean something other than "read this file", and the control bytes that would break
/// out of a header value further down.
pub(crate) fn valid_repo_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 512
        && !p.starts_with('-')
        && !p.split('/').any(|c| c.is_empty() || c == "." || c == "..")
        && !p.bytes().any(|b| b < 0x20 || b == 0x7f)
}

/// A git ref name, conservatively. Not `git check-ref-format` — this only has to be a safe, boring
/// label to store and echo back, and refusing an exotic-but-legal ref costs nothing here.
pub(crate) fn valid_ref_name(r: &str) -> bool {
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

pub(crate) async fn api_tokens(ctx: &Ctx, caller: &Caller) -> Resp {
    let Some(user) = caller.user.as_deref() else {
        return Resp::err(401, "login required");
    };
    // You only see your own; the site admin sees them all.
    let items: Vec<serde_json::Value> = ctx
        .store
        .tokens()
        .await
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

pub(crate) async fn api_create_token(ctx: &Ctx, caller: &Caller, body: &[u8]) -> Resp {
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
    // A token binds to the SCOPED id `<owner>/<name>` (names are unique only within an owner).
    let agent = match str_field(&v, "agent") {
        None => None,
        Some(spec) => {
            let Some((a_owner, a_name)) = spec.split_once('/') else {
                return Resp::err(400, "agent must be owner/name (e.g. alice/frontend)");
            };
            // You can only issue tokens for agents you can see.
            let meta = match gate(ctx, caller, a_owner, a_name, Action::Read).await {
                Ok(m) => m,
                Err(r) => return r,
            };
            Some(meta.scoped())
        }
    };
    let ttl_days = match v.get("ttl_days") {
        None | Some(serde_json::Value::Null) => None,
        Some(x) => match x.as_i64() {
            Some(n) if n > 0 && n <= 3650 => Some(n),
            _ => return Resp::err(400, "ttl_days wants an integer in 1..3650"),
        },
    };
    match issue_token(&ctx.store, &name, &user, agent.as_deref(), scope, ttl_days).await {
        Ok(secret) => {
            audit_append(ctx.root(), &user, audit::TOKEN_CREATE, agent.as_deref(), &format!("name={name} scope={}", scope.as_str())).await;
            // The plaintext appears this once — the server keeps only the sha256 digest, which
            // nobody can turn back.
            Resp::json_status(201, serde_json::json!({ "token": secret }))
        }
        Err(e) => Resp::err(500, &e),
    }
}

pub(crate) async fn api_revoke_token(ctx: &Ctx, caller: &Caller, id: &str) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let Some(t) = ctx.store.tokens().await.into_iter().find(|t| t.id == id) else {
        return Resp::err(404, "not found");
    };
    // Your own token, or the site admin.
    if !caller.is_admin && t.owner.as_deref() != Some(user.as_str()) {
        return Resp::err(404, "not found");
    }
    let _ = ctx.store.update_tokens(|toks| toks.retain(|x| x.id != id)).await;
    audit_append(ctx.root(), &user, audit::TOKEN_REVOKE, t.agent.as_deref(), &format!("id={id} name={}", t.name)).await;
    Resp::no_content()
}

// ── audit ──

pub(crate) async fn api_audit(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    let limit: usize = param(req.query(), "limit").and_then(|s| s.parse().ok()).unwrap_or(100).clamp(1, 1000);
    match param(req.query(), "agent") {
        // One agent's audit log: needs Manage on that agent (owner / member admin / site admin). The
        // `?agent=` value is the scoped `owner/name`, and the audit log is keyed on that scoped id.
        Some(spec) => {
            let Some((owner, name)) = spec.split_once('/') else {
                return Resp::err(400, "agent must be owner/name (e.g. alice/frontend)");
            };
            let meta = match gate(ctx, caller, owner, name, Action::Manage).await {
                Ok(x) => x,
                Err(r) => return r,
            };
            Resp::json(serde_json::json!(audit::query(ctx.root(), Some(&meta.scoped()), limit)))
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

/// `PUT /api/agent/<name>/blob` — content-addressed upload. Write-gated, then the server computes and
/// stores the sha256 (the client is never trusted). An optional client-claimed digest (`?sha256=` or
/// the `X-Agit-Blob-Sha256` header) is checked against the computed one and a mismatch is a 409.
/// Content-addressed ⇒ re-uploading identical bytes is idempotent.
///
/// A blob is opaque attacker-authored bytes, so it is deliberately NOT run through the secret scanner:
/// the scanner exists to keep secrets out of the git transcript history (the source of truth); a blob
/// never enters git, is never merged, and is served back only under the same ACL non-disclosure gate,
/// with the same hardened download headers as a raw file. It is inert storage, not reviewable content.
async fn api_blob_put(ctx: &Ctx, req: &Req, caller: &Caller, owner: &str, name: &str, body: &[u8]) -> Resp {
    // Size-first, like api_raw and the git push cap: refuse an oversize upload before doing any work.
    if req.content_length as u64 > BLOB_MAX || body.len() as u64 > BLOB_MAX {
        return Resp::err(413, &format!("blob too large; the ceiling is {BLOB_MAX} bytes"));
    }
    let meta = match gate(ctx, caller, owner, name, Action::Write).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Optional client-claimed digest — query param first, then header.
    let claimed = param(req.query(), "sha256").or_else(|| req.header("x-agit-blob-sha256").map(|s| s.to_string()));

    let digest = match ctx.blobs.put(meta.seg(), &meta.name, body).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("blob put failed for {}: {e}", meta.scoped());
            return Resp::err(500, "failed to store blob");
        }
    };
    if let Some(c) = claimed {
        let c = c.trim().to_ascii_lowercase();
        if !c.is_empty() && c != digest {
            // Reject rather than trust the client: the bytes they sent do not hash to what they said.
            return Resp::json_status(409, serde_json::json!({
                "error": "digest mismatch: the bytes do not hash to the claimed sha256",
                "claimed": c,
                "sha256": digest,
            }));
        }
    }
    let actor = caller.user.clone().unwrap_or_else(|| "anonymous".into());
    audit_append(ctx.root(), &actor, audit::BLOB_PUT, Some(&meta.scoped()), &digest).await;
    Resp::json_status(201, serde_json::json!({ "sha256": digest, "size": body.len() }))
}

/// `GET /api/agent/<name>/blob/<digest>` — content-addressed download. Read-gated, with the SAME
/// existence-non-disclosure as a private agent: the gate runs BEFORE the backend is touched, so
/// "no such blob" and "no access to this agent" are indistinguishable (401/403/404 all from the gate).
pub(crate) async fn api_blob_get(ctx: &Ctx, caller: &Caller, owner: &str, name: &str, digest: &str) -> Resp {
    // A malformed digest could never name a real blob — 404, the same class as gate()'s malformed-name
    // 404, so it leaks nothing. Checked before the gate: it is not agent-specific information.
    if !blob::valid_digest(digest) {
        return Resp::err(404, "not found");
    }
    let meta = match gate(ctx, caller, owner, name, Action::Read).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match ctx.blobs.get(meta.seg(), &meta.name, digest).await {
        Ok(None) => Resp::err(404, "not found"),
        Ok(Some(bytes)) => {
            // Content-addressing invariant, verified at read time: a mismatch means fs corruption or an
            // S3 object swapped underneath — refuse it (500 + audit) rather than serve bytes that do not
            // match their address. Re-hashing up to BLOB_MAX (100 MiB) of SHA-256 per download is real
            // CPU work, so it runs on the blocking pool (matching FsBlobs::put); the bytes are moved in
            // and handed back with the digest, so there is no extra IO and no clone.
            let (bytes, computed) = tokio::task::spawn_blocking(move || {
                let h = blob::sha256_hex(&bytes);
                (bytes, h)
            })
            .await
            .unwrap();
            if computed != digest {
                let actor = caller.user.clone().unwrap_or_else(|| "anonymous".into());
                audit_append(ctx.root(), &actor, audit::BLOB_CORRUPT, Some(&meta.scoped()), digest).await;
                return Resp::err(500, "stored blob failed its integrity check");
            }
            // The identical hardened headers as api_raw: a blob is attacker-authored opaque bytes served
            // from the hub's own cookie origin — exactly the stored-XSS surface these headers exist for.
            Resp::new(200, "application/octet-stream", bytes)
                .with("Content-Disposition", &format!("attachment; filename=\"{digest}\""))
                .with("X-Content-Type-Options", "nosniff")
                .with("Content-Security-Policy", "default-src 'none'; sandbox")
        }
        Err(e) => {
            eprintln!("blob get failed for {}/{digest}: {e}", meta.scoped());
            Resp::err(500, "failed to read blob")
        }
    }
}

pub(crate) fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| kv.strip_prefix(&format!("{key}="))).map(|v| v.to_string())
}

pub(crate) fn json_body(body: &[u8]) -> Option<serde_json::Value> {
    serde_json::from_slice(body).ok()
}

pub(crate) fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}
