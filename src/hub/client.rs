//! HTTP client implementation.
//!
//! Every call to the hub goes through here, so the auth header, the timeouts and the error
//! wording exist in exactly one place.

use super::*;
use crate::Result;
use crate::infra::config;
use crate::infra::credentials;
use anyhow::Context;
use std::time::Duration;

/// A response the hub answered explicitly, with an error status.
///
/// # Why the server's wording is carried through
///
/// A hardcoded explanation per status code turns 409 into "that name is taken (409). Pick
/// another name." — right for `POST /api/agents`, thoroughly misleading anywhere else, and the
/// misleading one is usually the one the user most needs to understand.
///
/// The server already puts `error` (human wording) and `kind` (a machine-readable category) in
/// the body; relaying that verbatim is more accurate than guessing the semantics client-side.
/// The status code stays in the type because it goes into the sentence the user sees.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    /// The server's machine-readable category: `not_found` / `conflict` / `provenance_rejected` ...
    pub kind: String,
    /// The server's human-readable wording. Empty when the body cannot be read.
    pub detail: String,
    base: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Say what the server said first, then what you can do about it. The order is
        // deliberate: a status code means nothing to the user, the server's sentence does.
        let said = if self.detail.trim().is_empty() {
            format!("the hub returned HTTP {}", self.status)
        } else {
            self.detail.trim().to_string()
        };
        write!(f, "{said}")?;
        // 404 carries a special meaning in this product: a private agent must be
        // indistinguishable from "does not exist" to a user without access (otherwise agent
        // names can be enumerated), so the wording deliberately does not separate the two.
        let hint = match self.status {
            401 => Some("log in again with `agit login`".to_string()),
            404 => {
                Some("it may not exist, or it may be a private agent you cannot access".to_string())
            }
            _ => None,
        };
        if let Some(h) = hint {
            write!(f, "\n  {h}")?;
        }
        write!(f, "\n  current hub: {}", self.base)
    }
}

impl std::error::Error for ApiError {}

/// One row of the collaborator list.
///
/// Defined at module level rather than inside the method so a test can pin "this is the element
/// of a bare array".
#[derive(Deserialize)]
struct Collaborator {
    username: String,
    role: String,
}

/// The body of a hub error response. One-to-one with the hub's `infra::error::Body`.
#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: String,
    #[serde(default)]
    kind: String,
}

pub struct Client {
    base: String,
    /// The current access token.
    ///
    /// `RefCell` because every request method takes `&self`, while a 401 has to swap in a new
    /// one (refresh) in place. Changing the signature to `&mut self` forces every call site to
    /// declare mut over a mutability that is purely an internal detail.
    token: std::cell::RefCell<Option<String>>,
    /// The credentials a 401 exchanges for a new access token — **this client's own**, not the
    /// "current hub" ones. `logout --all` builds one client per hub, and the wrong refresh token
    /// misreads a live server-side session as expired. `None` = this client does not refresh
    /// (the login flow, the PAT flow).
    cred: std::cell::RefCell<Option<credentials::HubCredential>>,
    agent: ureq::Agent,
}

/// After a failed exchange, wait for a sibling process to persist the new pair: each wait is
/// this long ...
const REFRESH_SETTLE_STEP: Duration = Duration::from_millis(100);
/// ... and there are at most this many of them. The total has to cover the window where the
/// server has already rotated but the local write has not finished, without holding a command
/// up for long on a credential that is genuinely dead.
const REFRESH_SETTLE_POLLS: usize = 20;

/// The in-process renewal gate: one client exchanges a token at a time (see
/// [`Client::try_refresh`]).
fn refresh_gate() -> &'static std::sync::Mutex<()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &GATE
}

impl Client {
    /// Build from the environment and the credentials file. The single construction entry point.
    pub fn from_env() -> Client {
        let cfg = ureq::Agent::config_builder()
            // A timeout is mandatory: hanging on an unresponsive hub buys nothing, and the
            // user reads it as agit being dead.
            .timeout_global(Some(Duration::from_secs(30)))
            // 4xx/5xx are not transport errors: by default ureq turns a status code into
            // `Error::StatusCode`, and that variant **drops the response body** — the refusal
            // the server wrote into the body disappears with it. With it off, status codes go
            // through [`Client::decode`] and the body reaches the user.
            .http_status_as_error(false)
            .build();
        let cred = credentials::current();
        Client {
            base: config::hub_url(),
            token: std::cell::RefCell::new(cred.as_ref().map(|c| c.access_token.clone())),
            cred: std::cell::RefCell::new(cred),
            agent: cfg.into(),
        }
    }

    /// A named hub plus a whole credential: requests carry its access token, and a 401 renews
    /// with its own refresh token and stores the new pair back under **its** hub.
    pub fn for_credential(hub: &str, cred: &credentials::HubCredential) -> Client {
        let mut c = Client::from_env();
        c.base = hub.trim_end_matches('/').to_string();
        *c.token.borrow_mut() = Some(cred.access_token.clone());
        *c.cred.borrow_mut() = Some(cred.clone());
        c
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// A named hub with no credentials (the constructor for the browser / device login flow).
    pub fn for_hub(hub: &str) -> Client {
        let mut c = Client::from_env();
        c.base = hub.trim_end_matches('/').to_string();
        *c.token.borrow_mut() = None;
        *c.cred.borrow_mut() = None;
        c
    }

    /// A named hub plus a ready-made token (the constructor for `login --with-token`).
    pub fn for_hub_with_token(hub: &str, token: &str) -> Client {
        let mut c = Client::from_env();
        c.base = hub.trim_end_matches('/').to_string();
        *c.token.borrow_mut() = Some(token.to_string());
        // A bare token has no matching refresh token; refreshing with the "current hub" one
        // renews the wrong account.
        *c.cred.borrow_mut() = None;
        c
    }

    /// Exchange a PAT for a proper pair of session tokens.
    pub fn login_with_pat(&self, pat: &str) -> Result<LoginResponse> {
        self.post_public("api/auth/login", &serde_json::json!({ "token": pat }))
    }

    pub fn has_token(&self) -> bool {
        self.token.borrow().is_some()
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.with_retry(path, |t| {
            let mut req = self.agent.get(self.url(path));
            if let Some(t) = t {
                req = req.header("Authorization", &format!("Bearer {t}"));
            }
            req.call()
        })
    }

    fn post<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.with_retry(path, |t| {
            let mut req = self.agent.post(self.url(path));
            if let Some(t) = t {
                req = req.header("Authorization", &format!("Bearer {t}"));
            }
            req.send_json(body)
        })
    }

