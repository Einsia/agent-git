//! JSON API handlers + shared helpers (sync bodies returning Resp). Verbatim from the monolith.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::io;
use std::net::IpAddr;
use std::process::Command;

use agit::hub::acl::{self, Action, AgentAcl, Caller, Decision, Deny, Lifecycle, Role, Scope, Visibility};
use agit::hub::blob::{self, BLOB_MAX};
use agit::hub::metrics::AuthResult;
use agit::hub::net::valid_agent_name;
use agit::hub::store::{AgentMeta, Invitation, Member, Org, OrgMember, User};
use agit::hub::{audit, auth, identity, kdf, mr, session as websession, store, totp};

use crate::cli::{create_agent, issue_token, list_agents, repo_path};
use crate::gitplumb::*;
use crate::content::{api_compare, api_diff, api_raw, api_session, provenance_verdict_json, session_self_provenance, session_summary};
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

pub(crate) async fn api(ctx: &Ctx, req: &Req, rest: &str, caller: &Caller, client_ip: Option<IpAddr>, body: &[u8]) -> Resp {
    let m = req.method.as_str();
    match (m, rest) {
        // Public, no auth: the version/schema the web report + client record to pin the hub version.
        // Registered at the anonymous tier (no `caller` gate) exactly like the open `repos` index.
        ("GET", "version") => return api_version(),
        ("POST", "login") => return api_login(ctx, req, body).await,
        ("POST", "register") => return api_register(ctx, client_ip, body).await,
        ("POST", "logout") => return api_logout(ctx, req, caller).await,
        ("GET", "me") => return api_me(ctx, caller).await,
        ("GET", "me/invitations") => return api_me_invitations(ctx, caller).await,
        ("POST", "me/password") => return api_me_password(ctx, req, caller, body).await,
        // Self-service password reset for a LOCKED-OUT user (no session, no old password). The unguessable
        // token forwarded by the operator is the only authority. `request` mints+delivers a reset link and
        // is anti-enumeration (always a generic 200); `consume` spends the token to set a new password.
        ("POST", "password-reset/request") => return api_password_reset_request(ctx, client_ip, body).await,
        ("POST", "password-reset/consume") => return api_password_reset_consume(ctx, body).await,
        // Consume a verification token (the capability is the auth — no session needed) and mint a fresh
        // one for the logged-in caller. See the email-squatting defense in store::get_identity_keys_by_email.
        ("POST", "verify-email") => return api_verify_email(ctx, body).await,
        ("POST", "me/verify/resend") => return api_me_verify_resend(ctx, caller).await,
        ("POST", "me/2fa/enroll") => return api_2fa_enroll(ctx, caller).await,
        ("POST", "me/2fa/confirm") => return api_2fa_confirm(ctx, caller, body).await,
        ("POST", "me/2fa/disable") => return api_2fa_disable(ctx, caller, body).await,
        ("GET", "agents") => return api_agents(ctx, req, caller).await,
        ("POST", "agents") => return api_create_agent(ctx, req, caller, body).await,
        ("GET", "tokens") => return api_tokens(ctx, caller).await,
        ("POST", "tokens") => return api_create_token(ctx, caller, body).await,
        ("GET", "audit") => return api_audit(ctx, req, caller).await,
        ("GET", "search") => return api_search(ctx, req, caller).await,
        // The cross-agent code-repo index. Open to any caller (anonymous included); only agents the
        // caller may Read contribute, so the ACL filter - not the route - is the gate.
        ("GET", "repos") => return api_repos(ctx, caller).await,
        ("GET", "orgs") => return api_orgs_list(ctx, caller).await,
        ("POST", "orgs") => return api_orgs_create(ctx, caller, body).await,
        // The admin USER ROSTER: list every account, or create one. Both are admin-only AND
        // login-session only (a token — even an admin's — is refused), matching the site-wide audit log
        // and the per-user recovery doors below. The /disable + /enable toggles are the `users/` prefix
        // routes further down.
        ("GET", "users") => return api_users_list(ctx, caller).await,
        ("POST", "users") => return api_users_create(ctx, caller, body).await,
        // Shared identity registry (encryption-recipients Wave 1). Enroll upserts the CALLER's own row;
        // the list form reads a batch (`?users=a,b,c`). The single-user GET is the `identity/<user>`
        // prefix route below. All require an authenticated caller.
        ("POST", "identity/enroll") => return api_identity_enroll(ctx, caller, body).await,
        // by-email resolves a committer email to the registered account owning it — the lookup that turns
        // provenance's "signed by this key" into "verified as this person". Exact route, matched BEFORE the
        // `identity/<user>` prefix so `by-email` is never read as a username.
        ("GET", "identity/by-email") => return api_identity_by_email(ctx, req, caller).await,
        ("GET", "identity") => return api_identity_list(ctx, req, caller).await,
        // The hub's escrow PUBLIC key (encryption-recipients Wave 5, hub-assist escrow). Any authenticated
        // caller may read it — it is a public key a hub-assist client seals its content key TO.
        ("GET", "escrow/pubkey") => return api_escrow_pubkey(ctx, caller).await,
        _ => {}
    }
    // identity/keys/<key_fpr> — revoke ONE of the caller's device keys. Matched BEFORE the identity/<user>
    // prefix so a `<user>` is never read as "keys".
    if let Some(fpr) = rest.strip_prefix("identity/keys/") {
        return match m {
            "DELETE" => api_identity_revoke_key(ctx, caller, fpr).await,
            _ => Resp::text(405, "method not allowed"),
        };
    }
    // identity/<user> — a single registry lookup. GET only; POST enroll is the exact route above and
    // never reaches here, so a `<user>` is always a real username (which has no '/').
    if let Some(user) = rest.strip_prefix("identity/") {
        return match m {
            "GET" => api_identity_get(ctx, caller, user).await,
            _ => Resp::text(405, "method not allowed"),
        };
    }
    // orgs/<name> and orgs/<name>/members[/<username>] — before the agent/ block, since an org name
    // is not an agent name.
    if let Some(after) = rest.strip_prefix("orgs/") {
        if let Some((name, tail)) = after.split_once("/members") {
            if tail.is_empty() || tail.starts_with('/') {
                return api_org_members(ctx, caller, name, tail, m, body).await;
            }
        }
        // orgs/<org>/invitations[/<id>[/accept|/decline]] — tail is "" or "/<id>..." only, so a
        // stray /invitationsXYZ does not slip through (same guard as /members).
        if let Some((name, tail)) = after.split_once("/invitations") {
            if tail.is_empty() || tail.starts_with('/') {
                return api_org_invitations(ctx, caller, name, tail, m, body).await;
            }
        }
        // orgs/<org>/transfer — hand ownership to an existing member.
        if let Some(name) = after.strip_suffix("/transfer") {
            return match m {
                "POST" => api_org_transfer(ctx, caller, name, body).await,
                _ => Resp::text(405, "method not allowed"),
            };
        }
        // orgs/<org>/kek/<envelopes|envelope|gens> — the Team-KEK endpoints (Wave 3). Same tail guard
        // as /members: "/kek..." only, so a stray /kekXYZ does not slip through.
        if let Some((name, tail)) = after.split_once("/kek") {
            if tail.is_empty() || tail.starts_with('/') {
                return api_org_kek(ctx, caller, name, tail, m, req.query(), body).await;
            }
        }
        // orgs/<org>/recovery — the OPT-IN offline recovery recipient (Wave 5). Owner-only to set/clear;
        // any member may show. Exact suffix, so a stray /recoveryXYZ does not match.
        if let Some(name) = after.strip_suffix("/recovery") {
            return api_org_recovery(ctx, caller, name, m, body).await;
        }
        // orgs/<org>/escrow — the OPT-IN hub-assist escrow mode (Wave 5). Owner-only to set.
        if let Some(name) = after.strip_suffix("/escrow") {
            return api_org_escrow(ctx, caller, name, m, body).await;
        }
        // orgs/<org>/settings — org policy knobs (today: members_can_create). Org-admin only to change.
        if let Some(name) = after.strip_suffix("/settings") {
            return api_org_settings(ctx, caller, name, m, body).await;
        }
        // orgs/<org>/overview - the org view (members + every readable agent owned by the org or its
        // members). Same membership-or-admin 404 gate as api_org_get. Exact suffix, so a stray
        // /overviewXYZ does not match.
        if let Some(name) = after.strip_suffix("/overview") {
            return match m {
                "GET" => api_org_overview(ctx, caller, name).await,
                _ => Resp::text(405, "method not allowed"),
            };
        }
        return match m {
            "GET" => api_org_get(ctx, caller, after).await,
            "DELETE" => api_org_delete(ctx, caller, after).await,
            _ => Resp::text(405, "method not allowed"),
        };
    }
    if let Some(id) = rest.strip_prefix("tokens/") {
        return match m {
            "DELETE" => api_revoke_token(ctx, caller, id).await,
            _ => Resp::text(405, "method not allowed"),
        };
    }
    // users/<username>/password — admin-only credential reset (the account-recovery door). The only
    // /api/users/... route; a username has no '/', so `<username>/password` is the only shape.
    if let Some(after) = rest.strip_prefix("users/") {
        if let Some(username) = after.strip_suffix("/password") {
            return match m {
                "POST" => api_admin_set_password(ctx, caller, username, body).await,
                _ => Resp::text(405, "method not allowed"),
            };
        }
        // Admin recovery for a user locked out of their authenticator: clear their 2FA.
        if let Some(username) = after.strip_suffix("/2fa-disable") {
            return match m {
                "POST" => api_admin_2fa_disable(ctx, caller, username).await,
                _ => Resp::text(405, "method not allowed"),
            };
        }
        // Admin force-mark a user's email verified (the out-of-band operator vouch).
        if let Some(username) = after.strip_suffix("/verify-email") {
            return match m {
                "POST" => api_admin_verify_email(ctx, caller, username).await,
                _ => Resp::text(405, "method not allowed"),
            };
        }
        // Admin soft-suspend a user (revoking their live sessions) / lift the suspension. Same admin +
        // login-session gate as the roster list/create above.
        if let Some(username) = after.strip_suffix("/disable") {
            return match m {
                "POST" => api_admin_set_disabled(ctx, caller, username, true).await,
                _ => Resp::text(405, "method not allowed"),
            };
        }
        if let Some(username) = after.strip_suffix("/enable") {
            return match m {
                "POST" => api_admin_set_disabled(ctx, caller, username, false).await,
                _ => Resp::text(405, "method not allowed"),
            };
        }
        return Resp::err(404, "not found");
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

        // agent/<owner>/<name>/session/<id>/provenance — the REGISTRY-CLASSIFIED verdict ("verified as
        // <person>" / "key mismatch"), which needs the store and so can't run inside the sync content
        // helper. The git self-verify runs on the blocking pool; the registry lookup + attribution runs
        // here, async, against `ctx.store`. See `classify_read_status`.
        if let Some(id) = tail.strip_suffix("/provenance") {
            let id = id.to_string();
            let self_status = tokio::task::spawn_blocking(move || {
                let repo = repo_path(&root, &seg, &name);
                has_head(&repo).then(|| session_self_provenance(&repo, &id, &query)).flatten()
            })
            .await
            .unwrap();
            let Some(self_status) = self_status else {
                return Resp::err(404, "not found");
            };
            let classified = classify_read_status(&ctx.store, self_status).await;
            return Resp::json(provenance_verdict_json(&classified));
        }

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

    // agent/<owner>/<name>/keys/<escrow|release> — the OPT-IN hub-assist escrow endpoints (Wave 5).
    // `keys/escrow` (POST, Write-gated) stores a content key sealed to the hub escrow key; `keys/release`
    // (POST, Read-gated — the SAME acl::decide gate as git fetch) releases escrowed CKs to a reader,
    // fail-closed. Both require the owning org to be in `escrow_mode = 'hub-assist'`.
    if let Some((name, tail)) = after.split_once("/keys/") {
        return match (m, tail) {
            ("POST", "escrow") => api_keys_escrow(ctx, caller, owner, name, body).await,
            ("POST", "release") => api_keys_release(ctx, caller, owner, name).await,
            _ => Resp::text(405, "method not allowed"),
        };
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

/// Attribute a self-verified session against the identity registry — the server-side "verified as
/// <person>" step, run live on the SESSION read path (the push-time equivalent lives in `prov.rs`).
///
/// A `Verified { email, .. }` self-status is upgraded by resolving the committer email to a VERIFIED
/// account (`store.get_identity_keys_by_email` is verified-only after the email-verify wave, so an
/// unverified or squatted email resolves to nothing) and comparing keys:
///   - registered key EQUALS the provenance pubkey  → `VerifiedAs` (a real, verified account),
///   - registered but the key DIFFERS               → `KeyMismatch` (a forgery — never rendered green),
///   - no verified account for that email           → `SignedUnregistered`.
/// The hub IS the registry, so it compares against its own stored key (`trusted_pubkey = None`; TOFU is
/// the client's job). Any non-`Verified` self-status (Unsigned / ContentTampered / BadSignature) passes
/// through unchanged — there is nothing to attribute.
async fn classify_read_status(
    store: &store::Store,
    self_status: agit::commands::ProvenanceStatus,
) -> agit::commands::ProvenanceStatus {
    let agit::commands::ProvenanceStatus::Verified { email, .. } = &self_status else {
        return self_status;
    };
    // Resolve the committer email to the VERIFIED account's whole device-key SET (empty when unknown /
    // unverified / ambiguous), then attribute match-ANY. The hub IS the registry, so it compares against
    // its own stored keys directly (`trusted_keys = None`; TOFU is the client's job).
    let keys = store.get_identity_keys_by_email(email).await;
    let registered = keys.first().map(|k| agit::commands::RegisteredIdentity {
        username: k.username.clone(),
        ed25519_keys: keys.iter().map(|k| k.ed25519_pub.clone()).collect(),
    });
    agit::commands::attribute_with_registry(self_status, registered, None)
}

/// `GET /api/version` — public (no auth). The version + schema the web report and the client read to
/// pin the hub version they talked to. `build_sha` is the git sha compiled in via `AGIT_BUILD_SHA` (a
/// release build sets it); when it was not set at compile time it is `null`, never a fabricated value.
pub(crate) fn api_version() -> Resp {
    Resp::json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "build_sha": option_env!("AGIT_BUILD_SHA"),
        "schema_version": store::schema_version(),
    }))
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
        ctx.metrics.record_auth(AuthResult::LoginFail);
        tracing::warn!(user = %store::normalize_username(&username), "login failed");
        audit_append(ctx.root(), &store::normalize_username(&username), audit::LOGIN_FAILED, None, &req.host()).await;
        // Don't say whether the user doesn't exist or the password is wrong — that hands the
        // brute-forcer a username dictionary.
        return Resp::err(401, "wrong username or password");
    };
    // Disabled (admin soft-suspend): the password was CORRECT (we are past the generic 401), so a clear
    // 403 here is not a password oracle — only someone who already knows the password learns the account
    // is suspended, which is inherent and acceptable (same shape as the 2FA-required disclosure below). A
    // WRONG password never reaches this line; it exits at the generic 401 above. Checked BEFORE the 2FA
    // gate and BEFORE any session is minted, so a disabled account can never obtain a cookie.
    if user.disabled {
        ctx.metrics.record_auth(AuthResult::LoginFail);
        tracing::warn!(user = %user.username, "login refused: account disabled");
        audit_append(ctx.root(), &user.username, audit::LOGIN_FAILED, None, "account disabled").await;
        return Resp::err(403, "this account is disabled; contact an admin");
    }
    // Second factor: once 2FA is active, a correct password alone is NOT sufficient. Require a `code`
    // (a current TOTP or an unused backup code). A missing/wrong code is a flat 401
    // {"error":"2fa_required"} and NO session — so an attacker holding only the password still cannot
    // get in. (It does reveal to someone who already has the correct password that 2FA is on; that is
    // inherent and acceptable — what must never leak is a session.)
    if user.totp_enabled {
        let Some(code) = str_field(&v, "code") else {
            return two_factor_required(ctx, &user.username).await;
        };
        let secret = user.totp_secret.clone().unwrap_or_default();
        let ok = if totp::verify(&secret, &user.username, &code) {
            true
        } else {
            // Backup code: verify AND consume it atomically inside the users write lock, so two
            // concurrent logins can never both spend the same one-time code (the stale read above is
            // not trusted for the mutation).
            let uname = user.username.clone();
            ctx.store
                .update_users(move |users| match users.iter_mut().find(|u| u.username == uname) {
                    Some(u) => match totp::consume_backup_code(&code, &u.totp_backup_codes) {
                        Some(remaining) => {
                            u.totp_backup_codes = remaining;
                            true
                        }
                        None => false,
                    },
                    None => false,
                })
                .await
                .unwrap_or(false)
        };
        if !ok {
            return two_factor_required(ctx, &user.username).await;
        }
    }
    let Ok(sid) = ctx.sessions.create(&user.username) else {
        return Resp::err(503, "couldn't create a session, try again shortly");
    };
    ctx.metrics.record_auth(AuthResult::LoginOk);
    tracing::info!(user = %user.username, admin = user.is_admin, "login success");
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

/// `GET /api/me` — the logged-in caller's own account facts. Includes `email` (the account's registered
/// committer email, from the identity registry, or `""` if none is enrolled) and `email_verified` (the
/// anti-squatting gate) so the Account page can render the Verified / Unverified badge and its resend
/// affordance without a second round-trip.
pub(crate) async fn api_me(ctx: &Ctx, caller: &Caller) -> Resp {
    let Some(u) = &caller.user else {
        return Resp::err(401, "not logged in");
    };
    // The email of record is the PRIMARY device key's committer email (self-asserted at enroll);
    // email_verified lives on the users row. Both reads tolerate a missing row (fresh account not yet
    // enrolled → "" / false). `key_count` is how many device keys the account has registered.
    let keys = ctx.store.list_identity_keys(u).await;
    let email = ctx.store.get_primary_identity_key(u).await.map(|k| k.email).unwrap_or_default();
    let email_verified = ctx.store.user(u).await.map(|user| user.email_verified).unwrap_or(false);
    Resp::json(serde_json::json!({
        "username": u,
        "is_admin": caller.is_admin,
        "email": email,
        "email_verified": email_verified,
        "key_count": keys.len(),
    }))
}

/// `POST /api/register` — self-service signup. Creates a **normal, non-admin** user and logs them in
/// with a session cookie, mirroring the CLI's `user add` crypto (cli.rs) + `api_login`'s session
/// issuance. Off unless the operator enabled it (`ctx.cfg.registration`).
///
/// **Security invariant**: `is_admin` is hardcoded `false`, and no admin field is ever read from the
/// body — registration can never grant admin. Admin stays CLI-only (`agit-hub user add --admin`).
pub(crate) async fn api_register(ctx: &Ctx, client_ip: Option<IpAddr>, body: &[u8]) -> Resp {
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
    // Per-IP registration rate limit (see `CtxInner::register_rl`). Charged once the request is a
    // well-formed signup — after the cheap format checks, before the account store is touched or the
    // argon2 hash runs — so a sweep is throttled at both the "is this name taken?" enumeration oracle
    // and the argon2 amplifier, while a malformed request (already a cheap 400) is not penalized. A
    // missing client IP (no ConnectInfo) fails open, exactly as the connection limiter does.
    if let Some(ip) = client_ip {
        if !ctx.register_rl.allow(&ip.to_string()) {
            return Resp::err(429, "too many registration attempts from your address; slow down").with("Retry-After", "60");
        }
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
    let user = User { username: username.clone(), pw_hash, salt, kdf: kdf_id, is_admin: false, created: store::now_iso(), ..Default::default() };
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
    tracing::info!(user = %username, "registration");
    audit_append(ctx.root(), &username, audit::USER_REGISTER, None, "").await;
    // Mint a verification token at account creation. A fresh registrant has no email on file yet (the
    // registry email is self-asserted later, at `identity enroll`), so this is a no-op here and the token
    // is minted lazily once an email is enrolled or an explicit resend is requested. The token is NEVER
    // returned in this response body — that would defeat verification.
    let _ = crate::emailverify::mint_and_deliver(&ctx.store, &username).await;
    Resp::json(serde_json::json!({ "username": username, "is_admin": false }))
        .with("Set-Cookie", &websession::set_cookie(&sid, ctx.cfg.tls))
}

/// Derive a fresh salt + argon2 hash for `password`, run under the login gate + blocking pool (the
/// same CPU/memory-amplifier defense `api_login`/`api_register` use — argon2 is deliberately slow, so
/// uncapped concurrency is a DoS lever). Returns `(pw_hash, salt, kdf_id)` or an error `Resp`. The
/// caller has already length-checked, so this is purely the crypto step both password-write paths
/// share.
async fn hash_new_password(ctx: &Ctx, password: &str) -> Result<(String, String, String), Resp> {
    let Ok(salt) = kdf::gen_salt() else {
        return Err(Resp::err(500, "no system entropy available, try again shortly"));
    };
    let kdf_id = kdf::current_kdf_id();
    let (pw, salt2, kdf2) = (password.to_string(), salt.clone(), kdf_id.clone());
    let hashed = {
        let _slot = ctx.login_gate.acquire().await.expect("login gate semaphore is never closed");
        tokio::task::spawn_blocking(move || kdf::hash_password(&pw, &salt2, &kdf2)).await.unwrap()
    };
    match hashed {
        Some(pw_hash) => Ok((pw_hash, salt, kdf_id)),
        None => Err(Resp::err(500, "password derivation failed")),
    }
}

/// `POST /api/me/password` — self-service password change for the logged-in user.
///
/// Body: `{ old_password, new_password }`. The old password is verified through the SAME argon2 path
/// as login (`auth::verify_login`); a wrong one is a 401, a too-short new one a 400 (the shared
/// `store::MIN_PASSWORD_LEN`). On success the new password is re-hashed with a fresh salt and the
/// current kdf, and every OTHER browser session for the account is revoked — the session that made
/// the change stays signed in, so rotating your password kicks a stolen cookie without logging you
/// out of the tab you did it from.
pub(crate) async fn api_me_password(ctx: &Ctx, req: &Req, caller: &Caller, body: &[u8]) -> Resp {
    let Some(username) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let (Some(old_password), Some(new_password)) = (str_field(&v, "old_password"), str_field(&v, "new_password")) else {
        return Resp::err(400, "want old_password and new_password");
    };
    // Verify the CURRENT password first — under the login gate, exactly like `api_login`, so this
    // endpoint is not an uncapped argon2 amplifier either. A wrong old password is a flat 401; we do
    // not say whether it was the password (there is nothing to enumerate — the user is already known).
    let verified = {
        let _slot = ctx.login_gate.acquire().await.expect("login gate semaphore is never closed");
        auth::verify_login(&ctx.store, &username, &old_password).await
    };
    if verified.is_none() {
        audit_append(ctx.root(), &username, audit::LOGIN_FAILED, None, "password change: wrong old password").await;
        return Resp::err(401, "old password is wrong");
    }
    // Enforce the same minimum the CLI and registration use, via the one shared constant.
    if new_password.chars().count() < store::MIN_PASSWORD_LEN {
        return Resp::err(400, "password too short (at least 8 characters)");
    }
    let (pw_hash, salt, kdf_id) = match hash_new_password(ctx, &new_password).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match ctx.store.set_password(&username, &pw_hash, &salt, &kdf_id).await {
        // verify_login just succeeded, so the row exists; a false here would be a concurrent delete.
        Ok(true) => {}
        Ok(false) => return Resp::err(404, "no such user"),
        Err(_) => return Resp::err(500, "couldn't update the password"),
    }
    // Kick every OTHER session for this account (a rotated password should invalidate a leaked
    // cookie), keeping the caller's own session alive so they are not logged out mid-change.
    let revoked = ctx.sessions.revoke_user(&username, req.sid().as_deref());
    tracing::info!(user = %username, revoked_sessions = revoked, "password changed");
    audit_append(ctx.root(), &username, audit::USER_PASSWORD, None, &format!("revoked {revoked} other session(s)")).await;
    Resp::json(serde_json::json!({ "ok": true, "revoked_sessions": revoked }))
}

/// `POST /api/users/<username>/password` — admin-only credential reset (the recovery path for a
/// locked-out user). Body: `{ new_password }`. Gated on `caller.is_admin`; no old password is asked
/// for (the whole point is that the user cannot supply it). Re-hashes with argon2 and revokes ALL of
/// the target's sessions — a reset is a lockout/recovery action, so any existing sign-in should die
/// with the old password.
pub(crate) async fn api_admin_set_password(ctx: &Ctx, caller: &Caller, username: &str, body: &[u8]) -> Resp {
    let Some(actor) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    if !caller.is_admin {
        return Resp::err(403, "admin only");
    }
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(new_password) = str_field(&v, "new_password") else {
        return Resp::err(400, "want new_password");
    };
    if new_password.chars().count() < store::MIN_PASSWORD_LEN {
        return Resp::err(400, "password too short (at least 8 characters)");
    }
    let target = store::normalize_username(username);
    // An admin already sees every account (they can `user list`), so telling them a name is unknown
    // leaks nothing — a plain 404 is the honest answer.
    if ctx.store.user(&target).await.is_none() {
        return Resp::err(404, "no such user");
    }
    let (pw_hash, salt, kdf_id) = match hash_new_password(ctx, &new_password).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match ctx.store.set_password(&target, &pw_hash, &salt, &kdf_id).await {
        Ok(true) => {}
        Ok(false) => return Resp::err(404, "no such user"),
        Err(_) => return Resp::err(500, "couldn't update the password"),
    }
    // A reset locks the old credential out everywhere: revoke every one of the target's sessions.
    let revoked = ctx.sessions.revoke_user(&target, None);
    tracing::info!(actor = %actor, user = %target, revoked_sessions = revoked, "admin password reset");
    audit_append(ctx.root(), &actor, audit::USER_PASSWORD_RESET, None, &format!("reset {target}; revoked {revoked} session(s)")).await;
    Resp::json(serde_json::json!({ "ok": true, "user": target, "revoked_sessions": revoked }))
}

/// `POST /api/password-reset/request` `{ username }` — self-service password-reset request for a
/// LOCKED-OUT user. UNAUTHENTICATED (the whole point is you cannot log in) and **anti-enumeration**: it
/// ALWAYS answers a generic 200 with a byte-identical body whether or not the account exists, so it can
/// never be used as a username oracle. When the account DOES exist, a single-use reset token is minted
/// and delivered operator-forwarded (the `<base>/reset-password?token=` link is logged/printed, same
/// hermetic stub as email verification); when it does not, nothing is minted and nothing is disclosed.
///
/// Rate-limited per-IP on the SAME budget as registration (`ctx.register_rl`), charged BEFORE the store
/// lookup — both are unauthenticated, cheap-to-retry abuse surfaces, and charging first keeps the
/// throttle independent of whether the account exists (so the 429 leaks nothing either). A missing client
/// IP fails open, exactly like registration and the connection limiter.
pub(crate) async fn api_password_reset_request(ctx: &Ctx, client_ip: Option<IpAddr>, body: &[u8]) -> Resp {
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(raw) = str_field(&v, "username") else {
        return Resp::err(400, "want a username");
    };
    // Charge the per-IP budget BEFORE touching the store, so a sweep is throttled and the rate check is
    // identical for existing and non-existent accounts (no enumeration via timing or 429 vs 200 keyed on
    // the username — the bucket is keyed on the IP only).
    if let Some(ip) = client_ip {
        if !ctx.register_rl.allow(&ip.to_string()) {
            return Resp::err(429, "too many password-reset requests from your address; slow down").with("Retry-After", "60");
        }
    }
    let username = store::normalize_username(&raw);
    // Do COMPARABLE work whether or not the account exists, so response time cannot enumerate accounts.
    // The hit branch mints a token (a DB INSERT) + appends an audit line; a bare read-only miss branch
    // used to be ~1000x faster. The miss branch below performs an EQUIVALENT DB write (a throwaway mint,
    // discarded, never delivered) + an audit append. Only mint+deliver a REAL link for a real account;
    // stay SILENT either way. The generic response is the SAME bytes on both branches, so the caller
    // cannot tell existence from the answer.
    if ctx.store.user(&username).await.is_some() {
        if crate::resetpw::mint_and_deliver(&ctx.store, &username).await.is_some() {
            audit_append(ctx.root(), &username, audit::USER_PASSWORD_RESET, None, "reset link requested (operator-forwarded)").await;
        }
    } else if crate::resetpw::equalize_nonexistent(&ctx.store).await.is_some() {
        // Equivalent DB write + audit for a nonexistent account. The actor is a fixed sentinel (never the
        // attacker-supplied string), and NO link is generated or delivered.
        audit_append(ctx.root(), "\u{0}nonexistent", audit::USER_PASSWORD_RESET, None, "reset requested for a nonexistent account (no link generated)").await;
    }
    // The ONE generic answer. Identical on every path so existence never leaks. Do NOT branch this.
    Resp::json(serde_json::json!({ "ok": true, "message": "if that account exists, a password reset link was generated" }))
}

/// `POST /api/password-reset/consume` `{ token, new_password }` — spend a reset token to set a new
/// password for a locked-out user. UNAUTHENTICATED: the unguessable single-use token IS the authority,
/// so — unlike `api_me_password` — **no old password is required**; a valid token IS required. The new
/// password is re-hashed with a fresh salt and the current kdf (the SAME path as the admin reset), and
/// EVERY session for the account is revoked (a reset is a recovery action — any live sign-in dies with
/// the old credential). Enforces the shared minimum password length; a weak/empty one is a 400 and the
/// token is left UNSPENT so the user can retry with a stronger password.
pub(crate) async fn api_password_reset_consume(ctx: &Ctx, body: &[u8]) -> Resp {
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let (Some(token), Some(new_password)) = (str_field(&v, "token"), str_field(&v, "new_password")) else {
        return Resp::err(400, "want token and new_password");
    };
    // Enforce the same minimum the CLI, registration and every other password path use, via the one
    // shared constant — checked BEFORE consuming the token so a rejected weak password does not burn the
    // single-use capability.
    if new_password.chars().count() < store::MIN_PASSWORD_LEN {
        return Resp::err(400, "password too short (at least 8 characters)");
    }
    // Validate + spend the token (single-use, unexpired). This is the ONLY authorization for the write;
    // a wrong/expired/spent token is a flat 400 and no password changes.
    let Some(username) = ctx.store.consume_password_reset_token(&token).await else {
        return Resp::err(400, "invalid or expired reset token");
    };
    let (pw_hash, salt, kdf_id) = match hash_new_password(ctx, &new_password).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match ctx.store.set_password(&username, &pw_hash, &salt, &kdf_id).await {
        Ok(true) => {}
        // The token proved this account existed when minted; a false here is a concurrent delete.
        Ok(false) => return Resp::err(404, "no such user"),
        Err(_) => return Resp::err(500, "couldn't update the password"),
    }
    // A reset locks the old credential out everywhere: revoke every one of the account's sessions (there
    // is no "current" session to preserve — the caller was logged out).
    let revoked = ctx.sessions.revoke_user(&username, None);
    tracing::info!(user = %username, revoked_sessions = revoked, "password reset via token");
    audit_append(ctx.root(), &username, audit::USER_PASSWORD_RESET, None, &format!("reset via token; revoked {revoked} session(s)")).await;
    Resp::json(serde_json::json!({ "ok": true, "revoked_sessions": revoked }))
}

// ── Two-factor authentication (TOTP) ──

/// The uniform "second factor required / wrong" outcome for [`api_login`]: a 401 with the exact
/// `{"error":"2fa_required"}` body and never a session. Records a failed-auth metric + audit line.
async fn two_factor_required(ctx: &Ctx, username: &str) -> Resp {
    ctx.metrics.record_auth(AuthResult::LoginFail);
    tracing::warn!(user = %username, "login blocked: second factor required or wrong");
    audit_append(ctx.root(), username, audit::LOGIN_FAILED, None, "second factor required or wrong").await;
    Resp::err(401, "2fa_required")
}

/// `POST /api/me/2fa/enroll` — begin TOTP enrollment for the logged-in caller. Generates a fresh
/// secret, stores it **PENDING** (secret set, 2FA not yet active), and returns the secret + an
/// `otpauth://totp/agit-hub:<username>?...` provisioning URI for an authenticator app. It does NOT
/// activate 2FA — that needs [`api_2fa_confirm`]. Re-enrolling overwrites a prior *pending* secret,
/// but refuses to clobber an ALREADY-ACTIVE 2FA (disable it first) so a stray call cannot silently
/// swap out a working authenticator.
pub(crate) async fn api_2fa_enroll(ctx: &Ctx, caller: &Caller) -> Resp {
    let Some(username) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let secret = totp::gen_secret();
    let Some(uri) = totp::provisioning_uri(&secret, &username) else {
        return Resp::err(500, "couldn't build the provisioning URI");
    };
    // Persist the pending secret under the write lock; refuse if 2FA is already active.
    let sec = secret.clone();
    let outcome = ctx
        .store
        .update_users(move |users| match users.iter_mut().find(|u| u.username == username) {
            None => Err(404),
            Some(u) if u.totp_enabled => Err(409),
            Some(u) => {
                u.totp_secret = Some(sec);
                u.totp_enabled = false;
                u.totp_backup_codes = vec![];
                Ok(())
            }
        })
        .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(409)) => return Resp::err(409, "2FA is already enabled; disable it first to re-enroll"),
        Ok(Err(_)) => return Resp::err(404, "no such user"),
        Err(_) => return Resp::err(500, "couldn't start enrollment"),
    }
    audit_append(ctx.root(), caller.user.as_deref().unwrap_or(""), audit::TWOFA_ENROLL, None, "").await;
    Resp::json(serde_json::json!({
        "secret": secret,
        "otpauth_uri": uri,
        "issuer": totp::ISSUER,
        "account": caller.user,
    }))
}

