//! Confirmed scopes survive uncertain replies; disable retries cannot replay converted grants.

use agit::{
    domain::{repo::Repo, session_privacy::AcceptedSnapshot},
    infra::{
        config,
        credentials::{HubCredential, save_at},
    },
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, process::Output};

pub struct Fixture {
    snapshot: AcceptedSnapshot,
    session_scope: bool,
    policy_revision: i64,
    session_revision: i64,
    revision: i64,
    explicit: bool,
    revoked: bool,
    default_suppressed: bool,
    operations: BTreeMap<String, (Value, Value)>,
    disables: BTreeMap<String, (Value, Value)>,
}

impl Fixture {
    pub fn new(snapshot: AcceptedSnapshot) -> Self {
        Self {
            snapshot,
            session_scope: true,
            policy_revision: 1,
            session_revision: 1,
            revision: 1,
            explicit: false,
            revoked: false,
            default_suppressed: false,
            operations: BTreeMap::new(),
            disables: BTreeMap::new(),
        }
    }

    pub fn policy(&self) -> Value {
        json!({"agent_id":self.snapshot.binding.repository_id,"session_id":self.snapshot.binding.session_id,
            "grant_scope":if self.session_scope { "session" } else { "commit" },
            "policy_revision":self.policy_revision,"grant_revision":self.session_revision})
    }

