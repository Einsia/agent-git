//! Explicit permission recovery is limited to named state carriers, never a recursive chmod.

use anyhow::{Context, Result, ensure};
use std::{
    ffi::{CString, OsStr},
    fs::{File, Metadata, Permissions},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Copy)]
struct Constraint {
    directory: bool,
    forbidden: u32,
}

impl Constraint {
    const DIRECTORY: Self = Self {
        directory: true,
        forbidden: 0o022,
    };
    const FILE: Self = Self {
        directory: false,
        forbidden: 0o022,
    };
    const PRIVATE_DIRECTORY: Self = Self {
        directory: true,
        forbidden: 0o077,
    };
    const PRIVATE_FILE: Self = Self {
        directory: false,
        forbidden: 0o077,
    };
}

fn constraint(relative: &Path) -> Result<Constraint> {
    let names = relative
        .iter()
        .map(|part| part.to_str().context("state path is not Unicode"))
        .collect::<Result<Vec<_>>>()?;
    let ordinary_dir = Constraint::DIRECTORY;
    let ordinary_file = Constraint::FILE;
    let value = match names.as_slice() {
        [] | ["store"] | ["store", ".locks"] | ["repos"] => ordinary_dir,
        ["layout-v1.lock"] => ordinary_file,
        ["store", runtime] if crate::adapter::RUNTIMES.contains(runtime) => ordinary_dir,
        ["store", runtime, filename] if crate::adapter::RUNTIMES.contains(runtime) => {
            let id = filename
                .strip_suffix(".json.lock")
                .or_else(|| filename.strip_suffix(".json"))
                .context("repair requires a native Link or its lock")?;
            crate::domain::merge_archive::RuntimeLinkKey {
                runtime: (*runtime).into(),
                session_id: id.into(),
            }
            .validate()?;
            ordinary_file
        }
        ["store", ".locks", "branches" | "repositories"] => ordinary_dir,
        ["store", ".locks", "branches" | "repositories", filename]
            if filename.strip_suffix(".lock").is_some_and(|digest| {
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
            }) =>
        {
            ordinary_file
        }
        ["repos", owner] => {
            crate::domain::repo::valid_name(owner)?;
            ordinary_dir
        }
        ["repos", owner, name, tail @ ..] => {
            crate::domain::repo::valid_name(owner)?;
            crate::domain::repo::valid_name(name)?;
            match tail {
                [] | [".git"] => ordinary_dir,
                [".git", "AGIT_MERGE_TX" | "AGIT_MERGE_TX.control"] => ordinary_file,
                [".git", "AGIT_MERGE_ARCHIVES"] => Constraint::PRIVATE_DIRECTORY,
                [".git", "AGIT_MERGE_ARCHIVES", filename] => {
                    let generation = [".json", ".recovery", ".control"]
                        .iter()
                        .find_map(|suffix| filename.strip_suffix(suffix))
                        .context("repair requires a named archive journal carrier")?;
                    crate::domain::merge_archive::checked_generation(generation)?;
                    Constraint::PRIVATE_FILE
                }
                [".git", filename] => {
                    let generation = filename
                        .strip_prefix("AGIT_MERGE_TX.landed-")
                        .or_else(|| filename.strip_prefix("AGIT_MERGE_TX.aborted-"))
                        .and_then(|name| name.strip_suffix(".json"))
                        .context("path is not a repairable transaction carrier")?;
                    crate::domain::merge_archive::checked_generation(generation)?;
                    Constraint::PRIVATE_FILE
                }
                _ => anyhow::bail!("path is not a repairable Agit authority carrier"),
            }
        }
        _ => anyhow::bail!("path is not a repairable Agit authority carrier"),
    };
    Ok(value)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "repair paths must not contain '..'"
    );
    Ok(std::path::absolute(path)?)
}

fn open_at(parent: &File, name: &OsStr, directory: bool) -> Result<File> {
    let name = CString::new(name.as_bytes())?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // Descriptor-relative, non-following opens bind every operation to its inspected parent.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    ensure!(
        descriptor >= 0,
        "cannot open state carrier without following redirects: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn owned(metadata: &Metadata, rule: Constraint, path: &Path, uid: u32) -> Result<()> {
    ensure!(
        metadata.uid() == uid,
        "refusing foreign-owned state {} (mode {:04o}, uid {}, required uid {})",
        path.display(),
        metadata.mode() & 0o7777,
        metadata.uid(),
        uid
    );
    ensure!(
        if rule.directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "state carrier has an unexpected type: {}",
        path.display()
    );
    ensure!(
        rule.directory || metadata.nlink() == 1,
        "refusing multiply-linked state carrier: {}",
        path.display()
    );
    Ok(())
}

