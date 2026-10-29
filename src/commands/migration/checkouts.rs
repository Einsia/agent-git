//! Recovery state of a repository's registered checkouts, read once per repository.
//!
//! The read-only preflight needs two facts about every registered checkout: whether its private
//! Git directory holds checkout-transaction metadata, and whether its branch has a legacy layout
//! recovery ref. For the layout `git worktree add` creates, both follow from files Git keeps (a
//! linked checkout's `.git` link and its registration under `<common>/worktrees/<id>`), from the
//! branches `git worktree list` already reports, and from one listing of the refs, so no Git
//! process runs per checkout unless its branch has a recovery ref, which the exact legacy probe
//! then reads. A checkout whose layout differs in any way is answered by the per-checkout probe
//! instead: a shortcut that guessed how Git resolves an unusual layout could look in the wrong
//! directory and report a checkout with pending recovery as settled.

use super::LEGACY_RECOVERY_REF_PREFIX;
use crate::domain::repo::{Repo, Worktree};
use anyhow::Result;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// `git rev-parse --verify <name>` resolves a recovery ref name through each of these prefixes,
/// so a listing that omits any of them could miss a ref the per-checkout probe would find.
const RECOVERY_REF_SPELLINGS: [&str; 5] = [
    LEGACY_RECOVERY_REF_PREFIX,
    "refs/refs/agit/layout-v0/",
    "refs/tags/refs/agit/layout-v0/",
    "refs/heads/refs/agit/layout-v0/",
    "refs/remotes/refs/agit/layout-v0/",
];
const MAX_GIT_LINK_BYTES: usize = 64 * 1024;
const MAX_REF_LISTING_BYTES: usize = 1024 * 1024;

pub(super) struct Checkouts {
    /// Checkouts in the plain layout, keyed by canonical path.
    plain: HashMap<PathBuf, PlainCheckout>,
    /// Every recovery ref name in ASCII lowercase, or None when the listing cannot stand in for
    /// per-branch lookups.
    recovery_refs: Option<BTreeSet<String>>,
}

struct PlainCheckout {
    git_dir: PathBuf,
    /// The checked-out branch; None only for a detached HEAD that names a commit.
    branch: Option<String>,
}

impl Checkouts {
    /// `common` is the repository's canonical common Git directory when it is known exactly, and
    /// `registered` is the `git worktree list` inspection run from `repo`.
    pub(super) fn inspect(repo: &Repo, common: Option<&Path>, registered: &[Worktree]) -> Self {
        let mut plain = HashMap::new();
        if let Some(common) = common {
            let mut seen = HashSet::new();
            let mut repeated = HashSet::new();
            for worktree in registered {
                let Ok(path) = worktree.path.canonicalize() else {
                    continue;
                };
                // Two registrations of one path carry two HEADs, and only one of them is the
                // registration Git reaches from the checkout.
                if !seen.insert(path.clone()) {
                    repeated.insert(path);
                    continue;
                }
                if let Some(checkout) = plain_checkout(common, worktree) {
                    plain.insert(path, checkout);
                }
            }
            for path in repeated {
                plain.remove(&path);
            }
        }
        let recovery_refs = if plain.is_empty() {
            None
        } else {
            recovery_refs(repo)
        };
        Checkouts {
            plain,
            recovery_refs,
        }
    }

    /// Whether the checkout at `root` has pending recovery. `metadata` and `legacy` are the
    /// per-checkout Git probes, run only for the facts these files cannot establish exactly.
    pub(super) fn pending(
        &self,
        root: &Path,
        metadata: impl FnOnce() -> Result<bool>,
        legacy: impl FnOnce() -> Result<bool>,
    ) -> Result<bool> {
        let checkout = root
            .canonicalize()
            .ok()
            .and_then(|path| self.plain.get(&path));
        let Some(checkout) = checkout else {
            return Ok(metadata()? || legacy()?);
        };
        if super::super::plumbing::interrupted_checkout_metadata_in(&checkout.git_dir)? {
            return Ok(true);
        }
        match (&self.recovery_refs, &checkout.branch) {
            (Some(_), None) => Ok(false),
            (Some(refs), Some(branch)) if !may_have_recovery_ref(refs, branch) => Ok(false),
            _ => legacy(),
        }
    }
}

/// Loose refs are files, so on a case-insensitive filesystem Git finds a recovery ref under a
/// branch name that differs from it only in case. Names are therefore compared without ASCII
/// case, and a branch outside ASCII, whose folding belongs to the filesystem, is never ruled out.
fn may_have_recovery_ref(refs: &BTreeSet<String>, branch: &str) -> bool {
    !branch.is_ascii()
        || refs.contains(&format!("{LEGACY_RECOVERY_REF_PREFIX}{branch}").to_ascii_lowercase())
}

