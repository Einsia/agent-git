use super::*;

fn insert_blob(
    builder: &mut Builder<'_>,
    paths: &mut BTreeMap<String, String>,
    path: &str,
    body: &[u8],
) -> String {
    let oid = git_digest("blob", body, 40).unwrap();
    builder.reserve(body.len()).unwrap();
    builder
        .item(&object_id(&oid, "blob"), "blob", &oid, body, false)
        .unwrap();
    builder
        .objects
        .insert(oid.clone(), ("blob".into(), body.into()));
    paths.insert(path.into(), oid.clone());
    oid
}

fn session_meta() -> meta::Meta {
    let mut snapshot = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        "/fixture".into(),
    );
    snapshot.turn = Some(1);
    snapshot
}

fn envelope(content: Value) -> crate::domain::transcript::Envelope {
    crate::domain::transcript::Envelope {
        source: "claude-code".into(),
        session_id: format!("agit-{}", "a".repeat(40)),
        object_hash: crate::domain::transcript::object_hash(&content),
        content,
    }
}

#[test]
fn utf8_chunks_preserve_every_byte_of_a_large_single_record() {
    let text = format!("{{\"value\":\"{}\"}}\n", "\u{1f600}".repeat(CHUNK_BYTES));
    let extents = chunk_extents(&text);
    let mut output = Vec::new();
    for (start, end) in &extents {
        assert!(*end - *start <= CHUNK_BYTES);
        assert!(text.is_char_boundary(*start) && text.is_char_boundary(*end));
        output.extend_from_slice(&text.as_bytes()[*start..*end]);
    }
    assert_eq!(output, text.as_bytes());
    assert_eq!(extents.first().unwrap().0, 0);
    assert_eq!(extents.last().unwrap().1, text.len());
    assert_eq!(chunk_extents(""), vec![(0, 0)]);
}

#[test]
fn tree_names_are_literal_context_and_object_framing_is_strict() {
    let oid = vec![0xabu8; 20];
    let mut body = b"100644 notes\nwith\ttabs\0".to_vec();
    body.extend_from_slice(&oid);
    let entries = tree_entries(&body, 40).unwrap();
    assert_eq!(entries[0].name, "notes\nwith\ttabs");
    assert_eq!(entries[0].oid, "ab".repeat(20));
    let mut truncated = body.clone();
    truncated.pop();
    assert!(tree_entries(&truncated, 40).is_err());
    let mut duplicate = body.clone();
    duplicate.extend_from_slice(&body);
    assert!(tree_entries(&duplicate, 40).is_err());
    let mut traversal = b"100644 ../outside\0".to_vec();
    traversal.extend_from_slice(&oid);
    assert!(tree_entries(&traversal, 40).is_err());
}

#[test]
fn log_mapping_preserves_repeated_hidden_events_and_all_envelope_carriers() {
    let directory = tempfile::tempdir().unwrap();
    let mut builder = Builder::new(directory.path());
    let mut paths = BTreeMap::new();
    let snapshot = session_meta();
    insert_blob(
        &mut builder,
        &mut paths,
        meta::FILE,
        &serde_json::to_vec(&snapshot).unwrap(),
    );
    let line = storage::envelope_line(&envelope(
        json!({"type":"user","text":"Hidden customer context"}),
    ));
    let id = storage::event_id(&line).unwrap();
    let event_oid = insert_blob(
        &mut builder,
        &mut paths,
        &meta::event_path(&id).unwrap(),
        line.as_bytes(),
    );
    insert_blob(
        &mut builder,
        &mut paths,
        meta::LOG_FILE,
        format!("{id}\n{id}\n").as_bytes(),
    );
    insert_blob(&mut builder, &mut paths, meta::VIEW_FILE, b"");
    let mapping = builder.snapshot(&"b".repeat(40), &paths).unwrap();
    assert_eq!(mapping["kind"], "session");
    assert_eq!(mapping["records"].as_array().unwrap().len(), 2);
    assert_eq!(
        mapping["records"][0]["carrier_item"],
        object_id(&event_oid, "blob")
    );
    assert_eq!(
        mapping["records"][1]["carrier_item"],
        mapping["records"][0]["carrier_item"]
    );
    let native = crate::domain::transcript::unwrap_strict(&format!("{line}{line}")).unwrap();
    assert_eq!(mapping["native_bytes"], native.len());
    assert_eq!(mapping["native_sha256"], digest(native.as_bytes()));
    let raw = builder
        .items
        .iter()
        .find(|item| item["id"] == object_id(&event_oid, "blob"))
        .unwrap();
    let bytes = std::fs::read(raw["chunks"][0]["path"].as_str().unwrap()).unwrap();
    assert_eq!(bytes, line.as_bytes());
}

