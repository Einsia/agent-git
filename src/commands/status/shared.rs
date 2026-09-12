//! Shared-file inspection compares immutable Git entries with raw local bytes, without filters.

use crate::adapter::native_snapshot::{Limits, read_file_bytes};
use crate::domain::repo::{Repo, Worktree};
use anyhow::{Context, ensure};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

mod git;

const MAX_OUTPUT: usize = 256 * 1024;
const MAX_RECORDS: usize = 1024;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_CHECKOUT_PATH_BYTES: usize = 4096;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_CHECKOUTS: usize = 32;
const MAX_ROWS: usize = 128;
const PATHS: [&str; 3] = [
    ":(top,literal)AGENTS.md",
    ":(top,literal)memory",
    ":(top,literal)skills",
];

type Entries = BTreeMap<String, Vec<Entry>>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    mode: String,
    oid: String,
    stage: u8,
}

#[derive(PartialEq, Eq)]
struct Snapshot {
    head: Vec<u8>,
    index: Vec<u8>,
    untracked: Vec<u8>,
    staged: Vec<u8>,
    common: PathBuf,
    toplevel: PathBuf,
    git_dir: PathBuf,
    format: String,
}

#[derive(serde::Serialize)]
pub(super) struct Page {
    pub items: Vec<Change>,
    pub incomplete: bool,
}

#[derive(serde::Serialize)]
pub(super) struct Change {
    repo: String,
    branch: Option<String>,
    checkout: Option<PathBuf>,
    path: Option<String>,
    staged: String,
    local_bytes: String,
}

impl Page {
    pub fn rows(&self) -> Vec<Vec<String>> {
        self.items
            .iter()
            .map(|change| {
                let target = if change.checkout.is_some() {
                    format!(
                        "{}@{}",
                        change.repo,
                        change.branch.as_deref().unwrap_or("(detached)")
                    )
                } else {
                    change.repo.clone()
                };
                vec![
                    cell(&target),
                    change
                        .path
                        .as_deref()
                        .map(cell)
                        .unwrap_or_else(|| "—".into()),
                    change.staged.clone(),
                    change.local_bytes.clone(),
                ]
            })
            .collect()
    }
}

struct Budget {
    git: git::Git,
    records: usize,
    bytes: usize,
    checkouts: usize,
}

pub(super) fn inspect(agents: &[(String, String, PathBuf)]) -> Page {
    let mut page = Page {
        items: Vec::new(),
        incomplete: false,
    };
    let git = match git::Git::new() {
        Ok(git) => git,
        Err(_) => {
            page.incomplete = !agents.is_empty();
            return page;
        }
    };
    let mut budget = Budget {
        git,
        records: MAX_RECORDS,
        bytes: MAX_TOTAL_BYTES,
        checkouts: MAX_CHECKOUTS,
    };
    for (owner, name, path) in agents {
        if budget.checkouts == 0 || page.items.len() >= MAX_ROWS {
            page.incomplete = true;
            break;
        }
        let slug = format!("{owner}/{name}");
        let primary = Repo::at(path).exact_root_inspection();
        let result = (|| -> crate::Result<Vec<Change>> {
            let common = budget.git.common_dir(&primary)?.canonicalize()?;
            let before = budget.git.worktrees(&primary)?;
            ensure!(
                before
                    .iter()
                    .all(|checkout| checkout.path.to_str().is_some()),
                "checkout path cannot be represented without losing identity"
            );
            let mut rows = Vec::new();
            let mut checkout_paths = BTreeSet::new();
            for checkout in &before {
                if budget.checkouts == 0 || rows.len() + page.items.len() >= MAX_ROWS {
                    page.incomplete = true;
                    break;
                }
                budget.checkouts -= 1;
                if let Ok(path) = checkout.path.canonicalize() {
                    ensure!(
                        checkout_paths.insert(path),
                        "multiple registrations identify the same checkout"
                    );
                }
                match inspect_checkout(checkout, &common, &mut budget) {
                    Ok(changes) => {
                        for change in changes {
                            page.incomplete |= change.2.starts_with("unavailable:");
                            if rows.len() + page.items.len() >= MAX_ROWS {
                                page.incomplete = true;
                                break;
                            }
                            rows.push(Change {
                                repo: slug.clone(),
                                branch: checkout.branch.clone(),
                                checkout: Some(checkout.path.clone()),
                                path: Some(change.0),
                                staged: change.1,
                                local_bytes: change.2,
                            });
                        }
                    }
                    Err(_) => {
                        page.incomplete = true;
                        rows.push(unavailable(&slug, Some(checkout)));
                    }
                }
            }
            ensure!(
                before == budget.git.worktrees(&primary)?,
                "checkout registrations changed"
            );
            ensure!(
                common == budget.git.common_dir(&primary)?.canonicalize()?,
                "repository storage changed"
            );
            Ok(rows)
        })();
        match result {
            Ok(rows) => page.items.extend(rows),
            Err(_) => {
                page.incomplete = true;
                page.items.push(unavailable(&slug, None));
            }
        }
    }
    page
}

