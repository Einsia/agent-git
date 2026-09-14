//! Reachable pointer enumeration uses only the selected local Git object graph.

use crate::domain::repo::Repo;
use anyhow::{Result, ensure};

/// Selected histories determine upload scope, including files removed from their current tips.
pub fn reachable(repo: &Repo, references: &[String]) -> Result<Vec<super::Pointer>> {
    let repo = repo.clone().local_objects_only();
    if references.is_empty() {
        return Ok(vec![]);
    }
    ensure!(
        references.iter().all(|reference| !reference.is_empty()
            && !reference.starts_with('-')
            && !reference.contains(['\0', '\n', '\r'])),
        "invalid LFS history reference"
    );
    let mut args = vec![
        "rev-list",
        "--objects",
        "--no-object-names",
        "--filter=blob:limit=1024",
    ];
    args.extend(references.iter().map(String::as_str));
    args.push("--");
    read_pointers(&repo, &args, None)
}

/// Captured commit snapshots determine pointer scope without resolving or walking live history.
#[cfg(feature = "cli")]
pub fn for_publication(
    repo: &Repo,
    plan: &crate::domain::repo::publication::PublicationPlan,
) -> Result<Vec<super::Pointer>> {
    frozen_commits(repo, plan.commit_objects())
}

/// The publication source has already validated the exact bare carrier.
#[cfg(feature = "cli")]
pub(crate) fn for_bare_publication(
    repo: &Repo,
    plan: &crate::domain::repo::publication::PublicationPlan,
) -> Result<Vec<super::Pointer>> {
    frozen_commits_in(
        &repo.clone().exact_bare_root_inspection(),
        plan.commit_objects(),
    )
}

#[cfg(feature = "cli")]
fn frozen_commits(repo: &Repo, commits: &[String]) -> Result<Vec<super::Pointer>> {
    frozen_commits_in(&repo.clone().exact_root_inspection(), commits)
}

#[cfg(feature = "cli")]
fn frozen_commits_in(repo: &Repo, commits: &[String]) -> Result<Vec<super::Pointer>> {
    use std::io::{Seek, Write};

    ensure!(
        !commits.is_empty() && commits.len() <= 4096,
        "frozen LFS history exceeds its publication root limit"
    );
    ensure!(
        commits.iter().all(|oid| {
            matches!(oid.len(), 40 | 64)
                && oid
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }),
        "frozen LFS history requires full immutable commit ids"
    );
    let repo = repo.clone().local_objects_only();
    let mut checked = 0;
    repo.git_cat_file_batch_check(commits.to_vec(), |oid, kind, _| {
        ensure!(
            commits.get(checked).is_some_and(|expected| expected == oid) && kind == "commit",
            "frozen LFS history contains an unavailable or mistyped commit"
        );
        checked += 1;
        Ok(())
    })?;
    ensure!(
        checked == commits.len(),
        "frozen LFS commit check is incomplete"
    );

    // Full commit ids cannot introduce stdin options. Each captured snapshot is visited without
    // walking parents, so topology overlays cannot alter the publication plan's object set.
    let mut input = tempfile::tempfile()?;
    for commit in commits {
        writeln!(input, "{commit}")?;
    }
    input.rewind()?;
    read_pointers(
        &repo,
        &[
            "rev-list",
            "--no-walk",
            "--objects",
            "--no-object-names",
            "--filter=blob:limit=1024",
            "--stdin",
        ],
        Some(input),
    )
}

