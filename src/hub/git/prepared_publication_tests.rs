fn prepared_source(
    home: &IsolatedHome,
    hub: &str,
) -> (Repo, PublicationPlan, String, RemoteIdentity) {
    let source = source(home, hub);
    // Availability uses its own bounded request policy rather than native low-speed deadlines.
    for key in ["http.lowSpeedLimit", "http.lowSpeedTime"] {
        source.0.git(&["config", "--local", key, "0"]).unwrap();
    }
    source
}

fn prepared_pointer(body: &[u8]) -> crate::domain::lfs::Pointer {
    use sha2::Digest as _;
    crate::domain::lfs::Pointer {
        oid: hex::encode(sha2::Sha256::digest(body)),
        size: body.len() as u64,
    }
}

fn record_prepared_pointer(repo: &Repo, name: &str, pointer: &crate::domain::lfs::Pointer) {
    std::fs::write(
        repo.root().join(name),
        format!(
            "version {}\noid sha256:{}\nsize {}\n",
            crate::domain::lfs::VERSION,
            pointer.oid,
            pointer.size
        ),
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("Record selected payload pointer").unwrap();
}

fn prepared_plan(repo: &Repo) -> PublicationPlan {
    let branch = repo.git(&["symbolic-ref", "--short", "HEAD"]).unwrap();
    PublicationPlan::freeze(repo, &[branch.trim().to_owned()]).unwrap()
}

fn prepared_cache(
    repo: &Repo,
    pointer: &crate::domain::lfs::Pointer,
    bytes: &[u8],
) -> std::path::PathBuf {
    let path = repo
        .root()
        .join(".git/lfs/objects")
        .join(&pointer.oid[..2])
        .join(&pointer.oid[2..4])
        .join(&pointer.oid);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn prepared_batch(objects: serde_json::Value) -> Reply {
    Reply {
        status: 200,
        content_type: "application/vnd.git-lfs+json",
        headers: Vec::new(),
        body: serde_json::to_vec(&serde_json::json!({"objects": objects})).unwrap(),
    }
}

#[test]
fn prepared_owner_binds_captured_history_and_keeps_only_owned_missing_bytes() {
    if !isolated("prepared_owner_binds_captured_history_and_keeps_only_owned_missing_bytes") {
        return;
    }
    use crate::hub::git::PreparedPayloadAvailability;
    let home = IsolatedHome::new();
    let present = prepared_pointer(b"remote present, no local cache");
    let missing = prepared_pointer(b"original staged bytes");
    let (remote_present, remote_missing) = (present.clone(), missing.clone());
    let hub = FakeHub::new(move |request| {
        assert_eq!(request.path, "/alice/example.git/info/lfs/objects/batch");
        prepared_batch(serde_json::json!([
            {"oid": remote_missing.oid, "size": remote_missing.size, "actions": {"upload": {"href": "https://unused.invalid/upload"}}},
            {"oid": remote_present.oid, "size": remote_present.size}
        ]))
    });
    let (repo, _, url, identity) = prepared_source(&home, &hub.base);
    record_prepared_pointer(&repo, "removed", &present);
    repo.git(&["rm", "removed"]).unwrap();
    record_prepared_pointer(&repo, "selected", &missing);
    let cache = prepared_cache(&repo, &missing, b"original staged bytes");
    let plan = prepared_plan(&repo);
    let frozen = FrozenPublication::prepare(&repo, &plan, &url, &identity).unwrap();
    let later = prepared_pointer(b"later unselected payload");
    record_prepared_pointer(&repo, "later", &later);
    repo.git(&["config", "lfs.storage", "different-cache"])
        .unwrap();
    rewrite(repo.root(), &url, "https://unselected.invalid/another.git");
    let prepared = frozen.prepare_payloads(missing.size).unwrap();
    assert_eq!(prepared.url(), url);
    assert_eq!(prepared.identity(), &identity);
    assert_eq!(prepared.plan(), &plan);
    let mut expected = vec![present.clone(), missing.clone()];
    expected.sort_by(|a, b| a.oid.cmp(&b.oid));
    assert_eq!(prepared.pointers(), expected);
    assert_eq!(
        prepared.availability(&present).unwrap(),
        PreparedPayloadAvailability::RemotePresent
    );
    assert_eq!(
        prepared.availability(&missing).unwrap(),
        PreparedPayloadAvailability::Staged
    );
    assert!(prepared.open_payload(&present).is_err());
    assert!(prepared.open_payload(&later).is_err());
    let wrong_size = crate::domain::lfs::Pointer {
        size: missing.size + 1,
        ..missing.clone()
    };
    assert!(prepared.open_payload(&wrong_size).is_err());
    std::fs::write(&cache, b"changed original cache").unwrap();
    std::fs::remove_file(&cache).unwrap();
    let mut bytes = Vec::new();
    prepared
        .open_payload(&missing)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"original staged bytes");
    let requests = hub.finish();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.header("Authorization"),
        Some("Bearer fake-alice-access")
    );
    assert_eq!(
        request.header("X-AgentGit-Expected-Agent-Id"),
        Some(AGENT_ID)
    );
    assert_eq!(request.header("X-AgentGit-Accept-Secret-Findings"), None);
    assert_eq!(
        request.header("User-Agent"),
        Some("frozen-publication-fixture")
    );
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["operation"], "upload");
    assert_eq!(body["objects"], serde_json::to_value(expected).unwrap());
    assert!(!repo.root().join("different-cache").exists());
    println!("{COMPLETE}");
}

