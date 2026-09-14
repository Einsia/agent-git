#[cfg(feature = "cli")]
mod frozen_publication_http {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/hub/git/prepared_publication_tests.rs"
    ));
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/hub/git/publication_execution_tests.rs"
    ));
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/hub/git/captured_publication_tests.rs"
    ));
    use super::*;
    use crate::domain::repo::{Repo, publication::PublicationPlan};
    use crate::hub::git::{FrozenPublication, PublicationStatus};
    use std::process::{Command, Stdio};

    const CHILD: &str = "AGIT_FROZEN_HTTP_TEST_ROOT";
    const COMPLETE: &str = "frozen HTTP boundary verified";
    const PUSH_PATH: &str = "/alice/example.git/info/refs?service=git-receive-pack";

    fn isolated(name: &str) -> bool {
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let home = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.env_clear();
        for name in [
            "PATH",
            "SystemRoot",
            "WINDIR",
            "ComSpec",
            "PATHEXT",
            "AGIT_TEST_REQUIRE_LFS",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        for path in ["tmp", "templates"] {
            std::fs::create_dir(home.path().join(path)).unwrap();
        }
        let output = command
            .args([
                "--exact",
                &format!(
                    "hub::git::git_credential_lifecycle_tests::frozen_publication_http::{name}"
                ),
                "--nocapture",
            ])
            .env(CHILD, home.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("TMP", home.path().join("tmp"))
            .env("TEMP", home.path().join("tmp"))
            .env("TMPDIR", home.path().join("tmp"))
            .env("GIT_TEMPLATE_DIR", home.path().join("templates"))
            .env("GIT_ALLOW_PROTOCOL", "http")
            .env("GIT_AUTHOR_NAME", "Frozen publication fixture")
            .env("GIT_AUTHOR_EMAIL", "publication@example.invalid")
            .env("GIT_COMMITTER_NAME", "Frozen publication fixture")
            .env("GIT_COMMITTER_EMAIL", "publication@example.invalid")
            .current_dir(home.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "isolated HTTP fixture: {output:?}");
        assert!(String::from_utf8(output.stdout).unwrap().contains(COMPLETE));
        false
    }

    fn source(home: &IsolatedHome, hub: &str) -> (Repo, PublicationPlan, String, RemoteIdentity) {
        config::set_global("hub.url", Some(hub)).unwrap();
        credentials::save(hub, &pair(hub, "alice")).unwrap();
        let repo = Repo::init(home.workspace()).unwrap();
        repo.git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "--no-gpg-sign",
            "-m",
            "Frozen publication fixture",
        ])
        .unwrap();
        let branch = repo.git(&["symbolic-ref", "--short", "HEAD"]).unwrap();
        let plan = PublicationPlan::freeze(&repo, &[branch.trim().to_string()]).unwrap();
        for (key, value) in [
            ("credential.helper", ""),
            ("http.proxy", ""),
            ("http.lowSpeedLimit", "1"),
            ("http.lowSpeedTime", "3"),
            ("http.userAgent", "frozen-publication-fixture"),
            ("protocol.version", "0"),
        ] {
            repo.git(&["config", "--local", key, value]).unwrap();
        }
        let url = format!("{hub}/alice/example.git");
        repo.git(&[
            "config",
            "--local",
            &format!("http.{url}.extraHeader"),
            "X-Unapproved: synthetic",
        ])
        .unwrap();
        let identity = RemoteIdentity::new(hub, AGENT_ID).unwrap();
        (repo, plan, url, identity)
    }

    fn rewrite(root: &Path, url: &str, destination: &str) {
        let repo = Repo::at(root);
        for suffix in ["insteadOf", "pushInsteadOf"] {
            repo.git(&[
                "config",
                "--local",
                &format!("url.{destination}.{suffix}"),
                url,
            ])
            .unwrap();
        }
        repo.git(&[
            "config",
            "--local",
            "http.userAgent",
            "changed-after-preparation",
        ])
        .unwrap();
    }

    fn assert_request(request: &WireRequest, path: &str, token: &str) {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, path);
        assert_eq!(
            request.header("Authorization"),
            Some(format!("Bearer {token}").as_str())
        );
        assert_eq!(
            request.header("X-AgentGit-Expected-Agent-Id"),
            Some(AGENT_ID)
        );
        assert_eq!(request.header("X-Unapproved"), None);
        assert_eq!(request.header("X-AgentGit-Accept-Secret-Findings"), None);
        assert_eq!(
            request.header("User-Agent"),
            Some("frozen-publication-fixture")
        );
        assert!(request.body.is_empty());
    }

    fn borrowed_execution(repo: &Repo, value: &str) -> crate::hub::git::frozen::Execution {
        let root =
            crate::domain::repo::inspection_git_path_spelling(repo.root().canonicalize().unwrap());
        let mut environment: std::collections::BTreeMap<_, _> = std::env::vars_os().collect();
        for name in [
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            environment.remove(std::ffi::OsStr::new(name));
        }
        environment.insert("GIT_DIR".into(), root.join(".git").into_os_string());
        crate::hub::git::frozen::Execution::fixture(
            root,
            environment,
            inherited_parameters(&[
                ("boundary.parameter", value),
                ("http.userAgent", value),
                ("http.proxy", ""),
                ("protocol.version", "0"),
            ]),
        )
    }

    #[test]
    fn borrowed_execution_binds_command_parameters_and_limits_without_replacing_default() {
        if !isolated(
            "borrowed_execution_binds_command_parameters_and_limits_without_replacing_default",
        ) {
            return;
        }
        use crate::hub::git::{
            OutputMode, TransportIdentity, execute_transport, execute_transport_in,
        };
        let home = IsolatedHome::new();
        let original = Repo::init(&home.workspace().join("original")).unwrap();
        let selected = Repo::init(&home.workspace().join("selected")).unwrap();
        for (repo, value) in [(&original, "original"), (&selected, "selected")] {
            repo.git(&["config", "boundary.command", value]).unwrap();
        }
        let transport = TransportIdentity {
            client: None,
            urls: Vec::new(),
            agent_id: None,
            execution: Some(borrowed_execution(&original, "original")),
            lfs: None,
            accept_secret_findings: false,
        };
        let selected = borrowed_execution(&selected, "selected");
        let args = ["config", "--get-regexp", "^boundary[.]"];
        let check = |run: crate::hub::git::TransportRun, expected: &str| {
            assert!(run.error.is_none());
            assert_eq!(run.attempts.len(), 1);
            let output = &run.attempts[0];
            assert!(output.complete && output.error.is_none() && output.outcome.ok());
            let text = std::str::from_utf8(&output.stdout).unwrap();
            let values: std::collections::BTreeMap<_, _> = text
                .lines()
                .map(|line| line.split_once(' ').unwrap())
                .collect();
            assert_eq!(
                values,
                std::collections::BTreeMap::from([
                    ("boundary.command", expected),
                    ("boundary.parameter", expected),
                ])
            );
        };
        check(
            execute_transport_in(
                None,
                &args,
                &transport,
                OutputMode::Captured,
                Some(&selected),
            ),
            "selected",
        );
        check(
            execute_transport(None, &args, &transport, OutputMode::Captured),
            "original",
        );
        let mut oversized = borrowed_execution(&original, "oversized");
        oversized
            .parameters
            .push(format!(" 'boundary.large'='{}'", "x".repeat(28 * 1024)));
        let refused = execute_transport_in(
            None,
            &args,
            &transport,
            OutputMode::Captured,
            Some(&oversized),
        );
        assert!(refused.attempts.is_empty());
        assert!(
            refused
                .error
                .unwrap()
                .to_string()
                .contains("environment limit"),
            "the selected configuration must be checked before process creation"
        );
        check(
            execute_transport(None, &args, &transport, OutputMode::Captured),
            "original",
        );
        println!("{COMPLETE}");
    }

    #[test]
    fn borrowed_execution_retry_updates_the_retained_client_for_following_transport() {
        if !isolated("borrowed_execution_retry_updates_the_retained_client_for_following_transport")
        {
            return;
        }
        use crate::hub::git::{
            OutputMode, TransportIdentity, execute_transport, execute_transport_in,
        };
        let home = IsolatedHome::new();
        let mut attempts = 0;
        let hub = FakeHub::new(move |request| {
            if request.path == GIT_PATH {
                attempts += 1;
                return if attempts == 1 {
                    denied()
                } else {
                    advertisement()
                };
            }
            assert_eq!(request.path, "/api/auth/refresh");
            refreshed()
        });
        let (repo, _plan, url, identity) = source(&home, &hub.base);
        let selected = Repo::init(&home.workspace().join("borrowed")).unwrap();
        let selected = borrowed_execution(&selected, "borrowed-execution");
        let args = ["ls-remote", "--heads", url.as_str()];
        let mut transport = TransportIdentity::new(Some(repo.root()), &args, &identity).unwrap();
        transport.execution = Some(borrowed_execution(&repo, "default-execution"));
        let run = execute_transport_in(
            None,
            &args,
            &transport,
            OutputMode::Captured,
            Some(&selected),
        );
        assert!(run.error.is_none());
        assert_eq!(run.attempts.len(), 2);
        assert!(!run.attempts[0].outcome.ok());
        assert!(run.attempts[1].outcome.ok());
        assert!(
            run.attempts
                .iter()
                .all(|attempt| attempt.complete && attempt.error.is_none())
        );
        assert_eq!(
            transport.token().unwrap().as_deref(),
            Some("fake-alice-fresh-access")
        );
        let following = execute_transport(None, &args, &transport, OutputMode::Captured);
        assert!(following.error.is_none());
        assert_eq!(following.attempts.len(), 1);
        assert!(following.attempts[0].outcome.ok());
        let requests = hub.finish();
        assert_eq!(requests.len(), 4);
        assert_git_request(&requests[0], Some("fake-alice-access"), true);
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/api/auth/refresh");
        assert_eq!(requests[1].header("Authorization"), None);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[1].body).unwrap(),
            serde_json::json!({"refresh_token": "fake-alice-refresh"})
        );
        assert_git_request(&requests[2], Some("fake-alice-fresh-access"), true);
        assert_git_request(&requests[3], Some("fake-alice-fresh-access"), true);
        for request in [&requests[0], &requests[2]] {
            assert_eq!(request.header("User-Agent"), Some("borrowed-execution"));
            assert_eq!(request.header("X-Unapproved"), None);
        }
        assert_eq!(requests[3].header("User-Agent"), Some("default-execution"));
        println!("{COMPLETE}");
    }

    #[test]
    fn advertisement_retry_keeps_prepared_target_and_login() {
        if !isolated("advertisement_retry_keeps_prepared_target_and_login") {
            return;
        }
        let home = IsolatedHome::new();
        let other = FakeHub::new(|_| denied());
        let other_base = other.base.clone();
        let source_root = home.workspace().to_path_buf();
        let mut attempts = 0;
        let hub = FakeHub::new(move |request| {
            if request.path == GIT_PATH {
                attempts += 1;
                if attempts == 1 {
                    let base = format!("http://{}", request.header("host").unwrap());
                    rewrite(
                        &source_root,
                        &format!("{base}/alice/example.git"),
                        &format!("{other_base}/other/repo.git"),
                    );
                    config::set_global("hub.url", Some(&other_base)).unwrap();
                    return denied();
                }
                return advertisement();
            }
            assert_eq!(request.path, "/api/auth/refresh");
            refreshed()
        });
        let (repo, plan, url, identity) = source(&home, &hub.base);
        credentials::save(&other.base, &pair(&other.base, "bob")).unwrap();
        let other_credentials = config::credentials_path(&other.base).unwrap();
        let before = std::fs::read(&other_credentials).unwrap();
        let publication = FrozenPublication::prepare(&repo, &plan, &url, &identity).unwrap();
        let advertised = publication.advertised_refs();
        let requests = hub.finish();
        let advertised = advertised.unwrap_or_else(|| {
            let paths: Vec<_> = requests
                .iter()
                .map(|request| request.path.as_str())
                .collect();
            panic!("authenticated advertisement must complete; observed request paths: {paths:?}");
        });
        assert_eq!(advertised.heads, vec![FAKE_OID.to_string()]);
        assert!(advertised.tags.is_empty());
        assert_eq!(std::fs::read(other_credentials).unwrap(), before);
        assert_eq!(requests.len(), 3);
        assert_request(&requests[0], GIT_PATH, "fake-alice-access");
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/api/auth/refresh");
        assert_eq!(requests[1].header("Authorization"), None);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[1].body).unwrap(),
            serde_json::json!({"refresh_token": "fake-alice-refresh"})
        );
        assert_request(&requests[2], GIT_PATH, "fake-alice-fresh-access");
        assert!(other.finish().is_empty());
        println!("{COMPLETE}");
    }

    #[test]
    fn push_retry_keeps_prepared_endpoint_and_preserves_failure() {
        if !isolated("push_retry_keeps_prepared_endpoint_and_preserves_failure") {
            return;
        }
        let home = IsolatedHome::new();
        let source_root = home.workspace().to_path_buf();
        let mut attempts = 0;
        let hub = FakeHub::new(move |request| {
            if request.path == PUSH_PATH {
                attempts += 1;
                if attempts == 1 {
                    let base = format!("http://{}", request.header("host").unwrap());
                    rewrite(
                        &source_root,
                        &format!("{base}/alice/example.git"),
                        &format!("{base}/other/repo.git"),
                    );
                }
                return denied();
            }
            assert_eq!(request.path, "/api/auth/refresh");
            refreshed()
        });
        let (repo, plan, url, identity) = source(&home, &hub.base);
        let publication = FrozenPublication::prepare(&repo, &plan, &url, &identity).unwrap();
        let result = publication.push_refs();
        assert!(!(result.0.ok() && result.1.as_ref().is_some_and(|phase| phase.ok())));
        assert!(result.1.is_none());
        assert_eq!(result.0.attempts.len(), 2);
        for attempt in &result.0.attempts {
            assert_eq!(attempt.batch, 0);
            assert!(!attempt.ok());
            assert!(crate::hub::git::looks_like_auth_failure(
                &attempt.outcome.stderr
            ));
            assert!(!attempt.outcome.stderr.contains("fake-alice-access"));
            assert!(!attempt.outcome.stderr.contains("fake-alice-fresh-access"));
            assert_eq!(attempt.refs.len(), plan.heads().len());
            for (observed, expected) in attempt.refs.iter().zip(plan.heads()) {
                assert_eq!(&observed.reference, expected);
                assert_eq!(observed.status, PublicationStatus::Unconfirmed);
            }
        }
        let requests = hub.finish();
        assert_eq!(requests.len(), 3);
        assert_request(&requests[0], PUSH_PATH, "fake-alice-access");
        assert_eq!(requests[1].path, "/api/auth/refresh");
        assert_request(&requests[2], PUSH_PATH, "fake-alice-fresh-access");
        println!("{COMPLETE}");
    }

    #[test]
    fn push_refuses_a_different_login_during_retry() {
        if !isolated("push_refuses_a_different_login_during_retry") {
            return;
        }
        let home = IsolatedHome::new();
        let saved_by_login = Arc::new(std::sync::Mutex::new(None));
        let snapshot = saved_by_login.clone();
        let hub = FakeHub::new(move |request| {
            assert_eq!(request.path, PUSH_PATH);
            let base = format!("http://{}", request.header("host").unwrap());
            credentials::save(&base, &pair(&base, "bob")).unwrap();
            *snapshot.lock().unwrap() =
                Some(std::fs::read(config::credentials_path(&base).unwrap()).unwrap());
            denied()
        });
        let (repo, plan, url, identity) = source(&home, &hub.base);
        let publication = FrozenPublication::prepare(&repo, &plan, &url, &identity).unwrap();
        let result = publication.push_refs();
        assert!(!(result.0.ok() && result.1.as_ref().is_some_and(|phase| phase.ok())));
        assert!(result.1.is_none());
        assert_eq!(result.0.attempts.len(), 1);
        assert!(crate::hub::git::looks_like_auth_failure(
            &result.0.attempts[0].outcome.stderr
        ));
        assert_eq!(
            std::fs::read(config::credentials_path(&hub.base).unwrap()).unwrap(),
            saved_by_login.lock().unwrap().as_ref().unwrap().clone()
        );
        let saved = credentials::load_checked(&hub.base).unwrap().unwrap();
        assert_eq!(saved.username, "bob");
        let requests = hub.finish();
        assert_eq!(requests.len(), 1);
        assert_request(&requests[0], PUSH_PATH, "fake-alice-access");
        println!("{COMPLETE}");
    }

    #[test]
    fn advertisement_refuses_redirects_to_another_repository() {
        if !isolated("advertisement_refuses_redirects_to_another_repository") {
            return;
        }
        let home = IsolatedHome::new();
        let other = FakeHub::new(|_| advertisement());
        let location = format!("{}{GIT_PATH}", other.base);
        let hub = FakeHub::new(move |_| Reply {
            status: 302,
            content_type: "text/plain",
            body: Vec::new(),
            headers: vec![("Location".into(), location.clone())],
        });
        let (repo, plan, url, identity) = source(&home, &hub.base);
        repo.git(&["config", "--local", "http.followRedirects", "true"])
            .unwrap();
        let publication = FrozenPublication::prepare(&repo, &plan, &url, &identity).unwrap();
        assert!(publication.advertised_refs().is_none());
        let requests = hub.finish();
        assert_eq!(requests.len(), 1);
        assert_request(&requests[0], GIT_PATH, "fake-alice-access");
        assert!(other.finish().is_empty());
        println!("{COMPLETE}");
    }

    #[test]
    fn user_agent_cannot_inject_headers_before_publication() {
        if !isolated("user_agent_cannot_inject_headers_before_publication") {
            return;
        }
        let home = IsolatedHome::new();
        let hub = FakeHub::new(|_| advertisement());
        let (repo, plan, url, identity) = source(&home, &hub.base);
        for value in [
            "fixture\r\nX-Unapproved: injected",
            "fixture\nHost: other.example.invalid",
            "fixture\rAuthorization: synthetic",
        ] {
            repo.git(&["config", "--local", "http.userAgent", value])
                .unwrap();
            assert!(
                FrozenPublication::prepare(&repo, &plan, &url, &identity).is_err(),
                "a user agent cannot introduce independent request headers"
            );
        }
        assert!(hub.finish().is_empty());
        println!("{COMPLETE}");
    }

    #[test]
    fn supervised_identity_comes_from_the_selected_repository() {
        if !isolated("supervised_identity_comes_from_the_selected_repository") {
            return;
        }
        let home = IsolatedHome::new();
        let hub = FakeHub::new(|_| advertisement());
        let (repo, plan, url, identity) = source(&home, &hub.base);
        crate::hub::identity::pin(&repo, &identity).unwrap();
        let other_root = home.workspace().join("other-repository");
        let other = Repo::init(&other_root).unwrap();
        let other_identity =
            RemoteIdentity::new(&hub.base, "00000000-0000-0000-0000-000000000002").unwrap();
        crate::hub::identity::pin(&other, &other_identity).unwrap();
        // The isolated child and environment lock outlive the source identity observation.
        unsafe {
            std::env::set_var("GIT_DIR", other_root.join(".git"));
            std::env::set_var("AGIT_EXPECTED_AGENT_ID", &other_identity.agent_id);
        }
        assert!(
            FrozenPublication::prepare(&repo, &plan, &url, &other_identity).is_err(),
            "an ambient repository cannot supply the selected source's supervised identity"
        );
        assert!(hub.finish().is_empty());
        println!("{COMPLETE}");
    }
}