fn unavailable(slug: &str, checkout: Option<&Worktree>) -> Change {
    Change {
        repo: slug.to_owned(),
        branch: checkout.and_then(|checkout| checkout.branch.clone()),
        checkout: checkout.map(|checkout| checkout.path.clone()),
        path: None,
        staged: "unavailable".into(),
        local_bytes: "unavailable: incomplete or changing evidence".into(),
    }
}

fn cell(value: &str) -> String {
    let mut chars = value.chars();
    let mut output = String::new();
    for character in chars.by_ref().take(160) {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    if chars.next().is_some() {
        output.push('…');
    }
    output
}

fn output(git: &git::Git, repo: &Repo, args: &[&str]) -> crate::Result<Vec<u8>> {
    let output = git.output(repo, args, MAX_OUTPUT)?;
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "shared-file metadata is unavailable"
    );
    Ok(output.stdout)
}

fn paths_output(git: &git::Git, repo: &Repo, args: &[&str]) -> crate::Result<Vec<u8>> {
    let mut command = args.to_vec();
    command.push("--");
    command.extend(PATHS);
    output(git, repo, &command)
}

fn snapshot(git: &git::Git, repo: &Repo, checkout: &Worktree) -> crate::Result<Snapshot> {
    let (toplevel, git_dir) = git.checkout_paths(repo)?;
    let toplevel = toplevel.canonicalize()?;
    let git_dir = git_dir.canonicalize()?;
    ensure!(toplevel == repo.root(), "Git selected a different checkout");
    let common = git.common_dir(repo)?.canonicalize()?;
    if checkout.primary {
        ensure!(
            git_dir == common,
            "Git selected a linked checkout as primary"
        );
    } else {
        ensure!(
            git_dir != common && git_dir.parent() == Some(common.join("worktrees").as_path()),
            "Git selected a different linked checkout registration"
        );
        // The common object store identifies a repository, not its selected checkout.
        // The administrative backlink must name this registration even when pointers drift.
        let limits = Limits {
            bytes: MAX_CHECKOUT_PATH_BYTES,
            working_bytes: MAX_CHECKOUT_PATH_BYTES + 1,
            ..Limits::default()
        };
        read_file_bytes(&repo.root().join(".git"), limits)?;
        let backlink = read_file_bytes(&git_dir.join("gitdir"), limits)?;
        let backlink = std::str::from_utf8(
            backlink
                .strip_suffix(b"\r\n")
                .or_else(|| backlink.strip_suffix(b"\n"))
                .context("checkout backlink is incomplete")?,
        )?;
        let backlink = Path::new(backlink);
        ensure!(
            backlink.is_absolute()
                && backlink.file_name().is_some_and(|name| name == ".git")
                && backlink
                    .parent()
                    .context("checkout backlink has no parent")?
                    .canonicalize()?
                    == repo.root(),
            "checkout backlink names another registration"
        );
    }
    let symbolic = git.output(repo, &["symbolic-ref", "--quiet", "HEAD"], 4096)?;
    let reference = match symbolic.status.code() {
        Some(0) if symbolic.stderr.is_empty() => Some(
            std::str::from_utf8(&symbolic.stdout)?
                .strip_suffix('\n')
                .context("checkout branch response is incomplete")?,
        ),
        Some(1) if symbolic.stdout.is_empty() && symbolic.stderr.is_empty() => None,
        _ => anyhow::bail!("checkout branch is unavailable"),
    };
    let branch = reference
        .map(|reference| {
            reference
                .strip_prefix("refs/heads/")
                .filter(|branch| !branch.is_empty())
                .context("checkout HEAD does not name a local branch")
        })
        .transpose()?;
    ensure!(
        branch == checkout.branch.as_deref(),
        "checkout branch differs from its registration"
    );
    let head = output(git, repo, &["rev-parse", "--verify", "--quiet", "HEAD"]);
    let head = match head {
        Ok(bytes) => bytes,
        Err(_) => {
            let reference = reference.context("unborn HEAD is not a branch")?;
            let missing =
                git.output(repo, &["show-ref", "--verify", "--quiet", reference], 4096)?;
            ensure!(
                missing.status.code() == Some(1) && missing.stderr.is_empty(),
                "HEAD object is unavailable"
            );
            Vec::new()
        }
    };
    let format = String::from_utf8(output(git, repo, &["rev-parse", "--show-object-format"])?)?
        .trim()
        .to_owned();
    ensure!(
        matches!(format.as_str(), "sha1" | "sha256"),
        "unsupported object format"
    );
    let expected_head = checkout
        .head
        .as_deref()
        .context("registration has no HEAD")?;
    if head.is_empty() {
        ensure!(
            valid_oid(expected_head, &format) && expected_head.bytes().all(|byte| byte == b'0'),
            "unborn HEAD differs from its registration"
        );
    } else {
        let observed_head = std::str::from_utf8(&head)?.trim_end_matches('\n');
        ensure!(
            valid_oid(observed_head, &format) && observed_head == expected_head,
            "checkout HEAD differs from its registration"
        );
    }
    let mut staged_args = vec![
        "diff",
        "--cached",
        "--name-status",
        "-z",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
    ];
    if !head.is_empty() {
        staged_args.push(std::str::from_utf8(&head)?.trim_end_matches('\n'));
    }
    let staged = paths_output(git, repo, &staged_args)?;
    Ok(Snapshot {
        head,
        staged,
        common,
        toplevel,
        git_dir,
        format,
        index: paths_output(git, repo, &["ls-files", "--stage", "-z"])?,
        untracked: paths_output(
            git,
            repo,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?,
    })
}

fn valid_oid(oid: &str, format: &str) -> bool {
    oid.len() == if format == "sha1" { 40 } else { 64 }
        && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn eligible(path: &str) -> bool {
    (matches!(path, "AGENTS.md" | "memory" | "skills")
        || path.starts_with("memory/")
        || path.starts_with("skills/"))
        && !path.contains(['\\', ':', '\0'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && !matches!(part, "." | ".." | ".git"))
}

fn records<'a>(bytes: &'a [u8], budget: &mut Budget) -> crate::Result<Vec<&'a [u8]>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(bytes.last() == Some(&0), "incomplete path framing");
    let mut output = Vec::new();
    for record in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        budget.records = budget
            .records
            .checked_sub(1)
            .context("shared-file record budget exhausted")?;
        ensure!(!record.is_empty(), "empty path record");
        output.push(record);
    }
    Ok(output)
}