#[test]
fn missing_or_corrupt_declared_log_cannot_become_complete_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let mut builder = Builder::new(directory.path());
    let mut paths = BTreeMap::new();
    insert_blob(
        &mut builder,
        &mut paths,
        meta::FILE,
        &serde_json::to_vec(&session_meta()).unwrap(),
    );
    assert!(builder.snapshot(&"b".repeat(40), &paths).is_err());
    insert_blob(
        &mut builder,
        &mut paths,
        meta::LOG_FILE,
        b"not-an-event-id\n",
    );
    assert!(builder.snapshot(&"b".repeat(40), &paths).is_err());
    let fake_id = "c".repeat(40);
    insert_blob(
        &mut builder,
        &mut paths,
        meta::LOG_FILE,
        format!("{fake_id}\n").as_bytes(),
    );
    let line = storage::envelope_line(&envelope(json!({"text":"Another event"})));
    insert_blob(
        &mut builder,
        &mut paths,
        &meta::event_path(&fake_id).unwrap(),
        line.as_bytes(),
    );
    assert!(builder.snapshot(&"b".repeat(40), &paths).is_err());
}

#[test]
fn metadata_controls_file_line_birth_and_legacy_session_classification() {
    for (snapshot, kind) in [
        (meta::Meta::new_file_line(), "file_line"),
        (
            meta::Meta::new_session_line("claude-code".into(), "/fixture".into()),
            "session_birth",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut builder = Builder::new(directory.path());
        let mut paths = BTreeMap::new();
        insert_blob(
            &mut builder,
            &mut paths,
            meta::FILE,
            &serde_json::to_vec(&snapshot).unwrap(),
        );
        assert_eq!(
            builder.snapshot(&"b".repeat(40), &paths).unwrap()["kind"],
            kind
        );
    }
    let directory = tempfile::tempdir().unwrap();
    let mut builder = Builder::new(directory.path());
    let mut paths = BTreeMap::new();
    let mut snapshot = session_meta();
    snapshot.layout = meta::LayoutVersion::V0;
    insert_blob(
        &mut builder,
        &mut paths,
        meta::FILE,
        &serde_json::to_vec(&snapshot).unwrap(),
    );
    let line = storage::envelope_line(&envelope(json!({"text":"Legacy full LOG"})));
    let oid = insert_blob(
        &mut builder,
        &mut paths,
        meta::LEGACY_LOG_FILE,
        line.as_bytes(),
    );
    let mapping = builder.snapshot(&"b".repeat(40), &paths).unwrap();
    assert_eq!(
        mapping["records"][0]["carrier_item"],
        object_id(&oid, "blob")
    );
    assert_eq!(
        mapping["native_sha256"],
        digest(
            crate::domain::transcript::unwrap_strict(&line)
                .unwrap()
                .as_bytes()
        )
    );
}

#[test]
fn wrapper_has_detached_identity_and_no_live_source_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let git = directory.path().join("agit-home/repos/audit/source/.git");
    let objects = directory.path().join("captured-objects");
    let lfs = directory.path().join("captured-lfs");
    std::fs::create_dir(&objects).unwrap();
    std::fs::create_dir(&lfs).unwrap();
    let head = "a".repeat(64);
    write_wrapper(&git, &head, &objects, &lfs).unwrap();
    assert_eq!(
        std::fs::read_to_string(git.join("HEAD")).unwrap(),
        format!("{head}\n")
    );
    assert_eq!(git.join("refs").read_dir().unwrap().count(), 0);
    let config = std::fs::read_to_string(git.join("config")).unwrap();
    assert!(config.contains("objectformat = sha256"));
    assert!(!config.contains("remote") && !config.contains("include"));
    assert!(config.contains(&git_quote(&path_text(&lfs).unwrap())));
}

fn fixture_workspace() -> AuditWorkspace {
    let directory = tempfile::tempdir().unwrap();
    write_owned(&directory.path().join("evidence/manifest.json"), b"{}").unwrap();
    write_owned(
        &directory
            .path()
            .join("agit-home/repos/audit/source/.git/config"),
        b"[core]\nbare = false\n",
    )
    .unwrap();
    let mut inputs = BTreeMap::new();
    let mut remaining = MAX_BYTES;
    inventory(
        &directory.path().join("evidence"),
        &mut inputs,
        None,
        &mut remaining,
    )
    .unwrap();
    inventory(
        &directory.path().join("agit-home"),
        &mut inputs,
        None,
        &mut remaining,
    )
    .unwrap();
    AuditWorkspace {
        directory,
        binding: ManifestBinding {
            audit_id: "fixture".into(),
            manifest_sha256: "a".repeat(64),
        },
        expected: vec![],
        manifest: json!({}),
        read_access: json!({}),
        inputs,
    }
}

