//! Review completion and consent remain separate, and destination drift invalidates publication.

use super::*;
use clap::Parser;
use std::cell::RefCell;

#[test]
fn model_completion_never_substitutes_for_final_consent() {
    for (answer, expected) in [
        (Some(true), Decision::Publish),
        (Some(false), Decision::Declined),
        (None, Decision::Declined),
    ] {
        let events = RefCell::new(Vec::new());
        let decision = confirm_publication(true, false, || {
            events.borrow_mut().push("confirmation");
            Ok(answer)
        })
        .unwrap();
        assert_eq!(decision, expected);
        assert_eq!(*events.borrow(), ["confirmation"]);
    }
    assert!(confirm_publication(true, false, || anyhow::bail!("cancelled")).is_err());
}

#[test]
fn incomplete_and_dry_run_reviews_cannot_reach_confirmation() {
    for dry_run in [false, true] {
        assert_eq!(
            confirm_publication(false, dry_run, || panic!("incomplete review must not ask"))
                .unwrap(),
            Decision::Incomplete
        );
    }
    assert_eq!(
        confirm_publication(true, true, || panic!("dry run must not ask to publish")).unwrap(),
        Decision::ReviewedOnly
    );
}

#[test]
fn audit_is_optional_and_global_yes_does_not_supply_consent() {
    for extra in [
        vec![],
        vec!["--yes"],
        vec!["--dry-run"],
        vec!["--allow-secrets"],
    ] {
        let mut argv = vec!["agit", "push", "me/qa@work", "--audit"];
        argv.extend(extra);
        let cli = crate::commands::Cli::try_parse_from(argv).unwrap();
        let crate::commands::Commands::Push(args) = cli.command.unwrap() else {
            panic!("push command")
        };
        assert!(args.audit);
        assert_eq!(
            confirm_publication(true, false, || Ok(Some(false))).unwrap(),
            Decision::Declined
        );
    }
    let cli = crate::commands::Cli::try_parse_from(["agit", "push", "me/qa@work"]).unwrap();
    let crate::commands::Commands::Push(args) = cli.command.unwrap() else {
        panic!("push command")
    };
    assert!(!args.audit);
}

#[test]
fn audit_json_is_rejected_without_changing_ordinary_push_json() {
    for version in ["1", "2"] {
        for tail in [vec!["--audit"], vec!["--audit", "--dry-run", "--yes"]] {
            let mut argv = vec![
                "agit",
                "--json",
                "--json-version",
                version,
                "push",
                "me/qa@work",
            ];
            argv.extend(tail);
            let cli = crate::commands::Cli::try_parse_from(argv).unwrap();
            assert!(
                crate::commands::json::incompatible(&cli.command.unwrap())
                    .unwrap()
                    .contains("--audit")
            );
        }
    }
    let cli =
        crate::commands::Cli::try_parse_from(["agit", "--json", "push", "me/qa@work"]).unwrap();
    assert!(crate::commands::json::incompatible(&cli.command.unwrap()).is_none());
}

fn remote() -> RemoteAgent {
    RemoteAgent {
        agent_id: "11111111-1111-4111-8111-111111111111".into(),
        owner: "me".into(),
        name: "qa".into(),
        clone_url: "https://hub.example/me/qa.git".into(),
        visibility: "private".into(),
        session_count: 0,
        updated_at: None,
        last_gist: None,
    }
}

fn intent() -> Intent {
    let remote = remote();
    Intent {
        hub: "https://hub.example".into(),
        account: "me".into(),
        owner: remote.owner.clone(),
        name: remote.name.clone(),
        url: remote.clone_url.clone(),
        visibility: remote.visibility.clone(),
        action: Action::Existing(remote),
    }
}