fn entries(bytes: &[u8], format: &str, budget: &mut Budget) -> crate::Result<Entries> {
    let mut entries = Entries::new();
    for record in records(bytes, budget)? {
        let (header, path) = std::str::from_utf8(record)?
            .split_once('\t')
            .context("invalid Git entry")?;
        ensure!(eligible(path), "unexpected shared-file path");
        let fields: Vec<_> = header.split(' ').collect();
        ensure!(fields.len() == 3, "invalid Git entry header");
        let oid = fields[1];
        let stage = fields[2].parse::<u8>()?;
        ensure!(stage <= 3 && valid_oid(oid, format), "invalid index entry");
        ensure!(
            matches!(fields[0], "100644" | "100755" | "120000" | "160000"),
            "unsupported entry mode"
        );
        let values = entries.entry(path.to_owned()).or_default();
        ensure!(
            !values.iter().any(|entry: &Entry| entry.stage == stage),
            "duplicate index stage"
        );
        values.push(Entry {
            mode: fields[0].to_owned(),
            oid: oid.to_owned(),
            stage,
        });
    }
    Ok(entries)
}

fn staged_changes(bytes: &[u8], budget: &mut Budget) -> crate::Result<BTreeMap<String, String>> {
    let fields = records(bytes, budget)?;
    ensure!(fields.len().is_multiple_of(2), "incomplete staged change");
    let mut changes = BTreeMap::new();
    for pair in fields.chunks_exact(2) {
        let path = std::str::from_utf8(pair[1])?;
        ensure!(eligible(path), "unexpected staged path");
        let state = match pair[0] {
            b"A" => "added",
            b"M" => "modified",
            b"D" => "deleted",
            b"T" => "type changed",
            b"U" => "conflicted",
            _ => anyhow::bail!("unsupported staged status"),
        };
        ensure!(
            changes.insert(path.to_owned(), state.to_owned()).is_none(),
            "duplicate staged path"
        );
    }
    Ok(changes)
}

