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
    use crate::domain::secrets::{ScanLimits, Source};
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
    assert!(complete.has_findings());
    assert_eq!(complete.prepared().plan(), &plan);
    for carrier in [Source::BlobObject, Source::CommitObject, Source::TagObject] {
        assert!(
            complete
                .report()
                .scan()
                .hits
                .iter()
                .any(|hit| hit.source == carrier),
            "missing captured carrier {carrier:?}"
        );
    }
    assert!(complete.report().scan().hits.iter().any(|hit| {
        hit.file
            .as_deref()
            .is_some_and(|file| file.starts_with("lfs object "))
    }));
    assert!(!complete.report().scan().hits.iter().any(|hit| {
        hit.file
            .as_deref()
            .is_some_and(|file| file.contains("live-only"))
    }));
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
    assert!(summary.binary_git_objects > 0);
    assert_eq!(summary.lfs.len(), 3);
    for (pointer, coverage) in [
        (&pointer, LfsInspectionCoverage::TextInspected),
        (&binary, LfsInspectionCoverage::BinaryNotScannedAsText),
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
    assert!(!summary.findings.is_empty());
    assert_eq!(summary.findings.len(), complete.report().scan().hits.len());
    let rendered = summary.render();
    assert_eq!(rendered, initial_summary);
    assert!(!rendered.contains(secret));
    assert!(!rendered.contains("fake-alice-access"));
    assert!(rendered.contains("bytes not inspected"));
    assert!(rendered.contains("not scanned as text"));
    assert!(rendered.contains("Model review: not performed"));
    assert_eq!(hub.finish().len(), 1);
    println!("{COMPLETE}");
}

#[test]
fn inspection_incomplete_owned_bytes_cannot_be_accepted_by_environment() {
    if !isolated("inspection_incomplete_owned_bytes_cannot_be_accepted_by_environment") {
        return;
    }
    use crate::domain::secrets::ScanLimits;
    use crate::hub::git::{InspectionFailure, PublicationInspection};
    for mode in [
        "short",
        "long",
        "hash",
        "binary-long",
        "text-limit",
        "budget",
    ] {
        let home = IsolatedHome::new();
        let bytes = if mode == "binary-long" {
            vec![0xff; 8192]
        } else {
            vec![b'x'; 8192]
        };
        let pointer = prepared_pointer(&bytes);
        let response = pointer.clone();
        let hub = FakeHub::new(move |_| {
            prepared_batch(serde_json::json!([
                {"oid":response.oid,"size":response.size,"actions":{"upload":{}}}
            ]))
        });
        let (repo, _, url, identity) = prepared_source(&home, &hub.base);
        record_prepared_pointer(&repo, "payload", &pointer);
        prepared_cache(&repo, &pointer, &bytes);
        let prepared = FrozenPublication::prepare(&repo, &prepared_plan(&repo), &url, &identity)
            .unwrap()
            .prepare_payloads(pointer.size)
            .unwrap();
        match mode {
            "short" => prepared.damage_staged_payload_for_test(&pointer, &bytes[..bytes.len() - 1]),
            "long" | "binary-long" => {
                let mut changed = bytes.clone();
                changed.push(0);
                prepared.damage_staged_payload_for_test(&pointer, &changed);
            }
            "hash" => {
                let mut changed = bytes.clone();
                changed[0] = b'y';
                prepared.damage_staged_payload_for_test(&pointer, &changed);
            }
            _ => (),
        }
        // This child process exclusively owns the acceptance environment and restores it per case.
        let prior = std::env::var_os("AGIT_ALLOW_SECRETS");
        unsafe {
            std::env::set_var("AGIT_ALLOW_SECRETS", "1");
        }
        let limits = ScanLimits {
            max_object_bytes: if mode == "text-limit" || mode == "binary-long" {
                1024
            } else {
                ScanLimits::DEFAULT.max_object_bytes
            },
            budget_bytes: if mode == "budget" {
                pointer.size
            } else {
                ScanLimits::DEFAULT.budget_bytes
            },
        };
        let result = prepared.inspect(limits);
        unsafe {
            match prior {
                Some(value) => std::env::set_var("AGIT_ALLOW_SECRETS", value),
                None => std::env::remove_var("AGIT_ALLOW_SECRETS"),
            }
        }
        let PublicationInspection::Blocked(blocked) = result else {
            panic!("incomplete {mode} content cannot be accepted");
        };
        assert_eq!(
            blocked.reason(),
            if matches!(mode, "text-limit" | "budget") {
                InspectionFailure::Incomplete
            } else {
                InspectionFailure::Content
            }
        );
        let summary = blocked.summary();
        assert!(matches!(summary.deterministic,
            crate::hub::git::inspection_summary::DeterministicInspectionState::Blocked { ref reason }
                if reason == &blocked.reason().to_string()
        ));
        assert_eq!(summary.lfs.len(), 1);
        assert_eq!(
            summary.lfs[0].coverage,
            crate::hub::git::inspection_summary::LfsInspectionCoverage::InspectionNotConfirmed
        );
        assert_eq!(
            summary.findings_truncated,
            blocked.report().scan().truncated
        );
        assert_eq!(
            summary.unscanned.oversized_objects,
            blocked.report().scan().unscanned.oversized.as_slice()
        );
        assert!(
            !summary
                .render()
                .contains("Deterministic inspection: complete")
        );
        assert_eq!(hub.finish().len(), 1);
    }
    println!("{COMPLETE}");
}

