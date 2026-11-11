//! The copy of its own executable a daemon starts its subprocesses from (see
//! [`crate::infra::own_image`]). On Linux the kernel keeps a link to the running image, but where
//! the install directory is a mount shared from a host (a VM's or container's virtiofs or bind
//! mount), an update renaming a new binary over it leaves that link unable to start: the copy
//! on this machine's own disk still can.
//!
//! One copy per build, under `<rc dir>/images/<build>/`, reused by every daemon of that build and
//! removed by a later daemon once none holds it. A copy made per start would grow the disk with
//! every restart. Taking a copy and removing copies happen only under one lock beside the build
//! directories, which no removal deletes: if a daemon could take its build's copy between another
//! build judging that copy unheld and deleting it, it would run on with every subprocess pointing
//! at a path that is gone.
//!
//! The copy is something agit runs, so it is made the way an install is: in an owner-only
//! directory, written to a temporary name and renamed into place, and checked against the running
//! image before it is reused, never run when it differs.

use anyhow::ensure;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

const IN_USE: &str = "in-use.lock";
/// Held while a daemon takes its build's copy or removes others; see the module doc.
const IMAGES_LOCK: &str = "images.lock";

#[cfg(windows)]
const NAME: &str = "agit.exe";
#[cfg(not(windows))]
const NAME: &str = "agit";

/// A copy of the running image, held for as long as the value lives: no other daemon removes it
/// meanwhile.
pub(crate) struct Image {
    pub path: PathBuf,
    _held: File,
}

/// Make or reuse this build's copy of the running executable, and remove the copies of builds no
/// daemon holds any more.
pub(crate) fn prepare() -> crate::Result<Image> {
    // The image this process runs, not the installed path: an update may have replaced that
    // already.
    #[cfg(target_os = "linux")]
    let running = PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let running = std::env::current_exe()
        .map_err(|error| anyhow::anyhow!("the running executable is unknown: {error}"))?;
    prepare_in(
        &super::rc_dir()?.join("images"),
        crate::infra::own_image::BUILD,
        &running,
    )
}

fn prepare_in(root: &Path, build: &str, running: &Path) -> crate::Result<Image> {
    crate::infra::config::create_state_dir(root)?;
    #[cfg(test)]
    tests::seam(tests::Seam::BeforeImagesLock);
    let images = crate::infra::config::state_file_options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(IMAGES_LOCK))?;
    fs2::FileExt::lock_exclusive(&images)?;
    let own = root.join(build);
    crate::infra::config::create_state_dir(&own)?;
    ensure!(
        !std::fs::symlink_metadata(&own)?.file_type().is_symlink(),
        "the image directory is a link"
    );
    let held = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(own.join(IN_USE))?;
    fs2::FileExt::lock_shared(&held)?;
    #[cfg(test)]
    tests::seam(tests::Seam::CopyHeld);
    let path = own.join(NAME);
    if !same_contents(&path, running)? {
        copy_into(&own, running, &path)?;
    }
    remove_unheld(root, build);
    drop(images);
    Ok(Image { path, _held: held })
}

/// Whether `copy` is a regular file holding exactly what `running` holds.
fn same_contents(copy: &Path, running: &Path) -> crate::Result<bool> {
    let Ok(metadata) = std::fs::symlink_metadata(copy) else {
        return Ok(false);
    };
    if !metadata.file_type().is_file() || metadata.len() != open_running(running)?.metadata()?.len()
    {
        return Ok(false);
    }
    Ok(digest(open_no_follow(copy)?)? == digest(open_running(running)?)?)
}

fn digest(mut file: File) -> crate::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

/// The running image, to read. Linux names it through the kernel's link to it, which has to be
/// followed; anywhere else a link here is refused.
fn open_running(running: &Path) -> std::io::Result<File> {
    #[cfg(target_os = "linux")]
    if running == Path::new("/proc/self/exe") {
        return File::open(running);
    }
    open_no_follow(running)
}

