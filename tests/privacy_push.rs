#![cfg(feature = "cli")]

//! A real Git receiver must see projected ancestry while encrypted snapshots retain the originals.

#[path = "support/lab_env.rs"]
mod lab_env;

use agit::domain::{meta, privacy_envelope::PrivacyEnvelope, repo::Repo, storage, transcript};
use agit::infra::{
    config,
    credentials::{HubCredential, save_at},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crypto_box::SecretKey;
use lab_env::LabEnv;
use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[path = "support/publication_http.rs"]
mod publication_http;
#[path = "support/startup_cache.rs"]
mod startup_cache;

const AGENT_ID: &str = "9f2c3b53-7fe0-412f-b62a-bf68a6845ce7";
const TRANSPORT_USER_AGENT: &str = "synthetic-publisher-transport";

struct Hub {
    base: String,
    posts: Arc<AtomicUsize>,
    manifests: Arc<Mutex<Vec<serde_json::Value>>>,
    registrations: Arc<AtomicUsize>,
    deny: Arc<AtomicBool>,
    /// Refuses a receive that updates only tags, as a Hub that closed them would.
    refuse_tags: Arc<AtomicBool>,
    unsupported_policy: Arc<AtomicBool>,
    /// Sessions a client from before encrypted publication was retired registers encrypted. Once
    /// one leaves the set, it continues as plain at its next revision when a writer registers it
    /// plain, once.
    legacy_encrypted: Arc<Mutex<std::collections::BTreeSet<String>>>,
    corrupt_receipt: Arc<AtomicBool>,
    /// The repository's location answers 404, as it does for a reader who lost access to it.
    location_gone: Arc<AtomicBool>,
    /// The Hub doesn't advertise the carried-record proof declaration.
    undeclared: Arc<AtomicBool>,
    /// The requests that carried the carried-record proof declaration, by method and path.
    declarations: Arc<Mutex<Vec<String>>>,
    creations: Arc<AtomicUsize>,
    viewing_public: Arc<Mutex<String>>,
    visibility: Arc<Mutex<&'static str>>,
    key_version: Arc<AtomicUsize>,
    key_requests: Arc<AtomicUsize>,
    /// Every publication preview request body, in arrival order.
    previews: Arc<Mutex<Vec<serde_json::Value>>>,
    /// A ref the next strategy registration moves before answering: `(name, oid)`.
    move_on_strategy: Arc<Mutex<Option<(String, String)>>>,
    /// How many more strategy registrations the Hub accepts before it answers 500, as a Hub whose
    /// database refused the registration does; `None` accepts every one.
    strategy_budget: Arc<Mutex<Option<usize>>>,
    /// The `max_new_commits` the Hub publishes in its configuration.
    max_new_commits: Arc<AtomicUsize>,
    /// A ref whose arrival on the Hub makes the repository public.
    publicize_when: Arc<Mutex<Option<String>>>,
    /// A ref withdrawn from the caller: Git transport hides it, and a preview planning it is
    /// refused by name, as a Hub advertising `publication_planned_refs` does.
    withdrawn: Arc<Mutex<Option<String>>>,
    /// Publication previews fail as unavailable, answering nothing about the refs they plan.
    previews_unavailable: Arc<AtomicBool>,
    /// Receives the Hub refuses as `publication_refs_moved`, one per receive, in order: each
    /// first moves a ref `(name, oid)`, as another member's push between a preview and its receive
    /// does, and with a visibility also gives the repository that visibility.
    refuse_moved: Arc<Mutex<Vec<(String, String, Option<&'static str>)>>>,
    /// While set, receives go through until one carries this oid, such as a later part's tip; the
    /// refusals start there.
    refuse_moved_from: Arc<Mutex<Option<String>>>,
    /// A file the Hub writes as it refuses a receive as `publication_refs_moved`, standing in for
    /// a setting the person changes on this computer between the refusal and the retry.
    write_on_refusal: Arc<Mutex<Option<(PathBuf, String)>>>,
    /// The refusal line the next refusal carries instead of `publication_refs_moved`, such as one
    /// the client does not retry.
    refuse_with: Arc<Mutex<Option<&'static str>>>,
    /// The next repository creation finds the name taken: the repository appears, as one made
    /// by someone else between the push's lookup and its creation, and the Hub answers 409.
    taken_on_create: Arc<AtomicBool>,
    /// The next receive is applied, but its answer is lost: the client gets a 502, as when a
    /// proxy gives up on a push the Hub went on to take.
    lose_receive_answer: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Hub {
    fn start(root: &Path, viewing_key: [u8; 32]) -> Self {
        let key = SecretKey::from(viewing_key);
        let public = STANDARD.encode(key.public_key().as_bytes());
        let viewing_public = Arc::new(Mutex::new(public.clone()));
        let served_public = Arc::clone(&viewing_public);
        let key_version = Arc::new(AtomicUsize::new(1));
        let served_version = Arc::clone(&key_version);
        let key_requests = Arc::new(AtomicUsize::new(0));
        let requested_keys = Arc::clone(&key_requests);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let url = format!("{base}/alice/app.git");
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let posts = Arc::new(AtomicUsize::new(0));
        let received = Arc::clone(&posts);
        let manifests = Arc::new(Mutex::new(Vec::new()));
        let recorded_manifests = Arc::clone(&manifests);
        let registrations = Arc::new(AtomicUsize::new(0));
        let registered = Arc::clone(&registrations);
        let deny = Arc::new(AtomicBool::new(false));
        let denied = Arc::clone(&deny);
        let refuse_tags = Arc::new(AtomicBool::new(false));
        let refusing_tags = Arc::clone(&refuse_tags);
        let unsupported_policy = Arc::new(AtomicBool::new(false));
        let unsupported = Arc::clone(&unsupported_policy);
        let legacy_encrypted = Arc::new(Mutex::new(std::collections::BTreeSet::<String>::new()));
        let legacy = Arc::clone(&legacy_encrypted);
        let corrupt_receipt = Arc::new(AtomicBool::new(false));
        let corrupting = Arc::clone(&corrupt_receipt);
        let location_gone = Arc::new(AtomicBool::new(false));
        let gone = Arc::clone(&location_gone);
        let undeclared = Arc::new(AtomicBool::new(false));
        let without_declaration = Arc::clone(&undeclared);
        let declarations = Arc::new(Mutex::new(Vec::new()));
        let declared_requests = Arc::clone(&declarations);
        let confirmations = Arc::new(AtomicUsize::new(0));
        let confirmed = Arc::clone(&confirmations);
        let creations = Arc::new(AtomicUsize::new(0));
        let created = Arc::clone(&creations);
        let previews = Arc::new(Mutex::new(Vec::new()));
        let recorded_previews = Arc::clone(&previews);
        let move_on_strategy = Arc::new(Mutex::new(None::<(String, String)>));
        let strategy_move = Arc::clone(&move_on_strategy);
        let strategy_budget = Arc::new(Mutex::new(None::<usize>));
        let strategies_left = Arc::clone(&strategy_budget);
        let max_new_commits = Arc::new(AtomicUsize::new(4096));
        let published_limit = Arc::clone(&max_new_commits);
        let withdrawn = Arc::new(Mutex::new(None::<String>));
        let hidden = Arc::clone(&withdrawn);
        let previews_unavailable = Arc::new(AtomicBool::new(false));
        let preview_unavailable = Arc::clone(&previews_unavailable);
        let visibility = Arc::new(Mutex::new("private"));
        let served_visibility = Arc::clone(&visibility);
        let refuse_moved = Arc::new(Mutex::new(
            Vec::<(String, String, Option<&'static str>)>::new(),
        ));
        let moving = Arc::clone(&refuse_moved);
        let refuse_moved_from = Arc::new(Mutex::new(None::<String>));
        let moving_from = Arc::clone(&refuse_moved_from);
        let write_on_refusal = Arc::new(Mutex::new(None::<(PathBuf, String)>));
        let writing = Arc::clone(&write_on_refusal);
        let refuse_with = Arc::new(Mutex::new(None::<&'static str>));
        let refusing_with = Arc::clone(&refuse_with);
        let taken_on_create = Arc::new(AtomicBool::new(false));
        let taking = Arc::clone(&taken_on_create);
        let lose_receive_answer = Arc::new(AtomicBool::new(false));
        let losing_answer = Arc::clone(&lose_receive_answer);
        let publicize_when = Arc::new(Mutex::new(None::<String>));
        let publicizing = Arc::clone(&publicize_when);
        let resolver_hub = base.clone();
        let root = root.to_owned();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(300);
            let mut strategy = serde_json::Value::Null;
            let mut preview = serde_json::Value::Null;
            let mut pending_receipt = None;
            let mandatory = r#"{"version":1,"source":"repository","require_envelope":false,"require_session_config":true,"publication_format_version":2,"protected_paths":[]}"#;
            let mandatory_digest =
                agit::domain::privacy_envelope::digest_bytes(mandatory.as_bytes());
            let mut sessions = std::collections::BTreeMap::<String, serde_json::Value>::new();
            let mut policies = std::collections::BTreeMap::<
                String,
                agit::domain::session_grants::SessionGrantPolicy,
            >::new();
            let mut last_request = String::new();
            while !stopping.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "mock Hub deadline elapsed after {last_request}"
                );
                let (mut stream, _) = match publication_http::accept(&listener) {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("mock Hub accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut first = String::new();
                if reader.read_line(&mut first).unwrap() == 0 {
                    continue;
                }
                last_request = first.trim().to_owned();
                let mut words = first.split_whitespace();
                let method = words.next().unwrap();
                let target = words.next().unwrap();
                // The Hub finds a repository whatever the case of its name.
                let target = if target.eq_ignore_ascii_case("/api/agents/alice/app") {
                    "/api/agents/alice/app"
                } else {
                    target
                };
                let mut length = 0;
                let mut content_type = String::new();
                let mut compressed = false;
                let mut user_agent = String::new();
                let mut authenticated = false;
                let mut collaborator_account = false;
                let mut receipt_id = None;
                let mut declared = false;
                let mut receiver_scope = None;
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if let Some(value) = lower.strip_prefix("content-type:") {
                        content_type = value.trim().to_owned();
                    }
                    compressed |= lower.trim() == "content-encoding: gzip";
                    if let Some(value) = lower.strip_prefix("user-agent:") {
                        user_agent = value.trim().to_owned();
                    }
                    assert!(
                        !lower.starts_with("transfer-encoding:"),
                        "fixture body must be bounded"
                    );
                    authenticated |= lower.trim() == "authorization: bearer synthetic-access";
                    collaborator_account |=
                        lower.trim() == "authorization: bearer synthetic-collaborator";
                    if let Some(value) = lower.strip_prefix("x-agentgit-privacy-preview-id:") {
                        receipt_id = Some(value.trim().to_owned());
                    }
                    declared |= lower.trim() == "x-agentgit-carried-records-proof: v1";
                    if let Some(value) = lower.strip_prefix("x-agentgit-privacy-receiver-scope:") {
                        receiver_scope = Some(value.trim().to_owned());
                    }
                }
                assert!(
                    authenticated || collaborator_account || target == "/api/auth/refresh",
                    "{first}"
                );
                if declared {
                    declared_requests
                        .lock()
                        .unwrap()
                        .push(format!("{method} {target}"));
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                drop(reader);
                if compressed {
                    let mut decoded = Vec::new();
                    flate2::read::GzDecoder::new(body.as_slice())
                        .read_to_end(&mut decoded)
                        .unwrap();
                    body = decoded;
                }
                if (target == "/api/agents/alice/app" && !root.join("alice/app.git").is_dir())
                    || target == "/api/agents/alice/notes"
                {
                    let body = r#"{"error":"repository is unavailable"}"#;
                    write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    continue;
                }
                // The Hub keeps no publication status for a version it never received.
                if target == format!("/api/repositories/{AGENT_ID}/location")
                    && gone.load(Ordering::Acquire)
                {
                    let body = r#"{"kind":"not_found","error":"source unavailable"}"#;
                    write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    continue;
                }
                if target.contains("/privacy?ref=") {
                    let body = r#"{"kind":"not_found","error":"version not found"}"#;
                    write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    continue;
                }
                let contains =
                    |needle: &[u8]| body.windows(needle.len()).any(|value| value == needle);
                if target.ends_with("/git-receive-pack")
                    && refusing_tags.load(Ordering::Acquire)
                    && contains(b"refs/tags/")
                    && !contains(b"refs/heads/")
                {
                    write!(
                        stream,
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    continue;
                }
                if method == "PUT" && target == "/api/agents/alice/app/privacy/strategy" {
                    let mut left = strategies_left.lock().unwrap();
                    if *left == Some(0) {
                        write!(
                            stream,
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .unwrap();
                        continue;
                    }
                    if let Some(left) = left.as_mut() {
                        *left -= 1;
                    }
                }
                if target.starts_with("/alice/app.git/") && denied.load(Ordering::Acquire) {
                    write!(
                        stream,
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    continue;
                }
                if target.starts_with("/api/me/privacy/") {
                    write!(stream, "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                let hidden_ref = hidden.lock().unwrap().clone();
                let json = match (method, target) {
                    ("POST", path)
                        if path.starts_with("/api/agents/alice/app/sessions/")
                            && path.ends_with("/reuses") =>
                    {
                        Some(json!({"ok":true}))
                    }
                    // This Hub gives no computer the originals of an encrypted snapshot.
                    ("POST", path)
                        if path.starts_with("/api/agents/alice/app/sessions/")
                            && path.ends_with("/original-key") =>
                    {
                        let body =
                            r#"{"kind":"original_access_denied","error":"no access to originals"}"#;
                        write!(stream, "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                        continue;
                    }
                    (method, path)
                        if path.starts_with("/api/agents/alice/app/sessions/")
                            && path.ends_with("/privacy-config") =>
                    {
                        let session = path
                            .strip_prefix("/api/agents/alice/app/sessions/")
                            .unwrap()
                            .strip_suffix("/privacy-config")
                            .unwrap();
                        if method == "POST" {
                            registered.fetch_add(1, Ordering::AcqRel);
                            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                            assert_eq!(request["expected_repository_id"], AGENT_ID);
                            let mut privacy = request["privacy"].clone();
                            let legacy = legacy.lock().unwrap().contains(session);
                            if legacy {
                                privacy["scheme"] = json!("session_owner_v2");
                            }
                            let mut config = json!({"repository_id":AGENT_ID,"session_id":session,"privacy":privacy,"revision":1,"created_at":"2026-01-01T00:00:00Z"});
                            if let Some(previous) = sessions.get(session)
                                && previous["privacy"]["scheme"] == "session_owner_v2"
                                && request["privacy"]["scheme"] == "plain"
                                && declared
                            {
                                config = previous.clone();
                                config["privacy"]["scheme"] = json!("plain");
                                config["revision"] =
                                    json!(previous["revision"].as_u64().unwrap() + 1);
                                sessions.insert(session.into(), config.clone());
                            }
                            policies.entry(session.into()).or_insert_with(|| {
                                agit::domain::session_grants::SessionGrantPolicy {
                                    agent_id: AGENT_ID.into(),
                                    session_id: session.into(),
                                    grant_scope: agit::domain::session_grants::GrantScope::Session,
                                    policy_revision: 1,
                                    grant_revision: 1,
                                }
                            });
                            if let Some(previous) = sessions.insert(session.into(), config.clone())
                            {
                                assert_eq!(previous, config);
                            }
                        }
                        if session == format!("agit-{}", "b".repeat(40)) {
                            sessions.entry(session.into()).or_insert_with(|| json!({"repository_id":AGENT_ID,"session_id":session,"privacy":{"scheme":"plain","hub":null,"creator":null,"owner":null,"owner_source":"unresolved"},"revision":1,"created_at":"2026-01-01T00:00:00Z"}));
                        }
                        let Some(config) = sessions.get(session) else {
                            let body = r#"{"kind":"session_privacy_unregistered","error":"register this session"}"#;
                            write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                            continue;
                        };
                        let key = if config["privacy"]["scheme"] == "session_owner_v2" {
                            json!({"account_id":config["privacy"]["owner"]["id"],"key_version":served_version.load(Ordering::Acquire),"public_key_algorithm":"x25519","public_key":served_public.lock().unwrap().clone()})
                        } else {
                            serde_json::Value::Null
                        };
                        Some(json!({"config":config,"owner_key":key}))
                    }
                    (method, path)
                        if path.starts_with("/api/agents/alice/app/sessions/")
                            && path.ends_with("/original-grant-policy") =>
                    {
                        use agit::domain::session_grants::{GrantPolicyMutation, GrantScope};
                        if method == "POST" && unsupported.load(Ordering::Acquire) {
                            let body = r#"{"kind":"unsupported","error":"Enabling session-level authorization after disabling it is unsupported."}"#;
                            write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                            continue;
                        }
                        let session = path
                            .strip_prefix("/api/agents/alice/app/sessions/")
                            .unwrap()
                            .strip_suffix("/original-grant-policy")
                            .unwrap();
                        let policy = policies
                            .get_mut(session)
                            .expect("policy requires registration");
                        if method == "GET" {
                            Some(json!(policy))
                        } else {
                            let request: GrantPolicyMutation =
                                serde_json::from_slice(&body).unwrap();
                            request.validate().unwrap();
                            assert_eq!(request.target, policy.target().unwrap());
                            assert_eq!(request.expected_policy_revision, policy.policy_revision);
                            assert_eq!(request.expected_grant_revision, policy.grant_revision);
                            assert_eq!(request.grant_scope, GrantScope::Commit);
                            if policy.grant_scope == GrantScope::Session {
                                policy.grant_scope = GrantScope::Commit;
                                policy.policy_revision += 1;
                                policy.grant_revision += 1;
                            }
                            Some(
                                json!({"policy":policy,"converted_recipients":0,"converted_snapshots":0}),
                            )
                        }
                    }
                    ("POST", "/api/me/session-key/ensure") => {
                        requested_keys.fetch_add(1, Ordering::Acquire);
                        Some(
                            json!({"ready":true,"key":{"account_id":"account-1","key_version":served_version.load(Ordering::Acquire),"public_key_algorithm":"x25519","public_key":served_public.lock().unwrap().clone()}}),
                        )
                    }
                    ("POST", "/api/auth/refresh") => {
                        assert_eq!(
                            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["refresh_token"],
                            "synthetic-refresh"
                        );
                        Some(
                            json!({"account_id":"account-1","username":"alice","access_token":"synthetic-access","refresh_token":"synthetic-refresh","access_expires_at":"2099-01-01T00:00:00Z","refresh_expires_at":"2099-01-01T00:00:00Z"}),
                        )
                    }
                    ("GET", "/api/auth/me") => Some(if collaborator_account {
                        json!({"account_id":"account-2","username":"bob"})
                    } else {
                        json!({"account_id":"account-1","username":"alice"})
                    }),
                    ("POST", "/api/agents") => {
                        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(request["name"], "app");
                        assert_eq!(request["public"], false);
                        assert_eq!(request["repo_origins"], json!([]));
                        assert!(request["owner"].is_null());
                        // A push creates only a repository that is new.
                        assert_eq!(request["expect_new"], true);
                        let destination = root.join("alice/app.git");
                        assert!(!destination.exists());
                        fs::create_dir_all(&destination).unwrap();
                        git(&destination, &["init", "--bare", "--initial-branch=main"]);
                        git(&destination, &["config", "http.receivepack", "true"]);
                        if taking.swap(false, Ordering::AcqRel) {
                            let body =
                                r#"{"kind":"conflict","error":"this name is taken","fix":[]}"#;
                            write!(stream, "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                            continue;
                        }
                        created.fetch_add(1, Ordering::Release);
                        Some(
                            json!({"agent_id":AGENT_ID,"owner":"alice","name":"app","push_url":url,"web_url":format!("{resolver_hub}/alice/app")}),
                        )
                    }
                    ("PUT", "/api/agents/alice/app/privacy/strategy") => {
                        if let Some((name, oid)) = strategy_move.lock().unwrap().take() {
                            git(&root.join("alice/app.git"), &["update-ref", &name, &oid]);
                        }
                        strategy = serde_json::from_slice(&body).unwrap();
                        assert_eq!(strategy["publication_format_version"], 2);
                        assert_eq!(
                            strategy["summary"],
                            json!({"session_modes":["plain","session_owner_v2"]})
                        );
                        strategy["agent_id"] = json!(AGENT_ID);
                        strategy["mandatory_policy"] = serde_json::from_str(mandatory).unwrap();
                        strategy["mandatory_policy_digest"] = json!(mandatory_digest);
                        Some(strategy.clone())
                    }
                    ("POST", "/api/agents/alice/app/privacy/publication/preview") => {
                        if preview_unavailable.load(Ordering::Acquire) {
                            let body = r#"{"error":"unavailable"}"#;
                            write!(stream, "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                            continue;
                        }
                        preview = serde_json::from_slice(&body).unwrap();
                        recorded_previews.lock().unwrap().push(preview.clone());
                        let planned = preview
                            .as_object_mut()
                            .unwrap()
                            .remove("planned_refs")
                            .expect("a Hub that advertises planned refs gets them");
                        if let Some(name) = &hidden_ref
                            && planned.as_array().unwrap().contains(&json!(name))
                        {
                            let body = json!({"kind":"refs_withdrawn","error":"these refs are withdrawn on the Hub","fix":[],"refused_refs":[name]}).to_string();
                            write!(stream, "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                            continue;
                        }
                        assert_eq!(preview["policy_digest"], strategy["policy_digest"]);
                        assert_eq!(
                            preview["expected_refs_digest"],
                            remote_refs_digest(&root.join("alice/app.git"), hidden_ref.as_deref())
                        );
                        assert_eq!(preview["envelope_format_version"], 2);
                        let manifest: agit::domain::session_privacy::SessionPublicationManifest =
                            serde_json::from_value(preview["manifest"].clone()).unwrap();
                        manifest.validate().unwrap();
                        recorded_manifests
                            .lock()
                            .unwrap()
                            .push(preview["manifest"].clone());
                        assert_eq!(
                            preview["snapshot_digest"],
                            agit::domain::privacy_envelope::digest_bytes(
                                &serde_json::to_vec(&manifest).unwrap()
                            )
                        );
                        assert_eq!(
                            preview["receiver_scope"],
                            format!("sessions:{}:{AGENT_ID}", served_visibility.lock().unwrap())
                        );
                        preview["preview_id"] =
                            json!(format!("preview-{}", confirmed.load(Ordering::Acquire)));
                        preview["mandatory_policy_digest"] = json!(mandatory_digest);
                        let mut response = preview.clone();
                        response["expires_at"] = json!(
                            (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339()
                        );
                        Some(response)
                    }
                    ("POST", "/api/agents/alice/app/privacy/publication/confirm") => {
                        let mut request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        request["mandatory_policy_digest"] = json!(mandatory_digest);
                        assert_eq!(request, preview);
                        confirmed.fetch_add(1, Ordering::Release);
                        pending_receipt = Some(preview.clone());
                        if corrupting.load(Ordering::Acquire) {
                            request["target_refs_digest"] = json!(
                                agit::domain::privacy_envelope::digest_bytes(b"another snapshot")
                            );
                        }
                        Some(request)
                    }
                    ("GET", "/api/agents/alice/app") => Some(
                        json!({"agent_id":AGENT_ID,"owner":"alice","name":"app","clone_url":url,"visibility":*served_visibility.lock().unwrap()}),
                    ),
                    // Where the repository lives now, asked before a session's privacy status.
                    ("GET", path) if path == format!("/api/repositories/{AGENT_ID}/location") => {
                        Some(
                            json!({"agent_id":AGENT_ID,"owner":"alice","name":"app","clone_url":url}),
                        )
                    }
                    // A publication plans against the limits the Hub publishes.
                    ("GET", "/api/hub/config") => Some(json!({
                        "mode":"live",
                        "limits":{"push":{
                            "max_new_commits":published_limit.load(Ordering::Acquire),
                            "max_request_bytes":2147483648u64
                        }},
                        "capabilities": if without_declaration.load(Ordering::Acquire) {
                            json!({"publication_planned_refs":true})
                        } else {
                            json!({"publication_planned_refs":true,"carried_records_proof":["v1"]})
                        }
                    })),
                    _ => None,
                };
                if let Some(json) = json {
                    let body = json.to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                } else {
                    assert!(
                        target.starts_with("/alice/app.git/"),
                        "unexpected request: {first}"
                    );
                    let refusal = (method == "POST" && target.ends_with("/git-receive-pack"))
                        .then(|| {
                            let mut from = moving_from.lock().unwrap();
                            if from.as_ref().is_some_and(|oid| !contains(oid.as_bytes())) {
                                return None;
                            }
                            *from = None;
                            let mut queued = moving.lock().unwrap();
                            (!queued.is_empty()).then(|| queued.remove(0))
                        })
                        .flatten();
                    if let Some((name, oid, visibility)) = refusal {
                        git(&root.join("alice/app.git"), &["update-ref", &name, &oid]);
                        if let Some(visibility) = visibility {
                            *served_visibility.lock().unwrap() = visibility;
                        }
                        if let Some((path, contents)) = writing.lock().unwrap().take() {
                            fs::create_dir_all(path.parent().unwrap()).unwrap();
                            fs::write(path, contents).unwrap();
                        }
                        // Framed as the Hub frames a refusal a client acts on, so git prints its
                        // kind: a fatal packet, inside the side band when the push asked for one.
                        let line = refusing_with.lock().unwrap().take().unwrap_or(
                            "ERR HTTP 409 publication_refs_moved: the repository changed after this publication was previewed; preview it again\n",
                        );
                        let mut packet = format!("{:04x}{line}", line.len() + 4);
                        if contains(b"side-band") {
                            packet = format!("{:04x}\u{1}{packet}", packet.len() + 5);
                        }
                        let body = format!("{packet}0000");
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/x-git-receive-pack-result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                        continue;
                    }
                    if method == "POST" && target.ends_with("/git-receive-pack") {
                        assert_eq!(user_agent, TRANSPORT_USER_AGENT);
                        let receipt: serde_json::Value = pending_receipt
                            .take()
                            .expect("receive needs a single-use confirmation");
                        assert_eq!(receipt_id.as_deref(), receipt["preview_id"].as_str());
                        assert_eq!(
                            receiver_scope.as_deref(),
                            receipt["receiver_scope"].as_str()
                        );
                        assert_eq!(
                            receipt["expected_refs_digest"],
                            remote_refs_digest(&root.join("alice/app.git"), hidden_ref.as_deref())
                        );
                        assert!(
                            body.windows(b" atomic".len())
                                .any(|value| value == b" atomic"),
                            "receipt-bound receive must request atomic refs"
                        );
                        received.fetch_add(1, Ordering::Release);
                    }
                    let (path, query) = target.split_once('?').unwrap_or((target, ""));
                    let mut command = git_command(&root);
                    if let Some(name) = &hidden_ref {
                        command
                            .env("GIT_CONFIG_COUNT", "1")
                            .env("GIT_CONFIG_KEY_0", "transfer.hideRefs")
                            .env("GIT_CONFIG_VALUE_0", name);
                    }
                    let mut child = command
                        .arg("http-backend")
                        .env("GIT_PROJECT_ROOT", &root)
                        .env("GIT_HTTP_EXPORT_ALL", "1")
                        .env("REQUEST_METHOD", method)
                        .env("PATH_INFO", path)
                        .env("QUERY_STRING", query)
                        .env("CONTENT_TYPE", content_type)
                        .env("CONTENT_LENGTH", body.len().to_string())
                        .env("REMOTE_USER", "alice")
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap();
                    child.stdin.take().unwrap().write_all(&body).unwrap();
                    let response = child.wait_with_output().unwrap();
                    assert!(response.status.success(), "{response:?}");
                    if method == "POST" && target.ends_with("/git-receive-pack") {
                        assert_eq!(
                            preview["target_refs_digest"],
                            remote_refs_digest(&root.join("alice/app.git"), hidden_ref.as_deref())
                        );
                        let mut publicizing = publicizing.lock().unwrap();
                        if publicizing.as_ref().is_some_and(|name| {
                            !git(&root.join("alice/app.git"), &["for-each-ref", name]).is_empty()
                        }) {
                            *publicizing = None;
                            *served_visibility.lock().unwrap() = "public";
                        }
                    }
                    let split = response
                        .stdout
                        .windows(4)
                        .position(|bytes| bytes == b"\r\n\r\n")
                        .unwrap();
                    let header = std::str::from_utf8(&response.stdout[..split]).unwrap();
                    assert!(!header.contains("Status:"), "{header}");
                    let body = &response.stdout[split + 4..];
                    if method == "POST"
                        && target.ends_with("/git-receive-pack")
                        && losing_answer.swap(false, Ordering::AcqRel)
                    {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        continue;
                    }
                    // Access probes may close after the response headers.
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\n{header}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(body);
                }
            }
        });
        Self {
            base,
            posts,
            manifests,
            registrations,
            deny,
            refuse_tags,
            unsupported_policy,
            legacy_encrypted,
            corrupt_receipt,
            location_gone,
            undeclared,
            declarations,
            creations,
            viewing_public,
            visibility,
            key_version,
            key_requests,
            previews,
            move_on_strategy,
            strategy_budget,
            max_new_commits,
            publicize_when,
            withdrawn,
            previews_unavailable,
            refuse_moved,
            refuse_moved_from,
            write_on_refusal,
            refuse_with,
            taken_on_create,
            lose_receive_answer,
            stop,
            worker: Some(worker),
        }
    }
}

/// The digest of the refs the caller is shown: every ref but `hidden`.
fn remote_refs_digest(repository: &Path, hidden: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let output = git(
        repository,
        &["for-each-ref", "--format=%(refname)%00%(objectname)"],
    );
    let mut refs: Vec<_> = output
        .lines()
        .filter(|line| hidden.is_none_or(|hidden| line.split('\0').next() != Some(hidden)))
        .collect();
    refs.sort();
    let mut digest = Sha256::new();
    for reference in refs {
        digest.update(reference.as_bytes());
        digest.update(b"\n");
    }
    format!("sha256:{}", hex::encode(digest.finalize()))
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let result = publication_http::join(self.worker.take().unwrap());
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.env_clear_local();
    for name in ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("empty-config"));
    command
}

/// Writes a session snapshot into the worktree as agit commits one: the tree then holds the
/// events its LOG names and no others.
fn write_snapshot(root: &Path, log: &str, view: &str) -> anyhow::Result<()> {
    match fs::remove_dir_all(root.join(meta::EVENTS_DIR)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    storage::write_snapshot(root, log, view)
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = git_command(root)
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[track_caller]
fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Creation and Git receive require consent and exact receipts; retries preserve session authority.
/// Projected Git uploads use the publisher's configured transport preferences.
/// Explicit plain selection survives renames and retries without changing source history or other sessions.
#[test]
fn session_push_registers_owner_and_preserves_mixed_history_and_immutable_retries() {
    use agit::domain::session_privacy::{AccountIdentity, CommitterAttribution, SessionPrivacy};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let key = SecretKey::from([25; 32]);
    let hub = Hub::start(&remote_root, key.to_bytes());
    // Sessions this test publishes encrypted were registered that way by clients from before
    // encrypted publication was retired.
    for session in ["a", "f"] {
        hub.legacy_encrypted
            .lock()
            .unwrap()
            .insert(format!("agit-{}", session.repeat(40)));
    }
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run_at = |args: &[&str], automatic: bool, selected_home: &Path, dir: &Path| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(dir)
            .env("AGIT_HOME", selected_home)
            .env("CODEX_HOME", selected_home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", selected_home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args.iter().map(|arg| {
                // An automatic push is the internal entry background spawners run.
                if automatic && *arg == "push" {
                    "__push-automatic"
                } else {
                    *arg
                }
            }));
        command.output().unwrap()
    };
    let run_home = |args: &[&str], automatic: bool, selected_home: &Path| {
        run_at(args, automatic, selected_home, temp.path())
    };
    let run = |args: &[&str], automatic: bool| run_home(args, automatic, &home);
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["config", "tag.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Accepted plain file").unwrap();
    repo.add_all().unwrap();
    repo.commit("Accepted file line").unwrap();
    let base = git(repo.root(), &["rev-parse", "HEAD"]);
    git(repo.root(), &["tag", "file-release"]);
    git(
        repo.root(),
        &["tag", "-a", "file-notes", "-m", "Reviewed file release"],
    );
    let file_tag = git(repo.root(), &["rev-parse", "refs/tags/file-notes"]);
    assert_success(&run(
        &["new", "alice/app", "-b", "unencrypted", "--no-launch"],
        false,
    ));
    let plain_birth = git(repo.root(), &["rev-parse", "refs/heads/unencrypted"]);
    let chosen_meta = meta::read_at_ref_result(&repo, &plain_birth)
        .unwrap()
        .unwrap();
    let mode_args = ["privacy", "mode", "alice/app@unencrypted", "encrypted"];
    assert!(!run(&mode_args, false).status.success());
    let unchanged = run(
        &["--json", "privacy", "mode", "alice/app@unencrypted"],
        false,
    );
    assert_success(&unchanged);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&unchanged.stdout).unwrap()["result"]["value"]
            ["local_mode"],
        "plain"
    );
    assert_eq!(
        chosen_meta.session_privacy.as_ref().unwrap().scheme,
        agit::domain::session_privacy::SessionScheme::Plain
    );
    let mode = run(
        &["--json", "privacy", "status", "alice/app@unencrypted"],
        false,
    );
    assert_success(&mode);
    let mode: serde_json::Value = serde_json::from_slice(&mode.stdout).unwrap();
    assert_eq!(mode["result"]["value"]["local_mode"], "plain");
    assert_eq!(
        mode["result"]["value"]["registered_mode"],
        serde_json::Value::Null
    );
    assert!(!remote.exists());
    let dry = run(&["push", "alice/app@main", "--dry-run"], false);
    assert!(!dry.status.success(), "{dry:?}");
    assert!(String::from_utf8_lossy(&dry.stderr).contains("Publication processing time:"));
    for flags in [vec!["--quiet"], vec!["--json"]] {
        let mut args = flags;
        args.extend(["push", "alice/app@main", "--dry-run"]);
        let output = run(&args, false);
        assert!(!output.status.success());
        for bytes in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(bytes).contains("Publication processing time:"));
        }
        if args.contains(&"--json") {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["ok"], false);
        }
    }
    assert_eq!(hub.creations.load(Ordering::Acquire), 0);
    assert!(!remote.exists());
    let unconfirmed = run(&["push", "alice/app@main"], false);
    assert!(!unconfirmed.status.success(), "{unconfirmed:?}");
    assert_eq!(hub.creations.load(Ordering::Acquire), 0);
    hub.corrupt_receipt.store(true, Ordering::Release);
    let create = ["--yes", "push", "alice/app@main"];
    let refused = run(&create, false);
    assert!(!refused.status.success(), "{refused:?}");
    let progress = String::from_utf8_lossy(&refused.stderr);
    assert!(
        progress
            .lines()
            .any(|line| line.starts_with("Getting receipt for batch") && line.contains("stopped")),
        "{progress}"
    );
    assert!(
        !progress.contains("Uploading and awaiting acknowledgement"),
        "{progress}"
    );
    assert_eq!(hub.creations.load(Ordering::Acquire), 1);
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert!(git(&remote, &["for-each-ref"]).is_empty());
    hub.corrupt_receipt.store(false, Ordering::Release);
    // Pushed from a project folder: the home directory itself is never one.
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let published = run_at(&create, false, &home, &project);
    assert_success(&published);
    // The first push the person makes from a folder that never uploaded by itself tells their
    // agent it could, to this repository, and to ask them first.
    let offered = String::from_utf8_lossy(&published.stdout);
    assert!(offered.contains("For the agent:"), "{offered}");
    assert!(offered.contains("`agit on --repo alice/app`"), "{offered}");
    assert!(
        offered.contains("(private: only its members can read it)"),
        "{offered}"
    );
    let progress = String::from_utf8_lossy(&published.stderr);
    for stage in [
        "Preparing publication snapshots",
        "Writing publication preview",
        "Rechecking source and destination before upload",
        "Uploading and awaiting acknowledgement",
        "Recording publication acknowledgements",
    ] {
        assert!(
            progress
                .lines()
                .any(|line| line.starts_with(stage) && line.contains("done (")),
            "{stage}: {progress}"
        );
        assert!(!String::from_utf8_lossy(&published.stdout).contains(stage));
    }
    assert_eq!(hub.creations.load(Ordering::Acquire), 1);
    assert!(hub.posts.load(Ordering::Acquire) > 0);
    // A branch never pushed is told as not published yet, not as a repository the reader cannot
    // see.
    let unpublished = run(&["privacy", "status", "alice/app@unencrypted"], false);
    assert!(!unpublished.status.success(), "{unpublished:?}");
    let said = String::from_utf8_lossy(&unpublished.stderr);
    assert!(
        said.contains("this version of alice/app@unencrypted isn't published yet"),
        "{said}"
    );
    // A status names no upload: the person asked for a status.
    assert!(said.contains("once it's published"), "{said}");
    assert!(!said.contains("agit push"), "{said}");
    assert!(!said.contains("private agent you cannot access"), "{said}");
    // A reader who lost access to the repository is not told the version isn't published: with
    // no receipt here either, only the repository's own answer can say which it is.
    hub.location_gone.store(true, Ordering::Release);
    let lost = run(&["privacy", "status", "alice/app@unencrypted"], false);
    hub.location_gone.store(false, Ordering::Release);
    assert!(!lost.status.success(), "{lost:?}");
    let said = String::from_utf8_lossy(&lost.stderr);
    assert!(!said.contains("isn't published yet"), "{said}");
    assert!(!said.contains("once it's published"), "{said}");
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), base);
    assert_eq!(git(&remote, &["rev-parse", "refs/tags/file-release"]), base);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/tags/file-notes"]),
        file_tag
    );
    git(repo.root(), &["tag", "-d", "file-release"]);
    hub.posts.store(0, Ordering::Release);
    let save = |metadata: &meta::Meta, content: &str| {
        let log = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &metadata.session,
        );
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), metadata).unwrap();
        repo.add_all().unwrap();
        repo.git(&[
            "-c",
            "commit.gpgsign=false",
            "-c",
            "i18n.commitEncoding=ISO-8859-1",
            "commit",
            "--author=Original Author <author@example.invalid>",
            "--date=1700000000 +0530",
            "-m",
            "Saved session snapshot\n\nReviewed by the original writer.",
        ])
        .unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    repo.git(&["checkout", "-b", "legacy"]).unwrap();
    let plain_meta = meta::Meta::new(
        format!("agit-{}", "b".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let plain = save(&plain_meta, "Registered plain conversation");
    git(repo.root(), &["tag", "plain-release"]);
    git(
        repo.root(),
        &["tag", "-a", "inner", "-m", "Reviewed inner release"],
    );
    let inner = git(repo.root(), &["rev-parse", "refs/tags/inner"]);
    git(
        repo.root(),
        &[
            "tag",
            "-a",
            "release-1",
            "inner",
            "-m",
            "Reviewed outer release",
        ],
    );
    let outer = git(repo.root(), &["rev-parse", "refs/tags/release-1"]);
    git(repo.root(), &["tag", "-d", "inner"]);
    repo.git(&["checkout", "-b", "work", &base]).unwrap();
    let mut metadata = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    metadata.session_privacy = Some(SessionPrivacy {
        scheme: agit::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(
            AccountIdentity::new(&hub.base, "session-owner").unwrap(),
        ))
        .unwrap()
    });
    metadata.committer_account = Some(AccountIdentity::new(&hub.base, "account-1").unwrap());
    fs::write(
        repo.root().join("native-private.txt"),
        "PRIVATE_NATIVE_FILE",
    )
    .unwrap();
    let original = save(&metadata, "Private transcript ORIGINAL_CONTEXT");
    let args = ["--yes", "push", "alice/app", "-b", "work", "-b", "legacy"];
    git(repo.root(), &["tag", &format!("agit-{original}")]);
    hub.deny.store(true, Ordering::Release);
    assert!(!run(&args, false).status.success());
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    hub.deny.store(false, Ordering::Release);
    let dry = run(&["push", "alice/app@work", "--dry-run"], false);
    assert!(!dry.status.success(), "{dry:?}");
    assert!(
        String::from_utf8_lossy(&dry.stderr).contains("registered before a dry-run"),
        "{dry:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    git(repo.root(), &["tag", "private-release"]);
    let refused_tag = run(&args, false);
    assert!(!refused_tag.status.success(), "{refused_tag:?}");
    assert!(
        String::from_utf8_lossy(&refused_tag.stderr)
            .contains("points to rewritten publication history"),
        "{refused_tag:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    git(repo.root(), &["tag", "-d", "private-release"]);
    hub.corrupt_receipt.store(true, Ordering::Release);
    let mut reviewed_args = args.to_vec();
    reviewed_args.push("--show-preview");
    let refused = run(&reviewed_args, false);
    assert!(!refused.status.success(), "{refused:?}");
    let displayed = String::from_utf8_lossy(&refused.stdout);
    assert!(displayed.contains("Registered plain conversation"));
    // The destination is confirmed by a person, so it reads as a sentence, not a JSON object.
    assert!(
        displayed.lines().any(|line| line
            == format!(
                "Publication destination: alice/app on {} (private, existing repository) as alice",
                hub.base
            )),
        "{displayed}"
    );
    // The session was only registered encrypted, never published that way, so no encrypted
    // snapshot bounds what readers saw: it continues as plain, and its files publish as any plain
    // session's do.
    assert!(displayed.contains("PRIVATE_NATIVE_FILE"), "{displayed}");
    assert!(
        String::from_utf8_lossy(&refused.stdout).contains("receipt differs"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), base);
    let path = String::from_utf8_lossy(&refused.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Privacy preview: "))
        .unwrap()
        .to_owned();
    let reviewed: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let tags = reviewed["plan"]["tags"].as_array().unwrap();
    assert!(
        tags.iter()
            .all(|tag| tag["name"] != "refs/tags/file-release")
    );
    assert!(
        tags.iter()
            .any(|tag| tag["name"] == "refs/tags/plain-release" && tag["oid"] == plain)
    );
    assert!(
        tags.iter()
            .any(|tag| tag["name"] == "refs/tags/release-1" && tag["oid"] == outer)
    );
    let objects = reviewed["plan"]["tag_objects"].as_array().unwrap();
    assert!(objects.contains(&json!(inner)));
    assert!(objects.contains(&json!(outer)));
    let planned = reviewed["plan"]["heads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|head| head["name"] == "refs/heads/work")
        .unwrap()["oid"]
        .as_str()
        .unwrap()
        .to_owned();
    let source_commit = git(repo.root(), &["cat-file", "commit", &original]);
    let committer = source_commit
        .lines()
        .find_map(|line| line.strip_prefix("committer "))
        .unwrap();
    let snapshot = reviewed["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|snapshot| snapshot["commit"] == planned)
        .unwrap();
    assert_eq!(
        snapshot["git"],
        json!({
            "author": "Original Author <author@example.invalid> 1700000000 +0530",
            "committer": committer,
            "encoding": "ISO-8859-1",
            "message": "Saved session snapshot\n\nReviewed by the original writer.\n",
        })
    );
    for field in snapshot["git"].as_object().unwrap().values() {
        for line in field
            .as_str()
            .unwrap()
            .lines()
            .filter(|line| !line.is_empty())
        {
            assert!(displayed.contains(line), "missing preview content: {line}");
        }
    }
    let rotated = SecretKey::from([26; 32]);
    *hub.viewing_public.lock().unwrap() = STANDARD.encode(rotated.public_key().as_bytes());
    hub.key_version.store(2, Ordering::Release);
    hub.corrupt_receipt.store(false, Ordering::Release);
    assert_success(&run(&args, false));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), planned);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/legacy"]), plain);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/tags/plain-release"]),
        plain
    );
    assert_eq!(git(&remote, &["rev-parse", "refs/tags/release-1"]), outer);
    for oid in [&inner, &outer] {
        assert_eq!(
            git(&remote, &["cat-file", "tag", oid]),
            git(repo.root(), &["cat-file", "tag", oid])
        );
    }
    assert!(
        git(
            &remote,
            &["for-each-ref", "--format=%(refname)", "refs/tags/"]
        )
        .lines()
        .all(|name| name != format!("refs/tags/agit-{original}"))
    );
    assert_eq!(git(&remote, &["rev-parse", &format!("{planned}^")]), base);
    let attribution = "--format=%an%n%ae%n%aI%n%cn%n%ce%n%cI%n%B";
    assert_eq!(
        git(&remote, &["show", "--no-patch", attribution, &planned]),
        git(repo.root(), &["show", "--no-patch", attribution, &original])
    );
    let envelope = |commit: &str| {
        PrivacyEnvelope::parse(
            git(
                &remote,
                &["show", &format!("{commit}:privacy/envelope.json")],
            )
            .as_bytes(),
        )
        .unwrap()
    };
    // That session was registered encrypted by an older client but never published that way, so
    // its first publication continues it as plain: no envelope, and its files publish as any plain
    // session's do.
    let tree = git(&remote, &["ls-tree", "-r", "--name-only", &planned]);
    assert!(
        !tree.lines().any(|path| path.starts_with("privacy/")),
        "{tree}"
    );
    assert!(tree.contains("native-private.txt"), "{tree}");
    let pushed = hub.posts.load(Ordering::Acquire);
    assert_success(&run(&args, false));
    assert_eq!(hub.posts.load(Ordering::Acquire), pushed);
    repo.set_auto_push(Some(true)).unwrap();
    save(&metadata, "Private continuation COMPLETE_CONTEXT");
    assert_success(&run(&["push", "alice/app@work"], true));
    let next = git(&remote, &["rev-parse", "refs/heads/work"]);
    assert_eq!(git(&remote, &["rev-parse", &format!("{next}^")]), planned);
    assert!(
        !git(&remote, &["ls-tree", "-r", "--name-only", &next])
            .lines()
            .any(|path| path.starts_with("privacy/"))
    );
    let receipt = agit::domain::privacy_receipt::PublicationReceipt::load(&repo, "work")
        .unwrap()
        .unwrap();
    assert!(receipt.session_snapshot.is_none());

    // Policy configuration precedes publication without becoming immutable encryption metadata.
    assert_success(&run(
        &["new", "alice/app", "-b", "prepublish", "--no-launch"],
        false,
    ));
    let retired = run(
        &[
            "--yes",
            "privacy",
            "mode",
            "alice/app@prepublish",
            "encrypted",
        ],
        false,
    );
    assert!(!retired.status.success(), "{retired:?}");
    assert!(
        String::from_utf8_lossy(&retired.stderr).contains(
            "Encrypted publication is retired: sessions publish only the redacted view, and the originals stay on this computer. Nothing was changed."
        ),
        "{retired:?}"
    );
    let before_policy = git(repo.root(), &["rev-parse", "refs/heads/prepublish"]);
    let created = meta::read_at_ref_result(&repo, &before_policy)
        .unwrap()
        .unwrap();
    hub.legacy_encrypted
        .lock()
        .unwrap()
        .insert(created.session.clone());
    // An unpublished session publishes plain, so it has no grant policy to configure.
    let unregistered = run(
        &[
            "--json",
            "privacy",
            "access",
            "policy",
            "alice/app@prepublish",
        ],
        false,
    );
    assert!(!unregistered.status.success(), "{unregistered:?}");
    assert!(
        String::from_utf8_lossy(&unregistered.stdout)
            .contains("plain sessions use repository read access"),
        "{unregistered:?}"
    );
    let unpublished = run(
        &[
            "--yes",
            "privacy",
            "access",
            "grant",
            "alice/app@prepublish",
        ],
        false,
    );
    assert!(!unpublished.status.success(), "{unpublished:?}");
    let refusal = String::from_utf8_lossy(&unpublished.stderr);
    assert!(
        refusal.contains("Granting access to originals is retired")
            && !refusal.contains("requires an accepted snapshot"),
        "{unpublished:?}"
    );
    let partial = run(
        &["privacy", "access", "policy", "alice/app@prepublish#1"],
        false,
    );
    assert!(!partial.status.success(), "{partial:?}");
    let again = run_at(
        &["--yes", "push", "alice/app@prepublish"],
        false,
        &home,
        &project,
    );
    assert_success(&again);
    // Once per folder.
    assert!(
        !String::from_utf8_lossy(&again.stdout).contains("For the agent:"),
        "{again:?}"
    );
    let published = git(&remote, &["rev-parse", "refs/heads/prepublish"]);
    let publication = envelope(&published);
    let authority = publication.session_binding.as_ref().unwrap();
    assert_eq!(authority.session_id, created.session);
    assert_eq!(authority.owner_key.account_id, "account-1");
    assert_eq!(authority.source_commit, before_policy);
    // A session an older client published encrypted keeps its grant policy, and the owner can
    // still narrow it.
    let policy = run(
        &[
            "--json",
            "privacy",
            "access",
            "policy",
            "alice/app@prepublish",
        ],
        false,
    );
    assert_success(&policy);
    let policy: serde_json::Value = serde_json::from_slice(&policy.stdout).unwrap();
    assert_eq!(
        policy["result"]["value"]["policy"]["grant_scope"],
        "session"
    );
    assert_eq!(
        policy["result"]["value"]["policy"]["session_id"],
        created.session
    );
    let disable = run(
        &[
            "--yes",
            "--json",
            "privacy",
            "access",
            "disable-session",
            "alice/app@prepublish",
        ],
        false,
    );
    assert_success(&disable);
    let disable: serde_json::Value = serde_json::from_slice(&disable.stdout).unwrap();
    assert_eq!(disable["result"]["value"]["converted_snapshots"], 0);
    let disabled_policy = disable["result"]["value"]["policy"].clone();
    assert_eq!(disabled_policy["grant_scope"], "commit");
    assert_eq!(
        git(repo.root(), &["rev-parse", "refs/heads/prepublish"]),
        before_policy
    );
    let policy_after = run(
        &[
            "--json",
            "privacy",
            "access",
            "policy",
            "alice/app@prepublish",
        ],
        false,
    );
    assert_success(&policy_after);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&policy_after.stdout).unwrap()["result"]["value"]
            ["policy"],
        disabled_policy
    );
    let repeated = run(
        &[
            "--yes",
            "privacy",
            "access",
            "disable-session",
            "alice/app@prepublish",
        ],
        false,
    );
    assert_success(&repeated);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/prepublish"]),
        published
    );
    hub.unsupported_policy.store(true, Ordering::Release);
    let unsupported = run(
        &[
            "--yes",
            "--json",
            "privacy",
            "access",
            "disable-session",
            "alice/app@prepublish",
        ],
        false,
    );
    assert!(!unsupported.status.success(), "{unsupported:?}");
    let response = String::from_utf8_lossy(&unsupported.stdout);
    assert!(
        response
            .contains("Enabling session-level authorization after disabling it is unsupported."),
        "{unsupported:?}"
    );
    assert!(!response.contains("is retained"));
    assert!(!response.contains("authorization is disabled"));
    let policy_after = run(
        &[
            "--json",
            "privacy",
            "access",
            "policy",
            "alice/app@prepublish",
        ],
        false,
    );
    assert_success(&policy_after);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&policy_after.stdout).unwrap()["result"]["value"]
            ["policy"],
        disabled_policy
    );
    let key_requests = hub.key_requests.load(Ordering::Acquire);
    repo.git(&["checkout", "--ignore-other-worktrees", "unencrypted"])
        .unwrap();
    let first_plain_source = save(&chosen_meta, "Explicit plain context");
    let second_plain_source = save(&chosen_meta, "Next plain context");
    repo.git(&["branch", "-m", "unencrypted", "publish-plain"])
        .unwrap();
    assert_success(&run(&["privacy", "mode", "alice/app@publish-plain"], false));
    let access = run(
        &["privacy", "access", "policy", "alice/app@publish-plain"],
        false,
    );
    assert!(!access.status.success());
    assert!(
        String::from_utf8_lossy(&access.stderr)
            .contains("plain sessions use repository read access")
    );
    let plain_push = ["--yes", "push", "alice/app@publish-plain"];
    hub.corrupt_receipt.store(true, Ordering::Release);
    assert!(!run(&plain_push, false).status.success());
    let locked = run(
        &[
            "--yes",
            "privacy",
            "mode",
            "alice/app@publish-plain",
            "encrypted",
        ],
        false,
    );
    assert!(!locked.status.success());
    assert!(String::from_utf8_lossy(&locked.stderr).contains("Encrypted publication is retired"));
    hub.corrupt_receipt.store(false, Ordering::Release);
    assert_success(&run(&plain_push, false));
    let plain_head = git(&remote, &["rev-parse", "refs/heads/publish-plain"]);
    assert_eq!(plain_head, second_plain_source);
    assert_eq!(
        git(repo.root(), &["rev-parse", "refs/heads/publish-plain"]),
        second_plain_source
    );
    assert!(
        git(
            &remote,
            &["ls-tree", "-r", &plain_head, "--", "privacy/envelope.json"]
        )
        .is_empty()
    );
    let plain_metadata: meta::Meta = serde_json::from_str(&git(
        &remote,
        &["show", &format!("{plain_head}:session/meta.json")],
    ))
    .unwrap();
    assert_eq!(
        plain_metadata.session_privacy.unwrap().scheme,
        agit::domain::session_privacy::SessionScheme::Plain
    );
    assert_eq!(
        storage::materialize_at(&remote, &plain_head, meta::LOG_FILE).unwrap(),
        storage::materialize_at(repo.root(), &second_plain_source, meta::LOG_FILE).unwrap()
    );
    assert_eq!(
        git(&remote, &["rev-parse", &format!("{plain_head}^")]),
        first_plain_source
    );
    assert_success(&run(&plain_push, false));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/publish-plain"]),
        plain_head
    );
    let third = save(&chosen_meta, "Continued plain context");
    assert_success(&run(&plain_push, false));
    let next_plain = git(&remote, &["rev-parse", "refs/heads/publish-plain"]);
    assert_eq!(
        git(&remote, &["rev-parse", &format!("{next_plain}^")]),
        plain_head
    );
    let receipt = agit::domain::privacy_receipt::PublicationReceipt::load(&repo, "publish-plain")
        .unwrap()
        .unwrap();
    assert_eq!(receipt.source, third);
    assert_eq!(receipt.published, next_plain);
    assert_eq!(
        receipt.mode,
        agit::domain::privacy_receipt::PublicationMode::Ordinary
    );
    assert_eq!(hub.key_requests.load(Ordering::Acquire), key_requests);
    for (target, mode, reason) in [
        (
            "alice/app@publish-plain",
            "encrypted",
            "Encrypted publication is retired",
        ),
        ("alice/app@prepublish", "plain", "already registered"),
    ] {
        let refused = run(&["--yes", "privacy", "mode", target, mode], false);
        assert!(!refused.status.success(), "{refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains(reason),
            "{refused:?}"
        );
    }

    // A publication restores repeated events across worker batches from one retained dictionary.
    use agit::domain::privacy::{
        dictionary::{Label, Origin, Record},
        keys, management,
        storage::Store,
    };
    let owner = keys::owner(&hub.base, "account-1").unwrap();
    let mut dictionary = Store::open(
        &keys::owner_directory(&home.join("privacy"), &owner).join("dictionary"),
        Some(owner),
    )
    .unwrap();
    let secret = "AKIA2E7YQXK4NMZ5VJ3T";
    let record = Record::new(
        &dictionary.dictionary_id,
        secret,
        Origin::RepositoryUser,
        Label::secret("explicit-block"),
    )
    .unwrap();
    dictionary
        .append(std::slice::from_ref(&record), None)
        .unwrap();
    let incidental = "ordinary remembered document label";
    let allowed = "allowed remembered fixture";
    let hidden = "hidden protected fixture";
    let retained = [incidental, allowed, hidden].map(|value| {
        Record::new(
            &dictionary.dictionary_id,
            value,
            Origin::Heuristic,
            Label::secret("generic-api-key"),
        )
        .unwrap()
    });
    dictionary.append(&retained, None).unwrap();
    dictionary
        .decide(&management::Decision {
            scope: management::scope(Some(repo.root())).unwrap(),
            token: retained[1].token.clone(),
            name: "allowed fixture".into(),
            block: None,
            allow: Some(true),
        })
        .unwrap();
    let projected_secret = "m7Qp4Vn9Rx2Lw8Cs5Tb3Yh6Jd1Fz0Kae";
    repo.git(&["checkout", "-b", "hydrated", &base]).unwrap();
    let mut hydrated_meta = meta::Meta::new(
        format!("agit-{}", "f".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    hydrated_meta.session_privacy = Some(SessionPrivacy {
        scheme: agit::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(AccountIdentity::new(&hub.base, "account-1").unwrap())).unwrap()
    });
    let repeated = transcript::wrap_lines(
        &json!({"type":"user","message":{"role":"user","content":record.token}}).to_string(),
        "claude-code",
        &hydrated_meta.session,
    );
    let padding = transcript::wrap_lines(
        &json!({"type":"user","message":{"role":"user","content":format!(
            "{} {incidental}; {allowed}; api_key=\"{projected_secret}\"",
            "padding ".repeat(20_000)
        )}})
        .to_string(),
        "claude-code",
        &hydrated_meta.session,
    );
    let omitted = transcript::wrap_lines(
        &json!({"type":"future_record","text":retained[2].token}).to_string(),
        "claude-code",
        &hydrated_meta.session,
    );
    let log = format!("{repeated}{padding}{repeated}{omitted}");
    let view = repeated.repeat(2);
    write_snapshot(repo.root(), &log, &view).unwrap();
    meta::write(repo.root(), &hydrated_meta).unwrap();
    repo.add_all().unwrap();
    repo.commit("Protected repeated events").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@hydrated"], false));
    // A branch new to the Hub whose name holds that request's protected value, spelled as a branch
    // name, is refused: the stored request shows only a placeholder, so the name is compared with
    // the request as publication restores it.
    let spelled = format!("ship-{}", secret.to_ascii_lowercase());
    repo.git(&["branch", &spelled, "hydrated"]).unwrap();
    let before = hub.posts.load(Ordering::Acquire);
    let refused = run(&["--yes", "push", &format!("alice/app@{spelled}")], false);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("rename the branch with `agit branch rename"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), before);
    repo.git(&["branch", "-D", &spelled]).unwrap();
    let restored = envelope(&git(&remote, &["rev-parse", "refs/heads/hydrated"]))
        .open_layer(&rotated)
        .unwrap();
    let (restored_log, restored_view) = restored.session_bytes().unwrap();
    let records = storage::parse_envelopes(&restored_log).unwrap();
    assert_eq!(records[0].content["message"]["content"], secret);
    assert_eq!(records[0].content, records[2].content);
    assert_eq!(
        *restored_view,
        storage::envelope_line(&records[0]).repeat(2)
    );
    assert!(restored.protected_values.contains(secret));
    // Source placeholders remain protected even when their record is omitted publicly. A
    // dictionary-only or allowed literal appearing in plaintext does not acquire a block.
    assert!(restored.protected_values.contains(hidden));
    assert!(restored.protected_values.contains(projected_secret));
    assert!(!restored.protected_values.contains(incidental));
    assert!(!restored.protected_values.contains(allowed));
    assert_eq!(
        storage::materialize_worktree(repo.root(), meta::LOG_FILE).unwrap(),
        log
    );

    let missing = Record::new(
        &dictionary.dictionary_id,
        "missing original",
        Origin::RepositoryUser,
        Label::secret("explicit-block"),
    )
    .unwrap();
    // Hydration is checked on a session's first, encrypted publication; `hydrated` itself now
    // continues as plain, which publishes the placeholder as it is.
    repo.git(&["checkout", "-q", "-b", "unhydrated", &base])
        .unwrap();
    let unhydrated_meta = meta::Meta {
        session: format!("agit-{}", "6".repeat(40)),
        ..hydrated_meta.clone()
    };
    hub.legacy_encrypted
        .lock()
        .unwrap()
        .insert(unhydrated_meta.session.clone());
    let unknown = transcript::wrap_lines(
        &json!({"type":"user","message":{"role":"user","content":missing.token}}).to_string(),
        "claude-code",
        &unhydrated_meta.session,
    );
    write_snapshot(repo.root(), &unknown, &unknown).unwrap();
    meta::write(repo.root(), &unhydrated_meta).unwrap();
    repo.add_all().unwrap();
    repo.commit("Unavailable original").unwrap();
    let received = hub.posts.load(Ordering::Acquire);
    let refused = run(&["--yes", "push", "alice/app@unhydrated"], false);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("publication hydration is incomplete"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), received);

    // A value the user registered is refused in a publication branch name. Only the privacy
    // worker's dictionary holds it, so a check that ran the detector alone would publish it.
    let registered = "zephyr-orchid-7741";
    let environment = git_command(temp.path());
    let mut add = Command::new(env!("CARGO_BIN_EXE_agit"));
    add.env_clear_local();
    for (name, value) in environment.get_envs() {
        if let Some(value) = value {
            add.env(name, value);
        }
    }
    let mut add = add
        .current_dir(temp.path())
        .env("AGIT_HOME", &home)
        .env("AGIT_HUB_URL", &hub.base)
        .env("CI", "1")
        .env("AGIT_TUI", "0")
        .env("AGIT_USE_SYSTEM_GIT", "1")
        .env(
            "AGIT_SECRETS_KEYSTORE",
            if cfg!(unix) { "file" } else { "os" },
        )
        .args(["secrets", "add", "branch-word", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    add.stdin
        .take()
        .unwrap()
        .write_all(registered.as_bytes())
        .unwrap();
    assert_success(&add.wait_with_output().unwrap());
    let named = format!("ship-{registered}");
    repo.git(&["branch", &named, "work"]).unwrap();
    let refused = run(&["--yes", "push", &format!("alice/app@{named}")], false);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("rename the branch with `agit branch rename"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), received);

    // A fresh checkout publishes only inherited public history and its own cross-runtime turns.
    let collaborator = temp.path().join("collaborator");
    startup_cache::seed(&collaborator);
    let credential_name = format!("{}.json", config::hub_host_key(&hub.base).unwrap());
    fs::create_dir_all(collaborator.join("credentials")).unwrap();
    save_at(
        &collaborator.join("credentials").join(&credential_name),
        &HubCredential {
            account_id: Some("account-2".into()),
            username: "bob".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-collaborator".into(),
            refresh_token: "synthetic-collaborator-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    assert_success(&run_home(
        &["clone", "alice/app", "--no-bind"],
        false,
        &collaborator,
    ));
    let collaborator_repo = Repo::at(collaborator.join("repos/alice/app"));
    assert!(
        !collaborator_repo
            .root()
            .join(".git/agit/privacy-accepted")
            .exists()
    );
    collaborator_repo
        .git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    let local_main = git(collaborator_repo.root(), &["rev-parse", "refs/heads/main"]);
    let teammate = temp.path().join("teammate");
    let cloned = git_command(temp.path())
        .args(["clone", "--quiet", "--single-branch", "--branch", "main"])
        .arg(&remote)
        .arg(&teammate)
        .output()
        .unwrap();
    assert_success(&cloned);
    git(&teammate, &["config", "user.name", "Teammate"]);
    git(
        &teammate,
        &["config", "user.email", "teammate@example.invalid"],
    );
    fs::write(teammate.join("shared-update.md"), "New shared knowledge\n").unwrap();
    git(&teammate, &["add", "shared-update.md"]);
    git(
        &teammate,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-gpg-sign",
            "-m",
            "Update shared knowledge",
        ],
    );
    git(&teammate, &["push", "origin", "main"]);
    let remote_main = git(&remote, &["rev-parse", "refs/heads/main"]);
    assert_ne!(remote_main, local_main);
    let fork_target = "alice/app@collaborator-fork";
    let fork_source = format!(
        "alice/app@{}",
        git(&remote, &["rev-parse", "refs/heads/legacy"])
    );
    assert_success(&run_home(
        &[
            "run",
            &fork_source,
            "--no-launch",
            "--as",
            "codex",
            "-b",
            "collaborator-fork",
        ],
        false,
        &collaborator,
    ));
    assert_success(&run_home(
        &["resume", fork_target, "--as", "claude-code", "--no-launch"],
        false,
        &collaborator,
    ));
    let live = walkdir::WalkDir::new(collaborator.join("claude/projects"))
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .expect("cross-runtime resume installs a Claude transcript")
        .into_path();
    let native_id = live.file_stem().unwrap().to_str().unwrap();
    let mut parent = fs::read_to_string(&live)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|record| record.get("uuid").cloned())
        .next_back()
        .unwrap_or(serde_json::Value::Null);
    let mut appended = fs::OpenOptions::new().append(true).open(&live).unwrap();
    for mut record in [
        json!({"type":"user","sessionId":native_id,"uuid":uuid::Uuid::now_v7(),"message":{"role":"user","content":"Collaborator continuation"}}),
        json!({"type":"assistant","sessionId":native_id,"uuid":uuid::Uuid::now_v7(),"message":{"role":"assistant","content":[{"type":"text","text":"Collaborator reply"}]}}),
    ] {
        record["parentUuid"] = parent;
        parent = record["uuid"].clone();
        record["cwd"] = json!(temp.path());
        writeln!(appended, "{record}").unwrap();
    }
    drop(appended);
    assert_success(&run_home(&["commit", fork_target], false, &collaborator));
    let pushed = run_home(&["--yes", "push", fork_target], false, &collaborator);
    assert_success(&pushed);
    let notice = String::from_utf8_lossy(&pushed.stdout);
    assert_eq!(
        notice.matches("main left as on the Hub").count(),
        1,
        "{notice}"
    );
    assert!(notice.contains("agit pull alice/app@main"), "{notice}");
    assert_success(&run_home(
        &["push", fork_target, "--dry-run"],
        false,
        &collaborator,
    ));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), remote_main);
    assert_eq!(
        git(collaborator_repo.root(), &["rev-parse", "refs/heads/main"]),
        local_main
    );
    assert_eq!(
        git(&remote, &["show", "main:shared-update.md"]),
        "New shared knowledge"
    );
    for args in [
        vec!["push", "alice/app@main", "--dry-run"],
        vec!["push", "alice/app", "--all", "--dry-run"],
    ] {
        let explicit = run_home(&args, false, &collaborator);
        assert!(!explicit.status.success(), "{explicit:?}");
        assert!(
            String::from_utf8_lossy(&explicit.stderr).contains("agit pull alice/app@main"),
            "{explicit:?}"
        );
        assert!(
            !String::from_utf8_lossy(&explicit.stdout).contains("main left as on the Hub"),
            "{explicit:?}"
        );
    }
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), remote_main);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/legacy"]),
        fork_source.strip_prefix("alice/app@").unwrap()
    );
    let published_meta: meta::Meta = serde_json::from_str(&git(
        &remote,
        &["show", &format!("collaborator-fork:{}", meta::FILE)],
    ))
    .unwrap();
    assert_eq!(published_meta.runtime, "claude-code");
    assert_eq!(
        published_meta.committer_account.unwrap().account_id,
        "account-2"
    );

    // A shallow boundary cannot hide a locally available parent from implicit-main selection.
    assert_success(&run_home(&["pull", "alice/app@main"], false, &collaborator));
    fs::write(
        collaborator_repo.root().join("local-main-update.md"),
        "Local shared knowledge\n",
    )
    .unwrap();
    git(collaborator_repo.root(), &["add", "local-main-update.md"]);
    git(
        collaborator_repo.root(),
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-gpg-sign",
            "-m",
            "Local shared knowledge",
        ],
    );
    let local_ahead = git(collaborator_repo.root(), &["rev-parse", "refs/heads/main"]);
    let shallow = collaborator_repo.root().join(".git/shallow");
    fs::write(&shallow, format!("{local_ahead}\n")).unwrap();
    let published = run_home(&["--yes", "push", fork_target], false, &collaborator);
    fs::remove_file(shallow).unwrap();
    assert_success(&published);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), local_ahead);
    assert!(
        !String::from_utf8_lossy(&published.stdout).contains("main left as on the Hub"),
        "{published:?}"
    );
}