#[test]
fn verification_allows_caller_siblings_but_rejects_input_and_wrapper_changes() {
    let workspace = fixture_workspace();
    write_owned(&workspace.path().join("workflow.md"), b"Caller workflow").unwrap();
    write_owned(&workspace.path().join("audit-report.json"), b"{}").unwrap();
    workspace.verify().unwrap();
    std::fs::write(workspace.path().join("evidence/manifest.json"), b"[]").unwrap();
    assert!(workspace.verify().is_err());
    let workspace = fixture_workspace();
    write_owned(
        &workspace
            .path()
            .join("agit-home/repos/audit/source/.git/refs/heads/injected"),
        b"unreviewed",
    )
    .unwrap();
    assert!(workspace.verify().is_err());
}

#[cfg(unix)]
#[test]
fn verification_rejects_symlink_inputs_without_following_them() {
    use std::os::unix::fs::symlink;
    let workspace = fixture_workspace();
    let input = workspace.path().join("evidence/manifest.json");
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), b"{}").unwrap();
    std::fs::remove_file(&input).unwrap();
    symlink(outside.path(), &input).unwrap();
    assert!(workspace.verify().is_err());
    assert!(open_regular(&input).is_err());
}

#[test]
fn undecodable_and_unknown_text_cannot_be_excluded_as_verified_binary() {
    let directory = tempfile::tempdir().unwrap();
    let mut builder = Builder::new(directory.path());
    let utf16 = [
        b"\xff\xfe".as_slice(),
        &"Private customer identity"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    ]
    .concat();
    let cases = [
        utf16,
        b"private customer: \xff\xfetail".to_vec(),
        b"private\0customer".to_vec(),
        b"\x89PNG\r\n\x1a\nprivate text".to_vec(),
    ];
    for bytes in cases {
        let oid = digest(&bytes);
        builder.item(&oid, "blob", &oid, &bytes, false).unwrap();
        assert!(matches!(
            builder.expected.last().unwrap().kind,
            ExpectedKind::Unavailable
        ));
        assert_eq!(
            builder.items.last().unwrap()["classification"],
            "unavailable"
        );
        let pointer = lfs::Pointer {
            oid,
            size: bytes.len() as u64,
        };
        builder.payload(bytes.as_slice(), &pointer).unwrap();
        assert!(matches!(
            builder.expected.last().unwrap().kind,
            ExpectedKind::Unavailable
        ));
    }
    let image = hex::decode("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000b49444154789c6360000200000500017a5eab3f0000000049454e44ae426082").unwrap();
    builder
        .item("png", "blob", &digest(&image), &image, false)
        .unwrap();
    assert!(matches!(
        builder.expected.last().unwrap().kind,
        ExpectedKind::VerifiedBinary
    ));
    assert_eq!(builder.items.last().unwrap()["binary_format"], "image/png");
    let pointer = lfs::Pointer {
        oid: digest(&image),
        size: image.len() as u64,
    };
    builder.payload(image.as_slice(), &pointer).unwrap();
    assert!(matches!(
        builder.expected.last().unwrap().kind,
        ExpectedKind::VerifiedBinary
    ));
    assert_eq!(builder.items.last().unwrap()["binary_format"], "image/png");
}

#[test]
fn lfs_original_bytes_remain_hash_and_size_checked_before_classification() {
    let directory = tempfile::tempdir().unwrap();
    let mut builder = Builder::new(directory.path());
    let bytes = b"private customer: \xff";
    let pointer = lfs::Pointer {
        oid: digest(bytes),
        size: bytes.len() as u64,
    };
    let mut changed = bytes.to_vec();
    changed[0] ^= 1;
    assert!(builder.payload(changed.as_slice(), &pointer).is_err());
    assert!(
        builder
            .payload(&bytes[..bytes.len() - 1], &pointer)
            .is_err()
    );
    assert!(builder.expected.is_empty());
    builder.payload(bytes.as_slice(), &pointer).unwrap();
    assert!(matches!(
        builder.expected[0].kind,
        ExpectedKind::Unavailable
    ));
}

