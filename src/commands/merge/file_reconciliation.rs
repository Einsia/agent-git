//! A conflicted draft requires explicit shared-path edits or current-request confirmation
//! before it can become a file result.

use anyhow::{Context, ensure};
use std::collections::BTreeSet;
use std::io::Read;
use std::process::{Command, Stdio};

use crate::Result;
use crate::domain::{meta, repo::Repo, storage};

const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

struct Draft {
    tree: String,
    conflicts: BTreeSet<String>,
}

fn path(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 4096
            && !std::path::Path::new(value).is_absolute()
            && !value.contains('\\')
            && value
                .split('/')
                .all(|part| !part.is_empty() && !matches!(part, "." | ".." | ".git")),
        "file merge reported an invalid conflict path"
    );
    Ok(())
}

fn parse(status: Option<i32>, bytes: &[u8], width: usize) -> Result<Draft> {
    ensure!(
        matches!(status, Some(0 | 1)),
        "file merge could not construct a draft"
    );
    ensure!(
        bytes.len() <= MAX_OUTPUT_BYTES && bytes.last() == Some(&0),
        "file merge report is incomplete or exceeds its limit"
    );
    let text = std::str::from_utf8(bytes).context("file merge report is not Unicode")?;
    let mut fields = text[..text.len() - 1].split('\0');
    let tree = fields.next().context("file merge tree is missing")?;
    ensure!(
        tree.len() == width
            && tree
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "file merge tree is not an immutable object id"
    );
    let mut conflicts = BTreeSet::new();
    for name in fields.by_ref() {
        if name.is_empty() {
            break;
        }
        path(name)?;
        ensure!(
            conflicts.len() < storage::MAX_SEQUENCE_EVENTS && conflicts.insert(name.to_owned()),
            "file merge conflict paths are duplicated or exceed their limit"
        );
    }
    let mut explained = BTreeSet::new();
    while let Some(count) = fields.next() {
        let count: usize = count
            .parse()
            .context("file merge message has an invalid path count")?;
        ensure!(
            count > 0 && count <= storage::MAX_SEQUENCE_EVENTS,
            "file merge message path count exceeds its limit"
        );
        let mut names = Vec::with_capacity(count.min(32));
        for _ in 0..count {
            let name = fields
                .next()
                .context("file merge message path is missing")?;
            path(name)?;
            names.push(name);
        }
        let kind = fields
            .next()
            .context("file merge message type is missing")?;
        fields.next().context("file merge message is incomplete")?;
        if status == Some(0) {
            continue;
        }
        match kind {
            "Auto-merging" => {}
            "CONFLICT (contents)" | "CONFLICT (modify/delete)" | "CONFLICT (add/add)" => {
                ensure!(
                    names.len() == 1 && conflicts.contains(names[0]),
                    "file merge conflict has no exact staged path"
                );
                explained.insert(names[0].to_owned());
            }
            _ => anyhow::bail!(
                "file merge has an unsupported structural conflict; reconcile that history before retrying"
            ),
        }
    }
    ensure!(
        if status == Some(0) {
            conflicts.is_empty() && explained.is_empty()
        } else {
            !conflicts.is_empty() && explained == conflicts
        },
        "file merge status and structured conflict evidence disagree"
    );
    Ok(Draft {
        tree: tree.to_owned(),
        conflicts,
    })
}

fn command(repo: &Repo) -> Command {
    let mut command = crate::infra::git_runtime::command();
    // Frozen commit identities require raw parents even if graph overlays change during capture.
    command
        .arg("--no-replace-objects")
        .args(["--shallow-file", "", "-c", "core.commitGraph=false"])
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(repo.root())
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stderr(Stdio::null());
    crate::infra::git_runtime::disable_grafts(&mut command);
    command
}

/// The merge of `target` and `source`, drawn against `base` when one is given; otherwise git finds
/// their common ancestor itself.
fn draft(repo: &Repo, target: &str, source: &str, base: Option<&str>) -> Result<Draft> {
    // The git this runs on may predate `merge-tree --merge-base`, so a given base is the one
    // parent of a stand-in for each side's tree, and git finds it as theirs.
    let sides = match base {
        Some(base) => {
            let stand_in = |side: &str| {
                let tree = repo.git(&["rev-parse", "--verify", &format!("{side}^{{tree}}")])?;
                super::super::plumbing::fixed_commit(repo, &tree, &[base], "agit: merge side")
            };
            Some((stand_in(target)?, stand_in(source)?))
        }
        None => None,
    };
    let (target, source) = match &sides {
        Some((target, source)) => (target.as_str(), source.as_str()),
        None => (target, source),
    };
    let mut child = command(repo)
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--messages",
            "-z",
            target,
            source,
        ])
        .stdout(Stdio::piped())
        .spawn()?;
    let mut output = Vec::new();
    let read = child
        .stdout
        .take()
        .context("file merge report pipe is missing")?
        .take((MAX_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut output);
    if read.is_err() || output.len() > MAX_OUTPUT_BYTES {
        let _ = child.kill();
    }
    let status = child.wait()?;
    read?;
    let draft = parse(status.code(), &output, target.len())?;
    ensure!(
        repo.git(&["cat-file", "-t", &draft.tree])? == "tree",
        "file merge result is not a tree"
    );
    Ok(draft)
}