/// `--to` names any repository the account can write to, and that repository never becomes the
/// checkout's default destination. A checkout of `alice/notes`, pinned to its own repository
/// identity, publishes to `alice/app` with `--to`: the history lands there, the checkout keeps its
/// pinned identity and origin, and a plain push still targets `alice/notes`, where the pin refuses
/// to create a replacement for the missing repository. `--to` names an existing destination in
/// another case, and the summary shown before anything uploads names the spelling the Hub stores. An
/// implementation that compared the destination with the pin would refuse the `--to` push; one
/// that bound the checkout to the destination would send the plain push to `alice/app`.
#[test]
fn an_explicit_destination_publishes_without_rebinding_the_checkout() {
    use agit::hub::identity::{self, RemoteIdentity};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let hub = Hub::start(&remote_root, [27; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/notes")).unwrap();
    for (key, value) in [
        ("http.userAgent", TRANSPORT_USER_AGENT),
        ("user.name", "Source Writer"),
        ("user.email", "source@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        repo.git(&["config", key, value]).unwrap();
    }
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Notes shared elsewhere\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("Notes").unwrap();
    let own = RemoteIdentity::new(&hub.base, "1c0de000-0000-4000-8000-000000000001").unwrap();
    let own_url = format!("{}/alice/notes.git", hub.base);
    identity::pin(&repo, &own).unwrap();
    repo.set_remote(&own_url).unwrap();

    // The destination already exists; `--to` names it in another case.
    let destination = remote_root.join("alice/app.git");
    fs::create_dir_all(&destination).unwrap();
    git(&destination, &["init", "--bare", "--initial-branch=main"]);
    git(&destination, &["config", "http.receivepack", "true"]);
    let pushed = run(&["--yes", "push", "alice/notes@main", "--to", "alice/App"]);
    assert_success(&pushed);
    assert!(
        String::from_utf8_lossy(&pushed.stdout).contains(&format!(
            "Publication destination: alice/app on {} (private, existing repository)",
            hub.base
        )),
        "{pushed:?}"
    );
    let published = remote_root.join("alice/app.git");
    assert_eq!(
        git(&published, &["rev-parse", "refs/heads/main"]),
        git(repo.root(), &["rev-parse", "refs/heads/main"])
    );
    assert_eq!(identity::read(&repo).unwrap(), Some(own));
    assert_eq!(repo.remote_url().unwrap(), own_url);

    let creations = hub.creations.load(Ordering::Acquire);
    let refused = run(&["--yes", "push", "alice/notes@main"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert_eq!(hub.creations.load(Ordering::Acquire), creations);
}

/// A migration must send the registered target identity in real Git objects and retain it
/// when a later ordinary push extends native source history. Same-identity pushes repair origin.
/// Collaborator commits stay unattributed; unprovable creators fail before any registration.
/// Legacy birth attribution survives continuation recorded under the destination account.
/// Without a terminal, a plain session publishes to a private repository unasked, but a public
/// destination takes the person's own `--yes`. A gate that only checked encryption would publish
/// publicly here; one that refused after preparing the upload would leave refs on the receiver.
#[test]
fn unattended_plain_publication_to_a_public_repository_needs_explicit_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["new", "alice/app", "-b", "work", "--no-launch"]));
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    hub.posts.store(0, Ordering::Release);

    *hub.visibility.lock().unwrap() = "public";
    let refused = run(&["push", "alice/app@work"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(
            "publishing to a public repository needs the person's confirmation: run it in a terminal, or pass --yes once they have confirmed it in this conversation"
        ),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert!(git(&remote, &["for-each-ref", "refs/heads/work"]).is_empty());

    assert_success(&run(&["--yes", "push", "alice/app@work"]));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/work"]),
        git(repo.root(), &["rev-parse", "refs/heads/work"])
    );

    *hub.visibility.lock().unwrap() = "private";
    let work = home.join("worktrees/alice/app/work");
    fs::write(work.join("README.md"), "Shared file, revised").unwrap();
    git(&work, &["commit", "-am", "Revise file"]);
    assert_success(&run(&["push", "alice/app@work"]));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/work"]),
        git(repo.root(), &["rev-parse", "refs/heads/work"])
    );
}

/// A receipt the Hub refuses because the repository's refs moved after the preview (another
/// member's push to any branch), or because it had expired when the upload arrived, is previewed
/// again, once, from the start. The second attempt reads
/// the destination and the project again and passes every gate again: a conversation dropped
/// meanwhile is not sent, by the retry or by any later publication of the push, a branch of a
/// failed group published again on its own included. A `--yes` given for the first attempt answers
/// it only while it is the same publication: a repository that became public meanwhile, or sessions
/// that changed, stop an unattended push before anything is sent. In a push in parts the retry's
/// first part is compared with the refused one, and when the `--yes` no longer stands, a later part
/// carrying encrypted sessions is not covered by a first part that went out under the plain-private
/// policy. A `--yes` withdrawn this way stays withdrawn for the rest of the push, the branches of a
/// failed group published again one at a time included. A second refusal is reported with the Hub's
/// reason instead of retried. A push that gave up at the first refusal loses the upload to an
/// unrelated push; one that resent its first receipt would be refused again; one that reused what
/// it judged before the move, carried the `--yes` onto content nobody confirmed, let the
/// plain-private policy stand in for it on a later encrypted part, or gave the `--yes` or the
/// project's earlier answer back to the next attempt, would publish what nobody checked; and one
/// that kept retrying would loop.
#[test]
fn a_receipt_refused_because_the_repository_moved_is_previewed_again_once() {
    use agit::domain::session_privacy::{AccountIdentity, SessionPrivacy, SessionScheme};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    // Its encrypted turns were registered that way by a client from before encrypted publication
    // was retired.
    hub.legacy_encrypted
        .lock()
        .unwrap()
        .insert(format!("agit-{}", "a".repeat(40)));
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let text = |out: &Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["new", "alice/app", "-b", "work", "--no-launch"]));
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    let tip = git(&remote, &["rev-parse", "refs/heads/main"]);
    let work = home.join("worktrees/alice/app/work");
    let previews = || hub.previews.lock().unwrap().len();

    let changed = "needs a new answer";

    // Another member's branch lands between the preview and the receive. The retry is the same
    // publication, so the `--yes` given for it still answers it, and it publishes.
    *hub.visibility.lock().unwrap() = "public";
    let before = previews();
    *hub.refuse_moved.lock().unwrap() = vec![("refs/heads/other".into(), tip.clone(), None)];
    let pushed = run(&["--yes", "push", "alice/app@work"]);
    assert_success(&pushed);
    assert!(text(&pushed).contains("previewing it again"), "{pushed:?}");
    assert!(!text(&pushed).contains(changed), "{pushed:?}");
    assert_eq!(previews(), before + 2);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/work"]),
        git(repo.root(), &["rev-parse", "refs/heads/work"])
    );
    *hub.visibility.lock().unwrap() = "private";

    // A receipt that had expired when its upload reached the Hub is previewed again the same way.
    // One refused for a reason a new preview of the same publication does not undo is told in
    // plain words, with nothing updated, and not previewed again.
    fs::write(work.join("README.md"), "Shared file, receipt expired").unwrap();
    git(&work, &["commit", "-am", "Receipt expired"]);
    let before = previews();
    *hub.refuse_moved.lock().unwrap() = vec![("refs/heads/other".into(), tip.clone(), None)];
    *hub.refuse_with.lock().unwrap() = Some("ERR HTTP 409 publication_receipt_invalid: expired\n");
    let pushed = run(&["--yes", "push", "alice/app@work"]);
    assert_success(&pushed);
    assert!(
        text(&pushed).contains(
            "this publication's receipt ran out before the upload reached the Hub; previewing it again"
        ),
        "{pushed:?}"
    );
    assert_eq!(previews(), before + 2);
    fs::write(work.join("README.md"), "Shared file, policy changed").unwrap();
    git(&work, &["commit", "-am", "Policy changed"]);
    *hub.refuse_moved.lock().unwrap() = vec![("refs/heads/other".into(), tip.clone(), None)];
    *hub.refuse_with.lock().unwrap() = Some("ERR HTTP 409 publication_receipt_invalid: policy\n");
    let refused = run(&["--yes", "push", "alice/app@work"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        !text(&refused).contains("previewing it again")
            && text(&refused).contains(
                "the repository's privacy policy changed on the Hub after this was previewed"
            )
            && text(&refused).contains("no branch or tag refs were updated"),
        "{refused:?}"
    );
    assert_success(&run(&["--yes", "push", "alice/app@work"]));
    let published = git(&remote, &["rev-parse", "refs/heads/work"]);

    // A push in three parts, refused at its second part while the repository turned public. The
    // retry plans from what the Hub holds, so its parts are numbered from one again: its first part
    // is what remained of the refused one, and a second part follows it. The `--yes` given for a
    // private publication answers neither, and nothing more is sent.
    for part in ["one", "two", "three", "four", "five", "six"] {
        fs::write(work.join("README.md"), format!("Shared file, part {part}")).unwrap();
        git(&work, &["commit", "-am", &format!("Part {part}")]);
    }
    let unpublished: Vec<String> = git(
        repo.root(),
        &[
            "rev-list",
            "--reverse",
            &format!("{published}..refs/heads/work"),
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    assert_eq!(unpublished.len(), 6);
    hub.max_new_commits.store(2, Ordering::Release);
    *hub.refuse_moved_from.lock().unwrap() = Some(unpublished[3].clone());
    *hub.refuse_moved.lock().unwrap() =
        vec![("refs/heads/fifth".into(), tip.clone(), Some("public"))];
    let refused = run(&["--yes", "push", "alice/app@work"]);
    hub.max_new_commits.store(4096, Ordering::Release);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(text(&refused).contains("in 3 parts"), "{refused:?}");
    assert!(text(&refused).contains("in 2 parts"), "{refused:?}");
    assert!(
        text(&refused).contains("previewing it again"),
        "{refused:?}"
    );
    assert!(text(&refused).contains(changed), "{refused:?}");
    assert!(
        text(&refused)
            .contains("publishing to a public repository needs the person's confirmation"),
        "{refused:?}"
    );
    assert!(hub.refuse_moved.lock().unwrap().is_empty());
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/work"]),
        unpublished[1]
    );
    *hub.visibility.lock().unwrap() = "private";
    let published = unpublished[1].clone();

    // The repository became public meanwhile: the `--yes` given for a private publication does not
    // answer a public one, and unattended the retry stops.
    fs::write(work.join("README.md"), "Shared file, revised").unwrap();
    git(&work, &["commit", "-am", "Revise file"]);
    let before = previews();
    *hub.refuse_moved.lock().unwrap() =
        vec![("refs/heads/later".into(), tip.clone(), Some("public"))];
    let refused = run(&["--yes", "push", "alice/app@work"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        text(&refused).contains("previewing it again"),
        "{refused:?}"
    );
    assert!(text(&refused).contains(changed), "{refused:?}");
    assert!(
        text(&refused)
            .contains("publishing to a public repository needs the person's confirmation"),
        "{refused:?}"
    );
    assert_eq!(previews(), before + 1);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), published);
    *hub.visibility.lock().unwrap() = "private";

    // Refused again on the retry: reported with the Hub's reason, and not tried a third time.
    let before = previews();
    *hub.refuse_moved.lock().unwrap() = vec![
        ("refs/heads/third".into(), tip.clone(), None),
        ("refs/heads/fourth".into(), tip.clone(), None),
    ];
    let refused = run(&["push", "alice/app@work"]);
    assert_eq!(refused.status.code(), Some(7), "{refused:?}");
    assert!(
        text(&refused).contains("the repository changed after this publication was previewed"),
        "{refused:?}"
    );
    assert_eq!(previews(), before + 2);
    assert!(hub.refuse_moved.lock().unwrap().is_empty());
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), published);

    // The person drops the conversation between the refusal and the retry. The retry reads the
    // project again before its preview and sends nothing.
    let withheld = home.join("projects").join("withheld");
    let before = previews();
    *hub.refuse_moved.lock().unwrap() = vec![("refs/heads/dropped".into(), tip.clone(), None)];
    *hub.write_on_refusal.lock().unwrap() = Some((
        withheld.clone(),
        json!([{"repository":"alice/app","branch":"work","session":null,"reason":"dropped","announced":false}])
            .to_string(),
    ));
    let refused = run(&["push", "alice/app@work"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        text(&refused).contains("will not be uploaded from this computer"),
        "{refused:?}"
    );
    assert!(
        text(&refused).contains("nothing more was sent"),
        "{refused:?}"
    );
    assert_eq!(previews(), before + 1);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), published);
    // Pushing the kept-back branch yourself is how the person learns why it is kept back: it
    // names the reason and sends nothing, not even a preview.
    let before = previews();
    let asked = run(&["--yes", "push", "alice/app@work"]);
    assert!(!asked.status.success(), "{asked:?}");
    assert!(
        text(&asked)
            .contains("will not be uploaded from this computer: the conversation was dropped"),
        "{asked:?}"
    );
    assert_eq!(previews(), before);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), published);
    fs::remove_file(&withheld).unwrap();

    // A push in parts whose retry is not the publication the `--yes` was given for: the repository
    // turned private meanwhile. Unattended, the retry's first part may go out on its own (plain
    // sessions to a private repository), but that is the policy's answer, not the person's, and it
    // covers no later part carrying encrypted sessions.
    repo.git(&["checkout", "-b", "talk"]).unwrap();
    let plain = meta::Meta::new(
        format!("agit-{}", "c".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let mut encrypted = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    encrypted.session_privacy = Some(SessionPrivacy {
        scheme: SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(
            AccountIdentity::new(&hub.base, "session-owner").unwrap(),
        ))
        .unwrap()
    });
    encrypted.committer_account = Some(AccountIdentity::new(&hub.base, "account-1").unwrap());
    let turn = |session: &meta::Meta, content: &str| {
        let log = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &session.session,
        );
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), session).unwrap();
        repo.add_all().unwrap();
        repo.commit(content).unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    let plain_turns: Vec<String> = (0..4)
        .map(|index| turn(&plain, &format!("plain turn {index}")))
        .collect();
    for index in 0..2 {
        turn(&encrypted, &format!("encrypted turn {index}"));
    }
    repo.git(&["checkout", "main"]).unwrap();
    hub.max_new_commits.store(4, Ordering::Release);
    *hub.visibility.lock().unwrap() = "public";
    *hub.refuse_moved.lock().unwrap() =
        vec![("refs/heads/sixth".into(), tip.clone(), Some("private"))];
    let stopped = run(&["--yes", "push", "alice/app@talk"]);
    hub.max_new_commits.store(4096, Ordering::Release);
    assert!(!stopped.status.success(), "{stopped:?}");
    assert!(text(&stopped).contains(changed), "{stopped:?}");
    assert!(
        text(&stopped).contains(
            "1 of 2 parts are published; part 2 carries encrypted sessions, which need the person's confirmation"
        ),
        "{stopped:?}"
    );
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/talk"]),
        plain_turns[3]
    );
    *hub.visibility.lock().unwrap() = "private";
    let held = git(&remote, &["rev-parse", "refs/heads/work"]);

    // The destination stays as it was, but the sessions change: the Hub gains the first of two
    // unpublished commits on another branch, so the retry's preview no longer carries it. The
    // `--yes` given for both does not answer what is left, and unattended the retry stops.
    let gained = git(repo.root(), &["rev-parse", "refs/heads/work"]);
    git(
        &remote,
        &[
            "fetch",
            "--quiet",
            &repo.root().to_string_lossy(),
            "refs/heads/work",
        ],
    );
    fs::write(work.join("README.md"), "Shared file, revised again").unwrap();
    git(&work, &["commit", "-am", "Revise file again"]);
    *hub.visibility.lock().unwrap() = "public";
    let before = previews();
    *hub.refuse_moved.lock().unwrap() = vec![("refs/heads/gained".into(), gained.clone(), None)];
    let refused = run(&["--yes", "push", "alice/app@work"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(text(&refused).contains(changed), "{refused:?}");
    assert!(
        text(&refused)
            .contains("publishing to a public repository needs the person's confirmation"),
        "{refused:?}"
    );
    assert_eq!(previews(), before + 1);
    let answered = hub.manifests.lock().unwrap().last().cloned().unwrap();
    assert!(
        answered["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|snapshot| snapshot["commit"] == gained),
        "{answered}"
    );
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), held);

    // Several branches with --keep-going, refused while the repository turned public. The group's
    // retry withdraws the `--yes`, and the branches then published again one at a time do not get
    // it back: none of them goes out.
    *hub.visibility.lock().unwrap() = "private";
    *hub.refuse_moved.lock().unwrap() =
        vec![("refs/heads/seventh".into(), tip.clone(), Some("public"))];
    let stopped = run(&["--yes", "push", "alice/app", "--all", "--keep-going"]);
    assert!(!stopped.status.success(), "{stopped:?}");
    assert!(text(&stopped).contains("one at a time"), "{stopped:?}");
    assert!(text(&stopped).contains(changed), "{stopped:?}");
    assert!(
        text(&stopped).contains("--yes no longer applies in this push"),
        "{stopped:?}"
    );
    assert!(hub.refuse_moved.lock().unwrap().is_empty());
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), held);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/talk"]),
        plain_turns[3]
    );

    // Several branches with --keep-going, and the person drops one conversation as the group is
    // refused. The group's retry stops on it, and so does that branch when it is published again on
    // its own: the command read the project before the group, and that read covers no later
    // publication. The other branch goes out.
    *hub.visibility.lock().unwrap() = "private";
    *hub.refuse_moved.lock().unwrap() = vec![("refs/heads/eighth".into(), tip.clone(), None)];
    *hub.write_on_refusal.lock().unwrap() = Some((
        withheld.clone(),
        json!([{"repository":"alice/app","branch":"work","session":null,"reason":"dropped","announced":false}])
            .to_string(),
    ));
    let partly = run(&["--yes", "push", "alice/app", "--all", "--keep-going"]);
    assert!(!partly.status.success(), "{partly:?}");
    assert!(text(&partly).contains("one at a time"), "{partly:?}");
    assert!(
        text(&partly).contains("`alice/app@work` will not be uploaded from this computer"),
        "{partly:?}"
    );
    assert!(
        text(&partly).contains("1 of 3 branches were not published: work"),
        "{partly:?}"
    );
    assert!(hub.refuse_moved.lock().unwrap().is_empty());
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), held);
    // talk ends on encrypted turns, which reach the Hub as their envelopes, not as the local
    // commits; its remote tip moving past the plain part published before is what shows the
    // fallback published it.
    let talk = git(&remote, &["rev-parse", "refs/heads/talk"]);
    assert_ne!(talk, plain_turns[3]);
    fs::remove_file(&withheld).unwrap();

    // The same without any retry: the group is refused for a reason the push does not retry,
    // and the person drops a conversation meanwhile. Each branch then published again on its own
    // is a later publication of the push, so it reads the project again, as a later group does.
    // talk gains a turn of the session it ended on, so the group reaches its receive whether or
    // not talk's earlier turns are already on the Hub.
    repo.git(&["checkout", "talk"]).unwrap();
    turn(&encrypted, "encrypted turn 2");
    repo.git(&["checkout", "main"]).unwrap();
    *hub.visibility.lock().unwrap() = "private";
    *hub.refuse_moved.lock().unwrap() = vec![("refs/heads/ninth".into(), tip.clone(), None)];
    *hub.refuse_with.lock().unwrap() =
        Some("ERR HTTP 413 quota_exceeded: the push exceeds the repository quota\n");
    *hub.write_on_refusal.lock().unwrap() = Some((
        withheld.clone(),
        json!([{"repository":"alice/app","branch":"talk","session":null,"reason":"dropped","announced":false}])
            .to_string(),
    ));
    let partly = run(&["--yes", "push", "alice/app", "--all", "--keep-going"]);
    assert!(!partly.status.success(), "{partly:?}");
    assert!(!text(&partly).contains("previewing it again"), "{partly:?}");
    assert!(text(&partly).contains("one at a time"), "{partly:?}");
    assert!(
        text(&partly).contains("`alice/app@talk` will not be uploaded from this computer"),
        "{partly:?}"
    );
    assert!(
        text(&partly).contains("1 of 3 branches were not published: talk"),
        "{partly:?}"
    );
    assert!(hub.refuse_moved.lock().unwrap().is_empty());
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/talk"]), talk);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/work"]),
        git(repo.root(), &["rev-parse", "refs/heads/work"])
    );
}