#[test]
fn session_lines_with_empty_identity_still_map_their_recorded_log() {
    for layout in [meta::LayoutVersion::V0, meta::LayoutVersion::V1] {
        let directory = tempfile::tempdir().unwrap();
        let mut builder = Builder::new(directory.path());
        let mut paths = BTreeMap::new();
        let mut snapshot = meta::Meta::new_session_line("claude-code".into(), "/fixture".into());
        snapshot.layout = layout;
        assert!(snapshot.session.is_empty());
        insert_blob(
            &mut builder,
            &mut paths,
            meta::FILE,
            &serde_json::to_vec(&snapshot).unwrap(),
        );
        assert_eq!(
            builder.snapshot(&"b".repeat(40), &paths).unwrap()["kind"],
            "session_birth"
        );
        let line = storage::envelope_line(&envelope(
            json!({"text":"Recorded full LOG without a session identity"}),
        ));
        let carrier = match layout {
            meta::LayoutVersion::V0 => insert_blob(
                &mut builder,
                &mut paths,
                meta::LEGACY_LOG_FILE,
                line.as_bytes(),
            ),
            meta::LayoutVersion::V1 => {
                let id = storage::event_id(&line).unwrap();
                let carrier = insert_blob(
                    &mut builder,
                    &mut paths,
                    &meta::event_path(&id).unwrap(),
                    line.as_bytes(),
                );
                insert_blob(
                    &mut builder,
                    &mut paths,
                    meta::LOG_FILE,
                    format!("{id}\n").as_bytes(),
                );
                carrier
            }
        };
        let mapping = builder.snapshot(&"b".repeat(40), &paths).unwrap();
        assert_eq!(mapping["kind"], "session");
        assert_eq!(mapping["records"].as_array().unwrap().len(), 1);
        assert_eq!(
            mapping["records"][0]["carrier_item"],
            object_id(&carrier, "blob")
        );
        let native = crate::domain::transcript::unwrap_strict(&line).unwrap();
        assert_eq!(mapping["native_bytes"], native.len());
        assert_eq!(mapping["native_sha256"], digest(native.as_bytes()));
    }
}

#[test]
fn readable_image_shaped_text_and_malformed_containers_cannot_skip_review() {
    let directory = tempfile::tempdir().unwrap();
    let mut builder = Builder::new(directory.path());
    let text = b"GIF89a private customer account;";
    builder
        .item("text", "blob", &digest(text), text, false)
        .unwrap();
    assert!(matches!(
        builder.expected.last().unwrap().kind,
        ExpectedKind::Text { .. }
    ));
    let malformed = [
        b"\xff\xd8\xffprivate customer account\xff\xd9".as_slice(),
        b"\x89PNG\r\n\x1a\n\0\0\0\rIHDRprivate customer account\0\0\0\0IEND\xaeB`\x82".as_slice(),
    ];
    for (ordinal, bytes) in malformed.into_iter().enumerate() {
        builder
            .item(
                &format!("unknown-{ordinal}"),
                "blob",
                &digest(bytes),
                bytes,
                false,
            )
            .unwrap();
        assert!(matches!(
            builder.expected.last().unwrap().kind,
            ExpectedKind::Unavailable
        ));
    }
    let image = hex::decode("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000b49444154789c6360000200000500017a5eab3f0000000049454e44ae426082").unwrap();
    let mut remaining = MAX_BYTES;
    assert_eq!(binary_format(&image, &mut remaining), Some("image/png"));
    assert_eq!(remaining, MAX_BYTES - 5);
    let mut corrupted = image.clone();
    corrupted[43] ^= 1;
    assert_eq!(binary_format(&corrupted, &mut remaining), None);
    let mut trailing = image.clone();
    trailing.extend_from_slice(b"private customer account");
    assert_eq!(binary_format(&trailing, &mut remaining), None);
    assert_eq!(binary_format(&image, &mut 4), None);
    for invalid in [
        "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000b49444154789c6260000200000500016d25bf7c0000000049454e44ae426082",
        "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000b49444154789c6365000200001e0006bca97c690000000049454e44ae426082",
        "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000a49444154789c636000000002000148afa4710000000049454e44ae426082",
    ] {
        assert_eq!(
            binary_format(&hex::decode(invalid).unwrap(), &mut remaining),
            None
        );
    }
}

#[cfg(windows)]
#[test]
fn wrapper_git_paths_use_windows_transport_spelling() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let git = root.join("agit-home/repos/audit/source/.git");
    let objects = Path::new(r"\\?\C:\captured objects");
    let lfs = Path::new(r"\\?\UNC\host\share\captured-lfs");
    write_wrapper(&git, &"a".repeat(40), objects, lfs).unwrap();
    assert_eq!(
        std::fs::read_to_string(git.join("objects/info/alternates")).unwrap(),
        format!("{}\n", git_quote(r"C:\captured objects"))
    );
    let config = std::fs::read_to_string(git.join("config")).unwrap();
    assert!(config.contains(&format!(
        "storage = {}",
        git_quote(r"\\host\share\captured-lfs")
    )));
    let hooks = crate::domain::repo::inspection_git_path_spelling(git.join("hooks"));
    assert!(config.contains(&format!(
        "hooksPath = {}",
        git_quote(&path_text(&hooks).unwrap())
    )));
    assert!(!config.contains(r"\\\\?\\"));
}
