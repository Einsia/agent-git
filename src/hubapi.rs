//! A minimal blocking HTTP client to an agit hub's JSON API — the client half agit never had.
//!
//! The client authenticates to a hub for git over HTTP Basic (a token in the password field of the
//! remote URL, `https://user:token@host/owner/name.git`); this module authenticates to the SAME hub the
//! SAME way for the JSON API (`agit identity keys` / `revoke` / `pin`), needing no separate login. The hub base URL and
//! credential are resolved from the active agent's bound remote, with `AGIT_HUB_URL` / `AGIT_HUB_TOKEN`
//! as overrides. No-hub / no-remote is a clear error, never a panic.

use anyhow::{bail, Context, Result};
use std::time::Duration;

/// How long the hub-store probe (`is_hub_store_url`) waits before giving up. A POSITIVE-identification
/// probe must never hang a `git clone`: unreachable/slow host → treat as NOT a hub and fall through to
/// passthrough. Short on purpose.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Does this URL's origin POSITIVELY identify as an agit-hub agent store?
///
/// The one signal that separates an agit-hub from a generic git host (github, gitlab, a plain http git
/// backend) is the JSON shape of `GET <base>/api/me`: `{username,...}` when the credential authenticates,
/// `{"error":...}` with a 401 when it does not. A generic host answers `/api/me` with an HTML 404, a
/// plain-text body, or nothing — none of which parse to that shape. This is a POSITIVE probe, never a
/// path-shape guess: a non-http(s) URL, a network failure, a timeout, or any non-agit response all return
/// `false` so the caller falls back to git passthrough. Bounded by [`PROBE_TIMEOUT`], so it never hangs.
pub fn is_hub_store_url(url: &str) -> bool {
    is_hub_store_url_timeout(url, PROBE_TIMEOUT)
}

/// [`is_hub_store_url`] with an explicit timeout — the seam tests drive against a local server.
pub fn is_hub_store_url_timeout(url: &str, timeout: Duration) -> bool {
    // Only an http(s) origin has a JSON API reachable this way; ssh/scp/local paths never do. A parse
    // error here (a bare name, an ssh URL, a garbage string) is a fast, network-free `false`.
    let Ok((base, auth)) = parse_http_url(url) else {
        return false;
    };
    probe_hub_me(&base, auth.as_ref(), timeout)
}

/// GET `<base>/api/me` (presenting `auth` if the URL carried a credential) and decide, from the response,
/// whether the origin is an agit-hub. Any transport failure is a silent `false`.
fn probe_hub_me(base: &str, auth: Option<&Auth>, timeout: Duration) -> bool {
    let url = format!("{base}/api/me");
    let agent = probe_agent(timeout);
    let mut req = agent.get(&url);
    if let Some(a) = auth {
        req = req.header("Authorization", &a.header_value());
    }
    let Ok(mut resp) = req.call() else {
        return false;
    };
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    is_hub_me_shape(status, &text)
}

/// The agit-hub `GET /api/me` signature. Authenticated → 200 with a `username` string; anonymous → 401
/// with an `error` string (`{"error":"not logged in"}`). A generic git host cannot produce EITHER: it has
/// no `/api/me`, so it answers with HTML, a 404 page, or a plain-text body that does not parse to a JSON
/// object carrying these keys at these statuses. Pinning the status (200-with-username / 401-with-error)
/// rather than accepting any `{"error":...}` keeps a random host's catch-all 404 JSON from false-matching.
fn is_hub_me_shape(status: u16, body: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let Some(obj) = v.as_object() else {
        return false;
    };
    let has_str = |k: &str| obj.get(k).and_then(|x| x.as_str()).is_some();
    (status == 200 && has_str("username")) || (status == 401 && has_str("error"))
}

/// A ureq agent for the hub probe: 4xx/5xx surface as a normal `Response` (we read the body), and every
/// stage of the request is bounded by `timeout` so an unreachable host fails fast instead of hanging.
fn probe_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

/// How to authenticate to the hub. `Basic` mirrors git's `user:token` password-field auth; `Bearer`
/// is the `AGIT_HUB_TOKEN` override. The hub accepts either (see its `credentials()` extractor).
#[derive(Debug, Clone)]
pub enum Auth {
    Basic { user: String, pass: String },
    Bearer(String),
}

