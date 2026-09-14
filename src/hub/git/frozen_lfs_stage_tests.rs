use super::*;
use sha2::{Digest, Sha256};

fn make_pointer(body: &[u8]) -> Pointer {
    Pointer {
        oid: hex::encode(Sha256::digest(body)),
        size: body.len() as u64,
    }
}

fn cache(body: &[u8]) -> (TempDir, Pointer) {
    let cache = tempfile::tempdir().unwrap();
    let pointer = make_pointer(body);
    let path = object_path(cache.path(), &pointer);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    (cache, pointer)
}

fn bytes(staged: &StagedLfsPayloads, pointer: &Pointer) -> Vec<u8> {
    let mut body = Vec::new();
    staged
        .open_payload(pointer)
        .unwrap()
        .read_to_end(&mut body)
        .unwrap();
    body
}

#[test]
fn staged_copies_survive_source_changes_and_drop_with_their_owner() {
    let body = b"private selected payload";
    let (source, pointer) = cache(body);
    let output = tempfile::tempdir().unwrap();
    let output_path = output.path().to_owned();
    let staged = StagedLfsPayloads::stage(
        source.path(),
        &[pointer.clone(), pointer.clone()],
        pointer.size,
        output,
    )
    .unwrap();
    assert_eq!(staged.pointers(), std::slice::from_ref(&pointer));
    assert_eq!(
        std::fs::read(object_path(source.path(), &pointer)).unwrap(),
        body
    );
    std::fs::write(object_path(source.path(), &pointer), b"changed").unwrap();
    assert_eq!(bytes(&staged, &pointer), body);
    std::fs::remove_file(object_path(source.path(), &pointer)).unwrap();
    assert_eq!(bytes(&staged, &pointer), body);
    let unselected = Pointer {
        size: pointer.size + 1,
        ..pointer.clone()
    };
    assert_eq!(
        staged.open_payload(&unselected).err().unwrap(),
        LfsStagingFailure::Pointers
    );
    let unknown = make_pointer(b"valid but unselected payload");
    unknown.validate().unwrap();
    assert_ne!(unknown.oid, pointer.oid);
    assert_eq!(
        staged.open_payload(&unknown).err().unwrap(),
        LfsStagingFailure::Pointers
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            staged.storage().metadata().unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(
            object_path(&staged.storage().join("objects"), &pointer)
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
    }
    drop(staged);
    assert!(!output_path.exists());
    assert!(source.path().is_dir());
}

#[test]
fn pointer_and_budget_failures_precede_source_access_and_output_writes() {
    let original = make_pointer(b"payload");
    let conflict = Pointer {
        size: original.size + 1,
        ..original.clone()
    };
    let malformed = Pointer {
        oid: "../PRIVATE_raw_path".into(),
        size: 1,
    };
    let empty_wrong_size = Pointer {
        oid: EMPTY_OID.into(),
        size: 1,
    };
    let nonempty_zero_size = Pointer {
        size: 0,
        ..original.clone()
    };
    let huge = |digit: char| Pointer {
        oid: digit.to_string().repeat(64),
        size: i64::MAX as u64,
    };
    for (selected, budget, expected) in [
        (
            vec![original.clone(), conflict],
            u64::MAX,
            LfsStagingFailure::Pointers,
        ),
        (vec![malformed], u64::MAX, LfsStagingFailure::Pointers),
        (
            vec![empty_wrong_size],
            u64::MAX,
            LfsStagingFailure::Pointers,
        ),
        (
            vec![nonempty_zero_size],
            u64::MAX,
            LfsStagingFailure::Pointers,
        ),
        (
            vec![original.clone()],
            original.size - 1,
            LfsStagingFailure::Budget,
        ),
        (
            vec![huge('a'), huge('b'), huge('c')],
            u64::MAX,
            LfsStagingFailure::Budget,
        ),
        (vec![original; 10_001], u64::MAX, LfsStagingFailure::Budget),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let error = StagedLfsPayloads::stage(
            Path::new("not-an-absolute-cache"),
            &selected,
            budget,
            directory,
        )
        .unwrap_err();
        assert_eq!(error.failure(), expected);
        assert_eq!(format!("{error}"), expected.to_string());
        assert_eq!(format!("{error:?}"), format!("{expected:?}"));
        assert!(path.exists());
        assert!(path.read_dir().unwrap().next().is_none());
        let directory = error.into_directory();
        assert_eq!(directory.path(), path);
    }
}

#[test]
fn empty_objects_and_empty_selection_do_not_require_a_cache_entry() {
    let parent = tempfile::tempdir().unwrap();
    let missing = parent.path().join("absent-original-cache");
    for selected in [vec![], vec![make_pointer(b"")]] {
        let staged =
            StagedLfsPayloads::stage(&missing, &selected, 0, tempfile::tempdir().unwrap()).unwrap();
        assert_eq!(staged.pointers(), selected);
        if let Some(pointer) = selected.first() {
            assert_eq!(bytes(&staged, pointer), b"");
            assert!(object_path(&staged.storage().join("objects"), pointer).is_file());
        }
        assert!(!missing.exists());
    }
}

#[test]
fn invalid_destinations_return_ownership_without_mutating_contents() {
    let (source, pointer) = cache(b"payload");
    let destination = tempfile::tempdir().unwrap();
    let sentinel = destination.path().join("caller-owned");
    std::fs::write(&sentinel, b"preserve").unwrap();
    let error = StagedLfsPayloads::stage(
        source.path(),
        std::slice::from_ref(&pointer),
        pointer.size,
        destination,
    )
    .unwrap_err();
    assert_eq!(error.failure(), LfsStagingFailure::Destination);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserve");
    let recovered = error.into_directory();
    assert_eq!(
        std::fs::read(recovered.path().join("caller-owned")).unwrap(),
        b"preserve"
    );
    let nested = tempfile::tempdir_in(source.path()).unwrap();
    let nested_path = nested.path().to_owned();
    let error = StagedLfsPayloads::stage(
        source.path(),
        std::slice::from_ref(&pointer),
        pointer.size,
        nested,
    )
    .unwrap_err();
    assert_eq!(error.failure(), LfsStagingFailure::Overlap);
    assert!(nested_path.read_dir().unwrap().next().is_none());
    assert_eq!(
        std::fs::read(object_path(source.path(), &pointer)).unwrap(),
        b"payload"
    );
    for selected in [vec![], vec![make_pointer(b"")]] {
        let same = tempfile::tempdir().unwrap();
        let same_path = same.path().to_owned();
        let error = StagedLfsPayloads::stage(&same_path, &selected, 0, same).unwrap_err();
        assert_eq!(error.failure(), LfsStagingFailure::Overlap);
        assert!(same_path.read_dir().unwrap().next().is_none());
        let parent = tempfile::tempdir().unwrap();
        let child = tempfile::tempdir_in(parent.path()).unwrap();
        let child_path = child.path().to_owned();
        let error = StagedLfsPayloads::stage(parent.path(), &selected, 0, child).unwrap_err();
        assert_eq!(error.failure(), LfsStagingFailure::Overlap);
        assert!(child_path.read_dir().unwrap().next().is_none());
    }
}

#[test]
fn bad_bytes_never_return_a_verified_staging_result() {
    let body = b"expected";
    for bad in [b"expecte".as_slice(), b"expected-extra", b"corrupt!"] {
        let (source, pointer) = cache(body);
        std::fs::write(object_path(source.path(), &pointer), bad).unwrap();
        let destination = tempfile::tempdir().unwrap();
        let path = destination.path().to_owned();
        let error = StagedLfsPayloads::stage(
            source.path(),
            std::slice::from_ref(&pointer),
            pointer.size,
            destination,
        )
        .unwrap_err();
        assert_eq!(error.failure(), LfsStagingFailure::Integrity);
        let staged = object_path(&path.join("storage/objects"), &pointer);
        assert!(staged.metadata().unwrap().len() <= pointer.size);
        assert_eq!(
            std::fs::read(object_path(source.path(), &pointer)).unwrap(),
            bad
        );
        drop(error);
        assert!(!path.exists());
    }
}

#[test]
fn later_object_failure_keeps_partial_copies_owned_until_cleanup() {
    let source = tempfile::tempdir().unwrap();
    let mut entries = [make_pointer(b"first"), make_pointer(b"second")];
    entries.sort_by(|a, b| a.oid.cmp(&b.oid));
    for (index, item) in entries.iter().enumerate() {
        let path = object_path(source.path(), item);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = if item == &make_pointer(b"first") {
            b"first".as_slice()
        } else {
            b"second".as_slice()
        };
        std::fs::write(path, if index == 0 { body } else { b"bad" }).unwrap();
    }
    let destination = tempfile::tempdir().unwrap();
    let path = destination.path().to_owned();
    let error = StagedLfsPayloads::stage(source.path(), &entries, 100, destination).unwrap_err();
    assert_eq!(error.failure(), LfsStagingFailure::Integrity);
    let first = File::open(object_path(&path.join("storage/objects"), &entries[0])).unwrap();
    entries[0].verify(first).unwrap();
    drop(error);
    assert!(!path.exists());
    assert!(object_path(source.path(), &entries[0]).is_file());
}

#[test]
fn an_open_source_handle_does_not_follow_a_replaced_cache_path() {
    let (source, pointer) = cache(b"original");
    let path = object_path(source.path(), &pointer);
    let mut opened = open_source(&path).unwrap();
    std::fs::rename(&path, source.path().join("retained")).unwrap();
    std::fs::write(&path, b"replaced").unwrap();
    let mut output = tempfile::tempfile().unwrap();
    copy_verified(&mut opened, &mut output, &pointer).unwrap();
    output.rewind().unwrap();
    let mut body = Vec::new();
    output.read_to_end(&mut body).unwrap();
    assert_eq!(body, b"original");
    assert_eq!(std::fs::read(path).unwrap(), b"replaced");
}

#[test]
fn directory_payload_is_refused_as_source() {
    let (source, pointer) = cache(b"payload");
    let path = object_path(source.path(), &pointer);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let error =
        StagedLfsPayloads::stage(source.path(), &[pointer], 100, tempfile::tempdir().unwrap())
            .unwrap_err();
    assert_eq!(error.failure(), LfsStagingFailure::Source);
}

#[cfg(unix)]
#[test]
fn missing_cache_below_an_existing_alias_cannot_overlap_staging() {
    for selected in [vec![], vec![make_pointer(b"")]] {
        let links = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().to_owned();
        let alias = links.path().join("alias");
        std::os::unix::fs::symlink(&destination, &alias).unwrap();
        let source = alias.join("storage/objects");
        let error = StagedLfsPayloads::stage(&source, &selected, 0, directory).unwrap_err();
        assert_eq!(error.failure(), LfsStagingFailure::Overlap);
        assert!(!source.exists());
        assert!(destination.read_dir().unwrap().next().is_none());
    }
}

#[cfg(unix)]
#[test]
fn dangling_cache_aliases_cannot_become_staging_directories() {
    for selected in [vec![], vec![make_pointer(b"")]] {
        for suffix in ["", ".", "objects", "objects/."] {
            let links = tempfile::tempdir().unwrap();
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().to_owned();
            let alias = links.path().join("alias");
            std::os::unix::fs::symlink(destination.join("storage"), &alias).unwrap();
            let error =
                StagedLfsPayloads::stage(&alias.join(suffix), &selected, 0, directory).unwrap_err();
            assert_eq!(error.failure(), LfsStagingFailure::Source);
            assert!(!destination.join("storage").exists());
            assert!(destination.read_dir().unwrap().next().is_none());
        }
    }
}

#[cfg(unix)]
#[test]
fn symlink_and_fifo_payloads_are_refused_without_following_or_blocking() {
    use std::os::unix::ffi::OsStrExt as _;
    let (source, pointer) = cache(b"payload");
    let path = object_path(source.path(), &pointer);
    let target = source.path().join("target");
    std::fs::rename(&path, &target).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert_eq!(open_source(&path).unwrap_err(), LfsStagingFailure::Source);
    std::fs::remove_file(&path).unwrap();
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert_eq!(open_source(&path).unwrap_err(), LfsStagingFailure::Source);
    assert_eq!(std::fs::read(target).unwrap(), b"payload");
}
