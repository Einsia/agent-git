#![cfg(feature = "cli")]

//! Unlock, resume and settlement preserve native history and its selected context.

use agit::domain::{
    meta,
    privacy_envelope::{PrivacyEnvelope, ViewingRecipient, digest_bytes},
    privacy_layer::PrivateLayer,
    repo::Repo,
    session_privacy::{
        AcceptedSnapshot, AccountIdentity, CommitterAttribution, OwnerKeyBinding, SessionPrivacy,
        SessionSnapshotBinding, SourceCommitter,
    },
    storage, transcript,
};
use agit::infra::{
    config,
    credentials::{HubCredential, save_at},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crypto_box::SecretKey;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[cfg(unix)]
#[path = "support/claude_recovery_terminal.rs"]
mod claude_recovery_terminal;
#[path = "support/original_access.rs"]
mod original_access;
#[path = "support/publication_identity.rs"]
mod publication_identity;
#[path = "support/startup_cache.rs"]
mod startup_cache;

#[test]
fn authorized_resume_recovers_original_codex_history_through_settlement() {
    recovery_roundtrip("codex", "codex", false);
}

#[test]
fn account_unlock_resume_preserves_native_history_and_upgrades_legacy_baselines() {
    recovery_roundtrip("claude-code", "claude-code", false);
}

#[test]
fn cross_runtime_recovery_uses_ordinary_conversion() {
    recovery_roundtrip("codex", "claude-code", false);
}

#[test]
fn legacy_recovery_with_new_turns_settles_its_quoted_context() {
    recovery_roundtrip("claude-code", "claude-code", true);
}

#[test]
#[ignore = "requires a real Claude CLI and configured model credentials"]
fn real_claude_loads_recovered_history_and_appends_a_turn() {
    let temp = tempfile::tempdir().unwrap();
    let (workspace, config, file) = prepare_real_claude_recovery(&temp, None);
    let id = file.file_stem().unwrap().to_str().unwrap();
    let native = std::fs::read_to_string(&file).unwrap();
    let mut command =
        Command::new(std::env::var_os("AGIT_TEST_CLAUDE").unwrap_or_else(|| "claude".into()));
    if let Some(model) = std::env::var_os("AGIT_TEST_CLAUDE_MODEL") {
        command.arg("--model").arg(model);
    }
    command
        .current_dir(&workspace)
        .env("AGIT_HOME", temp.path().join("agit"))
        .env("CLAUDE_CONFIG_DIR", &config)
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_MERGE_TX")
        .stdin(std::process::Stdio::null())
        .args([
            "--bare",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--tools",
            "",
            "--disable-slash-commands",
            "--resume",
            id,
            "--print",
            "--output-format",
            "json",
            "Reply with only the synthetic recovery marker from the historical record.",
        ]);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ORCHID-COPPER-927"),
        "{output:?}"
    );
    let continued = std::fs::read_to_string(file).unwrap();
    assert!(continued.len() > native.len());
    assert!(continued.contains("Reply with only the synthetic recovery marker"));
}

#[cfg(unix)]
#[test]
#[ignore = "requires a real Claude CLI; uses no model credentials or model requests"]
fn real_claude_interactively_loads_recovery_without_a_model_override() {
    let temp = tempfile::tempdir().unwrap();
    let (workspace, config, file) = prepare_real_claude_recovery(&temp, Some("glm-5.3"));
    let id = file.file_stem().unwrap().to_str().unwrap();
    claude_recovery_terminal::assert_loads(&config, &workspace, id);
}

fn prepare_real_claude_recovery(
    temp: &tempfile::TempDir,
    source_model: Option<&str>,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let workspace = recovery_workspace(temp.path());
    let raw = claude_history(source_model.unwrap_or("glm-5.3"));
    let saved = transcript::wrap_lines(&raw, "claude-code", &format!("agit-{}", "a".repeat(40)));
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let repo = Repo::init(&home.join("repos/local/recovery")).unwrap();
    repo.git(&["config", "user.name", "Synthetic User"])
        .unwrap();
    repo.git(&["config", "user.email", "synthetic@example.invalid"])
        .unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    repo.git(&["checkout", "-b", "session"]).unwrap();
    storage::write_snapshot(repo.root(), &saved, &saved).unwrap();
    meta::write(
        repo.root(),
        &meta::Meta::new(
            format!("agit-{}", "a".repeat(40)),
            "claude-code".into(),
            workspace.to_string_lossy().into_owned(),
        ),
    )
    .unwrap();
    let account = AccountIdentity::new("http://127.0.0.1:1", "account-alice").unwrap();
    let mut metadata = meta::read(repo.root()).unwrap();
    metadata.session_privacy = Some(SessionPrivacy::new(Some(account.clone())).unwrap());
    metadata.committer_account = Some(account.clone());
    let mut layer = PrivateLayer::new(
        &saved,
        &saved,
        serde_json::to_value(&metadata).unwrap(),
        BTreeMap::new(),
    )
    .unwrap();
    layer.source_committer = Some(
        SourceCommitter::from_commit(
            b"committer Alice <alice@example.invalid> 1 +0000\n\nOriginal session\n",
            Some(account),
        )
        .unwrap(),
    );
    let key = SecretKey::from([13; 32]);
    let public = STANDARD.encode(key.public_key().as_bytes());
    let owner_key = OwnerKeyBinding {
        account_id: "account-alice".into(),
        key_version: 1,
    };
    let recipient =
        ViewingRecipient::from_base64(owner_key.recipient_id().unwrap(), &public).unwrap();
    let envelope = PrivacyEnvelope::seal_session(
        digest_bytes(b"policy"),
        json!({}),
        &serde_json::to_vec(&layer).unwrap(),
        &recipient,
        SessionSnapshotBinding {
            repository_id: "00000000-0000-0000-0000-000000000001".into(),
            session_id: metadata.session.clone(),
            published_session_id: format!("agit-{}", "b".repeat(40)),
            owner_key,
            committer: CommitterAttribution::Account {
                account_id: "account-alice".into(),
            },
            source_commit: "c".repeat(40),
        },
    )
    .unwrap();
    let publication = serde_json::to_vec(&envelope).unwrap();
    std::fs::create_dir_all(repo.root().join("privacy")).unwrap();
    std::fs::write(repo.root().join("privacy/envelope.json"), &publication).unwrap();
    repo.add_all().unwrap();
    repo.commit("Synthetic recovered history").unwrap();
    let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let parent = repo.common_dir().unwrap().join("agit/privacy-recovery");
    std::fs::create_dir_all(&parent).unwrap();
    let recovered = layer.restore(&parent, &workspace).unwrap();
    agit::domain::privacy_recovery::write_manifest(recovered.path(), &publication).unwrap();
    std::fs::rename(recovered.path(), parent.join(head.trim())).unwrap();
    let config = temp.path().join("claude");
    let prepared = Command::new(env!("CARGO_BIN_EXE_agit"))
        .current_dir(&workspace)
        .env("AGIT_HOME", &home)
        .env("CLAUDE_CONFIG_DIR", &config)
        .env("AGIT_TUI", "0")
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_MERGE_TX")
        .stdin(std::process::Stdio::null())
        .args([
            "resume",
            "local/recovery@session",
            "--as",
            "claude-code",
            "--no-launch",
            "--cwd",
        ])
        .arg(&workspace)
        .output()
        .unwrap();
    assert!(prepared.status.success(), "{prepared:?}");
    let physical = agit::infra::git_runtime::path_for_git(workspace.canonicalize().unwrap());
    let project = config
        .join("projects")
        .join(agit::domain::store::slug_for(&physical));
    let file = std::fs::read_dir(project)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let native = std::fs::read_to_string(&file).unwrap();
    assert_native_history(&native, &raw, "claude-code", &file, &workspace);
    (workspace, config, file)
}