/// The first upload counts as soon as the Hub acknowledged a branch it updated, before anything
/// after that can fail. Here a push carries more tags than fit in one receive with its branch, so
/// they follow the branch in receives of their own, and the Hub refuses them: the push fails, and
/// its retry finds the branch up to date. Counting only at the end of a push that fully succeeded
/// would miss this installation's first upload for good.
#[test]
fn a_first_upload_counts_once_the_hub_acknowledged_a_branch() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [29; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("AGIT_TELEMETRY_HOST", "http://127.0.0.1:9")
            .env("AGIT_TELEMETRY_KEY", "synthetic_project")
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let telemetry = home.join("telemetry");
    let uploads = || {
        let queue: serde_json::Value = fs::read(telemetry.join("queue.json"))
            .ok()
            .and_then(|body| serde_json::from_slice(&body).ok())
            .unwrap_or_default();
        queue["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| entry["event"]["event"] == "cli_first_upload")
            .count()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["config", "tag.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    // The repository is created and first published while statistics are still undecided, so
    // nothing is counted yet.
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    assert_eq!(uploads(), 0);

    {
        fs::create_dir_all(&telemetry).unwrap();
        let gate = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(telemetry.join("gate.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&gate).unwrap();
        let preferences = json!({
            "preference": "enabled",
            "generation": 1,
            "device_id": uuid::Uuid::new_v4(),
            "decision_source": "explicit_enable",
            "notice_version": agit::telemetry::state::NOTICE_VERSION,
        });
        fs::write(telemetry.join("preferences.json"), preferences.to_string()).unwrap();
    }
    hub.refuse_tags.store(true, Ordering::Release);
    fs::write(repo.root().join("README.md"), "Shared file, revised").unwrap();
    repo.add_all().unwrap();
    repo.commit("Revise file").unwrap();
    git(repo.root(), &["tag", "release-2"]);
    let head = git(repo.root(), &["rev-parse", "HEAD"]);
    let more: String = (0..256)
        .map(|n| format!("create refs/tags/release-2-{n:03} {head}\n"))
        .collect();
    let mut update = git_command(repo.root())
        .arg("-C")
        .arg(repo.root())
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    update
        .stdin
        .take()
        .unwrap()
        .write_all(more.as_bytes())
        .unwrap();
    assert!(update.wait().unwrap().success());
    let refused = run(&["--yes", "push", "alice/app@main"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/main"]),
        git(repo.root(), &["rev-parse", "refs/heads/main"])
    );
    assert!(git(&remote, &["for-each-ref", "refs/tags/release-2"]).is_empty());
    assert_eq!(uploads(), 1, "{refused:?}");

    hub.refuse_tags.store(false, Ordering::Release);
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    assert!(!git(&remote, &["for-each-ref", "refs/tags/release-2"]).is_empty());
    assert_eq!(uploads(), 1);
}

/// A fork of an encrypted snapshot that carries only what that snapshot published goes up on the
/// Hub that holds the snapshot. Delivered to another Hub, which doesn't hold it, nothing of the
/// envelope and nothing the projection hid goes along: the delivery is refused, or the target gets
/// trees with no envelope in them.
#[test]
fn a_fork_of_an_encrypted_snapshot_takes_no_envelope_to_another_hub() {
    use agit::domain::session_privacy::{AccountIdentity, SessionPrivacy};
    const CANARY: &str = "canary-f0rk";
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let source_root = temp.path().join("source");
    let target_root = temp.path().join("target");
    let source = Hub::start(&source_root, [25; 32]);
    let target = Hub::start(&target_root, [26; 32]);
    for hub in [&source, &target] {
        save_at(
            &home
                .join("credentials")
                .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
            &HubCredential {
                account_id: Some("account-1".into()),
                username: "alice".into(),
                email: None,
                hub: Some(hub.base.clone()),
                access_token: "synthetic-access".into(),
                refresh_token: "synthetic-refresh".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    }
    let run = |hub: &Hub, args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let remote = source_root.join("alice/app.git");
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    for (key, value) in [
        ("http.userAgent", TRANSPORT_USER_AGENT),
        ("user.name", "Source Writer"),
        ("user.email", "source@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        repo.git(&["config", key, value]).unwrap();
    }
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&source, &["--yes", "push", "alice/app@main"]));
    repo.git(&["checkout", "-q", "-b", "work"]).unwrap();
    let mut encrypted = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    encrypted.session_privacy = Some(SessionPrivacy {
        scheme: agit::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(
            AccountIdentity::new(&source.base, "account-1").unwrap(),
        ))
        .unwrap()
    });
    encrypted.committer_account = Some(AccountIdentity::new(&source.base, "account-1").unwrap());
    let log = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":format!("please read /Users/alice/{CANARY}/plan.txt")}})
        ),
        "claude-code",
        &encrypted.session,
    );
    write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &encrypted).unwrap();
    repo.add_all().unwrap();
    repo.commit("encrypted turn").unwrap();
    source
        .legacy_encrypted
        .lock()
        .unwrap()
        .insert(encrypted.session.clone());
    assert_success(&run(&source, &["--yes", "push", "alice/app@work"]));
    source.legacy_encrypted.lock().unwrap().clear();
    let published = git(&remote, &["rev-parse", "refs/heads/work"]);
    assert!(
        git(&remote, &["ls-tree", "-r", "--name-only", &published])
            .lines()
            .any(|path| path == "privacy/envelope.json"),
        "published encrypted"
    );

    // A fork of the Hub's copy that carries only what it published, plus a turn of its own.
    repo.git(&[
        "fetch",
        "-q",
        remote.to_str().unwrap(),
        "refs/heads/work:refs/remotes/hub/work",
    ])
    .unwrap();
    repo.git(&["checkout", "-q", "-b", "forked", &published])
        .unwrap();
    repo.git(&["rm", "-rq", "privacy"]).unwrap();
    let forked = meta::Meta::new(
        format!("agit-{}", "b".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let mut log = storage::materialize_at(repo.root(), &published, meta::LOG_FILE).unwrap();
    log.push_str(&transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"a turn on the fork"}})
        ),
        "claude-code",
        &forked.session,
    ));
    write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &forked).unwrap();
    repo.add_all().unwrap();
    repo.commit("fork").unwrap();
    assert_success(&run(&source, &["--yes", "push", "alice/app@forked"]));

    // Delivered to a Hub that doesn't hold the encrypted snapshot.
    let delivered = run(
        &source,
        &[
            "--yes",
            "push",
            "alice/app@forked",
            "--to-hub",
            &target.base,
        ],
    );
    let target_remote = target_root.join("alice/app.git");
    if target_remote.is_dir() {
        for commit in git(&target_remote, &["rev-list", "--all"]).lines() {
            assert!(
                !git(&target_remote, &["ls-tree", "-r", "--name-only", commit])
                    .lines()
                    .any(|path| path.starts_with("privacy/")),
                "{commit} took an envelope to the other Hub: {delivered:?}"
            );
        }
        for line in git(&target_remote, &["rev-list", "--all", "--objects"]).lines() {
            let oid = line.split_whitespace().next().unwrap();
            if git(&target_remote, &["cat-file", "-t", oid]) == "blob" {
                assert!(
                    !git(&target_remote, &["cat-file", "blob", oid]).contains(CANARY),
                    "a hidden value reached the other Hub"
                );
            }
        }
    }
    if !delivered.status.success() {
        assert!(
            !target_remote.is_dir()
                || git(&target_remote, &["for-each-ref", "refs/heads/forked"]).is_empty(),
            "{delivered:?}"
        );
    }
}

#[test]
fn cross_hub_delivery_publishes_target_objects_and_continues_with_ordinary_push() {
    use agit::domain::session_privacy::{AccountIdentity, SessionPrivacy};
    use agit::hub::identity::{self, RemoteIdentity};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let source_root = temp.path().join("source");
    let target_root = temp.path().join("target");
    let source = Hub::start(&source_root, [25; 32]);
    let target = Hub::start(&target_root, [26; 32]);
    for hub in [&source, &target] {
        save_at(
            &home
                .join("credentials")
                .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
            &HubCredential {
                account_id: Some("account-1".into()),
                username: "alice".into(),
                email: None,
                hub: Some(hub.base.clone()),
                access_token: "synthetic-access".into(),
                refresh_token: "synthetic-refresh".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    }
    let run_at = |hub: &str, args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", hub)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let run = |hub: &Hub, args: &[&str]| run_at(&hub.base, args);
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    for (key, value) in [
        ("http.userAgent", TRANSPORT_USER_AGENT),
        ("user.name", "Source Writer"),
        ("user.email", "source@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        repo.git(&["config", key, value]).unwrap();
    }
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    repo.git(&["checkout", "-b", "work"]).unwrap();
    let source_account = AccountIdentity::new(&source.base, "account-1").unwrap();
    let mut metadata = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        "/work".into(),
    );
    metadata.session_privacy = None;
    metadata.committer_account = Some(source_account);
    fs::write(repo.root().join("artifact.txt"), "Delivered artifact\n").unwrap();
    let mut claim = metadata.clone();
    claim.session.clear();
    write_snapshot(repo.root(), "", "").unwrap();
    meta::write(repo.root(), &claim).unwrap();
    repo.add_all().unwrap();
    repo.commit("Claim session").unwrap();
    let save = |metadata: &meta::Meta, contents: &[&str], message: &str| {
        let log = transcript::wrap_lines(
            &contents
                .iter()
                .map(|content| {
                    format!(
                        "{}\n",
                        json!({"type":"user","message":{"role":"user","content":content}})
                    )
                })
                .collect::<String>(),
            "claude-code",
            &metadata.session,
        );
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), metadata).unwrap();
        repo.add_all().unwrap();
        assert!(
            repo.commit(message).unwrap(),
            "fixture must create {message}"
        );
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    let first = save(&metadata, &["First turn"], "Subject\n\nMilestone details.");
    repo.git(&["config", "author.name", "Other Author"])
        .unwrap();
    repo.git(&["config", "author.email", "other@example.invalid"])
        .unwrap();
    save(
        &metadata,
        &["First turn", "Other author's turn"],
        "Other author, own committer",
    );
    repo.git(&["config", "--unset", "author.name"]).unwrap();
    repo.git(&["config", "--unset", "author.email"]).unwrap();
    let mut collaborator = metadata.clone();
    collaborator.committer_account = Some(AccountIdentity::new(&source.base, "account-2").unwrap());
    save(&collaborator, &["First turn"], "Collaborator contribution");
    // An ordinary push of a session first published on another Hub registers it as the pusher's
    // only when this device is signed in there as its creator, or when its birth names no Hub.
    // Someone else's session from the target Hub is refused, named, before anything reaches the
    // source Hub; one that re-owned every such session would register it as account-1's here.
    // An admitted one names neither the other Hub's registration nor its accounts, including one
    // whose metadata predates recorded privacy and names its birth only by its committer.
    for (branch, id, birth, recorded, admitted) in [
        ("theirs", "d", Some("account-2"), true, false),
        ("mine", "e", Some("account-1"), true, true),
        ("legacy", "7", Some("account-1"), false, true),
        ("offline", "f", None, false, true),
    ] {
        repo.git(&["checkout", "-b", branch, &format!("{first}~2")])
            .unwrap();
        let mut elsewhere = metadata.clone();
        elsewhere.session = format!("agit-{}", id.repeat(40));
        let creator = birth.map(|creator| AccountIdentity::new(&target.base, creator).unwrap());
        elsewhere.session_privacy = recorded.then(|| SessionPrivacy::new(creator.clone()).unwrap());
        elsewhere.committer_account = creator;
        let mut claim = elsewhere.clone();
        claim.session.clear();
        write_snapshot(repo.root(), "", "").unwrap();
        meta::write(repo.root(), &claim).unwrap();
        repo.add_all().unwrap();
        repo.commit("Claim session").unwrap();
        save(&elsewhere, &["Session from elsewhere"], "Elsewhere");
        let before = (
            source.registrations.load(Ordering::Acquire),
            source.posts.load(Ordering::Acquire),
        );
        let pushed = run(&source, &["--yes", "push", &format!("alice/app@{branch}")]);
        repo.git(&["checkout", "work"]).unwrap();
        if !admitted {
            assert!(!pushed.status.success(), "{pushed:?}");
            let stderr = String::from_utf8_lossy(&pushed.stderr);
            assert!(
                stderr.contains(&format!(
                    "session {} was first published on",
                    elsewhere.session
                )) && stderr.contains("cannot be published here as yours"),
                "{pushed:?}"
            );
            assert_eq!(
                (
                    source.registrations.load(Ordering::Acquire),
                    source.posts.load(Ordering::Acquire),
                ),
                before
            );
            continue;
        }
        assert_success(&pushed);
        let tip = git(
            &source_root.join("alice/app.git"),
            &["rev-parse", &format!("refs/heads/{branch}")],
        );
        let published: serde_json::Value = serde_json::from_str(&git(
            &source_root.join("alice/app.git"),
            &["show", &format!("{tip}:session/meta.json")],
        ))
        .unwrap();
        assert_eq!(published["session"], elsewhere.session);
        assert!(published["committer_account"].is_null(), "{published}");
        if birth.is_some() {
            assert_eq!(
                published["session_privacy"]["hub"], source.base,
                "{branch}: {published}"
            );
            assert_eq!(published["session_privacy"]["creator"], "account-1");
        }
    }
    assert_success(&run(&source, &["--yes", "push", "alice/app@work"]));
    let source_refs = git(&source_root.join("alice/app.git"), &["show-ref"]);
    for (branch, hub, creator) in [
        ("foreign", source.base.as_str(), "account-2"),
        ("unknown", "https://unknown.example", "account-1"),
    ] {
        repo.git(&["checkout", "-b", branch]).unwrap();
        let mut foreign = metadata.clone();
        foreign.session = format!("agit-{}", "c".repeat(40));
        foreign.session_privacy =
            Some(SessionPrivacy::new(Some(AccountIdentity::new(hub, creator).unwrap())).unwrap());
        save(&foreign, &["Foreign session"], "Foreign creator");
        let refused = run(
            &source,
            &[
                "--yes",
                "push",
                &format!("alice/app@{branch}"),
                "--to-hub",
                &target.base,
            ],
        );
        assert!(!refused.status.success(), "{refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .contains("cannot verify this session's creator"),
            "{refused:?}"
        );
        assert_eq!(target.registrations.load(Ordering::Acquire), 0);
        assert_eq!(target.posts.load(Ordering::Acquire), 0);
        repo.git(&["checkout", "work"]).unwrap();
    }
    let refused = run(&target, &["--yes", "push", "alice/app@work"]);
    assert!(!refused.status.success());
    assert_eq!(target.posts.load(Ordering::Acquire), 0);
    target.corrupt_receipt.store(true, Ordering::Release);
    let failed = run(
        &source,
        &["--yes", "push", "alice/app@work", "--to-hub", &target.base],
    );
    assert!(!failed.status.success());
    assert_eq!(
        identity::read(&repo).unwrap(),
        Some(RemoteIdentity::new(&source.base, AGENT_ID).unwrap())
    );
    assert_eq!(
        repo.remote_url().unwrap(),
        format!("{}/alice/app.git", source.base)
    );
    assert_eq!(target.posts.load(Ordering::Acquire), 0);
    target.corrupt_receipt.store(false, Ordering::Release);
    let unconfirmed = run(
        &source,
        &["push", "alice/app@work", "--to-hub", &target.base],
    );
    assert!(!unconfirmed.status.success());
    assert_eq!(target.posts.load(Ordering::Acquire), 0);
    assert_success(&run(
        &source,
        &["--yes", "push", "alice/app@work", "--to-hub", &target.base],
    ));
    let remote = target_root.join("alice/app.git");
    let published = git(&remote, &["rev-parse", "refs/heads/work"]);
    assert_ne!(published, first);
    assert_eq!(
        git(&remote, &["show", &format!("{published}:artifact.txt")]),
        "Delivered artifact"
    );
    let published_meta: serde_json::Value = serde_json::from_str(&git(
        &remote,
        &["show", &format!("{published}:session/meta.json")],
    ))
    .unwrap();
    let target_session = published_meta["session"].as_str().unwrap().to_owned();
    assert_ne!(target_session, metadata.session);
    let public_log = storage::materialize_at(&remote, &published, meta::LOG_FILE).unwrap();
    assert!(!public_log.contains(&metadata.session));
    assert!(public_log.contains(&target_session));
    assert!(
        !git_command(&remote)
            .arg("-C")
            .arg(&remote)
            .args(["cat-file", "-e", &first])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(published_meta["session_privacy"]["hub"], target.base);
    assert!(published_meta["committer_account"].is_null());
    assert_eq!(published_meta["session_privacy"]["creator"], "account-1");
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%B", &published]),
        "Collaborator contribution"
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%ae", &published]),
        "unmapped@agentgit.local"
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%ce", &published]),
        "unmapped@agentgit.local"
    );
    let authored = format!("{published}^");
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%ae", &authored]),
        "unmapped@agentgit.local"
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%ce", &authored]),
        "account-1@agentgit.local"
    );
    let owned = format!("{published}~2");
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%ae", &owned]),
        "account-1@agentgit.local"
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%B", &owned]),
        "Subject\n\nMilestone details."
    );
    assert_eq!(
        identity::read(&repo).unwrap(),
        Some(RemoteIdentity::new(&target.base, AGENT_ID).unwrap())
    );
    let source_after = meta::read_at_ref_result(&repo, &first).unwrap().unwrap();
    assert_eq!(source_after.session, metadata.session);
    metadata.committer_account = Some(AccountIdentity::new(&target.base, "account-1").unwrap());
    metadata.turn = Some(2);
    let second = save(
        &metadata,
        &["First turn", "Second turn"],
        "Second subject\n\nSecond body.",
    );
    repo.git(&["remote", "remove", "origin"]).unwrap();
    assert_success(&run(&target, &["--yes", "push", "alice/app@work"]));
    assert_eq!(
        repo.remote_url().as_deref(),
        Some(format!("{}/alice/app.git", target.base).as_str())
    );
    let continued = git(&remote, &["rev-parse", "refs/heads/work"]);
    assert_ne!(continued, second);
    assert_eq!(
        git(&remote, &["rev-parse", &format!("{continued}^")]),
        published
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%B", &continued]),
        "Second subject\n\nSecond body."
    );
    let continued_meta: serde_json::Value = serde_json::from_str(&git(
        &remote,
        &["show", &format!("{continued}:session/meta.json")],
    ))
    .unwrap();
    assert_eq!(continued_meta["session"], target_session);
    assert_eq!(continued_meta["committer_account"]["hub"], target.base);
    let received = target.posts.load(Ordering::Acquire);
    repo.set_remote("https://obsolete.example/alice/app.git")
        .unwrap();
    assert_success(&run(&target, &["--yes", "push", "alice/app@work"]));
    assert_eq!(target.posts.load(Ordering::Acquire), received);
    assert_eq!(
        repo.remote_url().unwrap(),
        format!("{}/alice/app.git", target.base)
    );
    assert_success(&run(&target, &["push", "alice/app@work", "--dry-run"]));
    assert_eq!(target.posts.load(Ordering::Acquire), received);
    assert_eq!(
        git(&source_root.join("alice/app.git"), &["show-ref"]),
        source_refs
    );
    for manifest in target.manifests.lock().unwrap().iter() {
        for snapshot in manifest["snapshots"].as_array().unwrap() {
            if let Some(session) = snapshot.get("session_id") {
                let commit = snapshot["commit"].as_str().unwrap();
                let actual: serde_json::Value = serde_json::from_str(&git(
                    &remote,
                    &["show", &format!("{commit}:session/meta.json")],
                ))
                .unwrap();
                assert_eq!(&actual["session"], session);
                assert_eq!(session, &target_session);
            }
        }
    }

    // The remote list is read from this checkout alone: with both Hubs gone it still names the
    // delivered destination as origin, and the account, or the sign-in command, is the one for
    // that Hub, not for the Hub `AGIT_HUB_URL` selects. A list derived from the selected Hub names
    // the source.
    let (source_base, target_base) = (source.base.clone(), target.base.clone());
    drop(source);
    drop(target);
    // Listing prepares nothing, even in a state directory whose startup migration has not been
    // recorded: nothing under it but usage statistics is created or changed. A listing that took
    // the default startup would migrate, record the migration and judge project claims first.
    let _ = std::fs::remove_file(home.join("layout-v1.complete"));
    let state = || {
        walkdir::WalkDir::new(&home)
            .into_iter()
            .filter_entry(|entry| entry.path() != home.join("telemetry"))
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                let meta = entry.metadata().unwrap();
                (entry.into_path(), meta.len(), meta.modified().unwrap())
            })
            .collect::<Vec<_>>()
    };
    let before = state();
    let listed = run_at(&source_base, &["--json", "remote", "-v", "alice/app"]);
    assert_success(&listed);
    assert!(
        run_at(&source_base, &["remote", "alice/app"])
            .status
            .success()
    );
    assert_eq!(state(), before);
    assert!(!home.join("layout-v1.complete").exists());
    let document: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(
        document["result"]["value"],
        serde_json::json!({
            "repository": "alice/app",
            "remotes": [{
                "name": "origin",
                "hub": target_base,
                "address": format!("{target_base}/alice/app"),
                "account": "alice",
                "upstream": true,
            }],
        })
    );
    std::fs::remove_file(home.join("credentials").join(format!(
        "{}.json",
        config::hub_host_key(&target_base).unwrap()
    )))
    .unwrap();
    let signed_out = run_at(&source_base, &["remote", "-v", "alice/app"]);
    assert_success(&signed_out);
    assert_eq!(
        String::from_utf8_lossy(&signed_out.stdout),
        format!(
            "origin\t{target_base}/alice/app\tnot signed in: agit login --hub {target_base} · upstream\n"
        )
    );
    // An origin on that Hub that carries a query is not an address to print: the code-origin
    // sanitizer would change it, so neither the text nor the JSON shows any of it.
    repo.set_remote(&format!("{target_base}/alice/app.git?token=SENTINEL"))
        .unwrap();
    for args in [
        &["remote", "-v", "alice/app"][..],
        &["--json", "remote", "-v", "alice/app"],
    ] {
        let listed = run_at(&source_base, args);
        assert_success(&listed);
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&listed.stdout),
            String::from_utf8_lossy(&listed.stderr)
        );
        assert!(
            said.contains("origin") && !said.contains("SENTINEL"),
            "{said}"
        );
    }
}

