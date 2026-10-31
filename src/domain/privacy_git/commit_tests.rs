use super::*;

/// Projection preserves byte-encoded descriptions but cannot carry proofs for different objects.
#[test]
fn projected_commits_retain_metadata_and_drop_signatures_over_original_objects() {
    let source = b"tree aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
parent bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
author Andr\xe9 <author@example.invalid> 1700000000 +0530\n\
committer Original Committer <committer@example.invalid> 1700000300 -0700\n\
encoding ISO-8859-1\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n signed source commit\n -----END PGP SIGNATURE-----\n\
gpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n signed alternate object\n -----END PGP SIGNATURE-----\n\
mergetag object bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n type commit\n tag private-parent\n\n\
Keep the caf\xe9 message.\n\ncommitter Message text is not a header.\n";
    let parents = vec!["d".repeat(40), "e".repeat(40)];
    let projected = commit_body(&"c".repeat(40), &parents, source).unwrap();
    assert_eq!(
        projected,
        b"tree cccccccccccccccccccccccccccccccccccccccc\n\
parent dddddddddddddddddddddddddddddddddddddddd\n\
parent eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\n\
author Andr\xe9 <author@example.invalid> 1700000000 +0530\n\
committer Original Committer <committer@example.invalid> 1700000300 -0700\n\
encoding ISO-8859-1\n\n\
Keep the caf\xe9 message.\n\ncommitter Message text is not a header.\n"
    );
    assert_eq!(
        commit_body(&"c".repeat(40), &parents, &projected).unwrap(),
        projected
    );
    let preview = commit_preview(&projected).unwrap();
    assert_eq!(
        preview["author"]["escaped_bytes"],
        "Andr\\xe9 <author@example.invalid> 1700000000 +0530"
    );
    assert_eq!(
        preview["message"]["escaped_bytes"],
        "Keep the caf\\xe9 message.\\n\\ncommitter Message text is not a header.\\n"
    );
}

/// A commit carries an envelope exactly when `ls-tree` lists an entry at `privacy/envelope.json`,
/// whatever its type: a file, a directory, a symlink or a submodule, present or not. A `privacy`
/// that is itself a file or a submodule, or a directory without that exact name, carries none. A
/// lookup that resolves the whole path and trusts a "missing" answer passes the absent submodule on
/// a Git that reports it missing; one that counts only blobs passes the directory and the
/// submodules; one that matches names by prefix refuses the near names. A commit that is not local
/// is refused rather than read as carrying nothing.
#[test]
fn envelope_lookup_counts_every_entry_type_at_the_envelope_path() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&temp.path().join("source")).unwrap();
    let content = temp.path().join("content");
    std::fs::write(&content, "{}\n").unwrap();
    let blob = repo
        .git(&["hash-object", "-w", content.to_str().unwrap()])
        .unwrap();
    let blob = blob.as_str();
    let absent = "0123456789abcdef0123456789abcdef01234567";
    let commit = |entries: &[(&str, &str, &str)]| {
        repo.git(&["read-tree", "--empty"]).unwrap();
        for (mode, oid, path) in entries {
            repo.git(&[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("{mode},{oid},{path}"),
            ])
            .unwrap();
        }
        let tree = repo.git(&["write-tree"]).unwrap();
        repo.git(&["commit-tree", &tree, "-m", "case"]).unwrap()
    };
    let local = commit(&[("100644", blob, "README")]);
    let at = "privacy/envelope.json";
    let cases = [
        ("file", true, vec![("100644", blob, at)]),
        ("executable", true, vec![("100755", blob, at)]),
        (
            "directory",
            true,
            vec![("100644", blob, "privacy/envelope.json/x")],
        ),
        ("symlink", true, vec![("120000", blob, at)]),
        ("absent submodule", true, vec![("160000", absent, at)]),
        (
            "local submodule",
            true,
            vec![("160000", local.as_str(), at)],
        ),
        ("none", false, vec![("100644", blob, "README")]),
        ("privacy file", false, vec![("100644", blob, "privacy")]),
        (
            "privacy submodule",
            false,
            vec![("160000", absent, "privacy")],
        ),
        (
            "near names",
            false,
            vec![
                ("100644", blob, "privacy/envelope"),
                ("100644", blob, "privacy/envelope.json5"),
                ("100644", blob, "privacy/envelope.jsonx/x"),
            ],
        ),
    ]
    .map(|(name, enveloped, entries)| (name, enveloped, commit(&entries)));
    let commits: Vec<String> = cases.iter().map(|(_, _, commit)| commit.clone()).collect();
    let found = enveloped_commits(&repo, &commits).unwrap();
    for (name, enveloped, commit) in &cases {
        let listed = !repo
            .git(&["ls-tree", commit, "--", "privacy/envelope.json"])
            .unwrap()
            .is_empty();
        assert_eq!(
            listed, *enveloped,
            "{name}: ls-tree disagrees with the case"
        );
        assert_eq!(found.contains(commit), *enveloped, "{name}");
    }
    assert!(enveloped_commits(&repo, &[absent.to_owned()]).is_err());
}
