//! One worktree per session branch.
//!
//! The main checkout (`~/.agit/repos/<owner>/<name>`) stays on the `main` file line; every session
//! branch gets its own linked worktree under `~/.agit/worktrees/<owner>/<name>/<branch>` the first
//! time it needs a checkout (settlement, merge, `repo path <repo>@<branch>`). Two sessions
//! settling concurrently each write their own index and working tree instead of contending for one
//! HEAD, and deleting a branch no longer collides with "currently checked out".
//!
//! A repo whose main checkout still sits on a session branch migrates on demand: the first time
//! that branch is asked for a checkout, a clean main checkout moves back to `main` and the branch
//! gets a worktree; where it cannot move (uncommitted changes, no main, an open merge transaction)
//! the main checkout keeps serving as its worktree — only without switching back and forth.
//!
//! A worktree is agit's own cache: everything in it comes from the branch ref, and it can be
//! deleted and rebuilt at any time.

use crate::domain::repo::Repo;
use crate::infra::config;
use anyhow::Context as _;
use std::path::{Path, PathBuf};

/// Where this repo's linked worktrees live.
///
/// A repo under `repos/<owner>/<name>` → `worktrees/<owner>/<name>`; one outside the standard
/// location (tests, hand-made paths) → `<name>.worktrees` beside it.
pub fn home_for(primary: &Repo) -> crate::Result<PathBuf> {
    // Canonical paths throughout: git reports canonical worktree paths, and what is built here
    // must compare byte-for-byte equal to them, or neither "is there already a worktree" nor "is
    // this directory its own" can be answered.
    let root = canonical(primary.root());
    if let Ok(home) = config::agit_home()
        && let Ok(relative) = root.strip_prefix(canonical(&home).join("repos"))
        && relative.components().count() == 2
    {
        return Ok(canonical(&home).join("worktrees").join(relative));
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    Ok(root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.to_path_buf())
        .join(format!("{name}.worktrees")))
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Where a branch's worktree belongs, whether or not it exists.
pub fn dir_for(primary: &Repo, branch: &str) -> crate::Result<PathBuf> {
    anyhow::ensure!(
        !branch.is_empty()
            && !branch.starts_with('/')
            && branch
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "`{branch}` cannot name a worktree directory"
    );
    Ok(home_for(primary)?.join(branch))
}

/// The checkout that holds this branch (the main checkout or a linked worktree); never a new one.
///
/// Once the main checkout has been moved (`repo rename`, `clone --mine` promoting in place), a
/// linked worktree's `.git` file still points at the old address, so git repairs it first; a
/// worktree still sitting in the old home moves to its canonical place in the new one — it is a
/// cache, and moving it loses nothing.
pub fn existing(primary: &Repo, branch: &str) -> crate::Result<Option<Repo>> {
    primary.repair_worktrees();
    primary.prune_worktrees()?;
    let Some(holder) = primary
        .worktree_of(branch)?
        .filter(|w| w.path.join(".git").exists())
    else {
        return Ok(None);
    };
    if holder.primary {
        return Ok(Some(Repo::at(holder.path)));
    }
    let wanted = dir_for(primary, branch)?;
    if holder.path != wanted
        && !wanted.exists()
        && primary.move_worktree(&holder.path, &wanted).is_ok()
    {
        if let Ok(all) = config::worktrees_dir() {
            remove_empty_parents(&holder.path, &canonical(&all));
        }
        return Ok(Some(Repo::at(wanted)));
    }
    Ok(Some(Repo::at(holder.path)))
}

/// A `Repo` checked out on this branch, created when there is none.
///
/// The branch must already exist — branches are born only in import / fork / new / run, never
/// here.
pub fn checkout(primary: &Repo, branch: &str) -> crate::Result<Repo> {
    let refname = format!("refs/heads/{branch}");
    anyhow::ensure!(primary.has_ref(&refname), "no branch `{branch}`");
    if primary.current_branch().as_deref() == Some(branch) && !park_primary(primary)? {
        return Ok(primary.clone());
    }
    if let Some(repo) = existing(primary, branch)? {
        return Ok(repo);
    }
    let dir = dir_for(primary, branch)?;
    if dir.exists() {
        // The registration was pruned and the directory survives: an empty one is reused, one
        // with content is left alone — it is not ours.
        let empty = std::fs::read_dir(&dir)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        anyhow::ensure!(
            empty,
            "{} exists but is not a registered worktree; move it away first",
            dir.display()
        );
        std::fs::remove_dir(&dir).with_context(|| format!("cannot reuse {}", dir.display()))?;
    }
    primary.add_worktree(&dir, branch)
}

/// Move the main checkout back to `main`. Returns whether it now sits on main.
///
/// It moves only where that is safe: the main checkout is clean (untracked files count as dirty),
/// main exists, no merge transaction is open, and the checkout preflight passes. Otherwise it
/// stays unchanged — moving a user's uncommitted edits or a transaction in flight is far worse
/// than leaving one more branch occupied.
///
/// `main` may itself be checked out in a linked worktree by then (while the main checkout sat
/// dirty on A, a merge onto main or a `repo path` built one); git refuses to check out one branch
/// in two places, so a clean worktree there is reclaimed first, and a dirty or locked one blocks
/// the move.
pub fn park_primary(primary: &Repo) -> crate::Result<bool> {
    let Some(current) = primary.current_branch() else {
        return Ok(false);
    };
    if current == "main" {
        return Ok(true);
    }
    if primary.is_linked_worktree()
        || !primary.has_ref("refs/heads/main")
        || !primary.is_clean()?
        || crate::domain::mergetx::is_locked(primary.root())
    {
        return Ok(false);
    }
    primary.prune_worktrees()?;
    if let Some(holder) = primary.worktree_of("main")?
        && !holder.primary
    {
        if !Repo::at(&holder.path).is_clean()? {
            return Ok(false);
        }
        primary.remove_worktree(&holder.path)?;
        remove_empty_parents(&holder.path, &home_for(primary)?);
    }
    if let Err(error) = super::plumbing::ensure_safe_checkout(primary, "refs/heads/main") {
        crate::warn(&format!(
            "the primary checkout stays on `{current}`: moving it to main is not safe ({error:#})"
        ));
        return Ok(false);
    }
    primary.switch("main")?;
    Ok(true)
}

/// Free a branch from every checkout holding it (before the branch is deleted).
///
/// Uncommitted content in the worktree — a shared file the merge agent reconciled halfway is that
/// shape — is refused by default and discarded only under `force`; a branch locked by an open
/// merge transaction is never freed: `--abort` or `--continue` first.
pub fn release(primary: &Repo, branch: &str, force: bool) -> crate::Result<()> {
    anyhow::ensure!(
        crate::domain::mergetx::locking(primary.root(), branch).is_none(),
        "`{branch}` is the target of an open merge transaction; `agit merge --abort` or `--continue` first"
    );
    primary.prune_worktrees()?;
    let Some(holder) = primary.worktree_of(branch)? else {
        return Ok(());
    };
    if holder.primary {
        anyhow::ensure!(
            park_primary(primary)?,
            "`{branch}` is checked out in {} and the checkout cannot be parked on main \
             (uncommitted changes, an open merge, or no main); commit or discard them first",
            primary.root().display()
        );
        return Ok(());
    }
    let checkout = Repo::at(&holder.path);
    anyhow::ensure!(
        force || checkout.is_clean()?,
        "the worktree of `{branch}` at {} has uncommitted changes; settle them (`agit commit -m`) or pass --force to discard them",
        holder.path.display()
    );
    primary.remove_worktree(&holder.path)?;
    remove_empty_parents(&holder.path, &home_for(primary)?);
    Ok(())
}

/// Remove every linked worktree of this repo (before the whole repo is deleted). Returns how many
/// were removed.
///
/// The directory itself is deleted afterwards, so a worktree left behind keeps nothing but a
/// `.git` file pointing at empty space; on a re-clone under the same name it also blocks the new
/// worktree's place.
pub fn remove_all(primary: &Repo) -> crate::Result<usize> {
    primary.prune_worktrees()?;
    let mut removed = 0;
    for holder in primary.worktrees()? {
        if holder.primary {
            continue;
        }
        primary.remove_worktree(&holder.path)?;
        removed += 1;
    }
    if let Ok(home) = home_for(primary)
        && home.is_dir()
    {
        let _ = std::fs::remove_dir_all(&home);
    }
    Ok(removed)
}

/// After a branch is renamed, rename its worktree directory too (git has already fixed that
/// worktree's HEAD).
pub fn rename(primary: &Repo, old: &str, new: &str) -> crate::Result<()> {
    primary.prune_worktrees()?;
    let Some(holder) = primary.worktree_of(new)? else {
        return Ok(());
    };
    if holder.primary {
        return Ok(());
    }
    let from = dir_for(primary, old)?;
    if holder.path != from {
        return Ok(());
    }
    let to = dir_for(primary, new)?;
    primary.move_worktree(&from, &to)?;
    remove_empty_parents(&from, &home_for(primary)?);
    Ok(())
}

/// Once a branch like `a/b` is gone, collect the `a/` it emptied as well, up to the worktree home.
fn remove_empty_parents(path: &Path, stop: &Path) {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == stop || !dir.starts_with(stop) {
            break;
        }
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::meta::{self, Meta};

    /// A repo with a main file line and a session branch `s1`; the main checkout sits on `on`.
    fn fixture(on: &str) -> (tempfile::TempDir, Repo) {
        let d = tempfile::tempdir().unwrap();
        // Canonical path: worktree directories compare byte-for-byte with the paths git reports.
        let repo = Repo::init(&d.path().canonicalize().unwrap().join("repo")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        std::fs::write(repo.root().join("AGENTS.md"), "# team\n").unwrap();
        std::fs::create_dir_all(repo.root().join("memory")).unwrap();
        std::fs::write(repo.root().join("memory/.gitkeep"), "").unwrap();
        repo.add_all().unwrap();
        repo.commit("agit: init").unwrap();
        repo.git(&["branch", "s1", "main"]).unwrap();
        repo.git(&["branch", "s2", "main"]).unwrap();
        if on != "main" {
            repo.switch(on).unwrap();
        }
        (d, repo)
    }

    /// A linked worktree sits beside the repo, named after the branch, while the main checkout
    /// stays on main; asking again hands back the same one.
    #[test]
    fn a_session_branch_gets_its_own_worktree_and_the_primary_stays_on_main() {
        let (d, repo) = fixture("main");
        let wt = checkout(&repo, "s1").unwrap();
        assert_eq!(
            wt.root(),
            d.path().canonicalize().unwrap().join("repo.worktrees/s1")
        );
        assert!(wt.is_linked_worktree());
        assert_eq!(wt.current_branch().as_deref(), Some("s1"));
        assert_eq!(repo.current_branch().as_deref(), Some("main"));
        assert_eq!(wt.common_root().unwrap(), repo.root());

        let again = checkout(&repo, "s1").unwrap();
        assert_eq!(again.root(), wt.root());
        assert!(
            existing(&repo, "s2").unwrap().is_none(),
            "no worktree until asked"
        );
    }

    /// Two branches get worktrees that do not disturb each other, each HEAD on its own branch.
    #[test]
    fn two_branches_get_two_worktrees() {
        let (_d, repo) = fixture("main");
        let a = checkout(&repo, "s1").unwrap();
        let b = checkout(&repo, "s2").unwrap();
        assert_ne!(a.root(), b.root());
        assert_eq!(a.current_branch().as_deref(), Some("s1"));
        assert_eq!(b.current_branch().as_deref(), Some("s2"));
    }

    /// The main checkout still sits on a session branch (the legacy layout): a clean one moves
    /// back to main, and the branch then gets a worktree.
    #[test]
    fn a_clean_primary_parked_on_a_session_branch_moves_back_to_main() {
        let (_d, repo) = fixture("s1");
        let wt = checkout(&repo, "s1").unwrap();
        assert!(wt.is_linked_worktree());
        assert_eq!(repo.current_branch().as_deref(), Some("main"));
    }

    /// A dirty main checkout does not move; it keeps serving as that branch's checkout — not one
    /// byte of the user's edits may be lost.
    #[test]
    fn a_dirty_primary_keeps_serving_its_branch() {
        let (_d, repo) = fixture("s1");
        std::fs::write(repo.root().join("AGENTS.md"), "# edited\n").unwrap();
        let wt = checkout(&repo, "s1").unwrap();
        assert_eq!(wt.root(), repo.root());
        assert_eq!(repo.current_branch().as_deref(), Some("s1"));
        assert_eq!(
            std::fs::read_to_string(repo.root().join("AGENTS.md")).unwrap(),
            "# edited\n"
        );
    }

    /// `main` picked up a linked worktree while the main checkout sat dirty on A; once A is clean,
    /// the first settlement must still reach main: reclaim main's (clean) worktree first, then
    /// give A its own.
    #[test]
    fn parking_reclaims_a_clean_linked_worktree_of_main() {
        let (_d, repo) = fixture("s1");
        std::fs::write(repo.root().join("AGENTS.md"), "# dirty\n").unwrap();
        let main_wt = checkout(&repo, "main").unwrap();
        assert!(
            main_wt.is_linked_worktree(),
            "main lives in a linked worktree while the primary is busy"
        );
        std::fs::write(repo.root().join("AGENTS.md"), "# team\n").unwrap();

        let wt = checkout(&repo, "s1").unwrap();
        assert!(wt.is_linked_worktree());
        assert_eq!(repo.current_branch().as_deref(), Some("main"));
        assert!(
            !main_wt.root().exists(),
            "main's linked worktree was reclaimed"
        );
    }

    /// With main's linked worktree dirty nothing moves, and the main checkout keeps carrying A.
    #[test]
    fn parking_keeps_the_primary_when_mains_worktree_is_dirty() {
        let (_d, repo) = fixture("s1");
        std::fs::write(repo.root().join("AGENTS.md"), "# dirty\n").unwrap();
        let main_wt = checkout(&repo, "main").unwrap();
        std::fs::write(repo.root().join("AGENTS.md"), "# team\n").unwrap();
        std::fs::write(main_wt.root().join("memory/draft.md"), "half-reconciled\n").unwrap();

        let wt = checkout(&repo, "s1").unwrap();
        assert_eq!(wt.root(), repo.root());
        assert_eq!(repo.current_branch().as_deref(), Some("s1"));
        assert!(main_wt.root().join("memory/draft.md").exists());
    }

    /// An untracked new file counts as dirty: a memory file just written must not be left behind
    /// in a checkout nobody reads again.
    #[test]
    fn an_untracked_file_keeps_the_primary_from_parking() {
        let (_d, repo) = fixture("s1");
        std::fs::write(repo.root().join("memory/new.md"), "not yet added\n").unwrap();
        let wt = checkout(&repo, "s1").unwrap();
        assert_eq!(wt.root(), repo.root());
        assert_eq!(repo.current_branch().as_deref(), Some("s1"));
    }

    /// With uncommitted content in the worktree, `release` refuses by default and discards only
    /// under `force`.
    #[test]
    fn release_refuses_a_dirty_worktree_unless_forced() {
        let (_d, repo) = fixture("main");
        let wt = checkout(&repo, "s1").unwrap();
        std::fs::write(wt.root().join("memory/draft.md"), "half-reconciled\n").unwrap();
        let error = release(&repo, "s1", false).unwrap_err().to_string();
        assert!(error.contains("uncommitted"), "{error}");
        assert!(wt.root().exists());
        release(&repo, "s1", true).unwrap();
        assert!(!wt.root().exists());
    }

    /// Deleting the whole repo collects every linked worktree, along with their home directory.
    #[test]
    fn remove_all_clears_every_linked_worktree_and_their_home() {
        let (d, repo) = fixture("main");
        checkout(&repo, "s1").unwrap();
        checkout(&repo, "s2").unwrap();
        assert_eq!(remove_all(&repo).unwrap(), 2);
        assert!(
            !d.path()
                .canonicalize()
                .unwrap()
                .join("repo.worktrees")
                .exists()
        );
        assert_eq!(repo.worktrees().unwrap().len(), 1);
    }

    /// Deleting a branch releases its worktree first; a name like `a/b` also gets the parent
    /// directory it emptied collected.
    #[test]
    fn release_removes_the_worktree_and_empty_parents() {
        let (d, repo) = fixture("main");
        repo.git(&["branch", "topic/x", "main"]).unwrap();
        let wt = checkout(&repo, "topic/x").unwrap();
        assert!(wt.root().join(".git").exists());
        release(&repo, "topic/x", false).unwrap();
        assert!(!wt.root().exists());
        assert!(
            !d.path()
                .canonicalize()
                .unwrap()
                .join("repo.worktrees/topic")
                .exists()
        );
        repo.git(&["branch", "-D", "topic/x"]).unwrap();
    }

    /// A rename moves the worktree directory with it; git fixes HEAD itself.
    #[test]
    fn rename_moves_the_worktree_directory() {
        let (d, repo) = fixture("main");
        let wt = checkout(&repo, "s1").unwrap();
        repo.git(&["branch", "-m", "s1", "s9"]).unwrap();
        rename(&repo, "s1", "s9").unwrap();
        assert!(!wt.root().exists());
        let moved = Repo::at(d.path().canonicalize().unwrap().join("repo.worktrees/s9"));
        assert_eq!(moved.current_branch().as_deref(), Some("s9"));
    }

    /// Once the main checkout's whole directory has moved (rename, promotion in place), its
    /// worktrees are still found and usable, and they move to their place in the new home.
    #[test]
    fn a_moved_primary_still_reaches_its_worktrees() {
        let (d, repo) = fixture("main");
        let wt = checkout(&repo, "s1").unwrap();
        let base = d.path().canonicalize().unwrap();
        std::fs::rename(repo.root(), base.join("moved")).unwrap();
        let moved = Repo::at(base.join("moved"));

        let found = existing(&moved, "s1")
            .unwrap()
            .expect("the worktree survives the move");
        assert_eq!(found.root(), base.join("moved.worktrees/s1"));
        assert!(!wt.root().exists(), "moved out of the old home");
        assert_eq!(found.current_branch().as_deref(), Some("s1"));
        assert_eq!(found.common_root().unwrap(), base.join("moved"));
    }

    /// A branch name cannot carry the worktree directory outside the home.
    #[test]
    fn traversal_in_a_branch_name_is_refused() {
        let (_d, repo) = fixture("main");
        assert!(dir_for(&repo, "../escape").is_err());
        assert!(dir_for(&repo, "a//b").is_err());
    }
}
