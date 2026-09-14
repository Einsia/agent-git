//! Cache capture reads Git configuration and redirect metadata without initializing Git LFS.
//! Payload access is deferred; missing cache directories do not invalidate the captured path.

use super::{CONFIG_LIMIT, Source};
use crate::domain::repo::inspection_git_path_spelling;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum Failure {
    #[error("cannot capture the LFS source Git directory")]
    GitDirectory,
    #[error("cannot capture LFS storage configuration")]
    Configuration,
    #[error("LFS storage redirect metadata is unsupported or incomplete")]
    Redirect,
    #[error("captured LFS cache path is unsupported")]
    Path,
}

type Result<T> = std::result::Result<T, Failure>;

pub(super) fn capture(source: &Source) -> Result<PathBuf> {
    let gitdir = source
        .text(&["rev-parse", "--absolute-git-dir"])
        .map_err(|_| Failure::GitDirectory)?;
    let gitdir = resolve(&source.root, &gitdir)?;
    let gitdir =
        inspection_git_path_spelling(gitdir.canonicalize().map_err(|_| Failure::GitDirectory)?);
    if !gitdir.is_dir() {
        return Err(Failure::GitDirectory);
    }
    let output = source
        .output(&["config", "--includes", "--null", "--get", "lfs.storage"])
        .map_err(|_| Failure::Configuration)?;
    let raw = if output.status.success() && output.stderr.is_empty() {
        let value = output
            .stdout
            .strip_suffix(&[0])
            .ok_or(Failure::Configuration)?;
        if value.contains(&0) {
            return Err(Failure::Configuration);
        }
        std::str::from_utf8(value).map_err(|_| Failure::Configuration)?
    } else if output.status.code() == Some(1)
        && output.stderr.is_empty()
        && output.stdout.is_empty()
    {
        ""
    } else {
        return Err(Failure::Configuration);
    };
    let storage = git_storage(&gitdir)?;
    let storage = resolve(&storage, if raw.is_empty() { "lfs" } else { raw })?;
    resolve(&storage, "objects")
}

fn git_storage(gitdir: &Path) -> Result<PathBuf> {
    let common = gitdir.join("commondir");
    // Git LFS keeps a worktree-local cache when its Git directory owns an objects directory.
    if gitdir.join("objects").is_dir() || !common.is_file() {
        return Ok(gitdir.to_owned());
    }
    let Ok(file) = std::fs::File::open(common) else {
        return Ok(gitdir.to_owned());
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Ok(gitdir.to_owned());
    }
    let mut bytes = Vec::new();
    if file
        .take(CONFIG_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Ok(gitdir.to_owned());
    }
    if bytes.len() > CONFIG_LIMIT {
        return Err(Failure::Redirect);
    }
    let common = std::str::from_utf8(&bytes)
        .map_err(|_| Failure::Redirect)?
        .trim();
    resolve(gitdir, common)
}

fn resolve(base: &Path, value: &str) -> Result<PathBuf> {
    if value.chars().any(char::is_control) {
        return Err(Failure::Path);
    }
    let path = inspection_git_path_spelling(PathBuf::from(value));
    #[cfg(windows)]
    {
        use std::path::Prefix;
        let prefix = match path.components().next() {
            Some(Component::Prefix(prefix)) => Some(prefix.kind()),
            _ => None,
        };
        if (path.has_root() || prefix.is_some()) && !path.is_absolute() {
            return Err(Failure::Path);
        }
        if prefix.is_some_and(|prefix| !matches!(prefix, Prefix::Disk(_) | Prefix::UNC(_, _))) {
            return Err(Failure::Path);
        }
    }
    let path = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    if !path.is_absolute() || path.to_str().is_none() {
        return Err(Failure::Path);
    }
    // Lexical joins precede filesystem traversal, so link/.. does not follow the link first.
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if result.file_name().is_some() {
                    result.pop();
                }
            }
            part => result.push(part.as_os_str()),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_redirect_and_lexical_paths_do_not_require_existing_cache() {
        let root = tempfile::tempdir().unwrap();
        let root = inspection_git_path_spelling(root.path().canonicalize().unwrap());
        let gitdir = root.join("gitdir");
        std::fs::create_dir(&gitdir).unwrap();
        assert_eq!(git_storage(&gitdir).unwrap(), gitdir);
        std::fs::write(gitdir.join("commondir"), " ../missing-common \n").unwrap();
        assert_eq!(git_storage(&gitdir).unwrap(), root.join("missing-common"));
        std::fs::create_dir(gitdir.join("objects")).unwrap();
        assert_eq!(git_storage(&gitdir).unwrap(), gitdir);
        std::fs::remove_dir(gitdir.join("objects")).unwrap();
        std::fs::write(gitdir.join("commondir"), "\t\r\n").unwrap();
        assert_eq!(git_storage(&gitdir).unwrap(), gitdir);
        std::fs::write(gitdir.join("commondir"), vec![b'x'; CONFIG_LIMIT + 1]).unwrap();
        assert_eq!(git_storage(&gitdir).unwrap_err(), Failure::Redirect);
        std::fs::write(gitdir.join("commondir"), [0xff]).unwrap();
        assert_eq!(git_storage(&gitdir).unwrap_err(), Failure::Redirect);
        assert_eq!(
            resolve(&gitdir, "a/../cache").unwrap(),
            gitdir.join("cache")
        );
        assert_eq!(
            resolve(&gitdir, "~/literal").unwrap(),
            gitdir.join("~/literal")
        );
        assert_eq!(
            resolve(&gitdir, " spaced ").unwrap(),
            gitdir.join(" spaced ")
        );
        assert_eq!(
            resolve(&gitdir, "PRIVATE\nPATH").unwrap_err(),
            Failure::Path
        );
        assert!(!gitdir.join("lfs").exists());
        assert!(!root.join("missing-common").exists());
    }

    #[cfg(unix)]
    #[test]
    fn lexical_parent_does_not_follow_cache_links_and_objects_links_are_directories() {
        let root = tempfile::tempdir().unwrap();
        let gitdir = root.path().join("gitdir");
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir(&gitdir).unwrap();
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, gitdir.join("link")).unwrap();
        assert_eq!(
            resolve(&gitdir, "link/../payload").unwrap(),
            gitdir.join("payload")
        );
        std::fs::write(gitdir.join("commondir"), "../common").unwrap();
        std::os::unix::fs::symlink(&elsewhere, gitdir.join("objects")).unwrap();
        assert_eq!(git_storage(&gitdir).unwrap(), gitdir);
        assert!(!gitdir.join("payload").exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_cache_paths_keep_the_selected_volume_and_share() {
        let base = Path::new(r"C:\source\.git");
        for path in [
            r"C:cache",
            "C:",
            r"\cache",
            "/cache",
            r"\\.\pipe\cache",
            r"\\?\Volume{opaque}\cache",
        ] {
            assert_eq!(resolve(base, path).unwrap_err(), Failure::Path, "{path}");
        }
        assert_eq!(
            resolve(base, r"..\cache").unwrap(),
            Path::new(r"C:\source\cache")
        );
        assert_eq!(
            resolve(base, r"\\?\C:\cache\..\objects").unwrap(),
            Path::new(r"C:\objects")
        );
        assert_eq!(
            resolve(base, r"\\server\share\..\objects").unwrap(),
            Path::new(r"\\server\share\objects")
        );
    }
}