/// `POST /api/me/2fa/confirm` `{ code }` — verify a 6-digit TOTP against the PENDING secret (±1 time
/// step for clock skew). On success 2FA goes **active** and 10 one-time backup codes are minted and
/// returned **once** (only their sha256 digests are stored). No pending enrollment → 400; a wrong
/// code → 401.
pub(crate) async fn api_2fa_confirm(ctx: &Ctx, caller: &Caller, body: &[u8]) -> Resp {
    let Some(username) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(code) = str_field(&v, "code") else {
        return Resp::err(400, "want a 6-digit code");
    };
    let Some(user) = ctx.store.user(&username).await else {
        return Resp::err(404, "no such user");
    };
    if user.totp_enabled {
        return Resp::err(409, "2FA is already enabled");
    }
    let Some(secret) = user.totp_secret.clone() else {
        return Resp::err(400, "no pending enrollment; call enroll first");
    };
    if !totp::verify(&secret, &username, &code) {
        return Resp::err(401, "that code is not valid");
    }
    let (plain, hashes) = match totp::gen_backup_codes(totp::BACKUP_CODES) {
        Ok(x) => x,
        Err(_) => return Resp::err(500, "no system entropy available, try again shortly"),
    };
    // Activate under the write lock, re-checking the pending secret has not rotated under us (a
    // concurrent re-enroll) — the code proved control of *this* secret, so activating a different one
    // would be wrong.
    let activated = ctx
        .store
        .update_users(move |users| match users.iter_mut().find(|u| u.username == username) {
            Some(u) if !u.totp_enabled && u.totp_secret.as_deref() == Some(secret.as_str()) => {
                u.totp_enabled = true;
                u.totp_backup_codes = hashes;
                true
            }
            _ => false,
        })
        .await;
    match activated {
        Ok(true) => {}
        Ok(false) => return Resp::err(409, "enrollment changed; start over"),
        Err(_) => return Resp::err(500, "couldn't activate 2FA"),
    }
    audit_append(ctx.root(), caller.user.as_deref().unwrap_or(""), audit::TWOFA_ENABLE, None, &format!("{} backup codes issued", plain.len())).await;
    Resp::json(serde_json::json!({ "enabled": true, "backup_codes": plain }))
}

/// `POST /api/me/2fa/disable` `{ code_or_password }` — turn the caller's own 2FA off. Any ONE of a
/// current TOTP code, an unused backup code, or the account password proves control. On success the
/// secret and backup-code digests are cleared. A missing/wrong proof → 401.
pub(crate) async fn api_2fa_disable(ctx: &Ctx, caller: &Caller, body: &[u8]) -> Resp {
    let Some(username) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(proof) = str_field(&v, "code_or_password") else {
        return Resp::err(400, "want code_or_password");
    };
    let Some(user) = ctx.store.user(&username).await else {
        return Resp::err(404, "no such user");
    };
    if !user.totp_enabled {
        return Resp::err(400, "2FA is not enabled");
    }
    let secret = user.totp_secret.clone().unwrap_or_default();
    // Try the cheap checks (TOTP, then backup code) before spending an argon2 verify on the password.
    let ok = if totp::verify(&secret, &username, &proof) || totp::consume_backup_code(&proof, &user.totp_backup_codes).is_some() {
        true
    } else {
        // Password fallback, under the login gate + blocking pool exactly like `api_login`, so this is
        // not an uncapped argon2 amplifier.
        let _slot = ctx.login_gate.acquire().await.expect("login gate semaphore is never closed");
        auth::verify_login(&ctx.store, &username, &proof).await.is_some()
    };
    if !ok {
        return Resp::err(401, "that code or password is not valid");
    }
    let cleared = ctx
        .store
        .update_users(move |users| match users.iter_mut().find(|u| u.username == username) {
            Some(u) => {
                u.totp_secret = None;
                u.totp_enabled = false;
                u.totp_backup_codes = vec![];
                true
            }
            None => false,
        })
        .await;
    match cleared {
        Ok(true) => {}
        Ok(false) => return Resp::err(404, "no such user"),
        Err(_) => return Resp::err(500, "couldn't disable 2FA"),
    }
    audit_append(ctx.root(), caller.user.as_deref().unwrap_or(""), audit::TWOFA_DISABLE, None, "").await;
    Resp::json(serde_json::json!({ "enabled": false }))
}

/// `POST /api/users/<username>/2fa-disable` — admin-only recovery for a user locked out of their
/// authenticator. Clears the target's 2FA entirely (secret, active flag, backup codes). Gated on
/// `caller.is_admin`; no code is asked for (the point is the user cannot supply one). The sibling of
/// the admin password-reset door.
pub(crate) async fn api_admin_2fa_disable(ctx: &Ctx, caller: &Caller, username: &str) -> Resp {
    let Some(actor) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    if !caller.is_admin {
        return Resp::err(403, "admin only");
    }
    let target = store::normalize_username(username);
    // An admin already sees every account, so a plain 404 for an unknown name leaks nothing.
    if ctx.store.user(&target).await.is_none() {
        return Resp::err(404, "no such user");
    }
    let t = target.clone();
    let cleared = ctx
        .store
        .update_users(move |users| match users.iter_mut().find(|u| u.username == t) {
            Some(u) => {
                u.totp_secret = None;
                u.totp_enabled = false;
                u.totp_backup_codes = vec![];
                true
            }
            None => false,
        })
        .await;
    if cleared.is_err() {
        return Resp::err(500, "couldn't clear 2FA");
    }
    tracing::info!(actor = %actor, user = %target, "admin cleared 2FA");
    audit_append(ctx.root(), &actor, audit::TWOFA_ADMIN_DISABLE, None, &format!("cleared 2FA for {target}")).await;
    Resp::json(serde_json::json!({ "ok": true, "user": target, "enabled": false }))
}

/// `POST /api/verify-email` `{ token }` — consume an email-verification token and mark the owning account
/// verified. UNAUTHENTICATED: the unguessable token IS the capability (like an org invitation id), so no
/// session is required — anyone holding the link can complete the proof. The token is single-use and
/// expiring; an unknown or expired one is a flat 400. Idempotent-safe: verifying an already-verified
/// account is fine (the flag is simply set true again), but a spent token cannot be replayed.
pub(crate) async fn api_verify_email(ctx: &Ctx, body: &[u8]) -> Resp {
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(token) = str_field(&v, "token").filter(|s| !s.trim().is_empty()) else {
        return Resp::err(400, "want a verification token");
    };
    let Some((username, _email)) = ctx.store.consume_email_token(&token).await else {
        return Resp::err(400, "invalid or expired verification token");
    };
    match ctx.store.set_email_verified(&username, true).await {
        // The token proved this account, so the row exists; a false here would be a concurrent delete.
        Ok(true) => {}
        Ok(false) => return Resp::err(404, "no such user"),
        Err(_) => return Resp::err(500, "couldn't record the verification"),
    }
    tracing::info!(user = %username, "email verified via token");
    audit_append(ctx.root(), &username, audit::USER_EMAIL_VERIFY, None, "verified via token").await;
    // Minimal body (no username/email echo): this is an UNAUTHENTICATED endpoint, so it must not
    // confirm which account/email a token belongs to. Mirrors password-reset consume's minimal body.
    Resp::json(serde_json::json!({ "ok": true, "email_verified": true }))
}

/// `POST /api/me/verify/resend` — mint + deliver a fresh verification token for the logged-in caller's
/// registered email (the operator-forwarded delivery: the URL is logged and printable via the CLI). The
/// token is NEVER returned in the response — that would defeat verification. A caller with no email on
/// file yet (identity not enrolled) is a 400 with a hint.
pub(crate) async fn api_me_verify_resend(ctx: &Ctx, caller: &Caller) -> Resp {
    let Some(username) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    match crate::emailverify::mint_and_deliver(&ctx.store, &username).await {
        Some(_url) => {
            audit_append(ctx.root(), &username, audit::USER_EMAIL_RESEND, None, "self-service resend").await;
            Resp::json(serde_json::json!({ "ok": true }))
        }
        None => Resp::err(400, "no email on file to verify — enroll an identity email first (agit identity enroll)"),
    }
}

/// `POST /api/users/<username>/verify-email` — admin-only force-verify (the out-of-band operator vouch,
/// e.g. after confirming the address by hand). Gated on `caller.is_admin`; sets the target's
/// `email_verified` directly, no token consumed. The sibling of the admin password-reset / 2FA-disable
/// doors. Idempotent: force-verifying an already-verified account is a no-op success.
pub(crate) async fn api_admin_verify_email(ctx: &Ctx, caller: &Caller, username: &str) -> Resp {
    let Some(actor) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    if !caller.is_admin {
        return Resp::err(403, "admin only");
    }
    let target = store::normalize_username(username);
    if ctx.store.user(&target).await.is_none() {
        return Resp::err(404, "no such user");
    }
    match ctx.store.set_email_verified(&target, true).await {
        Ok(true) => {}
        Ok(false) => return Resp::err(404, "no such user"),
        Err(_) => return Resp::err(500, "couldn't record the verification"),
    }
    tracing::info!(actor = %actor, user = %target, "admin force-verified email");
    audit_append(ctx.root(), &actor, audit::USER_EMAIL_VERIFY, None, &format!("admin force-verified {target}")).await;
    Resp::json(serde_json::json!({ "ok": true, "user": target, "email_verified": true }))
}

// ── admin user roster (list / create / disable / enable) ──
//
// The web Admin panel's account management, previously CLI-only. Every door here is admin-only AND
// login-session only: a token — even one owned by an admin — is refused, exactly like the site-wide
// audit log. Issuing/using a token is the automation path; managing the human roster is a person's own
// session decision, so the `caller.token.is_some()` check below is deliberate, not incidental.

/// The admin-only + login-session gate shared by every roster endpoint. Returns the acting admin's
/// username on success, or the Resp to return on refusal: 401 for anonymous, 403 for a token caller
/// (even an admin's — this is a person-at-a-keyboard action), 403 for a non-admin.
fn require_admin_session(caller: &Caller) -> Result<String, Resp> {
    let Some(actor) = caller.user.clone() else {
        return Err(Resp::err(401, "login required"));
    };
    // A token is never enough for roster management, mirroring the site-wide audit log gate. Checked
    // before is_admin so an admin's token is refused as a token, not silently accepted.
    if caller.token.is_some() {
        return Err(Resp::err(403, "roster management takes a login session; a token can't do this"));
    }
    if !caller.is_admin {
        return Err(Resp::err(403, "admin only"));
    }
    Ok(actor)
}

/// One roster row: the account facts the panel renders. Only non-secret metadata — never `pw_hash`,
/// `salt`, or the TOTP secret. `totp_enabled` is the active-2FA flag (not whether an enrollment is
/// merely pending), matching what `GET /api/me` and the account page already expose about the caller.
fn roster_json(u: &store::User) -> serde_json::Value {
    serde_json::json!({
        "username": u.username,
        "is_admin": u.is_admin,
        "totp_enabled": u.totp_enabled,
        "email_verified": u.email_verified,
        "disabled": u.disabled,
        "created": u.created,
    })
}

/// `GET /api/users` — the full account roster (admin + login-session only). Answers `{ users: [...] }`,
/// each row the non-secret facts in [`roster_json`], sorted by username for a stable panel render.
pub(crate) async fn api_users_list(ctx: &Ctx, caller: &Caller) -> Resp {
    if let Err(r) = require_admin_session(caller) {
        return r;
    }
    let mut users = ctx.store.users().await;
    users.sort_by(|a, b| a.username.cmp(&b.username));
    let rows: Vec<serde_json::Value> = users.iter().map(roster_json).collect();
    Resp::json(serde_json::json!({ "users": rows }))
}

/// `POST /api/users` `{ username, password, is_admin? }` — admin creates an account (admin + login-session
/// only). Reuses registration's exact rules: username validity + reserved-name refusal, the unified
/// (user ∩ org) taken check, the shared minimum password length, and the argon2 hash under the login gate.
/// `is_admin` is honored ONLY because the whole endpoint is already admin-gated — a non-admin never
/// reaches the body parse (they are refused by `require_admin_session`), so this can never be an
/// escalation door. Unlike self-service registration, this does NOT log the new account in (no session
/// cookie): the admin stays themselves.
pub(crate) async fn api_users_create(ctx: &Ctx, caller: &Caller, body: &[u8]) -> Resp {
    let actor = match require_admin_session(caller) {
        Ok(a) => a,
        Err(r) => return r,
    };
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
    // Same minimum as registration / the CLI, via the one shared constant.
    if password.chars().count() < store::MIN_PASSWORD_LEN {
        return Resp::err(400, "password too short (at least 8 characters)");
    }
    // Unified account namespace: a username and an org name may never share a bare string. Checked here
    // like registration does; the users PRIMARY KEY makes the user-vs-user race safe (clean AlreadyExists).
    if ctx.store.org(&username).await.is_some() {
        return Resp::err(409, "that username is taken");
    }
    // `is_admin` defaults to false when the field is absent; only an admin (already gated above) can set it.
    let is_admin = v.get("is_admin").and_then(|b| b.as_bool()).unwrap_or(false);
    let (pw_hash, salt, kdf_id) = match hash_new_password(ctx, &password).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let user = store::User {
        username: username.clone(),
        pw_hash,
        salt,
        kdf: kdf_id,
        is_admin,
        created: store::now_iso(),
        ..Default::default()
    };
    match ctx.store.add_user(user).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Resp::err(409, "that username is taken"),
        Err(_) => return Resp::err(500, "couldn't create the account"),
    }
    tracing::info!(actor = %actor, user = %username, admin = is_admin, "admin created user");
    audit_append(ctx.root(), &actor, audit::USER_ADD, None, &format!("created {username}{}", if is_admin { " (admin)" } else { "" })).await;
    Resp::json(serde_json::json!({ "ok": true, "user": username, "is_admin": is_admin }))
}

/// `POST /api/users/<u>/disable` and `/enable` — admin soft-suspend / un-suspend an account (admin +
/// login-session only). On DISABLE, EVERY one of the target's live sessions is revoked, so a suspension
/// takes effect immediately rather than waiting for the session TTL. GUARDS on disable: an admin may not
/// disable their OWN account (self-lockout), and may not disable the LAST remaining admin (which would
/// leave the hub with no one able to administer it). Enable has no guards. Idempotent: disabling an
/// already-disabled account (or enabling an active one) is a success that re-asserts the state.
pub(crate) async fn api_admin_set_disabled(ctx: &Ctx, caller: &Caller, username: &str, disabled: bool) -> Resp {
    let actor = match require_admin_session(caller) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let target = store::normalize_username(username);
    let Some(target_user) = ctx.store.user(&target).await else {
        // An admin already sees every account (they can list the roster), so a plain 404 leaks nothing.
        return Resp::err(404, "no such user");
    };
    if disabled {
        // Never leave the hub un-administrable: refuse to disable the last ENABLED admin. Count admins who
        // are not already disabled; if the target is the only one, block it. Checked BEFORE the self guard,
        // so the sole admin disabling themselves gets the more informative "last admin" message. (A
        // non-admin target skips this — disabling a normal user can never exhaust the admin set.)
        if target_user.is_admin {
            let enabled_admins = ctx.store.users().await.into_iter().filter(|u| u.is_admin && !u.disabled).count();
            if enabled_admins <= 1 {
                return Resp::err(400, "can't disable the last remaining admin");
            }
        }
        // An admin locking THEMSELVES out (while other admins remain) is almost always a mistake and
        // strands their own session; refuse it. Compared on the normalized actor (caller.user is already
        // the stored, normalized username).
        if target == store::normalize_username(&actor) {
            return Resp::err(400, "you can't disable your own account");
        }
    }
    match ctx.store.set_user_disabled(&target, disabled).await {
        Ok(true) => {}
        Ok(false) => return Resp::err(404, "no such user"),
        Err(_) => return Resp::err(500, "couldn't update the account"),
    }
    // On disable, kick every live session for the account (recovery/lockout action — a suspended user
    // must not keep a working cookie). None = keep nothing; the target is not the caller (self-disable is
    // refused above), so there is no "current" session to preserve.
    let revoked = if disabled { ctx.sessions.revoke_user(&target, None) } else { 0 };
    let (action, verb) = if disabled { (audit::USER_DISABLE, "disabled") } else { (audit::USER_ENABLE, "enabled") };
    tracing::info!(actor = %actor, user = %target, revoked_sessions = revoked, "admin {verb} user");
    audit_append(ctx.root(), &actor, action, None, &format!("{verb} {target}; revoked {revoked} session(s)")).await;
    Resp::json(serde_json::json!({ "ok": true, "user": target, "disabled": disabled, "revoked_sessions": revoked }))
}

// ── shared identity registry (encryption-recipients Wave 1) ──
//
// The ONE registry serving both provenance signing-key lookup and (Wave 2+) encryption recipient
// key-wrapping. Enroll writes the CALLER's own row and proves possession; the gets are public reads for
// any authenticated caller (public keys are public).

/// The largest hex field the registry accepts, and the exact lengths for each fixed-width value. An
/// ed25519/X25519 public key is 32 bytes (64 hex chars); an ed25519 signature is 64 bytes (128 hex).
const ED25519_PUB_HEX: usize = 64;
const X25519_PUB_HEX: usize = 64;
const ENROLL_SIG_HEX: usize = 128;
/// The most usernames one batch lookup will resolve — bounds the work a single request can trigger.
const IDENTITY_BATCH_MAX: usize = 256;

