//! JSON API handlers + shared helpers (sync bodies returning Resp). Verbatim from the monolith.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::process::Command;

use agit::hub::acl::{self, Action, AgentAcl, Caller, Decision, Deny, Lifecycle, Role, Scope, Visibility};
use agit::hub::net::valid_agent_name;
use agit::hub::store::{AgentMeta, Member};
use agit::hub::{audit, auth, identity, mr, session as websession, store};

use crate::cli::{create_agent, issue_token, list_agents, repo_path};
use crate::gitplumb::*;
use crate::content::{api_compare, api_diff, api_raw, api_session, session_summary};
use crate::http::{Req, Resp};
use crate::limits::Permit;
use crate::router::{audit_deny, gate};
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

pub(crate) fn api(ctx: &Ctx, req: &Req, rest: &str, caller: &Caller, body: &[u8]) -> Resp {
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

pub(crate) fn api_login(ctx: &Ctx, req: &Req, body: &[u8]) -> Resp {
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

pub(crate) fn api_logout(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    if let Some(sid) = req.sid() {
        ctx.sessions.revoke(&sid);
    }
    if let Some(u) = &caller.user {
        audit::append(ctx.root(), u, audit::LOGOUT, None, "");
    }
    Resp::no_content().with("Set-Cookie", &websession::clear_cookie(ctx.cfg.tls))
}

pub(crate) fn api_me(caller: &Caller) -> Resp {
    match &caller.user {
        Some(u) => Resp::json(serde_json::json!({ "username": u, "is_admin": caller.is_admin })),
        None => Resp::err(401, "not logged in"),
    }
}

// ── agents ──

pub(crate) fn api_agents(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
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

pub(crate) fn api_create_agent(ctx: &Ctx, req: &Req, caller: &Caller, body: &[u8]) -> Resp {
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

pub(crate) fn clone_url(ctx: &Ctx, req: &Req, name: &str) -> String {
    format!("{}://{}/{name}.git", if ctx.cfg.tls { "https" } else { "http" }, req.host())
}

pub(crate) fn api_agent(ctx: &Ctx, req: &Req, caller: &Caller, meta: &AgentMeta) -> Resp {
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
pub(crate) fn sync_aid(ctx: &Ctx, meta: &AgentMeta, actor: &str) -> (Option<String>, &'static str, &'static str) {
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
pub(crate) fn api_agent_by_aid(ctx: &Ctx, req: &Req, caller: &Caller, aid: &str) -> Resp {
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

pub(crate) fn api_patch_agent(ctx: &Ctx, caller: &Caller, name: &str, body: &[u8]) -> Resp {
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
            if let Err(resp) = edit_agent(ctx, &meta.name, |m| m.visibility = vis.as_str().to_string()) {
                return resp;
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
        if let Err(resp) = edit_agent(ctx, &meta.name, |m| m.description = d.clone()) {
            return resp;
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
pub(crate) fn name_taken(ctx: &Ctx, name: &str) -> bool {
    ctx.store.agent(name).is_some() || repo_path(ctx.root(), name).exists()
}

/// Mutate the agents.json record for `name` under the lock, mapping a write failure to a 500. The
/// find-by-name / err-to-500 boilerplate that every field-editing handler otherwise repeats.
pub(crate) fn edit_agent(ctx: &Ctx, name: &str, f: impl FnOnce(&mut AgentMeta)) -> Result<(), Resp> {
    ctx.store
        .update_agents(|list| {
            if let Some(m) = list.iter_mut().find(|m| m.name == name) {
                f(m);
            }
        })
        .map_err(|_| Resp::err(500, "failed to write agents.json"))
}

/// Move an agent between lifecycle states. The state itself is enforced in `acl::decide` — this only
/// writes it down.
pub(crate) fn set_lifecycle(ctx: &Ctx, caller: &Caller, name: &str, to: Lifecycle, from: &[Lifecycle], action: &'static str) -> Resp {
    let meta = match gate(ctx, caller, name, Action::Manage) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Refusing the no-op transition is what makes each of these verbs mean something: "restore" on a
    // live agent is a caller who thinks it was deleted, and answering 204 would agree with them.
    if !from.contains(&meta.lifecycle()) {
        return Resp::err(409, &format!("this agent is {}", meta.lifecycle().as_str()));
    }
    if let Err(resp) = edit_agent(ctx, &meta.name, |m| m.lifecycle = to.as_str().to_string()) {
        return resp;
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
pub(crate) fn api_delete_agent(ctx: &Ctx, caller: &Caller, name: &str, query: &str) -> Resp {
    if param(query, "purge").as_deref() == Some("true") {
        return api_purge_agent(ctx, caller, name);
    }
    set_lifecycle(ctx, caller, name, Lifecycle::Deleted, &[Lifecycle::Active, Lifecycle::Archived], audit::AGENT_DELETE)
}

/// The irreversible one: the bytes go. Only reachable for an already soft-deleted agent, so nothing
/// live can be destroyed by a single mistyped verb.
pub(crate) fn api_purge_agent(ctx: &Ctx, caller: &Caller, name: &str) -> Resp {
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
pub(crate) fn api_fork_agent(ctx: &Ctx, req: &Req, caller: &Caller, name: &str, body: &[u8]) -> Resp {
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
pub(crate) fn api_star_agent(ctx: &Ctx, caller: &Caller, name: &str, body: &[u8]) -> Resp {
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
    if let Err(resp) = edit_agent(ctx, &meta.name, |m| {
        m.stars.retain(|u| u != &who);
        if on {
            m.stars.push(who.clone());
        }
    }) {
        return resp;
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
pub(crate) fn api_transfer_agent(ctx: &Ctx, caller: &Caller, name: &str, body: &[u8]) -> Resp {
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
    if let Err(resp) = edit_agent(ctx, &meta.name, |m| {
        m.owner = Some(target.clone());
        // The new owner's membership row, if any, is now noise at best and a demotion at worst (owner
        // outranks every role) — drop it rather than leave two answers to "what may they do".
        m.members.retain(|x| x.username != target);
    }) {
        return resp;
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

pub(crate) fn api_members(ctx: &Ctx, caller: &Caller, name: &str, tail: &str, method: &str, body: &[u8]) -> Resp {
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
            if let Err(resp) = edit_agent(ctx, &meta.name, |m| {
                match m.members.iter_mut().find(|x| x.username == username) {
                    Some(x) => x.role = role.as_str().to_string(),
                    None => m.members.push(Member { username: username.clone(), role: role.as_str().to_string() }),
                }
            }) {
                return resp;
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
pub(crate) const XSEARCH_SCAN_CAP: usize = 400;
/// Hits returned. Past this the scan stops early: nobody reads hit 200, and the work is real.
pub(crate) const XSEARCH_MAX_HITS: usize = 50;

/// `GET /api/search?q=` — one query across **every agent the caller may read**, over the fields
/// people actually remember: what they asked, what came back, which files were touched.
///
/// The permission is per agent and decided by `acl::decide`, exactly like everywhere else: an agent
/// you cannot read contributes nothing, and cannot even be inferred from a hit count.
pub(crate) fn api_search(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
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
pub(crate) fn api_mrs(ctx: &Ctx, caller: &Caller, name: &str, tail: &str, method: &str, query: &str, body: &[u8]) -> Resp {
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
pub(crate) fn mutation_actor(ctx: &Ctx, caller: &Caller, name: &str) -> Result<String, Resp> {
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
pub(crate) fn api_mr_list(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, query: &str) -> Resp {
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

pub(crate) fn can_read_agent(ctx: &Ctx, caller: &Caller, agent: &str) -> bool {
    acl::decide(caller, &ctx.store.agent_or_unowned(agent).to_acl(), Action::Read).allowed()
}

/// Serialize one endpoint **for this reader**, not for the person who opened the MR.
///
/// An MR's source is a different agent with its own ACL, and the opener's permission is not the
/// audience's: alice may open an MR from a private agent into a public one, and from then on everyone
/// who can read the *target* reads the object. Deciding again per reader is what keeps `gate`'s rule —
/// existence is itself a secret — true of the MR views too; checking only the opener leaves the name,
/// aid and ref of a private agent readable by anonymous.
pub(crate) fn mr_endpoint_json(ctx: &Ctx, caller: &Caller, e: &mr::Endpoint) -> serde_json::Value {
    if !can_read_agent(ctx, caller, &e.agent) {
        return serde_json::json!({ "aid": null, "agent": null, "ref": null, "redacted": true });
    }
    serde_json::json!({ "aid": e.aid, "agent": e.agent, "ref": e.git_ref })
}

pub(crate) fn mr_json(ctx: &Ctx, caller: &Caller, m: &mr::Mr) -> serde_json::Value {
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

pub(crate) fn api_mr_open(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, actor: &str, body: &[u8]) -> Resp {
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

pub(crate) fn api_mr_detail(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, id: usize) -> Resp {
    match ctx.store.mrs_for(&meta.name).into_iter().find(|m| m.id == id) {
        Some(m) => Resp::json(mr_json(ctx, caller, &m)),
        None => Resp::err(404, "not found"),
    }
}

pub(crate) fn api_mr_comment(ctx: &Ctx, meta: &AgentMeta, id: usize, actor: &str, body: &[u8]) -> Resp {
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
pub(crate) fn api_mr_close(ctx: &Ctx, caller: &Caller, meta: &AgentMeta, id: usize, actor: &str, body: &[u8]) -> Resp {
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

pub(crate) fn api_tokens(ctx: &Ctx, caller: &Caller) -> Resp {
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

pub(crate) fn api_create_token(ctx: &Ctx, caller: &Caller, body: &[u8]) -> Resp {
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

pub(crate) fn api_revoke_token(ctx: &Ctx, caller: &Caller, id: &str) -> Resp {
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

pub(crate) fn api_audit(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
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

pub(crate) fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| kv.strip_prefix(&format!("{key}="))).map(|v| v.to_string())
}

pub(crate) fn json_body(body: &[u8]) -> Option<serde_json::Value> {
    serde_json::from_slice(body).ok()
}

pub(crate) fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}