/// The private Git directory and branch of a registration whose layout is the plain one: the
/// main checkout's `.git` is the common directory itself, and a linked checkout's `.git` link
/// names a registration under `<common>/worktrees` whose `gitdir` names the checkout back and
/// whose `commondir` names this repository. Git reads the same files when it runs inside the
/// checkout, so the directory is the one `git rev-parse --git-path` would report there, and the
/// branch is the one `git symbolic-ref` would report.
fn plain_checkout(common: &Path, worktree: &Worktree) -> Option<PlainCheckout> {
    let branch = match (&worktree.branch, &worktree.head) {
        (Some(branch), _) => Some(branch.clone()),
        // `git worktree list` prints a zero object name and neither a branch nor `detached` when
        // HEAD cannot be resolved; `git symbolic-ref` then fails instead of reporting no branch.
        (None, Some(head)) if head.bytes().any(|byte| byte != b'0') => None,
        (None, _) => return None,
    };
    let link = worktree.path.join(".git");
    let metadata = std::fs::symlink_metadata(&link).ok()?;
    let git_dir = if metadata.is_dir() {
        (link.canonicalize().ok()? == common).then(|| common.to_path_buf())?
    } else if metadata.is_file() {
        let registration = linked_git_dir(&link)?.canonicalize().ok()?;
        let registrations = common.join("worktrees").canonicalize().ok()?;
        if registration.parent() != Some(registrations.as_path())
            || registered_link(&registration)? != link.canonicalize().ok()?
            || registered_common(&registration)? != common
        {
            return None;
        }
        registration
    } else {
        return None;
    };
    Some(PlainCheckout { git_dir, branch })
}

/// The directory a `.git` link names, parsed the way Git parses it: a `gitdir: ` line with any
/// trailing line terminators removed, relative to the directory holding the link.
fn linked_git_dir(link: &Path) -> Option<PathBuf> {
    let bytes = crate::domain::storage::read_bytes_capped(link, MAX_GIT_LINK_BYTES).ok()?;
    let target = native_path(trim_line_terminators(bytes.strip_prefix(b"gitdir: ")?))?;
    Some(link.parent()?.join(target))
}

/// The checkout link a registration's `gitdir` file names, canonicalized. Git trims trailing
/// whitespace and resolves a relative path against the registration directory.
fn registered_link(registration: &Path) -> Option<PathBuf> {
    let bytes =
        crate::domain::storage::read_bytes_capped(&registration.join("gitdir"), MAX_GIT_LINK_BYTES)
            .ok()?;
    let target = native_path(bytes.trim_ascii_end())?;
    registration.join(target).canonicalize().ok()
}

/// The common directory a registration's `commondir` file names, canonicalized. Git strips
/// trailing line terminators and resolves a relative path against the registration directory;
/// a registration without the file is not a usable Git directory.
fn registered_common(registration: &Path) -> Option<PathBuf> {
    let bytes = crate::domain::storage::read_bytes_capped(
        &registration.join("commondir"),
        MAX_GIT_LINK_BYTES,
    )
    .ok()?;
    let target = native_path(trim_line_terminators(&bytes))?;
    registration.join(target).canonicalize().ok()
}

fn trim_line_terminators(mut bytes: &[u8]) -> &[u8] {
    while let Some(rest) = bytes
        .strip_suffix(b"\n")
        .or_else(|| bytes.strip_suffix(b"\r"))
    {
        bytes = rest;
    }
    bytes
}