impl Auth {
    /// The `Authorization` header value this credential presents.
    fn header_value(&self) -> String {
        match self {
            Auth::Basic { user, pass } => format!("Basic {}", base64_encode(format!("{user}:{pass}").as_bytes())),
            Auth::Bearer(t) => format!("Bearer {t}"),
        }
    }
}

/// A resolved hub endpoint: the API base (`scheme://authority`, no path, no credentials) plus the
/// credential to present. `auth` is `None` only for a hub reachable anonymously — every write path here
/// requires it and says so.
#[derive(Debug, Clone)]
pub struct HubEndpoint {
    pub base: String,
    pub auth: Option<Auth>,
}

impl HubEndpoint {
    /// Resolve the endpoint from the environment overrides, else the active agent's primary remote.
    ///
    /// `AGIT_HUB_URL` (if set) names the hub; otherwise the active agent's primary remote does. Either
    /// way the URL is parsed into `scheme://authority` + optional `user:token` userinfo. `AGIT_HUB_TOKEN`
    /// (if set) overrides the credential as a bearer token. A non-http(s) remote (ssh) has no JSON API
    /// reachable this way and is a clear error.
    pub fn resolve() -> Result<HubEndpoint> {
        let env_url = std::env::var("AGIT_HUB_URL").ok().filter(|s| !s.trim().is_empty());
        let source = match env_url {
            Some(u) => u,
            None => {
                let agent = crate::agent::resolve(None).context("no active agent to find a hub remote from")?;
                hub_remote_of(&agent).context(
                    "this agent has no hub remote to enroll against.\n\
                     \x20 push it to a hub first (agit a push), or set AGIT_HUB_URL (and AGIT_HUB_TOKEN).",
                )?
            }
        };
        let (base, url_auth) = parse_http_url(&source)?;
        // AGIT_HUB_TOKEN overrides any credential parsed from the URL, as a bearer token.
        let auth = match std::env::var("AGIT_HUB_TOKEN").ok().filter(|s| !s.trim().is_empty()) {
            Some(t) => Some(Auth::Bearer(t.trim().to_string())),
            None => url_auth,
        };
        Ok(HubEndpoint { base, auth })
    }

    fn require_auth(&self) -> Result<&Auth> {
        self.auth.as_ref().context(
            "no hub credential found.\n\
             \x20 put a token in the remote URL's password field, or set AGIT_HUB_TOKEN.",
        )
    }

    /// `GET /api/me` — the authenticated caller's username. Enrollment binds `enroll_sig` to exactly
    /// this name (the server verifies against the caller identity), so the client must sign with the
    /// account it actually authenticates as, not a guess from the URL userinfo.
    pub fn me(&self) -> Result<String> {
        let auth = self.require_auth()?;
        let url = format!("{}/api/me", self.base);
        let mut resp = http_agent()
            .get(&url)
            .header("Authorization", &auth.header_value())
            .call()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        let v = ok_json(status, &text, &url)?;
        v.get("username")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .context("the hub did not identify the authenticated user (GET /api/me returned no username)")
    }