#[test]
fn prepared_availability_retries_with_captured_http_and_retained_account() {
    if !isolated("prepared_availability_retries_with_captured_http_and_retained_account") {
        return;
    }
    let home = IsolatedHome::new();
    let pointer = prepared_pointer(b"already remote");
    let returned = pointer.clone();
    let mut attempts = 0;
    let hub = FakeHub::new(move |request| {
        if request.path == "/api/auth/refresh" {
            return refreshed();
        }
        assert_eq!(request.path, "/alice/example.git/info/lfs/objects/batch");
        attempts += 1;
        if attempts == 1 {
            denied()
        } else {
            prepared_batch(serde_json::json!([returned]))
        }
    });
    let (repo, _, url, identity) = prepared_source(&home, &hub.base);
    record_prepared_pointer(&repo, "payload", &pointer);
    let frozen = FrozenPublication::prepare(&repo, &prepared_plan(&repo), &url, &identity).unwrap();
    config::set_global("hub.url", Some("https://unselected.invalid")).unwrap();
    repo.git(&["config", "http.userAgent", "changed after capture"])
        .unwrap();
    let prepared = frozen.prepare_payloads(0).unwrap();
    assert_eq!(prepared.url(), url);
    assert_eq!(prepared.pointers(), std::slice::from_ref(&pointer));
    assert!(prepared.open_payload(&pointer).is_err());
    assert!(!repo.root().join(".git/lfs").exists());
    let requests = hub.finish();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0].header("Authorization"),
        Some("Bearer fake-alice-access")
    );
    assert_eq!(requests[1].path, "/api/auth/refresh");
    assert_eq!(requests[1].header("Authorization"), None);
    assert_eq!(
        requests[2].header("Authorization"),
        Some("Bearer fake-alice-fresh-access")
    );
    for request in [&requests[0], &requests[2]] {
        assert_eq!(
            request.header("User-Agent"),
            Some("frozen-publication-fixture")
        );
        assert_eq!(
            request.header("X-AgentGit-Expected-Agent-Id"),
            Some(AGENT_ID)
        );
        assert_eq!(request.header("X-AgentGit-Accept-Secret-Findings"), None);
    }
    println!("{COMPLETE}");
}

#[test]
fn prepared_availability_and_staging_failures_cannot_return_an_owner() {
    if !isolated("prepared_availability_and_staging_failures_cannot_return_an_owner") {
        return;
    }
    for mode in [
        "unexpected",
        "size",
        "omitted",
        "missing",
        "corrupt",
        "budget",
    ] {
        let home = IsolatedHome::new();
        let pointer = prepared_pointer(b"valid payload");
        let returned = pointer.clone();
        let hub = FakeHub::new(move |request| {
            assert_eq!(request.path, "/alice/example.git/info/lfs/objects/batch");
            let objects = match mode {
                "unexpected" => serde_json::json!([prepared_pointer(b"foreign")]),
                "size" => serde_json::json!([{"oid": returned.oid, "size": returned.size + 1}]),
                "omitted" => serde_json::json!([]),
                _ => {
                    serde_json::json!([{"oid": returned.oid, "size": returned.size, "actions": {"upload": {}}}])
                }
            };
            prepared_batch(objects)
        });
        let (repo, _, url, identity) = prepared_source(&home, &hub.base);
        record_prepared_pointer(&repo, "payload", &pointer);
        if mode == "corrupt" {
            prepared_cache(&repo, &pointer, b"wrong bytes");
        }
        if mode == "budget" {
            prepared_cache(&repo, &pointer, b"valid payload");
        }
        let frozen =
            FrozenPublication::prepare(&repo, &prepared_plan(&repo), &url, &identity).unwrap();
        let budget = if mode == "budget" {
            pointer.size - 1
        } else {
            pointer.size
        };
        let error = frozen
            .prepare_payloads(budget)
            .err()
            .expect("preparation must fail closed");
        let expected = match mode {
            "unexpected" | "size" | "omitted" => "cannot prepare LFS availability",
            "missing" => "selected LFS payload is unavailable or unsupported",
            "corrupt" => "private LFS payload does not match its declared size and digest",
            "budget" => "LFS staging exceeds its object or byte budget",
            _ => unreachable!(),
        };
        assert_eq!(error.to_string(), expected);
        assert_eq!(hub.finish().len(), 1);
    }
    println!("{COMPLETE}");
}