    /// Send the request; on a 401, exchange the refresh token for a new access token and retry
    /// once.
    ///
    /// Exactly **one** retry: a 401 after the exchange means the refresh token is expired too,
    /// and retrying past that only fills the server log. The error surfaces there and the user
    /// signs in again.
    ///
    /// An access token is valid for one hour, so this path is walked constantly — it is where
    /// "one sign-in lasts a month" is implemented.
    fn with_retry<T, F>(&self, path: &str, send: F) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
        F: Fn(Option<&str>) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    {
        let mut resp = {
            let t = self.token.borrow();
            send(t.as_deref())
        }
        .map_err(|e| self.explain(e, path))?;

        // A status code is no longer a transport error (see why `from_env` turns
        // `http_status_as_error` off), so the 401 is tested here and not in the `Err` arm.
        if resp.status() == 401 && self.try_refresh() {
            let t = self.token.borrow();
            resp = send(t.as_deref()).map_err(|e| self.explain(e, path))?;
        }

        self.decode(resp, path)
    }

    /// Turn the response into a result: a non-2xx becomes an [`ApiError`] carrying the
    /// server's wording.
    fn decode<T: serde::de::DeserializeOwned>(
        &self,
        mut resp: ureq::http::Response<ureq::Body>,
        path: &str,
    ) -> Result<T> {
        if !resp.status().is_success() {
            return Err(self.api_error(resp, path));
        }
        resp.body_mut()
            .read_json::<T>()
            .with_context(|| format!("the JSON returned by hub {path} could not be parsed"))
    }

    /// A request that only cares whether it succeeded (DELETE / logout).
    fn expect_ok(&self, resp: ureq::http::Response<ureq::Body>, path: &str) -> Result<()> {
        if resp.status().is_success() {
            return Ok(());
        }
        Err(self.api_error(resp, path))
    }

    fn api_error(&self, mut resp: ureq::http::Response<ureq::Body>, path: &str) -> anyhow::Error {
        let status = resp.status().as_u16();
        // Left empty when the body cannot be read (not JSON, rewritten by a proxy). Display
        // then falls back to "the hub returned HTTP <code>" — imprecise, but more honest than
        // inventing an explanation.
        let (kind, detail) = match resp.body_mut().read_json::<ErrorBody>() {
            Ok(b) => (b.kind, b.error),
            Err(_) => (
                String::new(),
                format!("the hub returned HTTP {status} ({path})"),
            ),
        };
        anyhow::Error::new(ApiError {
            status,
            kind,
            detail,
            base: self.base.clone(),
        })
    }

    /// Exchange for a new access token on demand.
    ///
    /// For [`super::git`]: a git subprocess that hits a 401 cannot go through `with_retry`
    /// itself (it sends no HTTP request, it only gets a header injected), so it needs an
    /// explicit entry point.
    pub fn refresh_access(&self) -> bool {
        self.try_refresh()
    }

    /// Exchange the refresh token for a new pair and persist it. Returns whether it worked.
    ///
    /// A refresh token is **single-use** (the server replaces the whole row), and one credential
    /// may be read into memory by more than one client: once another client (or another process)
    /// has exchanged first, the refresh token we hold is void and using it again only yields a
    /// 401. So look at the newest credentials on disk for this hub first: different from the
    /// in-memory pair means adopt them and retry; only while disk still holds the same pair do
    /// we exchange ourselves, and after a failed exchange look at disk once more — if someone
    /// rotated it exactly between those two steps, the pair that landed is the answer.
    fn try_refresh(&self) -> bool {
        let Some(cred) = self.cred.borrow().clone() else {
            return false;
        };
        // One client renews at a time in-process: latecomers wait outside the gate, and by the
        // time they get in the one that went first has persisted the new pair, so they adopt
        // it — two clients never spend the same single-use refresh token.
        let _gate = refresh_gate().lock().unwrap_or_else(|e| e.into_inner());
        if self.adopt_newer_from_disk(&cred) {
            return true;
        }
        if cred.refresh_expired() {
            return false;
        }

        // The refresh endpoint carries no auth header — the access token is the expired one.
        let pair: super::TokenPair = match self.post_public(
            "api/auth/refresh",
            &serde_json::json!({ "refresh_token": cred.refresh_token }),
        ) {
            Ok(p) => p,
            Err(_) => return self.wait_for_a_sibling_to_land(&cred),
        };

        let fresh = credentials::HubCredential {
            username: cred.username,
            email: cred.email,
            hub: cred.hub,
            access_token: pair.access_token.clone(),
            access_expires_at: pair.access_expires_at,
            refresh_token: pair.refresh_token,
            refresh_expires_at: pair.refresh_expires_at,
        };
        if credentials::save(&self.base, &fresh).is_err() {
            return false;
        }
        *self.token.borrow_mut() = Some(pair.access_token);
        *self.cred.borrow_mut() = Some(fresh);
        true
    }