    /// `DELETE /api/identity/keys/<key_fpr>` — revoke ONE of the caller's own device keys. Returns the
    /// parsed JSON on 2xx; a non-2xx (e.g. 404 unknown key) is an error carrying the hub's message.
    pub fn revoke_identity_key(&self, key_fpr: &str) -> Result<serde_json::Value> {
        let auth = self.require_auth()?;
        let url = format!("{}/api/identity/keys/{key_fpr}", self.base);
        let mut resp = http_agent()
            .delete(&url)
            .header("Authorization", &auth.header_value())
            .call()
            .with_context(|| format!("DELETE {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        ok_json(status, &text, &url)
    }

    /// `GET /api/identity/<user>`. `Ok(None)` on 404 (unknown user), the parsed device-key SET
    /// (`{ username, keys: [...], + the primary key's fields at top level }`) on 2xx, an error on any
    /// other status.
    pub fn get_identity(&self, user: &str) -> Result<Option<serde_json::Value>> {
        let auth = self.require_auth()?;
        let url = format!("{}/api/identity/{user}", self.base);
        let mut resp = http_agent()
            .get(&url)
            .header("Authorization", &auth.header_value())
            .call()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if status == 404 {
            return Ok(None);
        }
        ok_json(status, &text, &url).map(Some)
    }

    /// `GET /api/identity/by-email?email=<committer-email>` — resolve a committer email to the registered
    /// account owning it, with its device-key SET (`{ username, keys: [...], + the primary key's fields }`).
    /// `Ok(None)` on 404 (the email maps to no VERIFIED account — a normal not-found, not an oracle). This
    /// is the lookup that turns provenance's "signed by this key" into "verified as this person" (match ANY).
    pub fn get_identity_by_email(&self, email: &str) -> Result<Option<serde_json::Value>> {
        self.get_opt(&format!("/api/identity/by-email?email={}", percent_encode_query(email)))
    }

    /// A `GET` returning the parsed JSON on 2xx, `Ok(None)` on 404, an error otherwise — the shared shape
    /// behind the org/kek reads below.
    fn get_opt(&self, path: &str) -> Result<Option<serde_json::Value>> {
        let auth = self.require_auth()?;
        let url = format!("{}{path}", self.base);
        let mut resp = http_agent()
            .get(&url)
            .header("Authorization", &auth.header_value())
            .call()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if status == 404 {
            return Ok(None);
        }
        ok_json(status, &text, &url).map(Some)
    }

    /// `GET /api/orgs/<org>` — the org roster the caller can see (members + name). `Ok(None)` on 404
    /// (unknown org, or one the caller is not a member of).
    pub fn get_org(&self, org: &str) -> Result<Option<serde_json::Value>> {
        self.get_opt(&format!("/api/orgs/{org}"))
    }

    /// `GET /api/agent/<owner>/<name>` — an agent's detail, including its ACL `members` (the axis-1
    /// authorized-fetcher grants). `Ok(None)` on 404 (unknown agent, or one the caller cannot see).
    /// Used by `agit hub doctor` to reconcile the ACL against the keybox reader set.
    pub fn get_agent(&self, owner: &str, name: &str) -> Result<Option<serde_json::Value>> {
        self.get_opt(&format!("/api/agent/{owner}/{name}"))
    }

    /// `GET /api/orgs/<org>/kek/gens` — `{ gens: [...], current: G }`: the generations the caller holds
    /// an envelope for, plus the org's active generation.
    pub fn kek_gens(&self, org: &str) -> Result<serde_json::Value> {
        let auth = self.require_auth()?;
        let url = format!("{}/api/orgs/{org}/kek/gens", self.base);
        let mut resp = http_agent()
            .get(&url)
            .header("Authorization", &auth.header_value())
            .call()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        ok_json(status, &text, &url)
    }

    /// `GET /api/orgs/<org>/kek/envelope?gen=G` — the CALLER'S OWN envelope of TK_gen. `Ok(None)` on 404
    /// (not a member, or no envelope at that generation).
    pub fn get_kek_envelope(&self, org: &str, gen: i64) -> Result<Option<serde_json::Value>> {
        self.get_opt(&format!("/api/orgs/{org}/kek/envelope?gen={gen}"))
    }

    /// `POST /api/orgs/<org>/kek/envelopes` — publish a Team-KEK generation's per-member envelopes.
    /// `envelopes` is an array of `{recipient, wrapped_kek, recipient_epoch}`.
    pub fn post_kek_envelopes(&self, org: &str, gen: i64, envelopes: serde_json::Value) -> Result<serde_json::Value> {
        self.post_json(&format!("/api/orgs/{org}/kek/envelopes"), &serde_json::json!({ "gen": gen, "envelopes": envelopes }))
    }

    /// A `POST` of a JSON body returning the parsed 2xx response (an error carries the hub's message) —
    /// the shared shape behind the Wave-5 org/escrow writes below.
    fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let auth = self.require_auth()?;
        let url = format!("{}{path}", self.base);
        let payload = serde_json::to_string(body).context("serializing request body")?;
        let mut resp = http_agent()
            .post(&url)
            .header("Authorization", &auth.header_value())
            .header("Content-Type", "application/json")
            .send(&payload)
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        ok_json(status, &text, &url)
    }

    /// `GET /api/escrow/pubkey` — the hub's escrow PUBLIC key (hex), the recipient a hub-assist client
    /// seals its content key TO (encryption-recipients Wave 5).
    pub fn escrow_pubkey(&self) -> Result<String> {
        let auth = self.require_auth()?;
        let url = format!("{}/api/escrow/pubkey", self.base);
        let mut resp = http_agent()
            .get(&url)
            .header("Authorization", &auth.header_value())
            .call()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        let v = ok_json(status, &text, &url)?;
        v.get("pubkey")
            .and_then(|p| p.as_str())
            .map(|s| s.to_string())
            .context("the hub returned no escrow pubkey")
    }

    /// `POST /api/orgs/<org>/recovery` `{key}` — set the org's OFFLINE recovery recipient (owner-only).
    pub fn set_org_recovery(&self, org: &str, key: &str) -> Result<serde_json::Value> {
        self.post_json(&format!("/api/orgs/{org}/recovery"), &serde_json::json!({ "key": key }))
    }

    /// `DELETE /api/orgs/<org>/recovery` — clear the org's OFFLINE recovery recipient (owner-only).
    pub fn clear_org_recovery(&self, org: &str) -> Result<serde_json::Value> {
        let auth = self.require_auth()?;
        let url = format!("{}/api/orgs/{org}/recovery", self.base);
        let mut resp = http_agent()
            .delete(&url)
            .header("Authorization", &auth.header_value())
            .call()
            .with_context(|| format!("DELETE {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        ok_json(status, &text, &url)
    }

    /// `POST /api/orgs/<org>/escrow` `{mode}` — set the org's hub-assist escrow mode (owner-only).
    pub fn set_org_escrow(&self, org: &str, mode: &str) -> Result<serde_json::Value> {
        self.post_json(&format!("/api/orgs/{org}/escrow"), &serde_json::json!({ "mode": mode }))
    }

    /// `POST /api/agent/<owner>/<name>/keys/escrow` `{kid, wrapped_ck}` — store a content key sealed to the
    /// hub escrow key (hub-assist escrow).
    pub fn post_escrow_key(&self, owner: &str, name: &str, kid: u32, wrapped_ck: &str) -> Result<serde_json::Value> {
        self.post_json(
            &format!("/api/agent/{owner}/{name}/keys/escrow"),
            &serde_json::json!({ "kid": kid, "wrapped_ck": wrapped_ck }),
        )
    }

    /// `POST /api/agent/<owner>/<name>/keys/release` — release the escrowed content keys the caller may
    /// read. `Ok(None)` on 404 (not permitted / not a hub-assist session), the `{released:[...]}` payload
    /// otherwise. Fail-closed at the hub: it only releases what `acl::decide(_, Read)` allows.
    pub fn release_keys(&self, owner: &str, name: &str) -> Result<Option<serde_json::Value>> {
        let auth = self.require_auth()?;
        let url = format!("{}/api/agent/{owner}/{name}/keys/release", self.base);
        let mut resp = http_agent()
            .post(&url)
            .header("Authorization", &auth.header_value())
            .header("Content-Type", "application/json")
            .send("{}")
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if status == 404 || status == 403 {
            return Ok(None);
        }
        ok_json(status, &text, &url).map(Some)
    }
}

/// A ureq agent that surfaces 4xx/5xx as a normal `Response` (so we can read the hub's error body)
/// rather than ureq's default of turning them into `Err(StatusCode)`.
fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).build().into()
}

/// Parse a 2xx body as JSON, or turn a non-2xx into an error carrying the hub's `error` message.
fn ok_json(status: u16, text: &str, url: &str) -> Result<serde_json::Value> {
    if (200..300).contains(&status) {
        return serde_json::from_str(text).with_context(|| format!("{url}: hub returned status {status} with a non-JSON body"));
    }
    let msg = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| text.trim().to_string());
    if msg.is_empty() {
        bail!("{url}: hub returned status {status}");
    }
    bail!("{url}: hub returned status {status}: {msg}");
}

