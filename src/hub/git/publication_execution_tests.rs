fn native_publication_available(repo: &Repo) -> bool {
    match crate::domain::lfs::local::require_client(repo) {
        Ok(()) => true,
        Err(error) => {
            assert!(
                std::env::var_os("AGIT_TEST_REQUIRE_LFS").is_none(),
                "{error:#}"
            );
            eprintln!("native publication upload fixture skipped: Git LFS is unavailable");
            println!("{COMPLETE}");
            false
        }
    }
}

fn native_inspected(
    prepared: crate::hub::git::PreparedPublication,
) -> crate::hub::git::CompleteInspection {
    match prepared.inspect(crate::domain::secrets::ScanLimits::DEFAULT) {
        crate::hub::git::PublicationInspection::Complete(complete) => complete,
        crate::hub::git::PublicationInspection::Blocked(_) => {
            panic!("owning publication fixture must finish deterministic inspection")
        }
    }
}

fn native_git_reply(request: &WireRequest, repository: &Path) -> Reply {
    let mut command = Command::new("git");
    command.arg("receive-pack").arg("--stateless-rpc");
    let advertise = request.path == PUSH_PATH;
    if advertise {
        command.arg("--advertise-refs");
    } else {
        assert_eq!(request.path, "/alice/example.git/git-receive-pack");
    }
    let mut child = command
        .arg(repository)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if !advertise {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&request.body)
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "private receive-pack fixture: {output:?}"
    );
    let mut body = Vec::new();
    if advertise {
        let service = "# service=git-receive-pack\n";
        body.extend_from_slice(format!("{:04x}{service}0000", service.len() + 4).as_bytes());
    }
    body.extend(output.stdout);
    Reply {
        status: 200,
        content_type: if advertise {
            "application/x-git-receive-pack-advertisement"
        } else {
            "application/x-git-receive-pack-result"
        },
        headers: vec![],
        body,
    }
}

fn native_lfs_object(
    request: &WireRequest,
    pointer: &crate::domain::lfs::Pointer,
) -> serde_json::Value {
    let url = format!(
        "http://{}/alice/example.git/info/lfs/objects/{}",
        request.header("Host").unwrap(),
        pointer.oid
    );
    serde_json::json!({
        "oid": pointer.oid,
        "size": pointer.size,
        "authenticated": true,
        "actions": {"upload": {"href": url}, "verify": {"href": format!("{url}/verify")}}
    })
}

fn native_locks_unavailable() -> Reply {
    Reply {
        status: 404,
        content_type: "application/vnd.git-lfs+json",
        headers: vec![],
        body: br#"{"message":"locking is not supported"}"#.to_vec(),
    }
}