fn recovery_workspace(root: &std::path::Path) -> std::path::PathBuf {
    let physical = root.join("workspace");
    std::fs::create_dir(&physical).unwrap();
    #[cfg(unix)]
    {
        let alias = root.join("workspace-alias");
        std::os::unix::fs::symlink(&physical, &alias).unwrap();
        assert_ne!(alias, alias.canonicalize().unwrap());
        alias
    }
    #[cfg(not(unix))]
    physical
}

fn recovery_roundtrip(source: &str, runtime: &str, legacy_commit: bool) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let workspace = recovery_workspace(temp.path());
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "user.name", "Synthetic User"])
        .unwrap();
    repo.git(&["config", "user.email", "synthetic@example.invalid"])
        .unwrap();
    repo.git(&["checkout", "-b", "encrypted"]).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let identity =
        agit::hub::identity::RemoteIdentity::new(&base, "00000000-0000-0000-0000-000000000001")
            .unwrap();
    agit::hub::identity::pin(&repo, &identity).unwrap();
    repo.set_remote(&format!("{base}/alice/app.git")).unwrap();
    let raw = if source == "claude-code" {
        claude_history("glm-5.3")
    } else {
        [
            json!({"type":"session_meta","payload":{"id":"source-runtime","cwd":"/source/workspace","base_instructions":"SYNTHETIC_OLD_AUTHORITY","approval_policy":"never"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"synthetic private session"}]}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"historical-call","arguments":"SYNTHETIC_OLD_TOOL"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"historical-call","output":"SYNTHETIC_OLD_SHELL_OUTPUT"}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"Read","call_id":"private-file","arguments":json!({"file_path":"/source/private.txt"}).to_string()}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"private-file","output":"SYNTHETIC_EXCLUDED_FILE_BODY"}}),
        ].into_iter().map(|record| format!("{record}\n")).collect::<String>()
    };
    let original = transcript::wrap_lines(&raw, source, &format!("agit-{}", "a".repeat(40)));
    // A compacted VIEW omits bootstrap and discarded conversation; only bootstrap may return.
    let view = if source == "codex" {
        original.split_once('\n').unwrap().1.to_owned()
    } else {
        original.clone()
    };
    let log_only = if source == "codex" {
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"SYNTHETIC_LOG_ONLY_EVIDENCE"}]}})
    } else {
        json!({"type":"assistant","message":{"role":"assistant","model":"glm-5.3","content":[{"type":"text","text":"SYNTHETIC_LOG_ONLY_EVIDENCE"}]}})
    };
    let log = format!(
        "{original}{}",
        transcript::wrap_lines(
            &format!("{log_only}\n"),
            source,
            &format!("agit-{}", "a".repeat(40))
        )
    );
    let mut metadata = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        source.into(),
        "/source/workspace".into(),
    );
    let ownership =
        SessionPrivacy::new(Some(AccountIdentity::new(&base, "account-alice").unwrap())).unwrap();
    metadata.session_privacy = Some(ownership.clone());
    metadata.committer_account = Some(AccountIdentity::new(&base, "account-bob").unwrap());
    let source_committer = SourceCommitter::from_commit(
        b"committer Bob <bob@example.invalid> 1 +0000\n\nOriginal session\n",
        metadata.committer_account.clone(),
    )
    .unwrap();
    let mut layer = PrivateLayer::new(
        &log,
        &view,
        serde_json::to_value(metadata).unwrap(),
        BTreeMap::new(),
    )
    .unwrap();
    layer.source_committer = Some(source_committer);
    layer
        .protected_values
        .insert("synthetic private session".into());
    layer.protected_values.extend([
        "SYNTHETIC_OLD_AUTHORITY".into(),
        "SYNTHETIC_OLD_TOOL".into(),
        "SYNTHETIC_OLD_SHELL_OUTPUT".into(),
    ]);
    let key = SecretKey::from([13; 32]);
    let public = STANDARD.encode(key.public_key().as_bytes());
    let owner_key = OwnerKeyBinding {
        account_id: "account-alice".into(),
        key_version: 1,
    };
    let recipient =
        ViewingRecipient::from_base64(owner_key.recipient_id().unwrap(), &public).unwrap();
    let envelope = PrivacyEnvelope::seal_session(
        digest_bytes(b"policy"),
        json!({"content":"sanitized", "metadata":{"session":format!("agit-{}", "b".repeat(40))}}),
        &serde_json::to_vec(&layer).unwrap(),
        &recipient,
        SessionSnapshotBinding {
            repository_id: identity.agent_id.clone(),
            session_id: format!("agit-{}", "a".repeat(40)),
            published_session_id: format!("agit-{}", "b".repeat(40)),
            owner_key,
            committer: CommitterAttribution::Account {
                account_id: "account-bob".into(),
            },
            source_commit: "c".repeat(40),
        },
    )
    .unwrap();
    let content_key = STANDARD.encode(
        key.unseal(
            &STANDARD
                .decode(&envelope.private_payload.wrapped_keys[0].ciphertext)
                .unwrap(),
        )
        .unwrap(),
    );
    std::fs::create_dir_all(repo.root().join("privacy")).unwrap();
    std::fs::write(
        repo.root().join("privacy/envelope.json"),
        serde_json::to_vec(&envelope).unwrap(),
    )
    .unwrap();
    let public_session = format!("agit-{}", "b".repeat(40));
    let public_log = transcript::wrap_lines(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"sanitized\"}}\n",
        "claude-code",
        &public_session,
    );
    storage::write_snapshot(repo.root(), &public_log, &public_log).unwrap();
    meta::write(
        repo.root(),
        &meta::Meta::new(public_session, "claude-code".into(), String::new()),
    )
    .unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "Synthetic encrypted publication"])
        .unwrap();
    let before = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();

    let expected = AcceptedSnapshot {
        version: 2,
        binding: envelope.session_binding.clone().unwrap(),
        published_commit: before.clone(),
        envelope_digest: digest_bytes(&serde_json::to_vec(&envelope).unwrap()),
    };
    let key_response = json!({"snapshot": expected, "content_key": content_key});
    let automatic_recovery = source == "codex" && runtime == "codex";
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let fork_access = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source_access = Arc::clone(&fork_access);
    let source_route = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let served_route = Arc::clone(&source_route);
    let response_hub = base.clone();
    let response_envelope = serde_json::to_string(&envelope).unwrap();
    let server = std::thread::spawn(move || {
        let mut grants = original_access::Fixture::new(expected.clone());
        let started = Instant::now();
        let mut requests = Vec::new();
        let mut key_requests = 0;
        let mut status_requests = 0;
        while !stopped.load(Ordering::Relaxed) && started.elapsed().as_secs() < 90 {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("mock Hub accept failed: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            match reader.read_line(&mut request) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(error) => panic!("mock Hub read failed: {error}"),
            }
            let path = request.split_whitespace().nth(1).unwrap().to_owned();
            let mut length = 0;
            let mut authenticated = false;
            let mut expected_ids = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                let header = line.to_ascii_lowercase();
                if let Some(value) = header.strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if header.trim() == "authorization: bearer synthetic-access" {
                    authenticated = true;
                }
                if let Some(value) = header.strip_prefix("x-agentgit-expected-agent-id:") {
                    expected_ids.push(value.trim().to_owned());
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            requests.push(path.clone());
            let route = served_route.load(Ordering::Acquire);
            if path.contains("/sessions/") || path.contains("/location") {
                assert_eq!(
                    expected_ids,
                    std::slice::from_ref(&expected.binding.repository_id)
                );
            }
            let canonical = path.replace("/alice/renamed/", "/alice/app/");
            let (status, response) = if path.ends_with("/location") {
                assert!(authenticated);
                assert_eq!(
                    path,
                    format!(
                        "/api/repositories/{}/location",
                        expected.binding.repository_id
                    )
                );
                match route {
                    2 => (
                        503,
                        json!({"kind":"repository_unavailable","error":"location temporarily unavailable"}),
                    ),
                    3 => (
                        404,
                        json!({"kind":"not_found","error":"source unavailable"}),
                    ),
                    _ => {
                        let name = if route == 1 { "renamed" } else { "app" };
                        (
                            200,
                            json!({"agent_id":expected.binding.repository_id,"owner":"alice","name":name,
                            "clone_url":format!("{response_hub}/alice/{name}.git")}),
                        )
                    }
                }
            } else if route == 1 && path.starts_with("/api/agents/alice/app/sessions/") {
                if path.contains("/privacy?ref=") {
                    (
                        200,
                        json!({"agent_id":"00000000-0000-0000-0000-000000000002",
                        "session_id":expected.binding.session_id,"commit":expected.published_commit,"scheme":"plain"}),
                    )
                } else {
                    (
                        412,
                        json!({"kind":"precondition_failed","error":"repository name now belongs to another identity"}),
                    )
                }
            } else if path.contains("/privacy?ref=") {
                assert!(authenticated);
                assert_eq!(
                    canonical,
                    format!(
                        "/api/agents/alice/app/sessions/{}/privacy?ref={}",
                        expected.binding.session_id, expected.published_commit
                    )
                );
                status_requests += 1;
                (
                    200,
                    json!({
                        "agent_id":expected.binding.repository_id,"session_id":expected.binding.session_id,
                        "commit":expected.published_commit,"scheme":"session_owner_v2",
                        "snapshot":expected,"envelope_bytes":response_envelope,
                        "original_status":"saved_encrypted",
                        "grant_policy":grants.policy(),
                        "display_status":if status_requests == 1 { "pending" } else { "ready" },
                        "capabilities":{"can_read_original":false,"can_manage_original_grants":false,
                            "reader":{"current_member":true,"owner_default":false,"committer_default":false,
                                "explicit_grant":false,"session_explicit_grant":false,"session_revoked":false,
                                "default_suppressed":false,"session_membership_invalidated":false,"revoked":false,"membership_invalidated":false}}
                    }),
                )
            } else if path.contains("/original-grant") {
                assert!(authenticated);
                grants.route(
                    request.split_whitespace().next().unwrap(),
                    &canonical,
                    &body,
                )
            } else if path.ends_with("/original-key") {
                assert!(
                    authenticated,
                    "original recovery requires the active account"
                );
                assert_eq!(
                    canonical,
                    format!(
                        "/api/agents/alice/app/sessions/{}/original-key",
                        expected.binding.session_id
                    )
                );
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap(),
                    json!({"snapshot": expected})
                );
                key_requests += 1;
                match (
                    source_access.load(Ordering::Acquire),
                    automatic_recovery,
                    key_requests,
                ) {
                    (1, _, _) => (
                        403,
                        json!({"kind": "original_access_denied", "error": "source grant revoked"}),
                    ),
                    (2, _, _) => (
                        503,
                        json!({"kind": "key_service_unavailable", "error": "try later"}),
                    ),
                    (_, true, 1) => (
                        403,
                        json!({"kind": "original_access_denied", "error": "no snapshot entitlement"}),
                    ),
                    (_, true, 2) => (
                        503,
                        json!({"kind": "key_service_unavailable", "error": "try later"}),
                    ),
                    (_, true, 3) => {
                        let mut mismatched = key_response.clone();
                        mismatched["snapshot"]["published_commit"] = "f".repeat(40).into();
                        (200, mismatched)
                    }
                    _ => (200, key_response.clone()),
                }
            } else {
                assert!(
                    authenticated,
                    "publication policies retain author authentication"
                );
                publication_identity::route(
                    &response_hub,
                    "alice",
                    request.split_whitespace().next().unwrap(),
                    &path,
                    &body,
                )
                .expect("known policy route")
            };
            let response = response.to_string();
            write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nCache-Control: private, no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        }
        (requests, key_requests)
    });
    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for name in ["PATH", "SystemRoot", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .current_dir(&workspace)
            .env("HOME", temp.path())
            .env("USERPROFILE", temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap()
    };
    let unlock_args = [
        "privacy",
        "unlock",
        "alice/app@encrypted",
        "--workspace",
        workspace.to_str().unwrap(),
    ];
    let anonymous = run(&unlock_args);
    assert!(!anonymous.status.success());
    assert!(String::from_utf8_lossy(&anonymous.stderr).contains("signed-in account"));
    let copy_identity =
        agit::hub::identity::RemoteIdentity::new(&base, "00000000-0000-0000-0000-000000000002")
            .unwrap();
    if automatic_recovery {
        // A signed-out fork consumes only published context, even when inherited original routing is absent.
        agit::hub::identity::rebind(&repo, &identity, &copy_identity).unwrap();
        repo.set_remote(&format!("{base}/bob/copy.git")).unwrap();
        let forked = run(&["fork", "alice/app@encrypted", "-b", "anonymous-copy"]);
        assert!(forked.status.success(), "{forked:?}");
        let copied = meta::read_at_ref(&repo, "anonymous-copy").unwrap();
        let origin = copied.fork_origin.unwrap();
        assert_eq!(
            origin.context,
            agit::domain::session_privacy::ForkContext::Sanitized
        );
        assert_eq!(
            origin.snapshot.unwrap().binding.repository_id,
            identity.agent_id
        );
        assert!(copied.session_privacy.unwrap().creator.is_none());
        assert_eq!(
            storage::materialize_at(repo.root(), "anonymous-copy", meta::LOG_FILE).unwrap(),
            public_log
        );
        assert!(
            repo.show_result("anonymous-copy", "privacy/envelope.json")
                .unwrap()
                .is_none()
        );
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap().trim(), before);
        agit::hub::identity::rebind(&repo, &copy_identity, &identity).unwrap();
        repo.set_remote(&format!("{base}/alice/app.git")).unwrap();
    }
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&base).unwrap())),
        &HubCredential {
            account_id: Some("account-bob".into()),
            username: "bob".into(),
            email: None,
            hub: Some(base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    if automatic_recovery {
        // Local edits cannot carry a parent's envelope into a different snapshot or alter staging on refusal.
        let draft = repo.root().join("draft.md");
        std::fs::write(&draft, "Selected draft\n").unwrap();
        repo.git(&["add", "draft.md"]).unwrap();
        std::fs::write(&draft, "Working draft\n").unwrap();
        let staged = repo.git(&["write-tree"]).unwrap();
        for args in [
            vec![
                "cherry-pick",
                "alice/app@encrypted#1",
                "--into",
                "alice/app@encrypted",
            ],
            vec!["revert", "alice/app@encrypted#1"],
            vec!["branch", "--repo", "alice/app", "seal", "encrypted"],
        ] {
            let refused = run(&args);
            assert!(!refused.status.success(), "{args:?}: {refused:?}");
            assert!(
                String::from_utf8_lossy(&refused.stderr)
                    .contains("native continuation or an independent fork"),
                "{args:?}: {refused:?}"
            );
            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap().trim(), before);
            assert_eq!(repo.git(&["write-tree"]).unwrap(), staged);
            assert_eq!(std::fs::read(&draft).unwrap(), b"Working draft\n");
        }
        repo.git(&["rm", "--cached", "--force", "draft.md"])
            .unwrap();
        std::fs::remove_file(draft).unwrap();
    }
    let resume_args = [
        "resume",
        "alice/app@encrypted",
        "--as",
        runtime,
        "--no-launch",
        "--cwd",
        workspace.to_str().unwrap(),
    ];
    let restored = repo
        .common_dir()
        .unwrap()
        .join("agit/privacy-recovery")
        .join(&before);
    if automatic_recovery {
        // Inherited ciphertext keeps source authority when the checkout publishes elsewhere.
        agit::hub::identity::rebind(&repo, &identity, &copy_identity).unwrap();
        repo.set_remote(&format!("{base}/bob/copy.git")).unwrap();
        source_route.store(2, Ordering::Release);
        let refused = run(&["fork", "alice/app@encrypted", "-b", "unresolved-copy"]);
        assert!(!refused.status.success(), "{refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("location temporarily unavailable"),
            "{refused:?}"
        );
        assert!(!repo.has_ref("refs/heads/unresolved-copy"));
        source_route.store(3, Ordering::Release);
        let sanitized = run(&["fork", "alice/app@encrypted", "-b", "inaccessible-source"]);
        assert!(sanitized.status.success(), "{sanitized:?}");
        assert_eq!(
            meta::read_at_ref(&repo, "inaccessible-source")
                .unwrap()
                .fork_origin
                .unwrap()
                .context,
            agit::domain::session_privacy::ForkContext::Sanitized
        );
        assert!(!restored.exists());
        source_route.store(0, Ordering::Release);
        let nested_identity =
            agit::hub::identity::RemoteIdentity::new(&base, "00000000-0000-0000-0000-000000000003")
                .unwrap();
        for display in ["pending", "ready"] {
            if display == "ready" {
                agit::hub::identity::rebind(&repo, &copy_identity, &nested_identity).unwrap();
                repo.set_remote(&format!("{base}/carol/nested.git"))
                    .unwrap();
            }
            let status = run(&["privacy", "status", &format!("alice/app@{before}")]);
            assert!(status.status.success(), "{status:?}");
            let text = String::from_utf8_lossy(&status.stdout);
            assert!(
                text.contains(&format!(
                    "Original saved encrypted; sanitized view {display}."
                )),
                "{status:?}"
            );
            assert!(text.contains("Original access: unavailable"), "{status:?}");
            assert!(!restored.exists());
            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap().trim(), before);
        }
        agit::hub::identity::rebind(&repo, &nested_identity, &copy_identity).unwrap();
        repo.set_remote(&format!("{base}/bob/copy.git")).unwrap();
        let sources = repo.common_dir().unwrap().join("agit/privacy-sources");
        std::fs::create_dir_all(&sources).unwrap();
        std::fs::write(
            sources.join(format!("{}.json", identity.agent_id)),
            serde_json::to_vec(&json!({"version":1,"identity":identity,"repository":"alice/app"}))
                .unwrap(),
        )
        .unwrap();
        source_route.store(1, Ordering::Release);
        let renamed = run(&["privacy", "status", &format!("alice/app@{before}")]);
        assert!(renamed.status.success(), "{renamed:?}");
        assert!(String::from_utf8_lossy(&renamed.stdout).contains("Original saved encrypted"));
        for reason in [
            "no snapshot entitlement",
            "try later",
            "differs from the requested snapshot",
        ] {
            let refused = run(&resume_args);
            assert!(!refused.status.success(), "{refused:?}");
            assert!(
                String::from_utf8_lossy(&refused.stderr).contains(reason),
                "{refused:?}"
            );
            assert!(!restored.exists());
            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap().trim(), before);
        }
    } else {
        let unlocked = run(&unlock_args);
        assert!(unlocked.status.success(), "{unlocked:?}");
        assert!(!String::from_utf8_lossy(&unlocked.stdout).contains(&content_key));
    }
    let resumed = run(&resume_args);
    assert!(resumed.status.success(), "{resumed:?}");
    if automatic_recovery {
        source_route.store(0, Ordering::Release);
        agit::hub::identity::rebind(&repo, &copy_identity, &identity).unwrap();
        repo.set_remote(&format!("{base}/alice/app.git")).unwrap();
    }
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap().trim(), before);
    assert_eq!(
        storage::materialize_worktree(&restored, meta::VIEW_FILE).unwrap(),
        view
    );
    assert_eq!(
        meta::read(&restored).unwrap().cwd,
        workspace.canonicalize().unwrap().to_string_lossy()
    );
    let key_text = STANDARD.encode(key.to_bytes());
    for file in walkdir::WalkDir::new(&restored) {
        let file = file.unwrap();
        if file.file_type().is_file() {
            let bytes = std::fs::read(file.path()).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains(&key_text));
            assert!(!text.contains(&content_key));
        }
    }

    assert!(
        String::from_utf8_lossy(&resumed.stdout).contains("Restoring private history"),
        "{}",
        String::from_utf8_lossy(&resumed.stdout)
    );
    let runtime_root = temp.path().join(if runtime == "codex" {
        ".codex/sessions"
    } else {
        ".claude/projects"
    });
    let mut runtime_file = walkdir::WalkDir::new(&runtime_root)
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .expect("resume must install a live transcript")
        .into_path();
    let active = std::fs::read_to_string(&runtime_file).unwrap();
    assert!(active.contains("synthetic private session"));
    assert!(!active.contains("SYNTHETIC_LOG_ONLY_EVIDENCE"));
    assert!(!active.contains("historical session evidence"));
    assert!(!active.contains("Historical session evidence loaded"));
    assert!(active.contains("SYNTHETIC_OLD_TOOL"));
    assert!(active.contains("SYNTHETIC_OLD_SHELL_OUTPUT"));
    assert_eq!(
        String::from_utf8_lossy(&resumed.stdout).contains("(lossy)"),
        source != runtime
    );
    if source == runtime {
        assert_native_history(&active, &raw, runtime, &runtime_file, &workspace);
    }
    if source == "claude-code" && !legacy_commit {
        assert_native_history(&active, &raw, runtime, &runtime_file, &workspace);
        let valid_again = run(&resume_args);
        assert!(
            String::from_utf8_lossy(&valid_again.stdout).contains("reusing the prepared runtime")
        );

        let old_file = runtime_file.clone();
        let (old_link, _, legacy) =
            install_legacy_baseline(&repo, &before, &home, &old_file, &workspace);
        let replaced = run(&resume_args);
        assert!(replaced.status.success(), "{replaced:?}");
        assert!(
            !String::from_utf8_lossy(&replaced.stdout).contains("reusing the prepared runtime")
        );
        assert_eq!(std::fs::read_to_string(&old_file).unwrap(), legacy);
        let previous: Value = serde_json::from_slice(&std::fs::read(&old_link).unwrap()).unwrap();
        let successor = previous["superseded_by"]
            .as_str()
            .unwrap()
            .strip_prefix("claude-code/")
            .unwrap();
        runtime_file = old_file.with_file_name(format!("{successor}.jsonl"));
        let active = std::fs::read_to_string(&runtime_file).unwrap();
        assert_native_history(&active, &raw, runtime, &runtime_file, &workspace);

        #[cfg(unix)]
        {
            let id = runtime_file.file_stem().unwrap().to_str().unwrap();
            let link_path = home.join("store/claude-code").join(format!("{id}.json"));
            let mut aliased_claim: Value =
                serde_json::from_slice(&std::fs::read(&link_path).unwrap()).unwrap();
            assert_eq!(
                aliased_claim["cwd"],
                workspace.canonicalize().unwrap().to_str().unwrap()
            );
            aliased_claim["cwd"] = json!(workspace);
            std::fs::write(&link_path, aliased_claim.to_string()).unwrap();
            let physical = workspace.canonicalize().unwrap();
            let mut physical_args = resume_args;
            physical_args[6] = physical.to_str().unwrap();
            let repeated = run(&physical_args);
            assert!(repeated.status.success(), "{repeated:?}");
            assert!(
                String::from_utf8_lossy(&repeated.stdout).contains("reusing the prepared runtime")
            );
            let legacy_file = runtime_root
                .join(agit::domain::store::slug_for(&workspace))
                .join(format!("{id}.jsonl"));
            assert_ne!(legacy_file, runtime_file);
            std::fs::create_dir_all(legacy_file.parent().unwrap()).unwrap();
            std::fs::rename(&runtime_file, &legacy_file).unwrap();
            let claim = std::fs::read(&link_path).unwrap();
            let tail = format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":"unsettled turn in lexical project"}})
            );
            std::fs::write(&legacy_file, format!("{active}{tail}")).unwrap();
            let refused = run(&resume_args);
            assert!(!refused.status.success(), "{refused:?}");
            assert_eq!(std::fs::read(&link_path).unwrap(), claim);
            assert_eq!(
                std::fs::read_to_string(&legacy_file).unwrap(),
                format!("{active}{tail}")
            );
            let legacy: String = active
                .lines()
                .map(|line| {
                    let mut record: Value = serde_json::from_str(line).unwrap();
                    record["cwd"] = json!(workspace);
                    format!("{record}\n")
                })
                .collect();
            aliased_claim["baseline_bytes"] = json!(legacy.len());
            aliased_claim["baseline_hash"] = json!(
                digest_bytes(legacy.as_bytes())
                    .strip_prefix("sha256:")
                    .unwrap()
            );
            std::fs::write(&link_path, aliased_claim.to_string()).unwrap();
            std::fs::write(&legacy_file, &legacy).unwrap();

            let repaired = run(&resume_args);
            assert!(repaired.status.success(), "{repaired:?}");
            assert!(
                !String::from_utf8_lossy(&repaired.stdout).contains("reusing the prepared runtime")
            );
            assert_eq!(std::fs::read_to_string(&legacy_file).unwrap(), legacy);
            let superseded: Value =
                serde_json::from_slice(&std::fs::read(&link_path).unwrap()).unwrap();
            let successor = superseded["superseded_by"]
                .as_str()
                .unwrap()
                .strip_prefix("claude-code/")
                .unwrap();
            runtime_file = runtime_file.with_file_name(format!("{successor}.jsonl"));
            assert_native_history(
                &std::fs::read_to_string(&runtime_file).unwrap(),
                &raw,
                runtime,
                &runtime_file,
                &workspace,
            );
        }

        let old_file = runtime_file.clone();
        let (link_path, claim, legacy) =
            install_legacy_baseline(&repo, &before, &home, &old_file, &workspace);
        let rewritten = legacy.replacen("synthetic private session", "synthetic edited session", 1);
        assert_ne!(rewritten, legacy);
        std::fs::write(&old_file, &rewritten).unwrap();
        let refused = run(&resume_args);
        assert!(!refused.status.success());
        assert_eq!(std::fs::read_to_string(&old_file).unwrap(), rewritten);
        assert_eq!(
            std::fs::read_to_string(&link_path).unwrap(),
            claim.to_string()
        );

        let tail = format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"unsettled genuine turn"}})
        );
        std::fs::write(&old_file, format!("{legacy}{tail}")).unwrap();
        let refused = run(&resume_args);
        assert!(!String::from_utf8_lossy(&refused.stdout).contains("reusing the prepared runtime"));
        assert_eq!(
            std::fs::read_to_string(&old_file).unwrap(),
            format!("{legacy}{tail}")
        );
        assert!(
            serde_json::from_slice::<Value>(&std::fs::read(&link_path).unwrap())
                .unwrap()
                .get("superseded_by")
                .is_none()
        );

        let bookkeeping = format!(
            "{}\n{}\n",
            json!({"type":"custom-title","sessionId":old_file.file_stem().unwrap().to_str().unwrap(),"customTitle":"Recovered session"}),
            json!({"type":"agent-name","sessionId":old_file.file_stem().unwrap().to_str().unwrap(),"agentName":"Recovered session"})
        );
        let attempted_resume = format!("{legacy}{bookkeeping}");
        std::fs::write(&old_file, &attempted_resume).unwrap();
        let repaired = run(&resume_args);
        assert!(
            repaired.status.success(),
            "{}",
            String::from_utf8_lossy(&repaired.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&old_file).unwrap(),
            attempted_resume
        );
        let old_claim: Value = serde_json::from_slice(&std::fs::read(&link_path).unwrap()).unwrap();
        let successor = old_claim["superseded_by"]
            .as_str()
            .unwrap()
            .strip_prefix("claude-code/")
            .unwrap();
        runtime_file = old_file.with_file_name(format!("{successor}.jsonl"));
        assert_native_history(
            &std::fs::read_to_string(&runtime_file).unwrap(),
            &raw,
            runtime,
            &runtime_file,
            &workspace,
        );
        let repeated = run(&resume_args);
        assert!(String::from_utf8_lossy(&repeated.stdout).contains("reusing the prepared runtime"));
    }
    let session_id =
        agit::adapter::session_id_from_stem(runtime_file.file_stem().unwrap().to_str().unwrap());
    let runtime_link = home
        .join("store")
        .join(runtime)
        .join(format!("{session_id}.json"));
    let native_link: Value =
        serde_json::from_slice(&std::fs::read(&runtime_link).unwrap()).unwrap();
    assert_eq!(native_link["privacy_recovery_format"], "native-v1");
    if legacy_commit {
        install_legacy_baseline(&repo, &before, &home, &runtime_file, &workspace);
    }
    std::fs::write(repo.root().join("README.md"), "Local continuation notes\n").unwrap();
    let file_args = [
        "commit",
        "alice/app@encrypted",
        "-m",
        "Add local notes",
        "--",
        "README.md",
    ];
    let staged = repo.git(&["write-tree"]).unwrap();
    let files = run(&file_args);
    assert!(!files.status.success(), "{files:?}");
    assert!(
        String::from_utf8_lossy(&files.stderr)
            .contains("native continuation or an independent fork"),
        "{files:?}"
    );
    assert_eq!(repo.git(&["write-tree"]).unwrap(), staged);
    assert_eq!(
        std::fs::read(repo.root().join("README.md")).unwrap(),
        b"Local continuation notes\n"
    );
    let settlement_before = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
    assert_eq!(settlement_before, before);
    if !legacy_commit {
        let resumed_again = run(&resume_args);
        assert!(resumed_again.status.success(), "{resumed_again:?}");
        assert!(
            String::from_utf8_lossy(&resumed_again.stdout).contains("reusing the prepared runtime")
        );
    }

    let mut appended = std::fs::OpenOptions::new()
        .append(true)
        .open(&runtime_file)
        .unwrap();
    let new_turn = if runtime == "codex" {
        vec![
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"new private turn"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"new private reply"}]}}),
        ]
    } else {
        vec![
            json!({"type":"user","sessionId":runtime_file.file_stem().unwrap().to_str().unwrap(),"uuid":uuid::Uuid::now_v7(),"parentUuid":null,"message":{"role":"user","content":"new private turn"}}),
            json!({"type":"assistant","sessionId":runtime_file.file_stem().unwrap().to_str().unwrap(),"uuid":uuid::Uuid::now_v7(),"parentUuid":null,"opaque":"preserve-native","message":{"role":"assistant","model":"glm-5.3","content":[{"type":"thinking","thinking":"native reasoning","signature":"native-signature"},{"type":"text","text":"new private reply"}]}}),
        ]
    };
    let mut parent = std::fs::read_to_string(&runtime_file)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|record| record.get("uuid").cloned())
        .next_back()
        .unwrap_or(Value::Null);
    let mut continuation = String::new();
    for mut record in new_turn {
        if runtime == "claude-code" {
            record["cwd"] = json!(workspace.canonicalize().unwrap());
            record["parentUuid"] = parent.clone();
            parent = record["uuid"].clone();
        }
        continuation.push_str(&format!("{record}\n"));
        writeln!(appended, "{record}").unwrap();
    }
    drop(appended);

    if legacy_commit {
        let bytes = std::fs::read(&runtime_file).unwrap();
        let claim = std::fs::read(&runtime_link).unwrap();
        let refused = run(&resume_args);
        assert!(!refused.status.success(), "{refused:?}");
        assert!(String::from_utf8_lossy(&refused.stderr).contains("unsettled content"));
        assert_eq!(std::fs::read(&runtime_file).unwrap(), bytes);
        assert_eq!(std::fs::read(&runtime_link).unwrap(), claim);
    }

    let moved = restored.with_extension("held");
    std::fs::rename(&restored, &moved).unwrap();
    for args in [resume_args.as_slice(), &["commit", "alice/app@encrypted"]] {
        let refused = run(args);
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("private recovery data is missing")
        );
        assert_eq!(
            repo.git(&["rev-parse", "HEAD"]).unwrap().trim(),
            settlement_before
        );
    }
    std::fs::rename(&moved, &restored).unwrap();
    let manifest = restored.join("recovery.json");
    let manifest_bytes = std::fs::read(&manifest).unwrap();
    let mut changed: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    changed["publication_digest"] = Value::String(digest_bytes(b"another publication"));
    std::fs::write(&manifest, changed.to_string()).unwrap();
    let refused = run(&["commit", "alice/app@encrypted"]);
    assert!(!refused.status.success());
    assert_eq!(
        repo.git(&["rev-parse", "HEAD"]).unwrap().trim(),
        settlement_before
    );
    std::fs::write(&manifest, manifest_bytes).unwrap();

    let metadata_path = restored.join(meta::FILE);
    let metadata_bytes = std::fs::read(&metadata_path).unwrap();
    let manifest_bytes = std::fs::read(&manifest).unwrap();
    let mut changed: Value = serde_json::from_slice(&metadata_bytes).unwrap();
    changed["milestone"] = json!("Changed after resume");
    std::fs::write(&metadata_path, changed.to_string()).unwrap();
    agit::domain::privacy_recovery::write_manifest(
        &restored,
        &serde_json::to_vec(&envelope).unwrap(),
    )
    .unwrap();
    let refused = run(&["commit", "alice/app@encrypted"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("changed after materialization"));
    assert_eq!(
        repo.git(&["rev-parse", "HEAD"]).unwrap().trim(),
        settlement_before
    );
    std::fs::write(&metadata_path, metadata_bytes).unwrap();
    std::fs::write(&manifest, manifest_bytes).unwrap();

    let committed = run(&["commit", "alice/app@encrypted"]);
    assert!(
        committed.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&committed.stdout),
        String::from_utf8_lossy(&committed.stderr)
    );
    let files = run(&file_args);
    assert!(files.status.success(), "{files:?}");
    let after = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
    assert_ne!(
        after,
        before,
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&committed.stdout),
        String::from_utf8_lossy(&committed.stderr)
    );
    assert_eq!(
        meta::read_at_ref(&repo, &after).unwrap().session_privacy,
        Some(ownership)
    );
    assert_eq!(
        meta::read_at_ref(&repo, &after).unwrap().committer_account,
        Some(AccountIdentity::new(&base, "account-bob").unwrap())
    );
    let committed_log = storage::materialize_at(repo.root(), &after, meta::LOG_FILE).unwrap();
    assert!(committed_log.starts_with(&log));
    assert!(committed_log.contains("SYNTHETIC_LOG_ONLY_EVIDENCE"));
    assert!(committed_log.contains("new private turn"));
    assert!(
        repo.show_result(&after, "privacy/envelope.json")
            .unwrap()
            .is_none()
    );
    let committed_view = storage::materialize_at(repo.root(), &after, meta::VIEW_FILE).unwrap();
    assert!(!committed_view.contains("SYNTHETIC_LOG_ONLY_EVIDENCE"));
    if legacy_commit {
        assert!(committed_view.contains("historical session evidence"));
    } else {
        assert!(committed_view.starts_with(&view));
        assert_eq!(
            committed_log.strip_prefix(&log),
            committed_view.strip_prefix(&view)
        );
        assert!(!committed_view.contains("recovered_evidence"));
    }
    let settled_link: Value =
        serde_json::from_slice(&std::fs::read(&runtime_link).unwrap()).unwrap();
    assert!(settled_link.get("privacy_recovery").is_none());
    assert!(settled_link.get("privacy_recovery_format").is_none());
    let repeated = run(&["commit", "alice/app@encrypted"]);
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap().trim(), after);

    let mut rematerialize = resume_args.to_vec();
    rematerialize.push("--force");
    let resumed = run(&rematerialize);
    assert!(resumed.status.success(), "{resumed:?}");
    let previous: Value = serde_json::from_slice(&std::fs::read(&runtime_link).unwrap()).unwrap();
    let (_, successor) = previous["superseded_by"]
        .as_str()
        .unwrap()
        .split_once('/')
        .unwrap();
    let reinstalled_file = walkdir::WalkDir::new(&runtime_root)
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| {
            entry.path().extension().is_some_and(|ext| ext == "jsonl")
                && entry.path().file_stem().is_some_and(|stem| {
                    agit::adapter::session_id_from_stem(&stem.to_string_lossy()) == successor
                })
        })
        .unwrap()
        .into_path();
    let reinstalled = std::fs::read_to_string(&reinstalled_file).unwrap();
    assert!(reinstalled.contains("new private turn"));
    assert!(reinstalled.contains("new private reply"));
    assert!(!reinstalled.contains("SYNTHETIC_LOG_ONLY_EVIDENCE"));
    assert_eq!(reinstalled.contains("recovered_evidence"), legacy_commit);
    if source == runtime && !legacy_commit {
        assert!(!String::from_utf8_lossy(&resumed.stdout).contains("(lossy)"));
        assert_native_history(
            &reinstalled,
            &format!("{raw}{continuation}"),
            runtime,
            &reinstalled_file,
            &workspace,
        );
    }
    if runtime == "claude-code" && (source == runtime || legacy_commit) {
        assert!(reinstalled.contains("preserve-native"));
        assert!(reinstalled.contains("native-signature"));
    }

    let public_export = run(&["export", "alice/app@encrypted", "--privacy"]);
    assert!(
        public_export.status.success(),
        "{}",
        String::from_utf8_lossy(&public_export.stderr)
    );
    let public_export = String::from_utf8(public_export.stdout).unwrap();
    assert!(public_export.contains("new private turn"));
    assert_eq!(
        public_export.contains("historical session evidence (policy-projected)"),
        legacy_commit
    );
    for omitted in [
        "SYNTHETIC_OLD_AUTHORITY",
        "SYNTHETIC_OLD_TOOL",
        "SYNTHETIC_OLD_SHELL_OUTPUT",
        "SYNTHETIC_EXCLUDED_FILE_BODY",
        "/source/private.txt",
    ] {
        assert!(
            !public_export.contains(omitted),
            "public export disclosed {omitted}"
        );
    }
    if automatic_recovery {
        use agit::domain::session_privacy::ForkContext;
        let fork =
            |point: &str, branch: &str| run(&["fork", &format!("alice/app@{point}"), "-b", branch]);
        fork_access.store(2, Ordering::Release);
        let failed = fork(&before, "unavailable-original");
        assert!(!failed.status.success(), "{failed:?}");
        assert!(!repo.has_ref("refs/heads/unavailable-original"));
        fork_access.store(1, Ordering::Release);
        let sanitized = fork(&before, "sanitized-copy");
        assert!(sanitized.status.success(), "{sanitized:?}");
        let sanitized_meta = meta::read_at_ref(&repo, "sanitized-copy").unwrap();
        assert_eq!(
            sanitized_meta.fork_origin.as_ref().unwrap().context,
            ForkContext::Sanitized
        );
        assert_eq!(
            sanitized_meta
                .session_privacy
                .as_ref()
                .unwrap()
                .creator
                .as_deref(),
            Some("account-bob")
        );
        assert_eq!(
            storage::materialize_at(repo.root(), "sanitized-copy", meta::LOG_FILE).unwrap(),
            public_log
        );
        assert!(
            repo.show("sanitized-copy", "privacy/envelope.json")
                .is_none()
        );

        fork_access.store(0, Ordering::Release);
        let original_copy = fork(&before, "original-copy");
        assert!(original_copy.status.success(), "{original_copy:?}");
        let copied = meta::read_at_ref(&repo, "original-copy").unwrap();
        assert_ne!(copied.session, sanitized_meta.session);
        assert_eq!(
            copied.session_privacy.as_ref().unwrap().creator.as_deref(),
            Some("account-bob")
        );
        let provenance = copied.fork_origin.as_ref().unwrap();
        assert_ne!(
            copied.session,
            provenance.snapshot.as_ref().unwrap().binding.session_id
        );
        assert_ne!(
            copied.session,
            provenance
                .snapshot
                .as_ref()
                .unwrap()
                .binding
                .published_session_id
        );
        assert_eq!(provenance.context, ForkContext::Original);
        assert_eq!(provenance.commit, before);
        assert_eq!(
            provenance
                .snapshot
                .as_ref()
                .unwrap()
                .binding
                .owner_key
                .account_id,
            "account-alice"
        );
        assert_eq!(
            storage::materialize_at(repo.root(), "original-copy", meta::LOG_FILE).unwrap(),
            log
        );
        assert_eq!(
            storage::materialize_at(repo.root(), "original-copy", meta::VIEW_FILE).unwrap(),
            view
        );
        assert!(
            repo.show("original-copy", "privacy/envelope.json")
                .is_none()
        );
        assert!(copied.runtime_instances.is_empty() && copied.baseline_bytes.is_none());
        fork_access.store(1, Ordering::Release);
        let independent = fork("original-copy", "independent-copy");
        assert!(independent.status.success(), "{independent:?}");
        assert_eq!(
            storage::materialize_at(repo.root(), "independent-copy", meta::LOG_FILE).unwrap(),
            log
        );
    }
    if automatic_recovery {
        agit::hub::identity::rebind(&repo, &identity, &copy_identity).unwrap();
        repo.set_remote(&format!("{base}/bob/copy.git")).unwrap();
        std::fs::remove_file(
            repo.common_dir()
                .unwrap()
                .join("agit/privacy-sources")
                .join(format!("{}.json", identity.agent_id)),
        )
        .unwrap();
        original_access::exercise(run, &repo, &home, &base, &before, || {
            source_route.store(1, Ordering::Release)
        });
    }
    stop.store(true, Ordering::Relaxed);
    let (requests, key_requests) = server.join().unwrap();
    assert_eq!(key_requests, if automatic_recovery { 7 } else { 1 });
    assert!(
        requests
            .iter()
            .all(|path| !path.contains("/bob/copy/") && !path.contains("/carol/nested/"))
    );
}