/// A plain session that continues its published history declares only what it adds: the next
/// push preserves just the heads the Hub already holds and snapshots just the new turns, so its
/// size follows the push, not the session. Its tip and the new turns' version tags go in one
/// receive under one receipt; sending the tags after the branch costs a second preview,
/// confirmation and receive on every push. A walk of the whole history declares every earlier turn
/// again. If a branch head of the advertisement that history was read from moves before a receipt
/// binds it, the declaration may leave out commits the Hub no longer holds, so the publication is
/// refused before anything is sent.
///
/// The bound applies to commits, not to tags: a tag the branch reaches below the boundary that the
/// Hub lacks still arrives, both one added to an earlier turn after it was published and a version
/// tag of a published turn the Hub lost, as when a tags phase fails after the heads land, and the
/// push that carries them still declares only the new turn. A plan that takes only its own
/// commits' tags leaves both off the Hub on every later push; one that publishes them by capturing
/// the whole history again declares every turn, and past the commit bound cannot push at all. The
/// added tag arrives without a new turn too: a push that skips a branch because its tip is still
/// the one its receipt acknowledged, without checking the tags, reports it up to date and never
/// sends the tag. Once the tag is acknowledged the branch is skipped again; a tag listing that is
/// recorded differently from how the skip reads it would never skip anything.
#[test]
fn continued_plain_session_declares_only_what_it_adds() {
    use agit::domain::session_privacy::SessionPublicationManifest;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [26; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    let base = git(repo.root(), &["rev-parse", "HEAD"]);
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    repo.git(&["checkout", "-b", "talk"]).unwrap();
    let session = meta::Meta::new(
        format!("agit-{}", "b".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let turn = |content: &str| {
        let log = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &session.session,
        );
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), &session).unwrap();
        repo.add_all().unwrap();
        repo.commit(content).unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    let first: Vec<String> = (0..3).map(|index| turn(&format!("turn {index}"))).collect();
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    let published = git(repo.root(), &["rev-parse", "HEAD"]);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/talk"]), published);

    let earlier = hub.previews.lock().unwrap().len();
    let receives = hub.posts.load(Ordering::Acquire);
    let added = std::collections::BTreeSet::from([turn("turn 3"), turn("turn 4")]);
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    let head = git(repo.root(), &["rev-parse", "HEAD"]);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/talk"]), head);
    for turn in &added {
        assert_eq!(
            git(&remote, &["rev-parse", &format!("refs/tags/agit-{turn}")]),
            *turn
        );
    }
    assert_eq!(hub.posts.load(Ordering::Acquire) - receives, 1);
    let previews = hub.previews.lock().unwrap()[earlier..].to_vec();
    assert_eq!(previews.len(), 1);
    for preview in previews {
        let declared: SessionPublicationManifest =
            serde_json::from_value(preview["manifest"].clone()).unwrap();
        assert_eq!(
            declared
                .preserved_commits
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([base.clone(), published.clone()])
        );
        assert_eq!(
            declared
                .snapshots
                .iter()
                .map(|snapshot| snapshot.commit().to_owned())
                .collect::<std::collections::BTreeSet<_>>(),
            added
        );
    }

    // A new branch off the published tip continues history another branch accepted and the
    // destination holds: it declares only its own turn, though it has accepted nothing itself.
    repo.git(&["checkout", "-b", "other"]).unwrap();
    let other = meta::Meta::new(
        format!("agit-{}", "d".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let log = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"another session"}})
        ),
        "claude-code",
        &other.session,
    );
    write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &other).unwrap();
    repo.add_all().unwrap();
    repo.commit("another session").unwrap();
    let branched = git(repo.root(), &["rev-parse", "HEAD"]);
    let earlier = hub.previews.lock().unwrap().len();
    assert_success(&run(&["--yes", "push", "alice/app@other"]));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/other"]), branched);
    for preview in &hub.previews.lock().unwrap()[earlier..] {
        let declared: SessionPublicationManifest =
            serde_json::from_value(preview["manifest"].clone()).unwrap();
        assert_eq!(
            declared
                .snapshots
                .iter()
                .map(|snapshot| snapshot.commit().to_owned())
                .collect::<Vec<_>>(),
            std::slice::from_ref(&branched)
        );
    }
    repo.git(&["checkout", "talk"]).unwrap();

    let version = format!("refs/tags/agit-{}", first[1]);
    assert_eq!(git(&remote, &["rev-parse", &version]), first[1]);
    // As after `agit fetch`: a push decides from local records alone only while the checkout's
    // main is the one it last fetched.
    git(
        repo.root(),
        &["update-ref", "refs/remotes/origin/main", "refs/heads/main"],
    );
    git(repo.root(), &["tag", "reviewed", &first[0]]);
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    assert_eq!(git(&remote, &["rev-parse", "refs/tags/reviewed"]), first[0]);
    let unchanged = run(&["--yes", "push", "alice/app@talk"]);
    assert_success(&unchanged);
    assert!(
        format!(
            "{}{}",
            String::from_utf8_lossy(&unchanged.stdout),
            String::from_utf8_lossy(&unchanged.stderr)
        )
        .contains("unchanged since their acknowledged publication: talk"),
        "{unchanged:?}"
    );
    git(&remote, &["update-ref", "-d", &version]);
    let earlier = hub.previews.lock().unwrap().len();
    let fifth = turn("turn 5");
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    assert_eq!(git(&remote, &["rev-parse", &version]), first[1]);
    assert_eq!(git(&remote, &["rev-parse", "refs/tags/reviewed"]), first[0]);
    let previews = hub.previews.lock().unwrap()[earlier..].to_vec();
    assert!(!previews.is_empty());
    for preview in previews {
        let declared: SessionPublicationManifest =
            serde_json::from_value(preview["manifest"].clone()).unwrap();
        assert_eq!(
            declared
                .preserved_commits
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([base.clone(), head.clone()])
        );
        assert_eq!(
            declared
                .snapshots
                .iter()
                .map(|snapshot| snapshot.commit().to_owned())
                .collect::<Vec<_>>(),
            std::slice::from_ref(&fifth)
        );
    }

    turn("turn 6");
    let earlier = hub.previews.lock().unwrap().len();
    let received = hub.posts.load(Ordering::Acquire);
    *hub.move_on_strategy.lock().unwrap() = Some(("refs/heads/talk".into(), published.clone()));
    let refused = run(&["--yes", "push", "alice/app@talk"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        format!(
            "{}{}",
            String::from_utf8_lossy(&refused.stdout),
            String::from_utf8_lossy(&refused.stderr)
        )
        .contains("changed during this publication"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), received);
    assert_eq!(hub.previews.lock().unwrap().len(), earlier);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/talk"]), published);

    // An unreadable accepted record has no user-side repair: the refusal names the branch, sends
    // nothing, keeps local commits, and asks for a report instead of an operation to run.
    *hub.move_on_strategy.lock().unwrap() = None;
    let local = git(repo.root(), &["rev-parse", "HEAD"]);
    for scope in fs::read_dir(repo.root().join(".git/agit/privacy-accepted")).unwrap() {
        fs::write(scope.unwrap().path().join("index.enc"), [0; 64]).unwrap();
    }
    let refused = run(&["--yes", "push", "alice/app@talk"]);
    assert!(!refused.status.success(), "{refused:?}");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("cannot push alice/app@talk; nothing was sent to the Hub")
            && stderr.contains("report the problem")
            && stderr.contains("`agit status`"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), received);
    assert_eq!(git(repo.root(), &["rev-parse", "HEAD"]), local);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/talk"]), published);
}

/// A plain session publishes its conversation records and nothing else the runtime wrote: a
/// pushed object never holds the instructions, memory or AGENTS.md text a Claude Code or Codex
/// transcript carries, while the local snapshot keeps the complete transcript. A session first
/// published while it held conversation only keeps that commit, and its next snapshot builds on it.
/// Records and a commit message recorded without protection, as ones captured before a rule
/// existed, are published as capture protects them today: the value they hold goes out as a
/// placeholder. One record holds a quoted assignment, which capture's judgement of the decoded
/// field finds and a scan of the serialized record does not, so a push that scanned the line would
/// send it. A line the Hub already holds stays as it was published, even when a value registered
/// since then is in it, so the LOG still extends the published one; a new line holding that value
/// gets a placeholder. A push that rechecked published lines would rewrite them, and one that
/// rechecked none would send the new value. A record protection changed when it was published
/// keeps its published line the same way, in a merge as in a turn; a push that matched lines only
/// by their unprotected form would protect it again under the newer value and drop the published
/// line. A file holding such a value refuses its publication by name, with nothing sent; a check
/// of generic rules alone would miss it, and one that skipped what it cannot read would send a
/// large text file unchecked. So does a value that spans what a file held and what is appended to
/// it, which a check of the appended bytes alone misses, and a file whose bytes are also a session
/// record's, which a check that took the record's pass for the file's would send.
#[test]
fn plain_publication_carries_only_conversation_records() {
    const CLAUDE_MD: &str = "CANARY-PROJECT-CLAUDE-MD-7f3a";
    const MEMORY_MD: &str = "CANARY-AUTO-MEMORY-MD-91c2";
    const AGENTS_MD: &str = "CANARY-PROJECT-AGENTS-MD-4be8";
    const GLOBAL_AGENTS_MD: &str = "CANARY-GLOBAL-CODEX-AGENTS-MD-d05e";
    const SR_USER: &str = "CANARY-SYSTEM-REMINDER-IN-USER-RECORD-31aa";
    const SR_TOOL: &str = "CANARY-SYSTEM-REMINDER-IN-TOOL-RESULT-6c0b";
    const OLD_CODEX: &str = "CANARY-OLDER-CODEX-USER-INSTRUCTIONS-e7d4";
    const CURSOR_UI: &str = "CANARY-CURSOR-USER-INFO-58f1";
    const CURSOR_OUTSIDE: &str = "CANARY-CURSOR-OUTSIDE-USER-QUERY-a29c";
    const ORPHAN_EVENT: &str = "CANARY-EVENT-OBJECT-NO-LOG-NAMES-0f9e";
    const ATTACHMENT_BODY: &str = "CANARY-ATTACHMENT-FILE-BODY-5d17";
    const PROMPT_SNAPSHOT: &str = "CANARY-TRAILING-PROMPT-SNAPSHOT-a6e3";
    const REQUEST_BLOB: &str = "CANARY-API-REQUEST-BLOB-BODY-2e90";
    const REQUEST: &str = "CANARY-API-REQUEST-BODY-7b14";
    const FILE_DELTA: &str = "CANARY-FILE-HISTORY-DELTA-CONTENT-d3c8";
    const CODEX_TURN_CONTEXT: &str = "CANARY-CODEX-TURN-CONTEXT-INSTRUCTIONS-5e21";
    const CODEX_WORLD_STATE: &str = "CANARY-CODEX-WORLD-STATE-9b07";
    const CODEX_SETTINGS: &str = "CANARY-CODEX-THREAD-SETTINGS-APPLIED-3f6a";
    const UNPROTECTED: &str = "AKIA2E7YQXK4NMZ5VJ3T";
    const AFTER_NEWLINE: &str = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
    const QUOTED: &str = "CedarBranchPetalRiver";
    const REGISTERED: &str = "heron-ledger-7731";
    const LATER: &str = "otter-quay-5512";
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let agit = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .args(args);
        command
    };
    let run = |args: &[&str]| agit(args).stdin(Stdio::null()).output().unwrap();
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));

    let snapshot = |branch: &str, runtime: &str, id: char, records: &[serde_json::Value]| {
        let session = meta::Meta::new(
            format!("agit-{}", id.to_string().repeat(40)),
            runtime.into(),
            String::new(),
        );
        let text: String = records.iter().map(|record| format!("{record}\n")).collect();
        let log = transcript::wrap_lines(&text, runtime, &session.session);
        repo.git(&["checkout", "-B", branch]).unwrap();
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), &session).unwrap();
        repo.add_all().unwrap();
        repo.commit(&format!("{branch} {}", records.len())).unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    let reminder = |body: &str| format!("<system-reminder>\n{body}\n</system-reminder>");
    let mut claude = vec![
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":format!("fix the failing test in {REGISTERED}")}}),
        json!({"type":"assistant","uuid":"u2","parentUuid":"u1","sessionId":"s","message":{"role":"assistant","content":[{"type":"text","text":"Looking."}]}}),
    ];
    repo.git(&["checkout", "main"]).unwrap();
    let clean = snapshot("talk", "claude-code", 'c', &claude);
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/talk"]), clean);
    let (first_log, _) = storage::materialize_pair_at(&remote, &clean).unwrap();
    let register = |name: &str, value: &str| {
        let mut add = agit(&["secrets", "add", name, "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        add.stdin
            .take()
            .unwrap()
            .write_all(value.as_bytes())
            .unwrap();
        assert_success(&add.wait_with_output().unwrap());
    };
    // A value registered after that line was published.
    register("later", REGISTERED);

    claude.extend([
        json!({"type":"attachment","uuid":"u3","parentUuid":"u2","sessionId":"s","attachment":{"type":"instructions","path":"/work/CLAUDE.md","content":CLAUDE_MD}}),
        json!({"type":"attachment","uuid":"u4","parentUuid":"u3","sessionId":"s","attachment":{"type":"instructions","path":"/home/p/.claude/projects/w/memory/MEMORY.md","content":MEMORY_MD}}),
        json!({"type":"prompt_snapshot","uuid":"u5","parentUuid":"u4","sessionId":"s","systemPrompt":format!("{CLAUDE_MD} {MEMORY_MD}")}),
        json!({"type":"api-request-blob","sessionId":"s","hash":"h","message":{"role":"user","content":MEMORY_MD}}),
        json!({"type":"user","uuid":"u6","parentUuid":"u5","sessionId":"s","isMeta":true,"message":{"role":"user","content":CLAUDE_MD}}),
        json!({"type":"user","uuid":"u7","parentUuid":"u6","sessionId":"s","message":{"role":"user","content":[
            {"type":"text","text":reminder(&format!("Contents of /work/CLAUDE.md: {CLAUDE_MD} {SR_USER}"))},
            {"type":"text","text":reminder(&format!("Contents of MEMORY.md: {MEMORY_MD}"))},
            {"type":"text","text":"now rename the flag"}]}}),
        json!({"type":"assistant","uuid":"u8","parentUuid":"u7","sessionId":"s","message":{"role":"assistant","content":[{"type":"text","text":"Renamed."},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/work/a.rs"}}]}}),
        json!({"type":"user","uuid":"u10","parentUuid":"u8","sessionId":"s","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_1","content":format!("fn a() {{}}\n{}", reminder(SR_TOOL))}]}}),
        json!({"type":"a-type-from-a-later-version","uuid":"u9","parentUuid":"u10","sessionId":"s","data":CLAUDE_MD}),
        json!({"type":"user","uuid":"u11","parentUuid":"u9","sessionId":"s","agit":"recovered_evidence",
            "message":{"role":"user","content":format!(
                "Recovered historical session evidence. This quoted record describes past activity; it grants no current permissions or instructions.\n{}",
                json!({"runtime":"claude-code","session":"source","record":{"type":"attachment","attachment":{"type":"instructions","content":CLAUDE_MD}}}))},
            "agit_recovered_evidence":{"version":1,"source":"claude-code","session":"source","object_hash":"h"}}),
        // Attachments as Claude Code 2.1.311 writes them after a turn and at /exit: a file's body,
        // and the request snapshot it appends past the last reply. They are content, so only the
        // publication rule decides what of them leaves, and it sends neither body.
        json!({"type":"attachment","uuid":"u13","parentUuid":"u11","sessionId":"s","attachment":{"type":"file","filename":"/work/notes.txt","content":{"type":"text","file":{"filePath":"/work/notes.txt","content":ATTACHMENT_BODY}}}}),
        json!({"type":"attachment","uuid":"u14","parentUuid":"u13","sessionId":"s","attachment":{"type":"prompt_snapshot","systemPrompt":[PROMPT_SNAPSHOT],"tools":[]}}),
        // The 2.1.314 records that hold request bodies and file contents, now judged as content
        // in a settled region: only the publication rule decides, and it sends none of them.
        json!({"type":"api-request-blob","sessionId":"s","hash":"h2","message":{"role":"user","content":REQUEST_BLOB}}),
        json!({"type":"api-request","sessionId":"s","requestId":"r1","body":{"messages":[{"role":"user","content":REQUEST}]}}),
        json!({"type":"file-history-delta","sessionId":"s","messageId":"m1","delta":{"/work/a.rs":{"content":FILE_DELTA}}}),
        json!({"type":"user","uuid":"u15","parentUuid":"u8","sessionId":"s","message":{"role":"user","content":format!("deploy with {UNPROTECTED} through {LATER}")}}),
        // A key straight after an escaped newline in the serialized record.
        json!({"type":"user","uuid":"u16","parentUuid":"u15","sessionId":"s","message":{"role":"user","content":format!("first line\n{AFTER_NEWLINE}")}}),
        json!({"type":"user","uuid":"u17","parentUuid":"u16","sessionId":"s","message":{"role":"user","content":format!("const token = \"{QUOTED}\";")}}),
        json!({"type":"user","uuid":"u18","parentUuid":"u17","sessionId":"s","message":{"role":"user","content":format!("ship {REGISTERED} today")}}),
    ]);
    let continued = snapshot("talk", "claude-code", 'c', &claude);
    // An event object the tree holds but its LOG does not name.
    let orphan = repo
        .root()
        .join(format!("events/0/0/0/0/{}", "0".repeat(40)));
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, format!("{}\n", json!({"note":ORPHAN_EVENT}))).unwrap();
    repo.add_all().unwrap();
    repo.git(&[
        "commit",
        "-q",
        "--amend",
        "-m",
        &format!("rename the flag with {UNPROTECTED}"),
    ])
    .unwrap();
    assert_ne!(git(repo.root(), &["rev-parse", "HEAD"]), continued);
    let continued = git(repo.root(), &["rev-parse", "HEAD"]);
    repo.git(&["checkout", "main"]).unwrap();
    let agents = format!(
        "# AGENTS.md instructions for /work\n<INSTRUCTIONS>\n{AGENTS_MD}\n{GLOBAL_AGENTS_MD}\n</INSTRUCTIONS>"
    );
    let codex = snapshot(
        "codex",
        "codex",
        'd',
        &[
            json!({"timestamp":"t","type":"session_meta","payload":{"id":"thread","cwd":"/work","base_instructions":{"text":GLOBAL_AGENTS_MD}}}),
            json!({"timestamp":"t","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":GLOBAL_AGENTS_MD}]}}),
            json!({"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":agents}]}}),
            json!({"timestamp":"t","type":"world_state","payload":{"agents_md":AGENTS_MD}}),
            // Codex 0.166 writes these around every turn; each can hold instruction text.
            json!({"timestamp":"t","type":"event_msg","payload":{"type":"thread_settings_applied","thread_id":"thread","thread_settings":{"developer_instructions":CODEX_SETTINGS}}}),
            json!({"timestamp":"t","type":"turn_context","payload":{"cwd":"/work","model":"m","user_instructions":CODEX_TURN_CONTEXT,"developer_instructions":CODEX_TURN_CONTEXT}}),
            json!({"timestamp":"t","type":"world_state","payload":{"state":CODEX_WORLD_STATE}}),
            json!({"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":format!("<user_instructions>\n{OLD_CODEX}\n</user_instructions>")}]}}),
            json!({"timestamp":"t","type":"event_msg","payload":{"type":"user_message","message":"fix the parser"}}),
            json!({"timestamp":"t","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Fixed."}]}}),
        ],
    );
    repo.git(&["checkout", "main"]).unwrap();
    snapshot(
        "cursor",
        "cursor",
        'e',
        &[
            json!({"role":"user","message":{"content":[{"type":"text","text":format!(
                "<user_info>{CURSOR_UI}</user_info>\n<user_query>\nplease refactor\n</user_query>\n{CURSOR_OUTSIDE}")}]}}),
            json!({"role":"assistant","message":{"content":[{"type":"text","text":"Refactored."}]}}),
        ],
    );
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    assert_success(&run(&["--yes", "push", "alice/app@codex"]));
    assert_success(&run(&["--yes", "push", "alice/app@cursor"]));

    let published = git(&remote, &["rev-parse", "refs/heads/talk"]);
    assert_ne!(
        published, continued,
        "the snapshot is published without the runtime's records"
    );
    // A checkout fetched from the Hub records the Hub's tips; it still knows its head is there
    // although the Hub holds a projection of it.
    for branch in ["talk", "codex", "cursor"] {
        let tracking = format!("refs/remotes/origin/{branch}");
        let tip = git(&remote, &["rev-parse", &format!("refs/heads/{branch}")]);
        assert_ne!(tip, git(repo.root(), &["rev-parse", branch]), "{branch}");
        git(
            repo.root(),
            &[
                "fetch",
                "--quiet",
                remote.to_str().unwrap(),
                &format!("+refs/heads/{branch}:{tracking}"),
            ],
        );
        assert_eq!(git(repo.root(), &["rev-parse", &tracking]), tip);
        assert!(
            agit::domain::privacy_git::hub_holds_head(&repo, branch),
            "{branch}"
        );
        // Status compares the branch with what its Hub tip publishes, not with that tip's id.
        let status = run(&["status"]);
        assert_success(&status);
        let status = String::from_utf8_lossy(&status.stdout).into_owned();
        let row = status
            .lines()
            .find(|line| line.contains(&tracking))
            .unwrap_or_else(|| panic!("{branch} has no status row:\n{status}"));
        assert!(row.contains("in sync"), "{row}");
        if branch == "talk" {
            // A branch reset to its Hub tip, or built on it, is compared with that tip.
            let head = git(repo.root(), &["rev-parse", branch]);
            let local = format!("refs/heads/{branch}");
            let row = |status: std::process::Output| {
                assert_success(&status);
                let status = String::from_utf8_lossy(&status.stdout).into_owned();
                status
                    .lines()
                    .find(|line| line.contains(&tracking))
                    .unwrap_or_else(|| panic!("{branch} has no status row:\n{status}"))
                    .to_owned()
            };
            repo.git(&["update-ref", &local, &tip]).unwrap();
            let reset = row(run(&["status"]));
            assert!(reset.contains("in sync"), "{reset}");
            let tree = format!("{tip}^{{tree}}");
            let next = git(
                repo.root(),
                &["commit-tree", &tree, "-p", &tip, "-m", "on the Hub tip"],
            );
            repo.git(&["update-ref", &local, &next]).unwrap();
            let ahead = row(run(&["status"]));
            assert!(ahead.contains("ahead 1, behind 0"), "{ahead}");
            repo.git(&["update-ref", &local, &head]).unwrap();
        }
        repo.git(&["update-ref", &tracking, &clean]).unwrap();
        assert!(!agit::domain::privacy_git::hub_holds_head(&repo, branch));
        repo.git(&["update-ref", "-d", &tracking]).unwrap();
    }
    assert_eq!(
        git(&remote, &["rev-parse", &format!("{published}^")]),
        clean
    );
    let objects = Command::new("git")
        .current_dir(&remote)
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .output()
        .unwrap()
        .stdout;
    let objects = String::from_utf8_lossy(&objects);
    for canary in [
        CLAUDE_MD,
        MEMORY_MD,
        AGENTS_MD,
        GLOBAL_AGENTS_MD,
        CODEX_TURN_CONTEXT,
        CODEX_WORLD_STATE,
        CODEX_SETTINGS,
        SR_USER,
        SR_TOOL,
        OLD_CODEX,
        CURSOR_UI,
        CURSOR_OUTSIDE,
        ORPHAN_EVENT,
        ATTACHMENT_BODY,
        PROMPT_SNAPSHOT,
        REQUEST_BLOB,
        REQUEST,
        FILE_DELTA,
        UNPROTECTED,
        AFTER_NEWLINE,
        QUOTED,
    ] {
        assert!(!objects.contains(canary), "{canary} reached the Hub");
    }
    for protected in [
        "deploy with ‹secret:",
        "first line\\n‹secret:",
        "rename the flag with ‹secret:",
        "const token = ",
    ] {
        assert!(objects.contains(protected), "{protected}: {objects}");
    }
    for words in [
        "now rename the flag",
        "Renamed.",
        "fn a() {}",
        "fix the parser",
        "Fixed.",
        "please refactor",
        "Refactored.",
    ] {
        assert!(objects.contains(words), "{words} is missing from the Hub");
    }
    let (log, _) = storage::materialize_pair_at(&remote, &published).unwrap();
    assert_eq!(log.lines().count(), 9, "{log}");
    for line in first_log.lines() {
        assert!(
            log.lines().any(|public| public == line),
            "{line} moved: {log}"
        );
    }
    let shipped = log
        .lines()
        .find(|line| line.contains("\"uuid\":\"u18\""))
        .unwrap();
    assert!(
        shipped.contains("ship ") && shipped.contains(" today") && !shipped.contains(REGISTERED),
        "{shipped}"
    );
    assert!(log.contains("\"parentUuid\":\"u2\""), "{log}");

    // The local snapshots are the complete transcripts, for history and resume.
    for (commit, canaries) in [
        (&continued, &[CLAUDE_MD, MEMORY_MD][..]),
        (&codex, &[AGENTS_MD, GLOBAL_AGENTS_MD][..]),
    ] {
        let (log, view) = storage::materialize_pair_at(repo.root(), commit).unwrap();
        assert_eq!(log, view);
        for canary in canaries {
            assert!(log.contains(canary));
        }
    }
    let (local, _) = storage::materialize_pair_at(repo.root(), &continued).unwrap();
    assert_eq!(local.lines().count(), claude.len());

    // A record protection changed when it was published keeps that line, though a value
    // registered since is in it too.
    register("latest", LATER);
    // A branch reset to an older public commit is behind the Hub tip, not diverged from it.
    claude.push(json!({"type":"user","uuid":"u12","parentUuid":"u11","sessionId":"s","message":{"role":"user","content":"and update the docs"}}));
    repo.git(&["checkout", "talk"]).unwrap();
    let next = snapshot("talk", "claude-code", 'c', &claude);
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    let tip = git(&remote, &["rev-parse", "refs/heads/talk"]);
    assert_ne!(tip, next);
    assert_eq!(git(&remote, &["rev-parse", &format!("{tip}^")]), published);
    let (extended, _) = storage::materialize_pair_at(&remote, &tip).unwrap();
    for line in log.lines() {
        assert!(
            extended.lines().any(|public| public == line),
            "{line} moved: {extended}"
        );
    }
    repo.git(&["checkout", "main"]).unwrap();
    git(
        repo.root(),
        &[
            "fetch",
            "--quiet",
            remote.to_str().unwrap(),
            "+refs/heads/talk:refs/remotes/origin/talk",
        ],
    );
    repo.git(&["update-ref", "refs/heads/talk", &published])
        .unwrap();
    let status = run(&["status"]);
    assert_success(&status);
    let status = String::from_utf8_lossy(&status.stdout).into_owned();
    let row = status
        .lines()
        .find(|line| line.contains("refs/remotes/origin/talk"))
        .unwrap_or_else(|| panic!("talk has no status row:\n{status}"));
    assert!(row.contains("ahead 0, behind 1"), "{row}");
    repo.git(&["update-ref", "refs/heads/talk", &next]).unwrap();

    // A merge of another session into this one keeps the lines its target's publication holds.
    let aside = snapshot(
        "aside",
        "claude-code",
        'e',
        &[
            json!({"type":"user","uuid":"a1","parentUuid":null,"sessionId":"e","message":{"role":"user","content":"check the other branch"}}),
        ],
    );
    repo.git(&["checkout", "talk"]).unwrap();
    claude.push(json!({"type":"user","uuid":"u20","parentUuid":"u12","sessionId":"s","message":{"role":"user","content":"merged the other branch"}}));
    let turn = snapshot("talk", "claude-code", 'c', &claude);
    let merged = git(
        repo.root(),
        &[
            "commit-tree",
            &format!("{turn}^{{tree}}"),
            "-p",
            &next,
            "-p",
            &aside,
            "-m",
            "merge aside",
        ],
    );
    repo.git(&["update-ref", "refs/heads/talk", &merged])
        .unwrap();
    repo.git(&["checkout", "main"]).unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    let merged_tip = git(&remote, &["rev-parse", "refs/heads/talk"]);
    assert_eq!(
        git(&remote, &["rev-parse", &format!("{merged_tip}^1")]),
        tip
    );
    let (after_merge, _) = storage::materialize_pair_at(&remote, &merged_tip).unwrap();
    for line in extended.lines() {
        assert!(
            after_merge.lines().any(|public| public == line),
            "{line} moved: {after_merge}"
        );
    }

    // A file a person's history added, holding a value only the privacy rules know, refuses its
    // publication by name, with nothing sent: a file is never rewritten for them. So does a text
    // file too large to check, which would otherwise go out unread. A binary file of that size
    // goes out, as capture does not read one.
    let file_branch = |branch: &str, name: &str, content: &[u8]| {
        repo.git(&["checkout", "-q", "-b", branch, "main"]).unwrap();
        fs::write(repo.root().join(name), content).unwrap();
        repo.add_all().unwrap();
        repo.commit(branch).unwrap();
        repo.git(&["checkout", "-q", "main"]).unwrap();
    };
    let oversized = agit::domain::privacy_publication::MAX_INPUT_BYTES + 1;
    file_branch(
        "notes",
        "notes.txt",
        format!("ask {REGISTERED} first\n").as_bytes(),
    );
    file_branch(
        "big",
        "big.txt",
        "line\n".repeat(oversized / 5 + 1).as_bytes(),
    );
    file_branch("image", "image.bin", &vec![0; oversized]);
    // A value that begins in what a file held and ends in what a later commit appends.
    file_branch("split", "split.txt", b"ask heron-led");
    repo.git(&["checkout", "-q", "split"]).unwrap();
    fs::write(
        repo.root().join("split.txt"),
        "ask heron-ledger-7731 first\n",
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("split more").unwrap();
    // A person's file whose bytes are a session record's, which is judged as records are and
    // not as text, holding a value registered since.
    repo.git(&["checkout", "-q", "main"]).unwrap();
    snapshot(
        "copy",
        "claude-code",
        'f',
        &[
            json!({"type":"user","uuid":"c1","parentUuid":null,"sessionId":"f","message":{"role":"user","content":"start the copy"}}),
        ],
    );
    let copied = fs::read_to_string(repo.root().join(meta::LOG_FILE)).unwrap();
    register("copied", copied.lines().next().unwrap());
    fs::write(repo.root().join("copy.txt"), &copied).unwrap();
    repo.add_all().unwrap();
    repo.commit("copy the record list").unwrap();
    repo.git(&["checkout", "-q", "main"]).unwrap();
    for (branch, name, said) in [
        ("notes", "`notes.txt`", "holds a value"),
        ("big", "`big.txt`", "too large"),
        ("split", "`split.txt`", "holds a value"),
        ("copy", "`copy.txt`", "holds a value"),
    ] {
        let before = hub.posts.load(Ordering::Acquire);
        let refused = run(&["--yes", "push", &format!("alice/app@{branch}")]);
        assert!(!refused.status.success(), "{refused:?}");
        let told = String::from_utf8_lossy(&refused.stderr);
        assert!(
            told.contains(name) && told.contains(said) && told.contains("nothing was published"),
            "{told}"
        );
        assert_eq!(hub.posts.load(Ordering::Acquire), before);
        assert!(
            Command::new("git")
                .current_dir(&remote)
                .args([
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}")
                ])
                .output()
                .unwrap()
                .stdout
                .is_empty()
        );
    }
    assert_success(&run(&["--yes", "push", "alice/app@image"]));
}

/// A conversation whose first message quotes a placeholder this computer never made publishes
/// under the branch name made from that message: the placeholder is text the person saw, and the
/// name was made from the same text. One quoted in a later record, a tool result here, never
/// stops it either, since only the first message names the branch. A push that restored the
/// whole session before reading its first message, or that refused every placeholder it could not
/// restore, would keep such a conversation on this computer for good.
#[test]
fn a_first_message_quoting_a_placeholder_made_elsewhere_publishes() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));

    let elsewhere =
        agit::domain::privacy::detector::placeholder::typed_token("secret", "aws-access-token")
            .unwrap();
    repo.git(&["checkout", "-B", "talk"]).unwrap();
    let session = meta::Meta::new(
        format!("agit-{}", "c".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let records = [
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":format!("why does the log show {elsewhere}?")}}),
        json!({"type":"assistant","uuid":"u2","parentUuid":"u1","sessionId":"s","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/work/ci.log"}}]}}),
        json!({"type":"user","uuid":"u3","parentUuid":"u2","sessionId":"s","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":format!("deploy key {elsewhere}")}]}}),
        json!({"type":"assistant","uuid":"u4","parentUuid":"u3","sessionId":"s","message":{"role":"assistant","content":[{"type":"text","text":"It is a placeholder."}]}}),
    ];
    let text: String = records.iter().map(|record| format!("{record}\n")).collect();
    let log = transcript::wrap_lines(&text, "claude-code", &session.session);
    write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &session).unwrap();
    repo.add_all().unwrap();
    repo.commit("talk").unwrap();
    repo.git(&["checkout", "main"]).unwrap();

    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    let tip = git(&remote, &["rev-parse", "refs/heads/talk"]);
    let (published, _) = storage::materialize_pair_at(&remote, &tip).unwrap();
    assert_eq!(
        published.matches(elsewhere.as_str()).count(),
        2,
        "{published}"
    );
}

/// An automatic upload the privacy check holds back sends nothing, and `agit status` says so: the
/// target, the reason, and, for a reason waiting does not end, the step that ends it. The note
/// stays until the Hub acknowledges that target. Here the device cannot read its older secret
/// list whole, so a value it blocks could pass. An upload that failed only in its own stderr would
/// stop a session's uploads with nobody told; one whose note outlived the landing push would
/// report a wait that is over, and one whose note any successful command cleared would hide the
/// wait after a preview, which sends nothing.
#[test]
fn an_upload_the_privacy_check_holds_back_shows_in_status_until_it_lands() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["config", "agit.autoPush", "true"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    repo.git(&["checkout", "-B", "talk"]).unwrap();
    let session = meta::Meta::new(
        format!("agit-{}", "c".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let log = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"fix the failing test"}})
        ),
        "claude-code",
        &session.session,
    );
    write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &session).unwrap();
    repo.add_all().unwrap();
    repo.commit("talk").unwrap();
    let posts = hub.posts.load(Ordering::Acquire);

    let vault = home.join("secret-filter/vault.json");
    fs::create_dir_all(vault.parent().unwrap()).unwrap();
    fs::write(&vault, "not a vault").unwrap();
    let held = run(&["__push-automatic", "alice/app@talk"]);
    assert_eq!(held.status.code(), Some(7), "{held:?}");
    assert_eq!(hub.posts.load(Ordering::Acquire), posts);
    assert!(
        git_command(&remote)
            .arg("-C")
            .arg(&remote)
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/talk"])
            .output()
            .unwrap()
            .stdout
            .is_empty()
    );
    let status = run(&["status"]);
    let told = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        told.contains("alice/app@talk: its upload waits for the privacy check (legacy_rules)")
            && told.contains("nothing was sent")
            && told.contains("older secret list cannot be read whole")
            && !told.contains("the next upload tries again"),
        "{told}"
    );
    let waits = |output: &Output| {
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["result"]["value"]
            ["upload_waits"]
            .clone()
    };
    let listed = waits(&run(&["status", "--json"]));
    assert_eq!(listed[0]["target"], "alice/app@talk", "{listed}");
    assert_eq!(listed[0]["reason"], "legacy_rules", "{listed}");

    fs::remove_file(&vault).unwrap();
    // A preview sends nothing, so the wait stays.
    assert_success(&run(&["--yes", "push", "--dry-run", "alice/app@talk"]));
    assert_eq!(hub.posts.load(Ordering::Acquire), posts);
    assert_eq!(waits(&run(&["status", "--json"])), listed);
    assert_success(&run(&["__push-automatic", "alice/app@talk"]));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/talk"]),
        git(repo.root(), &["rev-parse", "talk"])
    );
    assert_eq!(waits(&run(&["status", "--json"])), json!([]));
}