#[test]
fn native_publication_consumes_owned_bytes_and_retains_refresh_into_refs() {
    if !isolated("native_publication_consumes_owned_bytes_and_retains_refresh_into_refs") {
        return;
    }
    use crate::hub::git::{LfsAttemptKind, SecretFindingsAcceptance};
    let home = IsolatedHome::new();
    let receiver = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(receiver.path())
            .status()
            .unwrap()
            .success()
    );
    let receiver_path = receiver.path().to_owned();
    let payload =
        b"-----BEGIN PRIVATE KEY-----\nsynthetic fixture only\n-----END PRIVATE KEY-----\n";
    let pointer = prepared_pointer(payload);
    let returned = pointer.clone();
    let mut prepared = false;
    let mut uploaded = false;
    let mut verified = false;
    let hub = FakeHub::with_timeout(Duration::from_secs(120), move |request| {
        if request.path == "/api/auth/refresh" {
            assert_eq!(request.header("Authorization"), None);
            return refreshed();
        }
        if request.path == PUSH_PATH || request.path.ends_with("/git-receive-pack") {
            assert!(verified, "refs must follow completed LFS verification");
            assert_eq!(
                request.header("User-Agent"),
                Some("frozen-publication-fixture")
            );
            assert_eq!(
                request.header("Authorization"),
                Some("Bearer fake-alice-fresh-access")
            );
            assert_eq!(
                request.header("X-AgentGit-Accept-Secret-Findings"),
                Some("true")
            );
            return native_git_reply(request, &receiver_path);
        }
        if prepared {
            assert!(
                request
                    .header("User-Agent")
                    .unwrap()
                    .starts_with("git-lfs/")
            );
        } else {
            assert_eq!(request.header("User-Agent"), Some("captured-native-lfs"));
        }
        assert_eq!(
            request.header("X-AgentGit-Expected-Agent-Id"),
            Some(AGENT_ID)
        );
        if request.path.ends_with("/locks/verify") {
            return native_locks_unavailable();
        }
        if request.path.ends_with("/objects/batch") {
            if !prepared {
                prepared = true;
                assert_eq!(request.header("X-AgentGit-Accept-Secret-Findings"), None);
            } else {
                assert_eq!(
                    request.header("X-AgentGit-Accept-Secret-Findings"),
                    Some("true")
                );
                if request.header("Authorization") == Some("Bearer fake-alice-access") {
                    return denied();
                }
                assert_eq!(
                    request.header("Authorization"),
                    Some("Bearer fake-alice-fresh-access")
                );
            }
            return prepared_batch(serde_json::json!([native_lfs_object(request, &returned)]));
        }
        assert_eq!(
            request.header("Authorization"),
            Some("Bearer fake-alice-fresh-access")
        );
        assert_eq!(
            request.header("X-AgentGit-Accept-Secret-Findings"),
            Some("true")
        );
        if request.method == "PUT" {
            assert_eq!(request.body, payload);
            uploaded = true;
        } else {
            assert!(request.path.ends_with("/verify") && uploaded);
            verified = true;
        }
        Reply {
            status: 200,
            content_type: "application/vnd.git-lfs+json",
            headers: vec![],
            body: b"{}".to_vec(),
        }
    });
    let (repo, _, url, identity) = prepared_source(&home, &hub.base);
    if !native_publication_available(&repo) {
        return;
    }
    repo.git(&[
        "config",
        &format!("http.{url}/info/lfs/objects/batch.userAgent"),
        "captured-native-lfs",
    ])
    .unwrap();
    record_prepared_pointer(&repo, "payload", &pointer);
    repo.git(&["tag", "-a", "owned-tag", "-m", "Owned publication fixture"])
        .unwrap();
    let cache = prepared_cache(&repo, &pointer, payload);
    let plan = prepared_plan(&repo);
    let complete = native_inspected(
        FrozenPublication::prepare(&repo, &plan, &url, &identity)
            .unwrap()
            .prepare_payloads(pointer.size)
            .unwrap(),
    );
    assert!(complete.has_findings());
    std::fs::write(&cache, b"changed original payload").unwrap();
    for (key, value) in [
        ("lfs.url", "https://unselected.invalid/lfs"),
        ("lfs.storage", "unselected-cache"),
        ("http.userAgent", "unselected-agent"),
    ] {
        repo.git(&["config", key, value]).unwrap();
    }
    repo.git(&[
        "config",
        &format!("http.{url}/info/lfs/objects/batch.userAgent"),
        "unselected-native-agent",
    ])
    .unwrap();
    rewrite(
        repo.root(),
        &url,
        "https://unselected.invalid/repository.git",
    );
    let report = complete.publish(SecretFindingsAcceptance::Accept);
    assert!(report.ok(), "owned native publication: {report:?}");
    let lfs = report.lfs.as_ref().unwrap();
    assert_eq!(lfs.attempts.len(), 3);
    assert_eq!(lfs.attempts[0].kind, LfsAttemptKind::Version);
    assert!(!lfs.attempts[1].ok());
    assert!(lfs.attempts[2].ok());
    assert_eq!(lfs.attempts[2].pointers, vec![pointer]);
    assert!(report.heads.as_ref().unwrap().ok() && report.tags.as_ref().unwrap().ok());
    let remote = Repo::at(receiver.path());
    for reference in plan.heads().iter().chain(plan.tags()) {
        assert_eq!(
            remote.git(&["rev-parse", reference.name()]).unwrap().trim(),
            reference.oid()
        );
    }
    let requests = hub.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == "/api/auth/refresh")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "PUT")
            .count(),
        1
    );
    println!("native LFS upload and refs verified");
    println!("{COMPLETE}");
}

