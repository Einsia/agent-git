use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const ARCHIVE: &[u8] = include_bytes!(env!("AGIT_GIT_RUNTIME_ARCHIVE"));
static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();

pub(super) struct Runtime {
    root: PathBuf,
}

pub(super) fn runtime() -> Option<&'static Runtime> {
    RUNTIME.get().and_then(Option::as_ref)
}

pub(super) fn initialize() -> Result<()> {
    if RUNTIME.get().is_some() {
        return Ok(());
    }
    if std::env::var_os("AGIT_USE_SYSTEM_GIT").as_deref() == Some(OsStr::new("1")) {
        let _ = RUNTIME.set(None);
        return Ok(());
    }
    let home = crate::infra::config::agit_home()?.join("git-runtime");
    let runtime = materialize(ARCHIVE, &home)
        .context("cannot prepare bundled Git; check AGIT_HOME permissions or set AGIT_USE_SYSTEM_GIT=1 to use your installed Git and Git LFS")?;
    let _ = RUNTIME.set(Some(runtime));
    Ok(())
}

fn private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Git runtime cache must be a directory"
        );
        validate_unix_cache(path)?;
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(path.parent().context("Git cache has no parent")?)?;
        crate::infra::windows_security::private_directory(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn validate_unix_cache(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let owner = unsafe { libc::geteuid() };
    let canonical = std::fs::canonicalize(path)?;
    for (index, ancestor) in canonical.ancestors().enumerate() {
        let metadata = std::fs::symlink_metadata(ancestor)?;
        ensure!(
            metadata.is_dir(),
            "Git runtime ancestors must be directories"
        );
        if index == 0 {
            ensure!(
                metadata.uid() == owner,
                "Git runtime cache must belong to the current user"
            );
            ensure!(
                metadata.mode() & 0o022 == 0,
                "Git runtime cache must not be writable by other users"
            );
        } else {
            ensure!(
                (metadata.uid() == owner || metadata.uid() == 0)
                    && (metadata.mode() & 0o022 == 0 || metadata.mode() & 0o1000 != 0),
                "Git runtime ancestors must prevent replacement by other users: {}",
                ancestor.display()
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_unix_contents(root: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let owner = unsafe { libc::geteuid() };
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            (metadata.is_dir() || metadata.is_file())
                && metadata.uid() == owner
                && metadata.mode() & 0o022 == 0,
            "Git runtime contents must belong to the current user and reject writes by other users: {}",
            path.display()
        );
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(())
}

fn materialize(bytes: &[u8], cache: &Path) -> Result<Runtime> {
    private_directory(cache)?;
    let cache = super::path_for_git(std::fs::canonicalize(cache)?);
    let digest = hex::encode(Sha256::digest(bytes));
    let root = cache.join(&digest);
    let runtime = Runtime { root };
    let lock_path = cache.join(format!("{digest}.lock"));
    #[cfg(windows)]
    let lock = crate::infra::windows_security::open_private_control(&lock_path)?;
    #[cfg(unix)]
    let lock = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(lock_path)?
    };
    lock.lock_exclusive()?;
    if runtime.root.exists() {
        runtime.validate()?;
        return Ok(runtime);
    }
    let mut staging_builder = tempfile::Builder::new();
    staging_builder.prefix(".unpack-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staging_builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let staging = staging_builder.tempdir_in(&cache)?;
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        ensure!(
            path.components()
                .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
            "Git runtime archive contains an unsafe path"
        );
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir() || kind.is_hard_link(),
            "Git runtime archive contains an unsupported entry"
        );
        if let Some(target) = entry.link_name()? {
            ensure!(
                target
                    .components()
                    .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
                "Git runtime archive contains an unsafe link"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let destination = staging.path().join(path.as_ref());
            let directory = if kind.is_dir() {
                destination.as_path()
            } else {
                destination
                    .parent()
                    .context("Git runtime entry has no parent")?
            };
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)?;
        }
        ensure!(
            entry.unpack_in(staging.path())?,
            "Git runtime entry escaped its directory"
        );
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let quote = |path: PathBuf| -> Result<String> {
            Ok(serde_json::to_string(
                path.to_str().context("Git runtime path is not UTF-8")?,
            )?)
        };
        let defaults = format!(
            "[http]\n\tsslCAInfo = {}\n[init]\n\ttemplateDir = {}\n[include]\n\tpath = /etc/gitconfig\n",
            quote(runtime.root.join("ssl/cert.pem"))?,
            quote(runtime.root.join("share/git-core/templates"))?,
        );
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(staging.path().join("gitconfig"))?
            .write_all(defaults.as_bytes())?;
    }
    Runtime {
        root: staging.path().to_owned(),
    }
    .validate()?;
    std::fs::rename(staging.path(), &runtime.root)?;
    Ok(runtime)
}

impl Runtime {
    pub(super) fn git(&self) -> PathBuf {
        self.root.join(if cfg!(windows) {
            "cmd/git.exe"
        } else {
            "bin/git"
        })
    }

    fn exec_path(&self) -> PathBuf {
        self.root.join(if cfg!(windows) {
            "mingw64/libexec/git-core"
        } else {
            "libexec/git-core"
        })
    }

    fn validate(&self) -> Result<()> {
        #[cfg(unix)]
        validate_unix_contents(&self.root)?;
        let metadata = std::fs::symlink_metadata(&self.root)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Git runtime is not a regular directory"
        );
        for path in [
            self.git(),
            self.exec_path()
                .join(format!("git-lfs{}", std::env::consts::EXE_SUFFIX)),
        ] {
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| format!("Git runtime is incomplete: {}", path.display()))?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Git runtime executable is not a regular file"
            );
        }
        Ok(())
    }

    pub(super) fn configure(&self, command: &mut Command) {
        let inherited = command
            .get_envs()
            .find(|(key, _)| key.as_encoded_bytes().eq_ignore_ascii_case(b"PATH"))
            .map(|(_, value)| value.map(OsStr::to_owned))
            .unwrap_or(None);
        let mut paths = vec![self.git().parent().unwrap().to_owned(), self.exec_path()];
        if cfg!(windows) {
            paths.push(self.root.join("mingw64/bin"));
            paths.push(self.root.join("usr/bin"));
        }
        if let Some(inherited) = inherited {
            for path in std::env::split_paths(&inherited) {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        let path: OsString = std::env::join_paths(paths)
            .expect("existing executable search paths must be representable");
        command
            .env("PATH", path)
            .env("GIT_EXEC_PATH", self.exec_path());
        #[cfg(target_os = "linux")]
        if !command
            .get_envs()
            .any(|(key, _)| key == "GIT_CONFIG_SYSTEM")
        {
            command.env("GIT_CONFIG_SYSTEM", self.root.join("gitconfig"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut archive = tar::Builder::new(encoder);
        let runtime = Runtime {
            root: PathBuf::new(),
        };
        for path in [
            runtime.git(),
            runtime
                .exec_path()
                .join(format!("git-lfs{}", std::env::consts::EXE_SUFFIX)),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o755);
            header.set_cksum();
            archive
                .append_data(&mut header, path, &b"test"[..])
                .unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn concurrent_extraction_publishes_one_complete_runtime() {
        let home = tempfile::tempdir().unwrap();
        let cache = home.path().join("git-runtime");
        let archive = fixture();
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..3)
                .map(|_| scope.spawn(|| materialize(&archive, &cache).unwrap().root))
                .collect();
            let paths: Vec<_> = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect();
            assert!(paths.windows(2).all(|pair| pair[0] == pair[1]));
            assert!(!std::fs::read_dir(&cache).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".unpack-")
            }));
        });
    }

    #[test]
    fn private_search_paths_preserve_explicit_user_configuration() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: root.path().to_owned(),
        };
        let parent_path = std::env::var_os("PATH");
        let mut command = Command::new(runtime.git());
        command
            .env_clear()
            .env("PATH", root.path())
            .env("GIT_CONFIG_GLOBAL", "custom-profile");
        runtime.configure(&mut command);
        let environment: std::collections::BTreeMap<_, _> = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
            .collect();
        assert_eq!(
            environment.get(OsStr::new("GIT_CONFIG_GLOBAL")),
            Some(&Some("custom-profile".into()))
        );
        assert_eq!(
            environment.get(OsStr::new("GIT_EXEC_PATH")),
            Some(&Some(runtime.exec_path().into_os_string()))
        );
        assert_eq!(std::env::var_os("PATH"), parent_path);
        let paths: Vec<_> = std::env::split_paths(
            environment
                .get(OsStr::new("PATH"))
                .unwrap()
                .as_ref()
                .unwrap(),
        )
        .collect();
        assert_eq!(
            paths.first(),
            runtime.git().parent().map(Path::to_path_buf).as_ref()
        );
        assert_eq!(paths.last(), Some(&root.path().to_path_buf()));
    }

    #[test]
    fn incomplete_cache_fails_without_falling_back_to_system_git() {
        let home = tempfile::tempdir().unwrap();
        let cache = home.path().join("git-runtime");
        let archive = fixture();
        let runtime = materialize(&archive, &cache).unwrap();
        std::fs::remove_file(runtime.git()).unwrap();
        assert!(materialize(&archive, &cache).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn foreign_owned_cache_is_rejected_before_creating_a_lock() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let home = tempfile::tempdir().unwrap();
        let cache = if unsafe { libc::geteuid() } == 0 {
            std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::chown(home.path(), Some(1), None).unwrap();
            home.path()
        } else {
            Path::new("/")
        };
        let metadata = std::fs::metadata(cache).unwrap();
        assert_ne!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o777, 0o755);
        let error = materialize(&fixture(), cache).err().unwrap();
        assert!(
            error
                .to_string()
                .contains("must belong to the current user")
        );
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn private_cache_requires_ancestors_that_prevent_replacement() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let cache = home.path().join("cache");
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = materialize(&fixture(), &cache).err().unwrap();
        assert!(error.to_string().contains("must prevent replacement"));
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o1777)).unwrap();
        materialize(&fixture(), &cache).unwrap();
    }
}
