fn captured_source(
    home: &IsolatedHome,
    hub: &str,
) -> (Repo, PublicationPlan, String, RemoteIdentity) {
    let (_, plan, url, identity) = prepared_source(home, hub);
    let root = home.workspace().join("captured-source");
    std::fs::create_dir(&root).unwrap();
    std::fs::rename(home.workspace().join(".git"), root.join(".git")).unwrap();
    (Repo::at(root), plan, url, identity)
}

fn captured_inspected(
    captured: crate::hub::git::CapturedPublication,
) -> crate::hub::git::CompleteContentInspection {
    match captured.inspect(crate::domain::secrets::ScanLimits::DEFAULT) {
        crate::hub::git::ContentInspection::Complete(complete) => complete,
        crate::hub::git::ContentInspection::Blocked(blocked) => {
            panic!("captured fixture is incomplete: {}", blocked.reason())
        }
    }
}

#[test]
fn full_capture_has_no_destination_and_requires_every_payload_locally() {
    if !isolated("full_capture_has_no_destination_and_requires_every_payload_locally") {
        return;
    }
    use crate::hub::git::CapturedPublication;
    let home = IsolatedHome::new();
    let hub = FakeHub::new(|_| panic!("content capture must not contact the destination"));
    let (repo, _, _, _) = captured_source(&home, &hub.base);
    let payload = b"captured remote-present text";
    let pointer = prepared_pointer(payload);
    record_prepared_pointer(&repo, "payload.lfs", &pointer);
    let plan = prepared_plan(&repo);
    let config = std::fs::read(repo.root().join(".git/config")).unwrap();
    assert!(CapturedPublication::capture(&repo, &plan, pointer.size).is_err());
    let cache = prepared_cache(&repo, &pointer, payload);
    assert!(CapturedPublication::capture(&repo, &plan, pointer.size - 1).is_err());
    let captured = CapturedPublication::capture(&repo, &plan, pointer.size).unwrap();
    assert_eq!(captured.plan(), &plan);
    assert_eq!(captured.pointers(), std::slice::from_ref(&pointer));
    assert!(captured.snapshot_lfs_storage().unwrap().is_dir());
    for reference in plan.heads().iter().chain(plan.tags()) {
        assert_eq!(
            std::fs::read_to_string(captured.snapshot_git_dir().join(reference.name())).unwrap(),
            format!("{}\n", reference.oid()),
        );
    }
    std::fs::remove_file(cache).unwrap();
    let mut retained = Vec::new();
    captured
        .open_payload(&pointer)
        .unwrap()
        .read_to_end(&mut retained)
        .unwrap();
    assert_eq!(retained, payload);
    let complete = captured_inspected(captured);
    complete.verify_source(&repo).unwrap();
    assert!(complete.report().remote_present().is_empty());
    assert!(complete.report().scan().unscanned.is_empty());
    assert_eq!(
        std::fs::read(repo.root().join(".git/config")).unwrap(),
        config
    );
    assert!(hub.finish().is_empty());
    println!("{COMPLETE}");
}

#[test]
fn captured_inspection_blocks_before_destination_binding_when_budget_is_incomplete() {
    if !isolated("captured_inspection_blocks_before_destination_binding_when_budget_is_incomplete")
    {
        return;
    }
    use crate::domain::secrets::ScanLimits;
    use crate::hub::git::{CapturedPublication, ContentInspection, InspectionFailure};
    let home = IsolatedHome::new();
    let hub = FakeHub::new(|_| panic!("incomplete inspection must not bind a destination"));
    let (repo, plan, _, _) = captured_source(&home, &hub.base);
    let captured = CapturedPublication::capture(&repo, &plan, 0).unwrap();
    let result = captured.inspect(ScanLimits {
        budget_bytes: 0,
        ..ScanLimits::DEFAULT
    });
    let ContentInspection::Blocked(blocked) = result else {
        panic!("an unread captured commit must block inspection");
    };
    assert_eq!(blocked.reason(), InspectionFailure::Incomplete);
    assert!(blocked.report().scan().unscanned.over_budget.is_some());
    assert_eq!(blocked.captured().plan(), &plan);
    assert!(hub.finish().is_empty());
    println!("{COMPLETE}");
}