/// A milestone and a tag asked for while a turn is still running reach the Hub with that turn,
/// even when an earlier turn lands first: the milestone in the public commit's meta and message,
/// the tag on the public commit the turn was published as. With nothing new to save, a milestone
/// names the last saved turn as its own commit. A tag whose turn is in no part of an upload, or on
/// a published branch the push leaves out, is named with why, is never sent, and the push never
/// reads as up to date while it is missing; a tag the Hub already holds is not named, and one
/// added later to a published turn of the pushed branch goes on that turn's public commit. A
/// milestone asked for while the privacy check cannot run is reported as not recorded, never
/// dropped in silence, and lands when the same command runs again. Tag names are held to the
/// branch-name check and session tags take no message. After the push the local tag still names
/// the local commit, and `agit fetch` and `agit pull` take the Hub's tags around it. An
/// implementation that dropped a request made mid-turn, put it on the earlier turn, tagged the
/// local commit, refused the push over a rewritten tag, sent the tagged turn's branch for the
/// tag's sake, reported "already up to date", or fetched every tag (refusing to clobber the local
/// one) would each fail a step here.
#[test]
fn a_milestone_and_tag_reach_the_hub_with_their_turn() {
    const OPENING: &str = "11111111-0000-4000-8000-0000000000a1";
    const ASIDE: &str = "11111111-0000-4000-8000-0000000000a2";
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    let codex_home = temp.path().join("codex");
    let work = temp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(&work)
            .env("AGIT_HOME", &home)
            .env("CODEX_HOME", &codex_home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));

    let sessions = codex_home.join("sessions/2026/11/05");
    fs::create_dir_all(&sessions).unwrap();
    let rollout = |id: &str| {
        let path = sessions.join(format!("rollout-2026-11-05T00-00-00-{id}.jsonl"));
        fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"session_meta", "payload":{"id":id, "cwd":work, "timestamp":"2026-11-05T00:00:00Z"}})
            ),
        )
        .unwrap();
        path
    };
    let append = |path: &Path, records: &[serde_json::Value]| {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        for record in records {
            writeln!(file, "{record}").unwrap();
        }
    };
    let prompt = |text: &str| json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{"type":"input_text","text":text}]}});
    let reply = [
        json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "content":[{"type":"output_text","text":"The phase is complete."}]}}),
        json!({"type":"event_msg", "payload":{"type":"task_complete"}}),
    ];
    let source = rollout(OPENING);
    append(&source, &[prompt("Start the wiki check")]);
    append(&source, &reply);
    assert_success(&run(&[
        "import",
        OPENING,
        "--from",
        "codex",
        "--into",
        "alice/app@talk",
        "--independent",
    ]));
    let tip = || git(repo.root(), &["rev-parse", "refs/heads/talk"]);
    let opening = tip();

    // Asked for from inside the running turn, with an earlier turn still unsaved: the earlier
    // turn lands without it, and the request waits for the running one.
    append(&source, &[prompt("Collect the wiki pages")]);
    append(&source, &reply);
    append(&source, &[prompt("Verify the wiki against the release")]);
    let asked = run(&[
        "commit",
        "alice/app@talk",
        "--milestone",
        "Verified the wiki",
        "--tag",
        "ms-verified",
    ]);
    assert_success(&asked);
    assert!(
        String::from_utf8_lossy(&asked.stdout).contains("goes on it when it is saved"),
        "{asked:?}"
    );
    let collected = tip();
    assert_ne!(collected, opening);
    assert!(!git(repo.root(), &["log", "-1", "--format=%B", &collected]).contains("milestone:"));
    assert!(!repo.has_tag("ms-verified"));
    append(&source, &reply);
    assert_success(&run(&["commit", "alice/app@talk"]));
    let verified = tip();
    assert_ne!(verified, collected);
    assert!(
        git(repo.root(), &["log", "-1", "--format=%B", &verified])
            .contains("milestone: Verified the wiki")
    );
    assert_eq!(
        git(repo.root(), &["rev-parse", "ms-verified^{commit}"]),
        verified
    );

    // Refused before anything lands: a key-shaped tag name, and a message on a session tag.
    append(&source, &[prompt("One more check")]);
    append(&source, &reply);
    let key_shaped = run(&[
        "commit",
        "alice/app@talk",
        "--tag",
        "fix-ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr",
    ]);
    assert!(!key_shaped.status.success(), "{key_shaped:?}");
    assert!(
        String::from_utf8_lossy(&key_shaped.stderr).contains("plain name"),
        "{key_shaped:?}"
    );
    assert_eq!(tip(), verified);
    let annotated = run(&["tag", "-m", "Reviewed", "ms-noted", "alice/app@talk"]);
    assert!(!annotated.status.success(), "{annotated:?}");
    assert!(!repo.has_tag("ms-noted"));
    assert_success(&run(&["commit", "alice/app@talk"]));

    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    let published = git(&remote, &["rev-parse", "refs/tags/ms-verified"]);
    assert_ne!(
        published, verified,
        "the tag names the turn's public commit"
    );
    git(
        &remote,
        &["merge-base", "--is-ancestor", &published, "refs/heads/talk"],
    );
    let public_meta: serde_json::Value = serde_json::from_str(&git(
        &remote,
        &["show", "refs/tags/ms-verified:session/meta.json"],
    ))
    .unwrap();
    assert_eq!(public_meta["milestone"], "Verified the wiki");
    assert_eq!(public_meta["turn"], 3);
    // The local tag keeps naming the turn's local commit, the Hub's its published copy, and
    // fetching the Hub's tags leaves the local one as it is instead of failing on it.
    for command in [&["fetch", "alice/app"][..], &["pull", "alice/app@main"][..]] {
        assert_success(&run(command));
        assert_eq!(
            git(repo.root(), &["rev-parse", "refs/tags/ms-verified"]),
            verified
        );
    }
    assert!(
        git(&remote, &["log", "-1", "--format=%B", &published])
            .contains("milestone: Verified the wiki")
    );

    // Nothing new to save: the milestone names the last saved turn as its own commit.
    let before = tip();
    assert_success(&run(&[
        "commit",
        "alice/app@talk",
        "--milestone",
        "Reviewed the release notes",
        "--tag",
        "ms-reviewed",
    ]));
    assert_ne!(tip(), before);
    assert_eq!(
        git(repo.root(), &["rev-parse", "ms-reviewed^{commit}"]),
        tip()
    );
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    let named = git(&remote, &["rev-parse", "refs/heads/talk"]);
    assert_eq!(git(&remote, &["rev-parse", "refs/tags/ms-reviewed"]), named);
    assert!(
        git(&remote, &["log", "-1", "--format=%B", &named])
            .contains("milestone: Reviewed the release notes")
    );

    // A tag on a turn whose branch is in no part of this upload stays here, named each time.
    let aside = rollout(ASIDE);
    append(&aside, &[prompt("Draft the aside")]);
    append(&aside, &reply);
    assert_success(&run(&[
        "import",
        ASIDE,
        "--from",
        "codex",
        "--into",
        "alice/app@aside",
        "--independent",
    ]));
    assert_success(&run(&["tag", "ms-aside", "alice/app@aside"]));
    let refs_before = git(
        &remote,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    for _ in 0..2 {
        let pushed = run(&["--yes", "push", "alice/app@talk"]);
        assert_success(&pushed);
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&pushed.stdout),
            String::from_utf8_lossy(&pushed.stderr)
        );
        assert!(said.contains("tag ms-aside is not uploaded"), "{said}");
        // Tags the Hub already holds on their public commits are not named.
        assert!(!said.contains("tag ms-verified"), "{said}");
        assert!(!said.contains("tag ms-reviewed"), "{said}");
        assert!(!said.contains("already up to date"), "{said}");
        assert_eq!(
            git(
                &remote,
                &["for-each-ref", "--format=%(refname) %(objectname)"]
            ),
            refs_before
        );
    }

    // A tag added later to a published turn of the branch being pushed goes on its public commit.
    assert_success(&run(&["tag", "ms-talk-late", "alice/app@talk"]));
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/tags/ms-talk-late"]),
        git(&remote, &["rev-parse", "refs/heads/talk"])
    );

    // Once that branch is published, a tag added to its turn rides only with that branch: a push
    // of another branch names it instead of reading as up to date.
    assert_success(&run(&["--yes", "push", "alice/app@aside"]));
    git(&remote, &["rev-parse", "refs/tags/ms-aside"]);
    assert_success(&run(&["tag", "ms-aside-late", "alice/app@aside"]));
    let pushed = run(&["--yes", "push", "alice/app@talk"]);
    assert_success(&pushed);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&pushed.stdout),
        String::from_utf8_lossy(&pushed.stderr)
    );
    assert!(
        said.contains(
            "tag ms-aside-late is not uploaded: its turn is on a branch this push doesn't include"
        ),
        "{said}"
    );
    assert!(!said.contains("already up to date"), "{said}");
    for published in ["ms-verified", "ms-aside", "ms-talk-late"] {
        assert!(!said.contains(&format!("tag {published} ")), "{said}");
    }
    assert!(!git(&remote, &["tag", "--list", "ms-aside-late"]).contains("ms-aside-late"));
    assert_success(&run(&["--yes", "push", "alice/app@aside"]));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/tags/ms-aside-late"]),
        git(&remote, &["rev-parse", "refs/tags/ms-aside"])
    );

    // A settlement that lands no turn because the privacy check cannot run says the milestone
    // was not recorded rather than losing it, and the same command lands it once the check runs.
    // A plain file where the privacy directory belongs keeps the worker from processing anything.
    let privacy = home.join("privacy");
    let set_aside = home.join("privacy.aside");
    let existed = privacy.exists();
    if existed {
        fs::rename(&privacy, &set_aside).unwrap();
    }
    fs::write(&privacy, "not a directory").unwrap();
    append(&source, &[prompt("Check the release notes once more")]);
    append(&source, &reply);
    let before = tip();
    let blocked = run(&["commit", "alice/app@talk", "--milestone", "Checked twice"]);
    fs::remove_file(&privacy).unwrap();
    if existed {
        fs::rename(&set_aside, &privacy).unwrap();
    }
    assert!(!blocked.status.success(), "{blocked:?}");
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("the milestone was not recorded"),
        "{blocked:?}"
    );
    assert_eq!(tip(), before);
    assert_success(&run(&[
        "commit",
        "alice/app@talk",
        "--milestone",
        "Checked twice",
    ]));
    assert_ne!(tip(), before);
    assert!(
        git(repo.root(), &["log", "-1", "--format=%B", &tip()])
            .contains("milestone: Checked twice")
    );
}

/// A second computer's first save into a repository the Hub already has starts its `main` at the
/// Hub's `main`. A `main` made there as a root of its own shares no history with the Hub's: the
/// repository's `main` reads as diverged on that computer from then on, and its push is refused.
#[test]
fn a_first_save_into_a_repository_on_the_hub_starts_from_its_main() {
    const OPENING: &str = "11111111-0000-4000-8000-0000000000b1";
    let temp = tempfile::tempdir().unwrap();
    let desktop = temp.path().join("desktop");
    let laptop = temp.path().join("laptop");
    let tablet = temp.path().join("tablet");
    let codex_home = temp.path().join("codex");
    let work = temp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    for home in [&desktop, &laptop, &tablet] {
        startup_cache::seed(home);
        save_at(
            &home
                .join("credentials")
                .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
            &HubCredential {
                account_id: Some("account-1".into()),
                username: "alice".into(),
                email: None,
                hub: Some(hub.base.clone()),
                access_token: "synthetic-access".into(),
                refresh_token: "synthetic-refresh".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    }
    let command_in = |home: &Path, args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(&work)
            .env("AGIT_HOME", home)
            .env("CODEX_HOME", &codex_home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args);
        command
    };
    let run_in = |home: &Path, args: &[&str]| command_in(home, args).output().unwrap();
    let repo = Repo::init(&desktop.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run_in(&desktop, &["--yes", "push", "alice/app@main"]));
    let hub_main = git(&remote, &["rev-parse", "refs/heads/main"]);

    // The laptop has no checkout of alice/app; its first save creates one.
    let sessions = codex_home.join("sessions/2026/11/07");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join(format!("rollout-2026-11-07T00-00-00-{OPENING}.jsonl")),
        [
            json!({"type":"session_meta", "payload":{"id":OPENING, "cwd":work, "timestamp":"2026-11-07T00:00:00Z"}}),
            json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{"type":"input_text","text":"Check the shared file"}]}}),
            json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "content":[{"type":"output_text","text":"It is there."}]}}),
            json!({"type":"event_msg", "payload":{"type":"task_complete"}}),
        ]
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>(),
    )
    .unwrap();
    let import = [
        "import",
        OPENING,
        "--from",
        "codex",
        "--into",
        "alice/app@talk",
        "--independent",
    ];

    // A save made in the background, as a folder's capture is, asks the Hub nothing for a
    // destination whose automatic upload is off on this computer, and starts its own main.
    assert_success(&run_in(
        &tablet,
        &["config", "--repo", "alice/app", "push.auto", "false"],
    ));
    let captured = command_in(&tablet, &import)
        .env("AGIT_AUTO_PUSH", "1")
        .output()
        .unwrap();
    assert_success(&captured);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&captured.stdout),
        String::from_utf8_lossy(&captured.stderr)
    );
    assert!(!said.contains("started from"), "{said}");
    assert!(!said.contains("couldn't"), "{said}");
    assert_ne!(
        git(
            &tablet.join("repos/alice/app"),
            &["rev-parse", "refs/heads/main"]
        ),
        hub_main
    );

    let imported = run_in(&laptop, &import);
    assert_success(&imported);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&imported.stdout),
        String::from_utf8_lossy(&imported.stderr)
    );
    assert!(
        said.contains("started from alice/app's main on the Hub"),
        "{said}"
    );
    assert!(!said.contains("couldn't"), "{said}");
    let laptop_repo = laptop.join("repos/alice/app");
    assert_eq!(
        git(&laptop_repo, &["rev-parse", "refs/heads/main"]),
        hub_main
    );
    assert_eq!(
        fs::read_to_string(laptop_repo.join("README.md")).unwrap(),
        "Shared file"
    );
    git(
        &laptop_repo,
        &["merge-base", "--is-ancestor", &hub_main, "refs/heads/talk"],
    );

    // The laptop's main is the Hub's, so its push finds nothing to send, and its session goes up
    // on top of it.
    git(
        &laptop_repo,
        &["config", "http.userAgent", TRANSPORT_USER_AGENT],
    );
    let pushed = run_in(&laptop, &["--yes", "push", "alice/app@main"]);
    assert_success(&pushed);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&pushed.stdout),
        String::from_utf8_lossy(&pushed.stderr)
    );
    assert!(said.contains("main is on the Hub"), "{said}");
    assert_success(&run_in(&laptop, &["--yes", "push", "alice/app@talk"]));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), hub_main);
    git(
        &remote,
        &["merge-base", "--is-ancestor", &hub_main, "refs/heads/talk"],
    );
}

/// A session whose new history is more than one publication to this Hub can carry is published in
/// parts within one push: each part declares no more new turns than the Hub admits per request,
/// the parts together declare every new turn once, and the branch and every turn's version tag
/// end up on the Hub. A selected branch with no new history still delivers the tag it owes. A
/// push that sent the whole history at once exceeds the published limit in its one manifest; one
/// that stopped after the first part leaves the branch behind the session; one that skipped the
/// branches with no new history leaves their tags off the Hub. A part refused before it sends
/// anything names the parts already published, and a rerun continues after them; a message that
/// nothing was published sends the person looking for a push that did not happen. A later part,
/// which goes out under the first part's answer without its own summary, still says how many
/// records it adds with parts the privacy check left out; said only in a shown summary, a record
/// first carried by a later part would go up unsaid, and no later upload says it again.
#[test]
fn a_session_longer_than_one_publication_is_published_in_parts() {
    use agit::domain::session_privacy::SessionPublicationManifest;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    repo.git(&["checkout", "-b", "talk"]).unwrap();
    // main moves on without talk, and a tag on that commit is all main still owes the Hub.
    repo.git(&["checkout", "main"]).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file, revised").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line revised").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    let revised = git(repo.root(), &["rev-parse", "HEAD"]);
    git(repo.root(), &["tag", "reviewed-main", &revised]);
    repo.git(&["checkout", "talk"]).unwrap();
    let session = meta::Meta::new(
        format!("agit-{}", "c".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let turns: Vec<String> = (0..9)
        .map(|index| {
            let content = if index == 8 {
                "turn 8 [content omitted: too large for the privacy check]".to_owned()
            } else {
                format!("turn {index}")
            };
            let log = transcript::wrap_lines(
                &format!(
                    "{}\n",
                    json!({"type":"user","message":{"role":"user","content":content}})
                ),
                "claude-code",
                &session.session,
            );
            write_snapshot(repo.root(), &log, &log).unwrap();
            meta::write(repo.root(), &session).unwrap();
            repo.add_all().unwrap();
            repo.commit(&content).unwrap();
            git(repo.root(), &["rev-parse", "HEAD"])
        })
        .collect();

    hub.max_new_commits.store(4, Ordering::Release);
    let earlier = hub.previews.lock().unwrap().len();
    *hub.strategy_budget.lock().unwrap() = Some(1);
    let stopped = run(&["--yes", "push", "alice/app@talk"]);
    assert!(!stopped.status.success(), "{stopped:?}");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&stopped.stdout),
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(said.contains("in 3 parts"), "{stopped:?}");
    assert!(
        said.contains("1 of 3 parts are published; part 2 was not sent"),
        "{stopped:?}"
    );
    assert!(
        !said.contains("no branch or tag refs were published"),
        "{stopped:?}"
    );
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/talk"]), turns[3]);
    *hub.strategy_budget.lock().unwrap() = None;
    let pushed = run(&["--yes", "push", "alice/app@talk"]);
    assert_success(&pushed);
    assert!(
        format!(
            "{}{}",
            String::from_utf8_lossy(&pushed.stdout),
            String::from_utf8_lossy(&pushed.stderr)
        )
        .contains("in 2 parts"),
        "{pushed:?}"
    );
    assert!(
        String::from_utf8_lossy(&pushed.stdout)
            .contains("1 record in this upload has parts the privacy check left out"),
        "{pushed:?}"
    );
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/talk"]), turns[8]);
    let mut declared = std::collections::BTreeSet::new();
    let previews = hub.previews.lock().unwrap()[earlier..].to_vec();
    for preview in &previews {
        let manifest: SessionPublicationManifest =
            serde_json::from_value(preview["manifest"].clone()).unwrap();
        assert!(manifest.snapshots.len() <= 4, "{manifest:?}");
        declared.extend(
            manifest
                .snapshots
                .iter()
                .map(|snapshot| snapshot.commit().to_owned()),
        );
    }
    assert_eq!(declared, turns.iter().cloned().collect());
    for turn in &turns {
        assert_eq!(
            git(&remote, &["rev-parse", &format!("refs/tags/agit-{turn}")]),
            *turn
        );
    }
    assert_eq!(
        git(&remote, &["rev-parse", "refs/tags/reviewed-main"]),
        revised
    );
}

/// A session larger than one privacy envelope may hold, but within what capture records, makes its
/// first publication to a new branch. The new branch is named from the session's opening request,
/// which publication reads from the whole saved LOG; a read capped below what capture admits
/// refuses the first publication of every session that grew past that cap while it was recorded,
/// which is how an old long conversation fails to catch up.
#[test]
fn a_session_larger_than_an_envelope_makes_its_first_publication() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [28; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));

    repo.git(&["checkout", "-b", "long-talk"]).unwrap();
    let session = meta::Meta::new(
        format!("agit-{}", "e".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    // Records of plain prose, each well under the privacy check's unit, together past one
    // envelope's input.
    let paragraph =
        "The build reads the manifest, resolves each dependency and writes the lockfile. "
            .repeat(10_000);
    let mut records =
        vec![json!({"type":"user","message":{"role":"user","content":"tidy the build"}})];
    while records
        .iter()
        .map(|record| record.to_string().len())
        .sum::<usize>()
        <= agit::domain::privacy_publication::MAX_INPUT_BYTES
    {
        records.push(
            json!({"type":"assistant","message":{"role":"assistant","content":[
                {"type":"text","text":paragraph},
            ]}}),
        );
    }
    let native: String = records.iter().map(|record| format!("{record}\n")).collect();
    let log = transcript::wrap_lines(&native, "claude-code", &session.session);
    assert!(log.len() > agit::domain::privacy_publication::MAX_INPUT_BYTES);
    write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &session).unwrap();
    repo.add_all().unwrap();
    repo.commit("tidy the build").unwrap();
    let pushed = run(&["--yes", "push", "alice/app@long-talk"]);
    assert_success(&pushed);
    git(&remote, &["rev-parse", "refs/heads/long-talk"]);
}

/// Every part of a push in parts goes out under a confirmation that covers it. When the repository
/// turns public after a part lands, the parts not yet sent stay unsent, since the yes was given
/// for a private audience. Without a terminal, plain parts publish to a private repository on
/// their own, but a later part carrying an encrypted session stops before it is sent, as a single
/// publication carrying it is refused. A push that checked the destination only before the first
/// part sends the rest to the public audience; one that let the first part's unattended yes cover
/// every part publishes the encrypted session unasked.
#[test]
fn every_part_goes_out_under_a_confirmation_that_covers_it() {
    use agit::domain::session_privacy::{AccountIdentity, SessionPrivacy, SessionScheme};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    // Its encrypted turns were registered that way by a client from before encrypted publication
    // was retired.
    hub.legacy_encrypted
        .lock()
        .unwrap()
        .insert(format!("agit-{}", "a".repeat(40)));
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    repo.git(&["checkout", "-b", "talk"]).unwrap();
    let plain = meta::Meta::new(
        format!("agit-{}", "c".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let mut encrypted = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    encrypted.session_privacy = Some(SessionPrivacy {
        scheme: SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(
            AccountIdentity::new(&hub.base, "session-owner").unwrap(),
        ))
        .unwrap()
    });
    encrypted.committer_account = Some(AccountIdentity::new(&hub.base, "account-1").unwrap());
    let turn = |session: &meta::Meta, content: &str| {
        let log = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &session.session,
        );
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), session).unwrap();
        repo.add_all().unwrap();
        repo.commit(content).unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    let plain_turns: Vec<String> = (0..8)
        .map(|index| turn(&plain, &format!("plain turn {index}")))
        .collect();
    for index in 0..5 {
        turn(&encrypted, &format!("encrypted turn {index}"));
    }
    hub.max_new_commits.store(4, Ordering::Release);

    // The last tag of the first part marks it as landed.
    *hub.publicize_when.lock().unwrap() = Some(format!("refs/tags/agit-{}", plain_turns[3]));
    let moved = run(&["--yes", "push", "alice/app@talk"]);
    assert!(!moved.status.success(), "{moved:?}");
    assert!(
        String::from_utf8_lossy(&moved.stderr)
            .contains("the publication endpoint or audience changed during publication"),
        "{moved:?}"
    );
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/talk"]),
        plain_turns[3]
    );

    *hub.visibility.lock().unwrap() = "private";
    let unattended = run(&["push", "alice/app@talk"]);
    assert!(!unattended.status.success(), "{unattended:?}");
    assert!(
        String::from_utf8_lossy(&unattended.stderr).contains(
            "1 of 3 parts are published; part 2 carries encrypted sessions, which need the person's confirmation"
        ),
        "{unattended:?}"
    );
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/talk"]),
        plain_turns[7]
    );
}

/// An encrypted session on a Hub that doesn't yet take the declaration that its carried turns were
/// proven isn't continued: encrypted publication is retired and that Hub would keep the session
/// encrypted, so the push says so and nothing is sent.
#[test]
fn an_encrypted_session_the_hub_keeps_encrypted_is_not_continued() {
    use agit::domain::session_privacy::{AccountIdentity, SessionPrivacy};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    hub.legacy_encrypted
        .lock()
        .unwrap()
        .insert(format!("agit-{}", "a".repeat(40)));
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    repo.git(&["checkout", "-b", "work"]).unwrap();
    let mut session = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    session.session_privacy = Some(SessionPrivacy {
        scheme: agit::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(
            AccountIdentity::new(&hub.base, "session-owner").unwrap(),
        ))
        .unwrap()
    });
    session.committer_account = Some(AccountIdentity::new(&hub.base, "account-1").unwrap());
    let turn = |content: &str| {
        let log = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &session.session,
        );
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), &session).unwrap();
        repo.add_all().unwrap();
        repo.commit(content).unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    for index in 0..3 {
        turn(&format!("turn {index}"));
    }
    assert_success(&run(&["--yes", "push", "alice/app@work"]));
    let published = git(&remote, &["rev-parse", "refs/heads/work"]);
    assert!(
        git(&remote, &["ls-tree", "-r", "--name-only", &published])
            .lines()
            .any(|path| path == "privacy/envelope.json"),
        "published encrypted"
    );

    // Its next turns would continue it as plain, which this Hub doesn't take yet: the push says
    // so, and nothing is sent.
    turn("turn 3");
    turn("turn 4");
    hub.undeclared.store(true, Ordering::Release);
    let refused = run(&["--yes", "push", "alice/app@work"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(
            "this Hub doesn't yet accept a conversation that continues one published encrypted"
        ),
        "{refused:?}"
    );
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), published);
}

