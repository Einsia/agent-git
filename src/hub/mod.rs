//! AgentGit hub HTTP client.
//!
//! # The hub address
//!
//! The default is the public hub (`config::DEFAULT_HUB_URL`): it works as soon as it is
//! installed, with no environment variable to set first. A self-hosted instance or local
//! development overrides it with `AGIT_HUB_URL`:
//!
//! ```bash
//! AGIT_HUB_URL=http://127.0.0.1:8177 agit push    # a hub started locally
//! AGIT_HUB_URL=https://hub.corp.com  agit push    # self-hosted
//! ```
//!
//! Credentials are stored per address, so moving between hubs does not mean signing in again.
//!
//! # The server answers "which agent this repo picks up"
//!
//! No file in the code repo records the binding. `agit clone` with no argument calls
//! `/api/agents/for-repo` and lets the server look it up in reverse; the server intersects that
//! against **the scope the current user may access** and returns a candidate list.

pub mod client;
/// The git subprocesses that talk to the hub (`clone` / `fetch` / `push`): they inject the
/// bearer token.
pub mod git;
/// The repo-local, immutable pin of the remote identity.
pub mod identity;

pub use client::Client;

use serde::{Deserialize, Serialize};

/// One PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pr {
    pub id: u64,
    /// `owner/repo:branch`.
    pub source: String,
    pub target_branch: String,
    /// open / merged / stale / closed.
    pub state: String,
    #[serde(default)]
    pub title: Option<String>,
    /// The merge_summary on the proposing commit, if there is one.
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PrMergeResponse {
    /// fast-forward / mechanical-merge / adopt.
    pub mode: String,
}

/// My identity inside an org (`GET /api/orgs/{org}`; the hub answers 404 when I am not a member).
#[derive(Debug, Clone, Deserialize)]
pub struct OrgView {
    pub name: String,
    /// `owner` / `member` ... — only an owner can create an agent under the org.
    #[serde(default)]
    pub role: String,
}

/// The answer to one push-permission probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushAccess {
    Writable,
    ReadOnly,
    Missing,
}

impl PushAccess {
    /// How to read the status code of `info/refs?service=git-receive-pack`: 200 passed the write
    /// gate; 403 knows who you are but does not let you write; 404 means there is no such agent,
    /// **or there is one and you may not write to it** — the hub's write gate deliberately
    /// answers both cases with the same sentence, so that it does not leak whether an agent
    /// exists.
    /// `Missing` therefore says only "this step did not allow it"; whether the agent exists has to
    /// be asked separately. Any other status code is none of these three answers and is left to
    /// the caller as an error.
    pub fn from_status(status: u16) -> Option<PushAccess> {
        match status {
            200 => Some(PushAccess::Writable),
            403 => Some(PushAccess::ReadOnly),
            404 => Some(PushAccess::Missing),
            _ => None,
        }
    }
}

/// One agent on the hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAgent {
    /// The remote identity, unchanged by a rename or by a delete and a rebuild under the same
    /// name.
    pub agent_id: String,
    pub owner: String,
    pub name: String,
    /// The git clone URL.
    pub clone_url: String,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub session_count: usize,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// The opening prompt of the most recent session, for telling candidates apart in a list.
    #[serde(default)]
    pub last_gist: Option<String>,
}