/// Whether `s` is exactly `n` lowercase/uppercase hex characters. Rejects both an oversized field and a
/// non-hex one, so the registry only ever stores well-formed, fixed-width public material.
fn is_hex_len(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The public shape of one registry DEVICE key: pubkeys are public, so every field is disclosable to any
/// authenticated caller. This is the row both provenance-verify and (later) recipient-wrap consume.
fn identity_json(k: &store::IdentityKey) -> serde_json::Value {
    serde_json::json!({
        "username": k.username,
        "key_fpr": k.key_fpr,
        "ed25519_pub": k.ed25519_pub,
        "x25519_pub": k.x25519_pub,
        "label": k.label,
        "epoch": k.epoch,
        "enroll_sig": k.enroll_sig,
        "revoked": k.revoked,
        "created": k.created,
        "email": k.email,
    })
}

/// The public shape of an account's device-key SET: the `keys` array (one entry per non-revoked device
/// key), plus the PRIMARY key's fields mirrored at the top level for back-compat with single-key readers
/// (the encryption path's `hub_x25519`, which wraps to the primary device key this wave). `keys[0]` is the
/// primary (the list is latest-first). Callers must have at least one key — an empty set is a 404 upstream.
fn identity_set_json(keys: &[store::IdentityKey]) -> serde_json::Value {
    let primary = &keys[0];
    serde_json::json!({
        "username": primary.username,
        "ed25519_pub": primary.ed25519_pub,
        "x25519_pub": primary.x25519_pub,
        "epoch": primary.epoch,
        "keys": keys.iter().map(identity_json).collect::<Vec<_>>(),
    })
}

/// `POST /api/identity/enroll` `{ ed25519_pub, x25519_pub, epoch, enroll_sig }` — publish/rotate the
/// CALLER's own registry row. Authenticated; the username is the authenticated caller and is NEVER read
/// from the body, so a user can only ever enroll a key under their own name.
///
/// The server re-derives `enroll_sig`'s message from the caller's username + the submitted fields and
/// verifies it against the SUBMITTED `ed25519_pub` (possession proof), then requires a strictly higher
/// epoch than any stored one (monotonic, no rollback). A bad/oversized field, a failed signature, or a
/// stale epoch is a 400 — the hub can only ever replace a row, never mint a valid one.
pub(crate) async fn api_identity_enroll(ctx: &Ctx, caller: &Caller, body: &[u8]) -> Resp {
    let Some(username) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let (Some(ed25519_pub), Some(x25519_pub)) = (str_field(&v, "ed25519_pub"), str_field(&v, "x25519_pub")) else {
        return Resp::err(400, "want ed25519_pub and x25519_pub");
    };
    let Some(enroll_sig) = str_field(&v, "enroll_sig") else {
        return Resp::err(400, "want enroll_sig");
    };
    // epoch is a non-negative integer; a missing/negative/oversized one is a 400 (the JSON number is
    // read as i64, so a value beyond i64 fails `as_i64` and lands here).
    let Some(epoch) = v.get("epoch").and_then(|x| x.as_i64()).filter(|e| *e >= 0) else {
        return Resp::err(400, "want a non-negative integer epoch");
    };
    if !is_hex_len(&ed25519_pub, ED25519_PUB_HEX) || !is_hex_len(&x25519_pub, X25519_PUB_HEX) {
        return Resp::err(400, "ed25519_pub and x25519_pub must each be 32-byte hex (64 chars)");
    }
    if !is_hex_len(&enroll_sig, ENROLL_SIG_HEX) {
        return Resp::err(400, "enroll_sig must be a 64-byte ed25519 signature (128 hex chars)");
    }
    // Possession proof: verify enroll_sig over (username ‖ epoch ‖ ed25519_pub ‖ x25519_pub) against
    // the SUBMITTED ed25519_pub. The username is the authenticated caller, so a signature made for a
    // different name will not verify here.
    let msg = agit::agent::identity_enroll_message(&username, epoch, &ed25519_pub, &x25519_pub);
    if !agit::agent::verify_hex(&ed25519_pub, &msg, &enroll_sig) {
        return Resp::err(400, "enroll_sig does not verify against the submitted ed25519_pub");
    }
    // The committer email is optional and self-asserted (NOT covered by enroll_sig): it is the bridge from
    // a session's committer to this account for provenance attribution. Absent/blank = unset. The store
    // normalizes it (trim + lowercase) on write. An oversized value is refused to bound the row.
    let email = str_field(&v, "email").unwrap_or_default();
    if email.len() > 320 {
        return Resp::err(400, "email is too long (max 320 chars)");
    }
    // An optional device label (the machine hostname by default, client-side) — self-asserted, bounded.
    let label = str_field(&v, "label").unwrap_or_default();
    if label.len() > 64 {
        return Resp::err(400, "label is too long (max 64 chars)");
    }
    let key_fpr = store::ed25519_fingerprint(&ed25519_pub);
    // Capture the account's PRIMARY-key email BEFORE the add, so we can tell whether this enroll CHANGES the
    // account's asserted address. Normalized both sides, matching how the store stores + matches emails.
    let new_email = store::normalize_email(&email);
    let prev_email = ctx.store.get_primary_identity_key(&username).await.map(|k| store::normalize_email(&k.email)).unwrap_or_default();
    let row = store::IdentityKey {
        username: username.clone(),
        key_fpr: key_fpr.clone(),
        ed25519_pub,
        x25519_pub,
        label,
        epoch,
        enroll_sig,
        created: store::now_iso(),
        revoked: None,
        email,
    };
    // ADD a device key for the caller — this never overwrites the caller's OTHER keys, and (since username
    // is the authenticated caller) never another user's key.
    match ctx.store.add_identity_key(row).await {
        Ok(store::EnrollOutcome::Applied) => {}
        Ok(store::EnrollOutcome::StaleEpoch { stored }) => {
            return Resp::err(400, &format!("epoch {epoch} does not advance this device key's enrolled epoch {stored}"));
        }
        Err(_) => return Resp::err(500, "couldn't record the identity key"),
    }
    audit_append(ctx.root(), &username, audit::IDENTITY_ENROLL, None, &format!("epoch={epoch} key_fpr={key_fpr}")).await;
    // Anti-squatting: a CHANGED committer email must be RE-PROVEN. Reset `email_verified` to false and mint
    // a fresh verification token whenever the enrolled email differs from the prior one — otherwise a user
    // could verify address A, then re-enroll claiming address B and keep the verified flag, re-opening the
    // squatting hole. An unchanged email leaves the verified state (and any prior proof) untouched.
    if new_email != prev_email {
        let _ = ctx.store.set_email_verified(&username, false).await;
        if !new_email.is_empty() {
            let _ = crate::emailverify::deliver(&ctx.store, &username, &new_email).await;
            audit_append(ctx.root(), &username, audit::USER_EMAIL_RESEND, None, "identity enroll changed the email").await;
        }
    }
    Resp::json(serde_json::json!({ "username": username, "epoch": epoch, "key_fpr": key_fpr }))
}

/// `GET /api/identity/<user>` — one person's published device-key SET (`{ username, keys: [...], + the
/// primary key's fields at top level }`). Any authenticated caller may read (pubkeys are public). An
/// account with no non-revoked key is a 404, matching the hub's non-disclosure elsewhere.
pub(crate) async fn api_identity_get(ctx: &Ctx, caller: &Caller, user: &str) -> Resp {
    if caller.user.is_none() {
        return Resp::err(401, "login required");
    }
    let keys = ctx.store.list_identity_keys(user).await;
    if keys.is_empty() {
        return Resp::err(404, "not found");
    }
    Resp::json(identity_set_json(&keys))
}

/// `DELETE /api/identity/keys/<key_fpr>` — revoke ONE of the CALLER's own device keys. Caller-only: the
/// username is the authenticated caller, so a non-owner can never revoke someone else's key. A fingerprint
/// that names no live key of the caller's is a 404 (idempotent / non-disclosing).
pub(crate) async fn api_identity_revoke_key(ctx: &Ctx, caller: &Caller, key_fpr: &str) -> Resp {
    let Some(username) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    match ctx.store.revoke_identity_key(&username, key_fpr).await {
        Ok(true) => {
            audit_append(ctx.root(), &username, audit::IDENTITY_REVOKE, None, &format!("key_fpr={key_fpr}")).await;
            Resp::json(serde_json::json!({ "username": username, "key_fpr": key_fpr, "revoked": true }))
        }
        Ok(false) => Resp::err(404, "no such device key for this account"),
        Err(_) => Resp::err(500, "couldn't revoke the identity key"),
    }
}

/// `GET /api/identity/by-email?email=<committer-email>` — resolve a committer email to the registered
/// account owning it, returning the account + its device-key SET. Any authenticated caller may read (the
/// pubkeys and the email→account mapping are both needed to verify attribution, and registration already
/// publishes them). An email that maps to no VERIFIED account — or to more than one (ambiguous) — is a
/// normal 404, the same non-disclosing not-found the single-user GET returns.
pub(crate) async fn api_identity_by_email(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    if caller.user.is_none() {
        return Resp::err(401, "login required");
    }
    let Some(raw) = param(req.query(), "email").filter(|s| !s.trim().is_empty()) else {
        return Resp::err(400, "want an email query parameter");
    };
    // The value arrives percent-encoded (`dev%40x.com`); decode before the store normalizes and matches.
    let email = agit::hub::net::percent_decode_lossy(&raw);
    let keys = ctx.store.get_identity_keys_by_email(&email).await;
    if keys.is_empty() {
        return Resp::err(404, "not found");
    }
    Resp::json(identity_set_json(&keys))
}

/// `GET /api/identity?users=a,b,c` — a batch lookup. Any authenticated caller may read. Unknown users
/// are simply omitted from `keys` (non-disclosing); the batch is capped at [`IDENTITY_BATCH_MAX`].
pub(crate) async fn api_identity_list(ctx: &Ctx, req: &Req, caller: &Caller) -> Resp {
    if caller.user.is_none() {
        return Resp::err(401, "login required");
    }
    let raw = param(req.query(), "users").unwrap_or_default();
    let names: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .take(IDENTITY_BATCH_MAX)
        .collect();
    let keys = ctx.store.get_identity_keys(&names).await;
    let arr: Vec<serde_json::Value> = keys.iter().map(identity_json).collect();
    Resp::json(serde_json::json!({ "keys": arr }))
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
        // Creating under an org is allowed for an org admin (or a site admin), OR for a plain member
        // when the org's `members_can_create` policy permits it (the GitHub default). A member who is
        // refused gets a distinct 403 — they already know the org exists (the 404 non-disclosure above
        // has already let them through), so naming the policy leaks nothing.
        let may_create = caller.is_admin || o.is_admin(&user) || (o.is_member(&user) && o.members_can_create != 0);
        if !may_create {
            return Resp::err(403, "this org only lets admins create agents");
        }
    }
    // Whether to bootstrap the fresh bare repo into an immediately-cloneable, sessionless agent store
    // (one commit carrying an agent.toml with a minted aid). Default FALSE: the plain create is a name
    // reservation for pushing an EXISTING agent, and initializing it would collide with that push.
    let initialize = v.get("initialize").and_then(|x| x.as_bool()).unwrap_or(false);
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
        Ok(created) => {
            audit_append(ctx.root(), &user, audit::AGENT_CREATE, Some(&format!("{seg}/{name}")), &format!("visibility={} owner={owner} initialize={initialize}", visibility.as_str())).await;
            let repo = repo_path(ctx.root(), &seg, &name);
            // Bootstrap (initialize=true) mints an aid + commits agent.toml so the store is immediately
            // cloneable; otherwise the aid is read out of whatever the empty repo has (None for a fresh
            // reservation). Both the bootstrap and the read shell out to git, so they run on the blocking
            // pool. On a bootstrap failure the reservation still stands — report it as not-initialized.
            let name_for_git = name.clone();
            let (aid, aid_source, initialized) = tokio::task::spawn_blocking(move || {
                // Only bootstrap a repo we just CREATED. If create_agent merely re-recorded a claimed,
                // pre-existing repo (created=false), initializing would force a fresh orphan root over its
                // history and mint a conflicting aid -- so skip it, exactly as the CLI does.
                if initialize && created {
                    match initialize_store(&repo, &name_for_git) {
                        Ok(aid) => (Some(aid), "agent.toml", true),
                        Err(_) => {
                            let (aid, src) = agent_aid(&repo);
                            (aid, src, false)
                        }
                    }
                } else {
                    let (aid, src) = agent_aid(&repo);
                    (aid, src, false)
                }
            })
            .await
            .unwrap();
            Resp::json_status(
                201,
                serde_json::json!({
                    "name": name,
                    "owner": owner,
                    "full_name": format!("{seg}/{name}"),
                    // An un-initialized empty repo has no agent.toml yet — the aid only exists once the
                    // client pushes it (or `initialize` bootstraps one). Report null honestly.
                    "aid": aid,
                    "aid_source": aid_source,
                    // Whether the hub bootstrapped a valid, immediately-cloneable store on create.
                    "initialized": initialized,
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
    // A caller who cannot read the resolved agent must not tell a known-but-private aid from an unknown
    // one. Decide readability SILENTLY here rather than via gate(): gate() writes an audit-deny fs
    // record (and does deny-response work) on a private hit but NOT on an unknown aid, which is itself an
    // existence oracle (a persistent side-effect and a timing tell) even when both return an identical
    // 404. This endpoint is a convenience lookup, so a denied by-aid read is simply an unaudited 404,
    // exactly what the unknown-aid branch above returns.
    let seg = meta.seg().to_string();
    if !acl::decide(caller, &agent_acl(ctx, &meta).await, Action::Read).allowed() {
        return Resp::err(404, "not found");
    }
    // Post-decision repo-existence check, same as gate(): an authorized caller still 404s on an empty
    // namespace without it ever being an oracle for the unauthorized (who already 404'd above).
    if !repo_path(ctx.root(), &seg, &meta.name).exists() {
        return Resp::err(404, "not found");
    }
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
/// An org WRITE (create / invite / members / settings) performed with a bearer TOKEN requires that
/// token to carry `Write` scope — the exact gate the agent write endpoints use. A login session
/// (`token == None`) always passes; only a token narrower than Write is refused with a 403. Returns
/// `Some(resp)` to short-circuit, `None` to proceed.
fn deny_non_write_token(caller: &Caller) -> Option<Resp> {
    if caller.token.as_ref().is_some_and(|t| t.scope != Scope::Write) {
        return Some(Resp::err(403, Deny::TokenScope.reason()));
    }
    None
}

/// A MANAGEMENT-grade org action (transfer, delete) takes a LOGIN SESSION: ANY token — even an
/// admin's Write token — is refused, mirroring the roster-management and token-minting login-session
/// rule. Returns `Some(resp)` to short-circuit, `None` to proceed.
fn deny_any_token(caller: &Caller) -> Option<Resp> {
    if caller.token.is_some() {
        return Some(Resp::err(403, "this action takes a login session; a token can't do it"));
    }
    None
}

pub(crate) async fn api_orgs_create(ctx: &Ctx, caller: &Caller, body: &[u8]) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    if let Some(r) = deny_non_write_token(caller) {
        return r;
    }
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
                current_kek_gen: 0,
                recovery_x25519: String::new(),
                escrow_mode: "none".into(),
                // Members can create by default — the GitHub default. An admin can restrict it later
                // via POST /api/orgs/<org>/settings.
                members_can_create: 1,
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

/// `GET /api/orgs/<name>` - org detail. Existence non-disclosure, the same shape as the agent gate: a
/// missing org and one the caller may not see both answer 404, so org names cannot be enumerated.
pub(crate) async fn api_org_get(ctx: &Ctx, caller: &Caller, name: &str) -> Resp {
    let org = ctx.store.org(name).await;
    let visible = |o: &Org| caller.is_admin || caller.user.as_deref().is_some_and(|u| o.is_member(u));
    match org {
        Some(o) if visible(&o) => Resp::json(serde_json::json!({
            "name": o.name,
            "created": o.created,
            "members": org_members_json(&o),
            // Wave-5 opt-in state (both default to unset/none). The client reads recovery_x25519 during
            // `team rekey` to add the `@recovery` envelope, and escrow_mode to gate `a escrow enable`.
            "recovery_x25519": o.recovery_x25519,
            "escrow_mode": o.escrow_mode,
            // Member-create policy (bool for the UI toggle): true = members may create agents under the
            // org (the GitHub default), false = admins only.
            "members_can_create": o.members_can_create != 0,
        })),
        _ => Resp::err(404, "not found"),
    }
}

/// `GET /api/orgs/<name>/overview` - the org view: its members plus every agent the caller may Read
/// that is owned by the org OR by one of its members (a member's personal agents included). The gate is
/// **identical** to [`api_org_get`]: a missing org and one the caller may not see both answer 404, so
/// membership stays non-disclosing (a non-member cannot tell "no such org" from "not your org").
///
/// The security core is that the agent list is **ACL-filtered before it is ever counted**: each agent's
/// effective ACL (org members folded in via [`agent_acl`]) runs through `acl::decide(_, Read)`, and only
/// allowed agents make the list. A member's PRIVATE personal agent - which does not inherit org grants -
/// simply never appears for a caller without an explicit grant, and no count leaks its existence.
pub(crate) async fn api_org_overview(ctx: &Ctx, caller: &Caller, name: &str) -> Resp {
    // Existence non-disclosure: missing org and not-a-member both 404, byte-for-byte api_org_get.
    let Some(org) = ctx.store.org(name).await else {
        return Resp::err(404, "not found");
    };
    if !(caller.is_admin || caller.user.as_deref().is_some_and(|u| org.is_member(u))) {
        return Resp::err(404, "not found");
    }

    // The owner strings that "belong to" this org: the org itself (`org:<name>`) and each member's
    // bare username (their personal namespace). A member's personal agent is only listed when the
    // caller may Read it - org grants never fold into a personal agent, so a private one stays hidden.
    let org_owner = format!("org:{}", org.name);
    let members: std::collections::HashSet<&str> = org.members.iter().map(|m| m.username.as_str()).collect();

    // Filter BEFORE anything is counted (never leak counts of hidden agents), exactly as api_agents does.
    // The per-agent ACL read is an async store call, so this is a loop, not an iterator chain.
    let mut visible: Vec<AgentMeta> = Vec::new();
    for (seg, n) in list_agents(ctx.root()) {
        let meta = ctx.store.agent_or_unowned(&seg, &n).await;
        let Some(owner) = meta.owner.as_deref() else {
            continue; // unowned repos belong to no org member
        };
        if owner != org_owner && !members.contains(owner) {
            continue;
        }
        if !acl::decide(caller, &agent_acl(ctx, &meta).await, Action::Read).allowed() {
            continue;
        }
        visible.push(meta);
    }

    // The session tree per agent shells out (ls-tree), so the whole fan-out runs on the blocking pool in
    // one shot; each row's session count + distinct env slugs come back for the JSON.
    let root = ctx.root().to_path_buf();
    let paths: Vec<(String, String)> = visible.iter().map(|m| (m.seg().to_string(), m.name.clone())).collect();
    let git_info: Vec<(usize, Vec<String>)> = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|(seg, n)| {
                let repo = repo_path(&root, &seg, &n);
                if !has_head(&repo) {
                    return (0usize, Vec::<String>::new());
                }
                let refs = session_refs(&repo);
                let count = refs.len();
                // Distinct env slugs, first-appearance order. Old-layout sessions (env = None) carry no
                // slug, so they contribute a session count but no environment chip.
                let mut envs: Vec<String> = Vec::new();
                for r in &refs {
                    if let Some(e) = &r.env {
                        if !envs.contains(e) {
                            envs.push(e.clone());
                        }
                    }
                }
                (count, envs)
            })
            .collect()
    })
    .await
    .unwrap();

    let agents: Vec<serde_json::Value> = visible
        .iter()
        .zip(git_info)
        .map(|(m, (count, envs))| {
            let owner = m.owner.clone().unwrap_or_default();
            // "personal" = a bare username namespace, not `org:<name>`. The org's own agents are not
            // personal; a member's are.
            let personal = !owner.starts_with("org:");
            serde_json::json!({
                "owner": owner,
                "name": m.name,
                "aid": m.aid,
                "visibility": m.visibility,
                "sessions": count,
                "role": effective_role(caller, m),
                "personal": personal,
                "environments": envs,
            })
        })
        .collect();

    Resp::json(serde_json::json!({
        "name": org.name,
        "created": org.created,
        "members": org_members_json(&org),
        "agents": agents,
    }))
}

// ── Team-KEK envelopes (encryption-recipients Wave 3) ──
//
// The hub stores CIPHERTEXT only: the client computes every X25519 seal of TK, the hub just files the
// per-member rows and tracks `current_kek_gen`. Authorization is an ORG gate (never `acl::decide`):
// publishing needs org-admin; fetching your OWN envelope / listing your gens needs only membership. A
// caller who cannot see the org gets the same 404 as a missing one (existence non-disclosure).

/// The most envelopes one publish may carry, and the largest a single `wrapped_kek` string may be. A
/// TK envelope packs epk(32)‖nonce(24)‖ciphertext(48) base64'd (~140 chars); the bound is generous.
const KEK_ENVELOPES_MAX: usize = 4096;
const KEK_WRAP_MAX: usize = 1024;

/// `/api/orgs/<org>/kek/<envelopes|envelope|gens>` — dispatch the three Team-KEK routes behind one
/// membership gate.
pub(crate) async fn api_org_kek(
    ctx: &Ctx,
    caller: &Caller,
    name: &str,
    tail: &str,
    method: &str,
    query: &str,
    body: &[u8],
) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    // 404 for a missing org OR one the caller can't see — existence non-disclosure, as everywhere else.
    let Some(org) = ctx.store.org(name).await else {
        return Resp::err(404, "not found");
    };
    if !(caller.is_admin || org.is_member(&user)) {
        return Resp::err(404, "not found");
    }
    let sub = tail.strip_prefix('/').unwrap_or("");
    match (method, sub) {
        ("POST", "envelopes") => api_org_kek_publish(ctx, caller, &org, &user, body).await,
        // A member may fetch ONLY their OWN envelope: the recipient is the authenticated caller, never
        // read from the query. A missing gen envelope (or a gen the caller has none for) is a 404.
        ("GET", "envelope") => {
            let Some(gen) = param(query, "gen").and_then(|g| g.trim().parse::<i64>().ok()) else {
                return Resp::err(400, "want gen=<generation>");
            };
            match ctx.store.get_team_kek_envelope(&org.name, gen, &user).await {
                Some(e) => Resp::json(serde_json::json!({
                    "gen": e.gen,
                    "wrapped_kek": e.wrapped_kek,
                    "recipient_epoch": e.recipient_epoch,
                })),
                None => Resp::err(404, "not found"),
            }
        }
        // The generations available to THIS caller — the ones they hold an envelope for — plus the org's
        // active generation so the client can pick the newest to wrap under.
        ("GET", "gens") => {
            let mut mine = Vec::new();
            for g in ctx.store.list_team_kek_gens(&org.name).await {
                if ctx.store.get_team_kek_envelope(&org.name, g, &user).await.is_some() {
                    mine.push(g);
                }
            }
            Resp::json(serde_json::json!({ "gens": mine, "current": org.current_kek_gen }))
        }
        _ => Resp::text(405, "method not allowed"),
    }
}

/// `POST /api/orgs/<org>/kek/envelopes` `{ gen, envelopes:[{recipient, wrapped_kek, recipient_epoch}] }`
/// — ORG-ADMIN publishes a Team-KEK generation. The client computed every seal; the hub stores only the
/// ciphertext rows and advances `current_kek_gen` to `gen`. `gen` behind the current one is refused
/// (409); `gen == current` is an idempotent republish; `gen > current` advances the active generation.
async fn api_org_kek_publish(ctx: &Ctx, caller: &Caller, org: &Org, actor: &str, body: &[u8]) -> Resp {
    // The membership 404 already ran in the caller; a plain member who is not an admin is a 403.
    if !(caller.is_admin || org.is_admin(actor)) {
        return Resp::err(403, "must be an org admin to publish team-KEK envelopes");
    }
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(gen) = v.get("gen").and_then(|x| x.as_i64()).filter(|g| *g >= 1) else {
        return Resp::err(400, "want a positive integer gen");
    };
    let current = ctx.store.get_current_kek_gen(&org.name).await;
    if gen < current {
        return Resp::err(409, &format!("gen {gen} is behind the current generation {current}"));
    }
    let Some(arr) = v.get("envelopes").and_then(|e| e.as_array()) else {
        return Resp::err(400, "want envelopes: [{recipient, wrapped_kek, recipient_epoch}]");
    };
    if arr.is_empty() {
        return Resp::err(400, "envelopes must not be empty");
    }
    if arr.len() > KEK_ENVELOPES_MAX {
        return Resp::err(400, "too many envelopes in one publish");
    }
    let mut rows = Vec::with_capacity(arr.len());
    for e in arr {
        let (Some(recipient), Some(wrapped_kek)) = (str_field(e, "recipient"), str_field(e, "wrapped_kek")) else {
            return Resp::err(400, "each envelope needs recipient and wrapped_kek");
        };
        if wrapped_kek.is_empty() || wrapped_kek.len() > KEK_WRAP_MAX {
            return Resp::err(400, "wrapped_kek is missing or too large");
        }
        let recipient_epoch = e.get("recipient_epoch").and_then(|x| x.as_i64()).unwrap_or(0);
        rows.push(store::TeamKekEnvelope {
            org: org.name.clone(),
            gen,
            recipient,
            wrapped_kek,
            recipient_epoch,
            created: store::now_iso(),
        });
    }
    if ctx.store.upsert_team_kek_envelopes(&org.name, gen, &rows).await.is_err() {
        return Resp::err(500, "couldn't store the team-KEK envelopes");
    }
    // Advance the active generation ONLY after the envelopes are stored, so a reader never observes
    // current == gen without any envelopes to fetch.
    if ctx.store.set_current_kek_gen(&org.name, gen).await.is_err() {
        return Resp::err(500, "couldn't advance the team-KEK generation");
    }
    audit_append(
        ctx.root(),
        actor,
        audit::ORG_KEK_PUBLISH,
        None,
        &format!("org={} gen={gen} envelopes={}", org.name, rows.len()),
    )
    .await;
    Resp::json(serde_json::json!({ "org": org.name, "gen": gen, "envelopes": rows.len(), "current": gen }))
}

// ── Wave-5 opt-in escape hatches (both OFF by default; neither changes any wave-1..4 behavior) ──
//
// Feature 1: a per-org OFFLINE recovery recipient re-trusts an offline admin (NOT the hub) — the client
// seals TK to it during `team rekey`; the hub only files the ciphertext under `@recovery`, and the org
// still stores only a public key. Feature 2: hub-assist escrow re-trusts the HUB — the client seals its
// content key to the hub escrow PUBLIC key, and the hub releases it under the SAME `acl::decide(_, Read)`
// gate git fetch uses, fail-closed. Both are settable only by an org OWNER (an org admin, the ownership
// role — mirrors `api_org_transfer`/`api_org_delete`).

/// `GET /api/escrow/pubkey` — the hub's escrow PUBLIC key (hex). Any authenticated caller may read it; it
/// is a public key a hub-assist client seals its content key TO, so disclosing it grants nothing.
async fn api_escrow_pubkey(ctx: &Ctx, caller: &Caller) -> Resp {
    if caller.user.is_none() {
        return Resp::err(401, "login required");
    }
    Resp::json(serde_json::json!({ "pubkey": hex::encode(ctx.escrow.public) }))
}

/// `/api/orgs/<org>/recovery` — the OPT-IN offline recovery recipient (Wave 5, feature 1). GET shows it to
/// any member (empty = unset, the default); POST `{key}` sets a hex X25519 pubkey (OWNER-only); DELETE
/// clears it (OWNER-only). Existence non-disclosure: a missing/invisible org is a 404 like everywhere.
async fn api_org_recovery(ctx: &Ctx, caller: &Caller, name: &str, method: &str, body: &[u8]) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let Some(org) = ctx.store.org(name).await else {
        return Resp::err(404, "not found");
    };
    if !(caller.is_admin || org.is_member(&user)) {
        return Resp::err(404, "not found");
    }
    let is_owner = caller.is_admin || org.is_admin(&user);
    match method {
        "GET" => Resp::json(serde_json::json!({ "org": org.name, "recovery_x25519": org.recovery_x25519 })),
        "POST" => {
            if !is_owner {
                return Resp::err(403, "only an org owner may set the offline recovery recipient");
            }
            let Some(v) = json_body(body) else {
                return Resp::err(400, "want a JSON body");
            };
            let Some(key) = str_field(&v, "key") else {
                return Resp::err(400, "want key (hex X25519 pubkey)");
            };
            let key = key.trim().to_ascii_lowercase();
            // Refuse anything that is not a 32-byte hex X25519 pubkey — a junk key would silently never open.
            if hex::decode(&key).ok().filter(|b| b.len() == 32).is_none() {
                return Resp::err(400, "key must be a 64-hex-char (32-byte) X25519 public key");
            }
            let orgname = org.name.clone();
            let key2 = key.clone();
            let done = ctx
                .store
                .update_orgs(move |list| match list.iter_mut().find(|o| o.name == orgname) {
                    Some(o) => {
                        o.recovery_x25519 = key2.clone();
                        true
                    }
                    None => false,
                })
                .await
                .unwrap_or(false);
            if !done {
                return Resp::err(404, "not found");
            }
            audit_append(ctx.root(), &user, audit::ORG_RECOVERY_SET, None, &format!("org={}", org.name)).await;
            Resp::json(serde_json::json!({ "org": org.name, "recovery_x25519": key }))
        }
        "DELETE" => {
            if !is_owner {
                return Resp::err(403, "only an org owner may clear the offline recovery recipient");
            }
            let orgname = org.name.clone();
            let done = ctx
                .store
                .update_orgs(move |list| match list.iter_mut().find(|o| o.name == orgname) {
                    Some(o) => {
                        o.recovery_x25519 = String::new();
                        true
                    }
                    None => false,
                })
                .await
                .unwrap_or(false);
            if !done {
                return Resp::err(404, "not found");
            }
            audit_append(ctx.root(), &user, audit::ORG_RECOVERY_CLEAR, None, &format!("org={}", org.name)).await;
            Resp::json(serde_json::json!({ "org": org.name, "recovery_x25519": "" }))
        }
        _ => Resp::text(405, "method not allowed"),
    }
}