/// Encrypted publication is retired: a session an older client published encrypted continues as
/// plain at the next push, and its turns already out keep the projection their readers were shown.
/// The push that switches it says so once. Nothing it or a later push sends carries an envelope, or
/// a value the encrypted projection had hidden, though plain capture would leave that value alone
/// in a new turn; someone the owner gave access to the originals, who resumes the session from them
/// on their own computer and publishes, sends none either. A client that published the session's
/// local records as they are would show readers what they were never shown; one that kept the
/// session encrypted would keep uploading the originals.
#[test]
fn an_encrypted_session_continues_as_plain_and_keeps_its_published_turns() {
    use agit::domain::session_privacy::{
        AccountIdentity, PublishedSnapshot, SessionPrivacy, SessionPublicationManifest,
    };
    const CANARY: &str = "canary-k7q2";
    // A value the encrypted projection published as it was, registered only later.
    const LATER: &str = "larkspur-overture-1931";
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    hub.legacy_encrypted
        .lock()
        .unwrap()
        .insert(format!("agit-{}", "a".repeat(40)));
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run_with = |args: &[&str], selected_home: &Path, vars: &[(&str, &Path)]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", selected_home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .envs(vars.iter().copied())
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let run_home = |args: &[&str], selected_home: &Path| run_with(args, selected_home, &[]);
    let run = |args: &[&str]| run_home(args, &home);
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    fs::create_dir_all(repo.root().join("memory")).unwrap();
    fs::write(repo.root().join("memory/notes.md"), "base memory").unwrap();
    // A script and a symbolic link on the file line, which the session changes while encrypted.
    fs::create_dir_all(repo.root().join("tools")).unwrap();
    fs::write(repo.root().join("tools/run.sh"), "#!/bin/sh\necho base\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            repo.root().join("tools/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("../README.md", repo.root().join("tools/readme")).unwrap();
    }
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    repo.git(&["checkout", "-b", "work"]).unwrap();
    let mut session = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    session.session_privacy = Some(SessionPrivacy {
        scheme: agit::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(AccountIdentity::new(&hub.base, "account-1").unwrap())).unwrap()
    });
    session.committer_account = Some(AccountIdentity::new(&hub.base, "account-1").unwrap());
    let record = |content: &str| {
        transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &session.session,
        )
    };
    let reply = |content: &str| {
        transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":content}]}})
            ),
            "claude-code",
            &session.session,
        )
    };
    let commit = |log: &str, message: &str| {
        storage::write_snapshot(repo.root(), log, log).unwrap();
        meta::write(repo.root(), &session).unwrap();
        repo.add_all().unwrap();
        repo.commit(message).unwrap();
    };
    let mut log = record(&format!("please read /Users/alice/{CANARY}/plan.txt"));
    log.push_str(&reply("noted"));
    commit(&log, "turn 0");
    log.push_str(&record("turn 1"));
    log.push_str(&record(&format!("deploy with {LATER}")));
    // While published encrypted, the session changes a memory file and adds a skill. No upload
    // carries either: an encrypted snapshot publishes its records only.
    fs::write(
        repo.root().join("memory/notes.md"),
        "MEMORY-CANARY changed while encrypted",
    )
    .unwrap();
    fs::create_dir_all(repo.root().join("skills")).unwrap();
    fs::write(
        repo.root().join("skills/new.md"),
        "SKILL-CANARY added while encrypted",
    )
    .unwrap();
    fs::write(
        repo.root().join("tools/run.sh"),
        "#!/bin/sh\necho SCRIPT-CANARY\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        fs::remove_file(repo.root().join("tools/readme")).unwrap();
        std::os::unix::fs::symlink("../memory/notes.md", repo.root().join("tools/readme")).unwrap();
    }
    commit(&log, "turn 1");
    let originals = log.clone();
    assert_success(&run(&["--yes", "push", "alice/app@work"]));
    let published = git(&remote, &["rev-parse", "refs/heads/work"]);
    let published_source = git(repo.root(), &["rev-parse", "HEAD"]);
    // Another session published encrypted then, which a later plain one merges in.
    let other = meta::Meta {
        session: format!("agit-{}", "9".repeat(40)),
        ..session.clone()
    };
    hub.legacy_encrypted
        .lock()
        .unwrap()
        .insert(other.session.clone());
    repo.git(&["checkout", "-q", "-b", "other", "main"])
        .unwrap();
    let other_log = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":format!("please read /Users/alice/{CANARY}/other.txt")}})
        ),
        "claude-code",
        &other.session,
    );
    storage::write_snapshot(repo.root(), &other_log, &other_log).unwrap();
    meta::write(repo.root(), &other).unwrap();
    repo.add_all().unwrap();
    repo.commit("other turn").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@other"]));
    let other_published = git(&remote, &["rev-parse", "refs/heads/other"]);
    assert!(
        git(&remote, &["ls-tree", "-r", "--name-only", &other_published])
            .lines()
            .any(|path| path == "privacy/envelope.json"),
        "the merged branch was published encrypted"
    );
    let other_source = git(repo.root(), &["rev-parse", "HEAD"]);
    // A third session published encrypted twice; its later snapshot leaves out a record the earlier
    // one published, as a revert does.
    let third = meta::Meta {
        session: format!("agit-{}", "8".repeat(40)),
        ..session.clone()
    };
    hub.legacy_encrypted
        .lock()
        .unwrap()
        .insert(third.session.clone());
    repo.git(&["checkout", "-q", "-b", "third", "main"])
        .unwrap();
    let third_record = |content: &str| {
        transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &third.session,
        )
    };
    // A full Claude Code record, as capture keeps it: its folder holds the hidden value too.
    let reverted = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({
                "type": "user",
                "uuid": "00000000-0000-4000-8000-000000000001",
                "parentUuid": null,
                "sessionId": "00000000-0000-4000-8000-000000000002",
                "cwd": format!("/Users/alice/{CANARY}/project"),
                "gitBranch": "main",
                "version": "2.1.314",
                "timestamp": "2026-11-04T08:00:00Z",
                "message": {"role": "user", "content": format!("please read /Users/alice/{CANARY}/reverted.txt")}
            })
        ),
        "claude-code",
        &third.session,
    );
    let kept_turn = third_record("a turn the revert kept");
    let third_log = format!("{reverted}{kept_turn}");
    storage::write_snapshot(repo.root(), &third_log, &third_log).unwrap();
    meta::write(repo.root(), &third).unwrap();
    // A memory file the session adds while published encrypted, which no upload carries.
    fs::create_dir_all(repo.root().join("memory")).unwrap();
    fs::write(repo.root().join("memory/third.md"), "THIRD-CANARY").unwrap();
    repo.add_all().unwrap();
    repo.commit("third turn").unwrap();
    storage::write_snapshot(repo.root(), &kept_turn, &kept_turn).unwrap();
    repo.add_all().unwrap();
    repo.commit("revert the third session's first turn")
        .unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@third"]));
    let third_published = git(&remote, &["rev-parse", "refs/heads/third"]);
    let third_earlier = git(&remote, &["rev-parse", &format!("{third_published}^")]);
    for commit in [&third_published, &third_earlier] {
        assert!(
            git(&remote, &["ls-tree", "-r", "--name-only", commit])
                .lines()
                .any(|path| path == "privacy/envelope.json"),
            "the third session was published encrypted"
        );
    }
    let third_source = git(repo.root(), &["rev-parse", "HEAD"]);
    repo.git(&["checkout", "-q", "work"]).unwrap();
    // What a commit's tree publishes outside its envelope.
    let public_text = |commit: &str| {
        git(&remote, &["ls-tree", "-r", commit])
            .lines()
            .filter(|entry| !entry.ends_with("\tprivacy/envelope.json"))
            .map(|entry| {
                let blob = entry.split_whitespace().nth(2).unwrap();
                git(&remote, &["cat-file", "blob", blob])
            })
            .collect::<String>()
    };
    assert!(
        git(&remote, &["ls-tree", "-r", "--name-only", &published])
            .lines()
            .any(|path| path == "privacy/envelope.json"),
        "an older client published the session encrypted"
    );
    let shown = public_text(&published);
    assert!(
        shown.contains("turn 1") && !shown.contains(CANARY),
        "the encrypted projection hid the path its readers were not shown"
    );

    // Every object a push added: no envelope, and nothing the projection hid.
    let added = |from: &[&str], to: &str| {
        let mut range = vec!["rev-list", "--objects", to];
        let excluded = from
            .iter()
            .map(|commit| format!("^{commit}"))
            .collect::<Vec<_>>();
        range.extend(excluded.iter().map(String::as_str));
        let objects = git(&remote, &range);
        let mut text = String::new();
        for line in objects.lines() {
            let mut fields = line.splitn(2, ' ');
            let oid = fields.next().unwrap();
            let path = fields.next().unwrap_or_default();
            assert!(
                !path.starts_with("privacy"),
                "{path} was sent after the switch"
            );
            if git(&remote, &["cat-file", "-t", oid]) == "blob" {
                text.push_str(&git(&remote, &["cat-file", "blob", oid]));
            }
        }
        assert!(
            !text.contains(CANARY),
            "a hidden value was sent after the switch"
        );
        text
    };

    hub.legacy_encrypted.lock().unwrap().clear();
    // A supervised push of the session as it is sends nothing, so nothing switches: the Hub keeps
    // it encrypted, nobody is told, and the supervisor's receipt names the encrypted snapshot.
    let request = temp.path().join("supervised.json");
    fs::write(
        &request,
        serde_json::to_vec(&json!({
            "version": 1,
            "request_id": "6f0b6a52-3f61-4f0e-9a3c-1f8f8b5b8c01",
            "repository": "alice/app",
            "branch": "work",
            "source": published_source,
            "destination": {"hub": hub.base, "agent_id": AGENT_ID},
        }))
        .unwrap(),
    )
    .unwrap();
    let registrations = hub.registrations.load(Ordering::Acquire);
    let supervised = run_with(
        &["--yes", "push", "alice/app@work"],
        &home,
        &[("AGIT_RC_SUPERVISOR_PUSH_RESULT", &request)],
    );
    assert_success(&supervised);
    assert!(
        !String::from_utf8_lossy(&supervised.stderr).contains("now publishes like"),
        "{supervised:?}"
    );
    assert_eq!(hub.registrations.load(Ordering::Acquire), registrations);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), published);
    let result: serde_json::Value = serde_json::from_slice(&fs::read(&request).unwrap()).unwrap();
    assert_eq!(result["publication"]["published"], published, "{result}");
    assert_ne!(result["publication"]["mode"], "session_plain", "{result}");
    let earlier = hub.previews.lock().unwrap().len();
    log.push_str(&record("turn 2 after the switch"));
    commit(&log, "turn 2");
    // A Hub that doesn't take the declaration that the carried turns were proven refuses such a
    // push, so this client sends nothing and says why.
    hub.undeclared.store(true, Ordering::Release);
    let before_refusal = git(&remote, &["rev-parse", "refs/heads/work"]);
    let undeclared = run(&["--yes", "push", "alice/app@work"]);
    assert!(!undeclared.status.success(), "{undeclared:?}");
    assert!(
        String::from_utf8_lossy(&undeclared.stderr).contains(
            "this Hub doesn't yet accept a conversation that continues one published encrypted"
        ),
        "{undeclared:?}"
    );
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/work"]),
        before_refusal
    );
    hub.undeclared.store(false, Ordering::Release);
    // A push that stops after its proof and inspection, here for want of the person's confirmation
    // of a public repository, has registered nothing: the Hub still holds the session encrypted,
    // and nobody was told.
    *hub.visibility.lock().unwrap() = "public";
    let registrations = hub.registrations.load(Ordering::Acquire);
    let unconfirmed = run(&["push", "alice/app@work"]);
    *hub.visibility.lock().unwrap() = "private";
    assert!(!unconfirmed.status.success(), "{unconfirmed:?}");
    assert_eq!(
        hub.registrations.load(Ordering::Acquire),
        registrations,
        "nothing is registered before the push is confirmed"
    );
    assert!(
        !String::from_utf8_lossy(&unconfirmed.stderr).contains("now publishes like"),
        "{unconfirmed:?}"
    );
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/work"]),
        before_refusal
    );
    // Registered after the encrypted publication: a record the plain tip shares with it goes out
    // as today's protection writes it, not as the encrypted projection's line.
    let register_later = |selected_home: &Path| {
        let mut add = Command::new(env!("CARGO_BIN_EXE_agit"));
        add.env_clear_local();
        for (name, value) in git_command(temp.path()).get_envs() {
            if let Some(value) = value {
                add.env(name, value);
            }
        }
        let mut add = add
            .current_dir(temp.path())
            .env("AGIT_HOME", selected_home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .args(["secrets", "add", "later", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        add.stdin
            .take()
            .unwrap()
            .write_all(LATER.as_bytes())
            .unwrap();
        assert_success(&add.wait_with_output().unwrap());
    };
    register_later(&home);
    assert!(
        public_text(&published).contains(LATER),
        "the encrypted projection published the value as it was"
    );
    hub.declarations.lock().unwrap().clear();
    let switching = run(&["--yes", "push", "alice/app@work"]);
    assert_success(&switching);
    let switched = git(&remote, &["rev-parse", "refs/heads/work"]);
    // Files held back go back as the file line had them, their modes included: the script stays
    // executable and the link stays a link to where it pointed.
    let mode = |path: &str| {
        git(&remote, &["ls-tree", &switched, "--", path])
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    #[cfg(unix)]
    {
        assert_eq!(mode("tools/run.sh"), "100755");
        assert_eq!(mode("tools/readme"), "120000");
        assert_eq!(
            git(
                &remote,
                &["cat-file", "-p", &format!("{switched}:tools/readme")]
            ),
            "../README.md"
        );
    }
    assert_eq!(
        git(
            &remote,
            &["cat-file", "-p", &format!("{switched}:tools/run.sh")]
        ),
        "#!/bin/sh\necho base"
    );
    let switched_text = public_text(&switched);
    assert!(
        switched_text.contains("deploy with ") && !switched_text.contains(LATER),
        "the plain tip carries the registered value replaced"
    );
    // The switching push declares it on the session's registration and on its receive.
    let declared = hub.declarations.lock().unwrap().clone();
    assert!(
        declared
            .iter()
            .any(|request| request.starts_with("POST ") && request.ends_with("/privacy-config")),
        "{declared:?}"
    );
    assert!(
        declared
            .iter()
            .any(|request| request.ends_with("/git-receive-pack")),
        "{declared:?}"
    );
    let told = String::from_utf8_lossy(&switching.stderr);
    assert_eq!(
        told.matches("now publishes like your other sessions")
            .count(),
        1,
        "{told}"
    );
    assert!(
        told.contains("(`agit privacy access show alice/app@work` lists them)"),
        "{told}"
    );
    // The access the notice points to is still there, on the session's encrypted snapshots.
    let access = run(&["--json", "privacy", "access", "policy", "alice/app@work"]);
    assert_success(&access);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&access.stdout).unwrap()["result"]["value"]["policy"]
            ["session_id"],
        session.session
    );
    // A session forked from it is a session of its own, plain: it has no access to originals to
    // show or change, and the earlier session's grants are not its own.
    repo.git(&["checkout", "-q", "-b", "forkb", &published_source])
        .unwrap();
    repo.git(&["rm", "-rq", "--ignore-unmatch", "privacy"])
        .unwrap();
    let forked = meta::Meta {
        session: format!("agit-{}", "f".repeat(40)),
        session_privacy: None,
        ..session.clone()
    };
    meta::write(repo.root(), &forked).unwrap();
    repo.add_all().unwrap();
    repo.commit("fork").unwrap();
    for args in [
        vec!["--json", "privacy", "access", "policy", "alice/app@forkb"],
        vec![
            "--yes",
            "privacy",
            "access",
            "revoke",
            "alice/app@forkb",
            "--member",
            "account-2",
        ],
    ] {
        let refused = run(&args);
        assert!(!refused.status.success(), "{args:?}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stdout)
                .contains("plain sessions use repository read access")
                || String::from_utf8_lossy(&refused.stderr)
                    .contains("plain sessions use repository read access"),
            "{args:?}: {refused:?}"
        );
    }
    // The same from the encrypted commit as the Hub holds it, whose envelope is in its tree and
    // names the earlier session: the fork still finds no grants of its own.
    repo.git(&[
        "fetch",
        "-q",
        remote.to_str().unwrap(),
        "refs/heads/work:refs/remotes/hub/work",
    ])
    .unwrap();
    repo.git(&["checkout", "-q", "-b", "forkc", &published])
        .unwrap();
    repo.git(&["rm", "-rq", "privacy"]).unwrap();
    meta::write(
        repo.root(),
        &meta::Meta {
            session: format!("agit-{}", "6".repeat(40)),
            ..forked.clone()
        },
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("fork from the published commit").unwrap();
    for args in [
        vec!["--json", "privacy", "access", "policy", "alice/app@forkc"],
        vec![
            "--yes",
            "privacy",
            "access",
            "revoke",
            "alice/app@forkc",
            "--member",
            "account-2",
        ],
    ] {
        let refused = run(&args);
        assert!(!refused.status.success(), "{args:?}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stdout)
                .contains("plain sessions use repository read access")
                || String::from_utf8_lossy(&refused.stderr)
                    .contains("plain sessions use repository read access"),
            "{args:?}: {refused:?}"
        );
    }
    repo.git(&["checkout", "-q", "work"]).unwrap();
    let mode = run(&["--json", "privacy", "mode", "alice/app@work"]);
    assert_success(&mode);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&mode.stdout).unwrap()["result"]["value"]["local_mode"],
        "plain"
    );
    let switched = git(&remote, &["rev-parse", "refs/heads/work"]);
    assert_eq!(
        git(&remote, &["rev-parse", &format!("{switched}^")]),
        published
    );
    let sent = added(&[&published], &switched);
    assert!(sent.contains("turn 2 after the switch"));
    // The files it changed while encrypted stay as the file line had them, and nothing else of
    // the file line is lost.
    assert!(
        !sent.contains("MEMORY-CANARY") && !sent.contains("SKILL-CANARY"),
        "{sent}"
    );
    assert_eq!(
        git(&remote, &["show", &format!("{switched}:memory/notes.md")]),
        "base memory"
    );
    assert_eq!(
        git(&remote, &["show", &format!("{switched}:README.md")]),
        "Shared file"
    );
    assert!(
        !git(&remote, &["ls-tree", "-r", "--name-only", &switched])
            .lines()
            .any(|path| path == "skills/new.md")
    );
    assert!(
        told.contains("Files it changed before this, such as memory or skills, are not uploaded."),
        "{told}"
    );
    for preview in &hub.previews.lock().unwrap()[earlier..] {
        let declared: SessionPublicationManifest =
            serde_json::from_value(preview["manifest"].clone()).unwrap();
        for snapshot in &declared.snapshots {
            let PublishedSnapshot::Plain {
                config_revision, ..
            } = snapshot
            else {
                panic!("a turn after the switch is declared as {snapshot:?}");
            };
            assert_eq!(*config_revision, 2);
        }
    }

    // A change made after the switch is new, and goes up.
    fs::write(
        repo.root().join("memory/notes.md"),
        "memory after the switch",
    )
    .unwrap();
    log.push_str(&record("turn 3"));
    commit(&log, "turn 3");
    let later = run(&["--yes", "push", "alice/app@work"]);
    assert_success(&later);
    assert!(
        !String::from_utf8_lossy(&later.stderr).contains("now publishes like"),
        "the switch is told once"
    );
    let head = git(&remote, &["rev-parse", "refs/heads/work"]);
    assert!(added(&[&switched], &head).contains("turn 3"));
    assert_eq!(
        git(&remote, &["show", &format!("{head}:memory/notes.md")]),
        "memory after the switch"
    );

    // Someone the owner gave access to the originals resumes the session from them, on their own
    // computer, and publishes what they hold: the original records and a turn of their own.
    let grantee = temp.path().join("grantee");
    startup_cache::seed(&grantee);
    fs::create_dir_all(grantee.join("credentials")).unwrap();
    save_at(
        &grantee
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-2".into()),
            username: "bob".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-collaborator".into(),
            refresh_token: "synthetic-collaborator-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    assert_success(&run_home(&["clone", "alice/app", "--no-bind"], &grantee));
    let theirs = Repo::at(grantee.join("repos/alice/app"));
    theirs
        .git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    theirs.git(&["config", "user.name", "Grantee"]).unwrap();
    theirs
        .git(&["config", "user.email", "grantee@example.invalid"])
        .unwrap();
    let publish_from = |branch: &str, log: &str| {
        theirs
            .git(&["checkout", "-q", "-b", branch, &published])
            .unwrap();
        theirs.git(&["rm", "-rq", "privacy"]).unwrap();
        storage::write_snapshot(theirs.root(), log, log).unwrap();
        meta::write(theirs.root(), &session).unwrap();
        theirs.add_all().unwrap();
        theirs.commit(branch).unwrap();
        run_home(&["--yes", "push", &format!("alice/app@{branch}")], &grantee)
    };
    let refused_as = |output: &std::process::Output, branch: &str, why: &str| {
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(why),
            "{output:?}"
        );
        assert!(
            git(&remote, &["show-ref"])
                .lines()
                .all(|line| !line.ends_with(&format!("refs/heads/{branch}")))
        );
    };
    const MISALIGNED: &str = "doesn't line up with its turns";
    // The head the owner published plain, continued on this computer as resume restores it: its
    // metadata and lines, then a turn. It goes through, its published lines go up as the Hub
    // holds them, and the encrypted snapshot below doesn't decide. Turn 1's originals in place of
    // what the head published are refused: the head proves only its own lines.
    let shown_head = storage::materialize_at(&remote, &head, meta::LOG_FILE).unwrap();
    let continue_head = |branch: &str| {
        theirs
            .git(&["checkout", "-q", "-b", branch, &head])
            .unwrap();
        let mut log = storage::materialize_at(theirs.root(), &head, meta::LOG_FILE).unwrap();
        log.push_str(&record(&format!("turn on {branch}")));
        storage::write_snapshot(theirs.root(), &log, &log).unwrap();
        theirs.add_all().unwrap();
        theirs.commit(branch).unwrap();
        assert_success(&run_home(
            &["--yes", "push", &format!("alice/app@{branch}")],
            &grantee,
        ));
        let continued = git(&remote, &["rev-parse", &format!("refs/heads/{branch}")]);
        assert!(added(&[&head], &continued).contains(&format!("turn on {branch}")));
        let sent = storage::materialize_at(&remote, &continued, meta::LOG_FILE).unwrap();
        assert!(sent.starts_with(&shown_head), "{sent}");

        let swapped = format!("{branch}-originals");
        theirs
            .git(&["checkout", "-q", "-b", &swapped, &head])
            .unwrap();
        let mut log = originals.clone();
        log.push_str(&record("turn 2 after the switch"));
        log.push_str(&record("turn 3"));
        log.push_str(&record(&format!("turn on {swapped}")));
        storage::write_snapshot(theirs.root(), &log, &log).unwrap();
        theirs.add_all().unwrap();
        theirs.commit(&swapped).unwrap();
        refused_as(
            &run_home(
                &["--yes", "push", &format!("alice/app@{swapped}")],
                &grantee,
            ),
            &swapped,
            MISALIGNED,
        );
    };
    continue_head("fromhead");
    // This computer holds the snapshot only as published, and has no access to its originals.
    // A LOG that drops a record before what it published is refused, and so is the honest
    // original LOG: without the originals, nothing proves its records are the published ones.
    let mut shifted = reply("noted");
    shifted.push_str(&record("turn 1"));
    shifted.push_str(&record("turn from a shifted log"));
    refused_as(&publish_from("shifted", &shifted), "shifted", MISALIGNED);
    let mut unproven = originals.clone();
    unproven.push_str(&record("turn from unproven originals"));
    refused_as(&publish_from("unproven", &unproven), "unproven", MISALIGNED);
    // A continuation of the projection itself proves itself, and publishes what was published.
    // This computer has no record of how the session was published, and registered a value the
    // projection published as it was: the plain continuation carries it replaced.
    register_later(&grantee);
    let mut continued = storage::materialize_at(theirs.root(), &published, meta::LOG_FILE).unwrap();
    assert!(continued.contains(LATER));
    continued.push_str(&record("turn from the projection"));
    assert_success(&publish_from("fromprojection", &continued));
    let fromprojection = git(&remote, &["rev-parse", "refs/heads/fromprojection"]);
    assert!(added(&[&head], &fromprojection).contains("turn from the projection"));
    let projected_text = public_text(&fromprojection);
    assert!(
        projected_text.contains("deploy with ") && !projected_text.contains(LATER),
        "a clone's plain continuation carries the registered value replaced"
    );

    // Once the originals are unlocked on this computer, as `agit privacy unlock` leaves them,
    // the original LOG is proven record by record. Each of its published turns goes up as the
    // line published then, and the hidden value doesn't.
    let unlocked = theirs
        .common_dir()
        .unwrap()
        .join("agit/privacy-recovery")
        .join(&published);
    fs::create_dir_all(&unlocked).unwrap();
    for (path, bytes) in storage::snapshot_files(&originals, &originals).unwrap() {
        let file = unlocked.join(&path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, bytes).unwrap();
    }
    fs::create_dir_all(unlocked.join("session")).unwrap();
    fs::write(unlocked.join(meta::FILE), meta::to_text(&session).unwrap()).unwrap();
    let envelope = git_command(temp.path())
        .args(["-C", theirs.root().to_str().unwrap(), "cat-file", "blob"])
        .arg(format!("{published}:privacy/envelope.json"))
        .output()
        .unwrap();
    assert!(envelope.status.success());
    agit::domain::privacy_recovery::write_manifest(&unlocked, &envelope.stdout).unwrap();
    let mut resumed = originals.clone();
    resumed.push_str(&record("turn from elsewhere"));
    assert_success(&publish_from("elsewhere", &resumed));
    let elsewhere = git(&remote, &["rev-parse", "refs/heads/elsewhere"]);
    // On this computer the published encrypted snapshot names the session by its public id, not
    // by the id the plain snapshots carry; access to its originals is still found for them.
    let public_id: meta::Meta = serde_json::from_str(&git(
        &remote,
        &["show", &format!("{published}:session/meta.json")],
    ))
    .unwrap();
    assert_ne!(
        public_id.session, session.session,
        "published under a public id"
    );
    let access = run_home(
        &[
            "--json",
            "privacy",
            "access",
            "policy",
            "alice/app@elsewhere",
        ],
        &grantee,
    );
    assert_success(&access);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&access.stdout).unwrap()["result"]["value"]["policy"]
            ["session_id"],
        session.session
    );
    assert!(added(&[&head], &elsewhere).contains("turn from elsewhere"));
    // With the originals there, a continuation of the projection itself still goes through.
    let mut continued = storage::materialize_at(theirs.root(), &published, meta::LOG_FILE).unwrap();
    continued.push_str(&record("turn from the projection, originals held"));
    assert_success(&publish_from("fromprojection2", &continued));
    continue_head("fromhead2");
    // A session forked from the encrypted snapshot with its originals is merged into a
    // conversation on the head published plain, carrying the originals past the head's own lines.
    // The head proves only its own lines: the originals past them are held to the encrypted
    // snapshot below it, as they were published.
    theirs
        .git(&["checkout", "-q", "-b", "originals-fork", &published])
        .unwrap();
    theirs.git(&["rm", "-rq", "privacy"]).unwrap();
    let mut fork_log = originals.clone();
    fork_log.push_str(&record("turn on the originals' fork"));
    storage::write_snapshot(theirs.root(), &fork_log, &fork_log).unwrap();
    meta::write(
        theirs.root(),
        &meta::Meta {
            session: format!("agit-{}", "7".repeat(40)),
            session_privacy: None,
            ..session.clone()
        },
    )
    .unwrap();
    theirs.add_all().unwrap();
    theirs.commit("fork with the originals").unwrap();
    let shown_published = storage::parse_envelopes(
        &storage::materialize_at(&remote, &published, meta::LOG_FILE).unwrap(),
    )
    .unwrap();
    theirs
        .git(&["checkout", "-q", "-b", "remerge", &head])
        .unwrap();
    let mut log = storage::materialize_at(theirs.root(), &head, meta::LOG_FILE).unwrap();
    log.push_str(&record("turn on remerge"));
    storage::write_snapshot(theirs.root(), &log, &log).unwrap();
    theirs.add_all().unwrap();
    theirs.commit("turn on remerge").unwrap();
    theirs
        .git(&[
            "merge",
            "-q",
            "--no-ff",
            "--no-commit",
            "-s",
            "ours",
            "originals-fork",
        ])
        .unwrap();
    log.push_str(&originals);
    storage::write_snapshot(theirs.root(), &log, &log).unwrap();
    theirs.add_all().unwrap();
    theirs.commit("merge the originals' continuation").unwrap();
    // Its line through the fork reaches the earlier encrypted snapshot without passing the head,
    // and this computer can't read that one's originals yet: nothing is sent.
    let refused = run_home(&["--yes", "push", "alice/app@remerge"], &grantee);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("can't tell which of its turns were already published"),
        "{refused:?}"
    );
    // It says what the person can do instead.
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("agit privacy unlock"),
        "{refused:?}"
    );
    assert!(
        git_command(temp.path())
            .args([
                "-C",
                remote.to_str().unwrap(),
                "rev-parse",
                "--verify",
                "-q",
                "refs/heads/remerge"
            ])
            .output()
            .unwrap()
            .stdout
            .is_empty(),
        "nothing was sent"
    );
    // As published, or narrowed further by today's protection: the value registered since goes
    // out replaced.
    let narrowed = |published: &str, sent: &str| {
        published == sent
            || published.split_once(LATER).is_some_and(|(before, after)| {
                !sent.contains(LATER)
                    && sent.len() > before.len() + after.len()
                    && sent.starts_with(before)
                    && sent.ends_with(after)
            })
    };
    let shown = storage::materialize_at(&remote, &published, meta::LOG_FILE).unwrap();
    let sent = storage::materialize_at(&remote, &elsewhere, meta::LOG_FILE).unwrap();
    for envelope in storage::parse_envelopes(&shown).unwrap() {
        assert!(
            storage::parse_envelopes(&sent).unwrap().iter().any(|sent| {
                sent.source == envelope.source
                    && narrowed(&envelope.content.to_string(), &sent.content.to_string())
            }),
            "a published turn goes up as it was published: {envelope:?}"
        );
    }

    // That computer can read the earlier snapshot's originals too, as access to the session gives
    // it: a merge below asks every encrypted snapshot it reaches, and one it can't read refuses.
    let earlier_public = git(&remote, &["rev-parse", &format!("{published}^")]);
    let earlier_originals =
        storage::materialize_at(repo.root(), &format!("{published_source}^"), meta::LOG_FILE)
            .unwrap();
    let unlocked = theirs
        .common_dir()
        .unwrap()
        .join("agit/privacy-recovery")
        .join(&earlier_public);
    fs::create_dir_all(&unlocked).unwrap();
    for (path, bytes) in storage::snapshot_files(&earlier_originals, &earlier_originals).unwrap() {
        let file = unlocked.join(&path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, bytes).unwrap();
    }
    fs::create_dir_all(unlocked.join("session")).unwrap();
    fs::write(unlocked.join(meta::FILE), meta::to_text(&session).unwrap()).unwrap();
    let envelope = git_command(temp.path())
        .args(["-C", theirs.root().to_str().unwrap(), "cat-file", "blob"])
        .arg(format!("{earlier_public}:privacy/envelope.json"))
        .output()
        .unwrap();
    assert!(envelope.status.success());
    agit::domain::privacy_recovery::write_manifest(&unlocked, &envelope.stdout).unwrap();
    // Now that it can, the re-merge goes through: past the head's own lines, every turn goes up as
    // the encrypted snapshots published it, or narrowed further by today's protection.
    assert_success(&run_home(&["--yes", "push", "alice/app@remerge"], &grantee));
    let remerge = git(&remote, &["rev-parse", "refs/heads/remerge"]);
    assert!(added(&[&head, &elsewhere], &remerge).contains("turn on remerge"));
    let sent = storage::materialize_at(&remote, &remerge, meta::LOG_FILE).unwrap();
    assert!(sent.starts_with(&shown_head), "{sent}");
    for envelope in storage::parse_envelopes(&sent[shown_head.len()..]).unwrap() {
        assert!(
            envelope.content.to_string().contains("turn on remerge")
                || shown_published.iter().any(|published| {
                    published.source == envelope.source
                        && narrowed(
                            &published.content.to_string(),
                            &envelope.content.to_string(),
                        )
                }),
            "{envelope:?}"
        );
    }

    // On that computer a plain conversation merging the published commit in, keeping its own
    // tree, holds the merged turns to it too.
    let main_tip = git(&remote, &["rev-parse", "refs/heads/main"]);
    theirs
        .git(&["checkout", "-q", "-b", "crossmerge", &main_tip])
        .unwrap();
    let crossing = meta::Meta {
        session: format!("agit-{}", "e".repeat(40)),
        session_privacy: None,
        ..session.clone()
    };
    let own = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"a turn of their own"}})
        ),
        "claude-code",
        &crossing.session,
    );
    storage::write_snapshot(theirs.root(), &own, &own).unwrap();
    meta::write(theirs.root(), &crossing).unwrap();
    theirs.add_all().unwrap();
    theirs.commit("a turn of their own").unwrap();
    theirs
        .git(&[
            "merge",
            "-q",
            "--no-ff",
            "--no-commit",
            "-s",
            "ours",
            &published,
        ])
        .unwrap();
    let mut combined = own.clone();
    combined.push_str(&originals);
    storage::write_snapshot(theirs.root(), &combined, &combined).unwrap();
    meta::write(theirs.root(), &crossing).unwrap();
    theirs.add_all().unwrap();
    theirs.commit("merge the published conversation").unwrap();
    assert_success(&run_home(
        &["--yes", "push", "alice/app@crossmerge"],
        &grantee,
    ));
    let crossmerge = git(&remote, &["rev-parse", "refs/heads/crossmerge"]);
    assert!(added(&[&main_tip, &published], &crossmerge).contains("a turn of their own"));

    // A new session whose records say encrypted, as one inherited from an encrypted link does,
    // registers plain.
    repo.git(&["checkout", "-q", "-b", "inherited", "main"])
        .unwrap();
    let inherited_session = meta::Meta {
        session: format!("agit-{}", "c".repeat(40)),
        ..session.clone()
    };
    let inherited_log = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"an inherited turn"}})
        ),
        "claude-code",
        &inherited_session.session,
    );
    storage::write_snapshot(repo.root(), &inherited_log, &inherited_log).unwrap();
    meta::write(repo.root(), &inherited_session).unwrap();
    repo.add_all().unwrap();
    repo.commit("inherited turn").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@inherited"]));
    let inherited = git(&remote, &["rev-parse", "refs/heads/inherited"]);
    assert!(added(&[&elsewhere], &inherited).contains("an inherited turn"));
    let registered: meta::Meta = serde_json::from_str(&git(
        &remote,
        &["show", &format!("{inherited}:session/meta.json")],
    ))
    .unwrap();
    assert_eq!(
        registered.session_privacy.unwrap().scheme,
        agit::domain::session_privacy::SessionScheme::Plain
    );

    // A plain conversation that merges in one published encrypted publishes the merged turns no
    // wider than that projection showed them.
    repo.git(&["checkout", "-q", "-b", "merged", "main"])
        .unwrap();
    let merging = meta::Meta {
        session: format!("agit-{}", "d".repeat(40)),
        session_privacy: None,
        ..session.clone()
    };
    let own = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"a plain turn"}})
        ),
        "claude-code",
        &merging.session,
    );
    storage::write_snapshot(repo.root(), &own, &own).unwrap();
    meta::write(repo.root(), &merging).unwrap();
    repo.add_all().unwrap();
    repo.commit("a plain turn").unwrap();
    repo.git(&[
        "merge",
        "-q",
        "--no-ff",
        "--no-commit",
        "-s",
        "ours",
        &published_source,
    ])
    .unwrap();
    let mut combined = own.clone();
    combined.push_str(&originals);
    storage::write_snapshot(repo.root(), &combined, &combined).unwrap();
    meta::write(repo.root(), &merging).unwrap();
    repo.add_all().unwrap();
    repo.commit("merge the encrypted conversation").unwrap();
    assert_eq!(
        git(repo.root(), &["rev-parse", "HEAD^2"]),
        published_source,
        "a merge commit"
    );
    assert_success(&run(&["--yes", "push", "alice/app@merged"]));
    let merged = git(&remote, &["rev-parse", "refs/heads/merged"]);
    let main_tip = git(&remote, &["rev-parse", "refs/heads/main"]);
    assert!(added(&[&main_tip, &published], &merged).contains("a plain turn"));

    // The same on the head published plain, as another computer resumes it: the merged turns are
    // held to the merged branch's projection, which the head's lines don't cover.
    repo.git(&[
        "fetch",
        "-q",
        remote.to_str().unwrap(),
        "refs/heads/work:refs/remotes/hub/work",
    ])
    .unwrap();
    repo.git(&["checkout", "-q", "-b", "headmerge", &head])
        .unwrap();
    let mut continued = storage::materialize_at(repo.root(), &head, meta::LOG_FILE).unwrap();
    continued.push_str(&record("turn on headmerge"));
    storage::write_snapshot(repo.root(), &continued, &continued).unwrap();
    repo.add_all().unwrap();
    repo.commit("turn on headmerge").unwrap();
    repo.git(&[
        "merge",
        "-q",
        "--no-ff",
        "--no-commit",
        "-s",
        "ours",
        &other_source,
    ])
    .unwrap();
    continued.push_str(&other_log);
    storage::write_snapshot(repo.root(), &continued, &continued).unwrap();
    repo.add_all().unwrap();
    repo.commit("merge the other conversation").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@headmerge"]));
    let headmerge = git(&remote, &["rev-parse", "refs/heads/headmerge"]);
    // The branch's tip on the Hub is the merge, not the turn below it.
    assert_eq!(
        git(&remote, &["rev-list", "--parents", "-n", "1", &headmerge])
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>(),
        vec![
            git(&remote, &["rev-parse", &format!("{headmerge}^1")]).as_str(),
            other_published.as_str(),
        ],
        "the merge is published as the branch tip"
    );
    assert!(added(&[&head, &other_published], &headmerge).contains("turn on headmerge"));
    let sent = storage::materialize_at(&remote, &headmerge, meta::LOG_FILE).unwrap();
    assert!(sent.starts_with(&shown_head), "{sent}");
    // Past the head's lines, a merged turn goes up only as the merged branch's projection showed it.
    let projected = storage::parse_envelopes(
        &storage::materialize_at(&remote, &other_published, meta::LOG_FILE).unwrap(),
    )
    .unwrap();
    for envelope in storage::parse_envelopes(&sent[shown_head.len()..]).unwrap() {
        assert!(
            envelope.content.to_string().contains("turn on headmerge")
                || projected.iter().any(|published| {
                    published.source == envelope.source
                        && published.object_hash == envelope.object_hash
                        && published.content == envelope.content
                }),
            "{envelope:?}"
        );
    }

    // That plain head now holds the other session's published turn. On the grantee's computer,
    // with that session's originals unlocked, a branch continues them from its encrypted snapshot,
    // and a conversation on the plain head merges it in, carrying the originals past the head's
    // lines. They go up as the encrypted snapshot published them: the plain head proves only its
    // own lines.
    theirs
        .git(&[
            "fetch",
            "-q",
            remote.to_str().unwrap(),
            "refs/heads/headmerge:refs/remotes/hub/headmerge",
        ])
        .unwrap();
    let unlocked = theirs
        .common_dir()
        .unwrap()
        .join("agit/privacy-recovery")
        .join(&other_published);
    fs::create_dir_all(&unlocked).unwrap();
    for (path, bytes) in storage::snapshot_files(&other_log, &other_log).unwrap() {
        let file = unlocked.join(&path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, bytes).unwrap();
    }
    fs::create_dir_all(unlocked.join("session")).unwrap();
    fs::write(unlocked.join(meta::FILE), meta::to_text(&other).unwrap()).unwrap();
    let envelope = git_command(temp.path())
        .args(["-C", theirs.root().to_str().unwrap(), "cat-file", "blob"])
        .arg(format!("{other_published}:privacy/envelope.json"))
        .output()
        .unwrap();
    assert!(envelope.status.success());
    agit::domain::privacy_recovery::write_manifest(&unlocked, &envelope.stdout).unwrap();
    theirs
        .git(&["checkout", "-q", "-b", "continued-b", &other_published])
        .unwrap();
    theirs.git(&["rm", "-rq", "privacy"]).unwrap();
    let mut restored = other_log.clone();
    restored.push_str(&transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"a turn continuing the other session"}})
        ),
        "claude-code",
        &other.session,
    ));
    storage::write_snapshot(theirs.root(), &restored, &restored).unwrap();
    meta::write(theirs.root(), &other).unwrap();
    theirs.add_all().unwrap();
    theirs.commit("continue the other session").unwrap();
    theirs
        .git(&["checkout", "-q", "-b", "bmerge", &headmerge])
        .unwrap();
    theirs
        .git(&[
            "merge",
            "-q",
            "--no-ff",
            "--no-commit",
            "-s",
            "ours",
            "continued-b",
        ])
        .unwrap();
    let mut log = storage::materialize_at(theirs.root(), &headmerge, meta::LOG_FILE).unwrap();
    let shown_merged = log.clone();
    log.push_str(&restored);
    storage::write_snapshot(theirs.root(), &log, &log).unwrap();
    theirs.add_all().unwrap();
    theirs.commit("merge the continued session").unwrap();
    assert_success(&run_home(&["--yes", "push", "alice/app@bmerge"], &grantee));
    let bmerge = git(&remote, &["rev-parse", "refs/heads/bmerge"]);
    assert!(added(&[&headmerge], &bmerge).contains("a turn continuing the other session"));
    let sent = storage::materialize_at(&remote, &bmerge, meta::LOG_FILE).unwrap();
    assert!(sent.starts_with(&shown_merged), "{sent}");
    for envelope in storage::parse_envelopes(&sent[shown_merged.len()..]).unwrap() {
        assert!(
            envelope
                .content
                .to_string()
                .contains("a turn continuing the other session")
                || projected.iter().any(|published| {
                    published.source == envelope.source
                        && published.object_hash == envelope.object_hash
                        && published.content == envelope.content
                }),
            "{envelope:?}"
        );
    }

    // On the owner's computer, a conversation on the plain head merges the third session's later
    // snapshot, bringing back the record its revert left out, past the head's lines. Only the
    // earlier snapshot published that record, and it goes up as that one published it.
    repo.git(&["checkout", "-q", "-b", "revmerge", &head])
        .unwrap();
    let mut log = storage::materialize_at(repo.root(), &head, meta::LOG_FILE).unwrap();
    log.push_str(&record("turn on revmerge"));
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    repo.add_all().unwrap();
    repo.commit("turn on revmerge").unwrap();
    repo.git(&[
        "merge",
        "-q",
        "--no-ff",
        "--no-commit",
        "-s",
        "ours",
        &third_source,
    ])
    .unwrap();
    log.push_str(&reverted);
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    // The merge takes that memory file too; it isn't uploaded through the merge either.
    repo.git(&["checkout", &third_source, "--", "memory/third.md"])
        .unwrap();
    repo.add_all().unwrap();
    repo.commit("merge the reverted record back").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@revmerge"]));
    let revmerge = git(&remote, &["rev-parse", "refs/heads/revmerge"]);
    assert!(
        !git(&remote, &["ls-tree", "-r", "--name-only", &revmerge])
            .lines()
            .any(|path| path == "memory/third.md"),
        "a file the merged session changed while encrypted went up"
    );
    assert!(added(&[&head, &third_published], &revmerge).contains("turn on revmerge"));
    let sent = storage::materialize_at(&remote, &revmerge, meta::LOG_FILE).unwrap();
    assert!(sent.starts_with(&shown_head), "{sent}");
    let earlier_shown = storage::parse_envelopes(
        &storage::materialize_at(&remote, &third_earlier, meta::LOG_FILE).unwrap(),
    )
    .unwrap();
    let back = storage::parse_envelopes(&sent[shown_head.len()..]).unwrap();
    assert_eq!(back.len(), 2, "{sent}");
    assert!(back[0].content.to_string().contains("turn on revmerge"));
    assert_eq!(
        (&back[1].source, &back[1].object_hash, &back[1].content),
        (
            &earlier_shown[0].source,
            &earlier_shown[0].object_hash,
            &earlier_shown[0].content
        ),
        "{sent}"
    );
    assert!(!sent.contains("/Users/alice"), "{sent}");

    // The same with the Hub's copy of the later snapshot merged in, as a checkout that took it
    // from the Hub holds it: this computer published it, so the originals are still here.
    repo.git(&[
        "fetch",
        "-q",
        remote.to_str().unwrap(),
        "refs/heads/third:refs/remotes/hub/third",
    ])
    .unwrap();
    repo.git(&["checkout", "-q", "-b", "revmerge-public", &head])
        .unwrap();
    let mut log = storage::materialize_at(repo.root(), &head, meta::LOG_FILE).unwrap();
    log.push_str(&record("turn on revmerge-public"));
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    repo.add_all().unwrap();
    repo.commit("turn on revmerge-public").unwrap();
    repo.git(&[
        "merge",
        "-q",
        "--no-ff",
        "--no-commit",
        "-s",
        "ours",
        &third_published,
    ])
    .unwrap();
    log.push_str(&reverted);
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    repo.add_all().unwrap();
    repo.commit("merge the reverted record back from the Hub's copy")
        .unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@revmerge-public"]));
    let revmerge_public = git(&remote, &["rev-parse", "refs/heads/revmerge-public"]);
    assert!(
        added(&[&head, &third_published], &revmerge_public).contains("turn on revmerge-public")
    );
    let sent = storage::materialize_at(&remote, &revmerge_public, meta::LOG_FILE).unwrap();
    assert!(sent.starts_with(&shown_head), "{sent}");
    let back = storage::parse_envelopes(&sent[shown_head.len()..]).unwrap();
    assert_eq!(back.len(), 2, "{sent}");
    assert!(
        back[0]
            .content
            .to_string()
            .contains("turn on revmerge-public")
    );
    assert_eq!(
        (&back[1].source, &back[1].object_hash, &back[1].content),
        (
            &earlier_shown[0].source,
            &earlier_shown[0].object_hash,
            &earlier_shown[0].content
        ),
        "{sent}"
    );
    assert!(!sent.contains("/Users/alice"), "{sent}");
}

/// A plain conversation that takes in an encrypted session's records, on the computer that
/// published it, publishes each as the encrypted snapshot published it, or is refused with nothing
/// sent: readers were only ever shown that form. That holds whether it merges the Hub's copy or
/// this computer's own commits, forks it, or reaches it past a file-line commit, and for a record a
/// later snapshot left out, as a revert does, which goes up as the earlier one published it. On a
/// computer that holds those records but can't read the snapshot's originals, the merge is refused.
/// A plain session with no encrypted ancestry publishes as before.
#[test]
fn records_from_an_encrypted_session_go_up_as_published_or_are_refused() {
    use agit::domain::session_privacy::{AccountIdentity, SessionPrivacy};
    const CANARY: &str = "canary-r3v9";
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));

    // An encrypted session: its first snapshot holds a full record whose folder is hidden; its
    // later one leaves that record out, as a revert does.
    repo.git(&["checkout", "-q", "-b", "secret"]).unwrap();
    let mut encrypted = meta::Meta::new(
        format!("agit-{}", "c".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    encrypted.session_privacy = Some(SessionPrivacy {
        scheme: agit::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(AccountIdentity::new(&hub.base, "account-1").unwrap())).unwrap()
    });
    encrypted.committer_account = Some(AccountIdentity::new(&hub.base, "account-1").unwrap());
    let reverted = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({
                "type": "user",
                "uuid": "00000000-0000-4000-8000-000000000001",
                "parentUuid": null,
                "sessionId": "00000000-0000-4000-8000-000000000002",
                "cwd": format!("/Users/alice/{CANARY}/project"),
                "gitBranch": "main",
                "version": "2.1.314",
                "timestamp": "2026-11-04T08:00:00Z",
                "message": {"role": "user", "content": format!("please read /Users/alice/{CANARY}/notes.txt")}
            })
        ),
        "claude-code",
        &encrypted.session,
    );
    let kept = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"a turn the revert kept"}})
        ),
        "claude-code",
        &encrypted.session,
    );
    let first = format!("{reverted}{kept}");
    write_snapshot(repo.root(), &first, &first).unwrap();
    meta::write(repo.root(), &encrypted).unwrap();
    repo.add_all().unwrap();
    repo.commit("encrypted turn").unwrap();
    write_snapshot(repo.root(), &kept, &kept).unwrap();
    repo.add_all().unwrap();
    repo.commit("revert the first turn").unwrap();
    let later_source = git(repo.root(), &["rev-parse", "HEAD"]);
    let earlier_source = git(repo.root(), &["rev-parse", "HEAD^"]);
    // Registered encrypted by a client from before encrypted publication was retired.
    hub.legacy_encrypted
        .lock()
        .unwrap()
        .insert(encrypted.session.clone());
    assert_success(&run(&["--yes", "push", "alice/app@secret"]));
    hub.legacy_encrypted.lock().unwrap().clear();
    let later = git(&remote, &["rev-parse", "refs/heads/secret"]);
    let earlier = git(&remote, &["rev-parse", &format!("{later}^")]);
    for commit in [&earlier, &later] {
        assert!(
            git(&remote, &["ls-tree", "-r", "--name-only", commit])
                .lines()
                .any(|path| path == "privacy/envelope.json"),
            "published encrypted"
        );
    }
    assert!(
        !storage::materialize_at(&remote, &earlier, meta::LOG_FILE)
            .unwrap()
            .contains(CANARY),
        "the encrypted snapshot hid the folder"
    );

    repo.git(&[
        "fetch",
        "-q",
        remote.to_str().unwrap(),
        "refs/heads/secret:refs/remotes/hub/secret",
    ])
    .unwrap();
    let everything = || {
        (
            git(&remote, &["for-each-ref"]),
            git(&remote, &["rev-list", "--all", "--objects"]),
        )
    };
    let shown: Vec<_> = [&earlier, &later]
        .iter()
        .flat_map(|commit| {
            storage::parse_envelopes(
                &storage::materialize_at(&remote, commit, meta::LOG_FILE).unwrap(),
            )
            .unwrap()
        })
        .collect();
    // A push of `branch` either goes through with every record of the encrypted session exactly as
    // a snapshot of it published it (only the session binding differs) and nothing hidden on the
    // Hub, or is refused with the Hub unchanged.
    let published_or_refused = |branch: &str, own: &[&str]| {
        let before = everything();
        let pushed = run(&["--yes", "push", &format!("alice/app@{branch}")]);
        if !pushed.status.success() {
            let told = String::from_utf8_lossy(&pushed.stderr).into_owned();
            assert!(told.contains("nothing was sent"), "{branch}: {told}");
            assert_eq!(everything(), before, "{branch}: the Hub gained nothing");
            return Err(told);
        }
        let tip = git(&remote, &["rev-parse", &format!("refs/heads/{branch}")]);
        for line in git(&remote, &["rev-list", "--all", "--objects"]).lines() {
            if before.1.contains(line) {
                continue;
            }
            let oid = line.split_whitespace().next().unwrap();
            if git(&remote, &["cat-file", "-t", oid]) == "blob" {
                assert!(
                    !git(&remote, &["cat-file", "blob", oid]).contains(CANARY),
                    "{branch}: a hidden value reached the Hub"
                );
            }
        }
        for envelope in storage::parse_envelopes(
            &storage::materialize_at(&remote, &tip, meta::LOG_FILE).unwrap(),
        )
        .unwrap()
        {
            let text = envelope.content.to_string();
            assert!(
                own.iter().any(|own| text.contains(own))
                    || shown.iter().any(|published| {
                        published.source == envelope.source
                            && published.object_hash == envelope.object_hash
                            && published.content == envelope.content
                    }),
                "{branch}: {envelope:?}"
            );
        }
        Ok(())
    };
    // A plain conversation takes `snapshot` and carries the record back. Its own turn publishes;
    // the merge is refused, and the Hub gains nothing.
    let carry_back = |branch: &str, session: char, snapshot: &str| {
        repo.git(&["checkout", "-q", "-b", branch, "main"]).unwrap();
        let plain = meta::Meta::new(
            format!("agit-{}", session.to_string().repeat(40)),
            "claude-code".into(),
            String::new(),
        );
        let own = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":format!("a plain turn on {branch}")}})
            ),
            "claude-code",
            &plain.session,
        );
        write_snapshot(repo.root(), &own, &own).unwrap();
        meta::write(repo.root(), &plain).unwrap();
        repo.add_all().unwrap();
        repo.commit("a plain turn").unwrap();
        assert_success(&run(&["--yes", "push", &format!("alice/app@{branch}")]));
        repo.git(&[
            "merge",
            "-q",
            "--no-ff",
            "--no-commit",
            "-s",
            "ours",
            snapshot,
        ])
        .unwrap();
        let log = format!("{own}{reverted}");
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), &plain).unwrap();
        repo.add_all().unwrap();
        repo.commit("merge the record back").unwrap();
        if let Err(told) = published_or_refused(branch, &["a plain turn on"]) {
            panic!("{branch}: this computer can read the snapshots it merges: {told}");
        }
    };
    // The record is in the merged snapshot itself.
    carry_back("merged", 'd', &earlier);
    // The merged snapshot left it out, as a revert does; only the earlier one published it.
    carry_back("reverted", 'e', &later);
    // This computer's own commits of that session, rather than the Hub's copies.
    carry_back("merged-source", 'f', &earlier_source);
    carry_back("reverted-source", '1', &later_source);

    // A fork of the Hub's copy of the snapshot, as `agit fork` writes one: the envelope left out,
    // a new plain session holding the restored originals, and a turn of its own.
    repo.git(&["checkout", "-q", "-b", "forked", &earlier])
        .unwrap();
    repo.git(&["rm", "-rq", "privacy"]).unwrap();
    let forked = meta::Meta::new(
        format!("agit-{}", "2".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let mut log = first.clone();
    log.push_str(&transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"a turn on the fork"}})
        ),
        "claude-code",
        &forked.session,
    ));
    write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &forked).unwrap();
    repo.add_all().unwrap();
    repo.commit("fork").unwrap();
    published_or_refused("forked", &["a turn on the fork"]).unwrap();

    // A file-line commit between the encrypted snapshot and the plain session hides nothing.
    repo.git(&["checkout", "-q", "-b", "through-file-line", &earlier])
        .unwrap();
    repo.git(&["rm", "-rq", "privacy"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    repo.add_all().unwrap();
    repo.commit("file line").unwrap();
    let on_top = meta::Meta::new(
        format!("agit-{}", "3".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let mut log = first.clone();
    log.push_str(&transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"a turn past the file line"}})
        ),
        "claude-code",
        &on_top.session,
    ));
    write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &on_top).unwrap();
    repo.add_all().unwrap();
    repo.commit("a plain turn past the file line").unwrap();
    published_or_refused("through-file-line", &["a turn past the file line"]).unwrap();

    let main_tip = git(&remote, &["rev-parse", "refs/heads/main"]);
    // Another computer of the same account, which holds that session's record (copied, or kept
    // from when its originals were readable) but can't read the snapshot's originals: a merge of
    // the Hub's copy carrying the record past its own line is refused, and nothing is sent.
    let other_home = temp.path().join("other");
    startup_cache::seed(&other_home);
    fs::create_dir_all(other_home.join("credentials")).unwrap();
    fs::copy(
        home.join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        other_home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
    )
    .unwrap();
    let run_other = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &other_home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    assert_success(&run_other(&["clone", "alice/app", "--no-bind"]));
    let theirs = Repo::at(other_home.join("repos/alice/app"));
    theirs
        .git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    theirs
        .git(&["config", "user.name", "Second Computer"])
        .unwrap();
    theirs
        .git(&["config", "user.email", "second@example.invalid"])
        .unwrap();
    theirs
        .git(&["checkout", "-q", "-b", "unreadable", &main_tip])
        .unwrap();
    let plain = meta::Meta::new(
        format!("agit-{}", "4".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let own = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"a plain turn elsewhere"}})
        ),
        "claude-code",
        &plain.session,
    );
    write_snapshot(theirs.root(), &own, &own).unwrap();
    meta::write(theirs.root(), &plain).unwrap();
    theirs.add_all().unwrap();
    theirs.commit("a plain turn elsewhere").unwrap();
    theirs
        .git(&[
            "merge",
            "-q",
            "--no-ff",
            "--no-commit",
            "-s",
            "ours",
            &later,
        ])
        .unwrap();
    let log = format!("{own}{reverted}");
    write_snapshot(theirs.root(), &log, &log).unwrap();
    meta::write(theirs.root(), &plain).unwrap();
    theirs.add_all().unwrap();
    theirs.commit("merge the record back").unwrap();
    let before = everything();
    let refused = run_other(&["--yes", "push", "alice/app@unreadable"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("nothing was sent"),
        "{refused:?}"
    );
    assert_eq!(everything(), before, "the Hub gained nothing");
}