fn open_no_follow(path: &Path) -> std::io::Result<File> {
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// Copy `running` to `path` through a temporary file in `directory`, replacing whatever was there.
fn copy_into(directory: &Path, running: &Path, path: &Path) -> crate::Result<()> {
    let mut source = open_running(running)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".image-")
        .tempfile_in(directory)?;
    std::io::copy(&mut source, temporary.as_file_mut())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    temporary.as_file().sync_all()?;
    let _ = std::fs::remove_file(path);
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Remove every other build's copy that no running daemon holds. Called with the images lock
/// held, so no daemon takes a copy between the judgement and the removal.
fn remove_unheld(root: &Path, own: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        if entry.file_name() == own {
            continue;
        }
        let directory = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let (unheld, lock) = match File::open(directory.join(IN_USE)) {
            Ok(lock) => (fs2::FileExt::try_lock_exclusive(&lock).is_ok(), Some(lock)),
            // A copy left without its lock was never held by a running daemon.
            Err(error) => (error.kind() == std::io::ErrorKind::NotFound, None),
        };
        if unheld {
            #[cfg(test)]
            tests::seam(tests::Seam::BeforeRemoval(&directory));
            // Windows doesn't remove a file that is open; the images lock already keeps every
            // daemon out until the removal is done.
            #[cfg(windows)]
            drop(lock);
            let _ = std::fs::remove_dir_all(&directory);
        }
        #[cfg(not(windows))]
        drop(lock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Where a test may step into [`prepare_in`] and [`remove_unheld`] on the calling thread.
    pub(super) enum Seam<'a> {
        BeforeImagesLock,
        /// The daemon holds its build's copy, before it checks or writes it.
        CopyHeld,
        BeforeRemoval(&'a Path),
    }

    type Hook = Box<dyn FnMut(&Seam<'_>)>;

    thread_local! {
        static SEAM: RefCell<Option<Hook>> = RefCell::new(None);
    }

    pub(super) fn seam(point: Seam<'_>) {
        SEAM.with(|seam| {
            if let Some(hook) = seam.borrow_mut().as_mut() {
                hook(&point);
            }
        });
    }

    fn images_lock_held(root: &Path) -> bool {
        let lock = File::open(root.join(IMAGES_LOCK)).unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).is_err()
    }

    /// A daemon that starts while another build removes its build's old copy ends up holding a
    /// copy that exists. If it could take the copy between the other build judging it unheld and
    /// deleting it, it would run on with every subprocess pointing at a path that is gone. The
    /// removing build is stopped right before it deletes the old copy and the daemon starts then;
    /// both sides are checked to hold the images lock where they act, so a side that skips it
    /// fails here rather than only when the scheduler happens to interleave them.
    #[test]
    fn a_copy_being_taken_is_never_removed_by_another_build() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("images");
        let (running_a, running_b) = (directory.path().join("a"), directory.path().join("b"));
        std::fs::write(&running_a, b"build a").unwrap();
        std::fs::write(&running_b, b"build b").unwrap();
        let stale = root.join("build-a");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join(IN_USE), b"").unwrap();
        std::fs::write(stale.join(NAME), b"old").unwrap();

        let started = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (slot, hook_root, hook_running) = (started.clone(), root.clone(), running_a.clone());
        SEAM.with(|seam| {
            *seam.borrow_mut() = Some(Box::new(move |point: &Seam<'_>| {
                let Seam::BeforeRemoval(removing) = point else {
                    return;
                };
                assert!(removing.ends_with("build-a"));
                assert!(
                    images_lock_held(&hook_root),
                    "a copy is judged and removed under the images lock"
                );
                let (root, running) = (hook_root.clone(), hook_running.clone());
                let (reached, waiting) = std::sync::mpsc::channel();
                *slot.lock().unwrap() = Some(std::thread::spawn(move || {
                    let lock_root = root.clone();
                    SEAM.with(|seam| {
                        *seam.borrow_mut() = Some(Box::new(move |point: &Seam<'_>| match point {
                            Seam::BeforeImagesLock => reached.send(()).unwrap(),
                            Seam::CopyHeld => assert!(
                                images_lock_held(&lock_root),
                                "a copy is taken under the images lock"
                            ),
                            Seam::BeforeRemoval(_) => {}
                        }));
                    });
                    prepare_in(&root, "build-a", &running)
                }));
                waiting.recv().unwrap();
            }));
        });
        let b = prepare_in(&root, "build-b", &running_b).unwrap();
        SEAM.with(|seam| *seam.borrow_mut() = None);
        let a = started
            .lock()
            .unwrap()
            .take()
            .expect("build A started while build B removed its old copy")
            .join()
            .unwrap()
            .unwrap();

        assert_eq!(std::fs::read(&a.path).unwrap(), b"build a");
        let held = File::open(root.join("build-a").join(IN_USE)).unwrap();
        assert!(
            fs2::FileExt::try_lock_exclusive(&held).is_err(),
            "build A holds the copy it runs from"
        );
        assert_eq!(std::fs::read(&b.path).unwrap(), b"build b");
    }

    /// One copy per build, reused across starts, made again when it no longer matches the running
    /// image, and an old build's copy removed only once no daemon holds it. A copy per start grows
    /// the disk with every restart; reusing a changed copy runs something that is not this build;
    /// removing a held copy pulls the image out from under a running daemon's subprocesses.
    #[test]
    fn one_copy_per_build_is_reused_checked_and_removed_when_unheld() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            let root = crate::rc::rc_dir().unwrap().join("images");
            let held_old = root.join("held-build");
            let unheld_old = root.join("unheld-build");
            for directory in [&held_old, &unheld_old] {
                std::fs::create_dir_all(directory).unwrap();
                std::fs::write(directory.join(IN_USE), b"").unwrap();
                std::fs::write(directory.join(NAME), b"old").unwrap();
            }
            let holder = File::open(held_old.join(IN_USE)).unwrap();
            fs2::FileExt::lock_shared(&holder).unwrap();

            let first = prepare().unwrap();
            let running = std::fs::read(std::env::current_exe().unwrap()).unwrap();
            assert_eq!(std::fs::read(&first.path).unwrap(), running);
            let made = std::fs::metadata(&first.path).unwrap().modified().unwrap();
            drop(first);
            let again = prepare().unwrap();
            assert_eq!(
                std::fs::metadata(&again.path).unwrap().modified().unwrap(),
                made,
                "a matching copy is reused"
            );
            drop(again);
            // Same size, one byte different: only the contents tell it apart.
            let mut changed = running.clone();
            *changed.last_mut().unwrap() ^= 1;
            std::fs::write(
                root.join(crate::infra::own_image::BUILD).join(NAME),
                changed,
            )
            .unwrap();
            let remade = prepare().unwrap();
            assert_eq!(
                std::fs::read(&remade.path).unwrap(),
                running,
                "a changed copy is remade"
            );

            assert!(held_old.exists(), "a held copy stays");
            assert!(!unheld_old.exists(), "an unheld copy is removed");
            let builds = std::fs::read_dir(&root)
                .unwrap()
                .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
                .count();
            assert_eq!(builds, 2);
        });
    }

    /// The copy is what a subprocess starts from once an update has replaced the installed file:
    /// with the file this build was started from gone, the copy still holds this build and still
    /// starts. On Linux it is taken from the running image itself, so even an install that
    /// replaced the file before the daemon took its copy can't give it another build.
    #[cfg(unix)]
    #[test]
    fn a_pinned_copy_still_starts_once_the_installed_file_is_gone() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let installed = home.path().join("installed-agit");
        std::fs::write(&installed, b"#!/bin/sh\necho copy-of-this-build\n").unwrap();
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = home.path().join("images");
        let image = prepare_in(&root, "this-build", &installed).unwrap();
        std::fs::remove_file(&installed).unwrap();
        let output = std::process::Command::new(&image.path).output().unwrap();
        assert_eq!(output.stdout, b"copy-of-this-build\n");
        #[cfg(target_os = "linux")]
        {
            let running = std::fs::read("/proc/self/exe").unwrap();
            let taken =
                prepare_in(&root, "running", std::path::Path::new("/proc/self/exe")).unwrap();
            assert_eq!(std::fs::read(&taken.path).unwrap(), running);
        }
    }
}