#[test]
fn reviewed_destination_rejects_identity_audience_namespace_and_url_drift() {
    let intent = intent();
    let original = remote();
    intent
        .verify_observed(&original, Some(&original.agent_id))
        .unwrap();
    for changed in [
        RemoteAgent {
            owner: "other".into(),
            ..original.clone()
        },
        RemoteAgent {
            name: "other".into(),
            ..original.clone()
        },
        RemoteAgent {
            visibility: "public".into(),
            ..original.clone()
        },
        RemoteAgent {
            agent_id: "22222222-2222-4222-8222-222222222222".into(),
            ..original.clone()
        },
        RemoteAgent {
            clone_url: "https://hub.example/other/qa.git".into(),
            ..original.clone()
        },
        RemoteAgent {
            clone_url: "https://foreign.example/me/qa.git".into(),
            ..original.clone()
        },
    ] {
        assert!(
            intent
                .verify_observed(&changed, Some(&original.agent_id))
                .is_err(),
            "changed destination was accepted"
        );
    }
    let destination = intent.json();
    assert_eq!(destination["hub"], "https://hub.example");
    assert_eq!(destination["account"], "me");
    assert_eq!(destination["visibility"], "private");
    assert_eq!(destination["action"], "publish");
    assert_eq!(destination["agent_id"], original.agent_id);
}

#[test]
fn create_and_copy_intents_bind_their_audience_and_source() {
    let mut intent = intent();
    intent.action = Action::Create;
    assert_eq!(intent.json()["action"], "ensure");
    assert!(intent.json()["agent_id"].is_null());
    let source = RemoteAgent {
        owner: "alice".into(),
        clone_url: "https://hub.example/alice/qa.git".into(),
        ..remote()
    };
    intent.action = Action::Copy(source.clone());
    let json = intent.json();
    assert_eq!(json["action"], "ensure_and_relocate");
    assert_eq!(json["source"]["agent_id"], source.agent_id);
    assert_eq!(json["source"]["owner"], "alice");
    assert_eq!(json["visibility"], "private");
    assert!(intent.confirmation().contains("no server history copy"));
    assert_eq!(json["repo_origins"], json!([]));
}

#[test]
fn noncanonical_or_credentialed_git_destinations_are_refused() {
    for url in [
        "https://token@hub.example/me/qa.git",
        "https://hub.example.evil/me/qa.git",
        "https://hub.example/me/../qa.git",
        "https://hub.example/me/%2e%2e/qa.git",
        "https://hub.example/me/qa.git?x=1",
        "https://hub.example/me/qa.git\n",
        "file:///tmp/receiver",
    ] {
        assert!(
            verify_url("https://hub.example", url).is_err(),
            "noncanonical destination was accepted"
        );
    }
}

#[test]
fn incomplete_inspection_and_native_failures_keep_their_exit_categories() {
    assert_eq!(
        inspection_failure_code(InspectionFailure::Configuration),
        ExitCode::Usage
    );
    assert_eq!(
        inspection_failure_code(InspectionFailure::Content),
        ExitCode::Precondition
    );
    assert_eq!(
        inspection_failure_code(InspectionFailure::Incomplete),
        ExitCode::Policy
    );
    for (stderr, expected) in [
        ("HTTP 401", ExitCode::Auth),
        ("HTTP 422", ExitCode::Policy),
        ("HTTP 503", ExitCode::Network),
    ] {
        let phase = crate::hub::git::PublicationPhase {
            attempts: vec![crate::hub::git::PublicationAttempt {
                batch: 0,
                outcome: crate::hub::git::Outcome {
                    code: 1,
                    stderr: stderr.into(),
                },
                refs: vec![],
                complete: true,
                error: None,
            }],
            unattempted: vec![],
            error: None,
        };
        let report = PublicationReport {
            lfs: None,
            heads: Some(phase),
            tags: None,
            error: None,
        };
        assert!(!report.ok());
        assert_eq!(publication_failure_code(&report), expected);
    }
}