fn native_path(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

/// Every ref `git rev-parse --verify` could resolve for a recovery ref name, from one listing of
/// all refs. A pattern would miss a loose ref whose directory differs in case from it on a
/// case-insensitive filesystem, where `rev-parse` still finds the file, so names are matched
/// without ASCII case here. Only ASCII names under the recovery namespace's own spelling are
/// answered from the listing; any other spelling, a name whose case folding belongs to the
/// filesystem, an incomplete listing, or a diagnostic leaves every branch to the per-checkout
/// lookup. Git leaves a dangling symbolic ref out of every listing; agit never writes one here.
fn recovery_refs(repo: &Repo) -> Option<BTreeSet<String>> {
    let output = repo
        .inspection_output(
            &["for-each-ref", "--format=%(refname)"],
            MAX_REF_LISTING_BYTES,
        )
        .ok()?;
    if !output.status.success() || !output.stderr.is_empty() {
        return None;
    }
    let listing = std::str::from_utf8(&output.stdout).ok()?;
    let spellings = RECOVERY_REF_SPELLINGS.map(str::to_ascii_lowercase);
    let mut refs = BTreeSet::new();
    for name in listing.lines() {
        let folded = name.to_ascii_lowercase();
        if !spellings
            .iter()
            .any(|spelling| folded.starts_with(spelling))
        {
            continue;
        }
        if !name.starts_with(LEGACY_RECOVERY_REF_PREFIX) || !name.is_ascii() {
            return None;
        }
        refs.insert(folded);
    }
    Some(refs)
}

/// The common Git directory Git uses for the main checkout at `root`, canonicalized, when it is
/// `root/.git` itself: a real directory without a `commondir` redirection.
pub(super) fn main_checkout_common_dir(root: &Path) -> Option<PathBuf> {
    let git_dir = root.join(".git");
    let metadata = std::fs::symlink_metadata(&git_dir).ok()?;
    if !metadata.is_dir() {
        return None;
    }
    match std::fs::symlink_metadata(git_dir.join("commondir")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.canonicalize().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unexpected(probe: &'static str) -> impl FnOnce() -> Result<bool> {
        move || panic!("plain checkout reached the {probe} probe")
    }

    /// Plain checkouts are answered from Git's files: a journal in a linked checkout's private
    /// directory is pending recovery, a branch with a recovery ref still gets the exact legacy
    /// probe, and no other checkout reaches a per-checkout probe. Probing every checkout, reading
    /// the journal from the common directory, or ruling out a recovered branch fails here.
    #[test]
    fn plain_checkouts_reach_a_probe_only_for_recovered_branches() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(&directory.path().join("main")).unwrap();
        repo.git(&["commit", "--allow-empty", "-m", "base"])
            .unwrap();
        for branch in ["clean", "journal", "recovered"] {
            repo.git(&["branch", branch]).unwrap();
            repo.add_worktree(&directory.path().join(branch), branch)
                .unwrap();
        }
        let detached = directory.path().join("detached");
        repo.git(&["worktree", "add", "--detach", detached.to_str().unwrap()])
            .unwrap();
        repo.git(&["update-ref", "refs/agit/layout-v0/recovered", "HEAD"])
            .unwrap();
        let journal = Repo::at(directory.path().join("journal"))
            .git_path("agit-checkout-transaction.json")
            .unwrap();
        std::fs::write(&journal, b"pending").unwrap();

        let inspected = repo.clone().local_objects_only();
        let registered = inspected.inspection_worktrees().unwrap();
        let common = main_checkout_common_dir(repo.root()).unwrap();
        let checkouts = Checkouts::inspect(&inspected, Some(&common), &registered);
        for root in [repo.root(), &directory.path().join("clean"), &detached] {
            assert!(
                !checkouts
                    .pending(root, unexpected("metadata"), unexpected("legacy"))
                    .unwrap()
            );
        }
        assert!(
            checkouts
                .pending(
                    &directory.path().join("journal"),
                    unexpected("metadata"),
                    unexpected("legacy"),
                )
                .unwrap()
        );
        let mut legacy_probed = false;
        assert!(
            checkouts
                .pending(
                    &directory.path().join("recovered"),
                    unexpected("metadata"),
                    || {
                        legacy_probed = true;
                        Ok(true)
                    },
                )
                .unwrap()
        );
        assert!(legacy_probed);

        // A recovery ref under another spelling Git resolves, and a checkout whose `.git` link
        // Git's own rules would not resolve to its registration, leave the answer to the probes.
        repo.git(&["update-ref", "refs/tags/refs/agit/layout-v0/clean", "HEAD"])
            .unwrap();
        let unusual = directory.path().join("unusual");
        repo.git(&["branch", "unusual"]).unwrap();
        repo.add_worktree(&unusual, "unusual").unwrap();
        let link = std::fs::read_to_string(unusual.join(".git")).unwrap();
        std::fs::write(unusual.join(".git"), format!("{} \n", link.trim_end())).unwrap();
        let registered = inspected.inspection_worktrees().unwrap();
        let checkouts = Checkouts::inspect(&inspected, Some(&common), &registered);
        for (root, plain) in [(directory.path().join("clean"), true), (unusual, false)] {
            let (mut metadata, mut legacy) = (false, false);
            let pending = checkouts.pending(
                &root,
                || {
                    metadata = true;
                    Ok(false)
                },
                || {
                    legacy = true;
                    Ok(false)
                },
            );
            assert!(!pending.unwrap());
            assert_eq!((metadata, legacy), (!plain, true), "{}", root.display());
        }
    }
}