/// The hub remote of an agent: its primary remote's raw URL (with any credential), for base + auth
/// resolution. `None` when the store has no remote at all.
fn hub_remote_of(agent: &crate::agent::Agent) -> Option<String> {
    let remotes = crate::agent::store_remotes(&agent.store);
    let primary = crate::agent::primary_remote_name(&agent.store);
    match primary {
        Some(name) => remotes.into_iter().find(|(n, _)| *n == name).map(|(_, url)| url),
        None => remotes.into_iter().next().map(|(_, url)| url),
    }
}

/// Split an http(s) URL into `(base, auth)` where `base` is `scheme://authority` (no path, no userinfo)
/// and `auth` is a `Basic` credential when the URL carried `user:token` userinfo. A non-http(s) URL is
/// an error: the JSON API is not reachable over ssh.
/// Replace any `user:token@` userinfo with `***@` so a URL is safe to put in an error
/// message. A credentialed remote must never echo its token into stderr/logs.
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let shown = match authority.rsplit_once('@') {
        Some((_, host)) => format!("***@{host}"),
        None => authority.to_string(),
    };
    match path {
        Some(p) => format!("{scheme}://{shown}/{p}"),
        None => format!("{scheme}://{shown}"),
    }
}

fn parse_http_url(url: &str) -> Result<(String, Option<Auth>)> {
    let url = url.trim();
    let (scheme, rest) = url
        .split_once("://")
        .filter(|(s, _)| *s == "http" || *s == "https")
        .with_context(|| {
            let shown = redact_url(url);
            format!("the hub remote {shown:?} is not an http(s) URL — set AGIT_HUB_URL to the hub's https address")
        })?;
    // Userinfo lives only in the authority (up to the first '/'); the path is everything after.
    let authority = rest.split_once('/').map(|(a, _)| a).unwrap_or(rest);
    // The userinfo/host boundary is the LAST '@' (a password may itself contain '@').
    let (userinfo, host) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    if host.is_empty() {
        let shown = redact_url(url);
        bail!("the hub remote {shown:?} has no host");
    }
    let base = format!("{scheme}://{host}");
    // Only a `user:token` userinfo yields a usable Basic credential; a bare username (no token) does not.
    let auth = userinfo.and_then(|ui| ui.split_once(':')).and_then(|(u, p)| {
        if p.is_empty() {
            None
        } else {
            Some(Auth::Basic { user: u.to_string(), pass: p.to_string() })
        }
    });
    Ok((base, auth))
}

