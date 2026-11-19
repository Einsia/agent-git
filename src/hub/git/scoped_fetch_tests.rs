/// A credential retry rediscovers its scope, and unavailable selected refs cannot reuse local state.
#[test]
fn scoped_branch_pickup_binds_discovery_and_git_retries_to_one_reader() {
    use crate::domain::repo::Repo;
    use base64::Engine;
    let home = IsolatedHome::new();
    let source = Repo::init(&home.workspace().join("scoped-source")).unwrap();
    source
        .git(&["config", "user.name", "Synthetic Reader"])
        .unwrap();
    source
        .git(&["config", "user.email", "reader@example.invalid"])
        .unwrap();
    source
        .git(&["symbolic-ref", "HEAD", "refs/heads/main"])
        .unwrap();
    source
        .git(&[
            "commit",
            "--allow-empty",
            "-m",
            "synthetic selected content",
        ])
        .unwrap();
    source.git(&["branch", "selected"]).unwrap();
    source.git(&["tag", "historical-unselected"]).unwrap();
    let oid = source
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    let selected_oid = Arc::new(std::sync::Mutex::new(oid.clone()));
    let observed_oid = Arc::clone(&selected_oid);
    let main_oid = oid.clone();
    let source_path = source.root().to_owned();
    let available = Arc::new(AtomicBool::new(true));
    let selected_available = Arc::clone(&available);
    let mut discoveries = 0;
    let mut denied_git = false;
    let hub = FakeHub::with_config(
        serde_json::json!({"capabilities":{"git_fetch_scope":[1]}}),
        move |request| {
            if request.path == "/api/auth/refresh" {
                assert!(denied_git);
                assert_eq!(request.header(super::scoped_fetch::HEADER), None);
                return refreshed();
            }
            assert_eq!(
                request.header("X-AgentGit-Expected-Agent-Id"),
                Some(AGENT_ID)
            );
            if request.path.ends_with("/agit-refs") {
                discoveries += 1;
                let expected_oid = observed_oid.lock().unwrap().clone();
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["repository_id"], AGENT_ID);
                assert_eq!(
                    body["refs"],
                    serde_json::json!(["refs/heads/main", "refs/heads/selected"])
                );
                assert_eq!(body["reader_context"], serde_json::Value::Null);
                assert_eq!(request.header(super::scoped_fetch::HEADER), None);
                let token = if discoveries == 1 {
                    "Bearer fake-alice-access"
                } else {
                    "Bearer fake-alice-fresh-access"
                };
                assert_eq!(request.header("Authorization"), Some(token));
                return Reply {
                    status: 200,
                    content_type: "application/json",
                    headers: Vec::new(),
                    body: serde_json::to_vec(&serde_json::json!({
                        "scope":{"version":1,"repository_id":AGENT_ID,
                                 "reader_context":format!("{discoveries:064x}"),"refs":body["refs"]},
                        "refs":[{"name":"refs/heads/main","oid":main_oid},
                                {"name":"refs/heads/selected","oid":selected_available.load(Ordering::SeqCst).then_some(&expected_oid)}]
                    })).unwrap(),
                };
            }
            let scope: serde_json::Value = serde_json::from_slice(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(
                        request
                            .header(super::scoped_fetch::HEADER)
                            .expect("Git must carry its discovered scope"),
                    )
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(scope["reader_context"], format!("{discoveries:064x}"));
            if !denied_git {
                denied_git = true;
                return denied();
            }
            assert!(discoveries >= 2, "refresh must obtain another reader scope");
            assert_eq!(
                request.header("Authorization"),
                Some("Bearer fake-alice-fresh-access")
            );
            assert_eq!(request.header("Git-Protocol"), None);
            scoped_git_reply(&source_path, request)
        },
    );
    config::set_global("hub.url", Some(&hub.base)).unwrap();
    credentials::save(&hub.base, &pair(&hub.base, "alice")).unwrap();
    let identity = RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
    let url = format!("{}/alice/example.git", hub.base);
    let dest = home.workspace().join("scoped-clone");
    assert!(
        super::clone_selected(&url, &dest, &identity, Some("selected"))
            .unwrap()
            .ok()
    );
    let repo = Repo::at(&dest);
    assert_eq!(
        repo.git(&["rev-parse", "refs/remotes/origin/selected"])
            .unwrap()
            .trim(),
        oid
    );
    assert!(repo.git(&["tag", "--list"]).unwrap().trim().is_empty());
    source.git(&["checkout", "selected"]).unwrap();
    source
        .git(&["commit", "--allow-empty", "-m", "selected branch advances"])
        .unwrap();
    let advanced = source
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    *selected_oid.lock().unwrap() = advanced.clone();
    assert!(
        super::fetch_branch(&repo, &identity, "selected")
            .unwrap()
            .unwrap()
            .ok()
    );
    assert_eq!(
        repo.git(&["rev-parse", "refs/remotes/origin/selected"])
            .unwrap()
            .trim(),
        advanced
    );
    assert_eq!(
        repo.git(&["rev-parse", "refs/remotes/origin/main"])
            .unwrap()
            .trim(),
        oid
    );
    available.store(false, Ordering::SeqCst);
    let error = match super::fetch_branch(&repo, &identity, "selected") {
        Err(error) => error,
        Ok(_) => panic!("unavailable selected refs must not fetch a stale local copy"),
    };
    assert!(error.is::<super::ScopedRefUnavailable>());
    let config = std::fs::read_to_string(dest.join(".git/config")).unwrap();
    assert!(!config.contains("extraheader") && !config.contains("fake-alice"));
    let requests = hub.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == "/api/auth/refresh")
            .count(),
        1
    );
    assert!(requests.last().unwrap().path.ends_with("/agit-refs"));
}