    /// If the credentials on disk for this hub are newer than `ours` (a different access token,
    /// refresh not yet expired), switch to them and return true. This holds only for clients
    /// allowed to renew — a bare-token client never gets here.
    ///
    /// **It must be the same account.** The user may have signed in to the same hub as another
    /// account just as this 401 arrived; that pair on disk is also "newer and valid", but
    /// replaying the original request with it puts a different principal behind something the
    /// user started under another identity.
    fn adopt_newer_from_disk(&self, ours: &credentials::HubCredential) -> bool {
        let Some(disk) = credentials::load(&self.base) else {
            return false;
        };
        if disk.username != ours.username
            || disk.access_token == ours.access_token
            || disk.refresh_expired()
        {
            return false;
        }
        *self.token.borrow_mut() = Some(disk.access_token.clone());
        *self.cred.borrow_mut() = Some(disk);
        true
    }

    /// Where a failed exchange of our own converges: a refresh token is single-use, and the
    /// commonest reason an exchange fails is that **another process** just spent it while the
    /// new pair is not written to disk yet. This waits for that write, bounded — adopt it if it
    /// lands, admit failure only if it does not. The bound is [`REFRESH_SETTLE_POLLS`] ×
    /// [`REFRESH_SETTLE_STEP`]; sibling clients inside this process never get here, they are
    /// already serialized outside [`refresh_gate`].
    fn wait_for_a_sibling_to_land(&self, ours: &credentials::HubCredential) -> bool {
        for _ in 0..REFRESH_SETTLE_POLLS {
            if self.adopt_newer_from_disk(ours) {
                return true;
            }
            std::thread::sleep(REFRESH_SETTLE_STEP);
        }
        false
    }

    /// A POST with no auth header.
    ///
    /// The login flow itself cannot carry a token (there is none yet). A separate method rather
    /// than `post`
    /// Pair this machine, exchanging for an RC-only token.
    ///
    /// No new credential system is invented: the caller must already have run `agit login`, and
    /// this only trades the account identity for a long-lived token that can be revoked on its
    /// own and is scoped to the RC surface.
    pub fn rc_pair(
        &self,
        fingerprint: &str,
        display_name: &str,
        platform: &str,
    ) -> Result<crate::hub::RcPairResponse> {
        self.post(
            "api/rc/connections",
            &serde_json::json!({
                "machine_fingerprint": fingerprint,
                "display_name": display_name,
                "platform": platform,
                "agit_version": env!("CARGO_PKG_VERSION"),
            }),
        )
    }

    /// The machines under my account (offline ones included).
    pub fn rc_connections(&self) -> Result<Vec<crate::hub::RcConnection>> {
        self.get("api/rc/connections")
    }

    /// Revoke a machine: disconnect it at once and refuse further registration. The workspaces
    /// under it are **not** deleted — a machine going away does not mean those directories and
    /// session definitions should disappear.
    pub fn rc_revoke(&self, connection: &str) -> Result<()> {
        let _: serde_json::Value = self.post(
            &format!("api/rc/connections/{connection}/revoke"),
            &serde_json::json!({}),
        )?;
        Ok(())
    }