#[test]
fn prepared_inventory_preserves_bare_and_gitfile_sources_and_refuses_invalid_pointer_data() {
    if !isolated(
        "prepared_inventory_preserves_bare_and_gitfile_sources_and_refuses_invalid_pointer_data",
    ) {
        return;
    }
    let home = IsolatedHome::new();
    let pointer = prepared_pointer(b"remote bare payload");
    let returned = pointer.clone();
    let hub = FakeHub::new(move |request| {
        assert_eq!(request.path, "/alice/example.git/info/lfs/objects/batch");
        prepared_batch(serde_json::json!([returned]))
    });
    let (repo, _, url, identity) = prepared_source(&home, &hub.base);
    let empty = FrozenPublication::prepare(&repo, &prepared_plan(&repo), &url, &identity)
        .unwrap()
        .prepare_payloads(0)
        .unwrap();
    assert!(empty.pointers().is_empty());
    record_prepared_pointer(&repo, "payload", &pointer);
    let plan = prepared_plan(&repo);
    for bare in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("source");
        let gitdir = directory.path().join("git-data");
        let mut command = Command::new("git");
        command.arg("clone").arg("--no-hardlinks");
        if bare {
            command.arg("--bare");
        } else {
            command.arg("--separate-git-dir").arg(&gitdir);
        }
        let cloned = command
            .arg(repo.root())
            .arg(&checkout)
            .env("GIT_ALLOW_PROTOCOL", "file")
            .output()
            .unwrap();
        assert!(cloned.status.success(), "owned source fixture: {cloned:?}");
        let selected = Repo::at(&checkout);
        let prepared = FrozenPublication::prepare(&selected, &plan, &url, &identity)
            .unwrap()
            .prepare_payloads(0)
            .unwrap();
        assert_eq!(prepared.plan(), &plan);
        assert_eq!(prepared.pointers(), std::slice::from_ref(&pointer));
        assert!(!checkout.join("lfs").exists());
        assert!(!gitdir.join("lfs").exists());
    }
    std::fs::write(
        repo.root().join("invalid"),
        "version https://git-lfs.github.com/spec/v1\noid sha256:invalid\nsize 1\n",
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("Record invalid pointer fixture").unwrap();
    assert!(FrozenPublication::prepare(&repo, &prepared_plan(&repo), &url, &identity).is_err());
    assert_eq!(hub.finish().len(), 2);
    println!("{COMPLETE}");
}