fn claude_history(model: &str) -> String {
    let records = [
        json!({"type":"user","message":{"role":"user","content":"synthetic private session"}}),
        json!({"type":"assistant","opaque":{"retained":true},"message":{"id":"historical-message","role":"assistant","model":model,"content":[{"type":"thinking","thinking":"historical reasoning","signature":"historical-signature"},{"type":"tool_use","id":"historical-call","name":"Bash","input":{"command":"SYNTHETIC_OLD_TOOL"}}],"stop_reason":"tool_use","usage":{"input_tokens":7,"output_tokens":9}}}),
        json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"historical-call","content":"SYNTHETIC_OLD_SHELL_OUTPUT"}]}}),
        json!({"type":"assistant","message":{"role":"assistant","model":model,"content":[{"type":"tool_use","id":"private-file","name":"Read","input":{"file_path":"/source/private.txt"}}]}}),
        json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"private-file","content":"SYNTHETIC_EXCLUDED_FILE_BODY"}]}}),
        json!({"type":"assistant","message":{"role":"assistant","model":model,"content":[{"type":"text","text":"Synthetic recovery marker: ORCHID-COPPER-927"}],"stop_reason":"end_turn"}}),
    ];
    let mut parent = Value::Null;
    records
        .into_iter()
        .map(|mut record| {
            record["uuid"] = json!(uuid::Uuid::now_v7());
            if record["type"] == "assistant" {
                record["message"]["id"] = json!(format!("msg_{}", uuid::Uuid::now_v7()));
            }
            record["parentUuid"] = parent.clone();
            record["sessionId"] = json!("source-runtime");
            record["cwd"] = json!("/source/workspace");
            record["timestamp"] = json!("2026-01-01T00:00:00.000Z");
            record["isSidechain"] = json!(false);
            parent = record["uuid"].clone();
            format!("{record}\n")
        })
        .collect()
}