    /// The decision, so that "this call is deliberately unauthenticated" is visible in the code.
    pub fn post_public<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let resp = self
            .agent
            .post(self.url(path))
            .send_json(body)
            .map_err(|e| self.explain(e, path))?;
        self.decode(resp, path)
    }

    fn delete(&self, path: &str) -> Result<()> {
        self.delete_inner(path, None)
    }

    fn delete_expected(&self, path: &str, expected_agent_id: &str) -> Result<()> {
        self.delete_inner(path, Some(expected_agent_id))
    }

    fn delete_inner(&self, path: &str, expected_agent_id: Option<&str>) -> Result<()> {
        let send = |token: Option<&str>| {
            let mut req = self.agent.delete(self.url(path));
            if let Some(expected_agent_id) = expected_agent_id {
                req = req.header(super::identity::EXPECTED_AGENT_ID_HEADER, expected_agent_id);
            }
            if let Some(token) = token {
                req = req.header("Authorization", &format!("Bearer {token}"));
            }
            req.call()
        };
        let mut resp = {
            let token = self.token.borrow();
            send(token.as_deref())
        }
        .map_err(|e| self.explain(e, path))?;
        if resp.status() == 401 && self.try_refresh() {
            let token = self.token.borrow();
            resp = send(token.as_deref()).map_err(|e| self.explain(e, path))?;
        }
        self.expect_ok(resp, path)
    }

    fn patch<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.with_retry(path, |t| {
            let mut req = self.agent.patch(self.url(path));
            if let Some(t) = t {
                req = req.header("Authorization", &format!("Bearer {t}"));
            }
            req.send_json(body)
        })
    }

    fn put<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.with_retry(path, |t| {
            let mut req = self.agent.put(self.url(path));
            if let Some(t) = t {
                req = req.header("Authorization", &format!("Bearer {t}"));
            }
            req.send_json(body)
        })
    }

    // ── PR ─────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn pr_create(
        &self,
        to_owner: &str,
        to_name: &str,
        from_owner: &str,
        from_name: &str,
        expected_source_agent_id: &str,
        branch: &str,
        source_head: &str,
        target_branch: &str,
        message: Option<&str>,
    ) -> Result<super::Pr> {
        self.post(
            &format!("api/agents/{to_owner}/{to_name}/prs"),
            &pr_create_request(
                from_owner,
                from_name,
                expected_source_agent_id,
                branch,
                source_head,
                target_branch,
                message,
            ),
        )
    }

    pub fn pr_list(&self, owner: &str, name: &str) -> Result<Vec<super::Pr>> {
        self.get(&format!("api/agents/{owner}/{name}/prs"))
    }

    pub fn pr_get(&self, id: u64) -> Result<super::Pr> {
        self.get(&format!("api/prs/{id}"))
    }

    pub fn pr_merge(&self, id: u64, adopt: Option<&str>) -> Result<super::PrMergeResponse> {
        self.post(
            &format!("api/prs/{id}/merge"),
            &serde_json::json!({ "adopt": adopt }),
        )
    }

    // ── repo governance ────────────────────────────────────────────

    /// Change visibility (owner only).
    pub fn set_visibility(
        &self,
        owner: &str,
        name: &str,
        public: bool,
        expected_agent_id: &str,
    ) -> Result<()> {
        let _: serde_json::Value = self.patch(
            &format!("api/agents/{owner}/{name}"),
            &serde_json::json!({
                "visibility": if public { "public" } else { "private" },
                "expected_agent_id": expected_agent_id,
            }),
        )?;
        Ok(())
    }

    /// The pre-publish scan.
    ///
    /// `expected_agent_id` is the immutable identity, not a slug: `owner/name` can be deleted
    /// and rebuilt under the same name, and what this command makes public is **the object the
    /// local checkout points at**. Without it, the scan and the publish that follows land on a
    /// different repo of the same name — a GET precheck cannot close that window, the test has
    /// to travel with the request into the server's own lock.
    pub fn prepare_public_visibility(
        &self,
        owner: &str,
        name: &str,
        expected_agent_id: &str,
    ) -> Result<super::PreparePublicResponse> {
        self.post(
            &format!("api/agents/{owner}/{name}/visibility/public/prepare"),
            &serde_json::json!({ "expected_agent_id": expected_agent_id }),
        )
    }

    /// Confirm and go public. `expected_agent_id` has the same source as in prepare; the
    /// reasoning is there.
    pub fn confirm_public_visibility(
        &self,
        owner: &str,
        name: &str,
        intent_id: &str,
        confirmation: &str,
        accept_secret_findings: bool,
        expected_agent_id: &str,
    ) -> Result<super::RemoteAgent> {
        self.post(
            &format!("api/agents/{owner}/{name}/visibility/public/confirm"),
            &serde_json::json!({
                "intent_id": intent_id,
                "confirmation": confirmation,
                "accept_secret_findings": accept_secret_findings,
                "expected_agent_id": expected_agent_id,
            }),
        )
    }

    /// Rename on the remote (carries the far side's human wording when the server does not
    /// support it).
    pub fn rename_agent(
        &self,
        owner: &str,
        name: &str,
        new_name: &str,
        expected_agent_id: &str,
    ) -> Result<()> {
        let _: serde_json::Value = self.patch(
            &format!("api/agents/{owner}/{name}"),
            &serde_json::json!({
                "name": new_name,
                "expected_agent_id": expected_agent_id,
            }),
        )?;
        Ok(())
    }

    /// Delete on the remote.
    pub fn delete_agent(&self, owner: &str, name: &str, expected_agent_id: &str) -> Result<()> {
        self.delete_expected(&format!("api/agents/{owner}/{name}"), expected_agent_id)
    }

    /// The collaborator list. Returns (user, role).
    ///
    /// The server answers with a **bare array**, not `{"collaborators": [...]}`. Decoding one
    /// wrapper layer too many makes `agit repo collab list` report "invalid type: map, expected
    /// a sequence" — serde's internal wording, which never mentions collaborators at all.
    pub fn list_collaborators(&self, owner: &str, name: &str) -> Result<Vec<(String, String)>> {
        let rows: Vec<Collaborator> =
            self.get(&format!("api/agents/{owner}/{name}/collaborators"))?;
        Ok(rows.into_iter().map(|c| (c.username, c.role)).collect())
    }

    pub fn add_collaborator(
        &self,
        owner: &str,
        name: &str,
        user: &str,
        role: &str,
        expected_agent_id: &str,
    ) -> Result<()> {
        let _: serde_json::Value = self.put(
            &format!("api/agents/{owner}/{name}/collaborators/{user}"),
            &serde_json::json!({
                "role": role,
                "expected_agent_id": expected_agent_id,
            }),
        )?;
        Ok(())
    }

    pub fn remove_collaborator(
        &self,
        owner: &str,
        name: &str,
        user: &str,
        expected_agent_id: &str,
    ) -> Result<()> {
        self.delete_expected(
            &format!("api/agents/{owner}/{name}/collaborators/{user}"),
            expected_agent_id,
        )
    }

    /// Translate a **transport-layer** error into something useful to the user.
    ///
    /// Only unreachable, timed out and TLS problems are left — status codes go through
    /// [`Client::api_error`]. A raw `ureq` error ("Connection refused") gives the user nothing
    /// to act on; what they need is which address they are connecting to and how to change it.
    fn explain(&self, e: ureq::Error, _path: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "cannot reach hub {}: {e}\n  \
             Is the hub running? Point AGIT_HUB_URL at another address.",
            self.base
        )
    }

    // ── API ─────────────────────────────────────────────────────────

    pub fn health(&self) -> Result<Health> {
        self.get("api/health")
    }

    /// "What the newest version is right now" — the source for self-upgrade. A failure must not
    /// block the main flow: the caller decides whether this Err is swallowed (an incidental
    /// hint) or propagated (agit upgrade).
    pub fn cli_version(&self) -> Result<CliVersion> {
        self.get("api/cli/version")
    }

    pub fn reachable(&self) -> bool {
        self.health().is_ok()
    }

    /// "Who this token is on the server" — the one call that exists only to verify credentials.
    ///
    /// `health` is a public endpoint: a forged or revoked token still gets a 200 out of it. Only
    /// an endpoint that requires authentication can answer whether the credentials are still
    /// valid.
    pub fn me(&self) -> Result<super::Me> {
        self.get("api/auth/me")
    }

    /// Whether `<owner>/<name>` can be pushed to.
    ///
    /// It asks the first step of a push — `info/refs?service=git-receive-pack` — which already
    /// has to pass the hub's write gate (an org owner and a team member granted this agent both
    /// fall under that one decision). The CLI does not infer from "which orgs I am in": org
    /// membership and "can write this agent" are different things. This GET reads a status code
    /// and changes nothing.
    ///
    /// Behind the write gate sits an identity fence: whoever can write must also carry
    /// Expected-Agent-Id — missing is 428, mismatched is 412. The probe asks "can I write", not
    /// "is the local checkout this one", so it carries the current id the hub just answered
    /// with; the fence's question is compared by push itself before it pushes.
    pub fn push_access(
        &self,
        owner: &str,
        name: &str,
        expected_agent_id: &str,
    ) -> Result<super::PushAccess> {
        let path = format!("{owner}/{name}.git/info/refs?service=git-receive-pack");
        let status = self.status_of(&path, Some(expected_agent_id))?;
        super::PushAccess::from_status(status).ok_or_else(|| {
            anyhow::anyhow!(
                "{} answered {status} to the push-access probe for {owner}/{name}",
                self.base
            )
        })
    }

    /// My identity in this org; 404 when I am not a member (or the org does not exist).
    pub fn get_org(&self, name: &str) -> Result<super::OrgView> {
        self.get(&format!("api/orgs/{name}"))
    }

    /// The status code only. On a 401 it exchanges the token once and retries, like
    /// [`Self::with_retry`]; a second 401 is reported as expired credentials, not as "no
    /// permission".
    fn status_of(&self, path: &str, expected_agent_id: Option<&str>) -> Result<u16> {
        let send = |t: Option<&str>| {
            let mut req = self.agent.get(self.url(path));
            if let Some(t) = t {
                req = req.header("Authorization", &format!("Bearer {t}"));
            }
            if let Some(id) = expected_agent_id {
                req = req.header(super::identity::EXPECTED_AGENT_ID_HEADER, id);
            }
            req.call()
        };
        let mut resp = {
            let t = self.token.borrow();
            send(t.as_deref())
        }
        .map_err(|e| self.explain(e, path))?;
        if resp.status() == 401 && self.try_refresh() {
            let t = self.token.borrow();
            resp = send(t.as_deref()).map_err(|e| self.explain(e, path))?;
        }
        if resp.status() == 401 {
            return Err(self.api_error(resp, path));
        }
        Ok(resp.status().as_u16())
    }

    /// Revoke the current session.
    pub fn logout(&self) -> Result<()> {
        let _: serde_json::Value = self.post("api/auth/logout", &serde_json::json!({}))?;
        Ok(())
    }

    /// Sign in with username and password, exchanging for a pair of tokens.
    pub fn login(&self, username: &str, password: &str) -> Result<LoginResponse> {
        self.post(
            "api/auth/login",
            &LoginRequest {
                username: username.to_string(),
                password: password.to_string(),
            },
        )
    }

    /// List the agents visible to the current user.
    pub fn list_agents(&self) -> Result<Vec<RemoteAgent>> {
        self.get("api/agents")
    }

    pub fn get_agent(&self, owner: &str, name: &str) -> Result<RemoteAgent> {
        self.get(&format!("api/agents/{owner}/{name}"))
    }

    /// **A server-side reverse lookup**: which agents have worked in this code repo.
    ///
    /// This is the entry point for `agit clone` with no arguments, and the server-side duty that
    /// comes with the decision not to write a binding file into the code repo.
    pub fn agents_for_repo(&self, repo_origin: &str) -> Result<Vec<RemoteAgent>> {
        let encoded = urlencode(repo_origin);
        self.get(&format!("api/agents/for-repo?origin={encoded}"))
    }

    /// Publish: create the agent and get the push address.
    pub fn publish(&self, req: &PublishRequest) -> Result<PublishResponse> {
        self.post("api/agents", req)
    }

    /// Copy an agent into your own namespace.
    ///
    /// The server does a `clone --bare`, so the full git history (including the author of every
    /// snapshot) is in the copy. An empty `as_name` keeps the original name.
    pub fn clone_agent(
        &self,
        owner: &str,
        name: &str,
        as_name: Option<&str>,
        expected_source_agent_id: &str,
    ) -> Result<PublishResponse> {
        self.post(
            &format!("api/agents/{owner}/{name}/clone"),
            &clone_agent_request(as_name, expected_source_agent_id),
        )
    }

    pub fn create_share(&self, req: &ShareRequest) -> Result<ShareResponse> {
        self.post("api/shares", req)
    }

    pub fn list_shares(&self) -> Result<Vec<ShareResponse>> {
        self.get("api/shares")
    }

    pub fn revoke_share(&self, slug: &str) -> Result<()> {
        self.delete(&format!("api/shares/{slug}"))
    }

    /// Cross-session search (the legacy shape, a bare array).
    ///
    /// Kept because **an already-released CLI parses exactly this shape**: `/api/search` returns
    /// an array, and switching to an envelope makes those CLIs fail at the parse step. New code
    /// goes through [`Self::search_page`].
    ///
    /// Permission filtering happens on the server, and it has to be a query condition rather
    /// than a post-filter — a post-filter lets the hit count itself leak the existence of
    /// content the user cannot read.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        self.get(&format!("api/search?q={}&limit={limit}", urlencode(query)))
    }

    /// Search by category, with a paging envelope.
    ///
    /// `kind` is `sessions` / `agents` / `prs` / `people`. The type parameter decides how
    /// `items` is parsed: a caller that picks the wrong one gets a deserialization error rather
    /// than an empty list — which is why nothing is type-erased here.
    pub fn search_page<T: serde::de::DeserializeOwned>(
        &self,
        kind: &str,
        query: &str,
        sort: Option<&str>,
        page: usize,
        per: usize,
    ) -> Result<SearchPage<T>> {
        let mut path = format!("api/search/{kind}?q={}", urlencode(query));
        if let Some(s) = sort {
            path.push_str(&format!("&sort={}", urlencode(s)));
        }
        if page > 1 {
            path.push_str(&format!("&page={page}"));
        }
        if per > 0 {
            path.push_str(&format!("&per={per}"));
        }
        self.get(&path)
    }

    /// How many hits each of the four categories has.
    ///
    /// The session number means actually scanning the corpus, so this costs more than a
    /// single-category request — do not call it in a loop.
    pub fn search_counts(&self, query: &str) -> Result<SearchCounts> {
        self.get(&format!("api/search/counts?q={}", urlencode(query)))
    }
}