struct Carrier {
    file: File,
    name: std::ffi::OsString,
    path: PathBuf,
    rule: Constraint,
}

struct Repair {
    anchor: File,
    carriers: Vec<Carrier>,
    uid: u32,
}

impl Repair {
    fn prepare(home: &Path, target: &Path, uid: u32) -> Result<Self> {
        let home = absolute(home)?;
        let target = absolute(target)?;
        let relative = target
            .strip_prefix(&home)
            .context("repair target must be within AGIT_HOME")?;
        constraint(relative)
            .with_context(|| format!("refusing permission repair for {}", target.display()))?;
        let parent = home
            .parent()
            .context("AGIT_HOME must not be a filesystem root")?;
        crate::domain::merge_archive::validate_unix_ancestors(&home)?;
        let canonical = parent.canonicalize()?;
        let mut anchor = File::open("/")?;
        let mut outside = PathBuf::from("/");
        for component in canonical.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            outside.push(name);
            anchor = open_at(&anchor, name, true).with_context(|| {
                format!("cannot traverse external ancestor {}", outside.display())
            })?;
            let metadata = anchor.metadata()?;
            ensure!(
                (metadata.uid() == uid || metadata.uid() == 0)
                    && (metadata.mode() & 0o022 == 0 || metadata.mode() & 0o1000 != 0),
                "unsafe ancestor outside AGIT_HOME: {} (mode {:04o}, uid {}); have its owner secure it before retrying",
                outside.display(),
                metadata.mode() & 0o7777,
                metadata.uid()
            );
        }
        let mut repair = Self {
            anchor,
            carriers: Vec::new(),
            uid,
        };
        let mut path = home.clone();
        repair.push(&path, Constraint::DIRECTORY)?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                anyhow::bail!("repair path must contain only ordinary components");
            };
            path.push(name);
            repair.push(&path, constraint(path.strip_prefix(&home)?)?)?;
        }
        Ok(repair)
    }

    fn push(&mut self, path: &Path, rule: Constraint) -> Result<()> {
        let parent = self
            .carriers
            .last()
            .map_or(&self.anchor, |carrier| &carrier.file);
        let name = path.file_name().context("carrier has no filename")?;
        let file = open_at(parent, name, rule.directory)
            .with_context(|| format!("cannot inspect {}", path.display()))?;
        owned(&file.metadata()?, rule, path, self.uid)?;
        self.carriers.push(Carrier {
            file,
            name: name.into(),
            path: path.into(),
            rule,
        });
        Ok(())
    }

    fn apply(&self) -> Result<Vec<String>> {
        let mut report = Vec::new();
        for (index, carrier) in self.carriers.iter().enumerate() {
            let parent = if index == 0 {
                &self.anchor
            } else {
                &self.carriers[index - 1].file
            };
            let current = open_at(parent, &carrier.name, carrier.rule.directory)
                .with_context(|| format!("cannot recheck {}", carrier.path.display()))?;
            let metadata = carrier.file.metadata()?;
            let current_metadata = current.metadata()?;
            ensure!(
                metadata.dev() == current_metadata.dev()
                    && metadata.ino() == current_metadata.ino(),
                "state carrier was substituted during permission repair: {}",
                carrier.path.display()
            );
            owned(&metadata, carrier.rule, &carrier.path, self.uid)?;
            let before = metadata.mode() & 0o7777;
            let after = before & !carrier.rule.forbidden;
            if before != after {
                // Removing bits on the verified inode preserves bytes, locks and stricter modes.
                carrier
                    .file
                    .set_permissions(Permissions::from_mode(after))
                    .with_context(|| format!("cannot secure {}", carrier.path.display()))?;
            }
            let final_metadata = carrier.file.metadata()?;
            owned(&final_metadata, carrier.rule, &carrier.path, self.uid)?;
            ensure!(
                final_metadata.mode() & carrier.rule.forbidden == 0,
                "state permissions changed during repair: {}",
                carrier.path.display()
            );
            report.push(format!("{}: mode {before:04o} -> {after:04o}, uid {}; required uid {} and mode & {:04o} == 0",
                carrier.path.display(), metadata.uid(), self.uid, carrier.rule.forbidden));
        }
        Ok(report)
    }
}