#[test]
fn captured_source_changes_refuse_before_binding_requests() {
    if !isolated("captured_source_changes_refuse_before_binding_requests") {
        return;
    }
    use crate::hub::git::CapturedPublication;
    let home = IsolatedHome::new();
    let hub = FakeHub::new(|_| panic!("changed selection must stop before binding requests"));
    let (repo, plan, url, identity) = captured_source(&home, &hub.base);
    let complete = captured_inspected(CapturedPublication::capture(&repo, &plan, 0).unwrap());
    repo.git(&["commit", "--allow-empty", "-m", "Advance selected source"])
        .unwrap();
    assert!(complete.verify_source(&repo).is_err());
    assert!(complete.bind_destination(&repo, &url, &identity).is_err());
    assert!(hub.finish().is_empty());
    println!("{COMPLETE}");
}

#[test]
fn captured_destination_binding_retains_all_review_bytes_after_source_relocation() {
    if !isolated("captured_destination_binding_retains_all_review_bytes_after_source_relocation") {
        return;
    }
    use crate::hub::git::CapturedPublication;
    let home = IsolatedHome::new();
    let present_bytes =
        b"-----BEGIN PRIVATE KEY-----\nsynthetic fixture only\n-----END PRIVATE KEY-----\n";
    let missing_bytes = b"captured missing payload";
    let present = prepared_pointer(present_bytes);
    let missing = prepared_pointer(missing_bytes);
    let returned_present = present.clone();
    let returned_missing = missing.clone();
    let requests_seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = requests_seen.clone();
    let hub = FakeHub::new(move |request| {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(request.path, "/alice/example.git/info/lfs/objects/batch");
        prepared_batch(serde_json::json!([
            {"oid": returned_present.oid, "size": returned_present.size},
            {"oid": returned_missing.oid, "size": returned_missing.size, "actions": {"upload": {}}},
        ]))
    });
    let (repo, _, url, identity) = captured_source(&home, &hub.base);
    record_prepared_pointer(&repo, "present.lfs", &present);
    record_prepared_pointer(&repo, "missing.lfs", &missing);
    let present_cache = prepared_cache(&repo, &present, present_bytes);
    let missing_cache = prepared_cache(&repo, &missing, missing_bytes);
    let plan = prepared_plan(&repo);
    let complete = captured_inspected(
        CapturedPublication::capture(&repo, &plan, present.size + missing.size).unwrap(),
    );
    assert!(complete.has_findings());
    assert!(complete.report().scan().hits.iter().any(|hit| {
        hit.file
            .as_deref()
            .is_some_and(|file| file.starts_with("lfs object "))
    }));
    assert!(complete.report().remote_present().is_empty());
    assert_eq!(requests_seen.load(std::sync::atomic::Ordering::SeqCst), 0);
    complete.verify_source(&repo).unwrap();
    let stage = complete.captured().snapshot_lfs_storage().unwrap();
    std::fs::remove_file(present_cache).unwrap();
    std::fs::write(missing_cache, b"changed mutable original payload").unwrap();
    let moved = home.workspace().join("promoted-captured-source");
    std::fs::rename(repo.root(), &moved).unwrap();
    let moved_repo = Repo::at(moved);
    let bound = complete
        .bind_destination(&moved_repo, &url, &identity)
        .unwrap();
    assert_eq!(bound.prepared().plan(), &plan);
    assert_eq!(
        bound.prepared().missing_uploads(),
        std::slice::from_ref(&missing)
    );
    assert!(bound.has_findings());
    for (pointer, bytes) in [
        (&present, present_bytes.as_slice()),
        (&missing, missing_bytes.as_slice()),
    ] {
        let mut retained = Vec::new();
        bound
            .prepared()
            .open_payload(pointer)
            .unwrap()
            .read_to_end(&mut retained)
            .unwrap();
        assert_eq!(retained, bytes);
    }
    assert!(stage.is_dir());
    drop(bound);
    assert!(!stage.exists());
    assert_eq!(hub.finish().len(), 1);
    println!("{COMPLETE}");
}

