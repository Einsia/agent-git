//! Git worktrees, read from the files Git keeps for them, without running Git: hooks ask on
//! every turn, and a Git process per turn is too slow there.
//!
//! A linked worktree's `.git` is a file naming its Git directory, `<common>/worktrees/<id>`. That
//! directory names the repository's common directory in `commondir`, and names the worktree's
//! `.git` file back in `gitdir`. The main checkout is the one whose `.git` directory is the
//! common directory. Paths are compared only once canonicalized, never as Git spells them, so a
//! relative `gitdir:`, forward slashes on Windows and `\\?\` forms all compare alike. Anything
//! that cannot be read, or does not point back, is no worktree.

use std::path::{Path, PathBuf};

/// A linked worktree and the main checkout of its repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Linked {
    /// The worktree's top level.
    pub worktree: PathBuf,
    /// The main checkout's top level.
    pub main: PathBuf,
}

/// Larger than any `.git`, `commondir` or `gitdir` file Git writes; a larger one is not Git's.
const POINTER_BYTES: u64 = 4096;
/// The most worktrees of one checkout looked at.
const MAX_WORKTREES: usize = 64;

/// The linked worktree that `path` is in, with its main checkout, both canonical. `None` when the
/// nearest checkout above `path` is a main checkout, a separate clone, a submodule or a worktree
/// of a bare repository, when its files cannot be read or do not point back at it, and when the
/// worktree or its main checkout is a home directory or a filesystem root, which hold everything
/// on the computer rather than one project.
pub fn linked(path: &Path) -> Option<Linked> {
    let path = path.canonicalize().ok()?;
    let worktree = path
        .ancestors()
        .find(|dir| dir.join(".git").symlink_metadata().is_ok())?;
    let dotgit = worktree.join(".git");
    if !dotgit.symlink_metadata().ok()?.is_file() || holds_everything(worktree) {
        return None;
    }
    let pointer = read_pointer(&dotgit)?;
    let gitdir = resolve(worktree, pointer.strip_prefix("gitdir:")?.trim())?;
    let common = resolve(&gitdir, &read_pointer(&gitdir.join("commondir"))?)?;
    if gitdir.parent() != Some(common.join("worktrees").as_path())
        || resolve(&gitdir, &read_pointer(&gitdir.join("gitdir"))?)?
            != dotgit.canonicalize().ok()?
    {
        return None;
    }
    let main = common.parent()?;
    if main.join(".git").canonicalize().ok()? != common
        || !main.join(".git").symlink_metadata().ok()?.is_dir()
        || holds_everything(main)
    {
        return None;
    }
    Some(Linked {
        worktree: worktree.to_path_buf(),
        main: main.to_path_buf(),
    })
}

/// The linked worktrees of the main checkout at `main`, canonical, as Git lists them in its
/// `.git/worktrees`, each confirmed by [`linked`]. One whose directory is gone is left out.
pub fn of(main: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(main.join(".git").join("worktrees")) else {
        return Vec::new();
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect();
    names.sort();
    names
        .into_iter()
        .take(MAX_WORKTREES)
        .filter_map(|gitdir| {
            let dotgit = resolve(&gitdir, &read_pointer(&gitdir.join("gitdir"))?)?;
            let worktree = dotgit.parent()?;
            let found = linked(worktree)?;
            (found.main == main && found.worktree == worktree).then_some(found.worktree)
        })
        .collect()
}

fn holds_everything(dir: &Path) -> bool {
    dir.parent().is_none()
        || crate::infra::config::user_home()
            .and_then(|home| home.canonicalize().ok())
            .is_some_and(|home| home == dir)
}

/// A small file Git writes holding one path, trimmed.
fn read_pointer(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(POINTER_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() as u64 <= POINTER_BYTES).then(|| text.trim().to_owned())
}

/// `value` as Git means it, relative to `base` when not absolute, canonicalized.
fn resolve(base: &Path, value: &str) -> Option<PathBuf> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    base.join(value).canonicalize().ok()
}