pub(super) fn run(path: &Path) -> super::CmdResult {
    let home = crate::infra::config::agit_home()?;
    let repair = Repair::prepare(&home, path, unsafe { libc::geteuid() })?;
    for line in repair.apply()? {
        crate::ui::success(&line);
    }
    crate::ui::info(
        "Permission preparation complete. Retry the original operation; content and identity validation still apply.",
    );
    Ok(crate::ExitCode::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap().join("state");
        let target = home.join("store/codex/synthetic.json");
        crate::infra::config::create_state_dir(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"SYNTHETIC-STATE").unwrap();
        std::fs::set_permissions(&target, Permissions::from_mode(0o664)).unwrap();
        (temp, home, target)
    }

    #[test]
    fn repair_refuses_substituted_symlink_and_foreign_authority() {
        let (_temp, home, target) = fixture();
        let uid = unsafe { libc::geteuid() };
        if uid == 0 {
            let name = CString::new(target.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::chown(name.as_ptr(), 1, 1) }, 0);
            assert!(
                Repair::prepare(&home, &target, uid)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("foreign-owned")
            );
            assert_eq!(unsafe { libc::chown(name.as_ptr(), 0, 0) }, 0);
        } else {
            // A root-owned directory supplies real foreign metadata without changing ownership.
            let foreign = Path::new("/usr");
            assert!(
                Repair::prepare(foreign, foreign, uid)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("foreign-owned")
            );
        }
        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o664);

        let repair = Repair::prepare(&home, &target, uid).unwrap();
        let retained = target.with_extension("retained");
        std::fs::rename(&target, &retained).unwrap();
        std::fs::write(&target, b"SYNTHETIC-SUBSTITUTION").unwrap();
        assert!(
            repair
                .apply()
                .unwrap_err()
                .to_string()
                .contains("substituted")
        );
        assert_eq!(std::fs::read(&retained).unwrap(), b"SYNTHETIC-STATE");
        assert_eq!(std::fs::metadata(&retained).unwrap().mode() & 0o777, 0o664);

        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&retained, &target).unwrap();
        assert!(Repair::prepare(&home, &target, uid).is_err());
        std::fs::remove_file(&target).unwrap();
        std::fs::hard_link(&retained, &target).unwrap();
        assert!(
            Repair::prepare(&home, &target, uid)
                .err()
                .unwrap()
                .to_string()
                .contains("multiply-linked")
        );
    }

    #[test]
    fn repair_refuses_external_ancestors_and_unrecognized_paths_before_mutation() {
        let (_temp, home, target) = fixture();
        let uid = unsafe { libc::geteuid() };
        let unknown = home.join("unrelated");
        std::fs::write(&unknown, b"SYNTHETIC-UNRELATED").unwrap();
        std::fs::set_permissions(&unknown, Permissions::from_mode(0o666)).unwrap();
        assert!(Repair::prepare(&home, &unknown, uid).is_err());
        assert!(Repair::prepare(&home, home.parent().unwrap(), uid).is_err());
        let parent = home.parent().unwrap();
        std::fs::set_permissions(parent, Permissions::from_mode(0o777)).unwrap();
        assert!(Repair::prepare(&home, &target, uid).is_err());
        assert_eq!(std::fs::metadata(parent).unwrap().mode() & 0o777, 0o777);
        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o664);
        assert_eq!(std::fs::metadata(&unknown).unwrap().mode() & 0o777, 0o666);
        std::fs::set_permissions(parent, Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn private_archive_repair_only_removes_forbidden_bits_and_preserves_content() {
        let (_temp, home, _) = fixture();
        let repo = crate::domain::repo::Repo::init(&home.join("repos/me/qa")).unwrap();
        let generation = uuid::Uuid::now_v7().to_string();
        let target = home
            .join("repos/me/qa/.git/AGIT_MERGE_ARCHIVES")
            .join(format!("{generation}.json"));
        crate::infra::config::create_state_dir(target.parent().unwrap()).unwrap();
        crate::infra::config::state_file_options()
            .create_new(true)
            .write(true)
            .open(target.with_extension("control"))
            .unwrap();
        std::fs::write(&target, b"SYNTHETIC-UNVALIDATED-JOURNAL").unwrap();
        std::fs::set_permissions(target.parent().unwrap(), Permissions::from_mode(0o750)).unwrap();
        std::fs::set_permissions(&target, Permissions::from_mode(0o440)).unwrap();
        let before = std::fs::metadata(&target).unwrap();
        let repair = Repair::prepare(&home, &target, unsafe { libc::geteuid() }).unwrap();
        repair.apply().unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o400);
        assert_eq!(
            std::fs::metadata(target.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(std::fs::metadata(&target).unwrap().ino(), before.ino());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"SYNTHETIC-UNVALIDATED-JOURNAL"
        );
        let error = crate::domain::merge_archive::read(repo.root(), &generation).unwrap_err();
        assert!(error.is::<serde_json::Error>(), "{error:#}");
    }
}