#[test]
fn native_captured_publication_uploads_only_missing_owned_bytes_after_promotion_move() {
    if !isolated(
        "native_captured_publication_uploads_only_missing_owned_bytes_after_promotion_move",
    ) {
        return;
    }
    use crate::hub::git::{CapturedPublication, SecretFindingsAcceptance};
    let home = IsolatedHome::new();
    let other = FakeHub::new(|_| panic!("the current Hub cannot replace the captured client"));
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
    let present_bytes = b"captured remotely present text";
    let missing_bytes = b"captured locally owned upload";
    let present = prepared_pointer(present_bytes);
    let missing = prepared_pointer(missing_bytes);
    let returned_present = present.clone();
    let returned_missing = missing.clone();
    let mut bound = false;
    let mut uploaded = false;
    let mut verified = false;
    let hub = FakeHub::with_timeout(Duration::from_secs(120), move |request| {
        assert_eq!(
            request.header("Authorization"),
            Some("Bearer fake-alice-access")
        );
        if request.path == PUSH_PATH || request.path.ends_with("/git-receive-pack") {
            assert!(
                verified,
                "captured refs must follow verified owned payloads"
            );
            return native_git_reply(request, &receiver_path);
        }
        if request.path.ends_with("/locks/verify") {
            return native_locks_unavailable();
        }
        assert_eq!(
            request.header("X-AgentGit-Expected-Agent-Id"),
            Some(AGENT_ID)
        );
        if request.path.ends_with("/objects/batch") {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let selected: Vec<crate::domain::lfs::Pointer> =
                serde_json::from_value(body["objects"].clone()).unwrap();
            if !bound {
                bound = true;
                let mut expected = vec![returned_present.clone(), returned_missing.clone()];
                expected.sort_by(|a, b| a.oid.cmp(&b.oid));
                assert_eq!(selected, expected);
                return prepared_batch(serde_json::json!([
                    {"oid": returned_present.oid, "size": returned_present.size},
                    native_lfs_object(request, &returned_missing),
                ]));
            }
            assert_eq!(selected, vec![returned_missing.clone()]);
            return prepared_batch(serde_json::json!([native_lfs_object(
                request,
                &returned_missing
            )]));
        }
        if request.method == "PUT" {
            assert!(request.path.ends_with(&returned_missing.oid));
            assert_eq!(request.body, missing_bytes);
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
    let (repo, _, url, identity) = captured_source(&home, &hub.base);
    let client = crate::hub::Client::for_stored_hub(&hub.base);
    if !native_publication_available(&repo) {
        return;
    }
    record_prepared_pointer(&repo, "present.lfs", &present);
    record_prepared_pointer(&repo, "missing.lfs", &missing);
    let present_cache = prepared_cache(&repo, &present, present_bytes);
    let missing_cache = prepared_cache(&repo, &missing, missing_bytes);
    repo.git(&[
        "tag",
        "-a",
        "captured-release",
        "-m",
        "Captured publication metadata",
    ])
    .unwrap();
    let plan = prepared_plan(&repo);
    let complete = captured_inspected(
        CapturedPublication::capture(&repo, &plan, present.size + missing.size).unwrap(),
    );
    complete.verify_source(&repo).unwrap();
    credentials::save(&hub.base, &pair(&hub.base, "bob")).unwrap();
    config::set_global("hub.url", Some(&other.base)).unwrap();
    std::fs::remove_file(present_cache).unwrap();
    std::fs::write(missing_cache, b"unreviewed replacement payload").unwrap();
    repo.git(&["config", "lfs.storage", "unselected-live-cache"])
        .unwrap();
    let moved = home.workspace().join("published-copy");
    std::fs::rename(repo.root(), &moved).unwrap();
    let moved_repo = Repo::at(moved);
    let complete = complete
        .bind_destination_with_client(&moved_repo, &url, &identity, client)
        .unwrap();
    let report = complete.publish(SecretFindingsAcceptance::Reject);
    assert!(
        report.ok(),
        "captured publication after relocation: {report:?}"
    );
    let lfs = report.lfs.as_ref().unwrap();
    assert_eq!(lfs.remote_present, vec![present]);
    assert_eq!(lfs.attempts.len(), 2);
    assert_eq!(lfs.attempts.last().unwrap().pointers, vec![missing]);
    let remote = Repo::at(receiver.path());
    for reference in plan.heads().iter().chain(plan.tags()) {
        assert_eq!(
            remote.git(&["rev-parse", reference.name()]).unwrap().trim(),
            reference.oid()
        );
    }
    assert_eq!(
        credentials::load_checked(&hub.base)
            .unwrap()
            .unwrap()
            .username,
        "bob"
    );
    assert!(other.finish().is_empty());
    let requests = hub.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "PUT")
            .count(),
        1
    );
    println!("captured native LFS upload and relocated refs verified");
    println!("{COMPLETE}");
}

#[test]
fn upload_selection_is_sorted_independently_of_availability_response_order() {
    if !isolated("upload_selection_is_sorted_independently_of_availability_response_order") {
        return;
    }
    use crate::hub::git::CapturedPublication;
    let home = IsolatedHome::new();
    let first_bytes = b"selected payload alpha";
    let second_bytes = b"selected payload beta";
    let first = prepared_pointer(first_bytes);
    let second = prepared_pointer(second_bytes);
    let mut expected = vec![first.clone(), second.clone()];
    expected.sort_by(|left, right| left.oid.cmp(&right.oid));
    let returned = expected.clone();
    let hub = FakeHub::new(move |request| {
        assert!(request.path.ends_with("/objects/batch"));
        prepared_batch(serde_json::Value::Array(returned.iter().rev().map(|pointer| {
            serde_json::json!({"oid": pointer.oid, "size": pointer.size, "actions": {"upload": {}}})
        }).collect()))
    });
    let (repo, _, url, identity) = captured_source(&home, &hub.base);
    record_prepared_pointer(&repo, "alpha.lfs", &first);
    record_prepared_pointer(&repo, "beta.lfs", &second);
    prepared_cache(&repo, &first, first_bytes);
    prepared_cache(&repo, &second, second_bytes);
    let plan = prepared_plan(&repo);
    let budget = first.size + second.size;
    for full_capture in [false, true] {
        let complete = if full_capture {
            captured_inspected(CapturedPublication::capture(&repo, &plan, budget).unwrap())
                .bind_destination(&repo, &url, &identity)
                .unwrap()
        } else {
            native_inspected(
                FrozenPublication::prepare(&repo, &plan, &url, &identity)
                    .unwrap()
                    .prepare_payloads(budget)
                    .unwrap(),
            )
        };
        assert_eq!(complete.prepared().missing_uploads(), expected);
    }
    assert_eq!(hub.finish().len(), 2);
    println!("{COMPLETE}");
}

#[test]
fn maximum_selected_heads_keep_main_outside_the_explicit_reverification_limit() {
    if !isolated("maximum_selected_heads_keep_main_outside_the_explicit_reverification_limit") {
        return;
    }
    use crate::hub::git::CapturedPublication;
    let home = IsolatedHome::new();
    let hub = FakeHub::new(|_| panic!("source verification must not contact a destination"));
    let (repo, _, _, _) = captured_source(&home, &hub.base);
    repo.git(&["branch", "-M", "main"]).unwrap();
    let branches: Vec<String> = (0..64).map(|index| format!("selected-{index}")).collect();
    for branch in &branches {
        repo.git(&["branch", branch, "main"]).unwrap();
    }
    let plan = PublicationPlan::freeze(&repo, &branches).unwrap();
    assert_eq!(plan.heads().len(), 65);
    let complete = captured_inspected(CapturedPublication::capture(&repo, &plan, 0).unwrap());
    complete.verify_source(&repo).unwrap();
    repo.git(&["update-ref", "-d", "refs/heads/main"]).unwrap();
    assert!(complete.verify_source(&repo).is_err());
    assert!(hub.finish().is_empty());
    println!("{COMPLETE}");
}

#[test]
fn supplied_publication_client_rejects_another_hub_or_invalid_credential_binding() {
    if !isolated("supplied_publication_client_rejects_another_hub_or_invalid_credential_binding") {
        return;
    }
    use crate::hub::git::CapturedPublication;
    let home = IsolatedHome::new();
    let hub = FakeHub::new(|_| panic!("a mismatched client must not make availability requests"));
    let other = FakeHub::new(|_| panic!("a mismatched client must not contact its own Hub"));
    let (repo, plan, url, identity) = captured_source(&home, &hub.base);
    for client in [
        crate::hub::Client::for_credential(&other.base, &pair(&other.base, "alice")),
        crate::hub::Client::for_credential(&hub.base, &pair(&other.base, "alice")),
    ] {
        let complete = captured_inspected(CapturedPublication::capture(&repo, &plan, 0).unwrap());
        assert!(
            complete
                .bind_destination_with_client(&repo, &url, &identity, client)
                .is_err()
        );
    }
    assert!(hub.finish().is_empty());
    assert!(other.finish().is_empty());
    println!("{COMPLETE}");
}

#[test]
fn supplied_client_never_adopts_a_login_changed_during_audit_for_availability_retry() {
    if !isolated("supplied_client_never_adopts_a_login_changed_during_audit_for_availability_retry")
    {
        return;
    }
    use crate::hub::git::CapturedPublication;
    let home = IsolatedHome::new();
    let other = FakeHub::new(|_| panic!("the configured Hub cannot replace the supplied client"));
    let hub = FakeHub::new(|request| {
        assert_eq!(request.path, "/alice/example.git/info/lfs/objects/batch");
        assert_eq!(
            request.header("Authorization"),
            Some("Bearer fake-alice-access")
        );
        denied()
    });
    let (repo, _, url, identity) = captured_source(&home, &hub.base);
    let client = crate::hub::Client::for_stored_hub(&hub.base);
    let payload = b"captured audit text";
    let pointer = prepared_pointer(payload);
    record_prepared_pointer(&repo, "payload.lfs", &pointer);
    prepared_cache(&repo, &pointer, payload);
    let complete = captured_inspected(
        CapturedPublication::capture(&repo, &prepared_plan(&repo), pointer.size).unwrap(),
    );
    credentials::save(&hub.base, &pair(&hub.base, "bob")).unwrap();
    config::set_global("hub.url", Some(&other.base)).unwrap();
    let saved = std::fs::read(config::credentials_path(&hub.base).unwrap()).unwrap();
    assert!(
        complete
            .bind_destination_with_client(&repo, &url, &identity, client)
            .is_err()
    );
    assert_eq!(
        std::fs::read(config::credentials_path(&hub.base).unwrap()).unwrap(),
        saved
    );
    assert_eq!(
        credentials::load_checked(&hub.base)
            .unwrap()
            .unwrap()
            .username,
        "bob"
    );
    assert!(other.finish().is_empty());
    assert_eq!(hub.finish().len(), 1);
    println!("{COMPLETE}");
}