/// `/api/orgs/<org>/escrow` — the OPT-IN hub-assist escrow mode (Wave 5, feature 2). GET shows the mode to
/// any member; POST `{mode}` sets it to `none` | `hub-assist` (OWNER-only). Turning it on re-trusts the
/// hub with the ability to release the org's escrowed session keys under the ACL gate.
async fn api_org_escrow(ctx: &Ctx, caller: &Caller, name: &str, method: &str, body: &[u8]) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let Some(org) = ctx.store.org(name).await else {
        return Resp::err(404, "not found");
    };
    if !(caller.is_admin || org.is_member(&user)) {
        return Resp::err(404, "not found");
    }
    let is_owner = caller.is_admin || org.is_admin(&user);
    match method {
        "GET" => Resp::json(serde_json::json!({ "org": org.name, "escrow_mode": org.escrow_mode })),
        "POST" => {
            if !is_owner {
                return Resp::err(403, "only an org owner may change the escrow mode");
            }
            let Some(v) = json_body(body) else {
                return Resp::err(400, "want a JSON body");
            };
            let Some(mode) = str_field(&v, "mode") else {
                return Resp::err(400, "want mode (none | hub-assist)");
            };
            let mode = mode.trim().to_string();
            if mode != "none" && mode != "hub-assist" {
                return Resp::err(400, "mode must be none or hub-assist");
            }
            let orgname = org.name.clone();
            let mode2 = mode.clone();
            let done = ctx
                .store
                .update_orgs(move |list| match list.iter_mut().find(|o| o.name == orgname) {
                    Some(o) => {
                        o.escrow_mode = mode2.clone();
                        true
                    }
                    None => false,
                })
                .await
                .unwrap_or(false);
            if !done {
                return Resp::err(404, "not found");
            }
            audit_append(ctx.root(), &user, audit::ORG_ESCROW_MODE, None, &format!("org={} mode={mode}", org.name)).await;
            Resp::json(serde_json::json!({ "org": org.name, "escrow_mode": mode }))
        }
        _ => Resp::text(405, "method not allowed"),
    }
}

/// `/api/orgs/<org>/settings` — the org's policy knobs. Today the one knob is `members_can_create`
/// (whether a plain member may create agents under the org). GET shows it to any member; POST
/// `{ members_can_create: bool }` sets it (ORG-ADMIN only). A missing/invisible org is the uniform 404
/// (existence non-disclosure) every other org route gives.
async fn api_org_settings(ctx: &Ctx, caller: &Caller, name: &str, method: &str, body: &[u8]) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let Some(org) = ctx.store.org(name).await else {
        return Resp::err(404, "not found");
    };
    if !(caller.is_admin || org.is_member(&user)) {
        return Resp::err(404, "not found");
    }
    let is_admin = caller.is_admin || org.is_admin(&user);
    match method {
        "GET" => Resp::json(serde_json::json!({ "org": org.name, "members_can_create": org.members_can_create != 0 })),
        "POST" => {
            if let Some(r) = deny_non_write_token(caller) {
                return r;
            }
            if !is_admin {
                return Resp::err(403, "must be an org admin to change org settings");
            }
            let Some(v) = json_body(body) else {
                return Resp::err(400, "want a JSON body");
            };
            let Some(allow) = v.get("members_can_create").and_then(|x| x.as_bool()) else {
                return Resp::err(400, "want members_can_create (boolean)");
            };
            let orgname = org.name.clone();
            let val: i64 = if allow { 1 } else { 0 };
            let done = ctx
                .store
                .update_orgs(move |list| match list.iter_mut().find(|o| o.name == orgname) {
                    Some(o) => {
                        o.members_can_create = val;
                        true
                    }
                    None => false,
                })
                .await
                .unwrap_or(false);
            if !done {
                return Resp::err(404, "not found");
            }
            audit_append(ctx.root(), &user, audit::ORG_SETTINGS, None, &format!("org={} members_can_create={allow}", org.name)).await;
            Resp::json(serde_json::json!({ "org": org.name, "members_can_create": allow }))
        }
        _ => Resp::text(405, "method not allowed"),
    }
}

/// Whether an agent's OWNING ORG is in hub-assist escrow mode. A user-owned (non-org) agent has no org, so
/// it is never hub-assist — fail-closed, so escrow/release never apply outside an opted-in org.
async fn org_hub_assist(ctx: &Ctx, meta: &AgentMeta) -> bool {
    match meta.org_owner() {
        Some(org) => ctx.store.org(org).await.map(|o| o.escrow_mode == "hub-assist").unwrap_or(false),
        None => false,
    }
}

/// `POST /api/agent/<owner>/<name>/keys/escrow` `{ kid, wrapped_ck }` — store one session content key
/// sealed to the hub escrow key (Wave 5, hub-assist escrow). WRITE-gated (only a writer may escrow), and
/// only when the owning org is in `escrow_mode = 'hub-assist'`. `wrapped_ck` is ciphertext the hub cannot
/// open without its escrow SECRET, so this never hands the hub a plaintext key at rest.
async fn api_keys_escrow(ctx: &Ctx, caller: &Caller, owner: &str, name: &str, body: &[u8]) -> Resp {
    let meta = match gate(ctx, caller, owner, name, Action::Write).await {
        Ok(m) => m,
        Err(resp) => return resp,
    };
    // Escrow is meaningful ONLY for a hub-assist org: refuse otherwise (the caller can already write, so a
    // 403 discloses nothing new).
    if !org_hub_assist(ctx, &meta).await {
        return Resp::err(403, "hub-assist escrow is not enabled for this session's org");
    }
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(kid) = v.get("kid").and_then(|k| k.as_i64()).filter(|k| *k >= 0) else {
        return Resp::err(400, "want a non-negative integer kid");
    };
    let Some(wrapped_ck) = str_field(&v, "wrapped_ck") else {
        return Resp::err(400, "want wrapped_ck");
    };
    if wrapped_ck.is_empty() || wrapped_ck.len() > KEK_WRAP_MAX {
        return Resp::err(400, "wrapped_ck is missing or too large");
    }
    let key = store::EscrowKey {
        owner: owner.to_string(),
        name: name.to_string(),
        kid,
        wrapped_ck,
        created: store::now_iso(),
    };
    if ctx.store.upsert_escrow_key(&key).await.is_err() {
        return Resp::err(500, "couldn't store the escrowed key");
    }
    let actor = caller.user.clone().unwrap_or_default();
    audit_append(ctx.root(), &actor, audit::KEYS_ESCROW, Some(&format!("{owner}/{name}")), &format!("kid={kid}")).await;
    Resp::json(serde_json::json!({ "owner": owner, "name": name, "kid": kid }))
}

/// `POST /api/agent/<owner>/<name>/keys/release` — release every escrowed content key this caller may read
/// (Wave 5, hub-assist escrow). Gated by the SAME `acl::decide(_, Read)` as git fetch (via `gate`), so it
/// NEVER releases a plaintext CK to a caller who cannot already fetch the ciphertext, and it is FAIL-CLOSED:
/// any denial is the gate's own 403/404 non-disclosing response. When the owning org is not in hub-assist
/// mode, it is a 404 (the escrow surface is not even disclosed). Returns `{ released: [{kid, ck}] }`.
async fn api_keys_release(ctx: &Ctx, caller: &Caller, owner: &str, name: &str) -> Resp {
    let meta = match gate(ctx, caller, owner, name, Action::Read).await {
        Ok(m) => m,
        // The exact fetch gate's fail-closed response (403 for a reader denied more, 404 non-disclosing).
        Err(resp) => return resp,
    };
    // Hub-assist off → the escrow surface does not exist for this session (non-disclosing 404).
    if !org_hub_assist(ctx, &meta).await {
        return Resp::err(404, "not found");
    }
    let rows = ctx.store.get_escrow_keys(owner, name).await;
    let mut released = Vec::new();
    for r in &rows {
        // Open with the hub escrow SECRET. A row that will not open is skipped, never a plaintext leak.
        if let Ok(ck) = agit::keybox::open_tk_envelope(&r.wrapped_ck, &ctx.escrow.secret) {
            released.push(serde_json::json!({ "kid": r.kid, "ck": hex::encode(ck) }));
        }
    }
    let actor = caller.user.clone().unwrap_or_default();
    audit_append(ctx.root(), &actor, audit::KEYS_RELEASE, Some(&format!("{owner}/{name}")), &format!("released={}", released.len())).await;
    Resp::json(serde_json::json!({ "released": released }))
}

/// `/api/orgs/<name>/members[/<username>]` — the org membership routes. Authorization here is an ORG
/// gate (`is_admin` on the org), NOT `acl::decide` — decide stays agent-only. Managing members needs
/// org-admin (or site admin); listing needs only membership.
///
/// Membership is **invitation-only**: `POST` here changes an EXISTING member's role but can NOT add a
/// stranger — a POST for a non-member is refused with guidance to invite instead. The only paths that
/// mint a NEW membership are invitation ACCEPT and org TRANSFER (to an already-existing member).
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
            if let Some(r) = deny_non_write_token(caller) {
                return r;
            }
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
            // Guard against orphaning the org: refuse demoting its last admin to a non-admin role.
            // Mirrors the DELETE path's last-admin guard, checked inside the same lock so it can't race
            // a concurrent demotion (POST could otherwise sneak past DELETE's guard by demoting instead
            // of removing).
            let outcome = ctx
                .store
                .update_orgs(|list| {
                    let Some(o) = list.iter_mut().find(|o| o.name == orgname) else {
                        return SetRoleOutcome::Ok;
                    };
                    if role != "admin" {
                        let admins = o.members.iter().filter(|m| m.role == "admin").count();
                        let target_is_admin = o.members.iter().any(|m| m.username == username && m.role == "admin");
                        if admins == 1 && target_is_admin {
                            return SetRoleOutcome::LastAdmin;
                        }
                    }
                    match o.members.iter_mut().find(|m| m.username == username) {
                        // Existing member → a role change, unchanged.
                        Some(m) => m.role = role.clone(),
                        // Membership is invitation-only: this endpoint no longer adds a stranger. The
                        // only ways in are invitation ACCEPT and org TRANSFER (to an existing member).
                        None => return SetRoleOutcome::NotMember,
                    }
                    SetRoleOutcome::Ok
                })
                .await;
            match outcome {
                Ok(SetRoleOutcome::Ok) => {}
                Ok(SetRoleOutcome::LastAdmin) => return Resp::err(409, "an org must keep at least one admin"),
                Ok(SetRoleOutcome::NotMember) => {
                    return Resp::err(
                        409,
                        "membership is invitation-only: add members with `agit-hub org invite <org> <user>` (POST /api/orgs/<org>/invitations), then have them accept",
                    );
                }
                Err(_e) => return Resp::err(500, "couldn't update the org"),
            }
            audit_append(ctx.root(), &user, audit::ORG_MEMBER_ADD, None, &format!("org={} {username}={role}", org.name)).await;
            let fresh = ctx.store.org(&org.name).await.unwrap_or(org);
            Resp::json(org_members_json(&fresh))
        }
        ("DELETE", Some(target_user)) => {
            if let Some(r) = deny_non_write_token(caller) {
                return r;
            }
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

/// The result of an org role change, so the last-admin guard (demoting the sole admin) and the
/// invitation-only guard (the target is not a member, so this is not a role change) can be told apart
/// from an ordinary in-place update after the atomic `update_orgs`.
enum SetRoleOutcome {
    Ok,
    LastAdmin,
    /// The target username is not already a member — refuse, since membership is invitation-only.
    NotMember,
}

// ── org invitations (the consent flow) ──

/// Serialize one invitation for the API. Never leaks anything the caller can't already see (org name,
/// their own username, the offered role).
fn invitation_json(i: &Invitation) -> serde_json::Value {
    serde_json::json!({
        "id": i.id,
        "org": i.org,
        "username": i.invitee,
        "role": i.role,
        "status": i.status,
        "created_by": i.created_by,
        "created": i.created,
    })
}

/// `GET /api/me/invitations` — the caller's own pending invitations. Self-scoped, so it needs only a
/// logged-in session (no org membership — the whole point is you are not a member yet).
pub(crate) async fn api_me_invitations(ctx: &Ctx, caller: &Caller) -> Resp {
    let Some(user) = caller.user.as_deref() else {
        return Resp::err(401, "login required");
    };
    let items: Vec<serde_json::Value> = ctx
        .store
        .invitations()
        .await
        .into_iter()
        .filter(|i| i.invitee == user && i.is_pending())
        .map(|i| invitation_json(&i))
        .collect();
    Resp::json(serde_json::json!(items))
}

/// The outcome of an invitee-driven state transition (accept/decline), so a missing/resolved
/// invitation, a wrong invitee, and a success can be told apart after the atomic `update_invitations`.
enum InviteOutcome {
    /// Accepted — carries the role to grant when minting the membership.
    Accepted(String),
    /// The invitee-driven transition succeeded without a role payload (decline).
    Ok,
    /// No pending invitation with that id under this org.
    NotFound,
    /// The invitation exists and is pending, but belongs to someone else.
    NotYours,
}

/// `/api/orgs/<org>/invitations[/<id>[/accept|/decline]]` — the invitation routes.
///
/// Two distinct authorization models share this entry point:
///   - **admin actions** (list / create / revoke) require the caller to be an org admin (or site
///     admin), and hide a missing/invisible org behind a uniform 404, exactly like `api_org_members`.
///   - **invitee actions** (accept / decline) are gated by the invitation itself: only the named
///     invitee may act, and the unguessable id is the handle — so these do NOT require org membership
///     (the invitee is not a member yet).
pub(crate) async fn api_org_invitations(ctx: &Ctx, caller: &Caller, name: &str, tail: &str, method: &str, body: &[u8]) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    let orgname = store::normalize_username(name);
    let sub = tail.strip_prefix('/');
    match (method, sub) {
        // ── admin actions on the collection ──
        ("GET", None) | ("POST", None) => {
            // 404 for a missing org OR one the caller can't see — existence non-disclosure, as in
            // api_org_members. Managing invitations then needs org-admin (a plain member gets 403).
            let Some(org) = ctx.store.org(&orgname).await else {
                return Resp::err(404, "not found");
            };
            if !(caller.is_admin || org.is_member(&user)) {
                return Resp::err(404, "not found");
            }
            if !(caller.is_admin || org.is_admin(&user)) {
                return Resp::err(403, "must be an org admin to manage invitations");
            }
            if method == "GET" {
                let items: Vec<serde_json::Value> = ctx
                    .store
                    .invitations()
                    .await
                    .into_iter()
                    .filter(|i| i.org == org.name && i.is_pending())
                    .map(|i| invitation_json(&i))
                    .collect();
                return Resp::json(serde_json::json!(items));
            }
            // POST = create an invitation: a write, so a non-write token is refused.
            if let Some(r) = deny_non_write_token(caller) {
                return r;
            }
            api_org_invite_create(ctx, &user, &org, body).await
        }
        // ── invitee / admin actions on a specific invitation ──
        (_, Some(rest)) => {
            let (id, action) = match rest.split_once('/') {
                Some((id, act)) => (id, Some(act)),
                None => (rest, None),
            };
            // accept / decline / revoke all MUTATE state (a write), so a non-write token is refused
            // before any of them run. A login session (token == None) passes as before.
            if let ("POST" | "DELETE", _) = (method, &action) {
                if let Some(r) = deny_non_write_token(caller) {
                    return r;
                }
            }
            match (method, action) {
                ("POST", Some("accept")) => api_org_invite_respond(ctx, &user, &orgname, id, true).await,
                ("POST", Some("decline")) => api_org_invite_respond(ctx, &user, &orgname, id, false).await,
                ("DELETE", None) => api_org_invite_revoke(ctx, caller, &user, &orgname, id).await,
                _ => Resp::text(405, "method not allowed"),
            }
        }
        _ => Resp::text(405, "method not allowed"),
    }
}

/// `POST /api/orgs/<org>/invitations` — an org admin invites a user with a target role. Creates a
/// PENDING invitation; the invitee alone can accept it. Caller is already known to be an org admin.
async fn api_org_invite_create(ctx: &Ctx, actor: &str, org: &Org, body: &[u8]) -> Resp {
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(username) = str_field(&v, "username") else {
        return Resp::err(400, "want username");
    };
    let username = store::normalize_username(&username);
    // Role defaults to "member" when omitted, mirroring the org member roles.
    let role = str_field(&v, "role").unwrap_or_else(|| "member".into());
    if role != "member" && role != "admin" {
        return Resp::err(400, "role must be member or admin");
    }
    // Only a real, existing user — the same rule member-add follows, so an org can't accumulate
    // invitations for misspelled names that whoever registers them later inherits.
    if ctx.store.user(&username).await.is_none() {
        return Resp::err(400, "no such user");
    }
    if org.is_member(&username) {
        return Resp::err(409, "that person is already a member");
    }
    let id = match store::new_invite_id() {
        Ok(id) => id,
        Err(_) => return Resp::err(500, "couldn't mint an invitation id"),
    };
    let invite = Invitation {
        id: id.clone(),
        org: org.name.clone(),
        invitee: username.clone(),
        role: role.clone(),
        status: "pending".into(),
        created_by: actor.to_string(),
        created: store::now_iso(),
    };
    // Atomic create: refuse a second pending invitation for the same (org, user) under the lock, so two
    // racing invites can't both land.
    let created = ctx
        .store
        .update_invitations(|list| {
            if list.iter().any(|i| i.org == invite.org && i.invitee == invite.invitee && i.is_pending()) {
                return false;
            }
            list.push(invite.clone());
            true
        })
        .await;
    match created {
        Ok(true) => {
            audit_append(ctx.root(), actor, audit::ORG_INVITE, None, &format!("org={} {username}={role} id={id}", org.name)).await;
            Resp::json_status(201, invitation_json(&invite))
        }
        Ok(false) => Resp::err(409, "that person already has a pending invitation"),
        Err(_) => Resp::err(500, "couldn't create the invitation"),
    }
}

/// Accept (`accept = true`) or decline an invitation. ONLY the named invitee may act. On accept the
/// membership is minted with the invited role; on decline nothing is granted. Both mark the invitation
/// resolved atomically, so it can't be replayed.
async fn api_org_invite_respond(ctx: &Ctx, user: &str, orgname: &str, id: &str, accept: bool) -> Resp {
    let next = if accept { "accepted" } else { "declined" };
    // Flip the invitation state under the lock, capturing the role for the membership mint. Matching on
    // id + org + pending inside the lock makes a double-accept impossible.
    let outcome = ctx
        .store
        .update_invitations(|list| {
            let Some(inv) = list.iter_mut().find(|i| i.id == id && i.org == orgname) else {
                return InviteOutcome::NotFound;
            };
            if !inv.is_pending() {
                return InviteOutcome::NotFound;
            }
            if inv.invitee != user {
                return InviteOutcome::NotYours;
            }
            inv.status = next.to_string();
            if accept {
                InviteOutcome::Accepted(inv.role.clone())
            } else {
                InviteOutcome::Ok
            }
        })
        .await;
    match outcome {
        Ok(InviteOutcome::Accepted(role)) => {
            // Mint the membership. Idempotent by username, and never downgrades an existing admin.
            let (orgname, user, role) = (orgname.to_string(), user.to_string(), role);
            let added = ctx
                .store
                .update_orgs(|list| {
                    let Some(o) = list.iter_mut().find(|o| o.name == orgname) else {
                        return false; // org vanished between accept and mint (e.g. deleted) — nothing to grant
                    };
                    match o.members.iter_mut().find(|m| m.username == user) {
                        Some(m) => {
                            if m.role != "admin" {
                                m.role = role.clone();
                            }
                        }
                        None => o.members.push(OrgMember { username: user.clone(), role: role.clone() }),
                    }
                    true
                })
                .await
                .unwrap_or(false);
            if !added {
                return Resp::err(404, "not found");
            }
            audit_append(ctx.root(), &user, audit::ORG_INVITE_ACCEPT, None, &format!("org={orgname} {user}={role} id={id}")).await;
            Resp::json(serde_json::json!({ "org": orgname, "username": user, "role": role, "status": "accepted" }))
        }
        Ok(InviteOutcome::Ok) => {
            audit_append(ctx.root(), user, audit::ORG_INVITE_DECLINE, None, &format!("org={orgname} {user} id={id}")).await;
            Resp::json(serde_json::json!({ "org": orgname, "username": user, "status": "declined" }))
        }
        // A pending invitation that exists but isn't yours: the id is an unguessable secret, so a clear
        // 403 is safe and more useful than hiding it.
        Ok(InviteOutcome::NotYours) => Resp::err(403, "that invitation isn't yours"),
        Ok(InviteOutcome::NotFound) => Resp::err(404, "not found"),
        Err(_) => Resp::err(500, "couldn't update the invitation"),
    }
}

/// `DELETE /api/orgs/<org>/invitations/<id>` — an org admin revokes a still-pending invitation. The
/// row is marked `revoked` (a durable record), which drops it out of every pending listing.
async fn api_org_invite_revoke(ctx: &Ctx, caller: &Caller, user: &str, orgname: &str, id: &str) -> Resp {
    let Some(org) = ctx.store.org(orgname).await else {
        return Resp::err(404, "not found");
    };
    if !(caller.is_admin || org.is_member(user)) {
        return Resp::err(404, "not found");
    }
    if !(caller.is_admin || org.is_admin(user)) {
        return Resp::err(403, "must be an org admin to revoke invitations");
    }
    let revoked = ctx
        .store
        .update_invitations(|list| match list.iter_mut().find(|i| i.id == id && i.org == org.name && i.is_pending()) {
            Some(inv) => {
                inv.status = "revoked".into();
                true
            }
            None => false,
        })
        .await
        .unwrap_or(false);
    if !revoked {
        return Resp::err(404, "no such pending invitation");
    }
    audit_append(ctx.root(), user, audit::ORG_INVITE_REVOKE, None, &format!("org={orgname} id={id}")).await;
    Resp::no_content()
}

// ── org transfer + delete ──

/// `POST /api/orgs/<org>/transfer { new_owner }` — hand ownership to another EXISTING MEMBER.
///
/// The org model has no single "owner" field: ownership is the admin role (the creator is the first
/// admin). So a transfer promotes `new_owner` to admin and steps the caller down to a plain member —
/// the honest reading of "hand ownership over". Because the new admin is set in the SAME update, the
/// org never passes through a state with no admin, so the last-admin guard is never tripped.
pub(crate) async fn api_org_transfer(ctx: &Ctx, caller: &Caller, name: &str, body: &[u8]) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    // Handing ownership over is management-grade: a login session only, no token (even a Write one).
    if let Some(r) = deny_any_token(caller) {
        return r;
    }
    // 404 for a missing org OR one the caller can't see — existence non-disclosure, as elsewhere.
    let Some(org) = ctx.store.org(name).await else {
        return Resp::err(404, "not found");
    };
    if !(caller.is_admin || org.is_member(&user)) {
        return Resp::err(404, "not found");
    }
    // Only the current owner (an org admin) may hand ownership over.
    if !(caller.is_admin || org.is_admin(&user)) {
        return Resp::err(403, "only an org admin may transfer ownership");
    }
    let Some(v) = json_body(body) else {
        return Resp::err(400, "want a JSON body");
    };
    let Some(new_owner) = str_field(&v, "new_owner") else {
        return Resp::err(400, "want new_owner");
    };
    let new_owner = store::normalize_username(&new_owner);
    // Reject an unknown account and a non-member alike — you can only hand the org to someone already
    // in it.
    if ctx.store.user(&new_owner).await.is_none() {
        return Resp::err(400, "no such user");
    }
    if !org.is_member(&new_owner) {
        return Resp::err(400, "the new owner must already be a member of the org");
    }
    if new_owner == user {
        return Resp::err(409, "you already own this org");
    }
    let orgname = org.name.clone();
    // Promote new_owner to admin and demote the caller to member, atomically. A site admin acting on an
    // org they don't belong to has no membership row to demote — that's fine, the promotion is the
    // load-bearing half.
    let done = ctx
        .store
        .update_orgs(|list| {
            let Some(o) = list.iter_mut().find(|o| o.name == orgname) else {
                return false;
            };
            match o.members.iter_mut().find(|m| m.username == new_owner) {
                Some(m) => m.role = "admin".into(),
                None => return false, // membership vanished mid-flight
            }
            if let Some(m) = o.members.iter_mut().find(|m| m.username == user) {
                m.role = "member".into();
            }
            true
        })
        .await
        .unwrap_or(false);
    if !done {
        return Resp::err(404, "not found");
    }
    audit_append(ctx.root(), &user, audit::ORG_TRANSFER, None, &format!("org={orgname} {user} → {new_owner}")).await;
    let fresh = ctx.store.org(&orgname).await;
    Resp::json(serde_json::json!({
        "name": orgname,
        "new_owner": new_owner,
        "previous_owner": user,
        "members": fresh.as_ref().map(org_members_json).unwrap_or_else(|| serde_json::json!([])),
    }))
}