/// The subject of the one commit of a `main` this computer started on its own: import's, when the
/// Hub's `main` couldn't be read, and `agit init`'s.
const STARTED_HERE: &str = "agit: init (main file line)";

/// A `main` this computer started on its own, which shares no history with the `main` it is
/// pulled from.
pub(super) struct StartedHere {
    /// The fixed base its merge is drawn against: what the start holds exactly as agit's scaffold
    /// writes it. That takes the other side's version, and what the person added at the start, or
    /// changed from the scaffold, is carried in.
    pub(super) base: String,
    /// The files the person added at its start, or changed from the scaffold, which the merge
    /// carries in.
    pub(super) carried: Vec<String>,
}

/// `target` as a `main` started on this computer, when it shares no history with `source`: a
/// single root commit with [`STARTED_HERE`]'s subject. `None` for any other shape, and whenever
/// the two share history, where git finds the base itself. Whether anything ever published
/// `target` is the caller's to check.
pub(super) fn started_here(repo: &Repo, target: &str, source: &str) -> Result<Option<StartedHere>> {
    let related = command(repo)
        .args(["merge-base", target, source])
        .stdout(Stdio::null())
        .status()?;
    match related.code() {
        Some(0) => return Ok(None),
        Some(1) => {}
        _ => anyhow::bail!("couldn't compare the histories of the two file branches"),
    }
    let commit = repo.git(&["rev-list", "--parents", "-n", "1", target])?;
    if commit.split_whitespace().count() != 1
        || repo.git(&["log", "-1", "--format=%s", target])? != STARTED_HERE
    {
        return Ok(None);
    }
    let scaffold = tempfile::tempdir()?;
    super::super::init::scaffold(scaffold.path())?;
    // The base holds what the start holds exactly as the scaffold writes it, so the other side's
    // version of it wins, its removal included. Anything else at the start is the person's,
    // added: carried in where the other side has nothing there, and a conflict where it has
    // something else.
    let held = names(repo.git_bytes_result(&["ls-tree", "-r", "-z", "--name-only", target])?)?;
    let mut edits = Vec::with_capacity(held.len());
    for path in held {
        let written = scaffold.path().join(&path);
        let written = written
            .is_file()
            .then(|| std::fs::read(&written))
            .transpose()?;
        let unchanged = match written {
            Some(written) => {
                repo.git_bytes_result(&["cat-file", "blob", &format!("{target}:{path}")])?
                    == written
            }
            None => false,
        };
        if !unchanged {
            edits.push((path, None));
        }
    }
    let tree = super::super::plumbing::tree_apply_owned(repo, target, edits)?;
    let base = super::super::plumbing::fixed_commit(repo, &tree, &[], "agit: scaffold")?;
    let carried = names(repo.git_bytes_result(&[
        "diff-tree",
        "-r",
        "-z",
        "--name-only",
        "--no-renames",
        &base,
        target,
    ])?)?
    .into_iter()
    .filter(|path| !meta::is_storage_path(path) && path != meta::ATTRS_FILE)
    .collect();
    Ok(Some(StartedHere { base, carried }))
}

/// The base a transaction's file merge of `target` and `source` is drawn against: the fixed one
/// [`started_here`] gives when they share no history, which must be the one the transaction
/// recorded. `None` when they share history, and git finds the base itself.
pub(super) fn base_of(
    repo: &Repo,
    target: &str,
    source: &str,
    recorded: &str,
) -> Result<Option<String>> {
    let Some(started) = started_here(repo, target, source)? else {
        return Ok(None);
    };
    ensure!(
        started.base == recorded,
        "the merge transaction's base is not the one agit draws for a main started here"
    );
    Ok(Some(started.base))
}

/// Paths of a `-z` listing.
fn names(listing: Vec<u8>) -> Result<Vec<String>> {
    let text = String::from_utf8(listing).context("file listing is not Unicode")?;
    Ok(text
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect())
}