    pub fn route(&mut self, method: &str, path: &str, body: &[u8]) -> (u16, Value) {
        let snapshot = &self.snapshot;
        let root = format!(
            "/api/agents/alice/app/sessions/{}",
            snapshot.binding.session_id
        );
        let session = json!({"scope":"session","agent_id":snapshot.binding.repository_id,"session_id":snapshot.binding.session_id});
        if path == format!("{root}/original-grant-policy") {
            if method == "GET" {
                return (200, self.policy());
            }
            let body: Value = serde_json::from_slice(body).unwrap();
            assert_eq!(body["target"], session);
            if body["grant_scope"] == "session" {
                return (
                    400,
                    json!({"kind":"unsupported","error":"Enabling session-level authorization after disabling it is unsupported."}),
                );
            }
            let id = body["idempotency_key"].as_str().unwrap().to_owned();
            if let Some((original, result)) = self.disables.get(&id) {
                assert_eq!(&body, original);
                return (200, result.clone());
            }
            if !self.session_scope {
                return (
                    200,
                    json!({"policy":self.policy(),"converted_recipients":2,"converted_snapshots":1}),
                );
            }
            assert_eq!(body["expected_policy_revision"], self.policy_revision);
            assert_eq!(body["expected_grant_revision"], self.session_revision);
            self.session_scope = false;
            self.policy_revision += 1;
            self.session_revision += 1;
            self.revision = 1;
            let result =
                json!({"policy":self.policy(),"converted_recipients":2,"converted_snapshots":1});
            self.disables.insert(id, (body, result));
            return (
                503,
                json!({"kind":"response_unavailable","error":"synthetic uncertain disable reply"}),
            );
        }
        let prefix = format!("{root}/original-grants");
        let target = if self.session_scope {
            session
        } else {
            json!({"scope":"commit","agent_id":snapshot.binding.repository_id,
                "session_id":snapshot.binding.session_id,"published_commit_id":snapshot.published_commit,
                "envelope_digest":snapshot.envelope_digest})
        };
        if method == "GET" {
            assert_eq!(path.split_once('?').unwrap().0, prefix);
            let query: BTreeMap<_, _> =
                url::form_urlencoded::parse(path.split_once('?').unwrap().1.as_bytes())
                    .into_owned()
                    .collect();
            assert_eq!(query["scope"], target["scope"].as_str().unwrap());
            assert_eq!(
                query["expected_policy_revision"],
                self.policy_revision.to_string()
            );
            if !self.session_scope {
                assert_eq!(query["published_commit_id"], snapshot.published_commit);
                assert_eq!(query["envelope_digest"], snapshot.envelope_digest);
            } else {
                assert!(!query.contains_key("published_commit_id"));
                assert!(!query.contains_key("envelope_digest"));
            }
            return (
                200,
                json!({"target":target,"policy_revision":self.policy_revision,"grant_revision":self.revision,"readers":[
                    {"account_id":"account-alice","username":"alice","current_member":true,
                        "owner_default":!self.session_scope,"committer_default":false,"explicit_grant":!self.session_scope && self.explicit,
                        "session_explicit_grant":self.session_scope && self.explicit,"revoked":false,"session_revoked":false,
                        "default_suppressed":false,"membership_invalidated":false,"session_membership_invalidated":false,
                        "can_read":!self.session_scope || self.explicit},
                    {"account_id":"account-bob","username":"bob","current_member":true,
                        "owner_default":false,"committer_default":!self.session_scope && !self.default_suppressed,"explicit_grant":!self.session_scope && self.explicit && !self.revoked,
                        "session_explicit_grant":self.session_scope && self.explicit && !self.revoked,
                        "revoked":!self.session_scope && self.revoked,"session_revoked":self.session_scope && self.revoked,
                        "default_suppressed":self.session_scope && self.default_suppressed,"membership_invalidated":false,"session_membership_invalidated":false,
                        "can_read":!self.revoked && (self.explicit || (!self.session_scope && !self.default_suppressed))}
                ]}),
            );
        }
        assert_eq!(method, "POST");
        let body: Value = serde_json::from_slice(body).unwrap();
        if body["target"] != target || body["expected_policy_revision"] != self.policy_revision {
            return (
                409,
                json!({"kind":"conflict","error":"original grant policy changed; request superseded"}),
            );
        }
        if path == format!("{prefix}/preview") {
            assert_eq!(
                body,
                json!({"target":target,"expected_policy_revision":self.policy_revision})
            );
            return (
                200,
                json!({"id":format!("preview-{}-{}",self.policy_revision,self.revision),"target":target,
                "policy_revision":self.policy_revision,"grant_revision":self.revision,"expires_at":"2999-01-01T00:00:00Z",
                "members":[{"account_id":"account-alice","username":"alice"},{"account_id":"account-bob","username":"bob"}]}),
            );
        }
        assert_eq!(path, prefix);
        let id = body["idempotency_key"].as_str().unwrap().to_owned();
        if let Some((original, result)) = self.operations.get(&id) {
            assert_eq!(&body, original);
            return if result["grant_revision"] == self.revision {
                (200, result.clone())
            } else {
                (
                    409,
                    json!({"kind":"conflict","error":"grant operation was superseded"}),
                )
            };
        }
        assert_eq!(body["expected_grant_revision"], self.revision);
        let grant = match body["action"]["operation"].as_str().unwrap() {
            "grant" => {
                assert_eq!(
                    body["action"]["preview_id"],
                    format!("preview-{}-{}", self.policy_revision, self.revision)
                );
                self.explicit = true;
                self.revoked = false;
                true
            }
            "revoke" => {
                assert_eq!(body["action"]["account_id"], "account-bob");
                self.revoked = true;
                self.default_suppressed = true;
                false
            }
            "stop_explicit" => {
                self.explicit = false;
                false
            }
            other => panic!("unexpected original access action {other}"),
        };
        self.revision += 1;
        if self.session_scope {
            self.session_revision = self.revision;
        }
        let result = json!({"target":target,"policy_revision":self.policy_revision,"grant_revision":self.revision});
        self.operations.insert(id, (body, result.clone()));
        if grant {
            (
                503,
                json!({"kind":"response_unavailable","error":"synthetic uncertain reply"}),
            )
        } else {
            (200, result)
        }
    }
}