fn inspect_checkout(
    checkout: &Worktree,
    common: &Path,
    budget: &mut Budget,
) -> crate::Result<Vec<(String, String, String)>> {
    let repo = Repo::at(checkout.path.canonicalize()?).exact_root_inspection();
    let before = snapshot(&budget.git, &repo, checkout)?;
    ensure!(
        before.common == common,
        "checkout belongs to another repository"
    );
    let staged_changes = staged_changes(&before.staged, budget)?;
    let index = entries(&before.index, &before.format, budget)?;
    let mut untracked = BTreeSet::new();
    for path in records(&before.untracked, budget)? {
        let path = std::str::from_utf8(path)?;
        ensure!(eligible(path), "unexpected untracked path");
        ensure!(
            untracked.insert(path.to_owned()),
            "duplicate untracked path"
        );
    }
    let paths: BTreeSet<_> = staged_changes
        .keys()
        .chain(index.keys())
        .chain(untracked.iter())
        .cloned()
        .collect();
    let mut rows = Vec::new();
    for path in paths {
        let staged = index.get(&path);
        let state = if staged.is_some_and(|entries| entries.iter().any(|entry| entry.stage != 0)) {
            "conflicted"
        } else {
            staged_changes
                .get(&path)
                .map_or("unchanged", String::as_str)
        };
        let local = if state == "conflicted" {
            "unavailable: unmerged index".to_owned()
        } else {
            local_state(
                repo.root(),
                &path,
                staged.and_then(|entries| entries.first()),
                &before.format,
                budget,
            )
            .unwrap_or_else(|_| {
                "unavailable: unreadable, linked, changing or oversized file".into()
            })
        };
        if state != "unchanged" || local != "unchanged" {
            rows.push((path, state.to_owned(), local));
        }
    }
    ensure!(
        before == snapshot(&budget.git, &repo, checkout)?,
        "shared-file metadata changed during inspection"
    );
    Ok(rows)
}

fn local_state(
    root: &Path,
    path: &str,
    indexed: Option<&Entry>,
    format: &str,
    budget: &mut Budget,
) -> crate::Result<String> {
    ensure!(
        !matches!(path, "memory" | "skills"),
        "shared root is not a directory"
    );
    if indexed.is_some_and(|entry| !matches!(entry.mode.as_str(), "100644" | "100755")) {
        anyhow::bail!("index entry is not a regular file");
    }
    let absolute = root.join(path);
    let mut directory = root.to_path_buf();
    let mut parents = vec![Directory::open(root)?];
    for component in Path::new(path)
        .parent()
        .unwrap_or(Path::new(""))
        .components()
    {
        directory.push(component);
        match std::fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                for parent in &parents {
                    parent.verify()?;
                }
                return Ok(if indexed.is_some() {
                    "deleted"
                } else {
                    "absent"
                }
                .into());
            }
            Err(error) => return Err(error.into()),
            Ok(_) => parents.push(Directory::open(&directory)?),
        }
    }
    let metadata = match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            for parent in &parents {
                parent.verify()?;
            }
            return Ok(if indexed.is_some() {
                "deleted"
            } else {
                "absent"
            }
            .into());
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "shared path is not a regular file"
    );
    let length = usize::try_from(metadata.len()).context("shared file is oversized")?;
    ensure!(length <= MAX_FILE_BYTES, "shared file is oversized");
    budget.bytes = budget
        .bytes
        .checked_sub(length + 1)
        .context("shared-file byte budget exhausted")?;
    let bytes = read_file_bytes(
        &absolute,
        Limits {
            bytes: length,
            working_bytes: length + 1,
            ..Limits::default()
        },
    )?;
    for parent in &parents {
        parent.verify()?;
    }
    #[cfg(unix)]
    ensure!(
        same_unix_metadata(&metadata, &std::fs::symlink_metadata(&absolute)?),
        "shared file metadata changed"
    );
    let Some(indexed) = indexed else {
        return Ok("untracked".into());
    };
    let oid = match format {
        "sha1" => blob_digest::<sha1::Sha1>(&bytes),
        "sha256" => blob_digest::<sha2::Sha256>(&bytes),
        _ => unreachable!(),
    };
    if oid != indexed.oid {
        return Ok("modified (raw bytes)".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if (metadata.permissions().mode() & 0o111 != 0) != (indexed.mode == "100755") {
            return Ok("mode changed".into());
        }
    }
    Ok("unchanged".into())
}