#[test]
fn inspection_policy_failure_is_deferred_and_bare_capture_preserves_its_carrier() {
    if !isolated("inspection_policy_failure_is_deferred_and_bare_capture_preserves_its_carrier") {
        return;
    }
    use crate::domain::secrets::ScanLimits;
    use crate::hub::git::{InspectionFailure, PublicationInspection};
    let home = IsolatedHome::new();
    let hub = FakeHub::new(|_| panic!("empty inventory must not make availability requests"));
    let (repo, plan, url, identity) = prepared_source(&home, &hub.base);
    let location = tempfile::tempdir().unwrap();
    let bare = location.path().join("bare");
    let output = Command::new("git")
        .args(["clone", "--bare", "--no-hardlinks"])
        .arg(repo.root())
        .arg(&bare)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .output()
        .unwrap();
    assert!(output.status.success());
    let bare_repo = Repo::at(&bare);
    let prepared = FrozenPublication::prepare(&bare_repo, &plan, &url, &identity)
        .unwrap()
        .prepare_payloads(0)
        .unwrap();
    assert!(
        !bare.join(".git").exists(),
        "policy capture must not create a nested carrier"
    );
    assert!(matches!(
        prepared.inspect(ScanLimits::DEFAULT),
        PublicationInspection::Complete(_)
    ));
    let allowlist = config::agit_home()
        .unwrap()
        .join(crate::domain::secrets::ALLOWLIST_FILE);
    std::fs::write(&allowlist, [0xff]).unwrap();
    let frozen = FrozenPublication::prepare(&repo, &plan, &url, &identity).unwrap();
    std::fs::remove_file(allowlist).unwrap();
    let PublicationInspection::Blocked(blocked) = frozen
        .prepare_payloads(0)
        .unwrap()
        .inspect(ScanLimits::DEFAULT)
    else {
        panic!("captured invalid policy must remain blocked");
    };
    assert_eq!(blocked.reason(), InspectionFailure::Configuration);
    assert_eq!(
        blocked.reason().to_string(),
        "publication inspection configuration is invalid"
    );
    assert!(hub.finish().is_empty());
    println!("{COMPLETE}");
}

#[test]
fn inspection_uses_one_hit_cap_across_git_and_owned_lfs() {
    if !isolated("inspection_uses_one_hit_cap_across_git_and_owned_lfs") {
        return;
    }
    use crate::domain::secrets::ScanLimits;
    use crate::hub::git::{InspectionFailure, PublicationInspection};
    let home = IsolatedHome::new();
    let line = "key = AKIA4X7QZ2M5RT6VW3JH\n";
    let bytes = line.repeat(20).into_bytes();
    let pointer = prepared_pointer(&bytes);
    let response = pointer.clone();
    let hub = FakeHub::new(move |_| {
        prepared_batch(serde_json::json!([
            {"oid":response.oid,"size":response.size,"actions":{"upload":{}}}
        ]))
    });
    let (repo, _, url, identity) = prepared_source(&home, &hub.base);
    std::fs::write(repo.root().join("git-findings"), line.repeat(190)).unwrap();
    record_prepared_pointer(&repo, "payload", &pointer);
    prepared_cache(&repo, &pointer, &bytes);
    let prepared = FrozenPublication::prepare(&repo, &prepared_plan(&repo), &url, &identity)
        .unwrap()
        .prepare_payloads(pointer.size)
        .unwrap();
    let PublicationInspection::Blocked(blocked) = prepared.inspect(ScanLimits::DEFAULT) else {
        panic!("a later carrier must not receive a reset hit cap");
    };
    assert_eq!(blocked.reason(), InspectionFailure::Incomplete);
    assert!(blocked.report().scan().truncated);
    let summary = blocked.summary();
    assert!(summary.findings_truncated);
    assert_eq!(summary.findings.len(), blocked.report().scan().hits.len());
    assert!(summary.render().contains("Findings truncated: true"));
    assert!(blocked.report().scan().hits.iter().any(|hit| {
        hit.file
            .as_deref()
            .is_some_and(|file| file.starts_with("lfs object "))
    }));
    assert_eq!(hub.finish().len(), 1);
    println!("{COMPLETE}");
}