fn assert_native_history(
    raw: &str,
    original: &str,
    runtime: &str,
    file: &std::path::Path,
    cwd: &std::path::Path,
) {
    let physical = agit::infra::git_runtime::path_for_git(cwd.canonicalize().unwrap());
    let id = agit::adapter::session_id_from_stem(file.file_stem().unwrap().to_str().unwrap());
    uuid::Uuid::parse_str(&id).unwrap();
    let records: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for line in original.lines() {
        let mut expected: Value = serde_json::from_str(line).unwrap();
        if runtime == "claude-code" {
            expected["sessionId"] = json!(id);
            expected["cwd"] = json!(physical);
            assert!(
                records.contains(&expected),
                "missing native record: {expected}"
            );
        } else if expected["type"] == "session_meta" {
            let actual = &records[0];
            assert_eq!(
                actual["payload"]["base_instructions"],
                expected["payload"]["base_instructions"]
            );
            assert_eq!(
                actual["payload"]["approval_policy"],
                expected["payload"]["approval_policy"]
            );
            assert_eq!(actual["payload"]["id"], id);
            assert_eq!(actual["payload"]["cwd"], json!(physical));
        } else {
            assert!(
                records.contains(&expected),
                "missing native record: {expected}"
            );
        }
    }
    if runtime == "claude-code" {
        assert_eq!(records.len(), original.lines().count());
        assert_eq!(
            file.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            agit::domain::store::slug_for(&physical)
        );
    }
}

