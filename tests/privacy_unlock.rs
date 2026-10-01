#![cfg(feature = "cli")]

//! Unlock, resume and settlement preserve native history and its selected context.

use agit::domain::{
    meta,
    privacy_envelope::{PrivacyEnvelope, ViewingRecipient, digest_bytes},
    privacy_layer::PrivateLayer,
    repo::Repo,
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
#[path = "support/privacy_password_terminal.rs"]
mod privacy_password_terminal;
#[path = "support/publication_identity.rs"]
mod publication_identity;
#[path = "support/startup_cache.rs"]
mod startup_cache;
use privacy_password_terminal::terminal_password;

#[test]
fn anonymous_password_unlock_preserves_native_codex_history_through_settlement() {
    recovery_roundtrip("codex", "codex", false);
}

#[test]
fn claude_unlock_resume_preserves_native_history_and_upgrades_legacy_baselines() {
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
    let layer = PrivateLayer::new(
        &saved,
        &saved,
        serde_json::to_value(meta::read(repo.root()).unwrap()).unwrap(),
        BTreeMap::new(),
    )
    .unwrap();
    let key = SecretKey::from([13; 32]);
    let public = STANDARD.encode(key.public_key().as_bytes());
    let recipient =
        ViewingRecipient::from_base64(agit::domain::privacy_key::recipient_id(&public), &public)
            .unwrap();
    let envelope = PrivacyEnvelope::seal_layer(
        digest_bytes(b"policy"),
        digest_bytes(b"snapshot"),
        json!({}),
        &layer,
        &recipient,
        Vec::new(),
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
    let metadata = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        source.into(),
        "/source/workspace".into(),
    );
    let mut layer = PrivateLayer::new(
        &log,
        &view,
        serde_json::to_value(metadata).unwrap(),
        BTreeMap::new(),
    )
    .unwrap();
    layer
        .protected_values
        .insert("synthetic private session".into());
    layer.protected_values.extend([
        "SYNTHETIC_OLD_AUTHORITY".into(),
        "SYNTHETIC_OLD_TOOL".into(),
        "SYNTHETIC_OLD_SHELL_OUTPUT".into(),
    ]);
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/privacy-web-key.json")).unwrap();
    let record = fixture["record"].clone();
    let key = SecretKey::from_slice(
        &STANDARD
            .decode(fixture["private_key"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    let public = STANDARD.encode(key.public_key().as_bytes());
    let recipient =
        ViewingRecipient::from_base64(agit::domain::privacy_key::recipient_id(&public), &public)
            .unwrap();
    let envelope = PrivacyEnvelope::seal_layer(
        digest_bytes(b"policy"),
        digest_bytes(b"snapshot"),
        json!({"content":"sanitized", "metadata":{"session":format!("agit-{}", "b".repeat(40))}}),
        &layer,
        &recipient,
        Vec::new(),
    )
    .unwrap();
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

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let identity =
        agit::hub::identity::RemoteIdentity::new(&base, "00000000-0000-0000-0000-000000000001")
            .unwrap();
    agit::hub::identity::pin(&repo, &identity).unwrap();
    repo.set_remote(&format!("{base}/alice/app.git")).unwrap();
    let expected_commit = before.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let response_hub = base.clone();
    let server = std::thread::spawn(move || {
        let started = Instant::now();
        let mut requests = Vec::new();
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
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            requests.push(path.clone());
            let (status, response) = if path.contains("/privacy/keys/") {
                assert!(!authenticated, "public password unlock must be anonymous");
                assert_eq!(
                    path,
                    format!(
                        "/api/agents/alice/app/privacy/keys/{}?ref={expected_commit}",
                        record["recipient"].as_str().unwrap()
                    )
                );
                (
                    200,
                    json!({"agent_id":identity.agent_id,"commit":expected_commit,"session_id":format!("agit-{}", "b".repeat(40)),"key":record}),
                )
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
            write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        }
        requests
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
    let noninteractive = run(&unlock_args);
    assert!(!noninteractive.status.success());
    assert!(String::from_utf8_lossy(&noninteractive.stderr).contains("interactive terminal"));
    let (status, output) = terminal_password(
        &home,
        temp.path(),
        &workspace,
        &base,
        &unlock_args,
        fixture["password"].as_str().unwrap(),
    );
    assert!(status.success(), "{output}");
    assert!(!output.contains(fixture["password"].as_str().unwrap()));
    assert!(!output.contains("browser"));
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap().trim(), before);
    let restored = repo
        .common_dir()
        .unwrap()
        .join("agit/privacy-recovery")
        .join(&before);
    assert_eq!(
        storage::materialize_worktree(&restored, meta::VIEW_FILE).unwrap(),
        view
    );
    assert_eq!(
        meta::read(&restored).unwrap().cwd,
        workspace.canonicalize().unwrap().to_string_lossy()
    );
    let key_text = fixture["private_key"].as_str().unwrap();
    for file in walkdir::WalkDir::new(&restored) {
        let file = file.unwrap();
        if file.file_type().is_file() {
            assert!(
                !String::from_utf8_lossy(&std::fs::read(file.path()).unwrap()).contains(key_text)
            );
        }
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
    let resumed = run(&resume_args);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
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
    let anonymous_commit = run(&["commit", "alice/app@encrypted"]);
    assert!(!anonymous_commit.status.success());
    assert!(
        String::from_utf8_lossy(&anonymous_commit.stderr).contains("sign in"),
        "{anonymous_commit:?}"
    );
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    std::fs::write(repo.root().join("README.md"), "Local continuation notes\n").unwrap();
    let files = run(&[
        "commit",
        "alice/app@encrypted",
        "-m",
        "Add local notes",
        "--",
        "README.md",
    ]);
    assert!(
        files.status.success(),
        "{}",
        String::from_utf8_lossy(&files.stderr)
    );
    let settlement_before = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
    assert_ne!(settlement_before, before);
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
    let after = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
    assert_ne!(
        after,
        before,
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&committed.stdout),
        String::from_utf8_lossy(&committed.stderr)
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
    let viewing = SecretKey::from([13; 32]);
    let sealed = run(&[
        "export",
        "alice/app@encrypted",
        "--format",
        "privacy-envelope",
        "--viewing-public-key",
        &STANDARD.encode(viewing.public_key().as_bytes()),
    ]);
    assert!(
        sealed.status.success(),
        "{}",
        String::from_utf8_lossy(&sealed.stderr)
    );
    let sealed = PrivacyEnvelope::parse(&sealed.stdout).unwrap();
    let recovered = sealed.open_layer(&viewing).unwrap();
    let (private_log, private_view) = recovered.session_bytes().unwrap();
    let dictionary = agit::domain::secret_filter::RepositoryDictionary::open(repo.root()).unwrap();
    let local_log = dictionary
        .hydrate_envelopes_readonly(&committed_log)
        .unwrap()
        .text;
    let local_view = dictionary
        .hydrate_envelopes_readonly(&committed_view)
        .unwrap()
        .text;
    assert!(private_log.starts_with(&log));
    assert_eq!(private_log.as_str(), local_log);
    assert!(private_view.contains("synthetic private session"));
    assert_eq!(private_view.as_str(), local_view);
    assert_eq!(
        transcript::unwrap_strict(sealed.public_projection["session"]["log"].as_str().unwrap())
            .unwrap(),
        public_export
    );
    stop.store(true, Ordering::Relaxed);
    let requests = server.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|path| path.contains("/privacy/keys/"))
            .count(),
        2
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
    (path, claim, legacy)
}