#[cfg(all(feature = "secret-vault", unix))]
#[test]
fn inspection_captures_the_selected_bare_dictionary_before_private_reads() {
    if !isolated("inspection_captures_the_selected_bare_dictionary_before_private_reads") {
        return;
    }
    use crate::domain::secret_filter::{Matcher, RepositoryDictionary};
    use crate::domain::secrets::ScanLimits;
    use crate::hub::git::PublicationInspection;
    let home = IsolatedHome::new();
    let hub = FakeHub::new(|_| panic!("Git-only inspection must not make availability requests"));
    let (repo, _, url, identity) = prepared_source(&home, &hub.base);
    let secret = "blue \"horse\" battery";
    std::fs::write(
        repo.root().join("payload.json"),
        serde_json::json!({"value": secret}).to_string(),
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("Record registered payload fixture").unwrap();
    let plan = prepared_plan(&repo);
    let location = tempfile::tempdir().unwrap();
    let bare = location.path().join("bare");
    let cloned = Command::new("git")
        .args(["clone", "--bare", "--no-hardlinks"])
        .arg(repo.root())
        .arg(&bare)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .output()
        .unwrap();
    assert!(cloned.status.success());
    let dictionary = RepositoryDictionary::open_at_git_dir(&bare).unwrap();
    let learned = dictionary
        .protect_text(secret, &Matcher::for_test(&[("sec_bare", secret)]))
        .unwrap();
    assert!(learned.replacements > 0);
    let prepared = FrozenPublication::prepare(&Repo::at(&bare), &plan, &url, &identity)
        .unwrap()
        .prepare_payloads(0)
        .unwrap();
    std::fs::remove_dir_all(bare.join("agit/secret-dictionary")).unwrap();
    assert!(!bare.join(".git").exists());
    let PublicationInspection::Complete(complete) = prepared.inspect(ScanLimits::DEFAULT) else {
        panic!("captured dictionary policy remains readable");
    };
    assert!(
        complete
            .report()
            .scan()
            .hits
            .iter()
            .any(|hit| hit.rule == "registered-secret")
    );
    assert!(hub.finish().is_empty());
    println!("{COMPLETE}");
}

#[test]
fn inspection_summary_keeps_unvisited_lfs_inventory_after_policy_or_git_failure() {
    if !isolated("inspection_summary_keeps_unvisited_lfs_inventory_after_policy_or_git_failure") {
        return;
    }
    use crate::domain::secrets::ScanLimits;
    use crate::hub::git::inspection_summary::{
        DeterministicInspectionState, LfsInspectionCoverage,
    };
    use crate::hub::git::{InspectionFailure, PublicationInspection};
    for invalid_policy in [true, false] {
        let home = IsolatedHome::new();
        let bytes = b"\0\xffunvisited staged binary";
        let staged = prepared_pointer(bytes);
        let present = prepared_pointer(b"remote payload without local bytes");
        let (missing, remote) = (staged.clone(), present.clone());
        let hub = FakeHub::new(move |_| {
            prepared_batch(serde_json::json!([
                {"oid":missing.oid,"size":missing.size,"actions":{"upload":{}}}, remote
            ]))
        });
        let (repo, _, url, identity) = prepared_source(&home, &hub.base);
        record_prepared_pointer(&repo, "missing.lfs", &staged);
        record_prepared_pointer(&repo, "present.lfs", &present);
        prepared_cache(&repo, &staged, bytes);
        let allowlist = config::agit_home()
            .unwrap()
            .join(crate::domain::secrets::ALLOWLIST_FILE);
        if invalid_policy {
            std::fs::write(&allowlist, [0xff]).unwrap();
        }
        let frozen =
            FrozenPublication::prepare(&repo, &prepared_plan(&repo), &url, &identity).unwrap();
        if invalid_policy {
            std::fs::remove_file(allowlist).unwrap();
        }
        let prepared = frozen.prepare_payloads(staged.size).unwrap();
        let result = prepared.inspect(ScanLimits {
            budget_bytes: 0,
            ..ScanLimits::DEFAULT
        });
        let before = result.summary().render();
        let PublicationInspection::Blocked(blocked) = result else {
            panic!("early inspection failure must retain a blocked owner");
        };
        assert_eq!(
            blocked.reason(),
            if invalid_policy {
                InspectionFailure::Configuration
            } else {
                InspectionFailure::Incomplete
            }
        );
        assert!(blocked.report().scan().hits.is_empty());
        assert!(blocked.report().remote_present().is_empty());
        assert!(blocked.report().binary_lfs().is_empty());
        if invalid_policy {
            assert!(blocked.report().scan().unscanned.is_empty());
        }
        let summary = blocked.summary();
        assert!(matches!(
            summary.deterministic,
            DeterministicInspectionState::Blocked { .. }
        ));
        assert_eq!(summary.lfs.len(), 2);
        for (pointer, coverage) in [
            (&staged, LfsInspectionCoverage::InspectionNotConfirmed),
            (&present, LfsInspectionCoverage::RemotePresentNotInspected),
        ] {
            let item = summary
                .lfs
                .iter()
                .find(|item| item.pointer == pointer)
                .unwrap();
            assert_eq!(item.coverage, coverage);
        }
        assert_eq!(summary.render(), before);
        assert!(!before.contains("owned text; deterministic inspection complete"));
        assert!(!before.contains("owned binary; not scanned as text"));
        assert!(before.contains("remote present; bytes not inspected"));
        assert!(before.contains("inspection not fully confirmed"));
        assert_eq!(hub.finish().len(), 1);
    }
    println!("{COMPLETE}");
}