fn install_legacy_baseline(
    repo: &Repo,
    commit: &str,
    home: &std::path::Path,
    file: &std::path::Path,
    cwd: &std::path::Path,
) -> (std::path::PathBuf, Value, String) {
    let snapshot = agit::domain::privacy_recovery::RecoveredSnapshot::load(repo, commit)
        .unwrap()
        .unwrap();
    let id = file.file_stem().unwrap().to_str().unwrap();
    let physical = agit::infra::git_runtime::path_for_git(cwd.canonicalize().unwrap());
    let legacy = transcript::display::render_native(
        &snapshot.legacy_evidence_view().unwrap(),
        "claude-code",
        id,
        &physical,
    )
    .unwrap()
    .0;
    let path = home.join("store/claude-code").join(format!("{id}.json"));
    let mut claim: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    claim
        .as_object_mut()
        .unwrap()
        .remove("privacy_recovery_format");
    claim["baseline_bytes"] = json!(legacy.len());
    claim["baseline_hash"] = json!(
        digest_bytes(legacy.as_bytes())
            .strip_prefix("sha256:")
            .unwrap()
    );
    std::fs::write(file, &legacy).unwrap();
    std::fs::write(&path, claim.to_string()).unwrap();
    let link = agit::domain::link::read(&path).unwrap();
    std::fs::write(&path, format!("{}\n", link.to_json().unwrap())).unwrap();
    (path, claim, legacy)
}
