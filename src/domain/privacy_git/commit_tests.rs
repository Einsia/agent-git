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