#[test]
fn native_publication_rejects_findings_without_operation_acceptance() {
    if !isolated("native_publication_rejects_findings_without_operation_acceptance") {
        return;
    }
    use crate::hub::git::SecretFindingsAcceptance;
    let home = IsolatedHome::new();
    let payload =
        b"-----BEGIN PRIVATE KEY-----\nsynthetic fixture only\n-----END PRIVATE KEY-----\n";
    let pointer = prepared_pointer(payload);
    let returned = pointer.clone();
    let hub = FakeHub::new(move |request| {
        assert!(request.path.ends_with("/objects/batch"));
        assert_eq!(request.header("X-AgentGit-Accept-Secret-Findings"), None);
        prepared_batch(serde_json::json!([native_lfs_object(request, &returned)]))
    });
    let (repo, _, url, identity) = prepared_source(&home, &hub.base);
    record_prepared_pointer(&repo, "payload", &pointer);
    prepared_cache(&repo, &pointer, payload);
    let complete = native_inspected(
        FrozenPublication::prepare(&repo, &prepared_plan(&repo), &url, &identity)
            .unwrap()
            .prepare_payloads(pointer.size)
            .unwrap(),
    );
    assert!(complete.has_findings());
    unsafe {
        std::env::set_var("AGIT_ALLOW_SECRETS", "1");
    }
    let report = complete.publish(SecretFindingsAcceptance::Reject);
    assert!(!report.ok());
    assert!(report.error.is_some());
    assert!(report.lfs.is_none() && report.heads.is_none() && report.tags.is_none());
    assert_eq!(hub.finish().len(), 1);
    println!("{COMPLETE}");
}

#[test]
fn native_publication_retains_completed_batches_and_stops_before_unattempted_refs() {
    if !isolated("native_publication_retains_completed_batches_and_stops_before_unattempted_refs") {
        return;
    }
    use crate::hub::git::{LfsAttemptKind, SecretFindingsAcceptance};
    let home = IsolatedHome::new();
    let mut availability = 0;
    let mut uploads = 0;
    let hub = FakeHub::with_timeout(Duration::from_secs(120), move |request| {
        assert_eq!(request.header("X-AgentGit-Accept-Secret-Findings"), None);
        if request.path.ends_with("/locks/verify") {
            return native_locks_unavailable();
        }
        assert_eq!(request.path, "/alice/example.git/info/lfs/objects/batch");
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let pointers: Vec<crate::domain::lfs::Pointer> =
            serde_json::from_value(body["objects"].clone()).unwrap();
        if availability < 3 {
            availability += 1;
            return prepared_batch(serde_json::Value::Array(
                pointers
                    .iter()
                    .map(|pointer| native_lfs_object(request, pointer))
                    .collect(),
            ));
        }
        uploads += 1;
        if uploads == 1 {
            prepared_batch(serde_json::to_value(pointers).unwrap())
        } else {
            Reply {
                status: 400,
                content_type: "application/vnd.git-lfs+json",
                headers: vec![],
                body: br#"{"message":"selected batch is unavailable"}"#.to_vec(),
            }
        }
    });
    let (repo, _, url, identity) = prepared_source(&home, &hub.base);
    if !native_publication_available(&repo) {
        return;
    }
    let mut pointers = Vec::new();
    for index in 0..201 {
        let payload = format!("owned payload {index}\n");
        let pointer = prepared_pointer(payload.as_bytes());
        std::fs::write(
            repo.root().join(format!("payload-{index}")),
            format!(
                "version {}\noid sha256:{}\nsize {}\n",
                crate::domain::lfs::VERSION,
                pointer.oid,
                pointer.size
            ),
        )
        .unwrap();
        prepared_cache(&repo, &pointer, payload.as_bytes());
        pointers.push(pointer);
    }
    repo.add_all().unwrap();
    repo.commit("Record bounded native batches").unwrap();
    pointers.sort_by(|a, b| a.oid.cmp(&b.oid));
    let budget = pointers.iter().map(|pointer| pointer.size).sum();
    let complete = native_inspected(
        FrozenPublication::prepare(&repo, &prepared_plan(&repo), &url, &identity)
            .unwrap()
            .prepare_payloads(budget)
            .unwrap(),
    );
    assert!(!complete.has_findings());
    let report = complete.publish(SecretFindingsAcceptance::Reject);
    assert!(!report.ok());
    assert!(report.heads.is_none() && report.tags.is_none());
    let lfs = report.lfs.unwrap();
    assert!(lfs.error.is_some());
    assert_eq!(lfs.attempts.len(), 3);
    assert_eq!(lfs.attempts[0].kind, LfsAttemptKind::Version);
    assert_eq!(lfs.attempts[1].kind, LfsAttemptKind::Objects { batch: 0 });
    assert!(lfs.attempts[1].ok());
    assert_eq!(lfs.attempts[1].pointers, pointers[..100]);
    assert_eq!(lfs.attempts[2].kind, LfsAttemptKind::Objects { batch: 1 });
    assert!(!lfs.attempts[2].ok());
    assert_eq!(lfs.attempts[2].pointers, pointers[100..200]);
    assert_eq!(lfs.unattempted, pointers[200..]);
    let requests = hub.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path.ends_with("/objects/batch"))
            .count(),
        5
    );
    println!("native LFS batch failure and ref refusal verified");
    println!("{COMPLETE}");
}