#[test]
fn inherited_git_routing_is_refused_without_reporting_its_value() {
    for key in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_SHALLOW_FILE",
        "GIT_GRAFT_FILE",
        "GIT_PREFIX",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
    ] {
        let error = check_environment([(key.into(), "PRIVATE_ROUTING_VALUE".into())]).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "push --audit cannot inherit {key}; clear Git routing and injected configuration before retrying"
            )
        );
        assert!(!error.to_string().contains("PRIVATE_ROUTING_VALUE"));
    }
    assert!(check_environment([("GIT_CONFIG_COUNT".into(), "0".into())]).is_err());
    assert!(
        check_environment([
            ("GIT_CONFIG_GLOBAL".into(), "retained-config".into()),
            ("AGIT_SESSION".into(), "me/qa@work".into())
        ])
        .is_ok()
    );
    assert_eq!(
        check_environment([("Git_Dir".into(), "private-route".into())]).is_err(),
        cfg!(windows)
    );
    assert_eq!(
        check_environment([("gIt_cOnFiG_kEy_0".into(), "private-route".into())]).is_err(),
        cfg!(windows)
    );
}

#[test]
fn ensured_destinations_accept_a_fresh_same_target_id_but_existing_targets_do_not() {
    let mut intent = intent();
    let original = remote();
    let concurrent = RemoteAgent {
        agent_id: "22222222-2222-4222-8222-222222222222".into(),
        ..original.clone()
    };
    intent.action = Action::Create;
    intent.verify_observed(&concurrent, None).unwrap();
    assert!(
        intent
            .confirmation()
            .contains("Ensure the destination exists with this audience")
    );
    assert_eq!(intent.json()["repo_origins"], json!([]));
    assert!(
        intent
            .verify_observed(
                &RemoteAgent {
                    visibility: "public".into(),
                    ..concurrent.clone()
                },
                None
            )
            .is_err()
    );
    intent.action = Action::Existing(original.clone());
    assert!(
        intent
            .verify_observed(&concurrent, Some(&original.agent_id))
            .is_err()
    );
}

use crate::publication_test_process as process;