/// A branch the Hub refused as withdrawn stays in the checkout and out of later pushes until it is
/// named again, and `agit status` lists it as set aside for as long as it is. The refusal comes
/// from the preview, before anything is sent, so the Hub keeps the branch where the withdrawal left
/// it. The next push publishes the other branches and leaves the withdrawn one out. An automatic
/// push names its branch but does not ask again, and fails without a request rather than report a
/// push it did not make. A push that names the branch asks again and publishes it once the
/// withdrawal is restored, and so are version tags refused after their branch was already
/// published, even once a later push published the branch without them and local records show it
/// unchanged. Only the Hub accepting a named ref lets it go: a named push whose preview fails
/// leaves it set aside. A client that forgets the refusal, or lets go on being asked, meets it on
/// every push and publishes nothing else; one that never asks again keeps a restored branch, or a
/// refused tag whose branch it never had to set aside, off the Hub for good.
#[test]
fn a_branch_the_hub_withdrew_waits_until_it_is_named_again() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str], automatic: bool| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args.iter().map(|arg| {
                // An automatic push is the internal entry background spawners run.
                if automatic && *arg == "push" {
                    "__push-automatic"
                } else {
                    *arg
                }
            }))
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["config", "agit.autoPush", "true"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"], false));
    let turn = |branch: &str, session: char, content: &str| {
        repo.git(&["checkout", "-q", branch]).unwrap();
        let session = meta::Meta::new(
            format!("agit-{}", session.to_string().repeat(40)),
            "claude-code".into(),
            String::new(),
        );
        let log = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &session.session,
        );
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), &session).unwrap();
        repo.add_all().unwrap();
        repo.commit(content).unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    repo.git(&["branch", "kept"]).unwrap();
    repo.git(&["branch", "pulled"]).unwrap();
    let kept = turn("kept", 'b', "kept turn 0");
    let pulled = turn("pulled", 'e', "pulled turn 0");
    assert_success(&run(&["--yes", "push", "alice/app", "--all"], false));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/pulled"]), pulled);

    // The owner withdraws `pulled` on the Hub; both branches gain a turn.
    *hub.withdrawn.lock().unwrap() = Some("refs/heads/pulled".into());
    let kept_next = turn("kept", 'b', "kept turn 1");
    let pulled_next = turn("pulled", 'e', "pulled turn 1");
    let received = hub.posts.load(Ordering::Acquire);
    let refused = run(&["--yes", "push", "alice/app", "--all"], false);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("withdrawn on the Hub, so not published: refs/heads/pulled"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), received);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/pulled"]), pulled);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/kept"]), kept);
    let set_aside = || {
        let status = run(&["status", "--json"], false);
        assert_success(&status);
        let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        status["result"]["value"]["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|repository| repository["repo"] == "alice/app")
            .unwrap()["set_aside"]
            .clone()
    };
    assert_eq!(set_aside(), json!({"branches": ["pulled"], "error": null}));

    let planned = |from: usize| -> Vec<serde_json::Value> {
        hub.previews.lock().unwrap()[from..]
            .iter()
            .flat_map(|preview| preview["planned_refs"].as_array().unwrap().clone())
            .collect()
    };
    let earlier = hub.previews.lock().unwrap().len();
    let automatic = run(&["--yes", "push", "alice/app@pulled"], true);
    assert!(!automatic.status.success(), "{automatic:?}");
    assert_success(&run(&["--yes", "push", "alice/app", "--all"], false));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/kept"]), kept_next);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/pulled"]), pulled);
    assert!(!planned(earlier).contains(&json!("refs/heads/pulled")));

    // Naming it while the Hub answers nothing about it leaves it set aside: the next push still
    // publishes without it rather than meeting the refusal again.
    hub.previews_unavailable.store(true, Ordering::Release);
    assert!(
        !run(&["--yes", "push", "alice/app@pulled"], false)
            .status
            .success()
    );
    hub.previews_unavailable.store(false, Ordering::Release);
    let earlier = hub.previews.lock().unwrap().len();
    let after = turn("kept", 'b', "kept turn after the unanswered push");
    assert_success(&run(&["--yes", "push", "alice/app", "--all"], false));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/kept"]), after);
    assert!(!planned(earlier).contains(&json!("refs/heads/pulled")));

    // Restored on the Hub: naming the branch asks again and publishes it.
    *hub.withdrawn.lock().unwrap() = None;
    let earlier = hub.previews.lock().unwrap().len();
    assert_success(&run(&["--yes", "push", "alice/app@pulled"], false));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/pulled"]),
        pulled_next
    );
    assert!(planned(earlier).contains(&json!("refs/heads/pulled")));
    assert_eq!(set_aside(), json!({"branches": [], "error": null}));

    // Only a version tag is refused: its branch publishes, the tag waits for a named push. The tag
    // shares its branch's receive, so a client that does not send the branch alone after that
    // receive is refused keeps the branch off the Hub with it.
    let tagged = turn("kept", 'b', "kept turn 2");
    let version = format!("refs/tags/agit-{tagged}");
    *hub.withdrawn.lock().unwrap() = Some(version.clone());
    let refused = run(&["--yes", "push", "alice/app@kept"], false);
    assert!(!refused.status.success(), "{refused:?}");
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/kept"]), tagged);
    // A push that names no branch publishes the next turn without the tag. Once main is the one
    // last fetched, local records show the branch unchanged, with the set-aside tag in the listing
    // its publication acknowledged.
    let next = turn("kept", 'b', "kept turn 3");
    assert_success(&run(&["--yes", "push", "alice/app", "--all"], false));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/kept"]), next);
    git(
        repo.root(),
        &["update-ref", "refs/remotes/origin/main", "refs/heads/main"],
    );
    assert_success(&run(&["--yes", "push", "alice/app", "--all"], false));
    let unchanged = run(&["--yes", "push", "alice/app", "--all"], false);
    assert_success(&unchanged);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&unchanged.stdout),
        String::from_utf8_lossy(&unchanged.stderr)
    );
    assert!(
        said.contains("unchanged since their acknowledged publication") && said.contains("kept"),
        "{unchanged:?}"
    );
    *hub.withdrawn.lock().unwrap() = None;
    assert_success(&run(&["--yes", "push", "alice/app@kept"], false));
    assert_eq!(git(&remote, &["rev-parse", &version]), tagged);
}

/// A session branch the Hub refused for a reason its later turns do not change stays out of
/// automatic pushes, and a push the person names asks the Hub again. One refusal is a snapshot
/// holding more event entries than the Hub reads: the branch stays out while its tip still holds
/// more than that. The other is a branch built on a session withdrawn on the Hub: it stays out
/// until a named push is accepted. Each refusal is told in its own words, and as a push that
/// changed nothing on the Hub: the Hub turned it down before moving any ref. The next automatic
/// push leaves the branch out before any preview, rather than uploading the whole session again
/// only to meet the same refusal; once the Hub accepts the branch, automatic pushes carry it again.
/// A refusal the Hub can also send after its ref transaction still reads as unconfirmed. A client
/// that forgets the refusal uploads the session on every turn; one that never lets it go keeps a
/// session the Hub now accepts off it for good; one that reads every named refusal as nothing moved
/// would hide a receive the Hub may have applied.
#[test]
fn a_branch_the_hub_refuses_for_good_stays_out_of_automatic_pushes() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [29; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str], automatic: bool| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args.iter().map(|arg| {
                // An automatic push is the internal entry background spawners run.
                if automatic && *arg == "push" {
                    "__push-automatic"
                } else {
                    *arg
                }
            }))
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["config", "agit.autoPush", "true"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"], false));
    let main = git(&remote, &["rev-parse", "refs/heads/main"]);
    repo.git(&["branch", "big"]).unwrap();
    let session = meta::Meta::new(
        format!("agit-{}", "c".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let mut lines = String::new();
    let mut turn = |content: &str| {
        repo.git(&["checkout", "-q", "big"]).unwrap();
        lines.push_str(&format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":content}})
        ));
        let log = transcript::wrap_lines(&lines, "claude-code", &session.session);
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), &session).unwrap();
        repo.add_all().unwrap();
        repo.commit(content).unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    let text = |output: &std::process::Output| String::from_utf8_lossy(&output.stderr).into_owned();

    // The Hub refuses the receive as it refuses a snapshot past its event-entry limit. Its one
    // event gives the snapshot five entries, over the limit of four the refusal names.
    turn("big turn 0");
    *hub.refuse_moved.lock().unwrap() = vec![("refs/heads/main".into(), main.clone(), None)];
    *hub.refuse_with.lock().unwrap() = Some(
        "ERR HTTP 422 provenance_rejected: refs/heads/publication: session/meta.json unreadable (reserved event topology exceeds the per-push limit of 4 entries)\n",
    );
    let refused = run(&["--yes", "push", "alice/app@big"], true);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        text(&refused).contains(
            "alice/app@big holds more event entries than the Hub accepts (at most 4 in one session snapshot)"
        ),
        "{refused:?}"
    );
    assert!(
        text(&refused).contains("no branch or tag refs were updated"),
        "{refused:?}"
    );
    assert!(!text(&refused).contains("unconfirmed"), "{refused:?}");
    // The Hub's reason, not Git's "failed to push some refs", and no rerun of the same push.
    assert!(
        text(&refused).contains(
            "publication did not complete (HTTP 422 provenance_rejected: refs/heads/publication: session/meta.json unreadable"
        ),
        "{refused:?}"
    );
    assert!(
        !text(&refused).contains("is safe after resolving it"),
        "{refused:?}"
    );
    assert!(hub.refuse_moved.lock().unwrap().is_empty());
    let on_hub = Command::new("git")
        .arg("--git-dir")
        .arg(&remote)
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/big"])
        .output()
        .unwrap();
    assert!(!on_hub.status.success(), "{on_hub:?}");

    // A turn later the branch is larger still: the automatic push asks the Hub nothing.
    turn("big turn 1");
    let previews = hub.previews.lock().unwrap().len();
    let left_out = run(&["--yes", "push", "alice/app@big"], true);
    assert!(!left_out.status.success(), "{left_out:?}");
    assert!(
        text(&left_out).contains("it stays in this checkout and was not pushed"),
        "{left_out:?}"
    );
    assert_eq!(hub.previews.lock().unwrap().len(), previews);

    // The Hub accepts it now. Naming the branch asks again; the automatic push after the next
    // turn carries it.
    let named = turn("big turn 2");
    assert_success(&run(&["--yes", "push", "alice/app@big"], false));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/big"]), named);
    let next = turn("big turn 3");
    assert_success(&run(&["--yes", "push", "alice/app@big"], true));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/big"]), next);

    // A refusal the Hub can also send after its ref transaction leaves the outcome unknown.
    turn("big turn 4");
    *hub.refuse_moved.lock().unwrap() = vec![("refs/heads/big".into(), next.clone(), None)];
    *hub.refuse_with.lock().unwrap() = Some(
        "ERR HTTP 503 service_unavailable: repository commit is awaiting durable mutation reconciliation\n",
    );
    let unknown = run(&["--yes", "push", "alice/app@big"], false);
    assert!(!unknown.status.success(), "{unknown:?}");
    assert!(
        text(&unknown).contains("some ref updates are unconfirmed"),
        "{unknown:?}"
    );

    // Built on a withdrawn session: the Hub refuses the receive, and the branch is set aside as a
    // withdrawn one is until a named push is accepted.
    turn("big turn 5");
    *hub.refuse_moved.lock().unwrap() = vec![("refs/heads/big".into(), next.clone(), None)];
    *hub.refuse_with.lock().unwrap() = Some(
        "ERR HTTP 422 provenance_rejected: this push puts a withdrawn session on a branch; restore the session before publishing it again\n",
    );
    let refused = run(&["--yes", "push", "alice/app@big"], true);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        text(&refused).contains("alice/app@big builds on a session withdrawn on the Hub"),
        "{refused:?}"
    );
    assert!(
        text(&refused).contains("no branch or tag refs were updated"),
        "{refused:?}"
    );
    turn("big turn 6");
    let previews = hub.previews.lock().unwrap().len();
    let left_out = run(&["--yes", "push", "alice/app@big"], true);
    assert!(!left_out.status.success(), "{left_out:?}");
    assert_eq!(hub.previews.lock().unwrap().len(), previews);
    let restored = turn("big turn 7");
    assert_success(&run(&["--yes", "push", "alice/app@big"], false));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/big"]), restored);
    let after = turn("big turn 8");
    assert_success(&run(&["--yes", "push", "alice/app@big"], true));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/big"]), after);
}

/// The end of a session agit launched says, in one line, what became of the turns that launch
/// saved: uploaded, failed and why, or not uploaded and why, and nothing when it saved none. The
/// turns are counted from the launch rather than from the start of the conversation, and a failure
/// is told in its fixed class, never in the Hub's words. The upload runs in the settlement of each
/// turn, a process with no terminal of its own, so an end line that guessed from the settings
/// would call a failed upload uploading, one that counted the branch's turns would claim the turns
/// of earlier launches, one that read only the branch would call a turn whose settlement still
/// waits for its reply never saved, and one that trusted a leftover marker would keep saying a turn
/// is being saved after its settlement died.
#[cfg(unix)]
#[test]
fn the_end_of_a_launched_session_says_what_became_of_its_turns() {
    use std::os::unix::fs::PermissionsExt as _;
    const SID: &str = "dddddddd-0000-4000-8000-000000000004";

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [31; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let work = temp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let work = work.canonicalize().unwrap();
    let projects = temp
        .path()
        .join(".claude/projects")
        .join(agit::domain::store::slug_for(&work));
    fs::create_dir_all(&projects).unwrap();
    let transcript = projects.join(format!("{SID}.jsonl"));
    let turn = |n: usize| {
        let cwd = work.to_string_lossy();
        format!(
            "{}\n{}\n",
            json!({"type":"user","sessionId":SID,"cwd":cwd,"uuid":format!("u{n}"),
                "timestamp":format!("2026-11-02T00:00:{n:02}.000Z"),
                "message":{"role":"user","content":format!("synthetic prompt {n}")}}),
            json!({"type":"assistant","sessionId":SID,"cwd":cwd,"uuid":format!("a{n}"),
                "timestamp":format!("2026-11-02T00:00:{n:02}.500Z"),
                "message":{"role":"assistant","stop_reason":"end_turn",
                    "content":[{"type":"text","text":format!("synthetic answer {n}")}]}}),
        )
    };
    // The runtime agit launches adds the turn it is handed and settles it as Claude Code's Stop
    // hook does, then exits.
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let next = temp.path().join("next-turn");
    fs::write(
        bin.join("claude"),
        "#!/bin/sh\nif [ -s \"$AGIT_TEST_NEXT\" ]; then\n  cat \"$AGIT_TEST_NEXT\" >> \"$AGIT_TEST_TRANSCRIPT\"\n  : > \"$AGIT_TEST_NEXT\"\n  printf '%s' \"$AGIT_TEST_STOP\" | \"$AGIT_TEST_BIN\" hooks settle --runtime claude-code\nfi\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = vec![bin.clone()];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path).unwrap();
    let stop =
        json!({"session_id":SID,"cwd":work,"transcript_path":transcript,"hook_event_name":"Stop"});
    let agit = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        let output = command
            .current_dir(&work)
            .env("PATH", &path)
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_YES", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env("AGIT_SECRETS_KEYSTORE", "file")
            .env("AGIT_TEST_NEXT", &next)
            .env("AGIT_TEST_TRANSCRIPT", &transcript)
            .env("AGIT_TEST_STOP", stop.to_string())
            .env("AGIT_TEST_BIN", env!("CARGO_BIN_EXE_agit"))
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap();
        assert_success(&output);
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["config", "agit.autoPush", "true"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    agit(&["push", "alice/app@main"]);
    let main = git(&remote, &["rev-parse", "refs/heads/main"]);
    fs::write(&transcript, format!("{}{}", turn(1), turn(2))).unwrap();
    agit(&[
        "import",
        SID,
        "--from",
        "claude-code",
        "--into",
        "alice/app@work",
        "--independent",
    ]);
    // Each launch adds what it is handed, at most one turn; its end line is the one line agit
    // writes after it.
    let launch = |added: String| {
        fs::write(&next, added).unwrap();
        let output = agit(&["resume", "alice/app@work"]);
        let lines: Vec<_> = output
            .lines()
            .filter(|line| line.starts_with("agit: "))
            .map(str::to_owned)
            .collect();
        assert!(lines.len() <= 1, "{output}");
        lines.into_iter().next()
    };

    assert_eq!(
        launch(turn(3)).as_deref(),
        Some("agit: saved 1 turn of this conversation and uploaded it to alice/app.")
    );
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/work"]),
        git(repo.root(), &["rev-parse", "refs/heads/work"])
    );
    assert_eq!(launch(String::new()), None);

    // The Hub refuses the upload as one past its event-entry limit.
    *hub.refuse_moved.lock().unwrap() = vec![("refs/heads/main".into(), main, None)];
    *hub.refuse_with.lock().unwrap() = Some(
        "ERR HTTP 422 provenance_rejected: refs/heads/publication: session/meta.json unreadable (reserved event topology exceeds the per-push limit of 4 entries)\n",
    );
    assert_eq!(
        launch(turn(4)).as_deref(),
        Some(
            "agit: saved 1 turn of this conversation on this computer; the upload to alice/app failed: the session is larger than the Hub accepts, so later turns of it stay on this computer; pushing it yourself asks the Hub again."
        )
    );
    let previews = hub.previews.lock().unwrap().len();
    assert_eq!(
        launch(turn(5)).as_deref(),
        Some(
            "agit: saved 1 turn of this conversation on this computer; not uploaded: the session is larger than the Hub accepts, so later turns of it stay on this computer; pushing it yourself asks the Hub again."
        )
    );
    assert_eq!(hub.previews.lock().unwrap().len(), previews);

    agit(&["config", "--repo", "alice/app", "push.auto", "false"]);
    assert_eq!(
        launch(turn(6)).as_deref(),
        Some(
            "agit: saved 1 turn of this conversation on this computer; not uploaded: automatic upload is off for this repository."
        )
    );

    // The last turn's reply is written after its Stop, so its settlement waits for the reply apart
    // from the runtime, which ends first. Until the turn is on its branch, the end says it is
    // still being saved.
    let last = turn(7);
    let (prompt, reply) = last.split_once('\n').unwrap();
    let before = git(repo.root(), &["rev-parse", "refs/heads/work"]);
    assert_eq!(
        launch(format!("{prompt}\n")).as_deref(),
        Some("agit: the last turn of this conversation is still being saved.")
    );
    let settling: Vec<_> = fs::read_dir(home.join("projects/uploads/settling"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(settling.len(), 1, "{settling:?}");
    let marker = &settling[0];
    fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(reply.as_bytes())
        .unwrap();
    // The settlement removes its marker once the turn is on its branch; it runs apart from this
    // process, so the wait for it is bounded.
    let deadline = Instant::now() + Duration::from_secs(60);
    while marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!marker.exists(), "the settlement kept its marker");
    assert_ne!(git(repo.root(), &["rev-parse", "refs/heads/work"]), before);

    // A settlement that ended without removing its marker, as one that crashed does, leaves the
    // end saying nothing about a turn being saved, and its marker goes.
    let mut ended = Command::new("true").spawn().unwrap();
    let pid = ended.id();
    ended.wait().unwrap();
    fs::write(marker, json!({ "pid": pid }).to_string()).unwrap();
    assert_eq!(launch(String::new()), None);
    assert!(!marker.exists());
}

/// A conversation from a folder that doesn't upload by itself is not uploaded by an automatic
/// push, whichever path recorded its branch and whatever the checkout's own setting says; the
/// person's own push still uploads it. A client that judged only the checkout's setting would
/// upload what the folder keeps back.
/// An off the person sets for a destination before its checkout exists on this computer holds
/// once the checkout is created, whatever spelling named it: a folder bound to upload into it
/// leaves automatic upload off, and an automatic push of a saved conversation sends nothing.
/// Turning it on without a checkout says which step creates one, and a record that can't be read
/// keeps automatic upload off for every destination without its own setting. An implementation
/// that refused the off for want of a checkout, let a later checkout inherit around it, matched
/// only the exact spelling, or read a damaged record as "no record" would upload here.
#[test]
fn an_off_set_before_the_checkout_exists_holds_after_it_is_created() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [37; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    // A stand-in `claude`, so binding a folder for automatic upload finds a runtime to capture.
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("claude"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::write(bin.join("claude.cmd"), "@exit /b 0\r\n").unwrap();
    let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("PATH", &path)
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let setting = |repository: &str| {
        let out = run(&["--json", "config", "--repo", repository, "push.auto"]);
        assert_success(&out);
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        value["result"]["value"]["setting"].clone()
    };
    // The destination exists on the Hub; this computer has no checkout of it.
    let checkout = home.join("repos/alice/app");
    let repo = Repo::init(&checkout).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    fs::remove_dir_all(&checkout).unwrap();
    assert_success(&run(&["config", "push.auto", "true"]));

    let on = run(&["config", "--repo", "alice/app", "push.auto", "true"]);
    assert_eq!(on.status.code(), Some(4), "{on:?}");
    assert!(
        String::from_utf8_lossy(&on.stderr).contains("`agit clone alice/app`"),
        "{on:?}"
    );
    assert_success(&run(&[
        "config",
        "--repo",
        "alice/App",
        "push.auto",
        "false",
    ]));
    assert_eq!(setting("alice/app")["effective"], "false");
    assert!(!checkout.exists());

    // A folder bound to upload into it creates the checkout and leaves the person's off in place.
    let folder = temp.path().join("folder");
    fs::create_dir(&folder).unwrap();
    let folder = folder.canonicalize().unwrap();
    let bound = run(&[
        "--yes",
        "project",
        "bind",
        folder.to_str().unwrap(),
        "--repo",
        "alice/app",
        "--history",
        "none",
        "--auto-upload",
    ]);
    assert_success(&bound);
    assert!(
        String::from_utf8_lossy(&bound.stdout)
            .contains("automatic upload into alice/app stays off"),
        "{bound:?}"
    );
    let repo = Repo::open(&checkout).expect("the bind created the checkout");
    assert_eq!(repo.auto_push_override().unwrap(), None);
    assert_eq!(setting("alice/app")["effective"], "false");

    // A conversation saved there is not uploaded automatically.
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["checkout", "-q", "-b", "kept"]).unwrap();
    let mut session = meta::Meta::new(
        format!("agit-{}", "e".repeat(40)),
        "claude-code".into(),
        folder.to_string_lossy().into_owned(),
    );
    session.runtime_instances = vec!["019a0000-0000-7000-8000-00000000e0e0".into()];
    let log = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"stays here"}})
        ),
        "claude-code",
        &session.session,
    );
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &session).unwrap();
    repo.add_all().unwrap();
    repo.commit("stays here").unwrap();
    let posts = hub.posts.load(Ordering::Acquire);
    let refused = || {
        let automatic = run(&["__push-automatic", "alice/app@kept"]);
        assert!(!automatic.status.success(), "{automatic:?}");
        assert!(
            String::from_utf8_lossy(&automatic.stderr)
                .contains("automatic publication is disabled for this repository"),
            "{automatic:?}"
        );
        assert_eq!(hub.posts.load(Ordering::Acquire), posts);
        assert!(
            git(&remote, &["show-ref"])
                .lines()
                .all(|line| !line.ends_with("refs/heads/kept"))
        );
    };
    refused();

    // A record that can't be read keeps automatic upload off where nothing else decides it.
    fs::write(
        home.join("preferences/automatic-upload-off.json"),
        "not a record",
    )
    .unwrap();
    refused();

    // Once the checkout exists, another spelling reaches its own setting rather than a record
    // beside it, which its setting would outrank.
    fs::remove_file(home.join("preferences/automatic-upload-off.json")).unwrap();
    assert_success(&run(&[
        "config",
        "--repo",
        "alice/app",
        "push.auto",
        "true",
    ]));
    assert_success(&run(&[
        "config",
        "--repo",
        "alice/APP",
        "push.auto",
        "false",
    ]));
    assert_eq!(repo.auto_push_override().unwrap(), Some(false));
    assert_eq!(setting("alice/app")["effective"], "false");
}

#[test]
fn an_automatic_push_leaves_out_a_conversation_its_folder_keeps_back() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [31; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run_env = |args: &[&str], automatic: bool, vars: &[(&str, &str)]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .envs(vars.iter().copied())
            .stdin(Stdio::null())
            .args(args.iter().map(|arg| {
                // An automatic push is the internal entry background spawners run.
                if automatic && *arg == "push" {
                    "__push-automatic"
                } else {
                    *arg
                }
            }))
            .output()
            .unwrap()
    };
    let run = |args: &[&str], automatic: bool| run_env(args, automatic, &[]);
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"], false));

    // The folder is bound without automatic upload; the checkout's own setting is on.
    let folder = temp.path().join("folder");
    fs::create_dir(&folder).unwrap();
    let folder = folder.canonicalize().unwrap();
    assert_success(&run(
        &[
            "--yes",
            "project",
            "bind",
            folder.to_str().unwrap(),
            "--repo",
            "alice/app",
            "--history",
            "none",
        ],
        false,
    ));
    repo.git(&["config", "agit.autoPush", "true"]).unwrap();
    repo.git(&["checkout", "-q", "-b", "kept"]).unwrap();
    let mut session = meta::Meta::new(
        format!("agit-{}", "d".repeat(40)),
        "codex".into(),
        folder.to_string_lossy().into_owned(),
    );
    session.runtime_instances = vec!["019a0000-0000-7000-8000-00000000d0d0".into()];
    let log = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"kept back"}})
        ),
        "claude-code",
        &session.session,
    );
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &session).unwrap();
    repo.add_all().unwrap();
    repo.commit("kept back").unwrap();

    // The internal entry, run by hand, is an automatic push: held to the folder, and nothing more.
    let posts = hub.posts.load(Ordering::Acquire);
    let automatic = run(&["__push-automatic", "alice/app@kept"], false);
    assert!(!automatic.status.success(), "{automatic:?}");
    assert!(
        String::from_utf8_lossy(&automatic.stderr).contains(
            "`alice/app@kept` is not uploaded automatically: the folder it ran in left it out, or doesn't upload by itself"
        ),
        "{automatic:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), posts);
    assert!(
        git(&remote, &["show-ref"])
            .lines()
            .all(|line| !line.ends_with("refs/heads/kept"))
    );

    // The variable background children carry doesn't make a push automatic: the person's push of
    // the branch goes through.
    assert_success(&run_env(
        &["--yes", "push", "alice/app@kept"],
        false,
        &[("AGIT_AUTO_PUSH", "1")],
    ));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/kept"]),
        git(repo.root(), &["rev-parse", "kept"])
    );

    // A session the person dropped from the folder goes only where a push names its branch.
    const DROPPED: &str = "019a0000-0000-7000-8000-00000000d0d1";
    repo.git(&["checkout", "-q", "-b", "dropped", "main"])
        .unwrap();
    session.session = format!("agit-{}", "f".repeat(40));
    session.runtime_instances = vec![DROPPED.into()];
    let log = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"dropped"}})
        ),
        "claude-code",
        &session.session,
    );
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &session).unwrap();
    repo.add_all().unwrap();
    repo.commit("dropped").unwrap();
    for entry in fs::read_dir(home.join("projects")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let mut policy: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            policy["auto_upload"] = json!(true);
            policy["excluded"]
                .as_array_mut()
                .unwrap()
                .push(json!(format!("codex:{DROPPED}")));
            fs::write(&path, serde_json::to_vec(&policy).unwrap()).unwrap();
        }
    }
    let unnamed = run(&["--yes", "push", "alice/app", "--all"], false);
    assert_success(&unnamed);
    assert!(
        String::from_utf8_lossy(&unnamed.stdout)
            .contains("skipping dropped (its folder left it out)"),
        "{unnamed:?}"
    );
    assert!(
        git(&remote, &["show-ref"])
            .lines()
            .all(|line| !line.ends_with("refs/heads/dropped"))
    );
    assert_success(&run(&["--yes", "push", "alice/app@dropped"], false));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/dropped"]),
        git(repo.root(), &["rev-parse", "dropped"])
    );
}