#[test]
fn inspection_retains_frozen_carriers_and_owned_payloads_without_live_fallback() {
    if !isolated("inspection_retains_frozen_carriers_and_owned_payloads_without_live_fallback") {
        return;
    }
    use crate::domain::secrets::ScanLimits;
    use crate::hub::git::PublicationInspection;
    let home = IsolatedHome::new();
    let secret = "AKIA4X7QZ2M5RT6VW3JH";
    let bytes = format!("key = {secret}\n").into_bytes();
    let pointer = prepared_pointer(&bytes);
    let remote = prepared_pointer(b"present without cache");
    let binary_bytes = b"\0\xffowned binary";
    let binary = prepared_pointer(binary_bytes);
    let remote_binary = binary.clone();
    let (missing, present) = (pointer.clone(), remote.clone());
    let hub = FakeHub::new(move |request| {
        assert_eq!(request.path, "/alice/example.git/info/lfs/objects/batch");
        prepared_batch(serde_json::json!([
            {"oid":missing.oid,"size":missing.size,"actions":{"upload":{}}}, present,
            {"oid":remote_binary.oid,"size":remote_binary.size,"actions":{"upload":{}}}
        ]))
    });
    let (repo, _, url, identity) = prepared_source(&home, &hub.base);
    let initial = repo.git(&["rev-parse", "HEAD"]).unwrap();
    std::fs::write(repo.root().join("removed.txt"), &bytes).unwrap();
    repo.add_all().unwrap();
    repo.commit(&format!("captured {secret}")).unwrap();
    repo.git(&["rm", "removed.txt"]).unwrap();
    record_prepared_pointer(&repo, "missing.lfs", &pointer);
    record_prepared_pointer(&repo, "present.lfs", &remote);
    std::fs::write(repo.root().join("binary.git"), binary_bytes).unwrap();
    record_prepared_pointer(&repo, "binary.lfs", &binary);
    repo.git(&[
        "-c",
        "tag.gpgsign=false",
        "tag",
        "-a",
        "captured",
        "-m",
        &format!("captured {secret}"),
    ])
    .unwrap();
    let cache = prepared_cache(&repo, &pointer, &bytes);
    let binary_cache = prepared_cache(&repo, &binary, binary_bytes);
    let plan = prepared_plan(&repo);
    let prepared = FrozenPublication::prepare(&repo, &plan, &url, &identity)
        .unwrap()
        .prepare_payloads(pointer.size + binary.size)
        .unwrap();
    repo.git(&["reset", "--hard", initial.trim()]).unwrap();
    repo.git(&["tag", "-d", "captured"]).unwrap();
    std::fs::remove_file(cache).unwrap();
    std::fs::remove_file(binary_cache).unwrap();
    std::fs::write(repo.root().join("live-only"), format!("key = {secret}\n")).unwrap();
    std::fs::write(
        config::agit_home()
            .unwrap()
            .join(crate::domain::secrets::ALLOWLIST_FILE),
        secret,
    )
    .unwrap();
    let result = prepared.inspect(ScanLimits::DEFAULT);
    let initial_summary = result.summary().render();
    let PublicationInspection::Complete(complete) = result else {
        panic!("captured contents must be fully inspected");
    };
    assert_eq!(complete.prepared().plan(), &plan);
    assert_eq!(
        complete.report().remote_present(),
        std::slice::from_ref(&remote)
    );
    assert!(complete.report().scan().unscanned.is_empty());
    assert!(!complete.report().scan().truncated);
    use crate::hub::git::inspection_summary::{
        DeterministicInspectionState, LfsInspectionCoverage, ModelReviewState,
    };
    let summary = complete.summary();
    assert_eq!(summary.target, url);
    assert_eq!(summary.identity, &identity);
    assert!(matches!(
        summary.deterministic,
        DeterministicInspectionState::Complete
    ));
    assert_eq!(summary.model_review, ModelReviewState::NotPerformed);
    for (observed, expected) in [(&summary.heads, plan.heads()), (&summary.tags, plan.tags())] {
        assert_eq!(observed.len(), expected.len());
        for (observed, expected) in observed.iter().zip(expected) {
            assert_eq!(observed.name, expected.name());
            assert_eq!(observed.oid, expected.oid());
        }
    }
    assert_eq!(summary.lfs.len(), 3);
    for (pointer, coverage) in [
        (&pointer, LfsInspectionCoverage::PrivacyExcluded),
        (&binary, LfsInspectionCoverage::PrivacyExcluded),
        (&remote, LfsInspectionCoverage::RemotePresentNotInspected),
    ] {
        let payload = summary
            .lfs
            .iter()
            .find(|payload| payload.pointer == pointer)
            .unwrap();
        assert_eq!(payload.coverage, coverage);
    }
    let encoded = serde_json::to_string(&summary).unwrap();
    assert!(!encoded.contains(secret));
    assert!(!encoded.contains("fingerprint"));
    assert!(!encoded.contains("fake-alice-access"));
    let rendered = summary.render();
    assert_eq!(rendered, initial_summary);
    assert!(!rendered.contains(secret));
    assert!(!rendered.contains("fake-alice-access"));
    assert!(rendered.contains("bytes not inspected"));
    assert!(rendered.contains("excluded from privacy processing"));
    assert!(rendered.contains("Model review: not performed"));
    assert_eq!(hub.finish().len(), 1);
    println!("{COMPLETE}");
}