/// Percent-encode a value for a URL query slot. An email carries `@`, `+`, and `.` — `.` is safe in a
/// query, but `@`/`+` and anything non-alphanumeric must be escaped so the value reaches the hub intact
/// (a raw `+` would otherwise decode to a space). Unreserved chars (RFC 3986) pass through.
fn percent_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Standard base64 (RFC 4648, with padding). Hand-rolled to keep the client hermetic — the only place
/// the client needs to ENCODE base64 is the HTTP Basic `Authorization` header.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encodes_an_email_for_a_query_slot() {
        // `@` and `+` must escape (a raw `+` would decode to a space); unreserved chars pass through.
        assert_eq!(percent_encode_query("alice@corp.com"), "alice%40corp.com");
        assert_eq!(percent_encode_query("a+b@x.io"), "a%2Bb%40x.io");
        assert_eq!(percent_encode_query("plain.name-1_2~"), "plain.name-1_2~");
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // The exact vector the hub's own decode test uses: "git:secret123".
        assert_eq!(base64_encode(b"git:secret123"), "Z2l0OnNlY3JldDEyMw==");
    }

    #[test]
    fn parses_https_url_with_credentials() {
        let (base, auth) = parse_http_url("https://alice:tok_abc@hub.example.com/alice/frontend.git").unwrap();
        assert_eq!(base, "https://hub.example.com");
        match auth {
            Some(Auth::Basic { user, pass }) => {
                assert_eq!(user, "alice");
                assert_eq!(pass, "tok_abc");
            }
            other => panic!("expected Basic auth, got {other:?}"),
        }
    }

    #[test]
    fn keeps_the_port_and_drops_the_path() {
        let (base, auth) = parse_http_url("http://localhost:8567/bob/api.git").unwrap();
        assert_eq!(base, "http://localhost:8567");
        assert!(auth.is_none(), "no userinfo means no credential");
    }

    #[test]
    fn a_password_may_contain_an_at_sign() {
        // The userinfo/host split must be the LAST '@', or part of the token leaks into the host.
        let (base, auth) = parse_http_url("https://u:p@ss@hub.example.com/x/y.git").unwrap();
        assert_eq!(base, "https://hub.example.com");
        match auth {
            Some(Auth::Basic { user, pass }) => {
                assert_eq!(user, "u");
                assert_eq!(pass, "p@ss");
            }
            other => panic!("expected Basic auth, got {other:?}"),
        }
    }

    #[test]
    fn ssh_urls_are_rejected_with_guidance() {
        let e = parse_http_url("git@github.com:alice/frontend.git").unwrap_err().to_string();
        assert!(e.contains("not an http(s) URL"), "got: {e}");
    }

    #[test]
    fn parse_errors_never_echo_the_credential() {
        // A credentialed but non-http scheme, and a credentialed empty-host URL, must both
        // redact the token out of the error message.
        let e = parse_http_url("ftp://alice:s3cr3t-token@hub.example.com/x").unwrap_err().to_string();
        assert!(!e.contains("s3cr3t-token"), "token leaked into parse error: {e}");
        assert!(e.contains("***@hub.example.com"), "userinfo should be redacted: {e}");
        let e2 = parse_http_url("https://bob:hunter2@/no/host").unwrap_err().to_string();
        assert!(!e2.contains("hunter2"), "token leaked into no-host error: {e2}");
    }

    #[test]
    fn hub_me_shape_recognizes_the_agit_hub_and_rejects_everything_else() {
        // The two agit-hub answers: authed 200 with a username, anonymous 401 with an error.
        assert!(is_hub_me_shape(200, r#"{"username":"alice","is_admin":false,"key_count":1}"#));
        assert!(is_hub_me_shape(401, r#"{"error":"not logged in"}"#));

        // A generic host cannot forge this. A 404 catch-all that happens to be JSON is NOT a hub
        // (status is pinned): only 401 carries the anonymous `error`, only 200 the `username`.
        assert!(!is_hub_me_shape(404, r#"{"error":"not found"}"#));
        assert!(!is_hub_me_shape(200, r#"{"error":"not found"}"#));
        assert!(!is_hub_me_shape(401, r#"{"username":"alice"}"#));
        // HTML / plain-text / empty bodies (what github, gitlab, a git http backend return) never parse.
        assert!(!is_hub_me_shape(404, "<!DOCTYPE html><title>Not Found</title>"));
        assert!(!is_hub_me_shape(200, ""));
        assert!(!is_hub_me_shape(401, "Authentication failed"));
        // A JSON array/scalar is not the object shape.
        assert!(!is_hub_me_shape(200, r#"["username"]"#));
        assert!(!is_hub_me_shape(200, "42"));
    }

    /// The classification given a real (local) server, exercising the whole network path: a stubbed
    /// agit-hub `/api/me` is identified TRUE; a stubbed generic host (HTML 404 at /api/me) is FALSE.
    #[test]
    fn is_hub_store_url_probes_a_local_server() {
        // (1) A local stand-in for an agit-hub: 401 {"error":"not logged in"} at /api/me.
        let hub = stub_server(vec![(
            "/api/me",
            401,
            "application/json",
            r#"{"error":"not logged in"}"#,
        )]);
        assert!(
            is_hub_store_url_timeout(&format!("http://{}/alice/frontend.git", hub.addr), PROBE_TIMEOUT),
            "a positively-identified agit-hub store URL must classify TRUE"
        );

        // (2) A local stand-in for a generic git host: an HTML 404 at /api/me.
        let git = stub_server(vec![("/api/me", 404, "text/html", "<html>404</html>")]);
        assert!(
            !is_hub_store_url_timeout(&format!("http://{}/me/frontend.git", git.addr), PROBE_TIMEOUT),
            "a generic git host must NOT be misidentified as an agit-hub"
        );
    }

    #[test]
    fn non_http_and_unreachable_targets_are_false_and_do_not_hang() {
        // Non-http(s) inputs are a fast, network-free FALSE (no probe at all).
        for t in ["frontend", "git@github.com:me/f.git", "/srv/agents/f.git", "ssh://h/x.git", ""] {
            assert!(!is_hub_store_url(t), "{t:?} must be a network-free FALSE");
        }
        // An unreachable host must fail fast (bounded by the timeout), never hang. Port 1 is not served.
        let start = std::time::Instant::now();
        assert!(!is_hub_store_url_timeout("http://127.0.0.1:1/x.git", Duration::from_millis(400)));
        assert!(start.elapsed() < Duration::from_secs(5), "the probe must not hang on an unreachable host");
    }

    /// A throwaway one-request-per-connection HTTP server for the probe tests. Serves canned responses
    /// keyed by request path; unknown paths get a 404. Runs on a background thread until dropped.
    struct StubServer {
        addr: String,
        _handle: std::thread::JoinHandle<()>,
    }

    fn stub_server(routes: Vec<(&'static str, u16, &'static str, &'static str)>) -> StubServer {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
                let (status, ctype, body) = routes
                    .iter()
                    .find(|(p, _, _, _)| *p == path)
                    .map(|(_, s, c, b)| (*s, *c, *b))
                    .unwrap_or((404, "text/plain", "not found"));
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
                // One request per test is enough; keep serving in case a probe retries.
            }
        });
        StubServer { addr, _handle: handle }
    }
}
