use crate::domain::mergetx::{LOCK_FILE, Tx};
use crate::domain::refs::{self, Tail};
use crate::domain::repo::Repo;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::path::Path;

const MAX_TRANSACTION_BYTES: u64 = 1024 * 1024;
const MAX_REFERENCE_BYTES: usize = 4096;
const MAX_DISPLAY_TARGET_BYTES: usize = 512;

pub(super) fn inspect(root: &Path, slug: &str) -> Option<String> {
    let repo = Repo::at(root).local_objects_only();
    let directory = match repo.common_dir() {
        Ok(directory) => directory,
        Err(_) => {
            return Some("merge transaction: unavailable — Git directory cannot be read".into());
        }
    };
    let tx = match read(&directory.join(LOCK_FILE)) {
        Ok(None) => return None,
        Ok(Some(tx)) => tx,
        Err(reason) => return Some(format!("merge transaction: {reason}")),
    };
    if tx.target.len() > MAX_REFERENCE_BYTES || tx.source.len() > MAX_REFERENCE_BYTES {
        return Some(
            "merge transaction: unavailable — ref text exceeds the inspection budget".into(),
        );
    }
    if !oid(&tx.target_head) || !oid(&tx.source_head) || (!tx.base.is_empty() && !oid(&tx.base)) {
        return Some("merge transaction: invalid — frozen commit identity is malformed".into());
    }
    let target = format!("refs/heads/{}", tx.target);
    if !matches!(
        repo.git_status(&["check-ref-format", &target]),
        Ok((Some(0), _, _))
    ) {
        return Some("merge transaction: invalid — target branch name is malformed".into());
    }
    let status = match repo.git_status(&["show-ref", "--verify", "--quiet", &target]) {
        Ok((Some(1), _, stderr)) if stderr.is_empty() => "target missing".to_owned(),
        Ok((Some(0), _, stderr)) if stderr.is_empty() => {
            let head = match repo.git(&["rev-parse", "--verify", &format!("{target}^{{commit}}")]) {
                Ok(head) => head,
                Err(_) => {
                    return Some(
                        "merge transaction: unavailable — target commit cannot be read".into(),
                    );
                }
            };
            if head != tx.target_head {
                "target moved".to_owned()
            } else {
                source_status(&repo, slug, &tx)
            }
        }
        _ => return Some("merge transaction: unavailable — target ref cannot be inspected".into()),
    };
    if tx.target.len() > MAX_DISPLAY_TARGET_BYTES {
        return Some(format!(
            "merge transaction: {status}; inspect with `agit merge --status` and the explicit `--into` target"
        ));
    }
    Some(format!(
        "merge transaction: {status}; inspect with `agit merge --into {} --status`",
        crate::commands::import::selection_arg(&format!("{slug}@{}", tx.target))
    ))
}

fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn source_status(repo: &Repo, slug: &str, tx: &Tx) -> String {
    let Some(source_repo) = tx.source_repo.as_deref() else {
        return "open — target matches; source identity is unavailable".into();
    };
    if super::parse_repo(source_repo).is_err() {
        return "invalid — source repository identity is malformed".into();
    }
    if source_repo != slug {
        return "open — target matches; source repository is outside this inspection".into();
    }
    let spec = match crate::commands::merge::transaction_source_spec(tx) {
        Ok(spec) => spec,
        Err(_) => return "open — target matches; source selector cannot be inspected".into(),
    };
    if !matches!(spec.tail, Tail::None | Tail::Path(_)) {
        return match repo.git(&["cat-file", "-t", &tx.source_head]) {
            Ok(kind) if kind == "commit" => {
                "open — target matches; historical source selector was not re-evaluated".into()
            }
            _ => "unavailable — frozen source commit cannot be read".into(),
        };
    }
    match refs::resolve(repo, &spec) {
        Ok(source) if source.sha == tx.source_head => "open — target and source match".into(),
        Ok(_) => "source moved".into(),
        Err(_) => "source unavailable — source ref cannot be resolved locally".into(),
    }
}