/// Local relocation preserves every ref and claim field without reading or copying remote history.
#[test]
fn prepared_copy_relocates_only_local_content_and_exact_source_claims() {
    use crate::domain::{link, store::Store};
    use std::process::Command;
    use std::time::Instant;

    const CHILD: &str = "AGIT_AUDITED_LOCAL_PROMOTION_FIXTURE";
    const COMPLETE: &str = "prepared local promotion verified";
    let Some(root) = std::env::var_os(CHILD) else {
        let root = tempfile::tempdir().unwrap();
        for name in ["home", "agit", "tmp", "templates"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.env_clear();
        for key in ["PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let module = module_path!().split_once("::").unwrap().1;
        command
            .args([
                "--exact",
                &format!(
                    "{module}::prepared_copy_relocates_only_local_content_and_exact_source_claims"
                ),
                "--nocapture",
            ])
            .env(CHILD, root.path())
            .env("HOME", root.path().join("home"))
            .env("USERPROFILE", root.path().join("home"))
            .env("AGIT_HOME", root.path().join("agit"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.path().join("empty-config"))
            .env("GIT_TEMPLATE_DIR", root.path().join("templates"))
            .env("GIT_AUTHOR_NAME", "Audit promotion fixture")
            .env("GIT_AUTHOR_EMAIL", "audit@example.invalid")
            .env("GIT_COMMITTER_NAME", "Audit promotion fixture")
            .env("GIT_COMMITTER_EMAIL", "audit@example.invalid")
            .env("TMP", root.path().join("tmp"))
            .env("TEMP", root.path().join("tmp"))
            .env("TMPDIR", root.path().join("tmp"))
            .current_dir(root.path());
        let output = process::output(
            command,
            "audit promotion",
            "isolated child",
            Instant::now() + process::MODE_LIMIT,
        )
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8(output.stdout).unwrap().contains(COMPLETE));
        return;
    };
    let root = std::path::PathBuf::from(root);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let source = RemoteAgent {
        owner: "alice".into(),
        clone_url: format!("{hub}/alice/qa.git"),
        ..remote()
    };
    let destination = RemoteAgent {
        agent_id: "22222222-2222-4222-8222-222222222222".into(),
        clone_url: format!("{hub}/me/qa.git"),
        ..remote()
    };
    let source_identity = RemoteIdentity::new(&hub, &source.agent_id).unwrap();
    let destination_identity = RemoteIdentity::new(&hub, &destination.agent_id).unwrap();
    let source_path = config::repo_dir(&source.owner, &source.name).unwrap();
    let destination_path = config::repo_dir(&destination.owner, &destination.name).unwrap();
    let repo = Repo::init(&source_path).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    std::fs::write(source_path.join("payload.txt"), "reviewed local content\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("Create local audit fixture").unwrap();
    repo.git(&["branch", "selected"]).unwrap();
    repo.git(&["tag", "retained", "HEAD"]).unwrap();
    repo.set_remote(&source.clone_url).unwrap();
    identity::pin(&repo, &source_identity).unwrap();
    std::fs::write(source_path.join("uncommitted.txt"), "local work survives\n").unwrap();
    let refs = repo.git(&["show-ref"]).unwrap();
    let original_config = std::fs::read(source_path.join(".git/config")).unwrap();
    let store = Store::open_or_init().unwrap();
    let mut selected = link::Link::new("claude-code", "selected", Some(&root));
    selected.owner = Some(source.owner.clone());
    selected.agent = Some(source.name.clone());
    selected.branch = Some("selected".into());
    selected.baseline_bytes = Some(41);
    selected.baseline_hash = Some("fixture baseline".into());
    selected.materialized_from = Some(repo.git(&["rev-parse", "HEAD"]).unwrap());
    link::write(&store, &selected).unwrap();
    let mut other = selected.clone();
    other.session_id = "other".into();
    other.owner = Some("unrelated".into());
    link::write(&store, &other).unwrap();
    let other_before = other.to_json().unwrap();
    let mut legacy = selected.clone();
    legacy.session_id = "legacy".into();
    legacy.owner = None;
    link::write(&store, &legacy).unwrap();
    let legacy_before = legacy.to_json().unwrap();

    Repo::init(&destination_path).unwrap();
    assert!(
        super::super::super::clone::promote_to_prepared_destination(
            &source_path,
            &source,
            &destination,
            &hub
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(source_path.join(".git/config")).unwrap(),
        original_config
    );
    assert_eq!(repo.git(&["show-ref"]).unwrap(), refs);
    assert_eq!(
        link::get(&store, "claude-code", "selected")
            .unwrap()
            .to_json()
            .unwrap(),
        selected.to_json().unwrap()
    );
    std::fs::remove_dir_all(&destination_path).unwrap();

    let changed_source = RemoteAgent {
        agent_id: "33333333-3333-4333-8333-333333333333".into(),
        ..source.clone()
    };
    assert!(
        super::super::super::clone::promote_to_prepared_destination(
            &source_path,
            &changed_source,
            &destination,
            &hub
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(source_path.join(".git/config")).unwrap(),
        original_config
    );
    let plan = super::super::super::clone::promote_to_prepared_destination(
        &source_path,
        &source,
        &destination,
        &hub,
    )
    .unwrap();
    assert!(plan.writable && plan.promoted_in_place);
    assert_eq!(plan.identity, destination_identity);
    assert!(!source_path.exists());
    let moved = Repo::open(&destination_path).unwrap();
    assert_eq!(moved.git(&["show-ref"]).unwrap(), refs);
    assert_eq!(identity::read(&moved).unwrap(), Some(destination_identity));
    assert_eq!(
        moved.remote_url().as_deref(),
        Some(destination.clone_url.as_str())
    );
    assert_eq!(
        moved.upstream_url().as_deref(),
        Some(source.clone_url.as_str())
    );
    assert_eq!(
        std::fs::read_to_string(destination_path.join("uncommitted.txt")).unwrap(),
        "local work survives\n"
    );
    selected.owner = Some(destination.owner);
    selected.agent = Some(destination.name);
    assert_eq!(
        link::get(&store, "claude-code", "selected")
            .unwrap()
            .to_json()
            .unwrap(),
        selected.to_json().unwrap()
    );
    assert_eq!(
        link::get(&store, "claude-code", "other")
            .unwrap()
            .to_json()
            .unwrap(),
        other_before
    );
    assert_eq!(
        link::get(&store, "claude-code", "legacy")
            .unwrap()
            .to_json()
            .unwrap(),
        legacy_before
    );
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    println!("{COMPLETE}");
}