pub(super) fn conflicts(
    repo: &Repo,
    target: &str,
    source: &str,
    base: Option<&str>,
) -> Result<BTreeSet<String>> {
    Ok(draft(repo, target, source, base)?
        .conflicts
        .into_iter()
        .filter(|path| !meta::is_storage_path(path))
        .collect())
}

pub(super) fn tree(
    repo: &Repo,
    target: &str,
    source: &str,
    base: Option<&str>,
    shared: &[String],
    resolved: &[String],
) -> Result<String> {
    let draft = draft(repo, target, source, base)?;
    let conflicts: Vec<_> = draft
        .conflicts
        .iter()
        .filter(|path| !meta::is_storage_path(path))
        .collect();
    ensure!(
        resolved.len() <= storage::MAX_SEQUENCE_EVENTS,
        "resolved conflict paths exceed their limit"
    );
    let mut confirmed = BTreeSet::new();
    let mut bytes = 0usize;
    for name in resolved {
        path(name)?;
        bytes = bytes
            .checked_add(name.len())
            .context("resolved conflict path budget overflow")?;
        ensure!(
            bytes <= MAX_OUTPUT_BYTES,
            "resolved conflict paths exceed their byte limit"
        );
        ensure!(
            !meta::is_storage_path(name) && draft.conflicts.contains(name),
            "resolved path {name:?} is not a shared conflict in this transaction"
        );
        match std::fs::symlink_metadata(repo.root().join(name)) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink() || repo.root().join(name).exists(),
                "resolved conflict cannot preserve a dangling symlink"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            confirmed.insert(name.clone()),
            "resolved conflict path is duplicated"
        );
    }
    for path in &conflicts {
        ensure!(
            shared.contains(path) || confirmed.contains(*path),
            "shared conflict {path:?} has no explicit worktree edit or --resolved confirmation"
        );
    }
    // Confirmation selects current checkout bytes, never the conflict-marked draft or source text.
    let overlay: Vec<_> = shared
        .iter()
        .cloned()
        .chain(confirmed)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let tree = super::super::plumbing::tree_overlay_worktree(repo, &draft.tree, &overlay)?;
    if !conflicts.is_empty() {
        // Git checks its own marker rules in the frozen result; native conflict prose is never parsed.
        let status = command(repo)
            .args([
                "-c",
                "core.whitespace=-blank-at-eol,-blank-at-eof,-space-before-tab",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--check",
                target,
                &tree,
                "--",
            ])
            .args(conflicts)
            .stdout(Stdio::null())
            .status()?;
        ensure!(
            status.success(),
            "the shared reconciliation still fails Git's conflict-marker check"
        );
    }
    Ok(tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a `main` that is one root commit of agit's start, sharing no history with the other
    /// side, is drawn against the scaffold; its base holds the scaffold the start didn't change,
    /// and what the person added or changed is carried.
    #[test]
    fn only_a_start_of_its_own_is_drawn_against_the_scaffold() {
        use crate::commands::plumbing;
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        crate::commands::init::scaffold(repo.root()).unwrap();
        std::fs::write(repo.root().join("AGENTS.md"), "My instructions\n").unwrap();
        std::fs::write(repo.root().join("memory/mine.md"), "A fact of mine\n").unwrap();
        repo.add_all().unwrap();
        repo.commit(STARTED_HERE).unwrap();
        let start = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let hub_tree = plumbing::tree_apply_owned(
            &repo,
            &start,
            vec![("README.md".into(), Some(b"Shared\n".to_vec()))],
        )
        .unwrap();
        let hub = plumbing::commit_tree(&repo, &hub_tree, &[], "File line").unwrap();

        let started = started_here(&repo, &start, &hub).unwrap().unwrap();
        assert_eq!(started.carried, ["AGENTS.md", "memory/mine.md"]);
        let base = repo.ls_tree(&started.base);
        assert!(base.contains(&"memory/.gitkeep".to_owned()), "{base:?}");
        assert!(base.contains(&"skills/.gitkeep".to_owned()), "{base:?}");
        for added in ["AGENTS.md", "memory/mine.md"] {
            assert!(!base.contains(&added.to_owned()), "{base:?}");
        }
        // The same start gives the same base, so a transaction can be continued against it.
        assert_eq!(
            started_here(&repo, &start, &hub).unwrap().unwrap().base,
            started.base
        );
        assert!(
            base_of(&repo, &start, &hub, &started.base)
                .unwrap()
                .is_some()
        );
        assert!(base_of(&repo, &start, &hub, &hub).is_err());

        // A main that went on after its start, one that isn't agit's start, and one that shares
        // history with the other side are not.
        let start_tree = repo
            .git(&["rev-parse", &format!("{start}^{{tree}}")])
            .unwrap();
        let later = plumbing::commit_tree(&repo, &start_tree, &[&start], "later").unwrap();
        let other = plumbing::commit_tree(&repo, &start_tree, &[], "another root").unwrap();
        let related = plumbing::commit_tree(&repo, &hub_tree, &[&start], "File line").unwrap();
        for (target, source) in [(&later, &hub), (&other, &hub), (&start, &related)] {
            assert!(started_here(&repo, target, source).unwrap().is_none());
            assert!(
                base_of(&repo, target, source, &started.base)
                    .unwrap()
                    .is_none()
            );
        }
    }

    /// Graph overlays cannot change a file result or conceal an unavailable raw parent.
    #[test]
    fn file_merge_uses_raw_parents_without_shallow_grafts_replacements_or_commit_graph() {
        use crate::commands::plumbing;
        for overlay in [
            "graft",
            "shallow",
            "replace",
            "missing-parent",
            "missing-parent-shallow",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(directory.path()).unwrap();
            meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
            std::fs::write(repo.root().join("AGENTS.md"), "Base instructions\n").unwrap();
            repo.add_all().unwrap();
            repo.commit("synthetic base").unwrap();
            let base = repo.git(&["rev-parse", "HEAD"]).unwrap();
            let target_tree = plumbing::tree_apply_owned(
                &repo,
                &base,
                vec![("AGENTS.md".into(), Some(b"Target instructions\n".to_vec()))],
            )
            .unwrap();
            let target = plumbing::commit_tree(&repo, &target_tree, &[&base], "target").unwrap();
            let source_tree = plumbing::tree_apply_owned(
                &repo,
                &base,
                vec![("memory/source.md".into(), Some(b"Source memory\n".to_vec()))],
            )
            .unwrap();
            let source = plumbing::commit_tree(&repo, &source_tree, &[&base], "source").unwrap();
            repo.git(&["update-ref", "refs/heads/target", &target])
                .unwrap();
            repo.git(&["update-ref", "refs/heads/source", &source])
                .unwrap();
            repo.git(&["update-ref", "HEAD", &target, &base]).unwrap();
            let expected = tree(&repo, &target, &source, None, &[], &[]).unwrap();
            let original_target = repo
                .git_bytes_result(&["cat-file", "commit", &target])
                .unwrap();
            let original_source = repo
                .git_bytes_result(&["cat-file", "commit", &source])
                .unwrap();
            repo.git(&["config", "core.commitGraph", "true"]).unwrap();
            repo.git(&["commit-graph", "write", "--reachable"]).unwrap();
            assert!(
                repo.git_path("objects/info/commit-graph")
                    .unwrap()
                    .is_file()
            );
            let mut removed = None;
            match overlay {
                "graft" => std::fs::write(
                    repo.git_path("info/grafts").unwrap(),
                    format!("{source} {target}\n"),
                )
                .unwrap(),
                "shallow" | "missing-parent" | "missing-parent-shallow" => {
                    if overlay != "missing-parent" {
                        std::fs::write(
                            repo.git_path("shallow").unwrap(),
                            format!("{source}\n{target}\n"),
                        )
                        .unwrap();
                    }
                    if overlay.starts_with("missing-parent") {
                        let object = repo
                            .git_path("objects")
                            .unwrap()
                            .join(&base[..2])
                            .join(&base[2..]);
                        let bytes = std::fs::read(&object).unwrap();
                        let permissions = std::fs::metadata(&object).unwrap().permissions();
                        #[cfg(windows)]
                        {
                            let mut writable = permissions.clone();
                            #[expect(
                                clippy::permissions_set_readonly_false,
                                reason = "This Windows-only fixture clears the read-only attribute on a Git object."
                            )]
                            writable.set_readonly(false);
                            std::fs::set_permissions(&object, writable).unwrap();
                        }
                        std::fs::remove_file(&object).unwrap();
                        removed = Some((object, bytes, permissions));
                    }
                }
                "replace" => {
                    let replacement =
                        plumbing::commit_tree(&repo, &source_tree, &[&target], "replacement")
                            .unwrap();
                    repo.git(&[
                        "update-ref",
                        &format!("refs/replace/{source}"),
                        &replacement,
                    ])
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let refs = repo.git(&["show-ref"]).unwrap();
            let result = tree(&repo, &target, &source, None, &[], &[]);
            if overlay.starts_with("missing-parent") {
                assert!(result.is_err());
            } else {
                let actual = result.unwrap();
                assert_eq!(actual, expected, "{overlay}");
                assert_eq!(
                    repo.show_raw(&actual, "AGENTS.md").as_deref(),
                    Some("Target instructions\n")
                );
                assert_eq!(
                    repo.show_raw(&actual, "memory/source.md").as_deref(),
                    Some("Source memory\n")
                );
            }
            if let Some((object, bytes, permissions)) = removed {
                std::fs::write(&object, bytes).unwrap();
                std::fs::set_permissions(object, permissions).unwrap();
                assert_eq!(
                    tree(&repo, &target, &source, None, &[], &[]).unwrap(),
                    expected
                );
            }
            assert_eq!(repo.git(&["show-ref"]).unwrap(), refs);
            assert_eq!(
                repo.git_bytes_result(&["cat-file", "commit", &target])
                    .unwrap(),
                original_target
            );
            assert_eq!(
                repo.git_bytes_result(&["cat-file", "commit", &source])
                    .unwrap(),
                original_source
            );
        }
    }

    #[test]
    fn file_merge_refuses_v0_user_paths_before_storage_cleanup() {
        for user_path in ["LOG", "VIEW", "events/private.txt"] {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(directory.path()).unwrap();
            let metadata = meta::Meta::new_file_line();
            meta::write(repo.root(), &metadata).unwrap();
            repo.add_all().unwrap();
            repo.commit("file target").unwrap();
            let target = repo.git(&["rev-parse", "HEAD"]).unwrap();
            let mut legacy = metadata;
            legacy.layout = meta::LayoutVersion::V0;
            let tree = crate::commands::plumbing::tree_apply_owned(
                &repo,
                &target,
                vec![
                    (
                        meta::FILE.into(),
                        Some(meta::to_text(&legacy).unwrap().into_bytes()),
                    ),
                    (user_path.into(), Some(b"User owned legacy path\n".to_vec())),
                ],
            )
            .unwrap();
            let source =
                crate::commands::plumbing::commit_tree(&repo, &tree, &[&target], "legacy source")
                    .unwrap();
            let tx = crate::domain::mergetx::Tx {
                mode: Some(crate::domain::mergetx::Mode::Manual),
                exploration: None,
                generation: Some(uuid::Uuid::now_v7().to_string()),
                target: "main".into(),
                source: "source".into(),
                source_repo: Some("alice/repo".into()),
                source_branch: Some("source".into()),
                base: target.clone(),
                target_head: target.clone(),
                source_head: source.clone(),
                picked: vec![],
                summary: Some("Preserve legacy data".into()),
            };
            let refs = repo.git(&["show-ref"]).unwrap();
            assert!(super::super::merge_tree(&repo, &repo, &tx, &source, &[], true, &[]).is_err());
            assert_eq!(repo.git(&["show-ref"]).unwrap(), refs);
            assert_eq!(
                repo.show_raw(&source, user_path).as_deref(),
                Some("User owned legacy path\n")
            );
            assert_eq!(repo.git(&["status", "--porcelain"]).unwrap(), "");
        }
    }

    #[test]
    fn structured_conflicts_require_complete_known_path_relations() {
        let oid = "a".repeat(40);
        let valid = format!(
            "{oid}\0AGENTS.md\0\01\0AGENTS.md\0CONFLICT (contents)\0arbitrary localized prose\0"
        );
        assert_eq!(
            parse(Some(1), valid.as_bytes(), 40).unwrap().conflicts,
            BTreeSet::from(["AGENTS.md".into()])
        );
        assert!(parse(Some(0), valid.as_bytes(), 40).is_err());
        assert!(parse(Some(2), valid.as_bytes(), 40).is_err());
        for invalid in [
            valid.replace("CONFLICT (contents)", "CONFLICT (file/directory)"),
            valid.replace("\x001\0", "\x002\0"),
            valid.replace("AGENTS.md\0CONFLICT", "foreign.md\0CONFLICT"),
            valid.replace("AGENTS.md", "../AGENTS.md"),
            valid.replace("AGENTS.md\0\0", "AGENTS.md\0AGENTS.md\0\0"),
            valid.trim_end_matches('\0').to_owned(),
            format!("{oid}\0\0"),
        ] {
            assert!(
                parse(Some(1), invalid.as_bytes(), 40).is_err(),
                "{invalid:?}"
            );
        }
        assert!(parse(Some(0), format!("{oid}\0").as_bytes(), 40).is_ok());
    }
}