fn clone_agent_request(as_name: Option<&str>, expected_source_agent_id: &str) -> serde_json::Value {
    serde_json::json!({
        "name": as_name,
        "expected_source_agent_id": expected_source_agent_id,
    })
}

#[allow(clippy::too_many_arguments)]
fn pr_create_request(
    source_owner: &str,
    source_name: &str,
    expected_source_agent_id: &str,
    source_branch: &str,
    source_head: &str,
    target_branch: &str,
    message: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "source_owner": source_owner,
        "source_name": source_name,
        "expected_source_agent_id": expected_source_agent_id,
        "source_branch": source_branch,
        "source_head": source_head,
        "target_branch": target_branch,
        "message": message,
    })
}

impl Default for Client {
    fn default() -> Self {
        Self::from_env()
    }
}

/// Minimal URL encoding.
///
/// Hand-written rather than pulling in the urlencoding crate: it is used on two query parameters
/// and the logic is a few lines. RFC 3986's unreserved set is kept as is, everything else is
/// percent-encoded byte by byte (correct for UTF-8).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(base: &str) -> Client {
        Client {
            base: base.into(),
            token: std::cell::RefCell::new(None),
            cred: std::cell::RefCell::new(None),
            agent: ureq::Agent::new_with_defaults(),
        }
    }

    /// A fake hub: accepts `n` connections in order, reads one HTTP/1.1 request from each and
    /// lets `answer` decide the reply; collects the request text it saw, in order.
    /// `Connection: close` makes ureq reconnect for every request, so "the nth connection" is
    /// "the nth request".
    fn fake_hub(
        n: usize,
        answer: impl Fn(&str) -> (u16, String) + Send + 'static,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..n {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, body_len) = loop {
                    let k = sock.read(&mut chunk).unwrap();
                    if k == 0 {
                        break (buf.len(), 0);
                    }
                    buf.extend_from_slice(&chunk[..k]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (pos + 4, len);
                    }
                };
                while buf.len() < head_end + body_len {
                    let k = sock.read(&mut chunk).unwrap();
                    if k == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..k]);
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                let (status, body) = answer(&req);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
                seen.push(req);
            }
            seen
        });
        (base, handle)
    }

    /// `logout --all` builds one client per hub; after a 401 the renewal must use **that hub's
    /// own** refresh token, and the new pair it yields must be stored back under that hub.
    #[test]
    fn a_client_refreshes_with_its_own_hub_credential() {
        let (base, hub) = fake_hub(3, |req| {
            let first = req.lines().next().unwrap_or("").to_string();
            if first.starts_with("POST /api/auth/refresh") {
                assert!(req.contains("rt-other-hub"), "{req}");
                return (
                    200,
                    r#"{"access_token":"at-new","access_expires_at":"2099-01-01T00:00:00Z","refresh_token":"rt-new","refresh_expires_at":"2099-02-01T00:00:00Z"}"#.into(),
                );
            }
            assert!(first.starts_with("POST /api/auth/logout"), "{first}");
            if req.contains("Bearer at-new") {
                (200, "{}".into())
            } else {
                (401, r#"{"error":"expired","kind":"unauthorized"}"#.into())
            }
        });

        let _env = crate::infra::config::env_lock();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("AGIT_HOME");
        unsafe {
            std::env::set_var("AGIT_HOME", home.path());
        }
        let cred = credentials::HubCredential {
            username: "alice".into(),
            email: None,
            hub: Some(base.clone()),
            access_token: "at-old".into(),
            access_expires_at: "2000-01-01T00:00:00Z".into(),
            refresh_token: "rt-other-hub".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let client = Client::for_credential(&base, &cred);
        let result = client.logout();
        let saved = credentials::load(&base);
        unsafe {
            match old_home {
                Some(v) => std::env::set_var("AGIT_HOME", v),
                None => std::env::remove_var("AGIT_HOME"),
            }
        }
        result.unwrap();
        let seen = hub.join().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen[0].contains("Bearer at-old"), "{}", seen[0]);
        assert!(seen[1].starts_with("POST /api/auth/refresh"), "{}", seen[1]);
        assert!(seen[2].contains("Bearer at-new"), "{}", seen[2]);
        let saved = saved.expect("the refreshed pair is stored under that hub");
        assert_eq!(saved.access_token, "at-new");
        assert_eq!(saved.refresh_token, "rt-new");
        assert_eq!(saved.hub.as_deref(), Some(base.as_str()));
    }

    /// Two clients hold the same old pair: once A has exchanged it, B's refresh token is void —
    /// on its 401 B adopts the new pair A left on disk instead of handing the original 401 back.
    #[test]
    fn a_second_client_adopts_the_pair_its_sibling_already_refreshed() {
        let refreshed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen_by_hub = refreshed.clone();
        let (base, hub) = fake_hub(5, move |req| {
            let first = req.lines().next().unwrap_or("").to_string();
            if first.starts_with("POST /api/auth/refresh") {
                // A refresh token is single-use: the second use of the same one is a 401.
                if seen_by_hub.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return (401, r#"{"error":"used","kind":"unauthorized"}"#.into());
                }
                return (
                    200,
                    r#"{"access_token":"at-new","access_expires_at":"2099-01-01T00:00:00Z","refresh_token":"rt-new","refresh_expires_at":"2099-02-01T00:00:00Z"}"#.into(),
                );
            }
            if req.contains("Bearer at-new") {
                (200, "{}".into())
            } else {
                (401, r#"{"error":"expired","kind":"unauthorized"}"#.into())
            }
        });

        let _env = crate::infra::config::env_lock();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("AGIT_HOME");
        unsafe {
            std::env::set_var("AGIT_HOME", home.path());
        }
        let cred = credentials::HubCredential {
            username: "alice".into(),
            email: None,
            hub: Some(base.clone()),
            access_token: "at-old".into(),
            access_expires_at: "2000-01-01T00:00:00Z".into(),
            refresh_token: "rt-old".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        credentials::save(&base, &cred).unwrap();
        let a = Client::for_credential(&base, &cred);
        let b = Client::for_credential(&base, &cred);
        let ra = a.logout();
        let rb = b.logout();
        unsafe {
            match old_home {
                Some(v) => std::env::set_var("AGIT_HOME", v),
                None => std::env::remove_var("AGIT_HOME"),
            }
        }
        ra.unwrap();
        rb.unwrap();
        let seen = hub.join().unwrap();
        let firsts: Vec<String> = seen
            .iter()
            .map(|r| r.lines().next().unwrap_or("").to_string())
            .collect();
        // A: old token 401 → exchange → new token 200. B: old token 401 → adopt the new pair
        // from disk → new token 200, with **no** second call to refresh in between.
        assert!(firsts[0].starts_with("POST /api/auth/logout"), "{firsts:?}");
        assert!(
            firsts[1].starts_with("POST /api/auth/refresh"),
            "{firsts:?}"
        );
        assert!(
            firsts[2].starts_with("POST /api/auth/logout") && seen[2].contains("Bearer at-new")
        );
        assert!(
            firsts[3].starts_with("POST /api/auth/logout") && seen[3].contains("Bearer at-old")
        );
        assert!(
            firsts[4].starts_with("POST /api/auth/logout") && seen[4].contains("Bearer at-new")
        );
        assert_eq!(seen.len(), 5, "no second refresh attempt");
    }

    fn old_pair(base: &str, username: &str) -> credentials::HubCredential {
        credentials::HubCredential {
            username: username.into(),
            email: None,
            hub: Some(base.to_string()),
            access_token: "at-old".into(),
            access_expires_at: "2000-01-01T00:00:00Z".into(),
            refresh_token: "rt-old".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        }
    }

    fn new_pair(base: &str, username: &str) -> credentials::HubCredential {
        credentials::HubCredential {
            username: username.into(),
            email: None,
            hub: Some(base.to_string()),
            access_token: "at-new".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "rt-new".into(),
            refresh_expires_at: "2099-02-01T00:00:00Z".into(),
        }
    }

    /// Another **process** has just spent this refresh token while the new pair is not
    /// persisted yet: a failed exchange waits for that write and then adopts it, instead of
    /// handing the original 401 back. A thread plays that process here: it writes the new pair
    /// to disk only after the hub has rejected our refresh.
    #[test]
    fn a_client_waits_for_a_sibling_process_to_land_the_new_pair() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let tx = std::sync::Mutex::new(Some(tx));
        let (base, hub) = fake_hub(3, move |req| {
            let first = req.lines().next().unwrap_or("").to_string();
            if first.starts_with("POST /api/auth/refresh") {
                // This token is already spent by someone else — signal that "someone else"
                // to persist the new pair.
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                return (401, r#"{"error":"used","kind":"unauthorized"}"#.into());
            }
            if req.contains("Bearer at-new") {
                (200, "{}".into())
            } else {
                (401, r#"{"error":"expired","kind":"unauthorized"}"#.into())
            }
        });

        let _env = crate::infra::config::env_lock();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("AGIT_HOME");
        unsafe {
            std::env::set_var("AGIT_HOME", home.path());
        }
        let cred = old_pair(&base, "alice");
        credentials::save(&base, &cred).unwrap();
        let sibling_base = base.clone();
        let sibling = std::thread::spawn(move || {
            rx.recv().unwrap();
            std::thread::sleep(REFRESH_SETTLE_STEP * 3);
            credentials::save(&sibling_base, &new_pair(&sibling_base, "alice")).unwrap();
        });
        let result = Client::for_credential(&base, &cred).logout();
        sibling.join().unwrap();
        unsafe {
            match old_home {
                Some(v) => std::env::set_var("AGIT_HOME", v),
                None => std::env::remove_var("AGIT_HOME"),
            }
        }
        result.unwrap();
        let seen = hub.join().unwrap();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert!(seen[1].starts_with("POST /api/auth/refresh"), "{}", seen[1]);
        assert!(seen[2].contains("Bearer at-new"), "{}", seen[2]);
    }

    /// The newer pair on disk belongs to another account: it is not adopted. The user signing
    /// in as someone else does not make this request, started under the old identity,
    /// replayable as that other person.
    #[test]
    fn a_pair_from_another_account_is_never_adopted() {
        let (base, hub) = fake_hub(2, |req| {
            let first = req.lines().next().unwrap_or("").to_string();
            if first.starts_with("POST /api/auth/refresh") {
                return (401, r#"{"error":"used","kind":"unauthorized"}"#.into());
            }
            assert!(
                !req.contains("Bearer at-new"),
                "bob’s token must never be sent: {req}"
            );
            (401, r#"{"error":"expired","kind":"unauthorized"}"#.into())
        });

        let _env = crate::infra::config::env_lock();
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("AGIT_HOME");
        unsafe {
            std::env::set_var("AGIT_HOME", home.path());
        }
        // Disk already holds bob's new pair; the in-memory client still holds alice's old one.
        credentials::save(&base, &new_pair(&base, "bob")).unwrap();
        let result = Client::for_credential(&base, &old_pair(&base, "alice")).logout();
        unsafe {
            match old_home {
                Some(v) => std::env::set_var("AGIT_HOME", v),
                None => std::env::remove_var("AGIT_HOME"),
            }
        }
        let e = result.unwrap_err();
        assert_eq!(e.downcast_ref::<ApiError>().map(|a| a.status), Some(401));
        assert_eq!(hub.join().unwrap().len(), 2);
    }

    /// A bare token has no refresh token to use: a 401 stays a 401, and no other hub's
    /// credentials are used to refresh it.
    #[test]
    fn a_bare_token_client_never_refreshes() {
        let (base, hub) = fake_hub(1, |_| {
            (401, r#"{"error":"nope","kind":"unauthorized"}"#.into())
        });
        let client = Client::for_hub_with_token(&base, "at-bare");
        let e = client.logout().unwrap_err();
        let api = e.downcast_ref::<ApiError>().expect("an ApiError");
        assert_eq!(api.status, 401);
        assert_eq!(hub.join().unwrap().len(), 1);
    }

    #[test]
    fn url_join_never_doubles_slashes() {
        let c = client("http://h:8177");
        assert_eq!(c.url("api/health"), "http://h:8177/api/health");
        assert_eq!(c.url("/api/health"), "http://h:8177/api/health");
    }

    #[test]
    fn default_base_is_https() {
        // The default is the public hub, so the transport must be TLS — passwords, tokens and
        // whole session transcripts travel this path.
        assert!(
            config::DEFAULT_HUB_URL.starts_with("https://"),
            "the default hub must use TLS: {}",
            config::DEFAULT_HUB_URL
        );
    }

    #[test]
    fn urlencode_handles_paths_and_utf8() {
        // A repo origin is a URL; the `:` and `/` in it must be encoded or they break the
        // query string.
        assert_eq!(
            urlencode("git@github.com:o/r.git"),
            "git%40github.com%3Ao%2Fr.git"
        );
        // A CJK query term is percent-encoded per UTF-8, one byte at a time; the Chinese
        // literal is the fixture exercising multi-byte input, so it stays Chinese.
        assert_eq!(urlencode("缓存"), "%E7%BC%93%E5%AD%98");
        // Unreserved characters are left alone.
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn copying_sends_the_exact_source_identity_with_the_name() {
        let id = "00000000-0000-0000-0000-000000000001";
        let body = clone_agent_request(Some("photo-copy"), id);
        assert_eq!(body["name"], "photo-copy");
        assert_eq!(body["expected_source_agent_id"], id);
        assert_eq!(body.as_object().unwrap().len(), 2);
    }

    #[test]
    fn opening_a_pr_fences_both_the_source_object_and_exact_local_head() {
        let id = "00000000-0000-0000-0000-000000000001";
        let head = "1111111111111111111111111111111111111111";
        let body = pr_create_request(
            "alice",
            "photo-copy",
            id,
            "topic",
            head,
            "main",
            Some("ready"),
        );
        assert_eq!(body["source_owner"], "alice");
        assert_eq!(body["source_name"], "photo-copy");
        assert_eq!(body["expected_source_agent_id"], id);
        assert_eq!(body["source_branch"], "topic");
        assert_eq!(body["source_head"], head);
        assert_eq!(body["target_branch"], "main");
        assert_eq!(body["message"], "ready");
        assert_eq!(body.as_object().unwrap().len(), 7);
    }

    fn api_err(status: u16, kind: &str, detail: &str) -> ApiError {
        ApiError {
            status,
            kind: kind.into(),
            detail: detail.into(),
            base: "http://h".into(),
        }
    }

    #[test]
    fn not_found_message_does_not_reveal_existence() {
        // A private agent must be indistinguishable from "does not exist" to a user without
        // access, otherwise agent names can be enumerated.
        let msg = api_err(404, "not_found", "no such agent").to_string();
        assert!(
            msg.contains("not exist") && msg.contains("private"),
            "the 404 wording must blur the two cases: {msg}"
        );
    }

    /// The server's wording must appear verbatim in the error the user sees.
    ///
    /// This pins a real bug: with a hardcoded explanation per status code, "this name is
    /// revoked, pick a new one" is displayed as "that name is taken. Pick another name." — and
    /// that is exactly the sentence the user most needs to understand.
    #[test]
    fn server_wording_is_relayed_verbatim() {
        let detail = "this name is revoked; re-registering will not bring it back, pick a new one";
        let msg = api_err(409, "conflict", detail).to_string();
        assert!(msg.contains(detail), "{msg}");
        assert!(
            !msg.contains("Pick another name"),
            "no semantics may be invented: {msg}"
        );
        // The hub address always comes along: users connect to the wrong hub often.
        assert!(msg.contains("http://h"), "{msg}");
    }

    #[test]
    fn a_body_less_error_still_reports_the_status() {
        // A response rewritten by a proxy, or a hub answering with non-JSON, must not be silent.
        let msg = api_err(502, "", "").to_string();
        assert!(msg.contains("502"), "{msg}");
    }

    /// The collaborator list is a **bare array**, not `{"collaborators": [...]}`.
    ///
    /// Decoding one wrapper layer too many makes `agit repo collab list` report "invalid type:
    /// map, expected a sequence" — serde's internal wording, which never mentions collaborators
    /// and leaves the reader with no idea which end to look at.
    #[test]
    fn collaborators_arrive_as_a_bare_array() {
        let body = r#"[{"username":"alice","role":"write"},{"username":"bob","role":"read"}]"#;
        let rows: Vec<Collaborator> = serde_json::from_str(body).expect("a bare array must decode");
        let got: Vec<(String, String)> = rows.into_iter().map(|c| (c.username, c.role)).collect();
        assert_eq!(
            got,
            [
                ("alice".to_string(), "write".to_string()),
                ("bob".to_string(), "read".to_string())
            ]
        );
        // The wrapped form must not be accepted — it belongs to a different server contract,
        // and decoding both hides a real mismatch.
        assert!(serde_json::from_str::<Vec<Collaborator>>(r#"{"collaborators":[]}"#).is_err());
    }
}