fn read(path: &Path) -> Result<Option<Tx>, &'static str> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(metadata) if !metadata.is_file() || metadata.is_symlink() => {
            return Err("unavailable — transaction record is not a regular file");
        }
        Err(_) => return Err("unavailable — transaction record cannot be inspected"),
        Ok(_) => {}
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|_| "unavailable — transaction record cannot be opened")?;
    let metadata = file
        .metadata()
        .map_err(|_| "unavailable — transaction record cannot be inspected")?;
    if !metadata.is_file() || metadata.is_symlink() {
        return Err("unavailable — transaction record is not a regular file");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("unavailable — transaction record is a reparse point");
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_TRANSACTION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "unavailable — transaction record cannot be read")?;
    if bytes.len() as u64 > MAX_TRANSACTION_BYTES {
        return Err("unavailable — transaction exceeds the inspection budget");
    }
    serde_json::from_slice(&bytes).map(Some).map_err(
        |_| "invalid — transaction data cannot be parsed; preserve the record for recovery",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::meta::{self, Meta};

    fn fixture() -> (tempfile::TempDir, Repo, Tx) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("Synthetic transaction base").unwrap();
        repo.git(&["branch", "work"]).unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let tx = Tx {
            mode: None,
            exploration: None,
            generation: None,
            target: "work".into(),
            source: head.clone(),
            source_repo: Some("alice/chosen".into()),
            source_branch: None,
            base: head.clone(),
            target_head: head.clone(),
            source_head: head,
            picked: Vec::new(),
            summary: None,
        };
        (dir, repo, tx)
    }

    fn status(repo: &Repo, tx: &Tx) -> String {
        crate::domain::mergetx::lock(repo.root(), tx).unwrap();
        let path = repo.common_dir().unwrap().join(LOCK_FILE);
        let before = std::fs::read(&path).unwrap();
        let status = inspect(repo.root(), "alice/chosen").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), before);
        status
    }

    #[test]
    fn absent_record_is_not_a_stale_lock() {
        let (_dir, repo, _) = fixture();
        assert!(inspect(repo.root(), "alice/chosen").is_none());
        std::fs::write(repo.common_dir().unwrap().join("index.lock"), "").unwrap();
        assert!(inspect(repo.root(), "alice/chosen").is_none());
    }

    #[test]
    fn target_and_source_changes_are_classified_separately() {
        let (_dir, repo, mut tx) = fixture();
        repo.git(&["branch", "source"]).unwrap();
        tx.source = "source".into();
        tx.source_branch = Some("source".into());
        assert!(status(&repo, &tx).contains("target and source match"));
        std::fs::write(
            repo.root().join("shared.txt"),
            "Synthetic source advancement",
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("Synthetic source advancement").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["update-ref", "refs/heads/source", &head])
            .unwrap();
        assert!(status(&repo, &tx).contains("source moved"));
        repo.git(&["branch", "-D", "source"]).unwrap();
        assert!(status(&repo, &tx).contains("source unavailable"));
        repo.git(&["branch", "-D", "work"]).unwrap();
        assert!(status(&repo, &tx).contains("target missing"));
    }

    #[test]
    fn source_scope_and_history_do_not_borrow_a_working_directory() {
        let (_dir, repo, mut tx) = fixture();
        tx.source_repo = Some("bob/unreadable".into());
        assert!(status(&repo, &tx).contains("outside this inspection"));
        tx.source_repo = None;
        assert!(status(&repo, &tx).contains("source identity is unavailable"));
        tx.source_repo = Some("alice/chosen".into());
        tx.source = "source#1".into();
        tx.source_branch = Some("source".into());
        assert!(status(&repo, &tx).contains("historical source selector was not re-evaluated"));
    }

    #[test]
    fn malformed_identity_is_not_printed_or_followed() {
        let (_dir, repo, mut tx) = fixture();
        tx.target = "private-marker\nwork".into();
        let report = status(&repo, &tx);
        assert!(report.contains("target branch name is malformed"));
        assert!(!report.contains("private-marker"));
        tx.target = "work".into();
        tx.target_head = "private-marker".into();
        let report = status(&repo, &tx);
        assert!(report.contains("frozen commit identity is malformed"));
        assert!(!report.contains("private-marker"));
    }

    #[test]
    fn invalid_and_oversized_records_remain_intact() {
        let (_dir, repo, _) = fixture();
        let path = repo.common_dir().unwrap().join(LOCK_FILE);
        std::fs::write(&path, "{private-marker}").unwrap();
        let before = std::fs::read(&path).unwrap();
        let report = inspect(repo.root(), "alice/chosen").unwrap();
        assert!(report.contains("invalid — transaction data cannot be parsed"));
        assert!(!report.contains("private-marker"));
        assert!(!report.contains("--status"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::write(&path, vec![b' '; MAX_TRANSACTION_BYTES as usize + 1]).unwrap();
        assert!(
            inspect(repo.root(), "alice/chosen")
                .unwrap()
                .contains("exceeds the inspection budget")
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            MAX_TRANSACTION_BYTES + 1
        );
    }

    #[cfg(unix)]
    #[test]
    fn special_transaction_paths_are_never_followed_or_blocked_on() {
        use std::os::unix::ffi::OsStrExt as _;
        let (dir, repo, _) = fixture();
        let path = repo.common_dir().unwrap().join(LOCK_FILE);
        let target = dir.path().join("private-target");
        std::fs::write(&target, "private-marker").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(read(&path).unwrap_err().contains("not a regular file"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "private-marker");
        std::fs::remove_file(&path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read(&path).unwrap_err().contains("not a regular file"));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(read(&path).unwrap_err().contains("not a regular file"));
    }

    #[test]
    fn linked_worktrees_inspect_the_common_transaction() {
        let (_dir, repo, tx) = fixture();
        let parent = tempfile::tempdir().unwrap();
        let worktree = parent.path().join("linked");
        repo.git(&["worktree", "add", "--detach", worktree.to_str().unwrap()])
            .unwrap();
        crate::domain::mergetx::lock(repo.root(), &tx).unwrap();
        let report = inspect(&worktree, "alice/chosen").unwrap();
        assert!(report.contains("target and source match"));
    }
}