#[test]
fn native_publication_refuses_changed_owned_bytes_before_starting_a_native_phase() {
    if !isolated("native_publication_refuses_changed_owned_bytes_before_starting_a_native_phase") {
        return;
    }
    use crate::hub::git::SecretFindingsAcceptance;
    let home = IsolatedHome::new();
    let payload = b"verified private payload";
    let pointer = prepared_pointer(payload);
    let returned = pointer.clone();
    let hub = FakeHub::new(move |request| {
        assert_eq!(request.path, "/alice/example.git/info/lfs/objects/batch");
        prepared_batch(serde_json::json!([native_lfs_object(request, &returned)]))
    });
    let (repo, _, url, identity) = prepared_source(&home, &hub.base);
    record_prepared_pointer(&repo, "payload", &pointer);
    prepared_cache(&repo, &pointer, payload);
    let complete = native_inspected(
        FrozenPublication::prepare(&repo, &prepared_plan(&repo), &url, &identity)
            .unwrap()
            .prepare_payloads(pointer.size)
            .unwrap(),
    );
    complete
        .prepared()
        .damage_staged_payload_for_test(&pointer, b"different owned payload");
    let report = complete.publish(SecretFindingsAcceptance::Accept);
    assert!(!report.ok());
    assert!(report.heads.is_none() && report.tags.is_none());
    let lfs = report.lfs.unwrap();
    assert_eq!(
        lfs.error.as_deref(),
        Some("private LFS payload changed after inspection")
    );
    assert!(lfs.attempts.is_empty());
    assert_eq!(lfs.unattempted, vec![pointer]);
    assert_eq!(hub.finish().len(), 1);
    println!("{COMPLETE}");
}