pub fn exercise(
    run: impl Fn(&[&str]) -> Output,
    repo: &Repo,
    home: &Path,
    hub: &str,
    commit: &str,
    before_retry: impl FnOnce(),
) {
    let credential = |account: &str| {
        save_at(
            &home
                .join("credentials")
                .join(format!("{}.json", config::hub_host_key(hub).unwrap())),
            &HubCredential {
                account_id: Some(format!("account-{account}")),
                username: account.into(),
                email: None,
                hub: Some(hub.into()),
                access_token: "synthetic-access".into(),
                refresh_token: "synthetic-refresh".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    };
    let target = format!("alice/app@{commit}");
    let refs = repo.git(&["show-ref"]).unwrap();
    let denied = run(&["privacy", "access", "show", &target]);
    assert!(!denied.status.success(), "{denied:?}");
    credential("alice");
    let pending = repo.common_dir().unwrap().join("agit/privacy-access");
    let unconfirmed = run(&["privacy", "access", "grant", &target]);
    assert!(!unconfirmed.status.success(), "{unconfirmed:?}");
    assert!(!pending.exists());
    let uncertain = run(&["--yes", "privacy", "access", "grant", &target]);
    assert!(!uncertain.status.success(), "{uncertain:?}");
    assert!(
        String::from_utf8_lossy(&uncertain.stderr).contains("is retained"),
        "{uncertain:?}"
    );
    let request = std::fs::read_dir(&pending)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let original = std::fs::read(&request).unwrap();
    let saved: Value = serde_json::from_slice(&original).unwrap();
    let id = saved["request"]["mutation"]["idempotency_key"]
        .as_str()
        .unwrap();
    assert_eq!(saved["snapshot"]["published_commit"], commit);
    before_retry();
    let retry = run(&[
        "--yes",
        "--json",
        "privacy",
        "access",
        "retry",
        "alice/app",
        id,
    ]);
    assert!(retry.status.success(), "{retry:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&retry.stdout).unwrap()["result"]["value"]["grant_revision"],
        2
    );
    assert_eq!(std::fs::read(&request).unwrap(), original);
    assert_eq!(saved["request"]["mutation"]["target"]["scope"], "session");
    assert!(String::from_utf8_lossy(&uncertain.stderr).contains("all existing and future"));
    let disable = run(&["--yes", "privacy", "access", "disable-session", &target]);
    assert!(!disable.status.success(), "{disable:?}");
    assert!(String::from_utf8_lossy(&disable.stderr).contains("is retained"));
    let disable_path = std::fs::read_dir(&pending)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path != &request)
        .unwrap();
    let disable_bytes = std::fs::read(&disable_path).unwrap();
    let disable_saved: Value = serde_json::from_slice(&disable_bytes).unwrap();
    let disable_id = disable_saved["request"]["mutation"]["idempotency_key"]
        .as_str()
        .unwrap();
    let disabled_retry = run(&[
        "--yes",
        "privacy",
        "access",
        "retry",
        "alice/app",
        disable_id,
    ]);
    assert!(disabled_retry.status.success(), "{disabled_retry:?}");
    let stale = run(&["--yes", "privacy", "access", "retry", "alice/app", id]);
    assert!(!stale.status.success(), "{stale:?}");
    assert!(String::from_utf8_lossy(&stale.stderr).contains("superseded"));
    let policy = run(&["--json", "privacy", "access", "policy", &target]);
    assert!(policy.status.success(), "{policy:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&policy.stdout).unwrap()["result"]["value"]["policy"]["grant_scope"],
        "commit"
    );
    let revoke = run(&[
        "--yes", "privacy", "access", "revoke", &target, "--member", "bob",
    ]);
    assert!(revoke.status.success(), "{revoke:?}");
    let stale = run(&["--yes", "privacy", "access", "retry", "alice/app", id]);
    assert!(!stale.status.success(), "{stale:?}");
    assert!(String::from_utf8_lossy(&stale.stderr).contains("superseded"));
    let disabled_retry = run(&[
        "--yes",
        "privacy",
        "access",
        "retry",
        "alice/app",
        disable_id,
    ]);
    assert!(disabled_retry.status.success(), "{disabled_retry:?}");
    let repeated_disable = run(&["--yes", "privacy", "access", "disable-session", &target]);
    assert!(repeated_disable.status.success(), "{repeated_disable:?}");
    let after_retry = run(&["--json", "privacy", "access", "show", &target]);
    assert!(after_retry.status.success(), "{after_retry:?}");
    let state: Value = serde_json::from_slice(&after_retry.stdout).unwrap();
    assert_eq!(
        state["result"]["value"]["access"]["readers"][1]["can_read"],
        false
    );
    assert_eq!(std::fs::read(&disable_path).unwrap(), disable_bytes);
    let stopped = run(&["--yes", "privacy", "access", "stop", &target]);
    assert!(stopped.status.success(), "{stopped:?}");
    let shown = run(&["--json", "privacy", "access", "show", &target]);
    assert!(shown.status.success(), "{shown:?}");
    let output: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let state = &output["result"]["value"];
    assert_eq!(state["access"]["grant_revision"], 3);
    assert_eq!(state["access"]["readers"][0]["can_read"], true);
    assert_eq!(state["access"]["readers"][1]["revoked"], true);
    assert_eq!(state["access"]["readers"][1]["can_read"], false);
    assert!(
        state["access"]["readers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|reader| reader["explicit_grant"] == false)
    );
    credential("bob");
    let switched = run(&["--yes", "privacy", "access", "retry", "alice/app", id]);
    assert!(!switched.status.success(), "{switched:?}");
    assert_eq!(repo.git(&["show-ref"]).unwrap(), refs);
    assert_eq!(std::fs::read(&request).unwrap(), original);
}