fn read_pointers(
    repo: &Repo,
    args: &[&str],
    input: Option<std::fs::File>,
) -> Result<Vec<super::Pointer>> {
    let mut pointers = std::collections::BTreeMap::new();
    let mut batch = Vec::new();
    let mut inspected = 0usize;
    let read = |batch: &mut Vec<String>,
                pointers: &mut std::collections::BTreeMap<String, super::Pointer>|
     -> Result<()> {
        repo.git_cat_file_batch(
            std::mem::take(batch),
            super::POINTER_LIMIT - 1,
            |_, kind, body| {
                if kind == "blob"
                    && let crate::domain::repo::ObjectBody::Read(bytes) = body
                    && let Some(pointer) = super::Pointer::parse(bytes)?
                {
                    if let Some(previous) = pointers.insert(pointer.oid.clone(), pointer.clone()) {
                        ensure!(
                            previous == pointer,
                            "LFS history assigns conflicting sizes to one object"
                        );
                    }
                    ensure!(
                        pointers.len() <= 10_000,
                        "LFS history exceeds the supported object count"
                    );
                }
                Ok(())
            },
        )
    };
    let mut on_record = |record: &[u8]| {
        if record.is_empty() {
            return Ok(());
        }
        inspected += 1;
        ensure!(
            inspected <= 1_000_000,
            "LFS history inspection exceeded its object budget"
        );
        let oid = std::str::from_utf8(record)?;
        ensure!(
            matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid Git object identity during LFS inspection"
        );
        batch.push(oid.to_owned());
        if batch.len() == 256 {
            read(&mut batch, &mut pointers)?;
        }
        Ok(())
    };
    match input {
        Some(input) => repo.git_stream_split_stdin_file(args, input, b'\n', &mut on_record)?,
        None => repo.git_stream_split(args, b'\n', &mut on_record)?,
    }
    read(&mut batch, &mut pointers)?;
    Ok(pointers.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::lfs::{Pointer, VERSION};

    #[test]
    fn bare_history_includes_removed_payloads_and_direct_blob_tags_without_other_branches() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(&directory.path().join("work")).unwrap();
        let first = Pointer {
            oid: "a".repeat(64),
            size: 123,
        };
        let text = format!(
            "version {VERSION}\noid sha256:{}\nsize {}\n",
            first.oid, first.size
        );
        std::fs::write(repo.root().join("artifact"), text).unwrap();
        repo.add_all().unwrap();
        repo.commit("Store artifact pointer").unwrap();
        let blob = repo.git(&["rev-parse", "HEAD:artifact"]).unwrap();
        repo.git(&["tag", "blob", &blob]).unwrap();
        repo.git(&["rm", "artifact"]).unwrap();
        repo.commit("Remove artifact from tip").unwrap();
        repo.git(&["branch", "foreign"]).unwrap();
        repo.git(&["symbolic-ref", "HEAD", "refs/heads/foreign"])
            .unwrap();
        std::fs::write(
            repo.root().join("other"),
            format!(
                "version {VERSION}\noid sha256:{}\nsize 456\n",
                "b".repeat(64)
            ),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("Store an unrelated branch payload").unwrap();
        let bare = directory.path().join("objects.git");
        repo.git(&[
            "clone",
            "--bare",
            "--no-hardlinks",
            ".",
            bare.to_str().unwrap(),
        ])
        .unwrap();
        let bare = Repo::at(bare);
        assert_eq!(
            reachable(&bare, &["refs/heads/main".into()]).unwrap(),
            vec![first.clone()]
        );
        assert_eq!(
            reachable(&bare, &["refs/tags/blob".into()]).unwrap(),
            vec![first]
        );
        assert!(reachable(&bare, &["--all".into()]).is_err());
        assert!(reachable(&bare, &["refs/heads/missing".into()]).is_err());
        assert!(reachable(&bare, &[]).unwrap().is_empty());
    }
}

#[cfg(all(test, feature = "cli"))]
mod frozen_tests {
    use super::*;
    use crate::domain::lfs::{Pointer, VERSION};
    use crate::domain::repo::publication::PublicationPlan;
    use std::io::{Seek, Write};

    fn repository() -> (tempfile::TempDir, Repo) {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(&directory.path().join("repository")).unwrap();
        repo.git(&["config", "core.autocrlf", "false"]).unwrap();
        (directory, repo)
    }

    fn pointer(character: char, size: u64) -> Pointer {
        Pointer {
            oid: character.to_string().repeat(64),
            size,
        }
    }

    fn save_pointer(repo: &Repo, pointer: &Pointer) -> String {
        std::fs::write(
            repo.root().join("payload"),
            format!(
                "version {VERSION}\noid sha256:{}\nsize {}\n",
                pointer.oid, pointer.size
            ),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("Store pointer snapshot").unwrap();
        repo.git(&["rev-parse", "HEAD"]).unwrap()
    }

    /// Deleted pointers survive ref movement and topology overlays without admitting new history.
    #[test]
    fn captured_snapshots_keep_deleted_pointers_without_following_live_refs_or_parents() {
        let (_directory, repo) = repository();
        let expected = pointer('a', 17);
        let stored = save_pointer(&repo, &expected);
        repo.git(&["rm", "payload"]).unwrap();
        repo.commit("Remove pointer from tip").unwrap();
        let tip = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let plan = PublicationPlan::freeze(&repo, &["main".into()]).unwrap();
        assert!(
            frozen_commits(&repo, std::slice::from_ref(&tip))
                .unwrap()
                .is_empty()
        );
        save_pointer(&repo, &pointer('b', 23));
        repo.git(&["tag", "unselected-pointer"]).unwrap();
        repo.git(&["replace", &stored, &tip]).unwrap();
        std::fs::write(repo.root().join(".git/shallow"), format!("{tip}\n")).unwrap();
        std::fs::write(repo.root().join(".git/info/grafts"), format!("{tip}\n")).unwrap();
        let refs = repo.git(&["show-ref"]).unwrap();
        let index = std::fs::read(repo.root().join(".git/index")).unwrap();

        assert_eq!(for_publication(&repo, &plan).unwrap(), vec![expected]);
        assert_eq!(repo.git(&["show-ref"]).unwrap(), refs);
        assert_eq!(
            std::fs::read(repo.root().join(".git/index")).unwrap(),
            index
        );
        let nested = repo.root().join("nested");
        std::fs::create_dir(&nested).unwrap();
        assert!(for_publication(&Repo::at(&nested), &plan).is_err());
        std::fs::create_dir(nested.join(".git")).unwrap();
        assert!(for_publication(&Repo::at(&nested), &plan).is_err());
    }

    /// Identity deduplication cannot silently choose one size from contradictory snapshots.
    #[test]
    fn captured_snapshots_deduplicate_and_reject_conflicting_or_malformed_pointers() {
        let (_directory, repo) = repository();
        let expected = pointer('c', 31);
        save_pointer(&repo, &expected);
        std::fs::copy(repo.root().join("payload"), repo.root().join("duplicate")).unwrap();
        repo.add_all().unwrap();
        repo.commit("Repeat pointer in another path").unwrap();
        let plan = PublicationPlan::freeze(&repo, &["main".into()]).unwrap();
        assert_eq!(
            for_publication(&repo, &plan).unwrap(),
            vec![expected.clone()]
        );
        save_pointer(&repo, &pointer('c', expected.size + 1));
        let conflicting = PublicationPlan::freeze(&repo, &["main".into()]).unwrap();
        assert!(
            for_publication(&repo, &conflicting)
                .unwrap_err()
                .to_string()
                .contains("conflicting sizes")
        );

        std::fs::write(
            repo.root().join("payload"),
            format!(
                "version {VERSION}\noid sha256:{}\nsize 01\n",
                "d".repeat(64)
            ),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("Record a malformed pointer").unwrap();
        let tip = repo.git(&["rev-parse", "HEAD"]).unwrap();
        assert!(
            frozen_commits(&repo, &[tip])
                .unwrap_err()
                .to_string()
                .contains("noncanonical Git LFS object size")
        );
    }

    /// A missing captured snapshot or a revision expression cannot become an empty success.
    #[test]
    fn captured_roots_require_available_full_commit_ids() {
        let (_directory, repo) = repository();
        let commit = save_pointer(&repo, &pointer('e', 41));
        let tree = repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
        let blob = repo.git(&["rev-parse", "HEAD:payload"]).unwrap();
        for invalid in [
            vec![],
            vec!["--all".into()],
            vec![format!("{commit}\n--all")],
            vec![format!("^{commit}")],
            vec![commit[..12].to_owned()],
            vec!["f".repeat(64)],
            vec![tree],
            vec![blob],
            vec![commit; 4097],
        ] {
            assert!(frozen_commits(&repo, &invalid).is_err());
        }
        let plan = PublicationPlan::freeze(&repo, &["main".into()]).unwrap();
        let pointer_blob = repo.git(&["rev-parse", "HEAD:payload"]).unwrap();
        let path = repo
            .root()
            .join(".git/objects")
            .join(&pointer_blob[..2])
            .join(&pointer_blob[2..]);
        #[cfg(windows)]
        {
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            #[expect(
                clippy::permissions_set_readonly_false,
                reason = "This Windows-only fixture clears the read-only attribute on a Git object."
            )]
            permissions.set_readonly(false);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        std::fs::remove_file(path).unwrap();
        repo.git(&[
            "config",
            "remote.origin.url",
            "https://unavailable.invalid/repo.git",
        ])
        .unwrap();
        repo.git(&["config", "remote.origin.promisor", "true"])
            .unwrap();
        assert!(for_publication(&repo, &plan).is_err());
    }

    /// The last captured snapshot remains inspectable when its roots cannot fit in Windows argv.
    #[test]
    fn captured_roots_cross_windows_argument_limit_through_stdin() {
        let (_directory, repo) = repository();
        let expected = pointer('a', 59);
        save_pointer(&repo, &expected);
        let pointer_tree = repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
        repo.git(&["rm", "payload"]).unwrap();
        repo.commit("Remove pointer from the live branch").unwrap();
        let empty_tree = repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
        let refs = repo.git(&["show-ref"]).unwrap();
        let index = std::fs::read(repo.root().join(".git/index")).unwrap();
        let mut paths = tempfile::tempfile().unwrap();
        for index in 0..1001 {
            let tree = if index == 1000 {
                &pointer_tree
            } else {
                &empty_tree
            };
            let path = format!(".git/captured-lfs-{index}");
            std::fs::write(
                repo.root().join(&path),
                format!(
                    "tree {tree}\nauthor Pointer fixture <pointer@example.invalid> 1700000000 +0000\ncommitter Pointer fixture <pointer@example.invalid> 1700000000 +0000\n\nsnapshot {index}\n"
                ),
            )
            .unwrap();
            writeln!(paths, "{path}").unwrap();
        }
        paths.rewind().unwrap();
        let stored = repo
            .git_with_stdin_file(
                &[
                    "hash-object",
                    "-w",
                    "-t",
                    "commit",
                    "--stdin-paths",
                    "--no-filters",
                ],
                paths,
            )
            .unwrap();
        let roots: Vec<String> = stored.lines().map(str::to_owned).collect();
        assert_eq!(roots.len(), 1001);
        assert!(roots.iter().map(|oid| oid.len() + 1).sum::<usize>() > 32767);
        assert!(frozen_commits(&repo, &roots[..1000]).unwrap().is_empty());
        assert_eq!(frozen_commits(&repo, &roots).unwrap(), vec![expected]);
        assert_eq!(repo.git(&["show-ref"]).unwrap(), refs);
        assert_eq!(
            std::fs::read(repo.root().join(".git/index")).unwrap(),
            index
        );
    }
}