#[test]
fn native_publication_empty_and_remote_present_inventory_need_no_local_payload() {
    if !isolated("native_publication_empty_and_remote_present_inventory_need_no_local_payload") {
        return;
    }
    use crate::hub::git::SecretFindingsAcceptance;
    for with_pointer in [false, true] {
        let home = IsolatedHome::new();
        let receiver = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(receiver.path())
                .status()
                .unwrap()
                .success()
        );
        let receiver_path = receiver.path().to_owned();
        let pointer = prepared_pointer(b"already present without local payload");
        let returned = pointer.clone();
        let hub = FakeHub::new(move |request| {
            assert_eq!(request.header("X-AgentGit-Accept-Secret-Findings"), None);
            if request.path.ends_with("/objects/batch") {
                assert!(with_pointer);
                return prepared_batch(serde_json::json!([returned]));
            }
            native_git_reply(request, &receiver_path)
        });
        let (repo, _, url, identity) = prepared_source(&home, &hub.base);
        if with_pointer {
            record_prepared_pointer(&repo, "payload", &pointer);
        }
        let plan = prepared_plan(&repo);
        let complete = native_inspected(
            FrozenPublication::prepare(&repo, &plan, &url, &identity)
                .unwrap()
                .prepare_payloads(0)
                .unwrap(),
        );
        assert_eq!(
            complete.report().remote_present().len(),
            usize::from(with_pointer)
        );
        let report = complete.publish(SecretFindingsAcceptance::Reject);
        assert!(report.ok(), "empty native phase: {report:?}");
        let lfs = report.lfs.unwrap();
        assert!(lfs.ok() && lfs.attempts.is_empty() && lfs.unattempted.is_empty());
        assert_eq!(
            lfs.remote_present,
            if with_pointer { vec![pointer] } else { vec![] }
        );
        assert!(!repo.root().join(".git/lfs").exists());
        for reference in plan.heads() {
            assert_eq!(
                Repo::at(receiver.path())
                    .git(&["rev-parse", reference.name()])
                    .unwrap()
                    .trim(),
                reference.oid()
            );
        }
        let requests = hub.finish();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.path.ends_with("/objects/batch"))
                .count(),
            usize::from(with_pointer)
        );
    }
    println!("{COMPLETE}");
}

#[test]
fn native_publication_checks_credential_expanded_lfs_configuration_before_spawn() {
    if !isolated("native_publication_checks_credential_expanded_lfs_configuration_before_spawn") {
        return;
    }
    use crate::hub::git::SecretFindingsAcceptance;
    let home = IsolatedHome::new();
    let payload = b"owned payload with bounded process configuration";
    let pointer = prepared_pointer(payload);
    let returned = pointer.clone();
    let hub = FakeHub::new(move |request| {
        assert_eq!(request.path, "/alice/example.git/info/lfs/objects/batch");
        assert_eq!(request.header("User-Agent").unwrap().len(), 12000);
        assert_eq!(request.header("Authorization").unwrap().len(), 20007);
        prepared_batch(serde_json::json!([native_lfs_object(request, &returned)]))
    });
    let (repo, _, url, identity) = prepared_source(&home, &hub.base);
    let mut credential = pair(&hub.base, "alice");
    credential.access_token = "x".repeat(20000);
    credentials::save(&hub.base, &credential).unwrap();
    repo.git(&[
        "config",
        &format!("http.{url}/info/lfs/objects/batch.userAgent"),
        &"x".repeat(12000),
    ])
    .unwrap();
    record_prepared_pointer(&repo, "payload", &pointer);
    prepared_cache(&repo, &pointer, payload);
    let complete = native_inspected(
        FrozenPublication::prepare(&repo, &prepared_plan(&repo), &url, &identity)
            .unwrap()
            .prepare_payloads(pointer.size)
            .unwrap(),
    );
    let report = complete.publish(SecretFindingsAcceptance::Reject);
    assert!(!report.ok());
    assert!(report.heads.is_none() && report.tags.is_none());
    let lfs = report.lfs.unwrap();
    assert_eq!(
        lfs.error.as_deref(),
        Some("frozen transport configuration exceeds its environment limit")
    );
    assert!(lfs.attempts.is_empty());
    assert_eq!(lfs.unattempted, vec![pointer]);
    assert_eq!(hub.finish().len(), 1);
    println!("{COMPLETE}");
}