/// A version retry keeps its resolved commit, and successful Git cannot substitute an unrelated
/// fetched root merely because the requested commit already exists in the local object store.
#[test]
fn scoped_version_pickup_pins_retry_identity_and_requires_fetched_root_reachability() {
    use crate::domain::repo::Repo;
    use base64::Engine;
    let home = IsolatedHome::new();
    let source = Repo::init(&home.workspace().join("version-source")).unwrap();
    source
        .git(&["config", "user.name", "Synthetic Reader"])
        .unwrap();
    source
        .git(&["config", "user.email", "reader@example.invalid"])
        .unwrap();
    source
        .git(&["symbolic-ref", "HEAD", "refs/heads/main"])
        .unwrap();
    source
        .git(&["commit", "--allow-empty", "-m", "requested version"])
        .unwrap();
    let old = source.git(&["rev-parse", "HEAD"]).unwrap();
    let tree = source.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
    let other = source
        .git(&["commit-tree", &tree, "-m", "unrelated version"])
        .unwrap();
    let tag = format!("refs/tags/agit-{old}");
    source.git(&["update-ref", &tag, &old]).unwrap();
    source
        .git(&["update-ref", "refs/tags/unselected", &other])
        .unwrap();
    source
        .git(&["update-ref", "-d", "refs/heads/main"])
        .unwrap();
    let phase = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_phase = Arc::clone(&phase);
    let server_path = source.root().to_owned();
    let server_old = old.clone();
    let server_tag = tag.clone();
    let mut lookups = 0;
    let mut denied_git = false;
    let hub = FakeHub::with_config(
        serde_json::json!({"capabilities":{"git_fetch_scope":[1],"git_fetch_version":[1]}}),
        move |request| {
            if request.path == "/api/auth/refresh" {
                return refreshed();
            }
            assert_eq!(
                request.header("X-AgentGit-Expected-Agent-Id"),
                Some(AGENT_ID)
            );
            let phase = server_phase.load(Ordering::SeqCst);
            let context = if request.header("Authorization") == Some("Bearer fake-alice-access") {
                "a".repeat(64)
            } else {
                "b".repeat(64)
            };
            let reference = if phase == 0 {
                server_tag.as_str()
            } else {
                "refs/heads/selected"
            };
            if request.path.ends_with("/agit-version") {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                if phase == 0 {
                    let expected = if lookups == 0 {
                        &server_old[..7]
                    } else {
                        server_old.as_str()
                    };
                    assert_eq!(
                        body["prefix"], expected,
                        "retry must use the resolved full ID"
                    );
                    lookups += 1;
                }
                return Reply {
                    status: 200,
                    content_type: "application/json",
                    headers: Vec::new(),
                    body: serde_json::to_vec(
                        &serde_json::json!({"version":1,"repository_id":AGENT_ID,
                        "reader_context":context,"prefix":body["prefix"],
                        "selection":{"commit":server_old,"reference":reference}}),
                    )
                    .unwrap(),
                };
            }
            if request.path.ends_with("/agit-refs") {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["refs"], serde_json::json!([reference]));
                return Reply { status:200, content_type:"application/json", headers:Vec::new(),
                    body:serde_json::to_vec(&serde_json::json!({
                        "scope":{"version":1,"repository_id":AGENT_ID,"reader_context":context,"refs":body["refs"]},
                        "refs":[{"name":reference,"oid":server_old}]})).unwrap() };
            }
            let scope: serde_json::Value = serde_json::from_slice(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(request.header(super::scoped_fetch::HEADER).unwrap())
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(scope["refs"], serde_json::json!([reference]));
            assert_eq!(scope["reader_context"], context);
            if !denied_git {
                denied_git = true;
                return denied();
            }
            if phase == 0 {
                assert!(lookups >= 3)
            }
            if phase == 1 && request.method == "GET" {
                Repo::at(&server_path)
                    .git(&["update-ref", "refs/heads/selected", &other])
                    .unwrap();
            }
            scoped_git_reply(&server_path, request)
        },
    );
    config::set_global("hub.url", Some(&hub.base)).unwrap();
    credentials::save(&hub.base, &pair(&hub.base, "alice")).unwrap();
    let identity = RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
    let url = format!("{}/alice/example.git", hub.base);
    let dest = home.workspace().join("version-clone");
    let transfer = super::clone_version(&url, &dest, &identity, &format!("agit-{}", &old[..7]))
        .unwrap()
        .unwrap();
    assert!(transfer.outcome.ok(), "{}", transfer.outcome.stderr);
    assert_eq!(transfer.commit.as_deref(), Some(old.as_str()));
    let repo = Repo::at(&dest);
    assert_eq!(repo.git(&["tag", "--list"]).unwrap(), format!("agit-{old}"));
    assert_eq!(
        repo.git(&["config", "--get-all", "remote.origin.fetch"])
            .unwrap(),
        "+refs/heads/*:refs/remotes/origin/*",
        "later branch updates must not depend on the version tag"
    );
    assert!(
        repo.remote_branches().is_empty(),
        "a tag-only version needs no main branch"
    );
    source.git(&["update-ref", "-d", &tag]).unwrap();
    source
        .git(&["update-ref", "refs/heads/selected", &old])
        .unwrap();
    phase.store(1, Ordering::SeqCst);
    let error = match super::fetch_version(&repo, &identity, &format!("agit-{old}")) {
        Err(error) => error,
        Ok(_) => panic!("an unrelated fetched root must not authorize the old local object"),
    };
    assert!(error.to_string().contains("no longer contains"));
    assert_eq!(repo.git(&["cat-file", "-t", &old]).unwrap(), "commit");
    let requests = hub.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == "/api/auth/refresh")
            .count(),
        1
    );
}

fn scoped_git_reply(source_path: &Path, request: &WireRequest) -> Reply {
    let advertise = request.method == "GET";
    let mut command = crate::infra::git_runtime::command();
    command.arg("upload-pack").arg("--stateless-rpc");
    if advertise {
        command.arg("--advertise-refs");
    }
    let mut child = command
        .arg(source_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&request.body)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "synthetic Git server failed");
    let mut body = Vec::new();
    if advertise {
        let service = "# service=git-upload-pack\n";
        body.extend_from_slice(format!("{:04x}{service}0000", service.len() + 4).as_bytes());
    }
    body.extend(output.stdout);
    Reply {
        status: 200,
        content_type: if advertise {
            "application/x-git-upload-pack-advertisement"
        } else {
            "application/x-git-upload-pack-result"
        },
        body,
        headers: Vec::new(),
    }
}
