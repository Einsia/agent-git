use super::*;
use std::path::Path;

fn git_bytes(root: &Path, args: &[&str], input: Option<&[u8]>) -> Vec<u8> {
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.unwrap_or_default()).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git(root: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    String::from_utf8(git_bytes(root, args, input))
        .unwrap()
        .trim()
        .to_owned()
}

/// A tree object holding `entries` exactly as given, modes spelled as given.
fn stored_tree(root: &Path, entries: &[(&str, &str, &str)]) -> String {
    let mut bytes = Vec::new();
    for (mode, name, id) in entries {
        bytes.extend_from_slice(format!("{mode} {name}\0").as_bytes());
        for pair in id.as_bytes().chunks(2) {
            bytes.push(u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap());
        }
    }
    git(
        root,
        &["hash-object", "-w", "-t", "tree", "--literally", "--stdin"],
        Some(&bytes),
    )
}

/// A request answers what a lone read of it answers: the object the ID names, with replace refs
/// off; nothing for an object the repository lacks; a refusal for a body past its bound or of
/// another kind, after which the next request still gets its own answer, as it does after an
/// existence check. A reader started without the inspection setup follows the replace ref, and one
/// that kept reading the same stream after a refused or unread body hands the next request the
/// rest of that body.
#[test]
fn a_request_answers_what_a_lone_read_of_it_answers() {
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q"], None);
    let blob = git(
        temp.path(),
        &["hash-object", "-w", "--stdin"],
        Some(b"recorded\n"),
    );
    let tree = stored_tree(temp.path(), &[("100644", "note", &blob)]);
    let original = git(temp.path(), &["commit-tree", &tree, "-m", "original"], None);
    let replacement = git(
        temp.path(),
        &["commit-tree", &tree, "-m", "replacement"],
        None,
    );
    git(temp.path(), &["replace", &original, &replacement], None);
    let repo = Repo::at(temp.path()).local_objects_only();
    let objects = Objects::default();

    let lone = read(&repo, &["cat-file", "commit", &original], MAX_COMMIT).unwrap();
    assert!(String::from_utf8_lossy(&lone).ends_with("\noriginal\n"));
    assert_eq!(objects.commit(&repo, &original).unwrap(), lone);

    let absent = "0".repeat(original.len());
    assert!(
        objects
            .read(&repo, &absent, "blob", 1024)
            .unwrap()
            .is_none()
    );
    assert!(
        !objects
            .exists(&repo, &format!("{original}:absent"))
            .unwrap()
    );
    assert!(
        objects
            .exists(&repo, &format!("{original}^{{tree}}"))
            .unwrap()
    );
    // The tree's body was passed over, not left in the stream for the next request.
    assert_eq!(objects.commit(&repo, &original).unwrap(), lone);

    assert!(objects.read(&repo, &blob, "blob", 3).is_err());
    assert_eq!(
        objects.read(&repo, &blob, "blob", 1024).unwrap().unwrap(),
        b"recorded\n"
    );
    assert!(objects.read(&repo, &tree, "blob", 1024).is_err());
    assert_eq!(
        objects
            .read(&repo, &format!("{original}:note"), "blob", 1024)
            .unwrap()
            .unwrap(),
        b"recorded\n"
    );
}

/// Plain publication rewrites a snapshot's metadata into the tree Git's own round trip writes:
/// the source tree as `git ls-tree` lists it, the metadata blob swapped, written back by
/// `git mktree`. Git lists modes as its tree walk reports them, so an entry stored with a mode Git
/// spells another way (a group-writable file, a zero-padded directory) is written back as Git
/// spells it; a listing that kept the stored modes writes another tree. A commit shows itself
/// plain only while its tree has no top-level entry named `privacy`; a name that merely starts
/// with it doesn't count, and neither does a commit the repository lacks.
#[test]
fn plain_publication_writes_the_tree_gits_own_round_trip_writes() {
    let temp = tempfile::tempdir().unwrap();
    let source_root = temp.path().join("source");
    let destination_root = temp.path().join("destination");
    for root in [&source_root, &destination_root] {
        git(temp.path(), &["init", "-q", root.to_str().unwrap()], None);
    }
    let blob = |bytes: &[u8]| git(&source_root, &["hash-object", "-w", "--stdin"], Some(bytes));
    let metadata_blob = blob(br#"{"layout":"v1","session":"recorded"}"#);
    let file = blob(b"plain file\n");
    let session = stored_tree(
        &source_root,
        &[
            ("100644", "meta.json", &metadata_blob),
            ("100664", "notes.md", &file),
        ],
    );
    let nested = stored_tree(&source_root, &[("100664", "group-writable", &file)]);
    let top = stored_tree(
        &source_root,
        &[
            ("120000", "link", &file),
            // A submodule names a commit of another repository, which this one lacks.
            (
                "160000",
                "module",
                "1234567890abcdef1234567890abcdef12345678",
            ),
            ("040000", "nested", &nested),
            ("100644", "privacy-note", &file),
            ("100755", "run.sh", &file),
            ("040000", "session", &session),
            ("100644", "spaced name \u{e9}", &file),
        ],
    );
    let commit = git(&source_root, &["commit-tree", &top, "-m", "turn"], None);
    let source = Repo::at(&source_root).local_objects_only();
    let destination = Repo::at(&destination_root).local_objects_only();
    let objects = Objects::default();
    let metadata =
        crate::domain::meta::Meta::new("published".into(), "claude-code".into(), "/work".into());

    let public = super::super::plain_commit(
        &objects,
        &source,
        &destination,
        (&commit, None),
        &[],
        Some(&metadata),
    )
    .unwrap();
    let written = git(
        &destination_root,
        &["rev-parse", &format!("{public}^{{tree}}")],
        None,
    );
    let published_metadata = git(
        &destination_root,
        &["rev-parse", &format!("{written}:session/meta.json")],
        None,
    );
    assert_ne!(published_metadata, metadata_blob);

    fn round_trip(root: &Path, tree: &str, parts: &[&str], blob: &str) -> String {
        let mut listing = Vec::new();
        for entry in git_bytes(root, &["ls-tree", "-z", tree], None)
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let tab = entry.iter().position(|byte| *byte == b'\t').unwrap();
            let name = &entry[tab + 1..];
            if name == parts[0].as_bytes() {
                let header = std::str::from_utf8(&entry[..tab]).unwrap();
                let fields = header.split(' ').collect::<Vec<_>>();
                let replacement = match parts {
                    [_] => blob.to_owned(),
                    [_, rest @ ..] => round_trip(root, fields[2], rest, blob),
                    [] => unreachable!(),
                };
                listing.extend_from_slice(
                    format!("{} {} {replacement}\t", fields[0], fields[1]).as_bytes(),
                );
                listing.extend_from_slice(name);
            } else {
                listing.extend_from_slice(entry);
            }
            listing.push(0);
        }
        git(root, &["mktree", "-z", "--missing"], Some(&listing))
    }
    assert_eq!(
        written,
        round_trip(
            &source_root,
            &top,
            &["session", "meta.json"],
            &published_metadata
        )
    );

    let shown = |commit: &str| super::super::shown_plain(&objects, &source, &destination, commit);
    assert!(shown(&commit));
    let private = stored_tree(&source_root, &[("040000", "privacy", &session)]);
    let private = git(
        &source_root,
        &["commit-tree", &private, "-m", "envelope"],
        None,
    );
    assert!(!shown(&private));
    assert!(!shown(&"0".repeat(commit.len())));
}