/// `DELETE /api/orgs/<org>` — owner (an org admin) only. Refuses while the org still owns any agents,
/// so no repo/blob data is orphaned — the caller must transfer or delete those first. On success the
/// org row (and with it every membership) and every invitation for the org are swept.
pub(crate) async fn api_org_delete(ctx: &Ctx, caller: &Caller, name: &str) -> Resp {
    let Some(user) = caller.user.clone() else {
        return Resp::err(401, "login required");
    };
    // Deleting an org is management-grade: a login session only, no token (even a Write one).
    if let Some(r) = deny_any_token(caller) {
        return r;
    }
    let Some(org) = ctx.store.org(name).await else {
        return Resp::err(404, "not found");
    };
    if !(caller.is_admin || org.is_member(&user)) {
        return Resp::err(404, "not found");
    }
    if !(caller.is_admin || org.is_admin(&user)) {
        return Resp::err(403, "only an org admin may delete the org");
    }
    // Refuse-if-nonempty: an org that still owns agents cannot be deleted, or those repos and blobs are
    // orphaned. Any lifecycle counts (archived/soft-deleted repos still hold bytes on disk).
    let owns = ctx.store.agents().await.iter().any(|a| a.org_owner() == Some(org.name.as_str()));
    if owns {
        return Resp::err(409, "this org still owns agents — transfer or delete them first");
    }
    let orgname = org.name.clone();
    if ctx.store.update_orgs(|list| list.retain(|o| o.name != orgname)).await.is_err() {
        return Resp::err(500, "couldn't delete the org");
    }
    // Sweep pending (and resolved) invitations for the gone org so no dangling rows survive it.
    let _ = ctx.store.update_invitations(|list| list.retain(|i| i.org != orgname)).await;
    audit_append(ctx.root(), &user, audit::ORG_DELETE, None, &format!("org={orgname}")).await;
    Resp::no_content()
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

// ── code-repo index ──

/// How many sessions the cross-agent repo scan may walk before it stops and says so. The fan-out is
/// bounded the same way `api_search` bounds its content scan: over the cap, the scan halts and the
/// response's `capped` flag discloses the truncation rather than the counts silently going short.
pub(crate) const REPOS_SCAN_CAP: usize = 2000;

/// The most repos one response carries. Beyond this the newest-first list is truncated and `has_more`
/// says so - the oldest repos are the ones dropped.
pub(crate) const REPOS_MAX: usize = 500;

/// `GET /api/repos` - the code-repo index: one row per environment slug, aggregated across every agent
/// the caller may Read. There is no repo entity on the hub (an agent IS a bare git repo; the env <-> repo
/// link lives only inside session paths), so this fans out over `list_agents`, reads each readable
/// agent's session tree, and groups by env slug: summing sessions, deduping the agents attached to each
/// env, taking the newest activity, and reading ONE representative `cwd` per env from that env's newest
/// session (never every transcript).
///
/// Auth is open - anonymous is allowed - but "readable" is the whole gate: each agent runs through the
/// SAME `agent_acl` + `acl::decide(_, Read)` filter as `api_agents`, so a private agent the caller cannot
/// see contributes no sessions, no agent row, and no env. The scan is capped ([`REPOS_SCAN_CAP`]); the
/// `scanned`/`capped` fields disclose any truncation rather than the totals silently going short.
pub(crate) async fn api_repos(ctx: &Ctx, caller: &Caller) -> Resp {
    // Decide readability async (each per-agent ACL is a store read), then hand the subprocess-heavy
    // git fan-out to the blocking pool in one shot - exactly the shape of api_search.
    let mut readable: Vec<(String, String, String)> = Vec::new(); // (owner_display, name, seg)
    for (seg, name) in list_agents(ctx.root()) {
        let meta = ctx.store.agent_or_unowned(&seg, &name).await;
        if acl::decide(caller, &agent_acl(ctx, &meta).await, Action::Read).allowed() {
            let owner = meta.owner.clone().unwrap_or_else(|| seg.clone());
            readable.push((owner, name, seg));
        }
    }

    let root = ctx.root().to_path_buf();
    let (repos, scanned, capped, has_more): (Vec<serde_json::Value>, usize, bool, bool) = tokio::task::spawn_blocking(move || {
        // Per-env accumulator. `agents` is keyed by owner+name so one agent with many sessions in an
        // env is ONE row; `newest_*` track the winning session for the representative cwd (read once,
        // after grouping). `last_unix` is the sort/compare key; `last_rel` is what the UI shows.
        struct Agg {
            total_sessions: usize,
            last_unix: i64,
            last_rel: String,
            agents: Vec<(String, String, usize)>,
            newest_repo: std::path::PathBuf,
            newest_path: String,
        }
        let mut groups: std::collections::HashMap<String, Agg> = std::collections::HashMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        let mut capped = false;

        'agents: for (owner, name, seg) in &readable {
            let repo = repo_path(&root, seg, name);
            if !has_head(&repo) {
                continue;
            }
            // Sessions per env for THIS agent, plus the env's newest session file (one scoped git log),
            // built from the session tree we already have to walk.
            let refs = session_refs(&repo);
            let mut per_env: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for r in &refs {
                if scanned >= REPOS_SCAN_CAP {
                    capped = true;
                    break 'agents;
                }
                scanned += 1;
                // Old-layout sessions (env = None) carry no slug - they can't key a repo row.
                let Some(env) = &r.env else {
                    continue;
                };
                *per_env.entry(env.clone()).or_insert(0) += 1;
            }
            for (env, count) in per_env {
                // The env's newest commit under this agent: one `git log -1` scoped to the env dir gives
                // the absolute time (%ct, for cross-agent comparison), a relative string (%cr, for the
                // UI), and the file(s) it touched - the first .jsonl is a representative newest session.
                let envdir = format!("sessions/{env}");
                let (unix, rel, newest_path) = git(&repo, &["log", "-1", "--format=%ct\x1f%cr", "--name-only", "--", &format!(":(literal){envdir}")])
                    .map(|out| {
                        let mut lines = out.lines();
                        let head = lines.next().unwrap_or("");
                        let (ct, cr) = head.split_once('\x1f').unwrap_or(("0", ""));
                        let unix = ct.trim().parse::<i64>().unwrap_or(0);
                        let file = lines
                            .find(|l| l.starts_with(&format!("{envdir}/")) && l.ends_with(".jsonl"))
                            .unwrap_or("")
                            .to_string();
                        (unix, cr.trim().to_string(), file)
                    })
                    .unwrap_or((0, String::new(), String::new()));

                if !groups.contains_key(&env) {
                    order.push(env.clone());
                }
                let g = groups.entry(env.clone()).or_insert_with(|| Agg {
                    total_sessions: 0,
                    last_unix: i64::MIN,
                    last_rel: String::new(),
                    agents: Vec::new(),
                    newest_repo: repo.clone(),
                    newest_path: String::new(),
                });
                g.total_sessions += count;
                g.agents.push((owner.clone(), name.clone(), count));
                // Newest across agents wins the env's `last` AND its representative session.
                if unix > g.last_unix {
                    g.last_unix = unix;
                    g.last_rel = rel;
                    g.newest_repo = repo.clone();
                    g.newest_path = newest_path;
                }
            }
        }

        // Sort envs by activity, newest first; truncate to REPOS_MAX (dropping the oldest) and report it.
        order.sort_by(|a, b| groups[b].last_unix.cmp(&groups[a].last_unix));
        let has_more = order.len() > REPOS_MAX;
        order.truncate(REPOS_MAX);

        let repos: Vec<serde_json::Value> = order
            .into_iter()
            .map(|env| {
                let g = groups.remove(&env).expect("env in order is in groups");
                // ONE transcript read per env: the newest session's cwd, the human-readable path the
                // lossy slug can't give. The runtime is the 3rd path segment
                // (`sessions/<env>/<runtime>/<id>.jsonl`), so the right adapter parses it. Absent (no
                // session / no cwd / parse miss) → null.
                let runtime = g.newest_path.split('/').nth(2).unwrap_or("").to_string();
                let cwd = load_session(&g.newest_repo, &g.newest_path, None)
                    .map(|jsonl| digest(&runtime, &g.newest_path, &jsonl).cwd)
                    .filter(|c| !c.is_empty());
                let agents: Vec<serde_json::Value> = g
                    .agents
                    .iter()
                    .map(|(owner, name, sessions)| serde_json::json!({ "owner": owner, "name": name, "sessions": sessions }))
                    .collect();
                serde_json::json!({
                    "env": env,
                    "cwd": cwd,
                    "total_sessions": g.total_sessions,
                    "last": g.last_rel,
                    "agents": agents,
                })
            })
            .collect();

        (repos, scanned, capped, has_more)
    })
    .await
    .unwrap();

    Resp::json(serde_json::json!({
        "repos": repos,
        "has_more": has_more,
        "scanned": scanned,
        "capped": capped,
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
            // Bind at the action the token will actually take: a WRITE token must be REFUSED now if
            // the caller can only READ the target (e.g. a public agent they don't own), instead of
            // minting a token that 403s at the first push. A READ token still only needs Read.
            let bind_action = match scope {
                Scope::Write => Action::Write,
                Scope::Read => Action::Read,
            };
            let meta = match gate(ctx, caller, a_owner, a_name, bind_action).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Req;
    use crate::limits::{ConnLimiter, TokenBuckets, LOGIN_CONC, REGISTER_BURST, REGISTER_RATE_PER_SEC};
    use crate::server::{Cfg, Ctx, CtxInner};
    use agit::hub::acl::{Caller, Visibility};
    use agit::hub::blob::Blobs;
    use agit::hub::metrics::Metrics;
    use agit::hub::session::Sessions;
    use agit::hub::store::{Store, User};
    use std::net::IpAddr;
    use std::sync::Arc;

    /// An in-process Ctx over a fresh SQLite store + fs blobs, enough to drive the handlers directly.
    async fn harness() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_sqlite(dir.path()).await.unwrap();
        let blobs = Blobs::open(dir.path()).await.unwrap();
        let cfg = Cfg { host: IpAddr::from([127, 0, 0, 1]), port: 8177, tls: false, insecure: false, trusted_proxies: vec![], registration: false };
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
            escrow: {
                // A deterministic-enough test escrow keypair: a fixed secret is fine here (the tests
                // seal to `escrow.public` and open with `escrow.secret`, so only self-consistency matters).
                let secret = [7u8; 32];
                let public = agit::agent::x25519_public_from_secret(&secret);
                crate::server::EscrowKeypair { secret, public }
            },
        }));
        (dir, ctx)
    }

    async fn add_user(ctx: &Ctx, name: &str, pw: &str, admin: bool) {
        let salt = kdf::gen_salt().unwrap();
        let kdf_id = kdf::current_kdf_id();
        let pw_hash = kdf::hash_password(pw, &salt, &kdf_id).unwrap();
        ctx.store
            .add_user(User { username: name.into(), pw_hash, salt, kdf: kdf_id, is_admin: admin, created: store::now_iso(), ..Default::default() })
            .await
            .unwrap();
    }

    async fn create_agent_with_aid(ctx: &Ctx, owner: &str, name: &str, vis: Visibility, aid: &str) {
        crate::cli::create_agent(&ctx.store, name, owner, vis).await.unwrap();
        ctx.store
            .update_agents(|list| {
                if let Some(m) = list.iter_mut().find(|m| m.matches(owner, name)) {
                    m.aid = Some(aid.into());
                }
            })
            .await
            .unwrap();
    }

    fn caller(user: &str, admin: bool) -> Caller {
        Caller { user: Some(user.into()), is_admin: admin, token: None }
    }

    fn req(method: &str, sid: Option<&str>) -> Req {
        let mut headers = vec![("host".to_string(), "localhost:8177".to_string())];
        if let Some(s) = sid {
            headers.push(("cookie".to_string(), format!("agit_session={s}")));
        }
        Req { method: method.to_string(), target: "/".to_string(), headers, content_length: 0 }
    }

    fn body(v: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    // ── self-service password change ──

    #[tokio::test]
    async fn self_password_change_succeeds_and_rotates_the_login() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "old-password-1", false).await;
        let resp = api_me_password(
            &ctx,
            &req("POST", None),
            &caller("alice", false),
            &body(serde_json::json!({ "old_password": "old-password-1", "new_password": "new-password-2" })),
        )
        .await;
        assert_eq!(resp.status, 200);
        // The new password logs in; the old one no longer does.
        assert!(auth::verify_login(&ctx.store, "alice", "new-password-2").await.is_some(), "new password works");
        assert!(auth::verify_login(&ctx.store, "alice", "old-password-1").await.is_none(), "old password is dead");
    }

    #[tokio::test]
    async fn self_password_change_wrong_old_is_401() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "old-password-1", false).await;
        let resp = api_me_password(
            &ctx,
            &req("POST", None),
            &caller("alice", false),
            &body(serde_json::json!({ "old_password": "not-the-password", "new_password": "new-password-2" })),
        )
        .await;
        assert_eq!(resp.status, 401);
        // Nothing changed: the old password still works, the attempted new one does not.
        assert!(auth::verify_login(&ctx.store, "alice", "old-password-1").await.is_some());
        assert!(auth::verify_login(&ctx.store, "alice", "new-password-2").await.is_none());
    }

    #[tokio::test]
    async fn self_password_change_short_new_is_400() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "old-password-1", false).await;
        let resp = api_me_password(
            &ctx,
            &req("POST", None),
            &caller("alice", false),
            // Correct old password, so we reach the length check; "short" is under MIN_PASSWORD_LEN.
            &body(serde_json::json!({ "old_password": "old-password-1", "new_password": "short" })),
        )
        .await;
        assert_eq!(resp.status, 400);
        assert!(auth::verify_login(&ctx.store, "alice", "old-password-1").await.is_some(), "unchanged");
    }

    #[tokio::test]
    async fn self_password_change_requires_a_logged_in_user() {
        let (_d, ctx) = harness().await;
        let resp = api_me_password(
            &ctx,
            &req("POST", None),
            &Caller::anonymous(),
            &body(serde_json::json!({ "old_password": "x", "new_password": "new-password-2" })),
        )
        .await;
        assert_eq!(resp.status, 401);
    }

    // ── self-service password reset (operator-forwarded token) ──

    /// The anti-enumeration property: a reset request for a real account and for an unknown one return
    /// BYTE-IDENTICAL responses (same status, same body), and only the real account mints a token.
    #[tokio::test]
    async fn password_reset_request_is_anti_enumeration() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;

        let exists = api_password_reset_request(&ctx, None, &body(serde_json::json!({ "username": "alice" }))).await;
        // The existing account minted exactly one consumable token (the link is delivered out-of-band; the
        // response never carries it).
        assert_eq!(exists.status, 200);
        assert!(!String::from_utf8_lossy(&exists.body).contains("prt_"), "the token is never returned in the response");
        assert_eq!(ctx.store.password_reset_token_count().await, 1, "an existing account mints a reset token");

        let ghost = api_password_reset_request(&ctx, None, &body(serde_json::json!({ "username": "nobody" }))).await;
        // Same 200, and — critically — a BYTE-IDENTICAL body, so the answer is not an existence oracle.
        assert_eq!(ghost.status, exists.status, "status is identical for existing and unknown accounts");
        assert_eq!(ghost.body, exists.body, "body is byte-identical (anti-enumeration)");
        assert_eq!(ctx.store.password_reset_token_count().await, 1, "an unknown account mints NOTHING");
    }

    /// A valid token sets the new password (login flips old→new), invalidates ALL sessions, is single-use,
    /// and — the separation invariant — an email-verify token can NOT be spent here.
    #[tokio::test]
    async fn password_reset_consume_sets_password_and_kills_sessions() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "old-password-1", false).await;
        // A live session that a reset must kick.
        let sid = ctx.sessions.create("alice").unwrap();
        assert_eq!(ctx.sessions.lookup(&sid).as_deref(), Some("alice"));

        // Mint the token the operator would forward (stands in for the delivered link).
        let token = ctx.store.mint_password_reset_token("alice", std::time::Duration::from_secs(3600)).await.unwrap();

        // A too-short new password is a 400 and does NOT burn the token.
        let short = api_password_reset_consume(&ctx, &body(serde_json::json!({ "token": token.clone(), "new_password": "short" }))).await;
        assert_eq!(short.status, 400);

        // The valid consume sets the new password with NO old password supplied.
        let ok = api_password_reset_consume(&ctx, &body(serde_json::json!({ "token": token.clone(), "new_password": "new-password-2" }))).await;
        assert_eq!(ok.status, 200);
        assert!(auth::verify_login(&ctx.store, "alice", "new-password-2").await.is_some(), "the new password works");
        assert!(auth::verify_login(&ctx.store, "alice", "old-password-1").await.is_none(), "the old password is dead");
        // Every session is revoked — a reset is a recovery action.
        assert!(ctx.sessions.lookup(&sid).is_none(), "the reset invalidates existing sessions");

        // Single-use: replaying the same token is a 400.
        assert_eq!(api_password_reset_consume(&ctx, &body(serde_json::json!({ "token": token, "new_password": "another-pass-3" }))).await.status, 400);
    }

    /// Cross-space rejection AT THE API LAYER: an email-verification token is not a reset token (and an
    /// expired reset token is refused), so leaking a verify link can never reset a password.
    #[tokio::test]
    async fn password_reset_consume_rejects_verify_tokens_and_expired() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "old-password-1", false).await;

        // An email-verify token (evt_) can NOT be consumed as a reset token.
        let evt = ctx.store.mint_email_token("alice", "alice@corp.com", std::time::Duration::from_secs(3600)).await.unwrap();
        let cross = api_password_reset_consume(&ctx, &body(serde_json::json!({ "token": evt, "new_password": "new-password-2" }))).await;
        assert_eq!(cross.status, 400, "an email-verify token is not a reset token");
        assert!(auth::verify_login(&ctx.store, "alice", "old-password-1").await.is_some(), "the password did not change");

        // A reset token (prt_) can NOT be consumed as an email-verify token.
        let prt = ctx.store.mint_password_reset_token("alice", std::time::Duration::from_secs(3600)).await.unwrap();
        assert_eq!(api_verify_email(&ctx, &body(serde_json::json!({ "token": prt }))).await.status, 400, "a reset token is not a verify token");

        // An already-expired reset token (ttl 0) is refused.
        let expired = ctx.store.mint_password_reset_token("alice", std::time::Duration::from_secs(0)).await.unwrap();
        assert_eq!(api_password_reset_consume(&ctx, &body(serde_json::json!({ "token": expired, "new_password": "new-password-2" }))).await.status, 400);
        assert!(auth::verify_login(&ctx.store, "alice", "old-password-1").await.is_some(), "still unchanged");
    }

    /// The request endpoint is rate-limited per-IP on the same budget as registration.
    #[tokio::test]
    async fn password_reset_request_is_rate_limited_per_ip() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let flooder: IpAddr = "203.0.113.9".parse().unwrap();
        let mut statuses = Vec::new();
        for _ in 0..(crate::limits::REGISTER_BURST as usize + 4) {
            statuses.push(api_password_reset_request(&ctx, Some(flooder), &body(serde_json::json!({ "username": "alice" }))).await.status);
        }
        assert_eq!(statuses[0], 200, "the first request from an IP is allowed");
        assert!(statuses.contains(&429), "a sustained sweep from one IP is throttled (429)");
        assert_eq!(*statuses.last().unwrap(), 429, "once the bucket drains, the throttle sticks");
        // A different IP keeps its own bucket.
        let other: IpAddr = "203.0.113.10".parse().unwrap();
        assert_eq!(api_password_reset_request(&ctx, Some(other), &body(serde_json::json!({ "username": "alice" }))).await.status, 200);
        // A missing client IP fails open, exactly like registration.
        assert_eq!(api_password_reset_request(&ctx, None, &body(serde_json::json!({ "username": "alice" }))).await.status, 200);
    }

    // ── two-factor authentication (TOTP) ──

    /// Parse a Resp's JSON body (tests only).
    fn json_of(resp: &Resp) -> serde_json::Value {
        serde_json::from_slice(&resp.body).expect("response body is JSON")
    }

    /// Drive enroll → confirm to leave `name` with ACTIVE 2FA. Returns (secret, backup_codes).
    async fn activate_2fa(ctx: &Ctx, name: &str) -> (String, Vec<String>) {
        let enroll = api_2fa_enroll(ctx, &caller(name, false)).await;
        assert_eq!(enroll.status, 200, "enroll ok");
        let secret = json_of(&enroll)["secret"].as_str().unwrap().to_string();
        let code = totp::current_code(&secret, name).unwrap();
        let confirm = api_2fa_confirm(ctx, &caller(name, false), &body(serde_json::json!({ "code": code }))).await;
        assert_eq!(confirm.status, 200, "confirm ok");
        let codes: Vec<String> = json_of(&confirm)["backup_codes"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
        (secret, codes)
    }

    #[tokio::test]
    async fn enroll_is_pending_not_active_and_returns_a_provisioning_uri() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let resp = api_2fa_enroll(&ctx, &caller("alice", false)).await;
        assert_eq!(resp.status, 200);
        let v = json_of(&resp);
        assert!(v["secret"].as_str().is_some(), "secret returned once at enroll");
        let uri = v["otpauth_uri"].as_str().unwrap();
        assert!(uri.starts_with("otpauth://totp/agit-hub:alice"), "{uri}");
        assert!(uri.contains("issuer=agit-hub"), "{uri}");
        // Pending, NOT active: a secret is stored but 2FA is off, so login is not yet gated.
        let u = ctx.store.user("alice").await.unwrap();
        assert!(u.totp_secret.is_some(), "pending secret stored");
        assert!(!u.totp_enabled, "enroll must NOT activate 2FA");
        assert!(u.totp_backup_codes.is_empty(), "no backup codes until confirm");
    }

    #[tokio::test]
    async fn confirm_with_a_valid_code_activates_and_returns_backup_codes() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let (_secret, codes) = activate_2fa(&ctx, "alice").await;
        assert_eq!(codes.len(), totp::BACKUP_CODES, "10 backup codes returned once");
        let u = ctx.store.user("alice").await.unwrap();
        assert!(u.totp_enabled, "2FA is active after a confirmed code");
        assert_eq!(u.totp_backup_codes.len(), totp::BACKUP_CODES);
        // Only DIGESTS are stored — no backup-code plaintext lives in the record.
        for plain in &codes {
            assert!(!u.totp_backup_codes.contains(plain), "backup-code plaintext must never be stored");
            assert!(u.totp_backup_codes.contains(&totp::hash_backup_code(plain)), "the digest is what's stored");
        }
    }

    #[tokio::test]
    async fn confirm_with_a_wrong_code_is_401_and_stays_pending() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        api_2fa_enroll(&ctx, &caller("alice", false)).await;
        let resp = api_2fa_confirm(&ctx, &caller("alice", false), &body(serde_json::json!({ "code": "000000" }))).await;
        // A wrong code is a 4xx and 2FA stays inactive (fail closed).
        assert!(resp.status == 401 || resp.status == 400, "got {}", resp.status);
        assert!(!ctx.store.user("alice").await.unwrap().totp_enabled);
    }

    #[tokio::test]
    async fn login_with_password_alone_when_2fa_on_is_401_2fa_required() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        activate_2fa(&ctx, "alice").await;
        // Correct password, no code: must be refused with the non-enumerating signal and NO session.
        let resp = api_login(&ctx, &req("POST", None), &body(serde_json::json!({ "username": "alice", "password": "alice-password-1" }))).await;
        assert_eq!(resp.status, 401, "password alone is not enough once 2FA is on");
        assert_eq!(json_of(&resp)["error"].as_str(), Some("2fa_required"));
        assert!(!resp.extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("set-cookie")), "no session cookie is handed out");
    }

    #[tokio::test]
    async fn login_with_wrong_code_when_2fa_on_is_401_2fa_required() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        activate_2fa(&ctx, "alice").await;
        let resp = api_login(&ctx, &req("POST", None), &body(serde_json::json!({ "username": "alice", "password": "alice-password-1", "code": "000000" }))).await;
        assert_eq!(resp.status, 401);
        assert_eq!(json_of(&resp)["error"].as_str(), Some("2fa_required"));
    }

    #[tokio::test]
    async fn login_with_password_and_valid_totp_is_a_200_session() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let (secret, _codes) = activate_2fa(&ctx, "alice").await;
        let code = totp::current_code(&secret, "alice").unwrap();
        let resp = api_login(&ctx, &req("POST", None), &body(serde_json::json!({ "username": "alice", "password": "alice-password-1", "code": code }))).await;
        assert_eq!(resp.status, 200, "password + valid TOTP logs in");
        assert!(resp.extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("set-cookie")), "a session cookie is set");
    }

    #[tokio::test]
    async fn a_backup_code_logs_in_once_then_is_rejected() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let (_secret, codes) = activate_2fa(&ctx, "alice").await;
        let code = codes[0].clone();
        // First use: succeeds.
        let ok = api_login(&ctx, &req("POST", None), &body(serde_json::json!({ "username": "alice", "password": "alice-password-1", "code": code }))).await;
        assert_eq!(ok.status, 200, "a backup code works once");
        // It is now marked used (one consumed).
        assert_eq!(ctx.store.user("alice").await.unwrap().totp_backup_codes.len(), totp::BACKUP_CODES - 1);
        // Second use of the SAME code: rejected.
        let again = api_login(&ctx, &req("POST", None), &body(serde_json::json!({ "username": "alice", "password": "alice-password-1", "code": codes[0] }))).await;
        assert_eq!(again.status, 401, "a spent backup code no longer logs in");
        assert_eq!(json_of(&again)["error"].as_str(), Some("2fa_required"));
    }

    #[tokio::test]
    async fn disable_with_a_valid_totp_turns_2fa_off() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let (secret, _codes) = activate_2fa(&ctx, "alice").await;
        let code = totp::current_code(&secret, "alice").unwrap();
        let resp = api_2fa_disable(&ctx, &caller("alice", false), &body(serde_json::json!({ "code_or_password": code }))).await;
        assert_eq!(resp.status, 200);
        let u = ctx.store.user("alice").await.unwrap();
        assert!(!u.totp_enabled && u.totp_secret.is_none() && u.totp_backup_codes.is_empty(), "2FA fully cleared");
        // And now a plain password login works again.
        let login = api_login(&ctx, &req("POST", None), &body(serde_json::json!({ "username": "alice", "password": "alice-password-1" }))).await;
        assert_eq!(login.status, 200);
    }

    #[tokio::test]
    async fn disable_with_the_password_also_works() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        activate_2fa(&ctx, "alice").await;
        let resp = api_2fa_disable(&ctx, &caller("alice", false), &body(serde_json::json!({ "code_or_password": "alice-password-1" }))).await;
        assert_eq!(resp.status, 200);
        assert!(!ctx.store.user("alice").await.unwrap().totp_enabled);
    }

    #[tokio::test]
    async fn disable_with_a_wrong_proof_is_401_and_2fa_stays_on() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        activate_2fa(&ctx, "alice").await;
        let resp = api_2fa_disable(&ctx, &caller("alice", false), &body(serde_json::json!({ "code_or_password": "000000" }))).await;
        assert_eq!(resp.status, 401);
        assert!(ctx.store.user("alice").await.unwrap().totp_enabled, "2FA must survive a failed disable");
    }

    #[tokio::test]
    async fn admin_2fa_disable_clears_a_locked_out_users_2fa() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "root", "root-password-1", true).await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        activate_2fa(&ctx, "alice").await;
        // A non-admin cannot use the recovery door.
        let denied = api_admin_2fa_disable(&ctx, &caller("alice", false), "alice").await;
        assert_eq!(denied.status, 403);
        // The admin clears it.
        let resp = api_admin_2fa_disable(&ctx, &caller("root", true), "alice").await;
        assert_eq!(resp.status, 200);
        assert!(!ctx.store.user("alice").await.unwrap().totp_enabled, "admin recovery cleared 2FA");
        // Alice can now log in with just her password.
        let login = api_login(&ctx, &req("POST", None), &body(serde_json::json!({ "username": "alice", "password": "alice-password-1" }))).await;
        assert_eq!(login.status, 200);
    }

    #[tokio::test]
    async fn secret_and_backup_plaintext_never_appear_after_enroll_confirm() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let (secret, codes) = activate_2fa(&ctx, "alice").await;
        // /api/me never carries secret material.
        let me = json_of(&api_me(&ctx, &caller("alice", false)).await);
        let me_s = serde_json::to_string(&me).unwrap();
        assert!(!me_s.contains(&secret), "/api/me must not leak the TOTP secret");
        for c in &codes {
            assert!(!me_s.contains(c.as_str()), "/api/me must not leak a backup code");
        }
        // A subsequent login response carries no secret material either.
        let code = totp::current_code(&secret, "alice").unwrap();
        let login = api_login(&ctx, &req("POST", None), &body(serde_json::json!({ "username": "alice", "password": "alice-password-1", "code": code }))).await;
        let login_s = String::from_utf8_lossy(&login.body);
        assert!(!login_s.contains(&secret), "login response must not leak the TOTP secret");
        for c in &codes {
            assert!(!login_s.contains(c.as_str()), "login response must not leak a backup code");
        }
        // Re-enroll is refused while active (no fresh secret is minted behind the user's back).
        let reenroll = api_2fa_enroll(&ctx, &caller("alice", false)).await;
        assert_eq!(reenroll.status, 409, "cannot re-enroll while 2FA is active");
    }

    #[tokio::test]
    async fn self_password_change_revokes_other_sessions_but_keeps_current() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "old-password-1", false).await;
        let current = ctx.sessions.create("alice").unwrap();
        let other = ctx.sessions.create("alice").unwrap();
        let resp = api_me_password(
            &ctx,
            &req("POST", Some(&current)),
            &caller("alice", false),
            &body(serde_json::json!({ "old_password": "old-password-1", "new_password": "new-password-2" })),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(ctx.sessions.lookup(&current).as_deref(), Some("alice"), "the session that changed the password stays alive");
        assert_eq!(ctx.sessions.lookup(&other), None, "every other session for the account is kicked");
    }

    // ── admin-mediated reset ──

    #[tokio::test]
    async fn admin_reset_lets_the_user_log_in_and_refuses_non_admins() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "root", "root-password-1", true).await;
        add_user(&ctx, "bob", "bob-old-pass-1", false).await;

        // A non-admin caller cannot reset anyone's password.
        let denied = api_admin_set_password(&ctx, &caller("bob", false), "bob", &body(serde_json::json!({ "new_password": "hijacked-pass-1" }))).await;
        assert_eq!(denied.status, 403);
        // An anonymous caller is refused before anything else.
        let anon = api_admin_set_password(&ctx, &Caller::anonymous(), "bob", &body(serde_json::json!({ "new_password": "hijacked-pass-1" }))).await;
        assert_eq!(anon.status, 401);
        // Neither attempt touched the password.
        assert!(auth::verify_login(&ctx.store, "bob", "bob-old-pass-1").await.is_some());

        // The admin resets it; bob can now log in with the new password, not the old one.
        let ok = api_admin_set_password(&ctx, &caller("root", true), "bob", &body(serde_json::json!({ "new_password": "bob-new-pass-2" }))).await;
        assert_eq!(ok.status, 200);
        assert!(auth::verify_login(&ctx.store, "bob", "bob-new-pass-2").await.is_some(), "new password works");
        assert!(auth::verify_login(&ctx.store, "bob", "bob-old-pass-1").await.is_none(), "old password is dead");

        // A too-short reset is refused (the same shared minimum).
        let short = api_admin_set_password(&ctx, &caller("root", true), "bob", &body(serde_json::json!({ "new_password": "short" }))).await;
        assert_eq!(short.status, 400);
        // An unknown user is a plain 404 (an admin already sees every account).
        let missing = api_admin_set_password(&ctx, &caller("root", true), "ghost", &body(serde_json::json!({ "new_password": "whatever-pass-1" }))).await;
        assert_eq!(missing.status, 404);
    }

    // ── by-aid non-disclosure ──

    #[tokio::test]
    async fn by_aid_is_not_an_existence_oracle() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        add_user(&ctx, "bob", "bob-password-1", false).await;
        create_agent_with_aid(&ctx, "alice", "secret", Visibility::Private, "agt_secret1").await;

        // An anonymous caller must not tell a known-but-private aid from an unknown one: same status,
        // byte-identical body.
        let anon = Caller::anonymous();
        let priv_hit = api_agent_by_aid(&ctx, &req("GET", None), &anon, "agt_secret1").await;
        let unknown = api_agent_by_aid(&ctx, &req("GET", None), &anon, "agt_nosuchaid").await;
        assert_eq!(priv_hit.status, 404, "a private agent's aid does not resolve for anon");
        assert_eq!(unknown.status, 404, "an unknown aid is 404");
        assert_eq!(priv_hit.body, unknown.body, "the two are byte-identical — no existence oracle");

        // An authenticated non-owner is likewise given the same 404 for private vs unknown.
        let as_bob = caller("bob", false);
        let bob_priv = api_agent_by_aid(&ctx, &req("GET", None), &as_bob, "agt_secret1").await;
        let bob_unknown = api_agent_by_aid(&ctx, &req("GET", None), &as_bob, "agt_nosuchaid").await;
        assert_eq!(bob_priv.status, 404);
        assert_eq!(bob_priv.body, bob_unknown.body);

        // No observable SIDE-EFFECT either: a private-aid probe must write NO audit-deny (an unknown aid
        // writes none, so an audit entry for a private hit would itself be an existence oracle, distinct
        // from the identical 404). By-aid decides readability silently, never through the auditing gate.
        let trail = agit::hub::audit::query(ctx.root(), Some("alice/secret"), 100);
        assert!(trail.is_empty(), "a by-aid probe of a private agent must leave no audit trail: {} entries", trail.len());

        // The owner still resolves their own private agent by aid.
        let owner = api_agent_by_aid(&ctx, &req("GET", None), &caller("alice", false), "agt_secret1").await;
        assert_eq!(owner.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&owner.body).unwrap();
        assert_eq!(v["name"], "secret");
        assert_eq!(v["aid"], "agt_secret1");
        assert_eq!(v["visibility"], "private");
    }

    // ── shared identity registry (encryption-recipients Wave 1) ──

    /// Build a VALID signed enroll body for a fresh identity minted under `home`, signing the tuple as
    /// `username`. Returns the body plus the derived (ed25519_pub, x25519_pub) hex for assertions.
    fn signed_identity(home: &std::path::Path, username: &str, epoch: i64) -> (serde_json::Value, String, String) {
        let sk = agit::agent::load_or_create_signing_key(home).unwrap();
        let ed = hex::encode(sk.verifying_key().to_bytes());
        let x = hex::encode(agit::agent::x25519_public_from_secret(&agit::agent::derive_x25519_secret(&sk)));
        let msg = agit::agent::identity_enroll_message(username, epoch, &ed, &x);
        let sig = agit::agent::sign_hex(&sk, &msg);
        (serde_json::json!({ "ed25519_pub": ed, "x25519_pub": x, "epoch": epoch, "enroll_sig": sig }), ed, x)
    }

    fn get_req(target: &str) -> Req {
        Req { method: "GET".into(), target: target.into(), headers: vec![("host".into(), "localhost:8177".into())], content_length: 0 }
    }

    /// The happy path: enroll writes the CALLER's own row, and GET returns those exact pubkeys.
    #[tokio::test]
    async fn identity_enroll_stores_the_callers_row_and_get_returns_it() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "pw-alice-1234", false).await;
        let home = tempfile::tempdir().unwrap();
        let (b, ed, x) = signed_identity(home.path(), "alice", 0);

        let resp = api_identity_enroll(&ctx, &caller("alice", false), &body(b)).await;
        assert_eq!(resp.status, 200, "enroll should succeed: {}", String::from_utf8_lossy(&resp.body));

        // Stored under the caller's username, with the submitted public halves.
        let row = ctx.store.get_identity_key("alice").await.expect("alice is enrolled");
        assert_eq!(row.ed25519_pub, ed);
        assert_eq!(row.x25519_pub, x);
        assert_eq!(row.epoch, 0);

        // GET /api/identity/<user> returns the enrolled pubkeys to any authenticated caller.
        let got = api_identity_get(&ctx, &caller("bob", false), "alice").await;
        assert_eq!(got.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&got.body).unwrap();
        assert_eq!(v["ed25519_pub"], ed);
        assert_eq!(v["x25519_pub"], x);
        assert_eq!(v["epoch"], 0);
        assert_eq!(v["username"], "alice");
    }

    /// The anti-mint property: a signature that does not verify against the SUBMITTED ed25519_pub is
    /// refused, and nothing is stored — the hub can only replace a row, never forge one.
    #[tokio::test]
    async fn identity_enroll_rejects_a_signature_that_does_not_verify() {
        let (_d, ctx) = harness().await;
        let home = tempfile::tempdir().unwrap();
        let (mut b, _ed, _x) = signed_identity(home.path(), "alice", 0);
        b["enroll_sig"] = serde_json::json!("00".repeat(64)); // well-formed length, but not a real signature
        let resp = api_identity_enroll(&ctx, &caller("alice", false), &body(b)).await;
        assert_eq!(resp.status, 400, "a non-verifying enroll_sig must be refused");
        assert!(ctx.store.get_identity_key("alice").await.is_none(), "nothing is stored on rejection");
    }

    /// The hub cannot bind a submitted pubkey it does not hold the private key for: a signature made by a
    /// DIFFERENT key than the submitted ed25519_pub does not verify, so no valid row can be minted.
    #[tokio::test]
    async fn identity_enroll_cannot_be_minted_with_a_foreign_signature() {
        let (_d, ctx) = harness().await;
        let ha = tempfile::tempdir().unwrap();
        let hb = tempfile::tempdir().unwrap();
        let (b_a, ed_a, x_a) = signed_identity(ha.path(), "alice", 0);
        // Sign alice's exact tuple with a foreign key (hb). It is a real signature — just not by the key
        // whose public half is submitted — so verification against ed_a fails.
        let skb = agit::agent::load_or_create_signing_key(hb.path()).unwrap();
        let foreign_sig = agit::agent::sign_hex(&skb, &agit::agent::identity_enroll_message("alice", 0, &ed_a, &x_a));
        let mut forged = b_a;
        forged["enroll_sig"] = serde_json::json!(foreign_sig);
        let resp = api_identity_enroll(&ctx, &caller("alice", false), &body(forged)).await;
        assert_eq!(resp.status, 400, "a signature not made by the submitted key must be refused");
        assert!(ctx.store.get_identity_key("alice").await.is_none());
    }

    /// The epoch is monotonic: an epoch equal to or below the stored one is refused (no rollback), and
    /// the stored row is untouched; only a strictly higher epoch replaces it.
    #[tokio::test]
    async fn identity_enroll_rejects_a_non_advancing_epoch() {
        let (_d, ctx) = harness().await;
        let home = tempfile::tempdir().unwrap();

        let (b5, ed5, _x) = signed_identity(home.path(), "alice", 5);
        assert_eq!(api_identity_enroll(&ctx, &caller("alice", false), &body(b5)).await.status, 200);

        // Equal epoch → refused.
        let (b5b, _, _) = signed_identity(home.path(), "alice", 5);
        assert_eq!(api_identity_enroll(&ctx, &caller("alice", false), &body(b5b)).await.status, 400);
        // Lower epoch → refused.
        let (b3, _, _) = signed_identity(home.path(), "alice", 3);
        assert_eq!(api_identity_enroll(&ctx, &caller("alice", false), &body(b3)).await.status, 400);

        // The stored row is still epoch 5 with the original key.
        let row = ctx.store.get_identity_key("alice").await.unwrap();
        assert_eq!(row.epoch, 5);
        assert_eq!(row.ed25519_pub, ed5);

        // A strictly higher epoch is accepted.
        let (b6, _, _) = signed_identity(home.path(), "alice", 6);
        assert_eq!(api_identity_enroll(&ctx, &caller("alice", false), &body(b6)).await.status, 200);
        assert_eq!(ctx.store.get_identity_key("alice").await.unwrap().epoch, 6);
    }

    /// A user cannot enroll a key under another username: the body username (if any) is ignored and the
    /// row always lands under the authenticated caller.
    #[tokio::test]
    async fn a_user_cannot_enroll_a_key_under_another_username() {
        let (_d, ctx) = harness().await;
        let home = tempfile::tempdir().unwrap();
        // alice authenticates and signs for her OWN caller identity, but stuffs "username":"bob" in the body.
        let (mut b, ed, _x) = signed_identity(home.path(), "alice", 0);
        b["username"] = serde_json::json!("bob");
        let resp = api_identity_enroll(&ctx, &caller("alice", false), &body(b)).await;
        assert_eq!(resp.status, 200, "the enroll succeeds — but as the caller, never the body name");
        // The row lands under the caller (alice); bob has nothing.
        assert_eq!(ctx.store.get_identity_key("alice").await.unwrap().ed25519_pub, ed);
        assert!(ctx.store.get_identity_key("bob").await.is_none(), "the body username must be ignored");
    }

    /// The complementary guard: a signature bound to a DIFFERENT username than the caller does not verify
    /// against the caller identity, so it cannot enroll a key for someone else.
    #[tokio::test]
    async fn a_signature_bound_to_another_username_does_not_verify() {
        let (_d, ctx) = harness().await;
        let home = tempfile::tempdir().unwrap();
        // Sign the tuple AS "bob" (username=bob in the signed message), then submit it as caller alice.
        let (b_as_bob, _ed, _x) = signed_identity(home.path(), "bob", 0);
        let resp = api_identity_enroll(&ctx, &caller("alice", false), &body(b_as_bob)).await;
        assert_eq!(resp.status, 400, "a signature over a different username must not verify for this caller");
        assert!(ctx.store.get_identity_key("alice").await.is_none());
        assert!(ctx.store.get_identity_key("bob").await.is_none());
    }

    /// Unknown users are non-disclosing: a single GET is a 404, and a batch GET omits them rather than
    /// signaling their absence. A batch lookup also returns exactly the known rows.
    #[tokio::test]
    async fn identity_lookup_is_non_disclosing_for_unknown_users() {
        let (_d, ctx) = harness().await;
        let home = tempfile::tempdir().unwrap();
        let (b, ed, _x) = signed_identity(home.path(), "alice", 0);
        assert_eq!(api_identity_enroll(&ctx, &caller("alice", false), &body(b)).await.status, 200);

        // Single GET of an unknown user → 404.
        assert_eq!(api_identity_get(&ctx, &caller("alice", false), "ghost").await.status, 404);

        // Batch GET: known present, unknowns omitted (not padded, not signaled).
        let batch = api_identity_list(&ctx, &get_req("/api/identity?users=alice,ghost,phantom"), &caller("alice", false)).await;
        assert_eq!(batch.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        let keys = v["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 1, "only the known user is returned");
        assert_eq!(keys[0]["username"], "alice");
        assert_eq!(keys[0]["ed25519_pub"], ed);
    }

    /// Reads require an authenticated caller (pubkeys are public, but only to logged-in callers).
    #[tokio::test]
    async fn identity_reads_require_authentication() {
        let (_d, ctx) = harness().await;
        assert_eq!(api_identity_get(&ctx, &Caller::anonymous(), "alice").await.status, 401);
        assert_eq!(api_identity_list(&ctx, &get_req("/api/identity?users=alice"), &Caller::anonymous()).await.status, 401);
        assert_eq!(
            api_identity_enroll(&ctx, &Caller::anonymous(), &body(serde_json::json!({}))).await.status,
            401,
            "enroll requires a logged-in caller"
        );
    }

    // ── Team-KEK envelopes (encryption-recipients Wave 3) ──

    /// Plant an org with the given (username, role) members, current_kek_gen = 0.
    async fn mk_org(ctx: &Ctx, name: &str, members: &[(&str, &str)]) {
        let name = name.to_string();
        let members: Vec<OrgMember> =
            members.iter().map(|(u, r)| OrgMember { username: (*u).into(), role: (*r).into() }).collect();
        ctx.store
            .update_orgs(|list| list.push(Org { name: name.clone(), members: members.clone(), created: store::now_iso(), current_kek_gen: 0, recovery_x25519: String::new(), escrow_mode: "none".into(), members_can_create: 1 }))
            .await
            .unwrap();
    }

    /// A publish body sealing gen `g` to each named recipient (ciphertext is opaque to the hub).
    fn kek_body(g: i64, recipients: &[&str]) -> Vec<u8> {
        let envelopes: Vec<serde_json::Value> = recipients
            .iter()
            .map(|r| serde_json::json!({ "recipient": r, "wrapped_kek": format!("wrap-for-{r}"), "recipient_epoch": 0 }))
            .collect();
        body(serde_json::json!({ "gen": g, "envelopes": envelopes }))
    }

    /// Publishing envelopes is ORG-ADMIN only: a plain member is 403, an admin succeeds and the org's
    /// current generation advances monotonically.
    #[tokio::test]
    async fn kek_publish_is_org_admin_only_and_bumps_generation() {
        let (_d, ctx) = harness().await;
        mk_org(&ctx, "acme", &[("alice", "admin"), ("bob", "member")]).await;

        // A plain member cannot publish → 403 (they pass the membership gate but not the admin one).
        let r = api_org_kek(&ctx, &caller("bob", false), "acme", "/envelopes", "POST", "", &kek_body(1, &["alice", "bob"])).await;
        assert_eq!(r.status, 403, "a non-admin member must be refused publishing");
        assert_eq!(ctx.store.get_current_kek_gen("acme").await, 0, "a refused publish must not advance the generation");

        // The admin publishes gen 1 → 200, current advances to 1.
        let r = api_org_kek(&ctx, &caller("alice", false), "acme", "/envelopes", "POST", "", &kek_body(1, &["alice", "bob"])).await;
        assert_eq!(r.status, 200);
        assert_eq!(ctx.store.get_current_kek_gen("acme").await, 1);

        // Advancing to gen 2 works; a stale gen (1, now behind) is refused and does NOT roll back current.
        assert_eq!(api_org_kek(&ctx, &caller("alice", false), "acme", "/envelopes", "POST", "", &kek_body(2, &["alice"])).await.status, 200);
        assert_eq!(ctx.store.get_current_kek_gen("acme").await, 2);
        assert_eq!(api_org_kek(&ctx, &caller("alice", false), "acme", "/envelopes", "POST", "", &kek_body(1, &["alice"])).await.status, 409);
        assert_eq!(ctx.store.get_current_kek_gen("acme").await, 2, "a behind-gen publish must not roll the generation back");
    }

    /// A member may fetch ONLY their OWN envelope — the recipient is the authenticated caller, never
    /// another member — and a non-member is 404 on both envelope and gens (existence non-disclosure).
    #[tokio::test]
    async fn kek_fetch_is_own_envelope_only_and_membership_gated() {
        let (_d, ctx) = harness().await;
        mk_org(&ctx, "acme", &[("alice", "admin"), ("bob", "member")]).await;
        assert_eq!(api_org_kek(&ctx, &caller("alice", false), "acme", "/envelopes", "POST", "", &kek_body(1, &["alice", "bob"])).await.status, 200);

        // bob fetches his own envelope → 200, and it is HIS ciphertext, never alice's.
        let r = api_org_kek(&ctx, &caller("bob", false), "acme", "/envelope", "GET", "gen=1", &[]).await;
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["wrapped_kek"], "wrap-for-bob", "a member gets their OWN envelope");
        assert_eq!(v["gen"], 1);

        // There is no way to ask for another recipient's envelope: the recipient is always the caller,
        // so alice's row is invisible to bob. bob at a gen he has no envelope for → 404.
        assert_eq!(api_org_kek(&ctx, &caller("bob", false), "acme", "/envelope", "GET", "gen=2", &[]).await.status, 404);

        // gens lists only bob's own generations, plus the org's current gen.
        let g = api_org_kek(&ctx, &caller("bob", false), "acme", "/gens", "GET", "", &[]).await;
        assert_eq!(g.status, 200);
        let gv: serde_json::Value = serde_json::from_slice(&g.body).unwrap();
        assert_eq!(gv["gens"], serde_json::json!([1]));
        assert_eq!(gv["current"], 1);

        // A NON-member (carol) is 404 on envelope AND gens — the org's existence is not disclosed.
        add_user(&ctx, "carol", "carol-password-1", false).await;
        assert_eq!(api_org_kek(&ctx, &caller("carol", false), "acme", "/envelope", "GET", "gen=1", &[]).await.status, 404);
        assert_eq!(api_org_kek(&ctx, &caller("carol", false), "acme", "/gens", "GET", "", &[]).await.status, 404);
        assert_eq!(api_org_kek(&ctx, &caller("carol", false), "acme", "/envelopes", "POST", "", &kek_body(2, &["carol"])).await.status, 404, "a non-member cannot even see the org to publish");
    }

    /// Republishing the CURRENT generation (the `hub team sync` join path) is idempotent: it does not
    /// advance the generation and adds the new member's envelope.
    #[tokio::test]
    async fn kek_republish_current_generation_is_idempotent_join() {
        let (_d, ctx) = harness().await;
        mk_org(&ctx, "acme", &[("alice", "admin")]).await;
        assert_eq!(api_org_kek(&ctx, &caller("alice", false), "acme", "/envelopes", "POST", "", &kek_body(1, &["alice"])).await.status, 200);
        assert_eq!(ctx.store.get_current_kek_gen("acme").await, 1);
        // bob joins: republish gen 1 sealing to both. Allowed (gen == current), no bump.
        assert_eq!(api_org_kek(&ctx, &caller("alice", false), "acme", "/envelopes", "POST", "", &kek_body(1, &["alice", "bob"])).await.status, 200);
        assert_eq!(ctx.store.get_current_kek_gen("acme").await, 1, "a same-gen republish must not bump");
        assert!(ctx.store.get_team_kek_envelope("acme", 1, "bob").await.is_some(), "the joined member now has an envelope");
    }

    // ── Wave-5 opt-in escrow / recovery (both OFF by default) ──

    /// Setting the escrow mode and the recovery recipient are BOTH owner-only: a plain org member is 403,
    /// the owner (an org admin) succeeds, and a non-member is 404 (existence non-disclosure, never 403).
    #[tokio::test]
    async fn wave5_escrow_mode_and_recovery_are_owner_only() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        add_user(&ctx, "bob", "bob-password-12", false).await;
        add_user(&ctx, "carol", "carol-password-1", false).await;
        mk_org(&ctx, "acme", &[("alice", "admin"), ("bob", "member")]).await;

        // Default OFF, and any member may read it.
        let show = api_org_escrow(&ctx, &caller("bob", false), "acme", "GET", &[]).await;
        assert_eq!(show.status, 200);
        assert_eq!(json_of(&show)["escrow_mode"], "none", "escrow is off by default");

        // A plain member cannot set escrow mode → 403; the mode stays none.
        let denied = api_org_escrow(&ctx, &caller("bob", false), "acme", "POST", &body(serde_json::json!({ "mode": "hub-assist" }))).await;
        assert_eq!(denied.status, 403, "a non-owner member must be refused");
        assert_eq!(ctx.store.org("acme").await.unwrap().escrow_mode, "none");

        // The owner (admin) sets it → 200.
        let ok = api_org_escrow(&ctx, &caller("alice", false), "acme", "POST", &body(serde_json::json!({ "mode": "hub-assist" }))).await;
        assert_eq!(ok.status, 200);
        assert_eq!(ctx.store.org("acme").await.unwrap().escrow_mode, "hub-assist");
        // An unknown mode is rejected.
        assert_eq!(api_org_escrow(&ctx, &caller("alice", false), "acme", "POST", &body(serde_json::json!({ "mode": "sideways" }))).await.status, 400);

        // Recovery recipient: a plain member is 403; the owner sets a valid hex key.
        let key = hex::encode([9u8; 32]);
        assert_eq!(
            api_org_recovery(&ctx, &caller("bob", false), "acme", "POST", &body(serde_json::json!({ "key": key }))).await.status,
            403,
            "a non-owner cannot set the recovery recipient"
        );
        assert_eq!(ctx.store.org("acme").await.unwrap().recovery_x25519, "", "still unset after the refused set");
        let set = api_org_recovery(&ctx, &caller("alice", false), "acme", "POST", &body(serde_json::json!({ "key": key }))).await;
        assert_eq!(set.status, 200);
        assert_eq!(ctx.store.org("acme").await.unwrap().recovery_x25519, key);
        // Junk (not 32-byte hex) is refused.
        assert_eq!(api_org_recovery(&ctx, &caller("alice", false), "acme", "POST", &body(serde_json::json!({ "key": "nothex" }))).await.status, 400);
        // Owner clears it.
        assert_eq!(api_org_recovery(&ctx, &caller("alice", false), "acme", "DELETE", &[]).await.status, 200);
        assert_eq!(ctx.store.org("acme").await.unwrap().recovery_x25519, "");

        // A NON-member is 404 on both (never 403 — the org's existence is not disclosed).
        assert_eq!(api_org_escrow(&ctx, &caller("carol", false), "acme", "POST", &body(serde_json::json!({ "mode": "hub-assist" }))).await.status, 404);
        assert_eq!(api_org_recovery(&ctx, &caller("carol", false), "acme", "POST", &body(serde_json::json!({ "key": key }))).await.status, 404);
    }

    /// The hub-assist RELEASE path: with the org in hub-assist mode and a CK escrowed, the hub returns the
    /// exact content key to an ACL READER, and is FAIL-CLOSED (404, non-disclosing, no key) for a caller
    /// who cannot read the session.
    #[tokio::test]
    async fn wave5_hub_assist_release_returns_ck_to_reader_and_denies_a_non_reader() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        add_user(&ctx, "bob", "bob-password-12", false).await;
        add_user(&ctx, "carol", "carol-password-1", false).await;
        mk_org(&ctx, "acme", &[("alice", "admin"), ("bob", "member")]).await;
        create_agent_with_aid(&ctx, "org:acme", "frontend", Visibility::Private, "agt_fe1").await;

        // Owner turns on hub-assist.
        assert_eq!(api_org_escrow(&ctx, &caller("alice", false), "acme", "POST", &body(serde_json::json!({ "mode": "hub-assist" }))).await.status, 200);

        // The owner (a writer) escrows a content key sealed to the hub escrow pubkey, exactly as the client would.
        let ck = [42u8; 32];
        let wrapped = agit::keybox::seal_tk_for_member(&ck, &ctx.escrow.public).unwrap();
        let esc = api_keys_escrow(&ctx, &caller("alice", false), "acme", "frontend", &body(serde_json::json!({ "kid": 0, "wrapped_ck": wrapped }))).await;
        assert_eq!(esc.status, 200, "the owner escrows: {}", String::from_utf8_lossy(&esc.body));

        // bob (an org member = ACL reader) releases → gets the EXACT CK back.
        let rel = api_keys_release(&ctx, &caller("bob", false), "acme", "frontend").await;
        assert_eq!(rel.status, 200);
        let arr = json_of(&rel)["released"].as_array().unwrap().clone();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kid"], 0);
        assert_eq!(arr[0]["ck"], hex::encode(ck), "the hub releases the exact escrowed CK to a reader");

        // carol (not a member, cannot read) → fail-closed 404, and the CK never appears in the body.
        let denied = api_keys_release(&ctx, &caller("carol", false), "acme", "frontend").await;
        assert_eq!(denied.status, 404, "a non-reader is refused, non-disclosing");
        assert!(!String::from_utf8_lossy(&denied.body).contains(&hex::encode(ck)), "no CK leaks to a non-reader");
    }

    /// Escrow is inert unless the org opted in: with the org in the default `none` mode, an upload is 403
    /// (a session never escrows) and RELEASE is 404 even for a reader with a planted escrow row (the escrow
    /// surface is not disclosed). Turning the mode ON later makes release work — proving the gate is the mode.
    #[tokio::test]
    async fn wave5_escrow_is_off_unless_org_is_hub_assist() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        add_user(&ctx, "bob", "bob-password-12", false).await;
        mk_org(&ctx, "acme", &[("alice", "admin"), ("bob", "member")]).await;
        create_agent_with_aid(&ctx, "org:acme", "frontend", Visibility::Private, "agt_fe1").await;

        // Mode is `none` (default). The owner (a writer) cannot escrow → 403.
        let ck = [7u8; 32];
        let wrapped = agit::keybox::seal_tk_for_member(&ck, &ctx.escrow.public).unwrap();
        let esc = api_keys_escrow(&ctx, &caller("alice", false), "acme", "frontend", &body(serde_json::json!({ "kid": 0, "wrapped_ck": wrapped.clone() }))).await;
        assert_eq!(esc.status, 403, "a session never escrows unless the org is hub-assist");

        // Plant an escrow row directly (bypassing the endpoint) to prove RELEASE still 404s while mode=none,
        // even for a reader — so the mode, not just the upload path, is the gate.
        ctx.store
            .upsert_escrow_key(&store::EscrowKey { owner: "acme".into(), name: "frontend".into(), kid: 0, wrapped_ck: wrapped.clone(), created: store::now_iso() })
            .await
            .unwrap();
        let rel = api_keys_release(&ctx, &caller("bob", false), "acme", "frontend").await;
        assert_eq!(rel.status, 404, "release is 404 while the org is not hub-assist, even for a reader");

        // Flip the mode ON — now the same reader's release returns the CK.
        assert_eq!(api_org_escrow(&ctx, &caller("alice", false), "acme", "POST", &body(serde_json::json!({ "mode": "hub-assist" }))).await.status, 200);
        let rel = api_keys_release(&ctx, &caller("bob", false), "acme", "frontend").await;
        assert_eq!(rel.status, 200);
        assert_eq!(json_of(&rel)["released"].as_array().unwrap()[0]["ck"], hex::encode(ck));
    }

    /// Enroll publishes a committer email, and `by-email` resolves that email back to the account — the
    /// bridge provenance verification consults. Non-disclosing: an unknown email is a plain 404, and an
    /// anonymous caller is refused.
    #[tokio::test]
    async fn identity_enroll_with_email_then_lookup_by_email() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;

        // A real ed25519 keypair so the possession proof (enroll_sig) verifies.
        let home = tempfile::tempdir().unwrap();
        let sk = agit::agent::load_or_create_signing_key(home.path()).unwrap();
        let ed_pub = hex::encode(sk.verifying_key().to_bytes());
        let x_pub = "ab".repeat(32);
        let enroll_sig = agit::agent::sign_hex(&sk, &agit::agent::identity_enroll_message("alice", 0, &ed_pub, &x_pub));
        let enrolled = api_identity_enroll(
            &ctx,
            &caller("alice", false),
            &body(serde_json::json!({ "ed25519_pub": ed_pub, "x25519_pub": x_pub, "epoch": 0, "enroll_sig": enroll_sig, "email": "Alice@Corp.com" })),
        )
        .await;
        assert_eq!(enrolled.status, 200);

        let by_email = |email: &str| {
            let mut r = req("GET", None);
            r.target = format!("/api/identity/by-email?email={email}");
            r
        };
        // The email-squatting defense: an enrolled-but-UNVERIFIED email is NOT attributable, so the lookup
        // is a normal 404 (a squatter who never controls the mailbox never clears this gate).
        let before = api_identity_by_email(&ctx, &by_email("Alice%40corp.com"), &caller("bob", false)).await;
        assert_eq!(before.status, 404, "an unverified email must not attribute (anti-squatting)");

        // Verify alice's email out-of-band (admin force-verify here), then the lookup resolves.
        ctx.store.set_email_verified("alice", true).await.unwrap();
        // The committer email (percent-encoded, mixed case) resolves back to alice with her signing key.
        let hit = api_identity_by_email(&ctx, &by_email("Alice%40corp.com"), &caller("bob", false)).await;
        assert_eq!(hit.status, 200, "a verified email resolves for any authenticated caller");
        let v = json_of(&hit);
        assert_eq!(v["username"], "alice");
        assert_eq!(v["ed25519_pub"], ed_pub);

        // An email nobody enrolled is a normal not-found, not an oracle.
        let miss = api_identity_by_email(&ctx, &by_email("ghost%40corp.com"), &caller("bob", false)).await;
        assert_eq!(miss.status, 404);
        // Anonymous is refused before any lookup.
        let anon = api_identity_by_email(&ctx, &by_email("alice%40corp.com"), &Caller::anonymous()).await;
        assert_eq!(anon.status, 401);
    }

    // ── email verification ──

    /// Enroll an email, resend a token (operator-forwarded), consume it via POST /api/verify-email, and
    /// confirm the account is now verified and its email is attributable. Also covers the single-use +
    /// bad-token 400s and that /api/me exposes email + email_verified.
    #[tokio::test]
    async fn email_verification_flow_end_to_end() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let home = tempfile::tempdir().unwrap();
        let sk = agit::agent::load_or_create_signing_key(home.path()).unwrap();
        let ed_pub = hex::encode(sk.verifying_key().to_bytes());
        let x_pub = "ab".repeat(32);
        let enroll_sig = agit::agent::sign_hex(&sk, &agit::agent::identity_enroll_message("alice", 0, &ed_pub, &x_pub));
        api_identity_enroll(
            &ctx,
            &caller("alice", false),
            &body(serde_json::json!({ "ed25519_pub": ed_pub, "x25519_pub": x_pub, "epoch": 0, "enroll_sig": enroll_sig, "email": "alice@corp.com" })),
        )
        .await;

        // /api/me exposes the email and the (still false) verified flag.
        let me = json_of(&api_me(&ctx, &caller("alice", false)).await);
        assert_eq!(me["email"], "alice@corp.com");
        assert_eq!(me["email_verified"], false);

        // A bad token is a flat 400.
        assert_eq!(api_verify_email(&ctx, &body(serde_json::json!({ "token": "evt_nope" }))).await.status, 400);

        // Resend mints a fresh token (delivered out-of-band); the response never carries the token.
        let resend = api_me_verify_resend(&ctx, &caller("alice", false)).await;
        assert_eq!(resend.status, 200);
        assert!(!String::from_utf8_lossy(&resend.body).contains("evt_"), "the token is never returned in the response");

        // Pull the delivered token straight from the store (stands in for the operator forwarding it),
        // consume it, and confirm the account flips to verified.
        let token = ctx.store.mint_email_token("alice", "alice@corp.com", std::time::Duration::from_secs(3600)).await.unwrap();
        let ok = api_verify_email(&ctx, &body(serde_json::json!({ "token": token.clone() }))).await;
        assert_eq!(ok.status, 200);
        assert!(ctx.store.user("alice").await.unwrap().email_verified, "the token consume marks the account verified");
        // Single-use: replaying the same token is a 400.
        assert_eq!(api_verify_email(&ctx, &body(serde_json::json!({ "token": token }))).await.status, 400);

        // /api/me now reports verified, and the email is attributable.
        assert_eq!(json_of(&api_me(&ctx, &caller("alice", false)).await)["email_verified"], true);
        assert!(!ctx.store.get_identity_keys_by_email("alice@corp.com").await.is_empty());
    }

    /// Re-enrolling with a DIFFERENT email must RESET verification (the squatting hole otherwise reopens:
    /// verify address A, then claim address B keeping the verified flag).
    #[tokio::test]
    async fn re_enroll_with_a_new_email_resets_verification() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let home = tempfile::tempdir().unwrap();
        let sk = agit::agent::load_or_create_signing_key(home.path()).unwrap();
        let ed_pub = hex::encode(sk.verifying_key().to_bytes());
        let x_pub = "ab".repeat(32);
        let enroll = |epoch: i64, email: &str| {
            let sig = agit::agent::sign_hex(&sk, &agit::agent::identity_enroll_message("alice", epoch, &ed_pub, &x_pub));
            body(serde_json::json!({ "ed25519_pub": ed_pub, "x25519_pub": x_pub, "epoch": epoch, "enroll_sig": sig, "email": email }))
        };
        api_identity_enroll(&ctx, &caller("alice", false), &enroll(0, "alice@corp.com")).await;
        ctx.store.set_email_verified("alice", true).await.unwrap();
        assert!(!ctx.store.get_identity_keys_by_email("alice@corp.com").await.is_empty());

        // Re-enroll (advancing epoch) claiming a NEW email → verification is reset, neither email attributes.
        assert_eq!(api_identity_enroll(&ctx, &caller("alice", false), &enroll(1, "ceo@corp.com")).await.status, 200);
        assert!(!ctx.store.user("alice").await.unwrap().email_verified, "changing the email resets verification");
        assert!(ctx.store.get_identity_keys_by_email("ceo@corp.com").await.is_empty(), "the new (unverified) email does not attribute");
    }

    /// Admin force-verify (POST /api/users/<u>/verify-email) is admin-gated and flips the flag.
    #[tokio::test]
    async fn admin_verify_email_is_admin_gated() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        add_user(&ctx, "root", "root-password-1", true).await;

        // A non-admin cannot force-verify.
        assert_eq!(api_admin_verify_email(&ctx, &caller("alice", false), "alice").await.status, 403);
        // Unknown user is a 404 for an admin.
        assert_eq!(api_admin_verify_email(&ctx, &caller("root", true), "ghost").await.status, 404);
        // An admin flips it.
        let ok = api_admin_verify_email(&ctx, &caller("root", true), "alice").await;
        assert_eq!(ok.status, 200);
        assert!(ctx.store.user("alice").await.unwrap().email_verified);
    }

    // ── live registry attribution on the SESSION read path (classify_read_status) ──
    //
    // These pin the property the whole wave exists for: the surfaced session verdict is classified
    // against the identity registry, not just self-verified, and a forgery (KeyMismatch) is NEVER
    // upgraded to a positive attribution. They exercise `classify_read_status` — the exact async step
    // the `session/<id>/provenance` handler runs against `ctx.store` — with a self-verified `Verified`
    // as its input (the git self-verify is covered by `content.rs`/the client; here the store is under
    // test). The email-verify tie-in is exercised directly: `get_identity_keys_by_email` is verified-only.

    use agit::commands::ProvenanceStatus as PS;

    /// Enroll `user`'s signing key against `email`, optionally verifying the email. Mirrors the enroll +
    /// verify path a real account goes through, so attribution is gated exactly as production gates it.
    async fn enroll_identity(ctx: &Ctx, user: &str, ed25519_pub: &str, email: &str, verified: bool) {
        add_user(ctx, user, "a-long-enough-password", false).await;
        ctx.store
            .add_identity_key(store::IdentityKey {
                username: user.into(),
                key_fpr: String::new(), // the facade derives it
                ed25519_pub: ed25519_pub.into(),
                x25519_pub: "b".repeat(64),
                label: "test".into(),
                epoch: 0,
                enroll_sig: "sig".into(),
                created: store::now_iso(),
                revoked: None,
                email: email.into(),
            })
            .await
            .unwrap();
        if verified {
            ctx.store.set_email_verified(user, true).await.unwrap();
        }
    }

    fn verified_self(email: &str, pubkey: &str) -> PS {
        PS::Verified { aid: "agt_01".into(), email: email.into(), pubkey: pubkey.into() }
    }

    /// VERIFIED_AS: the committer email maps (verified) to an account whose registered ed25519 key EQUALS
    /// the provenance pubkey — a real, verified account. The verdict carries the attributed username.
    #[tokio::test]
    async fn read_classifies_verified_as_on_key_match() {
        let (_d, ctx) = harness().await;
        let key = "aa".repeat(32);
        enroll_identity(&ctx, "alice", &key, "alice@corp.com", true).await;

        let out = classify_read_status(&ctx.store, verified_self("alice@corp.com", &key)).await;
        match out {
            PS::VerifiedAs { username, email, pubkey, .. } => {
                assert_eq!(username, "alice");
                assert_eq!(email, "alice@corp.com");
                assert_eq!(pubkey, key);
            }
            other => panic!("expected VerifiedAs, got {other:?}"),
        }
        // And the JSON the SPA reads carries the positive word + username, never "verified".
        let v = provenance_verdict_json(&classify_read_status(&ctx.store, verified_self("alice@corp.com", &key)).await);
        assert_eq!(v["status"], "verified_as");
        assert_eq!(v["username"], "alice");
    }

    /// KEY_MISMATCH (the anti-forgery property): the email maps to a registered, verified account whose
    /// key DIFFERS from the signing key. It must classify as KeyMismatch — never VerifiedAs — and the
    /// rendered word must be "key_mismatch", never "verified"/"verified_as".
    #[tokio::test]
    async fn read_classifies_key_mismatch_and_never_verified() {
        let (_d, ctx) = harness().await;
        let registered = "aa".repeat(32);
        let forged = "bb".repeat(32); // signed by a DIFFERENT key than alice's registered one
        enroll_identity(&ctx, "alice", &registered, "alice@corp.com", true).await;

        let out = classify_read_status(&ctx.store, verified_self("alice@corp.com", &forged)).await;
        match &out {
            PS::KeyMismatch { email, claimed_username, .. } => {
                assert_eq!(email, "alice@corp.com");
                assert_eq!(claimed_username, "alice");
            }
            other => panic!("a forgery must be KeyMismatch, got {other:?}"),
        }
        let v = provenance_verdict_json(&out);
        assert_eq!(v["status"], "key_mismatch", "a forgery is NEVER rendered green");
        assert_ne!(v["status"], "verified");
        assert_ne!(v["status"], "verified_as");
    }

    /// SIGNED_UNREGISTERED: a validly self-signed session whose committer email maps to NO account
    /// degrades to signed-unregistered — a signature with no hub attribution, never a false "verified as".
    #[tokio::test]
    async fn read_classifies_signed_unregistered_when_email_unknown() {
        let (_d, ctx) = harness().await;
        let key = "aa".repeat(32);
        // Nobody enrolled nobody@corp.com.
        let out = classify_read_status(&ctx.store, verified_self("nobody@corp.com", &key)).await;
        assert!(matches!(out, PS::SignedUnregistered { .. }), "unknown email → signed_unregistered, got {out:?}");
        let v = provenance_verdict_json(&out);
        assert_eq!(v["status"], "signed_unregistered");
    }

    /// The email-verify tie-in: an UNVERIFIED email (a registered key exists, but the account never
    /// verified the address) is SIGNED_UNREGISTERED, NOT verified_as — even though the key would match.
    /// This inherits `get_identity_keys_by_email`'s verified-only gate; a squatter can't mint attribution.
    #[tokio::test]
    async fn read_unverified_email_is_signed_unregistered_not_verified_as() {
        let (_d, ctx) = harness().await;
        let key = "aa".repeat(32);
        // alice enrolls the matching key but never verifies the email.
        enroll_identity(&ctx, "alice", &key, "alice@corp.com", false).await;

        let out = classify_read_status(&ctx.store, verified_self("alice@corp.com", &key)).await;
        assert!(
            matches!(out, PS::SignedUnregistered { .. }),
            "an unverified email must NOT attribute even on a key match, got {out:?}"
        );
        let v = provenance_verdict_json(&out);
        assert_eq!(v["status"], "signed_unregistered");
    }

    /// A non-`Verified` self-status (nothing to attribute) passes through untouched: no false upgrade,
    /// no store lookup that could change the verdict. Unsigned stays unsigned even with a live registry.
    #[tokio::test]
    async fn read_passes_non_verified_status_through() {
        let (_d, ctx) = harness().await;
        let out = classify_read_status(&ctx.store, PS::Unsigned).await;
        assert!(matches!(out, PS::Unsigned));
        let out = classify_read_status(&ctx.store, PS::BadSignature).await;
        assert!(matches!(out, PS::BadSignature));
    }

    // ── agent-store creation: member-create policy + initialize-on-create ──

    /// Plant an org (via `mk_org`) then set its `members_can_create` policy directly in the store, so a
    /// test can drive both the permissive (1) and admins-only (0) states.
    async fn mk_org_policy(ctx: &Ctx, name: &str, members: &[(&str, &str)], members_can_create: i64) {
        mk_org(ctx, name, members).await;
        let n = name.to_string();
        ctx.store
            .update_orgs(move |list| {
                if let Some(o) = list.iter_mut().find(|o| o.name == n) {
                    o.members_can_create = members_can_create;
                }
            })
            .await
            .unwrap();
    }

    /// A plain member (non-admin) MAY create an agent under the org when `members_can_create = 1` (the
    /// GitHub default). The agent lands owned by `org:<name>`.
    #[tokio::test]
    async fn org_member_can_create_when_policy_allows() {
        let (_d, ctx) = harness().await;
        mk_org_policy(&ctx, "acme", &[("alice", "admin"), ("bob", "member")], 1).await;
        let r = api_create_agent(&ctx, &req("POST", None), &caller("bob", false), &body(serde_json::json!({ "name": "svc", "org": "acme" }))).await;
        assert_eq!(r.status, 201, "a member creates under the org when members_can_create=1");
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["owner"], "org:acme");
        assert_eq!(v["full_name"], "acme/svc");
    }

    /// A plain member is REFUSED (403) when `members_can_create = 0`; the distinct 403 (not the 404) is
    /// fine because they already proved membership, so it discloses nothing new.
    #[tokio::test]
    async fn org_member_cannot_create_when_policy_forbids() {
        let (_d, ctx) = harness().await;
        mk_org_policy(&ctx, "acme", &[("alice", "admin"), ("bob", "member")], 0).await;
        let r = api_create_agent(&ctx, &req("POST", None), &caller("bob", false), &body(serde_json::json!({ "name": "svc", "org": "acme" }))).await;
        assert_eq!(r.status, 403, "a member is refused when members_can_create=0");
        // Nothing was created.
        assert!(ctx.store.agent_scoped("acme", "svc").await.is_none());
    }

    /// An org ADMIN always creates, even under the admins-only policy.
    #[tokio::test]
    async fn org_admin_always_creates_regardless_of_policy() {
        let (_d, ctx) = harness().await;
        mk_org_policy(&ctx, "acme", &[("alice", "admin"), ("bob", "member")], 0).await;
        let r = api_create_agent(&ctx, &req("POST", None), &caller("alice", false), &body(serde_json::json!({ "name": "svc", "org": "acme" }))).await;
        assert_eq!(r.status, 201, "an org admin creates even when members_can_create=0");
    }

    /// A NON-member gets the uniform, non-disclosing 404 — a missing org and one the caller can't see are
    /// indistinguishable, so create can't be used to probe which orgs exist.
    #[tokio::test]
    async fn org_nonmember_gets_nondisclosing_404_on_create() {
        let (_d, ctx) = harness().await;
        mk_org_policy(&ctx, "acme", &[("alice", "admin")], 1).await;
        let r = api_create_agent(&ctx, &req("POST", None), &caller("mallory", false), &body(serde_json::json!({ "name": "svc", "org": "acme" }))).await;
        assert_eq!(r.status, 404, "a non-member can't tell the org exists");
        // A create aimed at a truly-missing org is the SAME 404 — no distinguishing the two.
        let r2 = api_create_agent(&ctx, &req("POST", None), &caller("mallory", false), &body(serde_json::json!({ "name": "svc", "org": "ghost" }))).await;
        assert_eq!(r2.status, 404);
    }

    /// Only an org admin may toggle `members_can_create`: a plain member is 403 (policy unchanged), a
    /// non-member is the non-disclosing 404, and the admin's change sticks AND is enforced on the next
    /// member create.
    #[tokio::test]
    async fn only_org_admin_toggles_member_create_policy() {
        let (_d, ctx) = harness().await;
        mk_org_policy(&ctx, "acme", &[("alice", "admin"), ("bob", "member")], 1).await;

        // A plain member cannot change it.
        let r = api_org_settings(&ctx, &caller("bob", false), "acme", "POST", &body(serde_json::json!({ "members_can_create": false }))).await;
        assert_eq!(r.status, 403, "a member cannot change org settings");
        assert_eq!(ctx.store.org("acme").await.unwrap().members_can_create, 1, "a refused toggle leaves the policy alone");

        // A non-member gets the uniform 404 (existence non-disclosure).
        let r = api_org_settings(&ctx, &caller("mallory", false), "acme", "POST", &body(serde_json::json!({ "members_can_create": false }))).await;
        assert_eq!(r.status, 404);

        // The admin sets admins-only, and it persists.
        let r = api_org_settings(&ctx, &caller("alice", false), "acme", "POST", &body(serde_json::json!({ "members_can_create": false }))).await;
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["members_can_create"], false);
        assert_eq!(ctx.store.org("acme").await.unwrap().members_can_create, 0);

        // And the policy is now enforced end-to-end: the member's create is refused.
        let c = api_create_agent(&ctx, &req("POST", None), &caller("bob", false), &body(serde_json::json!({ "name": "svc", "org": "acme" }))).await;
        assert_eq!(c.status, 403, "the new policy is enforced on the next member create");
    }

    /// GET on the settings route shows the policy to any member; the org GET carries it too (as a bool).
    #[tokio::test]
    async fn member_create_policy_is_readable() {
        let (_d, ctx) = harness().await;
        mk_org_policy(&ctx, "acme", &[("alice", "admin"), ("bob", "member")], 0).await;
        let r = api_org_settings(&ctx, &caller("bob", false), "acme", "GET", &[]).await;
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["members_can_create"], false, "a member reads the current policy");
        let g = api_org_get(&ctx, &caller("bob", false), "acme").await;
        let gv: serde_json::Value = serde_json::from_slice(&g.body).unwrap();
        assert_eq!(gv["members_can_create"], false, "the org GET surfaces the policy as a bool");
    }

    /// `initialize = true` bootstraps a valid, immediately-cloneable store: its default branch has a
    /// commit whose HEAD tree contains an `agent.toml` carrying an `agt_` aid — i.e. the store is
    /// adoptable (a plain clone would find the identity), closing the create-then-clone chicken-and-egg.
    #[tokio::test]
    async fn create_initialize_true_bootstraps_adoptable_store() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let r = api_create_agent(&ctx, &req("POST", None), &caller("alice", false), &body(serde_json::json!({ "name": "fresh", "initialize": true }))).await;
        assert_eq!(r.status, 201);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["initialized"], true, "the response reports it was initialized");
        let aid = v["aid"].as_str().expect("a bootstrapped store reports its minted aid");
        assert!(aid.starts_with("agt_"), "the minted aid is agt_-shaped: {aid}");

        // The repo has a real commit on its default branch, and its HEAD tree carries agent.toml.
        // (`rev-parse --verify HEAD` fails on an unborn branch, unlike the bare `rev-parse HEAD`.)
        let repo = repo_path(ctx.root(), "alice", "fresh");
        assert!(git(&repo, &["rev-parse", "--verify", "HEAD"]).is_some(), "an initialized store has a commit on its default branch");
        let tree = git(&repo, &["ls-tree", "--name-only", "HEAD"]).unwrap();
        assert!(tree.lines().any(|l| l == "agent.toml"), "HEAD tree contains agent.toml (adoptable): {tree:?}");

        // The committed agent.toml parses to the SAME agt_ aid the response reported — the store is
        // identified and adoptable, not a placeholder.
        let (read_aid, src) = agent_aid(&repo);
        assert_eq!(read_aid.as_deref(), Some(aid), "the committed agent.toml carries the reported aid");
        assert_eq!(src, "agent.toml");
    }

    /// The default (no `initialize`) leaves an EMPTY bare repo — no commits, no aid — exactly as before:
    /// a bare name reservation for pushing an existing agent.
    #[tokio::test]
    async fn create_default_leaves_empty_bare_repo() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let r = api_create_agent(&ctx, &req("POST", None), &caller("alice", false), &body(serde_json::json!({ "name": "reserved" }))).await;
        assert_eq!(r.status, 201);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["initialized"], false, "omitting initialize defaults to a bare reservation");
        assert!(v["aid"].is_null(), "an empty repo has no aid yet");
        let repo = repo_path(ctx.root(), "alice", "reserved");
        assert!(git(&repo, &["rev-parse", "--verify", "HEAD"]).is_none(), "the default create leaves an empty bare repo with no commits");
    }

    /// `initialize = false` explicitly is the same bare reservation as omitting it.
    #[tokio::test]
    async fn create_initialize_false_is_bare_reservation() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        let r = api_create_agent(&ctx, &req("POST", None), &caller("alice", false), &body(serde_json::json!({ "name": "reserved", "initialize": false }))).await;
        assert_eq!(r.status, 201);
        let repo = repo_path(ctx.root(), "alice", "reserved");
        assert!(git(&repo, &["rev-parse", "--verify", "HEAD"]).is_none(), "initialize=false leaves an empty bare repo");
    }

    // ── token binding is gated at the token's own scope (FIX 3) ──

    /// A WRITE token bound to an agent the caller can only READ (a public agent they don't own) is
    /// REFUSED at creation — not minted to 403 later at the first push. The gate now runs at
    /// Action::Write for a write-scoped token, and bob can only read alice's public agent.
    #[tokio::test]
    async fn write_token_bound_to_a_public_agent_you_only_read_is_refused() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        add_user(&ctx, "bob", "bob-password-1", false).await;
        create_agent(&ctx.store, "frontend", "alice", Visibility::Public).await.unwrap();
        let r = api_create_token(
            &ctx,
            &caller("bob", false),
            &body(serde_json::json!({ "name": "ci", "scope": "write", "agent": "alice/frontend" })),
        )
        .await;
        // bob CAN read alice/frontend (it's public), so this is a 403 (not the 404 existence-mask),
        // and no token is issued.
        assert_eq!(r.status, 403, "a write token for an agent you can only read is refused now");
        assert!(ctx.store.tokens().await.is_empty(), "no token was persisted");
    }

    /// A READ token bound to that SAME public agent still SUCCEEDS: read-scoped only needs Read, which
    /// bob has.
    #[tokio::test]
    async fn read_token_bound_to_a_public_agent_you_can_read_succeeds() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        add_user(&ctx, "bob", "bob-password-1", false).await;
        create_agent(&ctx.store, "frontend", "alice", Visibility::Public).await.unwrap();
        let r = api_create_token(
            &ctx,
            &caller("bob", false),
            &body(serde_json::json!({ "name": "ci", "scope": "read", "agent": "alice/frontend" })),
        )
        .await;
        assert_eq!(r.status, 201, "a read token for an agent you can read is fine");
        assert_eq!(ctx.store.tokens().await.len(), 1, "the read token was persisted");
    }

    /// A WRITE token bound to an agent the caller OWNS still SUCCEEDS: the owner has Write.
    #[tokio::test]
    async fn write_token_bound_to_an_agent_you_own_succeeds() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        create_agent(&ctx.store, "frontend", "alice", Visibility::Private).await.unwrap();
        let r = api_create_token(
            &ctx,
            &caller("alice", false),
            &body(serde_json::json!({ "name": "ci", "scope": "write", "agent": "alice/frontend" })),
        )
        .await;
        assert_eq!(r.status, 201, "the owner can mint a write token bound to their own agent");
        assert_eq!(ctx.store.tokens().await.len(), 1, "the write token was persisted");
    }

    // ── admin user roster (list / create / disable / enable) ──

    #[tokio::test]
    async fn roster_list_is_admin_and_login_session_only() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "root", "root-password-1", true).await;
        add_user(&ctx, "alice", "alice-password-1", false).await;

        // An admin (login session) sees the whole roster.
        let ok = api_users_list(&ctx, &caller("root", true)).await;
        assert_eq!(ok.status, 200);
        let body = String::from_utf8_lossy(&ok.body);
        assert!(body.contains("\"alice\"") && body.contains("\"root\""), "roster lists every account");
        assert!(body.contains("disabled"), "each row carries the disabled flag");
        assert!(!body.contains("pw_hash") && !body.contains("salt"), "no secret material leaks into the roster");

        // A non-admin is refused, an anonymous caller is refused, and an admin's TOKEN is refused
        // (roster management is login-session only, like the site-wide audit log).
        assert_eq!(api_users_list(&ctx, &caller("alice", false)).await.status, 403, "non-admin");
        assert_eq!(api_users_list(&ctx, &Caller::anonymous()).await.status, 401, "anonymous");
        assert_eq!(
            api_users_list(&ctx, &caller("root", true).with_token(None, Scope::Read)).await.status,
            403,
            "an admin's token can't manage the roster"
        );
    }

    #[tokio::test]
    async fn roster_create_user_enforces_the_rules() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "root", "root-password-1", true).await;

        // The admin creates a normal user; the given password then logs in.
        let ok = api_users_create(&ctx, &caller("root", true), &body(serde_json::json!({ "username": "bob", "password": "bob-password-1" }))).await;
        assert_eq!(ok.status, 200);
        assert!(auth::verify_login(&ctx.store, "bob", "bob-password-1").await.is_some(), "the created account can log in");
        assert!(!ctx.store.user("bob").await.unwrap().is_admin, "no is_admin field defaults to a normal user");

        // The password-length rule, username validity, and the taken check all bite.
        assert_eq!(
            api_users_create(&ctx, &caller("root", true), &body(serde_json::json!({ "username": "carol", "password": "short" }))).await.status,
            400,
            "password too short"
        );
        assert_eq!(
            api_users_create(&ctx, &caller("root", true), &body(serde_json::json!({ "username": "A", "password": "carol-password-1" }))).await.status,
            400,
            "invalid username"
        );
        assert_eq!(
            api_users_create(&ctx, &caller("root", true), &body(serde_json::json!({ "username": "bob", "password": "another-pass-1" }))).await.status,
            409,
            "taken"
        );

        // An admin may create another admin when they ask for one.
        let mk_admin = api_users_create(&ctx, &caller("root", true), &body(serde_json::json!({ "username": "dave", "password": "dave-password-1", "is_admin": true }))).await;
        assert_eq!(mk_admin.status, 200);
        assert!(ctx.store.user("dave").await.unwrap().is_admin, "is_admin:true from an admin creates an admin");

        // A non-admin can't create anyone — so a non-admin can't create an admin either. The account is
        // never written.
        let denied = api_users_create(&ctx, &caller("bob", false), &body(serde_json::json!({ "username": "evil", "password": "evil-password-1", "is_admin": true }))).await;
        assert_eq!(denied.status, 403);
        assert!(ctx.store.user("evil").await.is_none(), "the refused create wrote nothing");
        // An anonymous caller is refused before the body is parsed.
        assert_eq!(api_users_create(&ctx, &Caller::anonymous(), &body(serde_json::json!({ "username": "x", "password": "y" }))).await.status, 401);
    }

    #[tokio::test]
    async fn disable_blocks_login_revokes_sessions_and_enable_restores() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "root", "root-password-1", true).await;
        add_user(&ctx, "bob", "bob-password-1", false).await;
        // bob has a live session going in.
        let sid = ctx.sessions.create("bob").unwrap();
        assert_eq!(ctx.sessions.lookup(&sid).as_deref(), Some("bob"));

        // The admin disables bob: his live session is revoked and the flag is set.
        let dis = api_admin_set_disabled(&ctx, &caller("root", true), "bob", true).await;
        assert_eq!(dis.status, 200);
        assert_eq!(ctx.sessions.lookup(&sid), None, "disabling revokes the target's live sessions");
        assert!(ctx.store.user("bob").await.unwrap().disabled);

        // The CORRECT password now yields a 403 disabled (not a session); a WRONG password still yields the
        // generic 401, so disabling is not a password oracle.
        let good = api_login(&ctx, &req("POST", None), &body(serde_json::json!({ "username": "bob", "password": "bob-password-1" }))).await;
        assert_eq!(good.status, 403, "a disabled account can't log in even with the right password");
        assert!(!good.extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("set-cookie")), "no session cookie is issued to a disabled account");
        let bad = api_login(&ctx, &req("POST", None), &body(serde_json::json!({ "username": "bob", "password": "wrong-password-9" }))).await;
        assert_eq!(bad.status, 401, "a wrong password is still the generic 401 — no oracle");

        // Enable restores login.
        let en = api_admin_set_disabled(&ctx, &caller("root", true), "bob", false).await;
        assert_eq!(en.status, 200);
        assert!(!ctx.store.user("bob").await.unwrap().disabled);
        let back = api_login(&ctx, &req("POST", None), &body(serde_json::json!({ "username": "bob", "password": "bob-password-1" }))).await;
        assert_eq!(back.status, 200, "an enabled account logs in again");
    }

    #[tokio::test]
    async fn disable_guards_self_and_last_admin() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "root", "root-password-1", true).await;

        // Sole admin disabling themselves is the last-admin case: refused, and root stays enabled.
        let last = api_admin_set_disabled(&ctx, &caller("root", true), "root", true).await;
        assert_eq!(last.status, 400, "can't disable the last remaining admin");
        assert!(!ctx.store.user("root").await.unwrap().disabled, "root is untouched");

        // With a SECOND admin present, root disabling ITSELF is now the self-lockout guard (root2 remains).
        add_user(&ctx, "root2", "root2-password-1", true).await;
        let selfd = api_admin_set_disabled(&ctx, &caller("root", true), "root", true).await;
        assert_eq!(selfd.status, 400, "an admin can't disable their own account");
        assert!(!ctx.store.user("root").await.unwrap().disabled);

        // But root CAN disable the other admin (two enabled admins → one remains).
        let other = api_admin_set_disabled(&ctx, &caller("root", true), "root2", true).await;
        assert_eq!(other.status, 200);
        assert!(ctx.store.user("root2").await.unwrap().disabled);

        // An unknown target is a plain 404.
        assert_eq!(api_admin_set_disabled(&ctx, &caller("root", true), "ghost", true).await.status, 404);
    }

    // ── org write endpoints honor token scope (Fix 3) ──

    #[tokio::test]
    async fn org_writes_refuse_a_read_token_but_allow_a_write_token_and_a_login() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "pw-alice-1", false).await; // org admin
        add_user(&ctx, "bob", "pw-bob-1", false).await; // member
        add_user(&ctx, "carol", "pw-carol-1", false).await; // invitee / transfer target
        mk_org(&ctx, "acme", &[("alice", "admin"), ("bob", "member")]).await;

        let read = |u: &str| caller(u, false).with_token(None, Scope::Read);
        let writ = |u: &str| caller(u, false).with_token(None, Scope::Write);

        // create: a READ token is refused 403; a WRITE token succeeds (201).
        assert_eq!(api_orgs_create(&ctx, &read("alice"), &body(serde_json::json!({ "name": "readorg" }))).await.status, 403, "read token can't create an org");
        assert_eq!(api_orgs_create(&ctx, &writ("alice"), &body(serde_json::json!({ "name": "writeorg" }))).await.status, 201, "write token creates an org");
        assert!(ctx.store.org("readorg").await.is_none(), "the refused create planted nothing");

        // settings POST: read → 403, write → 200; GET stays allowed with a read token.
        assert_eq!(api_org_settings(&ctx, &read("alice"), "acme", "POST", &body(serde_json::json!({ "members_can_create": false }))).await.status, 403);
        assert_eq!(api_org_settings(&ctx, &writ("alice"), "acme", "POST", &body(serde_json::json!({ "members_can_create": false }))).await.status, 200);
        assert_eq!(api_org_settings(&ctx, &read("alice"), "acme", "GET", &[]).await.status, 200, "a read GET is still allowed");

        // members POST (role change): read → 403, write → 200.
        assert_eq!(api_org_members(&ctx, &read("alice"), "acme", "", "POST", &body(serde_json::json!({ "username": "bob", "role": "admin" }))).await.status, 403);
        assert_eq!(api_org_members(&ctx, &writ("alice"), "acme", "", "POST", &body(serde_json::json!({ "username": "bob", "role": "admin" }))).await.status, 200);
        assert_eq!(api_org_members(&ctx, &read("alice"), "acme", "", "GET", &[]).await.status, 200, "a read GET is still allowed");
        // members DELETE: read → 403 (bob is still a member afterward).
        assert_eq!(api_org_members(&ctx, &read("alice"), "acme", "/bob", "DELETE", &[]).await.status, 403);
        assert!(ctx.store.org("acme").await.unwrap().is_member("bob"), "the refused delete removed nobody");

        // invitations POST (create): read → 403, write → 201; GET (admin list) allowed with a read token.
        assert_eq!(api_org_invitations(&ctx, &read("alice"), "acme", "", "POST", &body(serde_json::json!({ "username": "carol", "role": "member" }))).await.status, 403);
        let inv = api_org_invitations(&ctx, &writ("alice"), "acme", "", "POST", &body(serde_json::json!({ "username": "carol", "role": "member" }))).await;
        assert!(inv.status == 200 || inv.status == 201, "write token creates an invitation: {}", inv.status);
        assert_eq!(api_org_invitations(&ctx, &read("alice"), "acme", "", "GET", &[]).await.status, 200, "a read GET is still allowed");

        // transfer + delete are MANAGEMENT-grade: refused for ANY token (read OR write), allowed only
        // from a login session (token == None).
        assert_eq!(api_org_transfer(&ctx, &read("alice"), "acme", &body(serde_json::json!({ "new_owner": "bob" }))).await.status, 403, "read token can't transfer");
        assert_eq!(api_org_transfer(&ctx, &writ("alice"), "acme", &body(serde_json::json!({ "new_owner": "bob" }))).await.status, 403, "even a write token can't transfer (login-session only)");
        assert_eq!(api_org_delete(&ctx, &read("alice"), "acme").await.status, 403, "read token can't delete");
        assert_eq!(api_org_delete(&ctx, &writ("alice"), "acme").await.status, 403, "even a write token can't delete (login-session only)");

        // A login SESSION (no token) still performs the management actions.
        assert_eq!(api_org_transfer(&ctx, &caller("alice", false), "acme", &body(serde_json::json!({ "new_owner": "bob" }))).await.status, 200, "a login session transfers");
        // bob is now the admin/owner; a login session deletes an (empty) org (204 no content).
        assert_eq!(api_org_delete(&ctx, &caller("bob", false), "acme").await.status, 204, "a login session deletes");
        assert!(ctx.store.org("acme").await.is_none(), "the org is gone");
    }

    // ── password-reset request equalizes work on the miss path (Fix 4) ──

    #[tokio::test]
    async fn password_reset_miss_equalizes_work_and_body_is_identical() {
        let (_d, ctx) = harness().await;
        add_user(&ctx, "alice", "pw-alice-1", false).await;
        let ip: Option<IpAddr> = Some(IpAddr::from([127, 0, 0, 1]));

        // Existing account: a real token is minted (count goes up) and the body is the generic answer.
        let before = ctx.store.password_reset_token_count().await;
        let hit = api_password_reset_request(&ctx, ip, &body(serde_json::json!({ "username": "alice" }))).await;
        assert_eq!(hit.status, 200);
        assert_eq!(ctx.store.password_reset_token_count().await, before + 1, "the hit path mints+keeps a real token");

        // Nonexistent account: the SAME generic body, byte-identical, and the miss path still did the
        // equalizing DB work (a throwaway mint) which it DISCARDED — so no token lingers from it.
        let after_hit = ctx.store.password_reset_token_count().await;
        let miss = api_password_reset_request(&ctx, ip, &body(serde_json::json!({ "username": "ghost-nobody" }))).await;
        assert_eq!(miss.status, 200);
        assert_eq!(miss.body, hit.body, "the miss response is byte-identical to the hit response (no enumeration via the body)");
        assert_eq!(ctx.store.password_reset_token_count().await, after_hit, "the miss path's throwaway token is discarded, leaving no lingering row");

        // The per-IP rate limit is charged BEFORE the lookup: drain the bucket, then even a well-formed
        // request (existing OR nonexistent user) is a 429 — the throttle does not depend on existence.
        let flood_ip: Option<IpAddr> = Some(IpAddr::from([10, 0, 0, 9]));
        let mut saw_429 = false;
        for _ in 0..(REGISTER_BURST as usize + 2) {
            let r = api_password_reset_request(&ctx, flood_ip, &body(serde_json::json!({ "username": "alice" }))).await;
            if r.status == 429 {
                saw_429 = true;
                break;
            }
        }
        assert!(saw_429, "the per-IP budget is charged before the lookup and eventually 429s");
    }

    // ── org overview + code-repo index (hub views) ──

    /// Commit one or more session transcripts into a bare agent repo at
    /// `sessions/<env>/<runtime>/<id>.jsonl`, each carrying a `cwd`. `unix` is the committer timestamp,
    /// so callers can order envs deterministically (newest-first). One commit per call, chained onto the
    /// current HEAD (so repeated calls accumulate history). Pure plumbing over a scratch index; the
    /// committer identity is passed per-invocation via env vars, never global config.
    fn seed_sessions(repo: &std::path::Path, unix: i64, files: &[(&str, &str, &str, &str)]) {
        use std::process::Command;
        let idx = repo.join("seed-index");
        let _ = std::fs::remove_file(&idx);
        // Start the scratch index from the current HEAD tree (if any), so earlier sessions survive.
        // `--verify` is required: a bare `rev-parse HEAD` succeeds on an unborn branch (prints "HEAD").
        if git(repo, &["rev-parse", "--verify", "HEAD"]).is_some() {
            let ok = Command::new("git").arg("-C").arg(repo).env("GIT_INDEX_FILE", &idx).args(["read-tree", "HEAD"]).status().unwrap().success();
            assert!(ok, "read-tree HEAD");
        }
        for (env, runtime, id, cwd) in files {
            let jsonl = format!("{{\"type\":\"user\",\"sessionId\":\"{id}\",\"cwd\":\"{cwd}\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n");
            let tmp = repo.join(format!("seed-{id}.jsonl"));
            std::fs::write(&tmp, jsonl).unwrap();
            let out = Command::new("git").arg("-C").arg(repo).args(["hash-object", "-w"]).arg(&tmp).output().unwrap();
            assert!(out.status.success(), "hash-object");
            let sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
            let _ = std::fs::remove_file(&tmp);
            let path = format!("sessions/{env}/{runtime}/{id}.jsonl");
            let ok = Command::new("git")
                .arg("-C")
                .arg(repo)
                .env("GIT_INDEX_FILE", &idx)
                .args(["update-index", "--add", "--cacheinfo", &format!("100644,{sha},{path}")])
                .status()
                .unwrap()
                .success();
            assert!(ok, "update-index {path}");
        }
        let out = Command::new("git").arg("-C").arg(repo).env("GIT_INDEX_FILE", &idx).args(["write-tree"]).output().unwrap();
        assert!(out.status.success(), "write-tree");
        let tree = String::from_utf8(out.stdout).unwrap().trim().to_string();
        let mut args: Vec<String> = vec!["commit-tree".into(), tree, "-m".into(), "seed sessions".into()];
        if let Some(head) = git(repo, &["rev-parse", "--verify", "HEAD"]) {
            args.push("-p".into());
            args.push(head.trim().to_string());
        }
        let date = format!("{unix} +0000");
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .env("GIT_AUTHOR_NAME", "seed")
            .env("GIT_AUTHOR_EMAIL", "seed@test.local")
            .env("GIT_COMMITTER_NAME", "seed")
            .env("GIT_COMMITTER_EMAIL", "seed@test.local")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .args(args.iter().map(|s| s.as_str()))
            .output()
            .unwrap();
        assert!(out.status.success(), "commit-tree: {}", String::from_utf8_lossy(&out.stderr));
        let commit = String::from_utf8(out.stdout).unwrap().trim().to_string();
        let ok = Command::new("git").arg("-C").arg(repo).args(["update-ref", "refs/heads/main", &commit]).status().unwrap().success();
        assert!(ok, "update-ref");
        let _ = std::fs::remove_file(&idx);
    }

    /// The env slug of every repo row in a /api/repos response.
    fn repo_envs(v: &serde_json::Value) -> Vec<String> {
        v["repos"].as_array().unwrap().iter().map(|r| r["env"].as_str().unwrap().to_string()).collect()
    }

    /// The `owner/name` of every agent listed in an org overview response.
    fn overview_agent_ids(v: &serde_json::Value) -> Vec<String> {
        v["agents"].as_array().unwrap().iter().map(|a| format!("{}/{}", a["owner"].as_str().unwrap(), a["name"].as_str().unwrap())).collect()
    }

    /// ACL LEAK (the security core): neither an org-owned PRIVATE agent nor a member's PRIVATE personal
    /// agent may leak into /api/repos for an anonymous caller; a PUBLIC agent does. Filtering happens
    /// before any counting, so the hidden agents contribute no env, no agent row, and no session count.
    #[tokio::test]
    async fn repos_never_leak_private_agents_to_anonymous() {
        let (_d, ctx) = harness().await;
        mk_org(&ctx, "acme", &[("alice", "admin")]).await;
        add_user(&ctx, "alice", "alice-password-1", false).await;

        // Org-owned PRIVATE (env priv-org), alice PRIVATE personal (env priv-me), org-owned PUBLIC (env pub).
        create_agent(&ctx.store, "org-secret", "org:acme", Visibility::Private).await.unwrap();
        create_agent(&ctx.store, "alice-diary", "alice", Visibility::Private).await.unwrap();
        create_agent(&ctx.store, "org-pub", "org:acme", Visibility::Public).await.unwrap();
        seed_sessions(&repo_path(ctx.root(), "acme", "org-secret"), 1_700_000_000, &[("env-priv-org", "claude-code", "s1", "/srv/secret")]);
        seed_sessions(&repo_path(ctx.root(), "alice", "alice-diary"), 1_700_000_100, &[("env-priv-me", "claude-code", "s2", "/home/alice/diary")]);
        seed_sessions(&repo_path(ctx.root(), "acme", "org-pub"), 1_700_000_200, &[("env-pub", "claude-code", "s3", "/srv/pub")]);

        let v = json_of(&api_repos(&ctx, &Caller::anonymous()).await);
        let envs = repo_envs(&v);
        assert_eq!(envs, vec!["env-pub"], "anonymous sees ONLY the public agent's env; both private envs are absent: {envs:?}");
        // The public row carries the public agent and nothing else - no hidden agent folded in.
        let pub_row = &v["repos"].as_array().unwrap()[0];
        assert_eq!(pub_row["total_sessions"], 1);
        let agents = pub_row["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["owner"], "org:acme");
        assert_eq!(agents[0]["name"], "org-pub");
    }

    /// The org overview is ACL-filtered too: a fellow MEMBER never sees another member's PRIVATE personal
    /// agent (personal agents do not inherit org grants), while the org-owned agents do surface.
    #[tokio::test]
    async fn overview_hides_a_members_private_personal_agent_from_other_members() {
        let (_d, ctx) = harness().await;
        mk_org(&ctx, "acme", &[("alice", "admin"), ("bob", "member")]).await;
        add_user(&ctx, "alice", "alice-password-1", false).await;
        add_user(&ctx, "bob", "bob-password-1", false).await;

        create_agent(&ctx.store, "org-pub", "org:acme", Visibility::Public).await.unwrap();
        create_agent(&ctx.store, "alice-diary", "alice", Visibility::Private).await.unwrap();
        seed_sessions(&repo_path(ctx.root(), "acme", "org-pub"), 1_700_000_000, &[("env-pub", "claude-code", "s1", "/srv/pub")]);
        seed_sessions(&repo_path(ctx.root(), "alice", "alice-diary"), 1_700_000_100, &[("env-me", "claude-code", "s2", "/home/alice/diary")]);

        // bob is a member, so he passes the org gate; alice's private personal agent still stays hidden.
        let v = json_of(&api_org_overview(&ctx, &caller("bob", false), "acme").await);
        let ids = overview_agent_ids(&v);
        assert!(ids.contains(&"org:acme/org-pub".to_string()), "the org-owned public agent surfaces: {ids:?}");
        assert!(!ids.contains(&"alice/alice-diary".to_string()), "a member's private personal agent must NOT leak to another member: {ids:?}");
    }

    /// ORG 404: the overview gate is byte-identical for a missing org and one the caller may not see, so
    /// membership cannot be probed. A non-member gets the SAME 404 (status AND body) as a missing org.
    #[tokio::test]
    async fn overview_404_for_non_member_is_byte_identical_to_missing_org() {
        let (_d, ctx) = harness().await;
        mk_org(&ctx, "acme", &[("alice", "admin")]).await;
        add_user(&ctx, "bob", "bob-password-1", false).await;

        let non_member = api_org_overview(&ctx, &caller("bob", false), "acme").await;
        let missing = api_org_overview(&ctx, &caller("bob", false), "ghost-org").await;
        assert_eq!(non_member.status, 404, "a non-member is refused with 404, never 403 or a body");
        assert_eq!(missing.status, 404);
        assert_eq!(non_member.body, missing.body, "non-member 404 is byte-identical to a missing-org 404 (non-disclosure)");
        // And an anonymous caller is likewise 404, never a leak of the member list.
        assert_eq!(api_org_overview(&ctx, &Caller::anonymous(), "acme").await.status, 404);
    }

    /// AGGREGATION: an env touched by TWO agents appears ONCE in /api/repos with both agents listed and
    /// their sessions SUMMED; and the org overview surfaces both an org-owned agent and a readable member
    /// personal agent (tagged personal).
    #[tokio::test]
    async fn repos_aggregates_two_agents_on_one_env_and_overview_lists_both_kinds() {
        let (_d, ctx) = harness().await;
        mk_org(&ctx, "acme", &[("alice", "admin")]).await;
        add_user(&ctx, "alice", "alice-password-1", false).await;

        // Two agents share ONE env slug with the same cwd: an org-owned agent (2 sessions) and alice's
        // public personal agent (1 session).
        create_agent(&ctx.store, "frontend", "org:acme", Visibility::Public).await.unwrap();
        create_agent(&ctx.store, "api", "alice", Visibility::Public).await.unwrap();
        seed_sessions(
            &repo_path(ctx.root(), "acme", "frontend"),
            1_700_000_000,
            &[("env-app", "claude-code", "f1", "/home/alice/proj/app"), ("env-app", "claude-code", "f2", "/home/alice/proj/app")],
        );
        seed_sessions(&repo_path(ctx.root(), "alice", "api"), 1_700_000_500, &[("env-app", "claude-code", "a1", "/home/alice/proj/app")]);

        // /api/repos (anonymous, both agents public): env-app appears ONCE, both agents, summed sessions.
        let v = json_of(&api_repos(&ctx, &Caller::anonymous()).await);
        let repos = v["repos"].as_array().unwrap();
        let app: Vec<&serde_json::Value> = repos.iter().filter(|r| r["env"] == "env-app").collect();
        assert_eq!(app.len(), 1, "the shared env appears exactly once: {repos:?}");
        let row = app[0];
        assert_eq!(row["total_sessions"], 3, "sessions are summed across both agents (2 + 1)");
        assert_eq!(row["cwd"], "/home/alice/proj/app", "the representative cwd comes from the newest session's transcript");
        let names: std::collections::HashSet<String> =
            row["agents"].as_array().unwrap().iter().map(|a| format!("{}/{}", a["owner"].as_str().unwrap(), a["name"].as_str().unwrap())).collect();
        assert!(names.contains("org:acme/frontend"), "the org agent is attached: {names:?}");
        assert!(names.contains("alice/api"), "the personal agent is attached: {names:?}");
        // Per-agent session counts are preserved.
        for a in row["agents"].as_array().unwrap() {
            let want = if a["name"] == "frontend" { 2 } else { 1 };
            assert_eq!(a["sessions"], want, "per-agent session count for {}", a["name"]);
        }

        // Org overview (as admin alice) surfaces BOTH the org-owned agent and alice's readable personal
        // agent, the personal one tagged.
        let ov = json_of(&api_org_overview(&ctx, &caller("alice", false), "acme").await);
        let agents = ov["agents"].as_array().unwrap();
        let frontend = agents.iter().find(|a| a["owner"] == "org:acme" && a["name"] == "frontend").expect("org-owned agent in overview");
        assert_eq!(frontend["personal"], false, "an org-owned agent is not personal");
        assert_eq!(frontend["sessions"], 2);
        assert!(frontend["environments"].as_array().unwrap().iter().any(|e| e == "env-app"), "env slug surfaces on the org agent");
        let api = agents.iter().find(|a| a["owner"] == "alice" && a["name"] == "api").expect("member personal agent in overview");
        assert_eq!(api["personal"], true, "a member's personal agent is tagged personal");
        assert_eq!(api["sessions"], 1);
    }
}