struct Directory {
    path: PathBuf,
    file: std::fs::File,
    #[cfg(unix)]
    metadata: std::fs::Metadata,
    #[cfg(windows)]
    metadata: WindowsDirectoryMetadata,
}

#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
struct WindowsDirectoryMetadata {
    created: i64,
    written: i64,
    changed: i64,
    attributes: u32,
}

#[cfg(windows)]
fn windows_directory_metadata(file: &std::fs::File) -> crate::Result<WindowsDirectoryMetadata> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FileBasicInfo,
        GetFileInformationByHandleEx,
    };
    let mut info = FILE_BASIC_INFO::default();
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileBasicInfo,
                std::ptr::from_mut(&mut info).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        } != 0
            && info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
            && info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "shared directory metadata is unavailable"
    );
    Ok(WindowsDirectoryMetadata {
        created: info.CreationTime,
        written: info.LastWriteTime,
        changed: info.ChangeTime,
        attributes: info.FileAttributes,
    })
}

impl Directory {
    fn open(path: &Path) -> crate::Result<Self> {
        ensure!(
            path.canonicalize()? == path,
            "shared directory is redirected"
        );
        let mut options = std::fs::OpenOptions::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
                FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            // Parent names must remain pinned while child paths are inspected.
            // Attribute-only access does not participate in sharing checks.
            options
                .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options.open(path)?;
        ensure!(
            file.metadata()?.is_dir(),
            "shared parent is not a directory"
        );
        let result = Self {
            path: path.to_owned(),
            #[cfg(unix)]
            metadata: file.metadata()?,
            #[cfg(windows)]
            metadata: windows_directory_metadata(&file)?,
            file,
        };
        result.identity()?;
        Ok(result)
    }

    fn identity(&self) -> crate::Result<(u64, u64)> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = self.file.metadata()?;
            Ok((metadata.dev(), metadata.ino()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
                GetFileInformationByHandle,
            };
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            ensure!(
                unsafe { GetFileInformationByHandle(self.file.as_raw_handle(), &mut info) } != 0
                    && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
                "shared directory identity is unavailable"
            );
            Ok((
                u64::from(info.dwVolumeSerialNumber),
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            ))
        }
        #[cfg(not(any(unix, windows)))]
        anyhow::bail!("shared directory identity is unsupported")
    }

    fn verify(&self) -> crate::Result<()> {
        let current = Self::open(&self.path)?;
        ensure!(
            self.identity()? == current.identity()?,
            "shared directory changed"
        );
        #[cfg(unix)]
        ensure!(
            same_unix_metadata(&self.metadata, &self.file.metadata()?),
            "shared directory metadata changed"
        );
        #[cfg(windows)]
        ensure!(
            self.metadata == current.metadata
                && self.metadata == windows_directory_metadata(&self.file)?,
            "shared directory metadata changed"
        );
        Ok(())
    }
}