/// Automatic upload into a public repository is switched on by an explicit answer from anyone: a
/// terminal, the menu bar's consent naming that repository, or `--yes` from any session, an
/// agent's included. Each row names an entry point, the destination's audience, whether `--yes`
/// was given and whether a terminal is attached. A switch-on nobody answered is refused and leaves
/// automatic upload off; user-wide or destination-less switches stay allowed, but an automatic push
/// into a public repository that nobody switched on is refused before upload. A switch-on answered
/// with no person present is recorded and listed for the menu bar to tell the person; one a person
/// answered is not. A gate that refused an agent's `--yes` would block what the person allowed; a
/// gate only at the entry points would let an inherited user preference publish publicly. The dry
/// run the menu bar asks first names the audience, so its question can say the repository is public. Choosing "On", or inheriting an enabled preference, in a first checkout's
/// menu at a terminal records the same enrollment as `--auto-push=true` there; a menu that only
/// stored the switch would leave every later automatic upload refused.
#[cfg(unix)]
#[test]
fn automatic_upload_into_a_public_repository_is_switched_on_by_an_explicit_answer() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [28; 32]);
    let sign_in = |home: &Path| {
        startup_cache::seed(home);
        save_at(
            &home
                .join("credentials")
                .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
            &HubCredential {
                account_id: Some("account-1".into()),
                username: "alice".into(),
                email: None,
                hub: Some(hub.base.clone()),
                access_token: "synthetic-access".into(),
                refresh_token: "synthetic-refresh".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    };
    sign_in(&home);
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    // A folder's automatic upload needs a runtime whose hooks capture its turns; a stand-in
    // `claude` first on the search path keeps the rows from depending on what the machine running
    // them has installed.
    let runtimes = temp.path().join("runtimes");
    fs::create_dir_all(&runtimes).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let claude = runtimes.join("claude");
        fs::write(&claude, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let search_path = std::env::join_paths(std::iter::once(runtimes.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let run_in = |home: &Path, args: &[&str], automatic: bool| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .env("PATH", &search_path)
            .current_dir(temp.path())
            .env("AGIT_HOME", home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args.iter().map(|arg| {
                // An automatic push is the internal entry background spawners run.
                if automatic && *arg == "push" {
                    "__push-automatic"
                } else {
                    *arg
                }
            }));
        command.output().unwrap()
    };
    let run = |args: &[&str], automatic: bool| run_in(&home, args, automatic);
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(
        &["new", "alice/app", "-b", "work", "--no-launch"],
        false,
    ));
    assert_success(&run(&["--yes", "push", "alice/app@main"], false));
    let work = home.join("worktrees/alice/app/work");
    let enrollment = repo.root().join(".git/agit/privacy-auto-consent.json");
    let unattended = home.join("notices/public-switch-on.json");
    // A terminal with no agent session: the person typing the command. When `answer` is given,
    // it is typed once the automatic-upload menu is drawn.
    let run_terminal = |home: &Path, args: &[&str], answer: Option<&str>| {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
        let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
        builder.env_clear_local();
        for (name, value) in git_command(temp.path()).get_envs() {
            if let Some(value) = value {
                builder.env(name, value);
            }
        }
        builder.env("AGIT_HOME", home);
        builder.env("CODEX_HOME", home.join("codex"));
        builder.env("CLAUDE_CONFIG_DIR", home.join("claude"));
        builder.env("AGIT_HUB_URL", &hub.base);
        builder.env("AGIT_TUI", "0");
        builder.env("AGIT_USE_SYSTEM_GIT", "1");
        builder.env("AGIT_SECRETS_KEYSTORE", "file");
        builder.env("TERM", "xterm");
        builder.cwd(temp.path());
        builder.args(args);
        let mut child = pair.slave.spawn_command(builder).unwrap();
        drop(pair.slave);
        let mut keyboard = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (sender, chunks) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut answer = answer;
        let mut screen = String::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            while let Ok(chunk) = chunks.try_recv() {
                screen.push_str(&String::from_utf8_lossy(&chunk));
            }
            if let Some(keys) = answer
                && screen.contains("Automatically push settled turns")
            {
                keyboard.write_all(keys.as_bytes()).unwrap();
                keyboard.flush().unwrap();
                answer = None;
            }
            if let Some(status) = child.try_wait().unwrap() {
                assert!(answer.is_none(), "agit {args:?} never asked: {screen}");
                break status.success();
            }
            assert!(
                Instant::now() < deadline,
                "agit {args:?} waited at a terminal: {screen}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    };
    let effective = |home: &Path| {
        let out = run_in(
            home,
            &["--json", "config", "--repo", "alice/app", "push.auto"],
            false,
        );
        assert_success(&out);
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        value["result"]["value"]["setting"]["effective"] == "true"
    };
    let project_dir = project.to_str().unwrap();
    let repo_switch: &[&str] = &["config", "--repo", "alice/app", "push.auto", "true"];
    let clone_switch: &[&str] = &["clone", "alice/app", "--auto-push=true"];
    let bind_switch: &[&str] = &[
        "project",
        "bind",
        project_dir,
        "--repo",
        "alice/app",
        "--history",
        "none",
        "--auto-upload",
    ];
    let repo_consent: &[&str] = &[
        "config",
        "--repo",
        "alice/app",
        "push.auto",
        "true",
        "--public-consent",
        "alice/app",
    ];
    let repo_consent_other: &[&str] = &[
        "config",
        "--repo",
        "alice/app",
        "push.auto",
        "true",
        "--public-consent",
        "alice/other",
    ];
    let bind_consent: &[&str] = &[
        "project",
        "bind",
        project_dir,
        "--repo",
        "alice/app",
        "--history",
        "none",
        "--auto-upload",
        "--public-consent",
        "alice/app",
    ];
    let user_switch: &[&str] = &["config", "push.auto", "true"];
    let init_switch: &[&str] = &["init", "fresh", "--no-bind", "--auto-push=true"];
    let menu_clone: &[&str] = &["clone", "alice/app"];
    const ON: &str = "\x1b[B\r";
    const INHERIT: &str = "\r";
    const ESCAPE: &str = "\x1b";
    const QUIT: &str = "q";
    struct Row<'a> {
        entry: &'a [&'a str],
        audience: &'static str,
        yes: bool,
        terminal: bool,
        /// Keys typed at the automatic-upload menu of a first checkout in a fresh home.
        menu: Option<&'static str>,
        /// The user preference the menu offers to inherit is on.
        inherits_on: bool,
        /// The menu records the person's enrollment of the public destination.
        enrolls: bool,
        switched_on: bool,
        /// Whether an automatic push from the main checkout then publishes.
        publishes: Option<bool>,
    }
    let row = |entry, audience, yes, terminal, switched_on, publishes| Row {
        entry,
        audience,
        yes,
        terminal,
        menu: None,
        inherits_on: false,
        enrolls: false,
        switched_on,
        publishes,
    };
    let menu = |keys, audience, inherits_on, enrolls, publishes| Row {
        entry: menu_clone,
        audience,
        yes: false,
        terminal: true,
        menu: Some(keys),
        inherits_on,
        enrolls,
        switched_on: true,
        publishes,
    };
    let rows = [
        row(repo_switch, "public", false, false, false, None),
        row(repo_switch, "public", true, false, true, Some(true)),
        row(repo_switch, "public", false, true, true, Some(true)),
        row(repo_switch, "private", false, false, true, Some(true)),
        row(clone_switch, "public", false, false, false, None),
        row(clone_switch, "public", true, false, true, None),
        row(clone_switch, "private", false, false, true, None),
        row(bind_switch, "public", true, false, true, None),
        // The menu bar's click, outside any agent session, consents for the repository it names
        // and enrolls it as a terminal answer does; consent naming another repository answers
        // nothing.
        row(repo_consent, "public", false, false, true, Some(true)),
        row(repo_consent_other, "public", false, false, false, None),
        // The menu bar passes --yes for the bind's own question; the consent answers the public one.
        row(bind_consent, "public", true, false, true, None),
        row(user_switch, "public", true, false, true, Some(false)),
        row(init_switch, "public", false, false, true, None),
        menu(ON, "public", false, true, Some(true)),
        menu(INHERIT, "public", true, true, Some(true)),
        menu(ON, "private", false, false, None),
        // A cancelled menu is not an answer: the inherited preference stays the user's, and
        // nothing enrolls the public destination, so the next automatic upload is refused.
        menu(ESCAPE, "public", true, false, Some(false)),
        menu(QUIT, "public", true, false, Some(false)),
    ];
    for (index, row) in rows.into_iter().enumerate() {
        let Row {
            entry,
            audience,
            yes,
            terminal,
            menu,
            inherits_on,
            enrolls,
            switched_on,
            publishes,
        } = row;
        let case = format!(
            "{entry:?} {audience} yes={yes} terminal={terminal} menu={menu:?} inherits_on={inherits_on}"
        );
        repo.git(&["config", "--local", "--unset-all", "agit.autoPush"])
            .ok();
        assert_success(&run(&["config", "--unset", "push.auto"], false));
        let _ = fs::remove_file(&enrollment);
        let _ = fs::remove_file(&unattended);
        *hub.visibility.lock().unwrap() = audience;

        if let Some(keys) = menu {
            // A first checkout is where the menu is asked; the main checkout already exists.
            let fresh = temp.path().join(format!("fresh-{index}"));
            sign_in(&fresh);
            if inherits_on {
                assert_success(&run_in(&fresh, &["config", "push.auto", "true"], false));
            }
            assert!(run_terminal(&fresh, entry, Some(keys)), "{case}");
            assert_eq!(effective(&fresh), switched_on, "{case}");
            assert_eq!(
                fresh
                    .join("repos/alice/app/.git/agit/privacy-auto-consent.json")
                    .exists(),
                enrolls,
                "{case}"
            );
            if let Some(publishes) = publishes {
                let branch = format!("menu-{index}");
                git(
                    &fresh.join("repos/alice/app"),
                    &["config", "http.userAgent", TRANSPORT_USER_AGENT],
                );
                assert_success(&run_in(
                    &fresh,
                    &["new", "alice/app", "-b", &branch, "--no-launch"],
                    false,
                ));
                let work = fresh.join("worktrees/alice/app").join(&branch);
                fs::write(work.join("README.md"), format!("Revision {index}")).unwrap();
                git(
                    &work,
                    &[
                        "-c",
                        "user.name=Source Writer",
                        "-c",
                        "user.email=source@example.invalid",
                        "commit",
                        "-am",
                        "Revise file",
                    ],
                );
                let local = git(&work, &["rev-parse", "HEAD"]);
                hub.posts.store(0, Ordering::Release);
                let pushed = run_in(
                    &fresh,
                    &["--json", "push", &format!("alice/app@{branch}")],
                    true,
                );
                assert_eq!(pushed.status.success(), publishes, "{case}: {pushed:?}");
                let published = git(&remote, &["for-each-ref", &format!("refs/heads/{branch}")]);
                if publishes {
                    assert_eq!(
                        git(&remote, &["rev-parse", &format!("refs/heads/{branch}")]),
                        local
                    );
                } else {
                    assert!(
                        format!(
                            "{}{}",
                            String::from_utf8_lossy(&pushed.stdout),
                            String::from_utf8_lossy(&pushed.stderr)
                        )
                        .contains("is not switched on;"),
                        "{case}: {pushed:?}"
                    );
                    assert_eq!(hub.posts.load(Ordering::Acquire), 0, "{case}");
                    assert_eq!(published, "", "{case}");
                }
            }
            continue;
        }

        let mut args = Vec::new();
        if yes {
            args.push("--yes");
        }
        args.extend_from_slice(entry);
        if terminal {
            assert_eq!(run_terminal(&home, &args, None), switched_on, "{case}");
        } else {
            let out = run(&args, false);
            assert_eq!(out.status.success(), switched_on, "{case}: {out:?}");
            if !switched_on {
                assert!(
                    String::from_utf8_lossy(&out.stderr).contains("needs a yes: run"),
                    "{case}: {out:?}"
                );
            }
        }
        // Only a public switch-on that enrolls a destination, answered with nobody at a terminal
        // or in the menu bar, is told; a user-wide or destination-less switch enrolls none.
        let enrolls = entry != user_switch && entry != init_switch;
        let told = switched_on
            && enrolls
            && audience == "public"
            && !terminal
            && !entry.contains(&"--public-consent");
        assert_eq!(
            fs::read_to_string(&unattended).is_ok_and(|text| text.contains("alice/app")),
            told,
            "{case}"
        );
        if entry[0] != "init" {
            assert_eq!(effective(&home), switched_on, "{case}");
        }

        if let Some(publishes) = publishes {
            fs::write(work.join("README.md"), format!("Revision {index}")).unwrap();
            git(&work, &["commit", "-am", "Revise file"]);
            let local = git(&work, &["rev-parse", "HEAD"]);
            let before = git(&remote, &["for-each-ref", "refs/heads/work"]);
            hub.posts.store(0, Ordering::Release);
            let pushed = run(&["--json", "push", "alice/app@work"], true);
            assert_eq!(pushed.status.success(), publishes, "{case}: {pushed:?}");
            if publishes {
                assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), local);
            } else {
                assert!(
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&pushed.stdout),
                        String::from_utf8_lossy(&pushed.stderr)
                    )
                    .contains("is not switched on;"),
                    "{case}: {pushed:?}"
                );
                assert_eq!(hub.posts.load(Ordering::Acquire), 0, "{case}");
                assert_eq!(git(&remote, &["for-each-ref", "refs/heads/work"]), before);
            }
        }
    }

    // An agent may switch a public upload on itself, with the menu bar's flag or `--yes`: it is
    // switched on and enrolled, and listed as unattended for the menu bar to tell the person.
    repo.git(&["config", "--local", "--unset-all", "agit.autoPush"])
        .ok();
    let _ = fs::remove_file(&enrollment);
    *hub.visibility.lock().unwrap() = "public";
    let mut inside = Command::new(env!("CARGO_BIN_EXE_agit"));
    inside.env_clear_local();
    for (name, value) in git_command(temp.path()).get_envs() {
        if let Some(value) = value {
            inside.env(name, value);
        }
    }
    let inside = inside
        .current_dir(temp.path())
        .env("AGIT_HOME", &home)
        .env("CODEX_HOME", home.join("codex"))
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .env("AGIT_HUB_URL", &hub.base)
        .env("CI", "1")
        .env("AGIT_TUI", "0")
        .env("AGIT_USE_SYSTEM_GIT", "1")
        .env("AGIT_SECRETS_KEYSTORE", "file")
        .env("AGIT_SESSION", "alice/app@work")
        .stdin(Stdio::null())
        .args(repo_consent)
        .output()
        .unwrap();
    assert_success(&inside);
    assert!(effective(&home));
    assert!(enrollment.exists());
    let listed = run(&["--json", "project", "list"], false);
    assert_success(&listed);
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(
        listed["result"]["value"]["public_switched_on_unattended"][0]["repository"], "alice/app",
        "{listed}"
    );

    // After the menu bar's own question, which for a repository it did not know to be public was
    // not the public one, its `--yes` does not switch a public upload on: agit asks for the public
    // consent, so the person sees the public question before anything uploads.
    repo.git(&["config", "--local", "--unset-all", "agit.autoPush"])
        .ok();
    let _ = fs::remove_file(&enrollment);
    let mut asked = Command::new(env!("CARGO_BIN_EXE_agit"));
    asked.env_clear_local();
    for (name, value) in git_command(temp.path()).get_envs() {
        if let Some(value) = value {
            asked.env(name, value);
        }
    }
    let asked = asked
        .current_dir(temp.path())
        .env("AGIT_HOME", &home)
        .env("CODEX_HOME", home.join("codex"))
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .env("AGIT_HUB_URL", &hub.base)
        .env("CI", "1")
        .env("AGIT_TUI", "0")
        .env("AGIT_USE_SYSTEM_GIT", "1")
        .env("AGIT_SECRETS_KEYSTORE", "file")
        .env("AGIT_MENUBAR_ASKED", "1")
        .stdin(Stdio::null())
        .arg("--yes")
        .args(repo_switch)
        .output()
        .unwrap();
    assert_eq!(asked.status.code(), Some(8), "{asked:?}");
    assert!(!effective(&home));
    assert!(!enrollment.exists());

    let mut probe = vec!["--json"];
    probe.extend_from_slice(bind_switch);
    probe.push("--dry-run");
    let probe = run(&probe, false);
    assert_success(&probe);
    let probe: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(
        probe["result"]["value"]["repository_visibility"], "public",
        "{probe}"
    );
}

/// A session branch that moved on the Hub while this checkout settled its own turn is refused
/// with the divergence and the commands that resolve it, piped and at a terminal. At a terminal
/// the stopped stage ends its own row: a progress display that leaves its last row open puts the
/// error on the stage's row, where a reader of that row sees only that the stage stopped.
#[test]
fn diverged_session_branch_names_the_divergence_and_next_commands() {
    let temp = tempfile::tempdir().unwrap();
    let desktop = temp.path().join("desktop");
    let laptop = temp.path().join("laptop");
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [29; 32]);
    let sign_in = |home: &Path| {
        startup_cache::seed(home);
        save_at(
            &home
                .join("credentials")
                .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
            &HubCredential {
                account_id: Some("account-1".into()),
                username: "alice".into(),
                email: None,
                hub: Some(hub.base.clone()),
                access_token: "synthetic-access".into(),
                refresh_token: "synthetic-refresh".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    };
    sign_in(&desktop);
    sign_in(&laptop);
    let run_in = |home: &Path, args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in git_command(temp.path()).get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&desktop.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run_in(
        &desktop,
        &["new", "alice/app", "-b", "work", "--no-launch"],
    ));
    assert_success(&run_in(&desktop, &["--yes", "push", "alice/app@main"]));
    assert_success(&run_in(&desktop, &["--yes", "push", "alice/app@work"]));
    assert_success(&run_in(&laptop, &["clone", "alice/app", "--no-bind"]));
    let laptop_repo = laptop.join("repos/alice/app");
    git(
        &laptop_repo,
        &["config", "http.userAgent", TRANSPORT_USER_AGENT],
    );
    let laptop_work = temp.path().join("laptop-work");
    git(
        &laptop_repo,
        &["worktree", "add", laptop_work.to_str().unwrap(), "work"],
    );

    let desktop_work = desktop.join("worktrees/alice/app/work");
    fs::write(desktop_work.join("README.md"), "Desktop turn").unwrap();
    git(&desktop_work, &["commit", "-am", "Desktop turn"]);
    assert_success(&run_in(&desktop, &["push", "alice/app@work"]));
    let hub_tip = git(&remote, &["rev-parse", "refs/heads/work"]);
    git(&laptop_work, &["config", "user.name", "Source Writer"]);
    git(
        &laptop_work,
        &["config", "user.email", "source@example.invalid"],
    );
    fs::write(laptop_work.join("NOTES.md"), "Laptop turn").unwrap();
    git(&laptop_work, &["add", "NOTES.md"]);
    git(
        &laptop_work,
        &["-c", "commit.gpgsign=false", "commit", "-m", "Laptop turn"],
    );
    let explains = |output: &str| {
        for expected in [
            "error remote branch work has diverged from this checkout",
            "local unpublished commits are kept",
            "`agit fork alice/app@work -b work-local`, then `agit push alice/app@work-local`",
            &format!(
                "`agit fetch alice/app`, then `agit merge alice/app@{} --into alice/app@work`",
                &hub_tip[..12]
            ),
        ] {
            assert!(output.contains(expected), "{expected}: {output}");
        }
    };
    let refused = run_in(&laptop, &["push", "alice/app@work"]);
    assert!(!refused.status.success(), "{refused:?}");
    explains(&String::from_utf8_lossy(&refused.stderr));

    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    let pair = native_pty_system().openpty(PtySize::default()).unwrap();
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
    builder.env_clear_local();
    for (name, value) in git_command(temp.path()).get_envs() {
        if let Some(value) = value {
            builder.env(name, value);
        }
    }
    builder.env("AGIT_HOME", &laptop);
    builder.env("CODEX_HOME", laptop.join("codex"));
    builder.env("CLAUDE_CONFIG_DIR", laptop.join("claude"));
    builder.env("AGIT_HUB_URL", &hub.base);
    builder.env("AGIT_TUI", "0");
    builder.env("AGIT_USE_SYSTEM_GIT", "1");
    builder.env("AGIT_SECRETS_KEYSTORE", "file");
    builder.env("TERM", "xterm");
    builder.cwd(temp.path());
    builder.args(["push", "alice/app@work"]);
    let mut child = pair.slave.spawn_command(builder).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (sender, chunks) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut screen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        while let Ok(chunk) = chunks.try_recv() {
            screen.extend(chunk);
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "push waited at a terminal");
        thread::sleep(Duration::from_millis(20));
    };
    thread::sleep(Duration::from_millis(200));
    while let Ok(chunk) = chunks.try_recv() {
        screen.extend(chunk);
    }
    assert!(!status.success());
    let screen = String::from_utf8_lossy(&screen)
        .replace("\u{1b}[2K", "")
        .replace("\u{1b}[31m", "")
        .replace("\u{1b}[39m", "");
    explains(&screen);
    let stopped = screen
        .lines()
        .find(|row| row.contains("Reading remote history and session modes — stopped"))
        .unwrap_or_else(|| panic!("{screen}"));
    assert!(!stopped.contains("error"), "{screen}");
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), hub_tip);
}

/// An automatic upload the Hub refuses because the conversation also continued on another
/// computer is told, and stays told: its record names the class, the next turn's end says it in
/// the class's own words, and `agit status` lists the branch with the command that shows what to
/// do, in text and in JSON, whatever page of sessions it shows; `project list --json`, which the
/// menu bar polls, carries the same list. Another conversation's successful upload in the same
/// folder hides none of it, and is not told the failure. The refusal repeats on every turn, so an
/// implementation that classed it as one the next turn gets past, read one last result for the
/// whole folder, listed only the rows of the page, or gave the menu bar a list of its own would
/// leave this conversation's turns on this computer with nothing said, or said differently.
#[cfg(unix)]
#[test]
fn an_upload_refused_as_diverged_is_told_at_the_turns_end_and_in_status() {
    const SID: &str = "eeeeeeee-0000-4000-8000-000000000005";
    const OTHER: &str = "ffffffff-0000-4000-8000-000000000006";

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [32; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let work = temp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let work = work.canonicalize().unwrap();
    let projects = temp
        .path()
        .join(".claude/projects")
        .join(agit::domain::store::slug_for(&work));
    fs::create_dir_all(&projects).unwrap();
    let transcript = |session: &str| projects.join(format!("{session}.jsonl"));
    let add_turn = |session: &str, n: usize| {
        let cwd = work.to_string_lossy();
        let records = format!(
            "{}\n{}\n",
            json!({"type":"user","sessionId":session,"cwd":cwd,"uuid":format!("{session}-u{n}"),
                "timestamp":format!("2026-11-02T00:00:{n:02}.000Z"),
                "message":{"role":"user","content":format!("synthetic prompt {n}")}}),
            json!({"type":"assistant","sessionId":session,"cwd":cwd,"uuid":format!("{session}-a{n}"),
                "timestamp":format!("2026-11-02T00:00:{n:02}.500Z"),
                "message":{"role":"assistant","stop_reason":"end_turn",
                    "content":[{"type":"text","text":format!("synthetic answer {n}")}]}}),
        );
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(transcript(session))
            .unwrap()
            .write_all(records.as_bytes())
            .unwrap();
    };
    let agit = |args: &[&str], input: Option<String>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in git_command(temp.path()).get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        let mut child = command
            .current_dir(&work)
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_YES", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env("AGIT_SECRETS_KEYSTORE", "file")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(args)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(input.unwrap_or_default().as_bytes())
            .unwrap();
        drop(stdin);
        let output = child.wait_with_output().unwrap();
        assert_success(&output);
        output
    };
    // A turn's end in Claude Code: its Stop hook, which settles the turn and uploads it.
    let stop = |session: &str| {
        let output = agit(
            &["hooks", "settle", "--runtime", "claude-code"],
            Some(
                json!({"session_id":session,"cwd":work,"transcript_path":transcript(session),
                    "hook_event_name":"Stop"})
                .to_string(),
            ),
        );
        String::from_utf8(output.stdout).unwrap()
    };

    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["config", "agit.autoPush", "true"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    agit(&["push", "alice/app@main"], None);
    for (session, branch) in [(SID, "alice/app@work"), (OTHER, "alice/app@other")] {
        add_turn(session, 1);
        agit(
            &[
                "import",
                session,
                "--from",
                "claude-code",
                "--into",
                branch,
                "--independent",
            ],
            None,
        );
    }
    agit(&["push", "alice/app@work"], None);

    // The conversation goes on on another computer, whose turn the Hub now holds.
    let hub_tip = git(&remote, &["rev-parse", "refs/heads/work"]);
    let tree = git(&remote, &["rev-parse", &format!("{hub_tip}^{{tree}}")]);
    let elsewhere = git(
        &remote,
        &[
            "-c",
            "user.name=Source Writer",
            "-c",
            "user.email=source@example.invalid",
            "commit-tree",
            &tree,
            "-p",
            &hub_tip,
            "-m",
            "A turn on another computer",
        ],
    );
    git(&remote, &["update-ref", "refs/heads/work", &elsewhere]);

    add_turn(SID, 2);
    assert!(!stop(SID).contains("systemMessage"));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), elsewhere);
    // The other conversation in this folder uploads, and is not told this one's failure.
    add_turn(OTHER, 2);
    assert!(!stop(OTHER).contains("systemMessage"));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/other"]),
        git(repo.root(), &["rev-parse", "refs/heads/other"])
    );

    add_turn(SID, 3);
    let said = stop(SID);
    let message: serde_json::Value =
        serde_json::from_str(said.lines().next().unwrap_or_default()).unwrap_or_default();
    assert_eq!(
        message["systemMessage"],
        "agit: the last automatic upload to alice/app failed: this conversation also continued on another computer, so the Hub has turns this computer doesn't; turns here stay on this computer, and `agit push` for this branch shows how to keep both or merge them.",
        "{said}"
    );

    let status = agit(&["status"], None);
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        shown.contains("uploads that didn't go through")
            && shown.contains(
                "alice/app@work: this conversation also continued on another computer, so the Hub has turns this computer doesn't"
            )
            && shown.contains("`agit push alice/app@work`")
            && !shown.contains("alice/app@other:"),
        "{shown}"
    );
    // Past the last page of sessions, the list is the same.
    let status = agit(&["status", "--json", "--offset", "1000"], None);
    let value =
        &serde_json::from_slice::<serde_json::Value>(&status.stdout).unwrap()["result"]["value"];
    assert_eq!(value["sessions"]["items"], json!([]), "{value}");
    assert_eq!(
        value["uploads_not_through"]["items"],
        json!([{
            "target": "alice/app@work",
            "state": "failed",
            "reason": "diverged",
            "description": "this conversation also continued on another computer, so the Hub has turns this computer doesn't; turns here stay on this computer, and `agit push` for this branch shows how to keep both or merge them",
            "command": "agit push alice/app@work",
        }]),
        "{value}"
    );
    let listed = agit(&["project", "list", "--json"], None);
    let listed =
        &serde_json::from_slice::<serde_json::Value>(&listed.stdout).unwrap()["result"]["value"];
    assert_eq!(
        listed["uploads_not_through"], value["uploads_not_through"],
        "{listed}"
    );
}

/// A file line that diverged from the Hub's, as when two people distill the same memory into
/// main, is refused with `pull` as the way out: pull merges a file line's files and stops only
/// where both sides changed the same lines. When the Hub's tip is here and holds exactly the
/// same files, the refusal says so. Offering only a fork or a merge by hand left a person
/// merging two mains that held the same files.
#[test]
fn a_diverged_file_line_is_offered_pull_and_told_when_the_files_match() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [29; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    let published = git(&remote, &["rev-parse", "refs/heads/main"]);

    // A teammate's distill lands on the Hub, and the same files are distilled here.
    let theirs = git(
        &remote,
        &[
            "-c",
            "user.name=Teammate",
            "-c",
            "user.email=teammate@example.invalid",
            "commit-tree",
            &format!("{published}^{{tree}}"),
            "-p",
            &published,
            "-m",
            "Teammate's distill",
        ],
    );
    git(&remote, &["update-ref", "refs/heads/main", &theirs]);
    repo.git(&["commit", "--allow-empty", "-m", "Distill here"])
        .unwrap();
    let mine = git(repo.root(), &["rev-parse", "refs/heads/main"]);

    // Before a fetch the Hub's tip isn't here, so nothing is said about its files or counted.
    let refused = run(&["--yes", "push", "alice/app@main"]);
    assert!(!refused.status.success(), "{refused:?}");
    let said = String::from_utf8_lossy(&refused.stderr).into_owned();
    for expected in [
        "remote branch main has diverged from this checkout (each side has commits the other does not;",
        "→ bring the Hub's changes in: `agit pull alice/app@main` (it merges the files and stops only where both sides changed the same lines), then `agit push alice/app@main`",
    ] {
        assert!(said.contains(expected), "{expected}: {said}");
    }
    assert!(!said.contains("same files"), "{said}");
    assert!(!said.contains("agit fork"), "{said}");

    assert_success(&run(&["fetch", "alice/app"]));
    let refused = run(&["--yes", "push", "alice/app@main"]);
    assert!(!refused.status.success(), "{refused:?}");
    let said = String::from_utf8_lossy(&refused.stderr).into_owned();
    for expected in [
        "(local +1 / Hub +1; Hub tip ",
        ", but both sides hold the same files; local unpublished commits are kept",
        "→ bring the Hub's line in: `agit pull alice/app@main`, then `agit push alice/app@main`",
    ] {
        assert!(said.contains(expected), "{expected}: {said}");
    }
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), theirs);

    // The two commands it names are the whole way: the pull merges, and the push publishes both.
    assert_success(&run(&["pull", "alice/app@main"]));
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    let merged = git(&remote, &["rev-parse", "refs/heads/main"]);
    assert_eq!(merged, git(repo.root(), &["rev-parse", "refs/heads/main"]));
    for side in [&mine, &theirs] {
        git(&remote, &["merge-base", "--is-ancestor", side, &merged]);
    }
}

/// A turn published without the records plain publication leaves out reaches the Hub as another
/// commit on the same parent, and the accepted publication record names the turn it publishes.
/// `agit run`, `agit resume` and `agit pull` take that Hub tip as the turn: the branch continues as
/// it is and reads as up to date. Compared as raw tips, the turn and its published form count as
/// one turn here and one on the Hub, and the session is refused right after its own upload. The
/// Hub tip stands for the turn only on a branch that holds it: a branch started again under the
/// same name after the upload, and a turn another computer adds on top of the Hub tip, are still a
/// divergence. Taking the record's word without checking that the branch holds the turn continues
/// the new branch over the published one; mapping any Hub tip that descends from a published turn
/// continues past the other computer's turn. A branch that tracks the Hub's is held to have
/// integrated its tracking ref when that ref is the published form of a turn it holds; read as a
/// commit to merge in, the uploader is told to merge its own turn. `agit merge` of the Hub tip into
/// the branch says there is nothing to merge. Pushing after another computer's turn names that turn
/// and a separate checkout of the Hub's copy; worded for an encrypted publication, it describes a
/// copy this plain session never had.
#[test]
fn run_resume_and_pull_take_a_published_form_as_the_turn_it_publishes() {
    const NATIVE: &str = "eeeeeeee-0000-4000-8000-000000000005";
    const REMADE: &str = "eeeeeeee-0000-4000-8000-000000000006";
    const CONTINUE: [&[&str]; 2] = [
        &["resume", "alice/app@talk", "--no-launch"],
        &["run", "alice/app@talk", "--no-launch"],
    ];
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [30; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let work = temp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let work = work.canonicalize().unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(&work)
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let said = |output: &Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    // Settles the first turn of session `id` on a `talk` started from main.
    let settle = |id: char, records: &[serde_json::Value]| {
        let mut session = meta::Meta::new(
            format!("agit-{}", id.to_string().repeat(40)),
            "claude-code".into(),
            work.to_string_lossy().into_owned(),
        );
        session.turn = Some(1);
        let text: String = records.iter().map(|record| format!("{record}\n")).collect();
        let log = transcript::wrap_lines(&text, "claude-code", &session.session);
        repo.git(&["checkout", "--quiet", "-B", "talk", "main"])
            .unwrap();
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), &session).unwrap();
        repo.add_all().unwrap();
        repo.commit("Turn 1").unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    let head = || git(repo.root(), &["rev-parse", "refs/heads/talk"]);

    let turn = settle(
        'f',
        &[
            json!({"type":"user","sessionId":NATIVE,"cwd":work,"uuid":"u1","parentUuid":null,
                "message":{"role":"user","content":"Rename the flag."}}),
            // Instructions the runtime put into the model's context: plain publication leaves
            // this record out.
            json!({"type":"attachment","sessionId":NATIVE,"cwd":work,"uuid":"u2","parentUuid":"u1",
                "attachment":{"type":"instructions","path":"/work/CLAUDE.md","content":"Project rules."}}),
            json!({"type":"assistant","sessionId":NATIVE,"cwd":work,"uuid":"u3","parentUuid":"u2",
                "message":{"role":"assistant","content":[{"type":"text","text":"Renamed."}]}}),
        ],
    );
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    let published = git(&remote, &["rev-parse", "refs/heads/talk"]);
    assert_ne!(published, turn, "the Hub holds the turn's published form");
    assert_eq!(
        git(&remote, &["rev-parse", &format!("{published}^")]),
        git(repo.root(), &["rev-parse", &format!("{turn}^")])
    );
    for args in CONTINUE {
        let output = run(args);
        assert_success(&output);
        assert!(
            !said(&output).contains("diverged"),
            "{args:?}: {}",
            said(&output)
        );
        assert_eq!(head(), turn, "{args:?}");
    }
    let pulled = run(&["pull", "alice/app"]);
    assert_success(&pulled);
    let pulled = said(&pulled);
    assert!(
        pulled.contains("talk is up to date") && !pulled.contains("diverged"),
        "{pulled}"
    );
    assert_eq!(head(), turn);
    // Merging the Hub tip brings nothing: merged anyway, the turn would land a second time beside
    // its original, and every later upload would add another published copy to merge.
    let merged = run(&[
        "merge",
        &format!("alice/app@{published}"),
        "--into",
        "alice/app@talk",
        "--manual",
    ]);
    assert_success(&merged);
    let merged = said(&merged);
    assert!(merged.contains("nothing to merge"), "{merged}");
    assert_eq!(head(), turn);
    // With the branch tracking the Hub's, its tracking ref names the published form, a sibling of
    // the turn, and run and resume still take it as the turn: read as a tip to merge in, the
    // computer that uploaded would be told to merge its own turn.
    repo.git(&["branch", "--quiet", "--set-upstream-to=origin/talk", "talk"])
        .unwrap();
    assert_eq!(
        repo.git(&["rev-parse", "talk@{upstream}"]).unwrap().trim(),
        published
    );
    for args in CONTINUE {
        let output = run(args);
        assert_success(&output);
        assert!(
            !said(&output).contains("not integrated"),
            "{args:?}: {}",
            said(&output)
        );
        assert_eq!(head(), turn, "{args:?}");
    }
    repo.git(&["branch", "--quiet", "--unset-upstream", "talk"])
        .unwrap();

    // `talk` deleted here and started again under its name, as `agit branch rm` and
    // `agit new -b talk` leave it: the Hub tip publishes a turn this branch does not hold.
    let remade = settle(
        'e',
        &[
            json!({"type":"user","sessionId":REMADE,"cwd":work,"uuid":"u1","parentUuid":null,
                "message":{"role":"user","content":"Start over."}}),
            json!({"type":"assistant","sessionId":REMADE,"cwd":work,"uuid":"u2","parentUuid":"u1",
                "message":{"role":"assistant","content":[{"type":"text","text":"Started."}]}}),
        ],
    );
    for args in CONTINUE {
        let output = run(args);
        assert_eq!(output.status.code(), Some(4), "{args:?}: {}", said(&output));
        assert!(
            said(&output).contains("diverged from the published tip"),
            "{args:?}: {}",
            said(&output)
        );
        assert_eq!(head(), remade, "{args:?}");
    }

    // The uploaded turn back on `talk`, and a turn another computer uploads on top of its
    // published form.
    repo.git(&["checkout", "--quiet", "-B", "talk", &turn])
        .unwrap();
    let elsewhere = git(
        &remote,
        &[
            "-c",
            "user.name=Laptop Writer",
            "-c",
            "user.email=laptop@example.invalid",
            "commit-tree",
            &format!("{published}^{{tree}}"),
            "-p",
            &published,
            "-m",
            "Turn 2",
        ],
    );
    git(&remote, &["update-ref", "refs/heads/talk", &elsewhere]);
    for args in CONTINUE {
        let output = run(args);
        assert_eq!(output.status.code(), Some(4), "{args:?}: {}", said(&output));
        assert!(
            said(&output).contains("diverged from the published tip"),
            "{args:?}: {}",
            said(&output)
        );
        assert_eq!(head(), turn, "{args:?}");
    }
    let pulled = run(&["pull", "alice/app"]);
    assert_eq!(pulled.status.code(), Some(4), "{}", said(&pulled));
    assert!(
        said(&pulled).contains("talk truly diverged"),
        "{}",
        said(&pulled)
    );
    assert_eq!(head(), turn);
    // Nothing here is new, so push names the Hub's turn as the other computer's and points at a
    // separate checkout of the Hub's copy, never at an encrypted copy this plain session never had.
    let pushed = run(&["--yes", "push", "alice/app@talk"]);
    assert!(!pushed.status.success(), "{}", said(&pushed));
    let refusal = said(&pushed);
    assert!(
        refusal.contains(
            "the Hub has a newer turn from another computer on top of what this checkout uploaded"
        ) && refusal.contains("clone alice/app into a separate checkout")
            && !refusal.contains("encrypted"),
        "{refusal}"
    );
    assert_eq!(head(), turn);
}

/// The publication summary says how many records the upload adds that keep parts the privacy check
/// left out, too large or with too many findings, each counted in the upload that first carries
/// it. A session goes on publishing its whole LOG, so a count over each snapshot would repeat an
/// old record on every later upload, and one that read only what the summary already showed would
/// leave such a record unsaid.
#[test]
fn an_upload_says_once_which_records_keep_parts_left_out() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let hub = Hub::start(&remote_root, [28; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    repo.git(&["checkout", "-b", "talk"]).unwrap();
    let session = meta::Meta::new(
        format!("agit-{}", "d".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let mut records = Vec::new();
    let mut upload = |added: &[serde_json::Value]| {
        records.extend_from_slice(added);
        let text: String = records.iter().map(|record| format!("{record}\n")).collect();
        let log = transcript::wrap_lines(&text, "claude-code", &session.session);
        write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), &session).unwrap();
        repo.add_all().unwrap();
        repo.commit(&format!("turn {}", records.len())).unwrap();
        let pushed = run(&["--yes", "push", "alice/app@talk"]);
        assert_success(&pushed);
        String::from_utf8_lossy(&pushed.stdout).into_owned()
    };
    let user = |text: &str| json!({"type":"user","message":{"role":"user","content":text}});
    let result = |text: &str| {
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_1","content":text}
        ]}})
    };

    let said = upload(&[
        user("read the log"),
        result("[content omitted: too large for the privacy check]"),
    ]);
    assert!(
        said.contains("1 record in this upload has parts the privacy check left out"),
        "{said}"
    );
    let said = upload(&[user("and the next one")]);
    assert!(!said.contains("left out"), "{said}");
    let said = upload(&[
        result("[content omitted: too many findings for the privacy check]"),
        result("[content omitted: too large for the privacy check] and more"),
    ]);
    assert!(
        said.contains("2 records in this upload have parts the privacy check left out"),
        "{said}"
    );
}

/// A push that decided to create its repository creates only a new one. When the name is taken
/// by the time it asks the Hub, nothing is created or published, the push says so by name, and
/// only the person decides whether to publish into the repository that exists: the `--yes` given
/// for creating it does not answer that, and with nobody to ask the whole push ends, also with
/// `--keep-going` over several branches. A push that took the existing repository in place of the
/// new one would publish where nobody chose to; one that went on to publish the branches one at a
/// time would find it there and do the same.
#[test]
fn a_repository_that_appears_before_its_creation_is_asked_about_again() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [29; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    repo.git(&["checkout", "-b", "talk"]).unwrap();
    let session = meta::Meta::new(
        format!("agit-{}", "e".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let log = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"hello"}})
        ),
        "claude-code",
        &session.session,
    );
    write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &session).unwrap();
    repo.add_all().unwrap();
    repo.commit("turn 1").unwrap();
    repo.git(&["checkout", "main"]).unwrap();

    hub.taken_on_create.store(true, Ordering::Release);
    let refused = run(&["--yes", "push", "alice/app", "--all", "--keep-going"]);
    assert!(!refused.status.success(), "{refused:?}");
    let said = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        said.contains("alice/app already exists on the Hub, so this push did not create it and published nothing")
            && said.contains("run `agit push` again at a terminal"),
        "{refused:?}"
    );
    assert!(git(&remote, &["for-each-ref"]).is_empty(), "{refused:?}");

    // A new push sees the repository that exists, and a `--yes` given for it publishes there.
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/main"]),
        git(repo.root(), &["rev-parse", "HEAD"])
    );
}

/// A push whose answer was lost, as when a proxy answers 502 for a push the Hub went on to take,
/// leaves the person not knowing whether the Hub has it. Its retry finds nothing to send and says
/// that the branch is on the Hub, also when a local tag it leaves out means there is still
/// something the Hub lacks: "nothing new to upload" alone left open whether the failed push had
/// reached it.
#[test]
fn a_retry_after_a_lost_answer_says_the_branch_is_on_the_hub() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [29; 32]);
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear_local();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let said = |output: &std::process::Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["config", "tag.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    repo.git(&["checkout", "-b", "talk"]).unwrap();
    let session = meta::Meta::new(
        format!("agit-{}", "e".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let log = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"hello"}})
        ),
        "claude-code",
        &session.session,
    );
    write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &session).unwrap();
    repo.add_all().unwrap();
    repo.commit("turn 1").unwrap();
    repo.git(&["checkout", "main"]).unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@main"]));

    fs::write(repo.root().join("README.md"), "Shared file, merged").unwrap();
    repo.add_all().unwrap();
    repo.commit("Take main's decisions").unwrap();
    let merged = git(repo.root(), &["rev-parse", "refs/heads/main"]);
    hub.lose_receive_answer.store(true, Ordering::Release);
    let failed = run(&["--yes", "push", "alice/app@main"]);
    assert!(!failed.status.success(), "{failed:?}");
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), merged);

    let refs = || {
        git(
            &remote,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        )
    };
    let before = refs();
    let retried = run(&["--yes", "push", "alice/app@main"]);
    assert_success(&retried);
    assert!(
        said(&retried).contains("alice/app: already up to date; main is on the Hub."),
        "{retried:?}"
    );
    assert_eq!(refs(), before);

    // A tag the push leaves out still keeps the repository from being up to date, and the branch
    // is named as on the Hub all the same.
    assert_success(&run(&["tag", "ms-talk", "alice/app@talk"]));
    let tagged = run(&["--yes", "push", "alice/app@main"]);
    assert_success(&tagged);
    let tagged = said(&tagged);
    assert!(
        tagged.contains("alice/app: main is already on the Hub; nothing new to upload.")
            && tagged.contains("tag ms-talk is not uploaded"),
        "{tagged}"
    );
    assert!(!tagged.contains("already up to date"), "{tagged}");
    assert_eq!(refs(), before);
}