impl RemoteAgent {
    /// The `<owner>/<name>` form.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub fn is_public(&self) -> bool {
        self.visibility == "public"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretRuleFinding {
    pub id: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicationFindings {
    pub suspected_secrets: u64,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub rules: Vec<SecretRuleFinding>,
    pub complete: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublicationSnapshot {
    pub refs_digest: String,
    pub ruleset_digest: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreparePublicResponse {
    pub intent_id: String,
    pub expires_at: String,
    pub confirmation_phrase: String,
    pub snapshot: PublicationSnapshot,
    pub findings: PublicationFindings,
    pub warning: String,
}

/// A sign-in request.
#[derive(Debug, Serialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// The sign-in response: a token pair plus the account.
#[derive(Debug, Deserialize)]
pub struct LoginResponse {
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
    pub access_token: String,
    pub access_expires_at: String,
    pub refresh_token: String,
    pub refresh_expires_at: String,
}

/// The refresh response (no username).
#[derive(Debug, Deserialize)]
pub struct TokenPair {
    pub access_token: String,
    pub access_expires_at: String,
    pub refresh_token: String,
    pub refresh_expires_at: String,
}

/// Publish an agent (it is named at the first publish).
#[derive(Debug, Serialize)]
pub struct PublishRequest {
    /// The agent name. **This is the only moment the user has to name anything** — the name means
    /// something only here.
    pub name: String,
    /// Which namespace to create it under. `None` is the caller themself; an org repo carries the
    /// org name, and the server decides from membership whether it may be created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Whether it is public. **Public by default**; private is what `agit push --private` asks
    /// for explicitly.
    ///
    /// Always sent explicitly, never left to the server's default — once the two ends drift apart
    /// on "what the default is", what drifts is visibility, and that is irreversible.
    pub public: bool,
    /// The code repo origins these sessions touch, for the server to build its reverse-lookup
    /// index on.
    #[serde(default)]
    pub repo_origins: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct PublishResponse {
    /// The immutable remote identity of the new copy.
    pub agent_id: String,
    /// Immutable source identity for copy responses. A fresh publish has no
    /// source; copy callers must require this to match what they fenced.
    #[serde(default)]
    pub forked_from: Option<String>,
    pub owner: String,
    pub name: String,
    /// The git URL to push to.
    pub push_url: String,
    /// The web link, the one shown to the user.
    pub web_url: String,
}
/// Share a session.
#[derive(Debug, Serialize)]
pub struct ShareRequest {
    /// The content, **already encrypted locally** — the hub never sees plaintext.
    /// The decryption key lives only in the sharing link's fragment (`#k=`) and never reaches the
    /// server.
    pub payload: String,
    #[serde(default)]
    pub encrypted: bool,
    pub expire_seconds: i64,
    #[serde(default)]
    pub max_views: Option<u32>,
    /// Password protection (the server stores the hash, never the plaintext).
    #[serde(default)]
    pub password_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ShareResponse {
    pub slug: String,
    pub url: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// One session hit. The fields correspond to `SessionHit` in the hub's `search/scan.rs`.
///
/// **One row per session**, not one per event: the same word can occur dozens of times inside one
/// session, and returning each occurrence turns a search into a screenful of the same session.
/// `other_hits` is how many more there are in this session.
#[derive(Debug, Deserialize)]
pub struct SearchHit {
    pub agent: String,
    pub session_id: String,
    /// The excerpt the hit landed in.
    pub excerpt: String,
    #[serde(default)]
    pub url: Option<String>,
    /// Quality signal: whether this attempt looks like it worked.
    ///
    /// `worked` / `failed` / `unknown`, and **`unknown` is the normal case** — the verdict is a
    /// heuristic read off the shape of the transcript (was the same question asked again, did it
    /// end in a change that landed), and with no signal it honestly says it does not know instead
    /// of guessing one: the user would go and act on an approach that in fact failed.
    ///
    /// An old hub returns `null`, so the type stays `Option`.
    #[serde(default)]
    pub outcome: Option<String>,
    /// How hard that verdict is: `low` / `medium` / `high`. `high` never occurs.
    #[serde(default)]
    pub confidence: Option<String>,
    /// The test, in plain words. **Whatever shows `outcome` must be able to show this next to
    /// it** — a worked-or-failed label with no grounds behind it makes the user act on a guess.
    #[serde(default)]
    pub outcome_reason: Option<String>,

    // ── The fields below exist only in the newer protocol. An old hub does not return them, so
    //    they are all `default`: a missing field must not become a parse failure when a new CLI
    //    talks to an old hub.
    /// Which kind of event the hit landed on: prompt / reply / tool / edit / summary.
    ///
    /// This is where this search differs from grep — "someone asked about it" and "someone
    /// actually ran it" are two different things.
    #[serde(default)]
    pub scope: Option<String>,
    /// The hit comes from a compact summary: that sentence is the compactor's paraphrase, not
    /// something anyone actually said.
    #[serde(default)]
    pub secondhand: bool,
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub turns: usize,
    /// How many more hits in this session are not shown.
    #[serde(default)]
    pub other_hits: usize,
    #[serde(default)]
    pub timestamp: Option<String>,
    /// The line number of the hit event in the raw transcript, for going back to the original for
    /// detail — the intermediate representation (IR) is a lossy projection.
    #[serde(default)]
    pub line: Option<usize>,
    /// How many sessions this row stands for, itself included. `1` or `0` (an old hub) = nothing
    /// was collapsed.
    ///
    /// When several people fall into the same pit the hub converges them into one row, so one
    /// result can stand for several sessions.
    #[serde(default)]
    pub group_size: usize,
    /// The sessions that were collapsed away, in `<agent>/<session_id>` form.
    #[serde(default)]
    pub grouped: Vec<String>,
}

/// The paged search envelope (`GET /api/search/{kind}`).
///
/// Generic rather than one struct per category: `total` / `page` / `per` / `incomplete` /
/// `unknown` mean the same thing for every category, and copying them out once per category only
/// makes "changed one, forgot the others" possible.
#[derive(Debug, Deserialize)]
// `#[serde(default)]` on `items` makes the derive infer a `T: Default` bound (it propagates
// conservatively from the field type `Vec<T>`), and a hit type has no reason to be `Default`.
// This hard-codes the bound to the one actually needed.
#[serde(bound(deserialize = "T: serde::de::DeserializeOwned"))]
pub struct SearchPage<T> {
    /// The category in effect. From it the client confirms that the word it asked for was
    /// recognized.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Total hits. **For the sessions category this can be a lower bound**; see `incomplete`.
    pub total: usize,
    #[serde(default)]
    pub page: usize,
    #[serde(default)]
    pub per: usize,
    #[serde(default)]
    pub items: Vec<T>,
    /// The server's scan budget ran out; some content went unread.
    ///
    /// This must be passed on to the user, never swallowed: silently handing back a truncated
    /// number reads as "searched, nothing there" — and what this feature is for is deciding from
    /// it whether a piece of work has to be redone.
    #[serde(default)]
    pub incomplete: bool,
    /// Qualifiers that were not recognized. `runtim:codex` (one letter mistyped) shows up here
    /// and is still searched as an ordinary word — it is reported so the user sees it, not to
    /// fail the query.
    #[serde(default)]
    pub unknown: Vec<String>,
    /// The body terms parsed out, with the qualifiers stripped.
    #[serde(default)]
    pub terms: Vec<String>,
}

/// One agent hit.
#[derive(Debug, Deserialize)]
pub struct AgentHit {
    pub owner: String,
    pub name: String,
    pub slug: String,
    pub visibility: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub size_bytes: i64,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub url: Option<String>,
}

/// One PR hit.
#[derive(Debug, Deserialize)]
pub struct PrHit {
    pub number: i64,
    pub agent: String,
    #[serde(default)]
    pub title: Option<String>,
    pub state: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub target_branch: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub updated_at: String,
    /// Which field the hit landed in: title / message / summary. `summary` is agent-generated.
    #[serde(default)]
    pub matched_in: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// One people hit. Users and orgs are merged into one category — someone searching for a name
/// does not have to know first which of the two it is.
#[derive(Debug, Deserialize)]
pub struct PersonHit {
    pub name: String,
    /// `user` or `org`.
    pub kind: String,
    /// The number of agents **visible to the current caller**, not the total.
    #[serde(default)]
    pub agents: i64,
    #[serde(default)]
    pub url: Option<String>,
}

/// How many hits each category has (`GET /api/search/counts`).
#[derive(Debug, Deserialize)]
pub struct SearchCounts {
    pub sessions: usize,
    pub agents: i64,
    pub prs: i64,
    pub people: i64,
    /// Whether the sessions number is a lower bound.
    #[serde(default)]
    pub sessions_incomplete: bool,
}

/// `GET /api/auth/me`: the current account as the server sees it.
#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Health {
    pub status: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// The latest CLI version the hub hands down (`GET /api/cli/version`).
///
/// `version` takes part in the semver comparison and answers "is this the one I should upgrade
/// to"; `commit` is the identity underneath that tag; `repo` builds the release asset URL.
/// A hub old enough to lack this endpoint gives the caller an ApiError(404) — self-upgrade being
/// silently absent against an old self-hosted hub is the right shape.
#[derive(Debug, Clone, Deserialize)]
pub struct CliVersion {
    pub version: String,
    pub tag: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub repo: String,
    /// The release / tag page URL.
    #[serde(default)]
    pub url: String,
    /// Non-empty = upgrade through the npm registry (platform subpackage + SRI check); empty =
    /// GitHub release.
    #[serde(default)]
    pub npm_package: String,
    #[serde(default)]
    pub stale: bool,
}

/// One paired machine (`agit rc list`).
#[derive(Debug, Deserialize)]
pub struct RcConnection {
    pub id: String,
    pub display_name: String,
    pub platform: String,
    pub agit_version: String,
    /// The server computes online state from heartbeats and never persists it — it changes
    /// every 15 seconds.
    pub online: bool,
    #[serde(default)]
    pub last_seen_at: Option<String>,
}

/// The response to pairing this machine: an **RC-only** token.
///
/// Separate from the account's API token, so it can be revoked on its own, and its scope covers
/// only the RC surface.
#[derive(Debug, Deserialize)]
pub struct RcPairResponse {
    pub connection_id: String,
    pub token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_api_shapes_require_the_immutable_id() {
        let agent: RemoteAgent = serde_json::from_str(
            r#"{"agent_id":"00000000-0000-0000-0000-000000000001","owner":"me","name":"photo","clone_url":"https://hub/me/photo.git"}"#,
        )
        .unwrap();
        assert_eq!(agent.agent_id, "00000000-0000-0000-0000-000000000001");
        assert!(
            serde_json::from_str::<RemoteAgent>(
                r#"{"owner":"me","name":"photo","clone_url":"https://hub/me/photo.git"}"#
            )
            .is_err(),
            "old slug-only responses must not silently become trusted identities"
        );

        let created: PublishResponse = serde_json::from_str(
            r#"{"agent_id":"00000000-0000-0000-0000-000000000002","owner":"me","name":"photo","push_url":"https://hub/me/photo.git","web_url":"https://hub/@me/photo"}"#,
        )
        .unwrap();
        assert_eq!(created.agent_id, "00000000-0000-0000-0000-000000000002");
        assert!(created.forked_from.is_none());

        let copy: PublishResponse = serde_json::from_str(
            r#"{"agent_id":"00000000-0000-0000-0000-000000000002","forked_from":"00000000-0000-0000-0000-000000000001","owner":"me","name":"photo","push_url":"https://hub/me/photo.git","web_url":"https://hub/@me/photo"}"#,
        )
        .unwrap();
        assert_eq!(
            copy.forked_from.as_deref(),
            Some("00000000-0000-0000-0000-000000000001")
        );
    }
}