#[cfg(unix)]
fn same_unix_metadata(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.mode() == after.mode()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

fn blob_digest<D: Digest>(bytes: &[u8]) -> String {
    let mut digest = D::new();
    digest.update(format!("blob {}\0", bytes.len()).as_bytes());
    digest.update(bytes);
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn a_retained_parent_prevents_replacement_until_child_inspection_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let parent = root.join("memory");
        let moved = root.join("moved-memory");
        std::fs::create_dir(&parent).unwrap();
        let child = parent.join("context.md");
        std::fs::write(&child, b"original").unwrap();
        let held = Directory::open(&parent).unwrap();

        assert!(std::fs::rename(&parent, &moved).is_err());
        std::fs::write(&child, b"observed").unwrap();
        let bytes = read_file_bytes(&child, Limits::default()).unwrap();
        held.verify().unwrap();
        assert_eq!(bytes, b"observed");
        assert!(!moved.exists());

        drop(held);
        std::fs::rename(&parent, &moved).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(&child, b"replaced").unwrap();
        assert_eq!(
            std::fs::read(moved.join("context.md")).unwrap(),
            b"observed"
        );
        assert_eq!(std::fs::read(&child).unwrap(), b"replaced");
    }

    #[cfg(windows)]
    #[test]
    fn a_parent_with_existing_delete_access_refuses_inspection() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().canonicalize().unwrap();
        let writer = std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&parent)
            .unwrap();
        assert!(Directory::open(&parent).is_err());
        drop(writer);
        Directory::open(&parent).unwrap().verify().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn restored_parent_attributes_do_not_hide_a_metadata_change() {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL,
            FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_WRITE_ATTRIBUTES, FileBasicInfo, SetFileInformationByHandle,
        };

        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().canonicalize().unwrap();
        let open_writer = || {
            std::fs::OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&parent)
                .unwrap()
        };
        let set = |writer: &std::fs::File, attributes, changed| {
            let info = FILE_BASIC_INFO {
                FileAttributes: attributes,
                ChangeTime: changed,
                ..Default::default()
            };
            assert_ne!(
                unsafe {
                    SetFileInformationByHandle(
                        writer.as_raw_handle(),
                        FileBasicInfo,
                        std::ptr::from_ref(&info).cast(),
                        std::mem::size_of::<FILE_BASIC_INFO>() as u32,
                    )
                },
                0,
                "directory metadata update failed: {}",
                std::io::Error::last_os_error()
            );
        };
        let baseline = 132_537_600_000_000_000;
        let setup = open_writer();
        set(&setup, 0, baseline);
        drop(setup);
        let held = Directory::open(&parent).unwrap();
        assert_eq!(held.metadata.changed, baseline);
        let original = held.metadata.attributes & !FILE_ATTRIBUTE_DIRECTORY;
        let normalized = |attributes| {
            if attributes == 0 {
                FILE_ATTRIBUTE_NORMAL
            } else {
                attributes
            }
        };
        let writer = open_writer();
        set(&writer, normalized(original ^ FILE_ATTRIBUTE_HIDDEN), 0);
        set(&writer, normalized(original), 0);
        drop(writer);
        let restored = windows_directory_metadata(&held.file).unwrap();
        assert_eq!(held.metadata.attributes, restored.attributes);
        assert_ne!(held.metadata.changed, restored.changed);
        assert!(held.verify().is_err());
        Directory::open(&parent).unwrap().verify().unwrap();
    }

    #[test]
    fn a_frozen_registration_binds_unborn_named_and_detached_head_observations() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        let hooks = directory.path().join("empty-hooks");
        std::fs::create_dir(&hooks).unwrap();
        for args in [
            vec!["config", "user.name", "Shared status fixture"],
            vec!["config", "user.email", "shared-status@example.invalid"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "core.hooksPath", hooks.to_str().unwrap()],
        ] {
            repo.git(&args).unwrap();
        }
        let repo = Repo::at(directory.path().canonicalize().unwrap()).local_objects_only();
        let registration = || git::Git::new().unwrap().worktrees(&repo).unwrap().remove(0);
        let observe = |checkout: &Worktree| {
            let head_before = std::fs::read(repo.root().join(".git/HEAD")).unwrap();
            let refs_before = repo.git(&["show-ref", "--head"]).ok();
            let result = snapshot(&git::Git::new().unwrap(), &repo, checkout);
            assert_eq!(
                std::fs::read(repo.root().join(".git/HEAD")).unwrap(),
                head_before
            );
            assert_eq!(repo.git(&["show-ref", "--head"]).ok(), refs_before);
            result
        };
        let unborn = registration();
        assert!(observe(&unborn).is_ok());
        repo.git(&["commit", "--allow-empty", "--quiet", "-m", "first head"])
            .unwrap();
        assert!(observe(&unborn).is_err());
        let named = registration();
        assert!(observe(&named).is_ok());
        repo.git(&["branch", "other"]).unwrap();
        repo.git(&["symbolic-ref", "HEAD", "refs/heads/other"])
            .unwrap();
        let other = registration();
        assert_eq!(named.head, other.head);
        assert!(observe(&named).is_err());
        assert!(observe(&other).is_ok());
        repo.git(&["commit", "--allow-empty", "--quiet", "-m", "second head"])
            .unwrap();
        let advanced = registration();
        assert_eq!(other.branch, advanced.branch);
        assert_ne!(other.head, advanced.head);
        assert!(observe(&other).is_err());
        assert!(observe(&advanced).is_ok());
        repo.git(&["switch", "--quiet", "--detach", "HEAD"])
            .unwrap();
        let detached = registration();
        assert_eq!(advanced.head, detached.head);
        assert!(detached.branch.is_none());
        assert!(observe(&advanced).is_err());
        assert!(observe(&detached).is_ok());
    }
}
